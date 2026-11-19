//! Who she is. The character lives in `persona.md` (re-read on every request,
//! so edits apply immediately, no restart). This module builds the messages
//! for each request and cleans what comes back.
//!
//! The prompt asks for short spoken replies, but the model is never trusted to
//! comply: `clean_reply` strips reasoning blocks and markdown and caps length.
//!
//! Memory reaches her only through `context`: a block the Rust code writes
//! from the database (date, mission, streak, oaths, facts, recalled lines)
//! plus an instruction for the current moment of the rite.

use std::path::{Path, PathBuf};

use crate::llm::{ChatMessage, Role, Sampling};
use crate::memory::{Focus, Hit, Line, Speaker, UpcomingOath};
use crate::rite::{PendingOath, Situation};
use crate::speech_out::split_sentences;
use crate::streak::Day;

/// Used only if persona.md cannot be read.
const FALLBACK_PROMPT: &str = "You are a calm, commanding voice companion. Your replies are \
spoken aloud: one to three short sentences, plain words, no lists, markdown or symbols.";

/// Hard cap on what she says per reply, whatever the model returns.
pub const MAX_SENTENCES: usize = 4;
/// Recent conversation sent with each request.
pub const HISTORY_LINES: usize = 16;
pub const HISTORY_CHARS: usize = 4000;

pub struct Persona {
    path: PathBuf,
    sampling: Sampling,
    warned: bool,
}

impl Persona {
    pub fn new(path: PathBuf, temperature: f32) -> Self {
        Self { path, sampling: Sampling { temperature, seed: None }, warned: false }
    }

    pub fn sampling(&self) -> Sampling {
        self.sampling
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The system prompt, read fresh from disk. HTML comments are notes for
    /// the editor and are not sent.
    pub fn system_prompt(&mut self) -> String {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => {
                self.warned = false;
                strip_comments(&text).trim().to_string()
            }
            Err(e) => {
                if !self.warned {
                    eprintln!("  ! cannot read {}: {e}; using a minimal persona", self.path.display());
                    self.warned = true;
                }
                FALLBACK_PROMPT.to_string()
            }
        }
    }

    /// persona.md + the context block + recent conversation (which ends with
    /// what I just said). A stage direction (not stored) stands in for my turn
    /// when the moment was not something I said: the opening, or a request to
    /// close made by command.
    pub fn messages(&mut self, context: &str, history: &[Line], stage: Option<&str>) -> Vec<ChatMessage> {
        let system = format!("{}\n\n{}", self.system_prompt(), context);
        let mut out = vec![ChatMessage::new(Role::System, system)];
        out.extend(history.iter().map(|l| {
            let role = match l.speaker {
                Speaker::Me => Role::User,
                Speaker::Her => Role::Assistant,
            };
            ChatMessage::new(role, l.text.clone())
        }));
        if let Some(stage) = stage {
            out.push(ChatMessage::new(Role::User, stage));
        } else if !out.iter().any(|m| m.role == Role::User) {
            out.push(ChatMessage::new(Role::User, "(The rite continues.)"));
        }
        out
    }
}

/// The stage direction for moments that are not a reply to my words.
pub fn stage_direction(situation: &Situation) -> Option<&'static str> {
    match situation {
        Situation::Greeting { .. } => Some("(The rite opens. You speak first.)"),
        Situation::AskOath => Some("(They ask to close the rite.)"),
        Situation::VigilComplete { .. } => Some("(The vigil's time is up.)"),
        _ => None,
    }
}

/// The most recent lines that fit in `max_lines` and `max_chars`, always
/// starting with something I said (chat APIs expect user before assistant).
/// The newest line is always included, even if it alone is over budget.
pub fn recent(lines: &[Line], max_lines: usize, max_chars: usize) -> &[Line] {
    let mut start = lines.len();
    let mut chars = 0;
    while start > 0 {
        let line = &lines[start - 1];
        let n = lines.len() - start;
        if n >= max_lines || (n > 0 && chars + line.text.len() > max_chars) {
            break;
        }
        chars += line.text.len();
        start -= 1;
    }
    while start < lines.len().saturating_sub(1) && lines[start].speaker != Speaker::Me {
        start += 1;
    }
    &lines[start..]
}

