//! Ears: microphone -> Silero VAD -> Whisper, all local via sherpa-onnx.
//!
//! The mic is only open while the gate is open. The core closes the gate
//! whenever she is not listening (thinking, speaking, asleep, deafened), and
//! the mic thread then drops the input stream, so she never hears herself.
//! VAD waits for a pause before transcribing, so long sentences are kept whole.

use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use rodio::Source;
use rodio::microphone::{Microphone, MicrophoneBuilder};
use sherpa_onnx::{
    LinearResampler, OfflineRecognizer, OfflineRecognizerConfig, VadModelConfig,
    VoiceActivityDetector,
};

use crate::config::Config;

const SAMPLE_RATE: i32 = 16_000;
const WHISPER_DIR: &str = "sherpa-onnx-whisper-small.en";

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

/// Load the models and start listening. Every finished utterance is passed
/// to `on_heard`. Returns once the models are loaded (or failed to).
///
/// Two threads: "mic" only drains the microphone (rodio keeps just 100 ms of
/// audio and silently drops the rest if nobody reads it), "transcriber" runs
/// VAD and Whisper. Slow transcription can therefore never lose speech.
///
/// Set `LASTHOPE_DEBUG_AUDIO=1` to save every utterance to `debug-audio/`.
pub fn spawn(cfg: &Config, gate: Gate, on_heard: impl Fn(String) + Send + 'static) -> Result<()> {
    let models = cfg.models_dir.clone();
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
        ears.transcribe_loop(rx, &on_heard, debug_dir);
    })?;
    init_rx.recv().context("transcriber thread died while loading")??;
    thread::Builder::new().name("mic".into()).spawn(move || capture_loop(&gate, &tx))?;
    Ok(())
}

/// Mic thread: open the mic while the gate is open, forward 50 ms mono chunks.
fn capture_loop(gate: &Gate, tx: &std_mpsc::Sender<Chunk>) {
    let mut announced = None;
    loop {
        gate.wait_open();
        let mut mic = match open_mic() {
            Ok(m) => m,
            Err(e) => {
                eprintln!("  ! microphone: {e:#}; retrying in 2s");
                thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        let channels = mic.channels().get() as usize;
        let rate = mic.sample_rate().get();
        if announced != Some((rate, channels)) {
            eprintln!("  (mic: {rate} Hz, {channels} ch)");
            announced = Some((rate, channels));
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
        // How long a pause ends an utterance. Long enough to breathe mid-thought.
        v.silero_vad.min_silence_duration = 0.8;
        v.silero_vad.min_speech_duration = 0.25;
        v.silero_vad.max_speech_duration = 30.0;
        v.silero_vad.window_size = 512;
        v.sample_rate = SAMPLE_RATE;
        v.num_threads = 1;
        let vad = VoiceActivityDetector::create(&v, 60.0).ok_or_else(|| anyhow!("failed to load VAD"))?;

        Ok(Self { asr, vad })
    }

    /// Transcriber thread: VAD finds utterances, Whisper turns them into text.
    fn transcribe_loop(
        &self,
        rx: std_mpsc::Receiver<Chunk>,
        on_heard: &dyn Fn(String),
        debug_dir: Option<PathBuf>,
    ) {
        let mut resampler: Option<(u32, LinearResampler)> = None;
        let mut history = History::default();
        let mut saved = 0u32;
        for chunk in rx {
            let (samples, rate) = match chunk {
                Chunk::Audio { samples, rate } => (samples, rate),
                Chunk::Closed => {
                    self.vad.reset();
                    history = History::default();
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
                // The VAD fires a little after a soft first word begins ("t" of
                // "talk"); give Whisper the audio just before it too.
                let speech = history.with_preroll(segment.start(), segment.samples());
                self.vad.pop();
                let started = Instant::now();
                let text = self.transcribe(&speech);
                eprintln!(
                    "  ({:.1}s of speech, transcribed in {} ms)",
                    speech.len() as f32 / SAMPLE_RATE as f32,
                    started.elapsed().as_millis()
                );
                if let Some(dir) = &debug_dir {
                    saved += 1;
                    let _ = std::fs::create_dir_all(dir);
                    let heard = dir.join(format!("heard-{saved:03}.wav"));
                    let context = dir.join(format!("context-{saved:03}.wav"));
                    sherpa_onnx::write(&heard.to_string_lossy(), &speech, SAMPLE_RATE);
                    sherpa_onnx::write(&context.to_string_lossy(), &history.context(&speech), SAMPLE_RATE);
                    eprintln!("  (saved {}: {text:?})", heard.display());
                }
                if let Some(text) = clean_transcript(&text) {
                    on_heard(text);
                }
            }
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

/// Audio before a VAD segment, prepended so soft first words survive.
const PREROLL: i64 = SAMPLE_RATE as i64 * 4 / 10; // 400 ms
/// Lead-in saved next to each utterance in debug mode.
const CONTEXT: i64 = SAMPLE_RATE as i64 * 2;
/// How much recent audio to keep.
const HISTORY: usize = SAMPLE_RATE as usize * 10;

/// The last few seconds fed to the VAD, indexed like the VAD's own sample
/// positions (counted from the last reset).
#[derive(Default)]
struct History {
    samples: Vec<f32>,
    /// VAD position of `samples[0]`.
    first: i64,
    /// VAD position where the previous utterance ended (never prepend into it).
    last_end: i64,
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

    fn slice(&self, from: i64, to: i64) -> &[f32] {
        let end = (self.first + self.samples.len() as i64).min(to).max(self.first);
        let from = from.max(self.first).min(end);
        &self.samples[(from - self.first) as usize..(end - self.first) as usize]
    }

    /// `segment` (starting at VAD position `start`) with up to PREROLL of the
    /// audio before it, not reaching back into the previous utterance.
    fn with_preroll(&mut self, start: i64, segment: &[f32]) -> Vec<f32> {
        let from = (start - PREROLL).max(self.last_end);
        let mut out = self.slice(from, start).to_vec();
        out.extend_from_slice(segment);
        self.last_end = start + segment.len() as i64;
        out
    }

    /// Up to CONTEXT of audio before `utterance` (which ends at `last_end`).
    fn context(&self, utterance: &[f32]) -> Vec<f32> {
        let start = self.last_end - utterance.len() as i64;
        self.slice(start - CONTEXT, self.last_end).to_vec()
    }
}

fn open_mic() -> Result<Microphone> {
    Ok(MicrophoneBuilder::new()
        .default_device()?
        .default_config()?
        .prefer_channel_counts([1.try_into().unwrap()])
        .prefer_sample_rates([16_000.try_into().unwrap()])
        .open_stream()?)
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

        for (pause_ms, expected) in [(400u32, 1usize), (1500, 2)] {
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
            ears.transcribe_loop(rx, &|t| heard.borrow_mut().push(t), None);
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
    fn preroll_reaches_back_but_not_into_previous_utterance() {
        let mut h = History::default();
        let audio: Vec<f32> = (0..SAMPLE_RATE * 2).map(|i| i as f32).collect();
        h.push(&audio);
        // first utterance at 1.0 s: gets 400 ms before it
        let seg = &audio[16_000..17_000];
        let out = h.with_preroll(16_000, seg);
        assert_eq!(out.len(), PREROLL as usize + 1000);
        assert_eq!(out[0], (16_000 - PREROLL) as f32);
        // next utterance 100 ms after the first ended: only that gap is prepended
        let seg = &audio[18_600..19_000];
        let out = h.with_preroll(18_600, seg);
        assert_eq!(out[0], 17_000.0);
        assert_eq!(out.len(), 1600 + 400);
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
