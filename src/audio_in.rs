//! Ears: microphone -> Silero VAD -> Whisper, all local via sherpa-onnx.
//!
//! The mic is only open while the gate is open. The core closes the gate
//! whenever she is not listening (thinking, speaking, asleep, deafened), and
//! the mic thread then drops the input stream, so she never hears herself.
//!
//! The VAD finds stretches of speech; a turn ends only after a longer silence
//! (END_OF_TURN_MS, default 1.2 s), and the whole turn is transcribed at once,
//! so thinking pauses don't split a sentence and Whisper gets full context.
//!
//! Bluetooth headsets drop to phone-call quality (16 kHz, both directions)
//! while their mic is open, so by default the headset's own mic is avoided
//! when another one (e.g. the laptop's) exists. MIC_DEVICE overrides this.

use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use rodio::Source;
use rodio::cpal::traits::{DeviceTrait, HostTrait};
use rodio::microphone::{Input, Microphone, MicrophoneBuilder, available_inputs};
use sherpa_onnx::{
    LinearResampler, OfflineRecognizer, OfflineRecognizerConfig, VadModelConfig,
    VoiceActivityDetector,
};

use crate::config::Config;

const SAMPLE_RATE: i32 = 16_000;
const WHISPER_DIR: &str = "sherpa-onnx-whisper-small.en";
/// Silence that closes one VAD speech segment (a turn may hold several).
const SEGMENT_SILENCE: f32 = 0.5;

/// Open/closed switch for the microphone, shared between the core and the
/// capture thread.
#[derive(Clone)]
pub struct Gate(Arc<(Mutex<bool>, Condvar)>);

impl Gate {
    pub fn new(open: bool) -> Self {
        Self(Arc::new((Mutex::new(open), Condvar::new())))
    }

    pub fn set(&self, open: bool) {
        let (lock, cvar) = &*self.0;
        let mut g = lock.lock().unwrap();
        if *g != open {
            *g = open;
            cvar.notify_all();
        }
    }

    pub fn is_open(&self) -> bool {
        *self.0.0.lock().unwrap()
    }

    fn wait_open(&self) {
        let (lock, cvar) = &*self.0;
        let mut g = lock.lock().unwrap();
        while !*g {
            g = cvar.wait(g).unwrap();
        }
    }
}

pub(crate) struct Ears {
    asr: OfflineRecognizer,
    vad: VoiceActivityDetector,
}

/// Audio handed from the mic thread to the transcriber.
enum Chunk {
    /// Mono samples at `rate` Hz.
    Audio { samples: Vec<f32>, rate: u32 },
    /// The mic was closed: drop any half-heard utterance.
    Closed,
}

/// Load the models and start listening. Every finished turn is passed to
/// `on_heard`. Returns once the models are loaded (or failed to).
///
/// Two threads: "mic" only drains the microphone (rodio keeps just 100 ms of
/// audio and silently drops the rest if nobody reads it), "transcriber" runs
/// VAD and Whisper. Slow transcription can therefore never lose speech.
///
/// Set `LASTHOPE_DEBUG_AUDIO=1` to save every turn to `debug-audio/`.
pub fn spawn(cfg: &Config, gate: Gate, on_heard: impl Fn(String) + Send + 'static) -> Result<()> {
    // fail fast on a MIC_DEVICE that matches nothing
    choose_input(cfg.mic_device.as_deref())?;
    let models = cfg.models_dir.clone();
    let mic_pref = cfg.mic_device.clone();
    let turn_gap = (cfg.end_of_turn.as_secs_f64() * SAMPLE_RATE as f64) as i64;
    let debug_dir = std::env::var("LASTHOPE_DEBUG_AUDIO")
        .ok()
        .filter(|v| !v.is_empty() && v != "0")
        .map(|_| PathBuf::from("debug-audio"));
    let (tx, rx) = std_mpsc::channel::<Chunk>();
    let (init_tx, init_rx) = std_mpsc::channel();
    thread::Builder::new().name("transcriber".into()).spawn(move || {
        let ears = match Ears::load(&models) {
            Ok(e) => {
                let _ = init_tx.send(Ok(()));
                e
            }
            Err(e) => {
                let _ = init_tx.send(Err(e));
                return;
            }
        };
        ears.transcribe_loop(rx, &on_heard, turn_gap, debug_dir);
    })?;
    init_rx.recv().context("transcriber thread died while loading")??;
    thread::Builder::new()
        .name("mic".into())
        .spawn(move || capture_loop(&gate, &tx, mic_pref.as_deref()))?;
    Ok(())
}