/// Everything the Rust code knows that is relevant to her next reply.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub today: Option<Day>,
    pub hour: u32,
    pub mission: String,
    /// The chain of seals as it stands today.
    pub streak: u32,
    /// The previous session: its day and summary.
    pub last_session: Option<(Day, Option<String>)>,
    /// The due oath still waiting for my verdict.
    pub pending_oath: Option<PendingOath>,
    /// An oath already sworn for a day still ahead.
    pub upcoming_oath: Option<UpcomingOath>,
    pub facts: Vec<String>,
    /// Lines from past sessions that match what I just said.
    pub recalled: Vec<Hit>,
    /// Focused work recorded today.
    pub focus_today: Focus,
    /// The vigil under way, if any: whole minutes left, and whether it runs.
    pub vigil: Option<(u32, bool)>,
}

/// Numbers as spoken words, so she (and the voice) say them as given.
pub fn number_word(n: u32) -> String {
    const ONES: [&str; 20] = [
        "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
        "eleven", "twelve", "thirteen", "fourteen", "fifteen", "sixteen", "seventeen", "eighteen",
        "nineteen",
    ];
    const TENS: [&str; 8] = ["twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety"];
    match n {
        0..20 => ONES[n as usize].to_string(),
        20..100 if n % 10 == 0 => TENS[n as usize / 10 - 2].to_string(),
        20..100 => format!("{} {}", TENS[n as usize / 10 - 2], ONES[n as usize % 10]),
        100..1000 if n % 100 == 0 => format!("{} hundred", ONES[n as usize / 100]),
        100..1000 => format!("{} hundred {}", ONES[n as usize / 100], number_word(n % 100)),
        _ => n.to_string(),
    }
}

/// "one minute", "twenty five minutes".
pub fn minutes_words(n: u32) -> String {
    format!("{} minute{}", number_word(n), if n == 1 { "" } else { "s" })
}

fn part_of_day(hour: u32) -> &'static str {
    match hour {
        5..=11 => "morning",
        12..=16 => "afternoon",
        17..=21 => "evening",
        _ => "night",
    }
}

fn day_name(day: Day, today: Option<Day>) -> String {
    match today.map(|t| t.0 - day.0) {
        Some(0) => "today".into(),
        Some(1) => "yesterday".into(),
        Some(-1) => "tomorrow".into(),
        _ => format!("{} {day}", day.weekday()),
    }
}

/// The block appended to persona.md: facts from memory, then what to do now.
pub fn context(s: &Snapshot, situation: &Situation) -> String {
    let mut c = String::from(
        "# What you know\n\
         Kept by the Lasthope program from its records. It is true; rely on it, never contradict \
         it, and never invent dates, numbers or past events beyond it.\n\n",
    );
    if let Some(today) = s.today {
        c += &format!("Now: {} {today}, {}.\n", today.weekday(), part_of_day(s.hour));
    }
    c += &format!("The great work: {}\n", s.mission);
    c += &match s.streak {
        0 => "The chain of seals: none unbroken right now (zero days).\n".to_string(),
        1 => "The chain of seals: one day.\n".to_string(),
        n => format!("The chain of seals: {} days unbroken.\n", number_word(n)),
    };
    match &s.last_session {
        None => c += "Past sessions: none. This is your first meeting.\n",
        Some((day, summary)) => {
            c += &format!("Last session: {}.", day_name(*day, s.today));
            if let Some(summary) = summary {
                c += &format!(" What happened: {summary}");
            }
            c += "\n";
        }
    }
    if let Some(o) = &s.pending_oath {
        c += &format!(
            "Their oath for {}, in their words: \"{}\". Not yet judged. (They swore it the day \
             before, so a \"tomorrow\" in it means {}.)\n",
            day_name(o.day, s.today),
            o.text,
            day_name(o.day, s.today)
        );
    }
    if let Some(o) = &s.upcoming_oath {
        c += &format!("Their oath for {}, already sworn: \"{}\".\n", day_name(o.day, s.today), o.text);
    }
    c += &match s.focus_today {
        Focus { minutes: 0, .. } => "Focused work today: none recorded yet.\n".to_string(),
        f => format!(
            "Focused work today: {} in {} vigil{}.\n",
            minutes_words(f.minutes),
            number_word(f.vigils),
            if f.vigils == 1 { "" } else { "s" }
        ),
    };
    match s.vigil {
        Some((left, true)) => c += &format!("A vigil is running now: {} left.\n", minutes_words(left)),
        Some((left, false)) => c += &format!("A vigil is paused with {} left.\n", minutes_words(left)),
        None => {}
    }
    if !s.facts.is_empty() {
        c += "What you know of them:\n";
        for f in &s.facts {
            c += &format!("- {f}\n");
        }
    }
    if !s.recalled.is_empty() {
        c += "From earlier sessions, possibly relevant:\n";
        for h in &s.recalled {
            let who = match h.speaker {
                Speaker::Me => "they said",
                Speaker::Her => "you said",
            };
            let text: String = h.text.chars().take(240).collect();
            c += &format!("- {}, {who}: \"{text}\"\n", day_name(h.day, s.today));
        }
    }
    c += "\n# Right now\n";
    c += &instruction(s, situation);
    c
}

