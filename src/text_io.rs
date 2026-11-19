//! Terminal side: typed input and slash-commands from stdin, a printer for
//! core events, and the text-mode stand-in for her voice.
//!
//! In `--text` mode stdin replaces the mic and stdout replaces the speaker.
//! In voice mode stdin still accepts slash-commands and typed lines.

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Result, anyhow};
use tokio::sync::{broadcast, oneshot};

use crate::core_loop::{Command, CoreHandle, Event, Input, Pacer, State};
use crate::speech_out::Mouth;

pub const COMMANDS_HELP: &str = "\
commands:
  /mute  /unmute          deafen her (closes the mic) / let her hear again
  /stop                   silence her: cut off the current reply
  /sleep /wake            pause / resume listening
  /timer start|pause|reset   the vigil (25 min)
  /end                    close the rite (end the session)
  /help";

/// Text mode "voice": the reply is already printed from the SheSpoke event.
pub struct TextMouth;

impl Mouth for TextMouth {
    fn speak(&self, _text: String, _cancel: Arc<AtomicBool>) -> oneshot::Receiver<Result<()>> {
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(Ok(()));
        rx
    }

    fn save_wav(&self, _text: String, _path: PathBuf) -> oneshot::Receiver<Result<()>> {
        let (tx, rx) = oneshot::channel();
        let _ = tx.send(Err(anyhow!("saving speech needs voice mode (run without --text)")));
        rx
    }
}

fn parse_command(line: &str) -> Option<Result<Command, String>> {
    let rest = line.strip_prefix('/')?;
    let words: Vec<&str> = rest.split_whitespace().collect();
    Some(match words.as_slice() {
        ["mute"] | ["deafen"] => Ok(Command::Mute { on: true }),
        ["unmute"] => Ok(Command::Mute { on: false }),
        ["stop"] | ["silence"] => Ok(Command::StopSpeaking),
        ["sleep"] | ["slumber"] => Ok(Command::Sleep { on: true }),
        ["wake"] => Ok(Command::Sleep { on: false }),
        ["end"] | ["quit"] | ["close"] => Ok(Command::EndSession),
        ["timer", "start"] | ["begin"] => Ok(Command::TimerStart),
        ["timer", "pause"] | ["hold"] => Ok(Command::TimerPause),
        ["timer", "reset"] | ["anew"] => Ok(Command::TimerReset),
        ["timer", ..] => Err("usage: /timer start | /timer pause | /timer reset".to_string()),
        _ => Err(COMMANDS_HELP.to_string()),
    })
}

/// Read stdin on its own thread. Lines starting with `/` are commands, the
/// rest is treated as heard speech. A typed line waits until her current turn
/// is over (see `Pacer`); commands go through at once. On EOF, text mode ends
/// the session.
pub fn spawn_stdin(handle: CoreHandle, pacer: Pacer, end_on_eof: bool) {
    std::thread::Builder::new()
        .name("stdin".into())
        .spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                match parse_command(line) {
                    Some(Ok(cmd)) => handle.command(cmd),
                    Some(Err(help)) => eprintln!("{help}"),
                    None => {
                        if !pacer.is_free() {
                            eprintln!("  · she is not done yet; your words wait their turn");
                        }
                        pacer.wait_and_claim();
                        handle.send(Input::Heard(line.to_string()));
                    }
                }
            }
            if end_on_eof {
                handle.send(Input::EndOfInput);
            }
        })
        .expect("spawn stdin thread");
}

/// Print core events until the session ends.
/// `echo_user`: print what I said (voice mode, or piped input).
pub async fn print_events(mut rx: broadcast::Receiver<Event>, echo_user: bool) {
    let mut last_running = None;
    let mut last_printed = None;
    loop {
        let event = match rx.recv().await {
            Ok(e) => e,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        };
        match event {
            Event::StateChanged { state, muted } => {
                let s = match state {
                    State::Asleep => "she sleeps",
                    State::Listening => "she listens",
                    State::Thinking => "she ponders",
                    State::Speaking => "she speaks",
                };
                eprintln!("  · {s}{}", if muted { " (deafened)" } else { "" });
            }
            Event::UserSpoke { text } => {
                if echo_user {
                    println!("you: {text}");
                }
            }
            Event::SheSpoke { text } => println!("her: {text}"),
            Event::TimerTick { remaining_secs, running, .. } => {
                // print on start/pause/finish and once a minute, not every second
                if last_running != Some(running)
                    || (running && remaining_secs % 60 == 0 && last_printed != Some(remaining_secs))
                {
                    last_printed = Some(remaining_secs);
                    eprintln!(
                        "  · vigil {:02}:{:02} {}",
                        remaining_secs / 60,
                        remaining_secs % 60,
                        if running { "running" } else { "held" }
                    );
                }
                last_running = Some(running);
            }
            Event::MemoryChanged => {}
            Event::Notice { text } => eprintln!("  · {text}"),
            Event::Error { message } => eprintln!("  ! {message}"),
            Event::SessionEnded => {
                eprintln!("  · the rite is closed");
                break;
            }
        }
        let _ = std::io::stdout().flush();
    }
}

pub fn stdin_is_terminal() -> bool {
    std::io::stdin().is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands() {
        assert_eq!(parse_command("/mute"), Some(Ok(Command::Mute { on: true })));
        assert_eq!(parse_command("/timer  start"), Some(Ok(Command::TimerStart)));
        assert_eq!(parse_command("/end"), Some(Ok(Command::EndSession)));
        assert!(matches!(parse_command("/nope"), Some(Err(_))));
        assert!(matches!(parse_command("/timer"), Some(Err(e)) if e.starts_with("usage: /timer")));
        assert_eq!(parse_command("talk to me"), None);
    }
}