/// Mic thread: open the mic while the gate is open, forward 50 ms mono chunks.
fn capture_loop(gate: &Gate, tx: &std_mpsc::Sender<Chunk>, mic_pref: Option<&str>) {
    let mut announced = None;
    loop {
        gate.wait_open();
        // chosen on every opening, so plugging headphones in or out is followed
        let (mut mic, name) = match open_mic(mic_pref) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("  ! microphone: {e:#}; retrying in 2s");
                thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        let channels = mic.channels().get() as usize;
        let rate = mic.sample_rate().get();
        let now = (name, rate, channels);
        if announced.as_ref() != Some(&now) {
            eprintln!("  (mic: {}, {rate} Hz, {channels} ch)", now.0);
            announced = Some(now);
        }
        let window = (rate / 20) as usize;
        'read: loop {
            let mut mono = Vec::with_capacity(window);
            while mono.len() < window {
                let mut sum = 0.0;
                for _ in 0..channels {
                    match mic.next() {
                        Some(s) => sum += s,
                        None => break 'read, // stream error: reopen
                    }
                }
                mono.push(sum / channels as f32);
            }
            if !gate.is_open() {
                break;
            }
            if tx.send(Chunk::Audio { samples: mono, rate }).is_err() {
                return;
            }
        }
        drop(mic); // closes the input stream until the gate reopens
        if tx.send(Chunk::Closed).is_err() {
            return;
        }
    }
}

fn device_name(d: &rodio::cpal::Device) -> Option<String> {
    d.description().ok().map(|d| d.name().to_string())
}

/// Which mic to open. `None` means the system default.
/// - MIC_DEVICE=default: the system default.
/// - MIC_DEVICE=<text>: the first input whose name contains it.
/// - unset: the default, unless it is the same device as the default output
///   (a headset) and another input exists: then that other input.
fn choose_input(pref: Option<&str>) -> Result<Option<Input>> {
    let inputs = available_inputs()?;
    match pref {
        Some(p) if p.eq_ignore_ascii_case("default") => Ok(None),
        Some(p) => {
            let wanted = p.to_lowercase();
            let names: Vec<String> = inputs.iter().map(|i| i.to_string()).collect();
            inputs
                .into_iter()
                .find(|i| i.to_string().to_lowercase().contains(&wanted))
                .map(Some)
                .ok_or_else(|| anyhow!("no microphone matches MIC_DEVICE={p:?}; available: {}", names.join(", ")))
        }
        None => {
            let host = rodio::cpal::default_host();
            let default_in = host.default_input_device().and_then(|d| device_name(&d));
            let default_out = host.default_output_device().and_then(|d| device_name(&d));
            if default_in.is_some() && default_in == default_out {
                Ok(inputs.into_iter().find(|i| Some(i.to_string()) != default_in))
            } else {
                Ok(None)
            }
        }
    }
}

