//! lasthope: a local voice companion.
//!
//! Ears (Whisper + VAD) and voice (Kokoro) run locally through sherpa-onnx;
//! only the LLM call leaves the machine. See readme.md.

mod audio_in;
mod config;
mod core_loop;
mod llm;
mod memory;
mod persona;
mod speech_out;
mod text_io;
mod timer;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::audio_in::Gate;
use crate::config::Config;
use crate::core_loop::{Command, Core, Input};
use crate::llm::LlmClient;
use crate::speech_out::{Mouth, Voice};

const USAGE: &str = "\
usage: lasthope [--text]

  --text   type instead of speaking: stdin replaces the mic, stdout the speaker
           (no speech models are loaded)

settings (environment or .env): LLM_BASE_URL, LLM_API_KEY, LLM_MODEL
optional: KOKORO_DIR (./kokoro-multi-lang-v1_0), MODELS_DIR (./models)";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}\n\n{}", text_io::COMMANDS_HELP);
        return Ok(());
    }
    if let Some(bad) = args.iter().find(|a| *a != "--text") {
        anyhow::bail!("unknown argument {bad:?}\n\n{USAGE}");
    }
    let text_mode = args.iter().any(|a| a == "--text");

    let cfg = Config::load()?;
    let llm = LlmClient::new(cfg.llm.clone())?;
    let (handle, inputs) = core_loop::channel();
    let gate = Gate::new(false);
    let pacer = core_loop::Pacer::default();

    let mouth: Arc<dyn Mouth> = if text_mode {
        Arc::new(text_io::TextMouth)
    } else {
        let t = Instant::now();
        eprintln!("loading voice and ears…");
        let voice = Voice::spawn(&cfg)?;
        let h = handle.clone();
        audio_in::spawn(&cfg, gate.clone(), move |text| h.send(Input::Heard(text)))?;
        eprintln!("models loaded in {:.1}s", t.elapsed().as_secs_f32());
        Arc::new(voice)
    };

    text_io::spawn_stdin(handle.clone(), pacer.clone(), text_mode);
    let echo_user = !text_mode || !text_io::stdin_is_terminal();
    let printer = tokio::spawn(text_io::print_events(handle.subscribe(), echo_user));
    {
        let h = handle.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                h.command(Command::EndSession);
            }
        });
    }

    eprintln!(
        "lasthope: {} mode, mind = {} — /help for commands, Ctrl-C to end",
        if text_mode { "text" } else { "voice" },
        llm.model()
    );

    let memory = Core::new(llm, mouth, gate, pacer, handle, inputs).run().await;
    let _ = tokio::time::timeout(Duration::from_secs(1), printer).await;
    eprintln!("  · {} lines spoken this session", memory.transcript().len());
    Ok(())
}
