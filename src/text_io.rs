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

use crate::audio_in::{self, MicChoice};
use crate::core_loop::{Command, CoreHandle, Event, Input, Pacer, State};
use crate::grimoire::Pick;
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
  /read [next|oath|random]   read a page with her (next unread page by default)
  /page BOOK PAGE         read the page you choose;  /leave  closes the book
  /books                  the shelf;  /book N  makes book N the one you read
  /reindex                look through BOOKS_DIR again for new and changed PDFs
  /mic [N|auto|default|name]   the microphones; choose one (at once, kept across restarts)
  /end                    close the rite (she asks tomorrow's oath; /end again skips it)
  /open                   open the next rite (when the web UI keeps her running)
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
    Books,
    Mics,
}

#[derive(Debug, PartialEq, Eq)]
enum Typed {
    Command(Command),
    Query(Query),
    /// `/mic N`: the Nth microphone of `/mic`.
    MicNumber(usize),
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
        ("open", "") => cmd(Command::OpenRite),
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
        ("read", "" | "next") => cmd(Command::Read { from: Pick::Next }),
        ("read", "oath") => cmd(Command::Read { from: Pick::Oath }),
        ("read", "random") => cmd(Command::Read { from: Pick::Random }),
        ("read", _) => Err("usage: /read [next|oath|random]".to_string()),
        ("page", t) => {
            let nums: Vec<Option<u32>> = t.split_whitespace().map(|n| n.parse().ok()).collect();
            match nums.as_slice() {
                [Some(book), Some(page)] => cmd(Command::ReadPage { book: i64::from(*book), page: *page }),
                _ => Err("usage: /page BOOK PAGE   (book numbers from /books)".to_string()),
            }
        }
        ("leave", "") => cmd(Command::LeavePage),
        ("books", "") => Ok(Typed::Query(Query::Books)),
        ("book", n) => match n.parse() {
            Ok(book) => cmd(Command::SetBook { book }),
            Err(_) => Err("usage: /book N   (numbers from /books)".to_string()),
        },
        ("reindex", "") => cmd(Command::Reindex),
        ("mic" | "mics", "") => Ok(Typed::Query(Query::Mics)),
        ("mic", "auto") => cmd(Command::SetMic { mic: None }),
        ("mic", t) => match t.parse() {
            Ok(0) => Err("usage: /mic N   (numbers from /mic)".to_string()),
            Ok(n) => Ok(Typed::MicNumber(n)),
            Err(_) => cmd(Command::SetMic { mic: Some(t.to_string()) }),
        },
        _ => Err(COMMANDS_HELP.to_string()),
    })
}

async fn answer(memory: &Memory, mics: &MicChoice, query: Query) -> anyhow::Result<String> {
    Ok(match query {
        Query::Mics => {
            let choice = mics.get();
            let listed = tokio::task::spawn_blocking(move || audio_in::describe_mics(choice.as_deref())).await??;
            format!("{listed}\n/mic N chooses one, /mic auto lets her choose, /mic default takes the system's")
        }
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
        Query::Books => {
            let books = memory.books().await?;
            let current = memory.current_book().await?.map(|b| b.id);
            if books.is_empty() {
                return Ok("the shelf is empty (BOOKS_DIR, then /reindex)".into());
            }
            let mut out: Vec<String> = books
                .iter()
                .map(|b| {
                    let state = match (b.status.as_str(), b.missing) {
                        (_, true) => "gone from the folder".to_string(),
                        ("ok", _) => format!("{} pages, read to page {}", b.pages, b.last_page),
                        ("no_text", _) => format!("{} pages, no text (a scan?)", b.pages),
                        _ => format!("unreadable: {}", b.error.as_deref().unwrap_or("?")),
                    };
                    let mark = if Some(b.id) == current { "*" } else { " " };
                    format!(" {mark}{:>3}  {}  ({state})", b.id, b.title)
                })
                .collect();
            let (answered, right) = memory.question_tally().await?;
            out.push(format!("questions answered: {answered}, right: {right}"));
            out.join("\n")
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
    mics: MicChoice,
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
                        match runtime.block_on(answer(&memory, &mics, q)) {
                            Ok(text) => eprintln!("{text}"),
                            Err(e) => eprintln!("  ! {e:#}"),
                        }
                    }
                    Some(Ok(Typed::MicNumber(n))) => match audio_in::mics(None) {
                        Ok(found) => match found.inputs.get(n - 1) {
                            Some(input) => handle.command(Command::SetMic { mic: Some(input.name.clone()) }),
                            None => eprintln!("there is no microphone {n} (see /mic)"),
                        },
                        Err(e) => eprintln!("  ! {e:#}"),
                    },
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
    let mut last_page = None;
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
                    State::Closed => continue, // "the rite is closed" is printed on SessionEnded
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
            Event::MemoryChanged | Event::ShelfChanged => {}
            Event::ReadingChanged { reading } => {
                let page = reading.map(|r| (r.title, r.page, r.pages));
                if page != last_page {
                    // a closed book is told by its own notice
                    if let Some((title, n, of)) = &page {
                        eprintln!("  · the grimoire: {title}, page {n} of {of}");
                    }
                    last_page = page;
                }
            }
            Event::Notice { text } => eprintln!("  · {text}"),
            Event::Error { message } => eprintln!("  ! {message}"),
            Event::SessionEnded => eprintln!("  · the rite is closed"),
            Event::Goodbye => break,
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
        assert_eq!(parse_command("/read"), cmd(Command::Read { from: Pick::Next }));
        assert_eq!(parse_command("/read oath"), cmd(Command::Read { from: Pick::Oath }));
        assert_eq!(parse_command("/page 2 41"), cmd(Command::ReadPage { book: 2, page: 41 }));
        assert!(matches!(parse_command("/page 2"), Some(Err(_))));
        assert_eq!(parse_command("/book 3"), cmd(Command::SetBook { book: 3 }));
        assert_eq!(parse_command("/mic"), Some(Ok(Typed::Query(Query::Mics))));
        assert_eq!(parse_command("/mic 2"), Some(Ok(Typed::MicNumber(2))));
        assert_eq!(parse_command("/mic auto"), cmd(Command::SetMic { mic: None }));
        assert_eq!(parse_command("/mic MacBook"), cmd(Command::SetMic { mic: Some("MacBook".into()) }));
        assert!(matches!(parse_command("/mic 0"), Some(Err(_))));
        assert_eq!(parse_command("talk to me"), None);
    }
}