/// For `--mics`: list inputs and say which one would be used.
pub fn describe_mics(pref: Option<&str>) -> Result<String> {
    let host = rodio::cpal::default_host();
    let default_in = host.default_input_device().and_then(|d| device_name(&d));
    let default_out = host.default_output_device().and_then(|d| device_name(&d));
    let mut out = String::from("microphones:\n");
    for input in available_inputs()? {
        let name = input.to_string();
        let tag = if Some(&name) == default_in.as_ref() { "  (system default)" } else { "" };
        out.push_str(&format!("  {name}{tag}\n"));
    }
    out.push_str(&format!("speaker: {}\n", default_out.as_deref().unwrap_or("none")));
    let chosen = match choose_input(pref)? {
        Some(i) => i.to_string(),
        None => default_in.unwrap_or_else(|| "none".into()),
    };
    out.push_str(&format!("she will listen through: {chosen}"));
    if pref.is_none() && Some(&chosen) != host.default_input_device().and_then(|d| device_name(&d)).as_ref() {
        out.push_str("  (the default mic is your headset's: avoided, it would drop the headset to call quality)");
    }
    Ok(out)
}

fn open_mic(pref: Option<&str>) -> Result<(Microphone, String)> {
    let (builder, name) = match choose_input(pref)? {
        Some(input) => {
            let name = input.to_string();
            (MicrophoneBuilder::new().device(input)?, name)
        }
        None => {
            let name = rodio::cpal::default_host()
                .default_input_device()
                .and_then(|d| device_name(&d))
                .unwrap_or_else(|| "default".into());
            (MicrophoneBuilder::new().default_device()?, name)
        }
    };
    let mic = builder
        .default_config()?
        .prefer_channel_counts([1.try_into().unwrap()])
        .prefer_sample_rates([16_000.try_into().unwrap()])
        .open_stream()?;
    Ok((mic, name))
}

impl Ears {
    pub(crate) fn load(models: &Path) -> Result<Self> {
        let whisper = models.join(WHISPER_DIR);
        let file = |p: std::path::PathBuf| -> Result<String> {
            anyhow::ensure!(
                p.exists(),
                "missing model file {} (run scripts/fetch-models.sh)",
                p.display()
            );
            Ok(p.to_string_lossy().into_owned())
        };

        let mut c = OfflineRecognizerConfig::default();
        c.model_config.whisper.encoder = Some(file(whisper.join("small.en-encoder.int8.onnx"))?);
        c.model_config.whisper.decoder = Some(file(whisper.join("small.en-decoder.int8.onnx"))?);
        c.model_config.whisper.language = Some("en".into());
        c.model_config.whisper.task = Some("transcribe".into());
        c.model_config.whisper.tail_paddings = -1;
        c.model_config.tokens = Some(file(whisper.join("small.en-tokens.txt"))?);
        c.model_config.num_threads = 4;
        let asr = OfflineRecognizer::create(&c).ok_or_else(|| anyhow!("failed to load Whisper"))?;

        let mut v = VadModelConfig::default();
        v.silero_vad.model = Some(file(models.join("silero_vad.onnx"))?);
        v.silero_vad.threshold = 0.5;
        v.silero_vad.min_silence_duration = SEGMENT_SILENCE;
        v.silero_vad.min_speech_duration = 0.25;
        v.silero_vad.max_speech_duration = 30.0;
        v.silero_vad.window_size = 512;
        v.sample_rate = SAMPLE_RATE;
        v.num_threads = 1;
        let vad = VoiceActivityDetector::create(&v, 60.0).ok_or_else(|| anyhow!("failed to load VAD"))?;

        Ok(Self { asr, vad })
    }