/// What she must do in this reply, written by the program, not the model.
fn instruction(s: &Snapshot, situation: &Situation) -> String {
    match situation {
        Situation::Greeting { ask_verdict } => {
            let mut i = match &s.last_session {
                None => "This is your first meeting and you speak first. In two or three sentences: \
                         introduce yourself, name their great work back to them, and tell them the \
                         chain of seals begins with tomorrow's oath."
                    .to_string(),
                Some((day, _)) => format!(
                    "The rite has just opened and you speak first. Greet them for the {} and ask \
                     what they did since {}.",
                    part_of_day(s.hour),
                    match day_name(*day, s.today).as_str() {
                        "today" => "earlier today".to_string(),
                        other => other.to_string(),
                    }
                ),
            };
            if *ask_verdict {
                if let Some(o) = &s.pending_oath {
                    i += &format!(
                        " Then ask plainly whether they kept or broke their oath for {}: \"{}\". \
                         Refer to it as that day's oath, not as a future one. Tell them to answer \
                         kept or broken.",
                        day_name(o.day, s.today),
                        o.text
                    );
                }
            }
            i + " At most three sentences."
        }
        Situation::Conversation => "Answer what they just said.".into(),
        Situation::VerdictRecorded { kept: true, oath } => format!(
            "They just declared their oath \"{oath}\" KEPT; it is recorded, and the chain of seals \
             now stands at exactly {n}. Acknowledge it in one line and say that number, {n}, \
             exactly as given (never count it yourself), then turn them to today's work.",
            n = number_word(s.streak)
        ),
        Situation::VerdictRecorded { kept: false, oath } => format!(
            "They just declared their oath \"{oath}\" BROKEN; it is recorded and the chain of \
             seals is reset to zero. Name that oath plainly and without cruelty, deny any \
             hopelessness, and give them one small act for today."
        ),
        Situation::VerdictUnclear => "You asked whether they kept their oath and their answer did \
             not say. Ask once more, in one short sentence: kept or broken?"
            .into(),
        Situation::AskOath => "They wish to close the rite. Before it closes, ask for tomorrow's \
             oath: one concrete promise for tomorrow, in their own words. Ask only that, in one or \
             two sentences."
            .into(),
        Situation::OathSworn => {
            let oath = s.upcoming_oath.as_ref().map(|o| o.text.as_str()).unwrap_or("their words");
            format!(
                "They have sworn tomorrow's oath: \"{oath}\". It is recorded. Seal it in one or two \
                 sentences and close the rite. Ask nothing."
            )
        }
        Situation::VigilComplete { minutes } => format!(
            "Their vigil of {} has just ended and is recorded. Today's total, this vigil already \
             included, is exactly {}: say that number as given and never add to it. In one or two \
             sentences, announce the vigil complete and command a short rest before the next \
             one. Do not close the rite. Ask nothing.",
            minutes_words(*minutes),
            minutes_words(s.focus_today.minutes)
        ),
        Situation::VigilLogged { minutes, yesterday, assumed } => {
            let counted = if *assumed {
                format!(
                    "They gave no length, so it is counted as the standard vigil, {} in all; \
                     do not ask how long it was.",
                    minutes_words(*minutes)
                )
            } else {
                format!("It is counted as {}.", minutes_words(*minutes))
            };
            if *yesterday {
                format!(
                    "They report focused work done yesterday, while you were away; it is recorded \
                     for yesterday. {counted} Acknowledge it in one line, then answer the rest of \
                     what they said or turn them to today's work."
                )
            } else {
                format!(
                    "They report focused work done while you were away; it is recorded. {counted} \
                     Today's total, this report already included, is exactly {}: say that number \
                     as given and never add to it. Acknowledge it in one line with that total, \
                     then answer the rest of what they said or turn them to what comes next.",
                    minutes_words(s.focus_today.minutes)
                )
            }
        }
    }
}

