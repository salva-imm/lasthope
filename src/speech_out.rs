//! Her voice: text -> loqui-g2p phonemes -> Kokoro (sherpa-onnx) -> rodio.
//!
//! A dedicated thread owns the TTS engine and the speaker. Replies are split
//! into sentences; the next sentence is synthesized while the previous one
//! plays, and speech can be cut off at any moment through a cancel flag.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use loqui_g2p::{Dialect, G2p, OovFallback};
use rodio::buffer::SamplesBuffer;
use rodio::{DeviceSinkBuilder, MixerDeviceSink, Player};
use sherpa_onnx::{GenerationConfig, OfflineTts, OfflineTtsConfig};
use tokio::sync::oneshot;

use crate::config::Config;

/// Kokoro speaker id (2 = the voice used so far) and pace.
const VOICE_ID: i32 = 2;
const SPEED: f32 = 1.0;
/// Silence before each reply (see `speak`).
const LEAD_IN_MS: usize = 200;

/// Words the phonemizer would get wrong, rewritten before phonemizing.
const PRONUNCIATIONS: &[(&str, &str)] = &[("lasthope", "Last Hope"), ("API", "A P I")];

/// Something that can say things. Voice mode speaks aloud; text mode prints.
pub trait Mouth: Send + Sync {
    /// Speak `text`; resolves when finished or cancelled via `cancel`.
    fn speak(&self, text: String, cancel: Arc<AtomicBool>) -> oneshot::Receiver<Result<()>>;
    /// Synthesize `text` into a WAV file instead of playing it.
    fn save_wav(&self, text: String, path: PathBuf) -> oneshot::Receiver<Result<()>>;
}

enum Job {
    Speak { text: String, cancel: Arc<AtomicBool>, done: oneshot::Sender<Result<()>> },
    Save { text: String, path: PathBuf, done: oneshot::Sender<Result<()>> },
}

/// Handle to the voice thread.
pub struct Voice {
    jobs: std_mpsc::Sender<Job>,
}

impl Voice {
    /// Load Kokoro and the phonemizer on a dedicated thread.
    pub fn spawn(cfg: &Config) -> Result<Self> {
        let dir = cfg.kokoro_dir.clone();
        let (jobs, rx) = std_mpsc::channel::<Job>();
        let (init_tx, init_rx) = std_mpsc::channel();
        thread::Builder::new().name("voice".into()).spawn(move || {
            let engine = match Engine::load(&dir) {
                Ok(e) => {
                    let _ = init_tx.send(Ok(()));
                    e
                }
                Err(e) => {
                    let _ = init_tx.send(Err(e));
                    return;
                }
            };
            for job in rx {
                match job {
                    Job::Speak { text, cancel, done } => {
                        let _ = done.send(engine.speak(&text, &cancel));
                    }
                    Job::Save { text, path, done } => {
                        let _ = done.send(engine.save_wav(&text, &path));
                    }
                }
            }
        })?;
        init_rx.recv().context("voice thread died while loading")??;
        Ok(Self { jobs })
    }

    fn submit(&self, make: impl FnOnce(oneshot::Sender<Result<()>>) -> Job) -> oneshot::Receiver<Result<()>> {
        let (tx, rx) = oneshot::channel();
        // If the thread is gone, the dropped sender resolves the receiver with an error.
        let _ = self.jobs.send(make(tx));
        rx
    }
}

impl Mouth for Voice {
    fn speak(&self, text: String, cancel: Arc<AtomicBool>) -> oneshot::Receiver<Result<()>> {
        self.submit(|done| Job::Speak { text, cancel, done })
    }

    fn save_wav(&self, text: String, path: PathBuf) -> oneshot::Receiver<Result<()>> {
        self.submit(|done| Job::Save { text, path, done })
    }
}

pub(crate) struct Engine {
    tts: OfflineTts,
    g2p: G2p,
}

impl Engine {
    pub(crate) fn load(dir: &Path) -> Result<Self> {
        let file = |name: &str| -> Result<String> {
            let p = dir.join(name);
            anyhow::ensure!(
                p.exists(),
                "missing Kokoro file {} (run scripts/fetch-models.sh)",
                p.display()
            );
            Ok(p.to_string_lossy().into_owned())
        };
        let mut c = OfflineTtsConfig::default();
        c.model.kokoro.model = Some(file("model.onnx")?);
        c.model.kokoro.voices = Some(file("voices.bin")?);
        c.model.kokoro.tokens = Some(file("tokens.txt")?);
        c.model.kokoro.length_scale = 1.0;
        c.model.num_threads = 4;
        c.max_num_sentences = 1;
        let tts = OfflineTts::create(&c).ok_or_else(|| anyhow!("failed to load Kokoro"))?;
        let g2p = G2p::new(Dialect::American, OovFallback::Neural)
            .map_err(|e| anyhow!("failed to load phonemizer: {e:?}"))?;
        Ok(Self { tts, g2p })
    }