    /// Transcriber thread: the VAD finds speech, segments are gathered into a
    /// turn, and a turn is transcribed once `turn_gap` samples of silence follow.
    fn transcribe_loop(
        &self,
        rx: std_mpsc::Receiver<Chunk>,
        on_heard: &dyn Fn(String),
        turn_gap: i64,
        debug_dir: Option<PathBuf>,
    ) {
        let mut resampler: Option<(u32, LinearResampler)> = None;
        let mut history = History::default();
        let mut turn = Turn::default();
        let mut saved = 0u32;
        let finish = |span: (i64, i64), history: &History, saved: &mut u32| {
            let speech = history.slice(span.0, span.1);
            let started = Instant::now();
            let text = self.transcribe(speech);
            eprintln!(
                "  ({:.1}s of speech, transcribed in {} ms)",
                speech.len() as f32 / SAMPLE_RATE as f32,
                started.elapsed().as_millis()
            );
            if let Some(dir) = &debug_dir {
                *saved += 1;
                let _ = std::fs::create_dir_all(dir);
                let heard = dir.join(format!("heard-{saved:03}.wav"));
                let context = dir.join(format!("context-{saved:03}.wav"));
                sherpa_onnx::write(&heard.to_string_lossy(), speech, SAMPLE_RATE);
                let lead_in = history.slice(span.0 - CONTEXT, span.1);
                sherpa_onnx::write(&context.to_string_lossy(), lead_in, SAMPLE_RATE);
                eprintln!("  (saved {}: {text:?})", heard.display());
            }
            if let Some(text) = clean_transcript(&text) {
                on_heard(text);
            }
        };

        for chunk in rx {
            let (samples, rate) = match chunk {
                Chunk::Audio { samples, rate } => (samples, rate),
                Chunk::Closed => {
                    // mic closed mid-turn (she started speaking, muted, ...): drop it
                    self.vad.reset();
                    history = History::default();
                    turn = Turn::default();
                    if let Some((_, r)) = &resampler {
                        r.reset();
                    }
                    continue;
                }
            };
            let samples = if rate as i32 == SAMPLE_RATE {
                samples
            } else {
                if resampler.as_ref().is_none_or(|(r, _)| *r != rate) {
                    resampler = LinearResampler::create(rate as i32, SAMPLE_RATE).map(|r| (rate, r));
                }
                match &resampler {
                    Some((_, r)) => r.resample(&samples, false),
                    None => {
                        eprintln!("  ! cannot resample {rate} Hz audio");
                        continue;
                    }
                }
            };
            self.vad.accept_waveform(&samples);
            history.push(&samples);

            while let Some(segment) = self.vad.front() {
                turn.add_segment(segment.start(), segment.n() as i64);
                self.vad.pop();
            }
            if let Some(span) = turn.take_if_done(history.end(), self.vad.detected(), turn_gap) {
                finish(span, &history, &mut saved);
            }
        }
        // input ended (tests): whatever was being said is a turn
        self.vad.flush();
        while let Some(segment) = self.vad.front() {
            turn.add_segment(segment.start(), segment.n() as i64);
            self.vad.pop();
        }
        if let Some(span) = turn.take_if_done(i64::MAX, false, 0) {
            finish(span, &history, &mut saved);
        }
    }

    /// Transcribe 16 kHz mono samples.
    pub(crate) fn transcribe(&self, samples: &[f32]) -> String {
        let stream = self.asr.create_stream();
        stream.accept_waveform(SAMPLE_RATE, samples);
        self.asr.decode(&stream);
        stream.get_result().map(|r| r.text).unwrap_or_default()
    }
}

/// Audio before the first segment of a turn, so soft first words survive
/// (the VAD fires a little after the "t" of "talk" begins).
const PREROLL: i64 = SAMPLE_RATE as i64 * 4 / 10; // 400 ms
/// Lead-in saved next to each turn in debug mode.
const CONTEXT: i64 = SAMPLE_RATE as i64 * 2;
/// How much recent audio to keep (also the longest possible turn).
const HISTORY: usize = SAMPLE_RATE as usize * 60;
/// A turn this long is transcribed even if speech continues.
const MAX_TURN: i64 = SAMPLE_RATE as i64 * 45;

/// Recent audio fed to the VAD, indexed like the VAD's own sample positions
/// (counted from the last reset).
#[derive(Default)]
struct History {
    samples: Vec<f32>,
    /// VAD position of `samples[0]`.
    first: i64,
}

impl History {
    fn push(&mut self, samples: &[f32]) {
        self.samples.extend_from_slice(samples);
        if self.samples.len() > 2 * HISTORY {
            let drop = self.samples.len() - HISTORY;
            self.samples.drain(..drop);
            self.first += drop as i64;
        }
    }