/// When she must ask for my verdict, the question is not left to the model:
/// if her reply never says "kept" or "broken", the question is added.
pub fn ensure_verdict_question(situation: &Situation, reply: String) -> String {
    let must_ask = matches!(
        situation,
        Situation::Greeting { ask_verdict: true } | Situation::VerdictUnclear
    );
    let lower = reply.to_lowercase();
    if must_ask && !lower.contains("kept") && !lower.contains("broken") {
        format!("{reply} Your oath: kept or broken?")
    } else {
        reply
    }
}

/// Oaths and plans are recorded separately and expire; they are not facts,
/// whatever the model thinks.
fn is_oath_or_plan(fact: &str) -> bool {
    let f = fact.to_lowercase();
    ["oath", "tomorrow", "plan to", "plans to", "planning to", "pledged", "promised", "swore",
        "sworn", "vowed"]
        .iter()
        .any(|w| f.contains(w))
}

/// The request that ends a session: a short summary and new durable facts.
pub fn summary_messages(lines: &[Line], known_facts: &[String], mission: &str) -> Vec<ChatMessage> {
    let mut system = String::from(
        "You keep the records of a personal voice companion. Below is one session between ME \
         (the person) and HER (the companion). Reply with JSON only, no prose, in this shape:\n\
         {\"summary\": \"...\", \"facts\": [\"...\"]}\n\n\
         summary: two to four plain sentences in the third person (\"They ...\") about what the \
         person did, said, decided or felt, including any oath sworn or judged.\n\
         facts: at most five new, durable facts the person stated about themselves, worth \
         remembering for months (projects, skills, habits, people in their life, circumstances, \
         preferences). Never the companion's advice or suggestions, and not what they did in \
         this session (that belongs in the summary). Each one \
         short sentence starting with \"They\". Nothing about the companion, no passing small \
         talk, nothing uncertain, and nothing already known. Leave out oaths, promises and plans \
         for tomorrow (they are recorded separately) and do not restate their great work, which \
         is already known. Use an empty list if there is nothing new.\n",
    );
    system += &format!("\nTheir great work (already known): {mission}\n");
    if !known_facts.is_empty() {
        system += "\nAlready known (do not repeat):\n";
        for f in known_facts {
            system += &format!("- {f}\n");
        }
    }
    let transcript = lines
        .iter()
        .map(|l| format!("{}: {}", if l.speaker == Speaker::Me { "ME" } else { "HER" }, l.text))
        .collect::<Vec<_>>()
        .join("\n");
    vec![ChatMessage::new(Role::System, system), ChatMessage::new(Role::User, transcript)]
}

pub const SUMMARY_SAMPLING: Sampling = Sampling { temperature: 0.2, seed: None };

/// Pull `{"summary": ..., "facts": [...]}` out of a reply, tolerating code
/// fences or prose around the JSON.
pub fn parse_summary(raw: &str) -> Option<(String, Vec<String>)> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    let v: serde_json::Value = serde_json::from_str(raw.get(start..=end)?).ok()?;
    let summary = v.get("summary")?.as_str()?.trim().to_string();
    let facts = v
        .get("facts")
        .and_then(|f| f.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|f| f.as_str())
                .map(|f| f.trim().to_string())
                .filter(|f| !f.is_empty() && f.len() <= 300 && !is_oath_or_plan(f))
                .take(5)
                .collect()
        })
        .unwrap_or_default();
    (!summary.is_empty()).then_some((summary, facts))
}

fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + 3..],
            None => return out, // unclosed comment: drop the remainder
        }
    }
    out.push_str(rest);
    out
}

