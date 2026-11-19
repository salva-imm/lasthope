//! The core: one task that owns all state. It takes `Input`s (what was heard,
//! commands) and publishes `Event`s. Voice mode, text mode and (phase 5) the
//! web UI all drive it through the same `CoreHandle`.
//!
//! A reply is a "turn": Thinking (LLM request) -> Speaking (voice) -> Idle.
//! Turns run as background tasks so commands (stop, mute, end) are handled
//! immediately, even mid-reply. Results carry a turn id; stale ones are dropped.

use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::audio_in::Gate;
use crate::llm::LlmClient;
use crate::memory::{Memory, Speaker};
use crate::persona;
use crate::speech_out::Mouth;
use crate::timer::{DEFAULT_VIGIL, Vigil};

const WAV_PATH: &str = "output.wav";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Asleep,
    Listening,
    Thinking,
    Speaking,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    /// Deafen her: close the mic and ignore input.
    Mute { on: bool },
    /// Cut off the current reply (speech, or a request still in flight).
    StopSpeaking,
    /// Pause listening; she finishes what she is saying.
    Sleep { on: bool },
    EndSession,
    TimerStart,
    TimerPause,
    TimerReset,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    StateChanged { state: State, muted: bool },
    UserSpoke { text: String },
    SheSpoke { text: String },
    TimerTick { remaining_secs: u64, total_secs: u64, running: bool },
    /// Facts/mission/streak changed (phase 3).
    #[allow(dead_code)]
    MemoryChanged,
    Notice { text: String },
    Error { message: String },
    SessionEnded,
}

#[derive(Debug)]
pub enum Input {
    Heard(String),
    Command(Command),
    /// Text mode stdin closed: end the session once the current turn is done.
    EndOfInput,
}

/// Cheap, cloneable way to talk to the core and watch it.
#[derive(Clone)]
pub struct CoreHandle {
    input: mpsc::UnboundedSender<Input>,
    events: broadcast::Sender<Event>,
}

impl CoreHandle {
    pub fn send(&self, input: Input) {
        let _ = self.input.send(input);
    }

    pub fn command(&self, cmd: Command) {
        self.send(Input::Command(cmd));
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }
}

/// Paces a typed-input reader: it waits until every line it sent has been
/// handled and her turn is over, so piped scripts behave like a conversation.
/// Pending count and busy flag change under one lock, so there is no gap
/// where a second line can slip in while the first is still in flight.
#[derive(Clone, Default)]
pub struct Pacer(Arc<(Mutex<PacerState>, Condvar)>);

#[derive(Default)]
struct PacerState {
    busy: bool,
    pending: u64,
}

impl Pacer {
    /// Block until she is free, then reserve the next line.
    pub fn wait_and_claim(&self) {
        let (lock, cvar) = &*self.0;
        let mut s = lock.lock().unwrap();
        while s.busy || s.pending > 0 {
            s = cvar.wait(s).unwrap();
        }
        s.pending += 1;
    }

    fn update(&self, handled: u64, busy: bool) {
        let (lock, cvar) = &*self.0;
        let mut s = lock.lock().unwrap();
        s.pending = s.pending.saturating_sub(handled);
        s.busy = busy;
        cvar.notify_all();
    }
}