    /// One sentence -> samples at the model's sample rate.
    pub(crate) fn synth(&self, sentence: &str) -> Result<Option<(Vec<f32>, u32)>> {
        let phonemes = self.g2p.phonemize(sentence);
        if phonemes.trim().is_empty() {
            return Ok(None);
        }
        let cfg = GenerationConfig {
            sid: VOICE_ID,
            speed: SPEED,
            phoneme_codepoints: Some(vec![phonemes.chars().map(|c| c as i32).collect()]),
            ..Default::default()
        };
        let audio = self
            .tts
            .generate_with_config::<fn(&[f32], f32) -> bool>("", &cfg, None)
            .ok_or_else(|| anyhow!("speech synthesis failed for {sentence:?}"))?;
        Ok(Some((audio.samples().to_vec(), audio.sample_rate() as u32)))
    }

    fn speak(&self, text: &str, cancel: &AtomicBool) -> Result<()> {
        // Open the speaker per reply so a changed output device (headphones) is picked up.
        let mut sink = open_speaker()?;
        sink.log_on_drop(false);
        let player = Player::connect_new(sink.mixer());
        // Bluetooth headphones often swallow the first moments of a new stream;
        // let that be silence rather than her first syllable.
        player.append(SamplesBuffer::new(
            1.try_into().unwrap(),
            24_000.try_into().unwrap(),
            vec![0.0f32; 24_000 * LEAD_IN_MS / 1000],
        ));

        for sentence in split_sentences(&for_speech(text)) {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            if let Some((samples, rate)) = self.synth(&sentence)? {
                player.append(SamplesBuffer::new(
                    1.try_into().unwrap(),
                    rate.try_into().context("zero sample rate")?,
                    samples,
                ));
            }
        }
        while !player.empty() {
            if cancel.load(Ordering::Relaxed) {
                player.stop();
                break;
            }
            thread::sleep(Duration::from_millis(30));
        }
        Ok(())
    }

    fn save_wav(&self, text: &str, path: &Path) -> Result<()> {
        let mut all = Vec::new();
        let mut rate = 0;
        for sentence in split_sentences(&for_speech(text)) {
            if let Some((samples, r)) = self.synth(&sentence)? {
                all.extend(samples);
                rate = r;
            }
        }
        anyhow::ensure!(!all.is_empty(), "nothing to say");
        let ok = sherpa_onnx::write(&path.to_string_lossy(), &all, rate as i32);
        anyhow::ensure!(ok, "could not write {}", path.display());
        Ok(())
    }
}

/// Open the default output device with a buffer size it actually supports.
/// rodio aims for 50 ms (2048 frames at 44.1 kHz), which Bluetooth headphones
/// on macOS reject ("supported range 14..=1024"), so clamp to the device's
/// range; then try the device's own default; then rodio's full fallback.
fn open_speaker() -> Result<MixerDeviceSink> {
    use rodio::cpal::traits::{DeviceTrait, HostTrait};
    use rodio::cpal::{BufferSize, SupportedBufferSize};

    let device = rodio::cpal::default_host().default_output_device();
    if let Some(device) = device {
        if let Ok(config) = device.default_output_config() {
            let ideal = (config.sample_rate() / 20).next_power_of_two() / 2; // ~50 ms
            let size = match *config.buffer_size() {
                SupportedBufferSize::Range { min, max } => ideal.clamp(min, max),
                SupportedBufferSize::Unknown => ideal,
            };
            for buffer in [BufferSize::Fixed(size), BufferSize::Default] {
                if let Ok(sink) = DeviceSinkBuilder::from_device(device.clone())
                    .map(|b| b.with_buffer_size(buffer))
                    .and_then(|b| b.open_stream())
                {
                    return Ok(sink);
                }
            }
        }
    }
    DeviceSinkBuilder::open_default_sink().context("opening the speaker")
}

