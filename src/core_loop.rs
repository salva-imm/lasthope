//! The core: one task that owns all state. It takes `Input`s (what was heard,
//! commands) and publishes `Event`s. Voice mode, text mode and (phase 5) the
//! web UI all drive it through the same `CoreHandle`.
//!
//! A session follows the rite (see rite.rs): she greets and asks about the
//! last oath, we talk, she asks for tomorrow's oath, and on close one LLM call
//! writes the summary and new facts. Every line is saved as it is said.
//!
//! The grimoire (see grimoire.rs): she teaches a page the Rust code picked,
//! asks two questions on it and judges my answers; old questions I got wrong
//! come back at the start of a later session.
//!
//! A reply is a "turn": Thinking (LLM request) -> Speaking (voice) -> Idle.
//! Turns run as background tasks so commands (stop, mute, end) are handled
//! immediately, even mid-reply. Results carry a turn id; stale ones are dropped.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::audio_in::Gate;
use crate::grimoire::{self, Page, Pick, ReadCmd, reading_command};
use crate::llm::{self, ChatMessage, LlmClient, Sampling, Trouble};
use crate::memory::{Memory, Speaker, VigilSource};
use crate::persona::{self, Persona, Snapshot};
use crate::rite::{Action, Rite, Situation};
use crate::speech_out::Mouth;
use crate::timer::{DEFAULT_VIGIL, MAX_MINUTES, Vigil, VigilCmd, vigil_command, vigil_report};

/// Facts sent with each request (the most recent ones).
const MAX_FACTS_IN_PROMPT: usize = 40;
/// Past lines recalled by full-text search for each reply.
const RECALLED_LINES: u32 = 3;
/// Transcript sent for the end-of-session summary (the tail, in characters).
const SUMMARY_CHARS: usize = 12_000;
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(45);

const WAV_PATH: &str = "output.wav";
/// A report of a vigil this soon after the timer ended one, with no length or
/// count of its own, is about that same vigil and not counted twice.
const SAME_VIGIL_WINDOW: Duration = Duration::from_secs(30 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// No session: the rite is closed (the web UI can open the next one).
    Closed,
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
    /// Open the next session once the rite has been closed (the web UI).
    OpenRite,
    /// Her voice (a Kokoro speaker id) and pace in percent (70-130).
    SetVoice { voice: i32, pace: u32 },
    SetSpeaksFirst { on: bool },
    /// Speak a past session's account aloud.
    ReadAloud { session: i64 },
    /// Delete a past session: its words and what she learned in it.
    BurnSession { session: i64 },
    /// Begin a vigil (of `minutes`, default 25) or resume a paused one.
    TimerStart {
        #[serde(default)]
        minutes: Option<u32>,
    },
    TimerPause,
    TimerReset,
    /// Record focused work done while the program was not running.
    LogVigil {
        minutes: u32,
        #[serde(default)]
        yesterday: bool,
    },
    /// Read with her: a page the Rust code picks (next unread, for the oath, random).
    Read { from: Pick },
    /// Read a page I chose in the viewer (it becomes my place in that book).
    ReadPage { book: i64, page: u32 },
    /// Close the book: no more questions on this page.
    LeavePage,
    /// Make a book the one being read.
    SetBook { book: i64 },
    /// Look through BOOKS_DIR again for new and changed PDFs.
    Reindex,
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
    /// The page open in the grimoire, or its question, changed.
    ReadingChanged { reading: Option<Reading> },
    /// Books, bookmarks or the indexing changed: re-read the shelf.
    ShelfChanged,
    /// The program is ending.
    Goodbye,
}

/// What the core is doing right now, for anyone who joins late (the web UI).
#[derive(Clone, Debug, Serialize)]
pub struct Live {
    pub state: State,
    pub muted: bool,
    /// The open session, if any.
    pub session: Option<i64>,
    pub vigil: VigilStatus,
    /// The page open in the grimoire.
    pub reading: Option<Reading>,
    /// The books folder is being read.
    pub indexing: bool,
}