/// Make a model reply fit to be spoken: drop `<think>` blocks, markdown and
/// list markup, join lines, and keep at most `MAX_SENTENCES` sentences.
pub fn clean_reply(raw: &str) -> String {
    let mut text = raw.to_string();
    while let Some(start) = text.find("<think>") {
        match text[start..].find("</think>") {
            Some(end) => text.replace_range(start..start + end + "</think>".len(), " "),
            None => text.truncate(start),
        }
    }

    let joined = text
        .lines()
        .map(|line| {
            let l = line.trim().trim_start_matches(['#', '>', '-', '•', '*']).trim_start();
            // "1. foo" / "2) foo" list markers
            let digits = l.chars().take_while(char::is_ascii_digit).count();
            let l = if digits > 0 && l[digits..].starts_with(['.', ')']) {
                l[digits + 1..].trim_start()
            } else {
                l
            };
            let l: String = l.chars().filter(|c| !matches!(c, '*' | '_' | '`' | '#')).collect();
            // a line without closing punctuation still ends a thought when spoken
            if !l.is_empty() && !l.ends_with(['.', '!', '?', ':', ';', ',']) {
                format!("{l}.")
            } else {
                l
            }
        })
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");

    split_sentences(cut_glued_tail(&joined))
        .into_iter()
        .take(MAX_SENTENCES)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Some models occasionally leak internal text glued onto the reply with no
/// space: "…your progress.We have already responded.Closing the rite."
/// Normal prose never has lowercase + [.!?] + uppercase with no space
/// ("Node.js" and "U.S." don't match), so cut the reply there.
fn cut_glued_tail(text: &str) -> &str {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    for w in chars.windows(3) {
        let [(_, a), (i, b), (_, c)] = [w[0], w[1], w[2]];
        if a.is_lowercase() && matches!(b, '.' | '!' | '?') && c.is_uppercase() {
            return &text[..i + b.len_utf8()];
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_think_markdown_and_lists() {
        let raw = "<think>user wants rust</think>**Rise.** The work waits.\n\n\
                   1. **Read** the book\n2) Write code\n- Rest";
        assert_eq!(
            clean_reply(raw),
            "Rise. The work waits. Read the book. Write code."
        );
    }

    #[test]
    fn caps_sentences_and_handles_unclosed_think() {
        assert_eq!(clean_reply("One. Two. Three. Four. Five. Six."), "One. Two. Three. Four.");
        assert_eq!(clean_reply("Speak. <think>never closed"), "Speak.");
        assert_eq!(clean_reply("  "), "");
    }

    #[test]
    fn cuts_leaked_glued_text() {
        assert_eq!(
            clean_reply("Study them in your vigil and seal your progress.We have already responded.Closing the rite."),
            "Study them in your vigil and seal your progress."
        );
        assert_eq!(clean_reply("Learn Node.js later. The U.S. can wait."), "Learn Node.js later. The U.S. can wait.");
    }

    #[test]
    fn comments_are_not_sent() {
        assert_eq!(strip_comments("a<!-- note -->b<!--x\ny-->c"), "abc");
        assert_eq!(strip_comments("keep<!-- open"), "keep");
    }

    #[test]
    fn messages_map_speakers_to_roles() {
        let dir = std::env::temp_dir().join(format!("lasthope-persona-{}", std::process::id()));
        std::fs::write(&dir, "<!-- notes -->\nYou are her.\n").unwrap();
        let mut p = Persona::new(dir.clone(), 0.8);
        let msgs = p.messages("CTX", &[line(Speaker::Me, "hi"), line(Speaker::Her, "Speak."), line(Speaker::Me, "ok")], None);
        let roles: Vec<Role> = msgs.iter().map(|m| m.role).collect();
        assert_eq!(roles, [Role::System, Role::User, Role::Assistant, Role::User]);
        assert_eq!(msgs[0].content, "You are her.\n\nCTX");
        // she speaks first: a stage direction stands in for my turn
        let msgs = p.messages("CTX", &[], stage_direction(&Situation::Greeting { ask_verdict: false }));
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1].content, "(The rite opens. You speak first.)");
        // closing by command: the direction follows the conversation
        let msgs = p.messages("CTX", &[line(Speaker::Me, "hi")], stage_direction(&Situation::AskOath));
        assert_eq!(msgs.last().unwrap().content, "(They ask to close the rite.)");
        std::fs::remove_file(dir).unwrap();
    }

    fn line(speaker: Speaker, text: &str) -> Line {
        Line { speaker, text: text.into(), at: String::new() }
    }

    fn texts(lines: &[Line]) -> Vec<&str> {
        lines.iter().map(|l| l.text.as_str()).collect()
    }

    #[test]
    fn history_keeps_tail_within_budget_starting_with_me() {
        use Speaker::*;
        let l: Vec<Line> = [(Me, "a"), (Her, "b"), (Me, "c"), (Her, "d"), (Me, "e")]
            .into_iter()
            .map(|(s, t)| line(s, t))
            .collect();
        assert_eq!(texts(recent(&l, 10, 1000)), ["a", "b", "c", "d", "e"]);
        assert_eq!(texts(recent(&l, 4, 1000)), ["c", "d", "e"]); // would start with Her "b"
        let l: Vec<Line> = [(Me, "aaaa"), (Her, "bbbb"), (Me, "cccc")].into_iter().map(|(s, t)| line(s, t)).collect();
        assert_eq!(texts(recent(&l, 10, 8)), ["cccc"]);
        assert_eq!(texts(recent(&l, 10, 2)), ["cccc"]); // newest always sent
        assert!(recent(&[], 10, 10).is_empty());
    }

    fn snapshot() -> Snapshot {
        let today = Day::parse("2026-11-19").unwrap();
        Snapshot {
            today: Some(today),
            hour: 21,
            mission: "Master Rust.".into(),
            streak: 3,
            last_session: Some((Day(today.0 - 1), Some("They studied lifetimes.".into()))),
            pending_oath: Some(PendingOath { session: 4, text: "Read chapter 9".into(), day: today }),
            upcoming_oath: None,
            facts: vec!["They train on Mondays.".into()],
            recalled: vec![Hit {
                session: 2,
                day: Day(today.0 - 7),
                speaker: Speaker::Me,
                text: "I hate borrow errors".into(),
            }],
            focus_today: Focus { minutes: 25, vigils: 1 },
            vigil: Some((12, true)),
        }
    }

    #[test]
    fn numbers_and_the_vigil_in_words() {
        assert_eq!(number_word(7), "seven");
        assert_eq!(number_word(25), "twenty five");
        assert_eq!(number_word(40), "forty");
        assert_eq!(number_word(125), "one hundred twenty five");
        assert_eq!(minutes_words(1), "one minute");
        let s = snapshot();
        let c = context(&s, &Situation::VigilComplete { minutes: 25 });
        assert!(c.contains("Focused work today: twenty five minutes in one vigil."), "{c}");
        assert!(c.contains("A vigil is running now: twelve minutes left."));
        assert!(c.contains("vigil of twenty five minutes has just ended"));
        let c = context(&Snapshot { focus_today: Focus::default(), vigil: None, ..s }, &Situation::Conversation);
        assert!(c.contains("Focused work today: none recorded yet."));
        assert!(!c.contains("A vigil is"));
    }

    #[test]
    fn context_carries_memory_and_the_moment() {
        let s = snapshot();
        let c = context(&s, &Situation::Greeting { ask_verdict: true });
        for want in [
            "Now: Thursday 2026-11-19, evening.",
            "The great work: Master Rust.",
            "The chain of seals: three days unbroken.",
            "Last session: yesterday. What happened: They studied lifetimes.",
            "Their oath for today, in their words: \"Read chapter 9\". Not yet judged.",
            "- They train on Mondays.",
            "- Thursday 2026-11-12, they said: \"I hate borrow errors\"",
            "ask what they did since yesterday",
            "kept or broke their oath for today: \"Read chapter 9\"",
        ] {
            assert!(c.contains(want), "missing {want:?} in:\n{c}");
        }
        let c = context(&Snapshot { last_session: None, ..s.clone() }, &Situation::Greeting { ask_verdict: false });
        assert!(c.contains("This is your first meeting"));
        let c = context(&Snapshot { streak: 4, ..s }, &Situation::VerdictRecorded { kept: true, oath: "Read".into() });
        assert!(c.contains("oath \"Read\" KEPT"));
        assert!(c.contains("now stands at exactly four"));
    }

    #[test]
    fn verdict_question_is_guaranteed() {
        let ask = Situation::Greeting { ask_verdict: true };
        assert_eq!(
            ensure_verdict_question(&ask, "Your oath stands.".into()),
            "Your oath stands. Your oath: kept or broken?"
        );
        let fine = "Was it kept, or broken?".to_string();
        assert_eq!(ensure_verdict_question(&ask, fine.clone()), fine);
        let plain = Situation::Greeting { ask_verdict: false };
        assert_eq!(ensure_verdict_question(&plain, "Welcome.".into()), "Welcome.");
    }

    #[test]
    fn summary_reply_parsing() {
        let raw = "```json\n{\"summary\": \"They studied.\", \"facts\": [\"They like tea.\", \"\", 3]}\n```";
        assert_eq!(parse_summary(raw), Some(("They studied.".into(), vec!["They like tea.".into()])));
        assert_eq!(parse_summary("{\"summary\": \"x\"}"), Some(("x".into(), vec![])));
        assert_eq!(parse_summary("no json here"), None);
        let raw = r#"{"summary": "s", "facts": ["They kept the oath.", "They plan to read tomorrow.",
                     "They have a sister, Mara, a nurse who will visit next week.", "They live in Berlin."]}"#;
        assert_eq!(
            parse_summary(raw).unwrap().1,
            vec!["They have a sister, Mara, a nurse who will visit next week.".to_string(), "They live in Berlin.".into()]
        );
        assert_eq!(parse_summary("{\"summary\": \"\"}"), None);
    }
}