/// Strip what cannot be spoken (markdown symbols) and apply pronunciation fixes.
pub fn for_speech(text: &str) -> String {
    let cleaned: String = text
        .chars()
        .filter(|c| !matches!(c, '*' | '_' | '#' | '`' | '~'))
        .map(|c| if matches!(c, '|' | '>') { ' ' } else { c })
        .collect();
    cleaned
        .split_whitespace()
        .map(|word| {
            // the first run of letters/digits, so "lasthope's" matches "lasthope"
            let core = word
                .find(char::is_alphanumeric)
                .map(|s| {
                    let rest = &word[s..];
                    &rest[..rest.find(|c: char| !c.is_alphanumeric()).unwrap_or(rest.len())]
                })
                .unwrap_or("");
            match PRONUNCIATIONS.iter().find(|(w, _)| w.eq_ignore_ascii_case(core)) {
                Some((_, say)) if !core.is_empty() => word.replacen(core, say, 1),
                _ => word.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Split into sentences on . ? ! (followed by space or end) and line breaks.
/// Decimals like "3.5" stay intact. Terminal punctuation is kept.
pub fn split_sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\n' {
            push_trimmed(&mut out, &mut current);
            continue;
        }
        current.push(c);
        if matches!(c, '.' | '?' | '!') {
            // keep runs like "?!" or "..." together
            while let Some(&n) = chars.peek() {
                if matches!(n, '.' | '?' | '!' | '"' | '\'' | ')') {
                    current.push(n);
                    chars.next();
                } else {
                    break;
                }
            }
            if chars.peek().is_none_or(|n| n.is_whitespace()) {
                push_trimmed(&mut out, &mut current);
            }
        }
    }
    push_trimmed(&mut out, &mut current);
    out
}

fn push_trimmed(out: &mut Vec<String>, current: &mut String) {
    let s = current.trim();
    if s.chars().any(char::is_alphanumeric) {
        out.push(s.to_string());
    }
    current.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_sentence_ends() {
        assert_eq!(
            split_sentences("The work waits. Did you keep it? Speak!"),
            vec!["The work waits.", "Did you keep it?", "Speak!"]
        );
    }

    #[test]
    fn keeps_decimals_and_runs() {
        assert_eq!(
            split_sentences("Version 3.5 is out... Really?! Yes"),
            vec!["Version 3.5 is out...", "Really?!", "Yes"]
        );
    }

    #[test]
    fn splits_lines_and_drops_empty() {
        assert_eq!(
            split_sentences("First line\n\n  \nSecond. . ."),
            vec!["First line", "Second."]
        );
        assert!(split_sentences("  ... ").is_empty());
    }

    /// Kokoro -> Whisper through the real modules, no mic or speaker needed.
    /// Needs the model files: `cargo test --release -- --ignored`
    #[test]
    #[ignore]
    fn voice_round_trip_through_whisper() {
        let engine = Engine::load(Path::new("./kokoro-multi-lang-v1_0")).unwrap();
        let ears = crate::audio_in::Ears::load(Path::new("./models")).unwrap();
        let said = "Did you keep your oath? Twenty five minutes of Rust and the EVM, Lasthope.";
        let mut heard = Vec::new();
        for sentence in split_sentences(&for_speech(said)) {
            let (samples, rate) = engine.synth(&sentence).unwrap().unwrap();
            let rs = sherpa_onnx::LinearResampler::create(rate as i32, 16_000).unwrap();
            heard.push(ears.transcribe(&rs.resample(&samples, true)));
        }
        let heard = heard.join(" ").to_lowercase();
        println!("heard: {heard}");
        for word in ["oath", "minutes", "rust", "evm", "last hope"] {
            assert!(heard.contains(word), "missing {word:?} in {heard:?}");
        }
    }

    /// Opens the real default speaker and plays 300 ms of silence.
    /// `cargo test --release opens_the_speaker -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn opens_the_speaker() {
        let mut sink = open_speaker().expect("speaker");
        sink.log_on_drop(false);
        println!("speaker config: {:?}", sink.config());
        let player = Player::connect_new(sink.mixer());
        player.append(SamplesBuffer::new(1.try_into().unwrap(), 24_000.try_into().unwrap(), vec![0.0f32; 7200]));
        player.sleep_until_end();
    }

    #[test]
    fn speech_cleanup() {
        assert_eq!(for_speech("**Rise**, #seeker"), "Rise, seeker");
        assert_eq!(for_speech("Welcome to LASTHOPE."), "Welcome to Last Hope.");
        assert_eq!(for_speech("lasthope's door"), "Last Hope's door");
        assert_eq!(for_speech("Retrying, the API hit the limit."), "Retrying, the A P I hit the limit.");
    }
}

