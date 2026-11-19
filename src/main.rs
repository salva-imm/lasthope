//! lasthope: a local voice companion.
//!
//! Ears (Whisper + VAD) and voice (Kokoro) run locally through sherpa-onnx;
//! only the LLM call leaves the machine. See readme.md.

mod audio_in;
mod config;
mod core_loop;
mod grimoire;
mod llm;
mod memory;
mod persona;
mod rite;
mod speech_out;
mod streak;
mod text_io;
mod timer;
mod web;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::audio_in::Gate;
use crate::config::Config;
use crate::core_loop::{Core, Input};
use crate::memory::Memory;
use crate::llm::LlmClient;
use crate::persona::Persona;
use crate::speech_out::{Mouth, Voice};

const USAGE: &str = "\
usage: lasthope [--text] [--no-ui] | --mics

  --text   type instead of speaking: stdin replaces the mic, stdout the speaker
           (no speech models are loaded)
  --no-ui  no web UI; the program ends when the rite closes
  --mics   list microphones and show which one she will listen through

settings (environment or .env): LLM_BASE_URL, LLM_API_KEY, LLM_MODEL
optional: LLM_TEMPERATURE (0.8), PERSONA_PATH (./persona.md), DB_PATH (./lasthope.db),
          MIC_DEVICE (part of a mic name, or \"default\"), END_OF_TURN_MS (1200),
          KOKORO_DIR (./kokoro-multi-lang-v1_0), MODELS_DIR (./models),
          UI_BIND (127.0.0.1; 0.0.0.0 lets your phone in, with a key), UI_PORT (7700), UI_DIR (./ui),
          BOOKS_DIR (./books: the grimoire's PDFs)

spoken commands: \"close the rite\" / \"end the session\" ends the session,
                 \"begin / pause / reset the vigil\" drives the focus timer,
                 \"read the next page\" / \"a random page\" / \"read a page for my oath\" /
                 \"leave the page\" opens and closes the grimoire,
                 \"please say …\" speaks the rest into output.wav";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}\n\n{}", text_io::COMMANDS_HELP);
        return Ok(());
    }
    if args.iter().any(|a| a == "--mics") {
        let pref = std::env::var("MIC_DEVICE").ok().filter(|v| !v.is_empty());
        println!("{}", audio_in::describe_mics(pref.as_deref())?);
        return Ok(());
    }
    if let Some(bad) = args.iter().find(|a| *a != "--text" && *a != "--no-ui") {
        anyhow::bail!("unknown argument {bad:?}\n\n{USAGE}");
    }
    let text_mode = args.iter().any(|a| a == "--text");
    let want_ui = !args.iter().any(|a| a == "--no-ui");

    let cfg = Config::load()?;
    let llm = LlmClient::new(cfg.llm.clone())?;
    let persona = Persona::new(cfg.persona_path.clone(), cfg.temperature);
    let memory = Memory::open(&cfg.db_path).await?;
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

    text_io::spawn_stdin(
        handle.clone(),
        pacer.clone(),
        memory.clone(),
        tokio::runtime::Handle::current(),
        text_mode,
    );
    let echo_user = !text_mode || !text_io::stdin_is_terminal();
    let printer = tokio::spawn(text_io::print_events(handle.subscribe(), echo_user));
    {
        let h = handle.clone();
        tokio::spawn(async move {
            // first Ctrl-C: close the session (summary included); second: quit now
            if tokio::signal::ctrl_c().await.is_ok() {
                eprintln!("  · closing (Ctrl-C again to quit at once)");
                h.send(Input::Shutdown);
            }
            if tokio::signal::ctrl_c().await.is_ok() {
                std::process::exit(130);
            }
        });
    }

    eprintln!(
        "lasthope: {} mode, mind = {}, persona = {}, memory = {} — /help for commands",
        if text_mode { "text" } else { "voice" },
        llm.model(),
        persona.path().display(),
        cfg.db_path.display()
    );

    // the web UI; while it is up, closing the rite does not end the program
    let ui_up = want_ui && {
        let mode = if text_mode { "text" } else { "voice" };
        let books = cfg.books_dir.clone();
        match web::start(cfg.ui_bind, cfg.ui_port, cfg.ui_dir.clone(), handle.clone(), memory.clone(), mode, books).await {
            Ok(links) => {
                for link in links {
                    eprintln!("chamber: {link}");
                }
                true
            }
            Err(e) => {
                eprintln!("  ! no web UI: {e:#}");
                false
            }
        }
    };

    Core::new(llm, persona, memory, mouth, gate, pacer, handle, inputs, cfg.books_dir.clone()).run(ui_up).await;
    let _ = tokio::time::timeout(Duration::from_secs(1), printer).await;
    Ok(())
}