    /// VAD position just after the newest sample.
    fn end(&self) -> i64 {
        self.first + self.samples.len() as i64
    }

    fn slice(&self, from: i64, to: i64) -> &[f32] {
        let end = self.end().min(to).max(self.first);
        let from = from.max(self.first).min(end);
        &self.samples[(from - self.first) as usize..(end - self.first) as usize]
    }
}

/// Gathers VAD segments into one turn: from PREROLL before the first segment
/// (never reaching into the previous turn) to the end of the last one.
#[derive(Default)]
struct Turn {
    span: Option<(i64, i64)>,
    last_end: i64,
}

impl Turn {
    fn add_segment(&mut self, start: i64, len: i64) {
        let end = start + len;
        match &mut self.span {
            Some((_, e)) => *e = (*e).max(end),
            None => self.span = Some(((start - PREROLL).max(self.last_end), end)),
        }
    }

    /// The finished turn, once nobody is speaking and `gap` samples have
    /// passed since its speech ended (or it has grown too long).
    fn take_if_done(&mut self, now: i64, speaking: bool, gap: i64) -> Option<(i64, i64)> {
        let (start, end) = self.span?;
        let quiet_long_enough = !speaking && now.saturating_sub(end) >= gap;
        if quiet_long_enough || end - start >= MAX_TURN {
            self.span = None;
            self.last_end = end;
            Some((start, end))
        } else {
            None
        }
    }
}