pub fn channel() -> (CoreHandle, mpsc::UnboundedReceiver<Input>) {
    let (input, rx) = mpsc::unbounded_channel();
    let (events, _) = broadcast::channel(256);
    (CoreHandle { input, events }, rx)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Turn {
    Idle,
    Thinking,
    Speaking,
    Saving,
}

enum Done {
    Reply(Result<String>),
    Spoke(Result<()>),
    Saved(Result<()>),
}

#[derive(PartialEq, Eq)]
enum Flow {
    Continue,
    Stop,
}

/// Phase 1 keeps the original trigger phrases; phase 2 removes them.
#[derive(Debug, PartialEq, Eq)]
enum Route {
    SaveWav(String),
    Ask(String),
    Ignore,
}

fn route(text: &str) -> Route {
    const SAVE: &[&str] = &["please generate", "please store", "please say"];
    const ASK: &[&str] = &["please ai", "colon colon ai", "::ai", "talk to me"];
    let lower = text.to_lowercase();
    let rest = |prefix: &str| {
        text.get(prefix.len()..)
            .unwrap_or(&lower[prefix.len()..])
            .trim_start_matches(|c: char| c.is_whitespace() || matches!(c, ',' | '.' | ':' | '-'))
            .trim()
            .to_string()
    };
    if let Some(p) = SAVE.iter().find(|p| lower.starts_with(*p)) {
        Route::SaveWav(rest(p))
    } else if let Some(p) = ASK.iter().find(|p| lower.starts_with(*p)) {
        Route::Ask(rest(p))
    } else {
        Route::Ignore
    }
}

pub struct Core {
    llm: LlmClient,
    mouth: Arc<dyn Mouth>,
    gate: Gate,
    pacer: Pacer,
    heard_since_refresh: u64,
    handle: CoreHandle,
    inputs: mpsc::UnboundedReceiver<Input>,
    done_tx: mpsc::UnboundedSender<(u64, Done)>,
    done_rx: mpsc::UnboundedReceiver<(u64, Done)>,
    memory: Memory,
    vigil: Vigil,
    muted: bool,
    asleep: bool,
    turn: Turn,
    turn_id: u64,
    cancel: Option<Arc<AtomicBool>>,
    task: Option<JoinHandle<()>>,
    end_when_idle: bool,
    published: Option<(State, bool)>,
}

impl Core {
    pub fn new(
        llm: LlmClient,
        mouth: Arc<dyn Mouth>,
        gate: Gate,
        pacer: Pacer,
        handle: CoreHandle,
        inputs: mpsc::UnboundedReceiver<Input>,
    ) -> Self {
        let (done_tx, done_rx) = mpsc::unbounded_channel();
        Self {
            llm,
            mouth,
            gate,
            pacer,
            heard_since_refresh: 0,
            handle,
            inputs,
            done_tx,
            done_rx,
            memory: Memory::default(),
            vigil: Vigil::new(DEFAULT_VIGIL),
            muted: false,
            asleep: false,
            turn: Turn::Idle,
            turn_id: 0,
            cancel: None,
            task: None,
            end_when_idle: false,
            published: None,
        }
    }

    /// Run until the session ends. Returns what was said.
    pub async fn run(mut self) -> Memory {
        self.refresh();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            let flow = tokio::select! {
                input = self.inputs.recv() => match input {
                    Some(input) => self.on_input(input),
                    None => Flow::Stop,
                },
                Some((id, done)) = self.done_rx.recv() => self.on_done(id, done),
                _ = tick.tick() => { self.on_tick(); Flow::Continue }
            };
            self.refresh();
            if flow == Flow::Stop || (self.end_when_idle && self.turn == Turn::Idle) {
                break;
            }
        }
        self.shutdown();
        self.memory
    }

    fn emit(&self, event: Event) {
        let _ = self.handle.events.send(event);
    }

    fn notice(&self, text: impl Into<String>) {
        self.emit(Event::Notice { text: text.into() });
    }

    fn state(&self) -> State {
        match self.turn {
            Turn::Speaking => State::Speaking,
            Turn::Thinking | Turn::Saving => State::Thinking,
            Turn::Idle if self.asleep => State::Asleep,
            Turn::Idle => State::Listening,
        }
    }

    /// Open the mic only when she is idle, awake and not deafened, and publish
    /// state changes.
    fn refresh(&mut self) {
        self.gate.set(self.turn == Turn::Idle && !self.asleep && !self.muted);
        self.pacer.update(std::mem::take(&mut self.heard_since_refresh), self.turn != Turn::Idle);
        let now = (self.state(), self.muted);
        if self.published != Some(now) {
            self.published = Some(now);
            self.emit(Event::StateChanged { state: now.0, muted: now.1 });
        }
    }

    fn on_input(&mut self, input: Input) -> Flow {
        match input {
            Input::Heard(text) => {
                self.heard_since_refresh += 1;
                self.on_heard(text.trim());
            }
            Input::Command(cmd) => return self.on_command(cmd),
            Input::EndOfInput => self.end_when_idle = true,
        }
        Flow::Continue
    }

    fn on_heard(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.muted || self.state() != State::Listening {
            self.notice(format!("not listening ({:?}{}), ignored: {text}",
                self.state(), if self.muted { ", deafened" } else { "" }));
            return;
        }
        self.memory.record(Speaker::Me, text);
        self.emit(Event::UserSpoke { text: text.to_string() });
        match route(text) {
            Route::Ask(prompt) if !prompt.is_empty() => self.start_reply(prompt),
            Route::SaveWav(content) if !content.is_empty() => self.start_save(content),
            Route::Ask(_) | Route::SaveWav(_) => self.notice("say something after the trigger phrase"),
            Route::Ignore => self.notice(
                "no trigger phrase; start with \"talk to me\" (or \"please say\" to save output.wav)",
            ),
        }
    }

    fn on_command(&mut self, cmd: Command) -> Flow {
        let now = Instant::now();
        match cmd {
            Command::Mute { on } => self.muted = on,
            Command::Sleep { on } => self.asleep = on,
            Command::StopSpeaking => self.stop_turn(),
            Command::EndSession => return Flow::Stop,
            Command::TimerStart => {
                self.vigil.start(now);
                self.emit_tick(now);
            }
            Command::TimerPause => {
                self.vigil.pause(now);
                self.emit_tick(now);
            }
            Command::TimerReset => {
                self.vigil.reset();
                self.emit_tick(now);
            }
        }
        Flow::Continue
    }

    fn on_done(&mut self, id: u64, done: Done) -> Flow {
        if id != self.turn_id || self.turn == Turn::Idle {
            return Flow::Continue; // a turn that was cancelled
        }
        self.task = None;
        match done {
            Done::Reply(Ok(text)) => {
                self.memory.record(Speaker::Her, &text);
                self.emit(Event::SheSpoke { text: text.clone() });
                self.start_speaking(text);
                return Flow::Continue;
            }
            Done::Reply(Err(e)) => self.emit(Event::Error { message: format!("{e:#}") }),
            Done::Spoke(Err(e)) => self.emit(Event::Error { message: format!("voice: {e:#}") }),
            Done::Spoke(Ok(())) => {}
            Done::Saved(Ok(())) => self.notice(format!("saved speech to {WAV_PATH}")),
            Done::Saved(Err(e)) => self.emit(Event::Error { message: format!("save: {e:#}") }),
        }
        self.turn = Turn::Idle;
        self.cancel = None;
        Flow::Continue
    }

    fn on_tick(&mut self) {
        let now = Instant::now();
        if self.vigil.running() {
            let finished = self.vigil.poll_finished(now);
            self.emit_tick(now);
            if finished {
                self.notice("the vigil is complete");
            }
        }
    }

    fn emit_tick(&self, now: Instant) {
        self.emit(Event::TimerTick {
            remaining_secs: self.vigil.remaining(now).as_secs_f64().ceil() as u64,
            total_secs: self.vigil.total().as_secs(),
            running: self.vigil.running(),
        });
    }

    fn next_turn(&mut self, turn: Turn) -> u64 {
        self.turn_id += 1;
        self.turn = turn;
        self.turn_id
    }

    fn start_reply(&mut self, prompt: String) {
        let id = self.next_turn(Turn::Thinking);
        let llm = self.llm.clone();
        let tx = self.done_tx.clone();
        self.task = Some(tokio::spawn(async move {
            let reply = llm.chat(&persona::messages(&prompt), persona::SAMPLING).await;
            let _ = tx.send((id, Done::Reply(reply)));
        }));
    }

    fn start_speaking(&mut self, text: String) {
        self.turn = Turn::Speaking;
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Some(cancel.clone());
        let rx = self.mouth.speak(text, cancel);
        self.spawn_wait(rx, Done::Spoke);
    }

    fn start_save(&mut self, content: String) {
        self.next_turn(Turn::Saving);
        let rx = self.mouth.save_wav(content, PathBuf::from(WAV_PATH));
        self.spawn_wait(rx, Done::Saved);
    }

    fn spawn_wait(
        &mut self,
        rx: tokio::sync::oneshot::Receiver<Result<()>>,
        wrap: fn(Result<()>) -> Done,
    ) {
        let id = self.turn_id;
        let tx = self.done_tx.clone();
        self.task = Some(tokio::spawn(async move {
            let res = rx.await.unwrap_or_else(|_| Err(anyhow!("voice thread is gone")));
            let _ = tx.send((id, wrap(res)));
        }));
    }

    /// Cut off whatever she is doing. Speech stops at the next audio buffer;
    /// a pending LLM request is abandoned.
    fn stop_turn(&mut self) {
        match self.turn {
            Turn::Speaking => {
                if let Some(c) = &self.cancel {
                    c.store(true, Ordering::Relaxed);
                }
                // stay Speaking until the voice thread confirms it has stopped
            }
            Turn::Thinking => {
                if let Some(t) = self.task.take() {
                    t.abort();
                }
                self.turn = Turn::Idle;
                self.notice("reply abandoned");
            }
            Turn::Saving | Turn::Idle => {}
        }
    }

    fn shutdown(&mut self) {
        if let Some(c) = &self.cancel {
            c.store(true, Ordering::Relaxed);
        }
        if let Some(t) = self.task.take() {
            t.abort();
        }
        self.gate.set(false);
        self.pacer.update(u64::MAX, false); // release a waiting stdin reader
        self.emit(Event::SessionEnded);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_trigger_phrases() {
        assert_eq!(route("Talk to me, what is an oath?"), Route::Ask("what is an oath?".into()));
        assert_eq!(route("please AI explain gas"), Route::Ask("explain gas".into()));
        assert_eq!(route("Please say Hello there."), Route::SaveWav("Hello there.".into()));
        assert_eq!(route("talk to me"), Route::Ask(String::new()));
        assert_eq!(route("what time is it"), Route::Ignore);
    }

    #[test]
    fn commands_have_stable_json() {
        let c: Command = serde_json::from_str(r#"{"cmd":"mute","on":true}"#).unwrap();
        assert_eq!(c, Command::Mute { on: true });
        let c: Command = serde_json::from_str(r#"{"cmd":"timer_start"}"#).unwrap();
        assert_eq!(c, Command::TimerStart);
        let e = serde_json::to_string(&Event::StateChanged { state: State::Listening, muted: false }).unwrap();
        assert_eq!(e, r#"{"event":"state_changed","state":"listening","muted":false}"#);
    }
}
