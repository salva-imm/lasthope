//! The core: one task that owns all state. It takes `Input`s (what was heard,
//! commands) and publishes `Event`s. Voice mode, text mode and (phase 5) the
//! web UI all drive it through the same `CoreHandle`.
//!
//! A session follows the rite (see rite.rs): she greets and asks about the
//! last oath, we talk, she asks for tomorrow's oath, and on close one LLM call
//! writes the summary and new facts. Every line is saved as it is said.
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
use crate::llm::{self, ChatMessage, LlmClient, Trouble};
use crate::memory::{Memory, Speaker};
use crate::persona::{self, Persona, Snapshot};
use crate::rite::{Action, Rite, Situation};
use crate::speech_out::Mouth;
use crate::timer::{DEFAULT_VIGIL, Vigil};

/// Facts sent with each request (the most recent ones).
const MAX_FACTS_IN_PROMPT: usize = 40;
/// Past lines recalled by full-text search for each reply.
const RECALLED_LINES: u32 = 3;
/// Transcript sent for the end-of-session summary (the tail, in characters).
const SUMMARY_CHARS: usize = 12_000;
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(45);

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
    /// Close the rite: she asks for tomorrow's oath first, a second request closes.
    EndSession,
    /// My verdict on the last oath (the Kept / Broken buttons).
    OathVerdict { kept: bool },
    SetMission { text: String },
    /// Strike a fact out of her memory.
    ForgetFact { id: i64 },
    TimerStart,
    TimerPause,
    TimerReset,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    SessionStarted { session: i64 },
    StateChanged { state: State, muted: bool },
    UserSpoke { text: String },
    SheSpoke { text: String },
    TimerTick { remaining_secs: u64, total_secs: u64, running: bool },
    /// Facts, mission, oath or streak changed: re-read them.
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
    /// Ctrl-C: close the session now (summary included, no oath question).
    Shutdown,
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

struct PacerState {
    busy: bool,
    pending: u64,
}

impl Default for PacerState {
    /// Busy until the core has opened the session (she may speak first).
    fn default() -> Self {
        Self { busy: true, pending: 0 }
    }
}

impl Pacer {
    /// True when a line sent now would be heard straight away.
    pub fn is_free(&self) -> bool {
        let s = self.0.0.lock().unwrap();
        !s.busy && s.pending == 0
    }

    /// Block until she is free (without reserving anything).
    pub fn wait_free(&self) {
        let (lock, cvar) = &*self.0;
        let mut s = lock.lock().unwrap();
        while s.busy || s.pending > 0 {
            s = cvar.wait(s).unwrap();
        }
    }

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
    /// The LLM request is being retried; the reply is still coming.
    Retrying(Trouble),
    Reply(Result<String>),
    Spoke(Result<()>),
    Saved(Result<()>),
}

#[derive(PartialEq, Eq)]
enum Flow {
    Continue,
    Stop,
}

/// Everything heard goes to her, except these spoken commands.
#[derive(Debug, PartialEq, Eq)]
enum Spoken {
    /// "please say …" / "please generate …" / "please store …": speak the rest into output.wav.
    SaveWav(String),
    /// "close the rite", "end the session".
    EndSession,
    Talk,
}