/// Drop Whisper's non-speech markers ("[BLANK_AUDIO]", "(wind blowing)") and
/// anything left with no letters or digits.
fn clean_transcript(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    let mut depth = 0i32;
    for c in raw.chars() {
        match c {
            '[' | '(' => depth += 1,
            ']' | ')' => depth = (depth - 1).max(0),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    let out = out.split_whitespace().collect::<Vec<_>>().join(" ");
    out.chars().any(char::is_alphanumeric).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed the transcriber thread Kokoro speech in 50 ms chunks, all at once
    /// (a worst case for slow transcription). Nothing may be lost.
    /// Needs the model files: `cargo test --release -- --ignored`
    #[test]
    #[ignore]
    fn transcriber_keeps_whole_utterances() {
        let voice = crate::speech_out::Engine::load(Path::new("./kokoro-multi-lang-v1_0")).unwrap();
        let ears = Ears::load(Path::new("./models")).unwrap();
        let (a, rate) = voice.synth("Talk to me.").unwrap().unwrap();
        let (b, _) = voice.synth("Tell me something about Rust and the EVM.").unwrap().unwrap();
        // Real mic level: the user's recordings peak around 0.04-0.08 RMS, Kokoro ~0.6.
        let quiet = |v: &[f32]| v.iter().map(|x| x * 0.08).collect::<Vec<f32>>();
        let (a, b) = (quiet(&a), quiet(&b));

        // a turn ends after 1.2 s of silence: shorter thinking pauses stay in one turn
        let gap = (1.2 * SAMPLE_RATE as f64) as i64;
        for (pause_ms, expected) in [(400u32, 1usize), (900, 1), (1800, 2)] {
            let mut audio = vec![0.0; rate as usize / 2];
            audio.extend(&a);
            audio.extend(vec![0.0; (rate * pause_ms / 1000) as usize]);
            audio.extend(&b);
            audio.extend(vec![0.0; rate as usize * 2]);

            let (tx, rx) = std_mpsc::channel();
            for c in audio.chunks(rate as usize / 20) {
                tx.send(Chunk::Audio { samples: c.to_vec(), rate }).unwrap();
            }
            drop(tx);
            let heard = std::cell::RefCell::new(Vec::new());
            ears.transcribe_loop(rx, &|t| heard.borrow_mut().push(t), gap, None);
            let heard = heard.into_inner();
            println!("pause {pause_ms} ms -> {heard:?}");
            assert_eq!(heard.len(), expected, "{heard:?}");
            let all = heard.join(" ").to_lowercase();
            assert!(all.starts_with("talk to me"), "{all}");
            assert!(all.contains("rust") && all.contains("evm"), "{all}");
            ears.vad.reset();
        }
    }

    /// Diagnostic: how late does the VAD open a segment on real recordings?
    /// `cargo test --release vad_onset_lag -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn vad_onset_lag() {
        let ears = Ears::load(Path::new("./models")).unwrap();
        let mut files: Vec<_> = std::fs::read_dir("debug-audio").unwrap().flatten().map(|e| e.path()).collect();
        files.sort();
        for f in files {
            let wave = sherpa_onnx::Wave::read(&f.to_string_lossy()).unwrap();
            let speech = wave.samples();
            let lead = SAMPLE_RATE as usize; // 1 s of silence before
            let onset = lead + speech.iter().position(|x| x.abs() > 0.01).unwrap_or(0);
            let mut audio = vec![0.0f32; lead];
            audio.extend_from_slice(speech);
            audio.extend(vec![0.0f32; SAMPLE_RATE as usize]);
            ears.vad.reset();
            for c in audio.chunks(800) {
                ears.vad.accept_waveform(c);
            }
            ears.vad.flush();
            if let Some(seg) = ears.vad.front() {
                let lag_ms = (seg.start() - onset as i64) * 1000 / SAMPLE_RATE as i64;
                println!("{}: segment opens {lag_ms:+} ms vs first sound", f.display());
            }
            while ears.vad.front().is_some() {
                ears.vad.pop();
            }
        }
    }

    #[test]
    fn turn_joins_segments_until_a_long_silence() {
        let sec = SAMPLE_RATE as i64;
        let mut t = Turn::default();
        t.add_segment(2 * sec, sec); // speech 2.0-3.0 s
        assert_eq!(t.take_if_done(3 * sec + sec / 2, false, sec), None); // 0.5 s quiet: wait
        t.add_segment(3 * sec + sec / 2, sec); // speech again 3.5-4.5 s
        assert_eq!(t.take_if_done(5 * sec, true, sec), None); // still speaking
        assert_eq!(t.take_if_done(5 * sec + sec / 2, false, sec), Some((2 * sec - PREROLL, 4 * sec + sec / 2)));
        assert_eq!(t.take_if_done(9 * sec, false, sec), None); // nothing pending
        // next turn starts 0.1 s after the last: preroll stops at the previous end
        t.add_segment(4 * sec + sec / 2 + sec / 10, sec);
        assert_eq!(t.take_if_done(20 * sec, false, sec).unwrap().0, 4 * sec + sec / 2);
    }

    #[test]
    fn overlong_turn_is_cut() {
        let mut t = Turn::default();
        t.add_segment(PREROLL, MAX_TURN);
        assert_eq!(t.take_if_done(MAX_TURN, true, i64::MAX), Some((0, PREROLL + MAX_TURN)));
    }

    #[test]
    fn history_trims_and_keeps_positions() {
        let mut h = History::default();
        let chunk = vec![1.0f32; HISTORY];
        h.push(&chunk);
        h.push(&chunk);
        h.push(&[2.0; 10]);
        assert_eq!(h.samples.len(), HISTORY);
        assert_eq!(h.first, (2 * HISTORY + 10 - HISTORY) as i64);
        let end = (2 * HISTORY + 10) as i64;
        assert_eq!(h.slice(end - 10, end), &[2.0; 10]);
        assert!(h.slice(0, 5).is_empty()); // trimmed away
    }

    #[test]
    fn transcript_cleaning() {
        assert_eq!(clean_transcript(" [BLANK_AUDIO] "), None);
        assert_eq!(clean_transcript("(wind blowing) ..."), None);
        assert_eq!(
            clean_transcript(" Talk to me,  (coughs) what is the EVM? "),
            Some("Talk to me, what is the EVM?".into())
        );
    }
}
