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
use crate::memory::{Memory, Speaker};
use crate::speech_out::Mouth;

pub const COMMANDS_HELP: &str = "\
commands:
  /mute  /unmute          deafen her (closes the mic) / let her hear again
  /stop                   silence her: cut off the current reply
  /sleep /wake            pause / resume listening
  /timer start [min]|pause|reset   the vigil (25 min unless told)
  /vigil [yesterday] [min]   a vigil you kept while she was not running (25 min)
  /kept  /broken          your verdict on the last oath
  /status                 the great work, the chain of seals, the oaths
  /facts                  what she knows of you;  /forget N  strikes fact N out
  /mission [text]         show or rename the great work
  /search words           search everything ever said
  /end                    close the rite (she asks tomorrow's oath; /end again skips it)
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

/// Read-only questions answered straight from memory.
#[derive(Debug, PartialEq, Eq)]
enum Query {
    Status,
    Facts,
    Mission,
    Search(String),
}

#[derive(Debug, PartialEq, Eq)]
enum Typed {
    Command(Command),
    Query(Query),
}

fn parse_command(line: &str) -> Option<Result<Typed, String>> {
    let rest = line.strip_prefix('/')?;
    let (head, tail) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let tail = tail.trim();
    let cmd = |c| Ok(Typed::Command(c));
    Some(match (head, tail) {
        ("mute" | "deafen", "") => cmd(Command::Mute { on: true }),
        ("unmute", "") => cmd(Command::Mute { on: false }),
        ("stop" | "silence", "") => cmd(Command::StopSpeaking),
        ("sleep" | "slumber", "") => cmd(Command::Sleep { on: true }),
        ("wake", "") => cmd(Command::Sleep { on: false }),
        ("end" | "quit" | "close", "") => cmd(Command::EndSession),
        ("timer", "pause") | ("hold", "") => cmd(Command::TimerPause),
        ("timer", "reset") | ("anew", "") => cmd(Command::TimerReset),
        ("timer", t) | ("begin", t) if head == "begin" || t.starts_with("start") => {
            let n = if head == "begin" { t } else { t["start".len()..].trim() };
            match n {
                "" => cmd(Command::TimerStart { minutes: None }),
                n => match n.parse() {
                    Ok(m) => cmd(Command::TimerStart { minutes: Some(m) }),
                    Err(_) => Err("usage: /timer start [minutes]".to_string()),
                },
            }
        }
        ("timer", _) => Err("usage: /timer start [minutes] | /timer pause | /timer reset".to_string()),
        ("vigil", t) => {
            let (yesterday, n) = match t.strip_prefix("yesterday") {
                Some(rest) => (true, rest.trim()),
                None => (false, t),
            };
            match n {
                "" => cmd(Command::LogVigil { minutes: 25, yesterday }),
                n => match n.parse() {
                    Ok(minutes) => cmd(Command::LogVigil { minutes, yesterday }),
                    Err(_) => Err("usage: /vigil [yesterday] [minutes]".to_string()),
                },
            }
        }
        ("kept", "") => cmd(Command::OathVerdict { kept: true }),
        ("broken", "") => cmd(Command::OathVerdict { kept: false }),
        ("mission", "") => Ok(Typed::Query(Query::Mission)),
        ("mission", text) => cmd(Command::SetMission { text: text.to_string() }),
        ("forget", n) => match n.parse() {
            Ok(id) => cmd(Command::ForgetFact { id }),
            Err(_) => Err("usage: /forget N   (numbers from /facts)".to_string()),
        },
        ("status", "") => Ok(Typed::Query(Query::Status)),
        ("facts", "") => Ok(Typed::Query(Query::Facts)),
        ("search", "") => Err("usage: /search words".to_string()),
        ("search", q) => Ok(Typed::Query(Query::Search(q.to_string()))),
        _ => Err(COMMANDS_HELP.to_string()),
    })
}

async fn answer(memory: &Memory, query: Query) -> anyhow::Result<String> {
    Ok(match query {
        Query::Mission => format!("the great work: {}", memory.mission().await?),
        Query::Facts => {
            let facts = memory.facts().await?;
            if facts.is_empty() {
                "she knows nothing of you yet".into()
            } else {
                facts.iter().map(|f| format!("  {:>3}  {}", f.id, f.text)).collect::<Vec<_>>().join("\n")
            }
        }
        Query::Status => {
            let today = memory.today().await?;
            let mut out = format!(
                "the great work: {}\nthe chain of seals: {}",
                memory.mission().await?,
                memory.streak().await?.current(today)
            );
            let focus = memory.focus_on(today).await?;
            out += &format!("\nfocused today: {} min in {} vigil(s)", focus.minutes, focus.vigils);
            if let Some(o) = memory.pending_oath(today, 0).await? {
                out += &format!("\noath due {} (awaits /kept or /broken): {}", o.day, o.text);
            }
            if let Some(o) = memory.upcoming_oath(today).await? {
                out += &format!("\noath for {} {}: {}", o.day.weekday(), o.day, o.text);
            }
            out
        }
        Query::Search(q) => {
            let hits = memory.search(&q, 10, None).await?;
            if hits.is_empty() {
                "nothing found".into()
            } else {
                hits.iter()
                    .map(|h| {
                        let who = if h.speaker == Speaker::Me { "you" } else { "her" };
                        format!("  {} #{} {who}: {}", h.day, h.session, h.text)
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
    })
}

/// Read stdin on its own thread. Lines starting with `/` are commands, the
/// rest is treated as heard speech. A typed line waits until her current turn
/// is over (see `Pacer`); commands go through at once. On EOF, text mode ends
/// the session.
pub fn spawn_stdin(
    handle: CoreHandle,
    pacer: Pacer,
    memory: Memory,
    runtime: tokio::runtime::Handle,
    end_on_eof: bool,
) {
    std::thread::Builder::new()
        .name("stdin".into())
        .spawn(move || {
            let interactive = stdin_is_terminal();
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                match parse_command(line) {
                    Some(Ok(Typed::Command(cmd))) => handle.command(cmd),
                    Some(Ok(Typed::Query(q))) => {
                        // answer once what was said before has been handled
                        pacer.wait_free();
                        match runtime.block_on(answer(&memory, q)) {
                            Ok(text) => eprintln!("{text}"),
                            Err(e) => eprintln!("  ! {e:#}"),
                        }
                    }
                    Some(Err(help)) => eprintln!("{help}"),
                    None => {
                        if interactive && !pacer.is_free() {
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
            Event::SessionStarted { session } => eprintln!("  · the rite opens (session #{session})"),
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
        let cmd = |c| Some(Ok(Typed::Command(c)));
        assert_eq!(parse_command("/mute"), cmd(Command::Mute { on: true }));
        assert_eq!(parse_command("/timer  start"), cmd(Command::TimerStart { minutes: None }));
        assert_eq!(parse_command("/timer start 50"), cmd(Command::TimerStart { minutes: Some(50) }));
        assert_eq!(parse_command("/begin 10"), cmd(Command::TimerStart { minutes: Some(10) }));
        assert_eq!(parse_command("/vigil"), cmd(Command::LogVigil { minutes: 25, yesterday: false }));
        assert_eq!(parse_command("/vigil yesterday 40"), cmd(Command::LogVigil { minutes: 40, yesterday: true }));
        assert!(matches!(parse_command("/vigil soon"), Some(Err(_))));
        assert_eq!(parse_command("/end"), cmd(Command::EndSession));
        assert_eq!(parse_command("/kept"), cmd(Command::OathVerdict { kept: true }));
        assert_eq!(parse_command("/forget 12"), cmd(Command::ForgetFact { id: 12 }));
        assert_eq!(
            parse_command("/mission Master Rust."),
            cmd(Command::SetMission { text: "Master Rust.".into() })
        );
        assert_eq!(parse_command("/mission"), Some(Ok(Typed::Query(Query::Mission))));
        assert_eq!(parse_command("/search storage slots"), Some(Ok(Typed::Query(Query::Search("storage slots".into())))));
        assert!(matches!(parse_command("/forget x"), Some(Err(_))));
        assert!(matches!(parse_command("/nope"), Some(Err(_))));
        assert!(matches!(parse_command("/timer"), Some(Err(e)) if e.starts_with("usage: /timer")));
        assert_eq!(parse_command("talk to me"), None);
    }
}