fn spoken_command(text: &str) -> Spoken {
    const SAVE: &[&str] = &["please say", "please generate", "please store"];
    // Whisper writes "rite" as "right" or "write" often enough to accept those too.
    const END: &[&str] = &[
        "close the rite", "close the right", "close the write",
        "end the rite", "end the right", "end the session", "end session",
    ];
    let lower = text.to_lowercase();
    if let Some(p) = SAVE.iter().find(|p| lower.starts_with(*p)) {
        let rest = text
            .get(p.len()..)
            .unwrap_or(&lower[p.len()..])
            .trim_start_matches(|c: char| c.is_whitespace() || matches!(c, ',' | '.' | ':' | '-'))
            .trim();
        return Spoken::SaveWav(rest.to_string());
    }
    // The whole utterance must be the command, give or take a polite word,
    // so "we should end the session soon" is still just conversation.
    let words: String = lower
        .chars()
        .map(|c| if c.is_alphanumeric() || c == ' ' { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut core = words.as_str();
    for filler in ["okay ", "ok ", "alright ", "now ", "lasthope ", "last hope ", "please "] {
        core = core.strip_prefix(filler).unwrap_or(core);
    }
    for filler in [" now", " please", " for tonight", " lasthope", " last hope"] {
        core = core.strip_suffix(filler).unwrap_or(core);
    }
    if END.contains(&core) { Spoken::EndSession } else { Spoken::Talk }
}

pub struct Core {
    llm: LlmClient,
    persona: Persona,
    memory: Memory,
    mouth: Arc<dyn Mouth>,
    gate: Gate,
    pacer: Pacer,
    heard_since_refresh: u64,
    handle: CoreHandle,
    inputs: mpsc::UnboundedReceiver<Input>,
    done_tx: mpsc::UnboundedSender<(u64, Done)>,
    done_rx: mpsc::UnboundedReceiver<(u64, Done)>,
    /// This session's id (0 until opened).
    session: i64,
    rite: Rite,
    vigil: Vigil,
    muted: bool,
    asleep: bool,
    turn: Turn,
    turn_id: u64,
    cancel: Option<Arc<AtomicBool>>,
    task: Option<JoinHandle<()>>,
    end_when_idle: bool,
    /// Close the session once her current reply has been spoken.
    close_after_reply: bool,
    published: Option<(State, bool)>,
    /// What I said last, used to recall related lines from past sessions.
    last_heard: String,
    /// A reply the rite asked for while she was busy (e.g. "close" pressed
    /// while she speaks); started as soon as her turn is over.
    deferred: Option<Situation>,
    /// What the reply in flight is about.
    replying_to: Option<Situation>,
    /// Why the reply in flight is being retried (she says it once per turn).
    trouble: Option<Trouble>,
}

impl Core {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        llm: LlmClient,
        persona: Persona,
        memory: Memory,
        mouth: Arc<dyn Mouth>,
        gate: Gate,
        pacer: Pacer,
        handle: CoreHandle,
        inputs: mpsc::UnboundedReceiver<Input>,
    ) -> Self {
        let (done_tx, done_rx) = mpsc::unbounded_channel();
        Self {
            llm,
            persona,
            memory,
            mouth,
            gate,
            pacer,
            heard_since_refresh: 0,
            handle,
            inputs,
            done_tx,
            done_rx,
            session: 0,
            rite: Rite::begin(None, false).0,
            vigil: Vigil::new(DEFAULT_VIGIL),
            muted: false,
            asleep: false,
            turn: Turn::Idle,
            turn_id: 0,
            cancel: None,
            task: None,
            end_when_idle: false,
            close_after_reply: false,
            published: None,
            last_heard: String::new(),
            deferred: None,
            replying_to: None,
            trouble: None,
        }
    }

    /// Run one session until it is closed.
    pub async fn run(mut self) {
        let mut flow = match self.open().await {
            Ok(flow) => flow,
            Err(e) => {
                self.emit(Event::Error { message: format!("opening the session: {e:#}") });
                Flow::Stop
            }
        };
        self.refresh();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        while flow == Flow::Continue {
            flow = tokio::select! {
                input = self.inputs.recv() => match input {
                    Some(input) => self.on_input(input).await,
                    None => Flow::Stop,
                },
                Some((id, done)) = self.done_rx.recv() => self.on_done(id, done).await,
                _ = tick.tick() => { self.on_tick(); Flow::Continue }
            };
            if flow == Flow::Continue && self.turn == Turn::Idle {
                if let Some(situation) = self.deferred.take() {
                    self.start_reply(situation).await;
                }
            }
            self.refresh();
            if self.end_when_idle && self.turn == Turn::Idle {
                flow = Flow::Stop;
            }
        }
        self.close().await;
    }

    /// Seal any session a crash left open, start this one, and let the rite
    /// decide whether she speaks first.
    async fn open(&mut self) -> Result<Flow> {
        for id in self.memory.unfinished_sessions(None).await? {
            self.notice(format!("sealing an unfinished session (#{id})"));
            let summary = self.summarize(id).await;
            self.memory.finish_session(id, summary.as_deref(), true).await?;
        }
        // a summary that failed last time (network, rate limit) is retried
        for id in self.memory.sessions_missing_summary(3).await? {
            self.notice(format!("writing the missing account of session #{id}"));
            if let Some(summary) = self.summarize(id).await {
                self.memory.finish_session(id, Some(&summary), true).await?;
            }
        }
        self.session = self.memory.start_session().await?;
        self.emit(Event::SessionStarted { session: self.session });
        let today = self.memory.today().await?;
        let pending = self.memory.pending_oath(today, self.session).await?;
        let speaks_first = self.memory.speaks_first().await?;
        let (rite, actions) = Rite::begin(pending, speaks_first);
        self.rite = rite;
        Ok(self.apply(actions).await)
    }

    /// End the session: stop speaking, write the summary and new facts.
    async fn close(&mut self) {
        if let Some(c) = &self.cancel {
            c.store(true, Ordering::Relaxed);
        }
        if let Some(t) = self.task.take() {
            t.abort();
        }
        self.gate.set(false);
        if self.session != 0 {
            self.notice("she records the night");
            let summary = self.summarize(self.session).await;
            if let Err(e) = self.memory.finish_session(self.session, summary.as_deref(), false).await {
                self.emit(Event::Error { message: format!("closing the session: {e:#}") });
            }
        }
        self.pacer.update(u64::MAX, false); // release a waiting stdin reader
        self.emit(Event::SessionEnded);
    }

    /// One LLM call: a short summary of `session` and new facts (stored here).
    /// None if nothing was said or the call failed.
    async fn summarize(&self, session: i64) -> Option<String> {
        let lines = self.memory.session_lines(session).await.ok()?;
        if !lines.iter().any(|l| l.speaker == Speaker::Me) {
            return None;
        }
        // the tail of the transcript, within budget
        let mut start = lines.len();
        let mut chars = 0;
        while start > 0 && chars + lines[start - 1].text.len() <= SUMMARY_CHARS {
            chars += lines[start - 1].text.len();
            start -= 1;
        }
        let known: Vec<String> =
            self.memory.facts().await.unwrap_or_default().into_iter().map(|f| f.text).collect();
        let mission = self.memory.mission().await.unwrap_or_default();
        let request = persona::summary_messages(&lines[start..], &known, &mission);
        let raw = match tokio::time::timeout(
            SUMMARY_TIMEOUT,
            self.llm.chat(&request, persona::SUMMARY_SAMPLING),
        )
        .await
        {
            Ok(Ok(raw)) => raw,
            Ok(Err(e)) => {
                self.emit(Event::Error { message: format!("summary: {e:#}") });
                return None;
            }
            Err(_) => {
                self.emit(Event::Error { message: "summary: timed out".into() });
                return None;
            }
        };
        let Some((summary, facts)) = persona::parse_summary(&raw) else {
            self.emit(Event::Error { message: format!("summary: unreadable reply {raw:?}") });
            return None;
        };
        match self.memory.add_facts(session, &facts).await {
            Ok(0) => {}
            Ok(n) => {
                self.notice(format!("she learned {n} new thing{} about you", if n == 1 { "" } else { "s" }));
                self.emit(Event::MemoryChanged);
            }
            Err(e) => self.emit(Event::Error { message: format!("saving facts: {e:#}") }),
        }
        Some(summary)
    }

    fn emit(&self, event: Event) {
        let _ = self.handle.events.send(event);
    }

    fn notice(&self, text: impl Into<String>) {
        self.emit(Event::Notice { text: text.into() });
    }

    fn error(&self, context: &str, e: anyhow::Error) {
        self.emit(Event::Error { message: format!("{context}: {e:#}") });
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

    async fn on_input(&mut self, input: Input) -> Flow {
        match input {
            Input::Heard(text) => {
                self.heard_since_refresh += 1;
                self.on_heard(text.trim()).await
            }
            Input::Command(cmd) => self.on_command(cmd).await,
            Input::EndOfInput => {
                self.end_when_idle = true;
                Flow::Continue
            }
            Input::Shutdown => Flow::Stop,
        }
    }

    async fn on_heard(&mut self, text: &str) -> Flow {
        if text.is_empty() {
            return Flow::Continue;
        }
        if self.muted || self.state() != State::Listening {
            self.notice(format!("not listening ({:?}{}), ignored: {text}",
                self.state(), if self.muted { ", deafened" } else { "" }));
            return Flow::Continue;
        }
        self.emit(Event::UserSpoke { text: text.to_string() });
        match spoken_command(text) {
            Spoken::EndSession => {
                let actions = self.rite.close_requested();
                self.apply(actions).await
            }
            Spoken::SaveWav(content) if content.is_empty() => {
                self.notice("say the words to save after \"please say\"");
                Flow::Continue
            }
            Spoken::SaveWav(content) => {
                self.start_save(content);
                Flow::Continue
            }
            Spoken::Talk => {
                if let Err(e) = self.memory.add_message(self.session, Speaker::Me, text).await {
                    self.error("saving what you said", e);
                }
                self.last_heard = text.to_string();
                let actions = self.rite.heard(text);
                self.apply(actions).await
            }
        }
    }

    async fn on_command(&mut self, cmd: Command) -> Flow {
        let now = Instant::now();
        match cmd {
            Command::Mute { on } => self.muted = on,
            Command::Sleep { on } => self.asleep = on,
            Command::StopSpeaking => self.stop_turn(),
            Command::EndSession => {
                let actions = self.rite.close_requested();
                return self.apply(actions).await;
            }
            Command::OathVerdict { kept } => {
                let actions = self.rite.verdict_given(kept);
                if actions.is_empty() {
                    self.notice("no oath awaits a verdict");
                }
                return self.apply(actions).await;
            }
            Command::SetMission { text } => match self.memory.set_mission(text.trim()).await {
                Ok(()) => {
                    self.notice("the great work is renamed");
                    self.emit(Event::MemoryChanged);
                }
                Err(e) => self.error("setting the mission", e),
            },
            Command::ForgetFact { id } => match self.memory.forget_fact(id).await {
                Ok(true) => {
                    self.notice(format!("fact {id} struck out"));
                    self.emit(Event::MemoryChanged);
                }
                Ok(false) => self.notice(format!("no fact {id}")),
                Err(e) => self.error("forgetting", e),
            },
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

    /// Carry out what the rite decided.
    async fn apply(&mut self, actions: Vec<Action>) -> Flow {
        for action in actions {
            match action {
                Action::Reply(situation) if self.turn != Turn::Idle => self.deferred = Some(situation),
                Action::Reply(situation) => self.start_reply(situation).await,
                Action::RecordVerdict { oath_session, kept } => {
                    match self.memory.record_verdict(oath_session, kept).await {
                        Ok(streak) => {
                            let today = self.memory.today().await.ok();
                            let chain = today.map(|t| streak.current(t)).unwrap_or(streak.count);
                            self.notice(format!(
                                "oath {} — the chain of seals: {chain}",
                                if kept { "kept" } else { "broken" }
                            ));
                            self.emit(Event::MemoryChanged);
                        }
                        Err(e) => self.error("recording the verdict", e),
                    }
                }
                Action::RecordOath(text) => match self.memory.set_oath(self.session, &text).await {
                    Ok(day) => {
                        self.notice(format!("oath sworn for {} {day}", day.weekday()));
                        self.emit(Event::MemoryChanged);
                    }
                    Err(e) => self.error("recording the oath", e),
                },
                Action::CloseAfterReply => self.close_after_reply = true,
                Action::Close => return Flow::Stop,
            }
        }
        Flow::Continue
    }

    async fn on_done(&mut self, id: u64, done: Done) -> Flow {
        if id != self.turn_id || self.turn == Turn::Idle {
            return Flow::Continue; // a turn that was cancelled
        }
        if let Done::Retrying(trouble) = done {
            // say it once per turn; the request keeps going
            if self.turn == Turn::Thinking && self.trouble.replace(trouble).is_none() {
                self.say_status(trouble.retrying_line());
            }
            return Flow::Continue;
        }
        self.task = None;
        match done {
            Done::Reply(Ok(raw)) => {
                let mut text = persona::clean_reply(&raw);
                if let Some(situation) = self.replying_to.take() {
                    text = persona::ensure_verdict_question(&situation, text);
                }
                if !text.is_empty() {
                    if let Err(e) = self.memory.add_message(self.session, Speaker::Her, &text).await {
                        self.error("saving her words", e);
                    }
                    self.emit(Event::SheSpoke { text: text.clone() });
                    self.start_speaking(text);
                    return Flow::Continue;
                }
                self.emit(Event::Error { message: format!("unspeakable reply: {raw:?}") });
            }
            Done::Reply(Err(e)) => {
                self.emit(Event::Error { message: format!("{e:#}") });
                self.replying_to = None;
                let line = self.trouble.take().map_or(llm::FAILED_LINE, Trouble::gave_up_line);
                // spoken, but not part of the conversation: never saved
                self.notice(line);
                self.start_speaking(line.to_string());
                return Flow::Continue;
            }
            Done::Spoke(Err(e)) => self.emit(Event::Error { message: format!("voice: {e:#}") }),
            Done::Spoke(Ok(())) | Done::Retrying(_) => {}
            Done::Saved(Ok(())) => self.notice(format!("saved speech to {WAV_PATH}")),
            Done::Saved(Err(e)) => self.emit(Event::Error { message: format!("save: {e:#}") }),
        }
        self.turn = Turn::Idle;
        self.cancel = None;
        if self.close_after_reply { Flow::Stop } else { Flow::Continue }
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

    /// Gather what she needs to know from memory for this moment.
    async fn prepare(&mut self, situation: &Situation) -> Result<Vec<ChatMessage>> {
        let m = &self.memory;
        let today = m.today().await?;
        let mut facts: Vec<String> = m.facts().await?.into_iter().map(|f| f.text).collect();
        facts.drain(..facts.len().saturating_sub(MAX_FACTS_IN_PROMPT));
        let recalled = if *situation == Situation::Conversation && !self.last_heard.is_empty() {
            m.search(&self.last_heard, RECALLED_LINES, Some(self.session)).await?
        } else {
            vec![]
        };
        let snapshot = Snapshot {
            today: Some(today),
            hour: m.local_hour().await?,
            mission: m.mission().await?,
            streak: m.streak().await?.current(today),
            last_session: m.last_session(self.session).await?,
            pending_oath: self.rite.pending().cloned(),
            upcoming_oath: m.upcoming_oath(today).await?,
            facts,
            recalled,
        };
        let context = persona::context(&snapshot, situation);
        let lines = m.session_lines(self.session).await?;
        let history = persona::recent(&lines, persona::HISTORY_LINES, persona::HISTORY_CHARS);
        Ok(self.persona.messages(&context, history, persona::stage_direction(situation)))
    }

    /// Ask her to speak, about `situation`.
    async fn start_reply(&mut self, situation: Situation) {
        let id = self.next_turn(Turn::Thinking);
        let messages = match self.prepare(&situation).await {
            Ok(m) => m,
            Err(e) => {
                self.error("reading memory", e);
                self.turn = Turn::Idle;
                return;
            }
        };
        self.replying_to = Some(situation);
        self.trouble = None;
        let sampling = self.persona.sampling();
        let llm = self.llm.clone();
        let tx = self.done_tx.clone();
        self.task = Some(tokio::spawn(async move {
            let on_retry = |t| {
                let _ = tx.send((id, Done::Retrying(t)));
            };
            let reply = llm.chat_reporting(&messages, sampling, on_retry).await;
            let _ = tx.send((id, Done::Reply(reply)));
        }));
    }

    /// A short status line while she is still thinking: shown, and spoken
    /// without changing her turn (the mic stays closed while she thinks).
    fn say_status(&self, line: &str) {
        self.notice(line);
        let _ = self.mouth.speak(line.to_string(), Arc::new(AtomicBool::new(false)));
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spoken_commands() {
        assert_eq!(spoken_command("Please say Hello there."), Spoken::SaveWav("Hello there.".into()));
        assert_eq!(spoken_command("please generate"), Spoken::SaveWav(String::new()));
        assert_eq!(spoken_command("Close the rite."), Spoken::EndSession);
        assert_eq!(spoken_command("Okay, close the right now."), Spoken::EndSession);
        assert_eq!(spoken_command("End the session, Lasthope."), Spoken::EndSession);
        assert_eq!(spoken_command("We should end the session soon."), Spoken::Talk);
        assert_eq!(spoken_command("Talk to me, what is an oath?"), Spoken::Talk);
        assert_eq!(spoken_command("Say something."), Spoken::Talk);
    }

    #[test]
    fn commands_have_stable_json() {
        let c: Command = serde_json::from_str(r#"{"cmd":"mute","on":true}"#).unwrap();
        assert_eq!(c, Command::Mute { on: true });
        let c: Command = serde_json::from_str(r#"{"cmd":"timer_start"}"#).unwrap();
        assert_eq!(c, Command::TimerStart);
        let c: Command = serde_json::from_str(r#"{"cmd":"oath_verdict","kept":false}"#).unwrap();
        assert_eq!(c, Command::OathVerdict { kept: false });
        let c: Command = serde_json::from_str(r#"{"cmd":"forget_fact","id":3}"#).unwrap();
        assert_eq!(c, Command::ForgetFact { id: 3 });
        let e = serde_json::to_string(&Event::StateChanged { state: State::Listening, muted: false }).unwrap();
        assert_eq!(e, r#"{"event":"state_changed","state":"listening","muted":false}"#);
    }
}
