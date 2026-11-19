//! Ears: microphone -> Silero VAD -> Whisper, all local via sherpa-onnx.
//!
//! The mic is only open while the gate is open. The core closes the gate
//! whenever she is not listening (thinking, speaking, asleep, deafened), and
//! the capture thread then drops the input stream, so she never hears herself.
//! VAD waits for a pause before transcribing, so long sentences are kept whole.

use std::path::Path;
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

/// Load the models and start the capture thread. Every finished utterance is
/// passed to `on_heard`. Returns once the models are loaded (or failed to).
pub fn spawn(cfg: &Config, gate: Gate, on_heard: impl Fn(String) + Send + 'static) -> Result<()> {
    let models = cfg.models_dir.clone();
    let (init_tx, init_rx) = std::sync::mpsc::channel();
    thread::Builder::new().name("ears".into()).spawn(move || {
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
        ears.run(&gate, &on_heard);
    })?;
    init_rx.recv().context("ears thread died while loading")?
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

    fn run(&self, gate: &Gate, on_heard: &dyn Fn(String)) {
        loop {
            gate.wait_open();
            let mic = match open_mic() {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("! microphone: {e:#}; retrying in 2s");
                    thread::sleep(Duration::from_secs(2));
                    continue;
                }
            };
            self.capture(mic, gate, on_heard);
            // mic dropped here: the input stream is closed until the gate reopens
            self.vad.reset();
        }
    }

    /// Read the mic in ~100 ms windows until the gate closes or the stream ends.
    fn capture(&self, mut mic: Microphone, gate: &Gate, on_heard: &dyn Fn(String)) {
        let channels = mic.channels().get() as usize;
        let rate = mic.sample_rate().get();
        let resampler = (rate as i32 != SAMPLE_RATE)
            .then(|| LinearResampler::create(rate as i32, SAMPLE_RATE))
            .flatten();
        let window = (rate / 10) as usize;
        let mut mono = Vec::with_capacity(window);

        loop {
            mono.clear();
            while mono.len() < window {
                let mut sum = 0.0;
                for _ in 0..channels {
                    match mic.next() {
                        Some(s) => sum += s,
                        None => return,
                    }
                }
                mono.push(sum / channels as f32);
            }
            if !gate.is_open() {
                return;
            }
            match &resampler {
                Some(r) => self.vad.accept_waveform(&r.resample(&mono, false)),
                None => self.vad.accept_waveform(&mono),
            }
            while let Some(segment) = self.vad.front() {
                let started = Instant::now();
                let text = self.transcribe(segment.samples());
                self.vad.pop();
                if let Some(text) = clean_transcript(&text) {
                    eprintln!("  (heard in {} ms)", started.elapsed().as_millis());
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