/// The page open in the grimoire, and the question waiting for my answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Reading {
    pub book: i64,
    pub title: String,
    pub page: u32,
    pub pages: u32,
    pub question: Option<String>,
    /// The question is one I once got wrong.
    pub again: bool,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct VigilStatus {
    pub remaining_secs: u64,
    pub total_secs: u64,
    pub running: bool,
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
    live: Arc<watch::Sender<Live>>,
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

    pub fn live(&self) -> Live {
        self.live.borrow().clone()
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
    let total = DEFAULT_VIGIL.as_secs();
    let live = watch::Sender::new(Live {
        state: State::Closed,
        muted: false,
        session: None,
        vigil: VigilStatus { remaining_secs: total, total_secs: total, running: false },
        reading: None,
        indexing: false,
    });
    (CoreHandle { input, events, live: Arc::new(live) }, rx)
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Flow {
    Continue,
    /// The rite is closed: end this session.
    Close,
    /// End the session and the program (Ctrl-C, end of typed input).
    Quit,
}

/// Everything heard goes to her, except these spoken commands.
#[derive(Debug, PartialEq, Eq)]
enum Spoken {
    /// "please say …" / "please generate …" / "please store …": speak the rest into output.wav.
    SaveWav(String),
    /// "close the rite", "end the session".
    EndSession,
    /// "begin the vigil", "pause the vigil", "reset the vigil".
    Vigil(VigilCmd),
    /// "read the next page", "a random page", "leave the page".
    Read(ReadCmd),
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
    if END.contains(&core) {
        return Spoken::EndSession;
    }
    if let Some(cmd) = vigil_command(text) {
        return Spoken::Vigil(cmd);
    }
    reading_command(text).map_or(Spoken::Talk, Spoken::Read)
}

/// The page open in the grimoire and the questions on it.
struct Quiz {
    /// The page shown (the current question's page).
    shown: Page,
    /// Waiting for my answer.
    current: Option<Question>,
    queue: VecDeque<Question>,
}

#[derive(Clone)]
struct Question {
    /// Its row in `questions` (0: not asked yet).
    id: i64,
    text: String,
    page: Page,
    /// Asked again: the first asking, which I got wrong.
    first_id: Option<i64>,
}

/// A grimoire request in flight.
enum Call {
    Lesson(Page),
    /// Her question, and what I said to it.
    Judge(Question, String),
}

/// Which page to read.
enum How {
    Pick(Pick),
    Page(i64, u32),
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
    deferred: VecDeque<Situation>,
    /// What the reply in flight is about.
    replying_to: Option<Situation>,
    /// Why the reply in flight is being retried (she says it once per turn).
    trouble: Option<Trouble>,
    /// When the timer last finished a vigil.
    last_bell: Option<Instant>,
    books_dir: PathBuf,
    indexing: Arc<AtomicBool>,
    quiz: Option<Quiz>,
    /// A lesson or a judgement in flight, with its turn id.
    call: Option<(u64, Call)>,
    /// Old questions I got wrong wait to be asked again this session.
    review_due: bool,
    /// She has replied to something this session.
    replied: bool,
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
        books_dir: PathBuf,
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
            deferred: VecDeque::new(),
            replying_to: None,
            trouble: None,
            last_bell: None,
            books_dir,
            indexing: Arc::new(AtomicBool::new(false)),
            quiz: None,
            call: None,
            review_due: false,
            replied: false,
        }
    }

    /// Run sessions. Without `stay_up` the program ends with the first
    /// session; with it (the web UI is up) the core then waits, rite closed,
    /// for the next `OpenRite`.
    pub async fn run(mut self, stay_up: bool) {
        self.load_settings().await;
        self.reindex();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            let flow = self.session(&mut tick).await;
            if flow == Flow::Quit || !stay_up || !self.wait_for_rite(&mut tick).await {
                break;
            }
        }
        let now = Instant::now();
        if !self.vigil.elapsed(now).is_zero() && !self.vigil.remaining(now).is_zero() {
            self.notice(format!(
                "the vigil is left unfinished ({} of {} min); unfinished vigils are not counted",
                self.vigil.elapsed(now).as_secs() / 60,
                self.vigil.total().as_secs() / 60
            ));
        }
        self.emit(Event::Goodbye);
    }

    /// One session, from opening to the sealed account.
    async fn session(&mut self, tick: &mut tokio::time::Interval) -> Flow {
        let mut flow = match self.open().await {
            Ok(flow) => flow,
            Err(e) => {
                self.emit(Event::Error { message: format!("opening the session: {e:#}") });
                Flow::Quit
            }
        };
        self.refresh();
        while flow == Flow::Continue {
            flow = tokio::select! {
                input = self.inputs.recv() => match input {
                    Some(input) => self.on_input(input).await,
                    None => Flow::Quit,
                },
                Some((id, done)) = self.done_rx.recv() => self.on_done(id, done).await,
                _ = tick.tick() => { self.on_tick().await; Flow::Continue }
            };
            if flow == Flow::Continue && self.turn == Turn::Idle {
                if let Some(situation) = self.deferred.pop_front() {
                    self.start_reply(situation).await;
                }
            }
            if flow == Flow::Continue && self.review_ready() {
                self.start_review().await;
            }
            self.refresh();
            if self.end_when_idle && self.turn == Turn::Idle {
                flow = Flow::Quit;
            }
        }
        self.close().await;
        self.refresh();
        flow
    }

    /// The rite is closed: the timer still runs and settings still change,
    /// but nothing is heard until `OpenRite`. False: quit instead.
    async fn wait_for_rite(&mut self, tick: &mut tokio::time::Interval) -> bool {
        loop {
            tokio::select! {
                input = self.inputs.recv() => match input {
                    None | Some(Input::Shutdown | Input::EndOfInput) => return false,
                    Some(Input::Command(Command::OpenRite)) => return true,
                    Some(Input::Command(cmd)) => { self.on_command(cmd).await; }
                    Some(Input::Heard(text)) => {
                        self.heard_since_refresh += 1;
                        self.notice(format!("the rite is closed; unheard: {text}"));
                    }
                },
                Some((id, done)) = self.done_rx.recv() => { self.on_done(id, done).await; }
                _ = tick.tick() => self.on_tick().await,
            }
            self.refresh();
        }
    }

    /// Her voice and pace, as last chosen.
    async fn load_settings(&mut self) {
        let voice = self.memory.setting("voice").await.ok().flatten().and_then(|v| v.parse().ok());
        let speed = self.memory.setting("speed").await.ok().flatten().and_then(|v| v.parse().ok());
        self.mouth.set_voice(
            voice.unwrap_or(crate::speech_out::DEFAULT_VOICE),
            speed.unwrap_or(crate::speech_out::DEFAULT_SPEED),
        );
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
        self.close_after_reply = false;
        self.deferred.clear();
        self.replying_to = None;
        self.last_heard.clear();
        self.quiz = None;
        self.call = None;
        self.review_due = true;
        self.replied = false;
        self.publish_reading();
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
        self.turn = Turn::Idle;
        self.cancel = None;
        self.close_after_reply = false;
        self.deferred.clear();
        if self.session != 0 {
            self.notice("she records the night");
            let summary = self.summarize(self.session).await;
            if let Err(e) = self.memory.finish_session(self.session, summary.as_deref(), false).await {
                self.emit(Event::Error { message: format!("closing the session: {e:#}") });
            }
        }
        self.session = 0;
        self.rite = Rite::begin(None, false).0;
        self.quiz = None;
        self.call = None;
        self.review_due = false;
        self.publish_reading();
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
            Turn::Idle if self.session == 0 => State::Closed,
            Turn::Idle if self.asleep => State::Asleep,
            Turn::Idle => State::Listening,
        }
    }

    /// Open the mic only when she is idle, awake and not deafened, and publish
    /// state changes.
    fn refresh(&mut self) {
        self.gate.set(self.turn == Turn::Idle && !self.asleep && !self.muted && self.session != 0);
        self.pacer.update(std::mem::take(&mut self.heard_since_refresh), self.turn != Turn::Idle);
        let now = (self.state(), self.muted);
        if self.published != Some(now) {
            self.published = Some(now);
            self.emit(Event::StateChanged { state: now.0, muted: now.1 });
        }
        let session = (self.session != 0).then_some(self.session);
        self.handle.live.send_if_modified(|live| {
            let changed = live.state != now.0 || live.muted != now.1 || live.session != session;
            (live.state, live.muted, live.session) = (now.0, now.1, session);
            changed
        });
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
            Input::Shutdown => Flow::Quit,
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
            Spoken::Vigil(cmd) => {
                self.vigil_cmd(cmd, true);
                Flow::Continue
            }
            Spoken::Read(ReadCmd::Read(pick)) => {
                self.read(How::Pick(pick), true).await;
                Flow::Continue
            }
            Spoken::Read(ReadCmd::Leave) => {
                self.leave_page(true);
                Flow::Continue
            }
            Spoken::Talk if self.answering() => {
                self.answer(text).await;
                Flow::Continue
            }
            Spoken::Talk => {
                if let Err(e) = self.memory.add_message(self.session, Speaker::Me, text).await {
                    self.error("saving what you said", e);
                }
                self.converse(text).await
            }
        }
    }

    /// What I said (already saved) goes to the rite, and she answers it.
    async fn converse(&mut self, text: &str) -> Flow {
        self.last_heard = text.to_string();
        let logged = self.heard_report(text).await;
        let mut actions = self.rite.heard(text);
        if let Some(situation) = logged {
            // she acknowledges the vigil, unless the rite wants something else
            for a in &mut actions {
                if *a == Action::Reply(Situation::Conversation) {
                    *a = Action::Reply(situation.clone());
                }
            }
        }
        self.apply(actions).await
    }

    async fn on_command(&mut self, cmd: Command) -> Flow {
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
            Command::TimerStart { minutes } => self.vigil_cmd(VigilCmd::Start { minutes }, false),
            Command::TimerPause => self.vigil_cmd(VigilCmd::Pause, false),
            Command::TimerReset => self.vigil_cmd(VigilCmd::Reset, false),
            Command::OpenRite => self.notice("the rite is already open"),
            Command::SetVoice { voice, pace } => self.set_voice(voice, pace).await,
            Command::SetSpeaksFirst { on } => match self.memory.set_speaks_first(on).await {
                Ok(()) => {
                    self.notice(if on { "she will speak first" } else { "she will wait for you to speak" });
                    self.emit(Event::MemoryChanged);
                }
                Err(e) => self.error("saving the setting", e),
            },
            Command::ReadAloud { session } => self.read_aloud(session).await,
            Command::BurnSession { session } => self.burn(session).await,
            Command::LogVigil { minutes, yesterday } => {
                if (1..=MAX_MINUTES).contains(&minutes) {
                    self.log_vigil(minutes, yesterday).await;
                } else {
                    self.notice(format!("a vigil is one to {MAX_MINUTES} minutes"));
                }
            }
            Command::Read { from } => self.read(How::Pick(from), false).await,
            Command::ReadPage { book, page } => self.read(How::Page(book, page), false).await,
            Command::LeavePage => self.leave_page(false),
            Command::SetBook { book } => self.set_book(book).await,
            Command::Reindex => self.reindex(),
        }
        Flow::Continue
    }

    async fn set_voice(&mut self, voice: i32, pace: u32) {
        let Some((_, name)) = crate::speech_out::VOICES.iter().find(|(id, _)| *id == voice) else {
            self.notice(format!("no voice {voice}"));
            return;
        };
        let speed = pace.clamp(70, 130) as f32 / 100.0;
        self.mouth.set_voice(voice, speed);
        let saved = async {
            self.memory.set_setting("voice", &voice.to_string()).await?;
            self.memory.set_setting("speed", &format!("{speed:.2}")).await
        };
        if let Err(e) = saved.await {
            self.error("saving her voice", e);
        }
        self.notice(format!("her voice: {name}, pace {speed:.2}"));
        self.emit(Event::MemoryChanged);
        if self.turn == Turn::Idle {
            self.speak_line("So I will sound.".into());
        }
    }

    async fn read_aloud(&mut self, session: i64) {
        if self.turn != Turn::Idle {
            self.notice("she is not done speaking");
            return;
        }
        match self.memory.session_info(session).await {
            Ok(Some(info)) => match info.summary {
                Some(summary) => self.speak_line(summary),
                None => self.notice("that night has no account"),
            },
            Ok(None) => self.notice(format!("no night {session}")),
            Err(e) => self.error("reading the archive", e),
        }
    }

    async fn burn(&mut self, session: i64) {
        if session == self.session {
            self.notice("the open rite cannot be burned; close it first");
            return;
        }
        match self.memory.delete_session(session).await {
            Ok(true) => {
                self.notice(format!("night {session} is burned"));
                self.emit(Event::MemoryChanged);
            }
            Ok(false) => self.notice(format!("no night {session}")),
            Err(e) => self.error("burning the night", e),
        }
    }

    /// Start, pause or reset the vigil. Spoken commands get a spoken answer;
    /// commands (typed, or the UI) only a notice.
    fn vigil_cmd(&mut self, cmd: VigilCmd, speak: bool) {
        let now = Instant::now();
        let left = |v: &Vigil| {
            let secs = v.remaining(now).as_secs();
            if secs < 60 {
                "less than a minute remains".to_string()
            } else {
                format!("{} remain", persona::minutes_words(secs.div_ceil(60) as u32))
            }
        };
        let line = match cmd {
            VigilCmd::Start { .. } if self.vigil.running() => {
                format!("The vigil is already running; {}.", left(&self.vigil))
            }
            VigilCmd::Start { minutes } => {
                let partway = !self.vigil.elapsed(now).is_zero() && !self.vigil.remaining(now).is_zero();
                if partway && minutes.is_none() {
                    self.vigil.start(now);
                    format!("The vigil resumes; {}.", left(&self.vigil))
                } else {
                    let total = minutes.map_or(DEFAULT_VIGIL, |m| Duration::from_secs(u64::from(m) * 60));
                    self.vigil = Vigil::new(total);
                    self.vigil.start(now);
                    format!("The vigil begins: {}.", persona::minutes_words((total.as_secs() / 60) as u32))
                }
            }
            VigilCmd::Pause if self.vigil.running() => {
                self.vigil.pause(now);
                format!("The vigil is held; {}.", left(&self.vigil))
            }
            VigilCmd::Pause => "No vigil is running.".to_string(),
            VigilCmd::Reset => {
                self.vigil.reset();
                "The vigil is cleared.".to_string()
            }
        };
        self.emit_tick(now);
        self.notice(line.clone());
        if speak {
            self.speak_line(line);
        }
    }

    /// The timer ran out: record the minutes and let her announce it.
    async fn vigil_complete(&mut self, now: Instant) {
        let minutes = (self.vigil.total().as_secs() / 60).max(1) as u32;
        self.last_bell = Some(now);
        match self.record_vigil(minutes, false, VigilSource::Timer).await {
            Ok(total) => self.notice(format!("the vigil is complete: {minutes} min recorded, {total} min today")),
            Err(e) => self.error("recording the vigil", e),
        }
        if self.session == 0 {
            // no rite to speak in: the bell alone
            if self.turn == Turn::Idle {
                self.speak_line("The vigil is complete.".into());
            }
            return;
        }
        if self.close_after_reply || self.rite.closing() {
            return;
        }
        let actions = vec![Action::Reply(Situation::VigilComplete { minutes })];
        self.apply(actions).await;
    }

    /// If what I said reports a vigil kept elsewhere, record it and return
    /// what she should acknowledge.
    async fn heard_report(&mut self, text: &str) -> Option<Situation> {
        if !self.rite.takes_reports() {
            return None; // e.g. tomorrow's oath: "I swear to complete two vigils"
        }
        let report = vigil_report(text)?;
        let just_rang = self.last_bell.is_some_and(|t| t.elapsed() < SAME_VIGIL_WINDOW);
        if just_rang && !report.explicit && !report.yesterday {
            self.notice("that vigil is already recorded");
            return None;
        }
        self.log_vigil(report.minutes, report.yesterday).await;
        Some(Situation::VigilLogged {
            minutes: report.minutes,
            yesterday: report.yesterday,
            assumed: report.assumed,
        })
    }

    async fn log_vigil(&mut self, minutes: u32, yesterday: bool) {
        match self.record_vigil(minutes, yesterday, VigilSource::Told).await {
            Ok(total) => self.notice(format!(
                "{minutes} min of focused work recorded for {} ({total} min that day)",
                if yesterday { "yesterday" } else { "today" }
            )),
            Err(e) => self.error("recording the vigil", e),
        }
    }

    /// Store a vigil; returns the day's total minutes.
    async fn record_vigil(&self, minutes: u32, yesterday: bool, source: VigilSource) -> Result<u32> {
        let today = self.memory.today().await?;
        let day = if yesterday { crate::streak::Day(today.0 - 1) } else { today };
        self.memory.add_vigil(self.session, day, minutes, source).await?;
        self.emit(Event::MemoryChanged);
        Ok(self.memory.focus_on(day).await?.minutes)
    }

    /// Carry out what the rite decided.
    async fn apply(&mut self, actions: Vec<Action>) -> Flow {
        for action in actions {
            match action {
                Action::Reply(situation) if self.turn != Turn::Idle => self.deferred.push_back(situation),
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
                Action::Close => return Flow::Close,
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
        let done = match (self.call.take(), done) {
            (Some((call_id, call)), Done::Reply(reply)) if call_id == id => {
                return self.reading_reply(call, reply).await;
            }
            (_, done) => done,
        };
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
                    self.replied = true;
                    self.start_speaking(text);
                    return Flow::Continue;
                }
                self.emit(Event::Error { message: format!("unspeakable reply: {raw:?}") });
            }
            Done::Reply(Err(e)) => {
                self.emit(Event::Error { message: format!("{e:#}") });
                let trouble = self.trouble.take();
                let line = match self.replying_to.take() {
                    // the bell must ring even without her mind
                    Some(Situation::VigilComplete { .. }) => "The vigil is complete. Rest a moment, then return to the work.",
                    _ => trouble.map_or(llm::FAILED_LINE, Trouble::gave_up_line),
                };
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
        if self.close_after_reply { Flow::Close } else { Flow::Continue }
    }

    async fn on_tick(&mut self) {
        let now = Instant::now();
        if self.vigil.running() {
            let finished = self.vigil.poll_finished(now);
            self.emit_tick(now);
            if finished {
                self.vigil_complete(now).await;
            }
        }
    }

    fn emit_tick(&self, now: Instant) {
        let v = VigilStatus {
            remaining_secs: self.vigil.remaining(now).as_secs_f64().ceil() as u64,
            total_secs: self.vigil.total().as_secs(),
            running: self.vigil.running(),
        };
        self.handle.live.send_replace(Live { vigil: v, ..self.handle.live() });
        self.emit(Event::TimerTick {
            remaining_secs: v.remaining_secs,
            total_secs: v.total_secs,
            running: v.running,
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
            focus_today: m.focus_on(today).await?,
            vigil: {
                let now = Instant::now();
                let left = self.vigil.remaining(now);
                let under_way = !self.vigil.elapsed(now).is_zero() && !left.is_zero();
                under_way.then(|| (left.as_secs().div_ceil(60) as u32, self.vigil.running()))
            },
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
        let sampling = self.persona.sampling();
        self.spawn_chat(id, messages, sampling);
    }

    /// Send `messages` to the model for turn `id`; the reply comes back as
    /// `Done::Reply`, with `Done::Retrying` on the way if the API stumbles.
    fn spawn_chat(&mut self, id: u64, messages: Vec<ChatMessage>, sampling: Sampling) {
        self.trouble = None;
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

    /// Say a fixed line (not from the model, not saved) as a turn of its own.
    fn speak_line(&mut self, text: String) {
        self.next_turn(Turn::Speaking);
        self.start_speaking(text);
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

    // --- the grimoire ------------------------------------------------------

    /// Say a line of hers that belongs to the conversation (saved, shown,
    /// spoken) in the current turn.
    async fn say(&mut self, text: String) {
        if let Err(e) = self.memory.add_message(self.session, Speaker::Her, &text).await {
            self.error("saving her words", e);
        }
        self.emit(Event::SheSpoke { text: text.clone() });
        self.replied = true;
        self.start_speaking(text);
    }

    /// A fixed line about the grimoire: spoken if I asked aloud, else a notice.
    fn tell(&mut self, line: impl Into<String>, speak: bool) {
        let line = line.into();
        self.notice(line.clone());
        if speak && self.turn == Turn::Idle {
            self.speak_line(line);
        }
    }

    fn publish_reading(&self) {
        let reading = self.quiz.as_ref().map(|q| Reading {
            book: q.shown.book,
            title: q.shown.title.clone(),
            page: q.shown.page,
            pages: q.shown.pages,
            question: q.current.as_ref().map(|c| c.text.clone()),
            again: q.current.as_ref().is_some_and(|c| c.first_id.is_some()),
        });
        let changed = self.handle.live.send_if_modified(|live| {
            let changed = live.reading != reading;
            live.reading = reading.clone();
            changed
        });
        if changed {
            self.emit(Event::ReadingChanged { reading });
        }
    }

    /// A question waits for my answer, and the rite is not asking for
    /// something else (a verdict, tomorrow's oath).
    fn answering(&self) -> bool {
        self.rite.is_open() && self.quiz.as_ref().is_some_and(|q| q.current.is_some())
    }

    /// Choose a page (the Rust code, never the model), show it, and have her
    /// teach it.
    async fn read(&mut self, how: How, speak: bool) {
        if self.session == 0 {
            return self.notice("open the rite first; she reads only within it");
        }
        if !self.rite.is_open() {
            return self.notice("the rite waits for an answer about the oath; read after");
        }
        if self.turn != Turn::Idle {
            return self.notice("she is not done speaking");
        }
        let (book, page) = match self.choose_page(&how).await {
            Ok(Ok(found)) => found,
            Ok(Err(why)) => return self.tell(why, speak),
            Err(e) => return self.error("choosing a page", e),
        };
        let page = match self.memory.page(book, page).await {
            Ok(Some(p)) if (p.text.len() as i64) < grimoire::MIN_PAGE_CHARS => {
                return self.tell("That page holds too little text to teach.", speak);
            }
            Ok(Some(p)) => p,
            Ok(None) => return self.tell(format!("There is no page {page} in that book."), speak),
            Err(e) => return self.error("opening the page", e),
        };
        // the bookmark follows the pages I read in order, or chose myself
        if matches!(how, How::Pick(Pick::Next) | How::Page(..)) {
            let marked = async {
                self.memory.set_last_page(book, page.page).await?;
                self.memory.set_current_book(book).await
            };
            if let Err(e) = marked.await {
                self.error("keeping your place", e);
            }
            self.emit(Event::ShelfChanged);
        }
        self.quiz = Some(Quiz { shown: page.clone(), current: None, queue: VecDeque::new() });
        self.publish_reading();
        let messages = grimoire::lesson_messages(&self.persona.system_prompt(), &page);
        let id = self.next_turn(Turn::Thinking);
        self.replying_to = None;
        self.call = Some((id, Call::Lesson(page)));
        self.spawn_chat(id, messages, grimoire::LESSON_SAMPLING);
    }

    /// (book, page) to read, or what to say when there is none.
    async fn choose_page(&self, how: &How) -> Result<Result<(i64, u32), String>> {
        let m = &self.memory;
        Ok(match how {
            How::Page(book, page) => match m.book(*book).await? {
                Some(b) if b.readable() => Ok((*book, *page)),
                Some(b) => Err(format!("{} has no text she can read.", b.title)),
                None => Err("There is no such book on the shelf.".into()),
            },
            How::Pick(Pick::Next) => match m.current_book().await? {
                None => Err("No book on the shelf has text to read.".into()),
                Some(b) => match m.next_page(b.id).await? {
                    Some(p) => Ok((b.id, p)),
                    None => Err(format!("{} is read to its end. Choose another book, or a random page.", b.title)),
                },
            },
            How::Pick(Pick::Oath) => {
                let today = m.today().await?;
                let wanted = match m.oath_in_force(today).await? {
                    Some(oath) => oath,
                    None => {
                        self.notice("no oath is sworn; she reads for the great work instead");
                        m.mission().await?
                    }
                };
                match m.best_page_for(&wanted).await? {
                    Some(found) => Ok(found),
                    None => Err("No page on the shelf answers your oath.".into()),
                }
            }
            How::Pick(Pick::Random) => {
                let book = m.current_book().await?.map(|b| b.id);
                match m.random_page(book).await? {
                    Some(found) => Ok(found),
                    None => Err("No book on the shelf has text to read.".into()),
                }
            }
        })
    }

    /// My answer to the open question: stored, then judged against the page.
    async fn answer(&mut self, text: &str) {
        if let Err(e) = self.memory.add_message(self.session, Speaker::Me, text).await {
            self.error("saving what you said", e);
        }
        let Some(quiz) = self.quiz.as_mut() else { return };
        let Some(question) = quiz.current.take() else { return };
        let last = quiz.queue.is_empty();
        if let Err(e) = self.memory.answer_question(question.id, text).await {
            self.error("saving your answer", e);
        }
        self.publish_reading();
        let messages = grimoire::judge_messages(
            &self.persona.system_prompt(),
            &question.page,
            &question.text,
            text,
            last,
        );
        let id = self.next_turn(Turn::Thinking);
        self.replying_to = None;
        self.call = Some((id, Call::Judge(question, text.to_string())));
        self.spawn_chat(id, messages, grimoire::JUDGE_SAMPLING);
    }

    /// The model's lesson or judgement came back.
    async fn reading_reply(&mut self, call: Call, reply: Result<String>) -> Flow {
        let raw = match reply {
            Ok(raw) => Some(raw),
            Err(e) => {
                self.error("the grimoire", e);
                None
            }
        };
        match call {
            Call::Lesson(page) => {
                let Some((explanation, questions)) = raw.as_deref().and_then(grimoire::parse_lesson) else {
                    if let Some(raw) = &raw {
                        self.emit(Event::Error { message: format!("unreadable lesson: {raw:?}") });
                    }
                    self.failed_reading("The page would not open to me. Ask for it again.");
                    return Flow::Continue;
                };
                let mut queue = VecDeque::new();
                for text in questions {
                    match self.memory.ask_question(page.book, page.page, self.session, &text, None).await {
                        Ok(id) => queue.push_back(Question { id, text, page: page.clone(), first_id: None }),
                        Err(e) => self.error("saving her question", e),
                    }
                }
                let Some(first) = queue.pop_front() else {
                    self.failed_reading("The page would not open to me. Ask for it again.");
                    return Flow::Continue;
                };
                let line = format!("{} First question: {}", persona::clean_reply(&explanation), first.text);
                if let Some(quiz) = self.quiz.as_mut() {
                    quiz.current = Some(first);
                    quiz.queue = queue;
                }
                self.publish_reading();
                self.say(line).await;
            }
            Call::Judge(question, said) => {
                let judged = raw.as_deref().and_then(grimoire::parse_judgement);
                if judged == Some(grimoire::Judged::Aside) {
                    // not an answer: she talks of it as ever, and the question waits
                    if let Err(e) = self.memory.unanswer_question(question.id).await {
                        self.error("reopening the question", e);
                    }
                    if let Some(quiz) = self.quiz.as_mut() {
                        quiz.current = Some(question);
                    }
                    self.publish_reading();
                    self.notice("not an answer to her question; it stays open");
                    self.turn = Turn::Idle; // the judging turn is over: her reply is a new one
                    return self.converse(&said).await;
                }
                let Some(grimoire::Judged::Answer { correct, reply: verdict }) = judged else {
                    if let Some(raw) = &raw {
                        self.emit(Event::Error { message: format!("unreadable judgement: {raw:?}") });
                    }
                    // the question stays open: answering again judges again
                    if let Some(quiz) = self.quiz.as_mut() {
                        quiz.current = Some(question);
                    }
                    self.publish_reading();
                    self.failed_reading("I could not weigh that answer. Give it once more.");
                    return Flow::Continue;
                };
                if let Err(e) = self.memory.judge_question(question.id, correct, &verdict).await {
                    self.error("saving the verdict", e);
                }
                self.notice(format!("answer judged {}", if correct { "right" } else { "wrong" }));
                let mut line = persona::clean_reply(&verdict);
                let next = self.quiz.as_mut().and_then(|q| q.queue.pop_front());
                if let Some(mut next) = next {
                    if next.id == 0 {
                        match self.memory.ask_question(next.page.book, next.page.page, self.session, &next.text, next.first_id).await {
                            Ok(id) => next.id = id,
                            Err(e) => self.error("saving the question", e),
                        }
                    }
                    line += &match next.first_id {
                        Some(_) => format!(" {}", missed_intro(&next, "Another you once missed")),
                        None => format!(" Second question: {}", next.text),
                    };
                    if let Some(quiz) = self.quiz.as_mut() {
                        quiz.shown = next.page.clone();
                        quiz.current = Some(next);
                    }
                }
                self.publish_reading();
                self.say(line).await;
            }
        }
        Flow::Continue
    }

    /// A grimoire request failed: say so (not saved), and let her listen.
    fn failed_reading(&mut self, line: &str) {
        let line = match self.trouble.take() {
            Some(t) => t.gave_up_line(),
            None => line,
        };
        self.notice(line);
        self.start_speaking(line.to_string());
    }

    fn leave_page(&mut self, speak: bool) {
        if self.quiz.take().is_none() {
            return self.tell("No book is open.", speak);
        }
        self.publish_reading();
        self.tell("The book is closed.", speak);
    }

    async fn set_book(&mut self, id: i64) {
        match self.memory.book(id).await {
            Ok(Some(b)) if b.readable() => match self.memory.set_current_book(id).await {
                Ok(()) => {
                    self.notice(format!("now reading {}", b.title));
                    self.emit(Event::ShelfChanged);
                }
                Err(e) => self.error("choosing the book", e),
            },
            Ok(Some(b)) => self.notice(format!("{} has no text she can read", b.title)),
            Ok(None) => self.notice(format!("no book {id}")),
            Err(e) => self.error("choosing the book", e),
        }
    }

    /// Time to ask again the questions I once got wrong: once per session,
    /// after we have spoken, when nothing else is going on.
    fn review_ready(&self) -> bool {
        self.review_due
            && self.turn == Turn::Idle
            && self.deferred.is_empty()
            && self.replied
            && !self.last_heard.is_empty()
            && self.rite.is_open()
            && !self.close_after_reply
            && self.quiz.as_ref().is_none_or(|q| q.current.is_none())
    }

    async fn start_review(&mut self) {
        self.review_due = false;
        let missed = match self.memory.missed_questions(grimoire::REASK).await {
            Ok(m) => m,
            Err(e) => return self.error("reading old questions", e),
        };
        let mut queue = VecDeque::new();
        for m in missed {
            if let Ok(Some(page)) = self.memory.page(m.book, m.page).await {
                queue.push_back(Question { id: 0, text: m.question, page, first_id: Some(m.first_id) });
            }
        }
        let Some(mut first) = queue.pop_front() else { return };
        match self.memory.ask_question(first.page.book, first.page.page, self.session, &first.text, first.first_id).await {
            Ok(id) => first.id = id,
            Err(e) => return self.error("saving the question", e),
        }
        let line = format!("Before the work: {}", missed_intro(&first, "a question you once got wrong"));
        self.quiz = Some(Quiz { shown: first.page.clone(), current: Some(first), queue });
        self.publish_reading();
        self.next_turn(Turn::Speaking);
        self.say(line).await;
    }

    /// Read `BOOKS_DIR` again in the background: new and changed PDFs only.
    fn reindex(&self) {
        if !self.books_dir.is_dir() {
            return self.notice(format!(
                "the grimoire: no books folder at {} (set BOOKS_DIR to a folder of PDFs)",
                self.books_dir.display()
            ));
        }
        if self.indexing.swap(true, Ordering::SeqCst) {
            return self.notice("the shelf is already being read");
        }
        let (memory, dir, handle, flag) =
            (self.memory.clone(), self.books_dir.clone(), self.handle.clone(), self.indexing.clone());
        handle.live.send_modify(|l| l.indexing = true);
        self.emit(Event::ShelfChanged);
        tokio::spawn(async move {
            let say = |text: String| {
                let _ = handle.events.send(Event::Notice { text });
            };
            match grimoire::index(&memory, &dir, |t| say(format!("the grimoire: {t}"))).await {
                Ok(report) => say(format!("the grimoire: {}", report.describe())),
                Err(e) => {
                    let _ = handle.events.send(Event::Error { message: format!("the grimoire: {e:#}") });
                }
            }
            flag.store(false, Ordering::SeqCst);
            handle.live.send_modify(|l| l.indexing = false);
            let _ = handle.events.send(Event::ShelfChanged);
        });
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

/// "<lead>, from <book>, page <n>: <question>"
fn missed_intro(q: &Question, lead: &str) -> String {
    format!("{lead}, from {}, page {}: {}", q.page.title, persona::number_word(q.page.page), q.text)
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
        assert_eq!(spoken_command("Begin the vigil."), Spoken::Vigil(VigilCmd::Start { minutes: None }));
        assert_eq!(spoken_command("Pause the vigil."), Spoken::Vigil(VigilCmd::Pause));
        assert_eq!(spoken_command("I did a vigil."), Spoken::Talk);
        assert_eq!(spoken_command("We should end the session soon."), Spoken::Talk);
        assert_eq!(spoken_command("Talk to me, what is an oath?"), Spoken::Talk);
        assert_eq!(spoken_command("Say something."), Spoken::Talk);
    }

    #[test]
    fn commands_have_stable_json() {
        let c: Command = serde_json::from_str(r#"{"cmd":"mute","on":true}"#).unwrap();
        assert_eq!(c, Command::Mute { on: true });
        let c: Command = serde_json::from_str(r#"{"cmd":"timer_start"}"#).unwrap();
        assert_eq!(c, Command::TimerStart { minutes: None });
        let c: Command = serde_json::from_str(r#"{"cmd":"timer_start","minutes":50}"#).unwrap();
        assert_eq!(c, Command::TimerStart { minutes: Some(50) });
        let c: Command = serde_json::from_str(r#"{"cmd":"log_vigil","minutes":40}"#).unwrap();
        assert_eq!(c, Command::LogVigil { minutes: 40, yesterday: false });
        let c: Command = serde_json::from_str(r#"{"cmd":"oath_verdict","kept":false}"#).unwrap();
        assert_eq!(c, Command::OathVerdict { kept: false });
        let c: Command = serde_json::from_str(r#"{"cmd":"forget_fact","id":3}"#).unwrap();
        assert_eq!(c, Command::ForgetFact { id: 3 });
        let e = serde_json::to_string(&Event::StateChanged { state: State::Listening, muted: false }).unwrap();
        assert_eq!(e, r#"{"event":"state_changed","state":"listening","muted":false}"#);
    }
}
