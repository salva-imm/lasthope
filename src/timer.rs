//! The vigil: a focus timer owned by the core. Pure state over `Instant`s so
//! it can be tested without waiting; the core drives it with a 1 s tick.
//!
//! Also the words for it: spoken commands ("begin the vigil") and reports of
//! focused work done while the program was not running ("I completed a vigil
//! yesterday"). Both are read from my own words here, never by the model.

use std::time::{Duration, Instant};

pub const DEFAULT_VIGIL: Duration = Duration::from_secs(25 * 60);

#[derive(Clone, Debug)]
pub struct Vigil {
    total: Duration,
    /// Time accumulated in earlier running stretches (before the last pause).
    banked: Duration,
    /// Set while running.
    since: Option<Instant>,
}

impl Vigil {
    pub fn new(total: Duration) -> Self {
        Self { total, banked: Duration::ZERO, since: None }
    }

    pub fn total(&self) -> Duration {
        self.total
    }

    pub fn running(&self) -> bool {
        self.since.is_some()
    }

    pub fn elapsed(&self, now: Instant) -> Duration {
        let live = self.since.map(|s| now.saturating_duration_since(s)).unwrap_or_default();
        (self.banked + live).min(self.total)
    }

    pub fn remaining(&self, now: Instant) -> Duration {
        self.total - self.elapsed(now)
    }

    /// Start or resume. Starting a finished vigil begins a fresh one.
    pub fn start(&mut self, now: Instant) {
        if self.remaining(now).is_zero() {
            self.banked = Duration::ZERO;
        }
        if self.since.is_none() {
            self.since = Some(now);
        }
    }

    pub fn pause(&mut self, now: Instant) {
        if self.since.is_some() {
            self.banked = self.elapsed(now);
            self.since = None;
        }
    }

    pub fn reset(&mut self) {
        self.banked = Duration::ZERO;
        self.since = None;
    }

    /// Call on every tick. Returns true exactly once, when a running vigil
    /// reaches zero; the vigil then stops.
    pub fn poll_finished(&mut self, now: Instant) -> bool {
        if self.running() && self.remaining(now).is_zero() {
            self.banked = self.total;
            self.since = None;
            true
        } else {
            false
        }
    }
}

// --- the words for it ---------------------------------------------------

/// A spoken vigil command. The whole utterance must be the command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VigilCmd {
    /// Begin (with an optional length) or resume.
    Start { minutes: Option<u32> },
    Pause,
    Reset,
}

/// Whisper hears "vigil" as "virgil" now and then.
const VIGIL_NOUNS: &[&str] = &["vigil", "vigils", "virgil", "virgils"];
/// Longest vigil or report accepted, in minutes.
pub const MAX_MINUTES: u32 = 600;

fn tokens(text: &str) -> Vec<String> {
    text.to_lowercase()
        .replace('’', "'")
        .split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// A number at `i` (digits, or words up to ninety-nine): (value, tokens used).
fn number_at(t: &[String], i: usize) -> Option<(u32, usize)> {
    const ONES: [&str; 20] = [
        "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
        "eleven", "twelve", "thirteen", "fourteen", "fifteen", "sixteen", "seventeen", "eighteen",
        "nineteen",
    ];
    const TENS: [&str; 8] = ["twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety"];
    let w = t.get(i)?.as_str();
    if let Ok(n) = w.parse::<u32>() {
        return Some((n, 1));
    }
    if let Some(n) = ONES.iter().position(|x| *x == w) {
        return Some((n as u32, 1));
    }
    let tens = TENS.iter().position(|x| *x == w)? as u32 * 10 + 20;
    match t.get(i + 1).and_then(|n| ONES[1..10].iter().position(|x| x == n)) {
        Some(one) => Some((tens + one as u32 + 1, 2)),
        None => Some((tens, 1)),
    }
}

/// A duration starting at `i`: "25 minutes", "an hour", "two hours",
/// "half an hour", "an hour and a half". (minutes, tokens used).
fn duration_at(t: &[String], i: usize) -> Option<(u32, usize)> {
    let is = |k: usize, words: &[&str]| t.get(k).is_some_and(|w| words.contains(&w.as_str()));
    if is(i, &["half"]) && is(i + 1, &["an", "a"]) && is(i + 2, &["hour"]) {
        return Some((30, 3));
    }
    let (n, len) = if is(i, &["an", "a"]) && is(i + 1, &["hour"]) { (1, 1) } else { number_at(t, i)? };
    let unit = i + len;
    if is(unit, &["minute", "minutes", "min", "mins"]) {
        return Some((n, len + 1));
    }
    if is(unit, &["hour", "hours"]) {
        if is(unit + 1, &["and"]) && is(unit + 2, &["a"]) && is(unit + 3, &["half"]) {
            return Some((n * 60 + 30, len + 4));
        }
        return Some((n * 60, len + 1));
    }
    None
}

/// "begin the vigil", "start a fifty minute vigil", "pause the timer",
/// "reset the vigil", give or take a polite word.
pub fn vigil_command(text: &str) -> Option<VigilCmd> {
    const LEAD: &[&str] = &[
        "okay", "ok", "alright", "now", "lasthope", "last", "hope", "please", "let's", "lets", "so", "then",
        "i", "i'll", "i'm", "we", "we'll", "will", "am", "going", "gonna", "to", "want", "wanna",
    ];
    const TRAIL: &[&str] = &["now", "please", "lasthope", "last", "hope"];
    const START: &[&str] = &["begin", "start", "resume", "continue"];
    const PAUSE: &[&str] = &["pause", "hold", "stop", "halt"];
    const RESET: &[&str] = &["reset", "cancel", "clear", "abandon"];
    const FILL: &[&str] = &["the", "a", "an", "my", "our", "new", "of", "for", "another", "timer"];

    if text.trim_end().ends_with('?') {
        return None;
    }
    let t = tokens(text);
    let mut a = 0;
    let mut b = t.len();
    while a < b && LEAD.contains(&t[a].as_str()) {
        a += 1;
    }
    while b > a && TRAIL.contains(&t[b - 1].as_str()) {
        b -= 1;
    }
    let t = &t[a..b];
    let verb = t.first()?.as_str();
    let rest = &t[1..];
    // one vigil noun (or "timer"), and nothing but fill words and a length
    if !rest.iter().any(|w| VIGIL_NOUNS.contains(&w.as_str()) || w == "timer") {
        return None;
    }
    let mut minutes = None;
    let mut i = 0;
    while i < rest.len() {
        if let Some((m, len)) = duration_at(rest, i) {
            minutes = Some(m);
            i += len;
        } else if VIGIL_NOUNS.contains(&rest[i].as_str()) || FILL.contains(&rest[i].as_str()) {
            i += 1;
        } else {
            return None;
        }
    }
    if START.contains(&verb) {
        Some(VigilCmd::Start { minutes: minutes.filter(|m| (1..=MAX_MINUTES).contains(m)) })
    } else if minutes.is_some() {
        None
    } else if PAUSE.contains(&verb) {
        Some(VigilCmd::Pause)
    } else if RESET.contains(&verb) {
        Some(VigilCmd::Reset)
    } else {
        None
    }
}

/// Focused work I report having done (the program was not running, or I
/// kept the vigil elsewhere).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Report {
    pub minutes: u32,
    /// A length, a count or "another" was said, so it cannot be the vigil
    /// the timer just finished.
    pub explicit: bool,
    /// No length was said: each vigil counted as the standard 25 minutes.
    pub assumed: bool,
    pub yesterday: bool,
}

/// "I completed a vigil", "did two vigils yesterday", "I finished a forty
/// minute vigil", "I did an hour of focused work". Only plain past-tense
/// statements count: questions, plans and negations are not reports.
pub fn vigil_report(text: &str) -> Option<Report> {
    const DONE: &[&str] = &["completed", "finished", "did", "done", "kept", "held"];
    const NOT: &[&str] = &[
        "not", "no", "didn't", "didnt", "never", "couldn't", "couldnt", "wasn't", "wasnt",
        "haven't", "havent", "hadn't", "hadnt", "don't", "dont", "won't", "wont", "failed",
        "missed", "skipped",
    ];
    const LATER: &[&str] = &[
        "will", "i'll", "going", "gonna", "tomorrow", "plan", "planning", "want", "wanna",
        "should", "need", "must", "shall", "let's", "lets", "later", "next",
    ];
    const ASKING: &[&str] = &[
        "have", "has", "should", "can", "could", "will", "do", "does", "how", "what", "when",
        "why", "is", "are", "shall", "would",
    ];
    const FOCUS: &[&str] = &["focus", "focused", "deep"];

    let t = tokens(text);
    let has = |words: &[&str]| t.iter().any(|w| words.contains(&w.as_str()));
    if text.trim_end().ends_with('?') || t.first().is_some_and(|w| ASKING.contains(&w.as_str())) {
        return None;
    }
    if !has(DONE) || has(NOT) || has(LATER) {
        return None;
    }

    // durations and other numbers, by position
    let mut durations = vec![]; // (start, end, minutes)
    let mut numbers = vec![]; // (end, value)
    let mut i = 0;
    while i < t.len() {
        if let Some((m, len)) = duration_at(&t, i) {
            durations.push((i, i + len, m));
            i += len;
        } else if let Some((n, len)) = number_at(&t, i) {
            numbers.push((i + len, n));
            i += len;
        } else {
            i += 1;
        }
    }
    let vigil_at: Vec<usize> = (0..t.len()).filter(|&k| VIGIL_NOUNS.contains(&t[k].as_str())).collect();
    if vigil_at.is_empty() && !(has(FOCUS) && !durations.is_empty()) {
        return None;
    }
    // "two vigils", "two 25 minute vigils": a number just before the noun
    // (or before a length just before it)
    let count = vigil_at.iter().find_map(|&v| {
        let end = durations.iter().find(|d| d.1 == v).map_or(v, |d| d.0);
        numbers.iter().find(|n| n.0 == end).map(|n| n.1)
    });
    let minutes = match (durations.as_slice(), count) {
        ([], c) => c.unwrap_or(1) * (DEFAULT_VIGIL.as_secs() / 60) as u32,
        ([one], Some(c)) => one.2 * c,
        (all, _) => all.iter().map(|d| d.2).sum(),
    };
    (1..=MAX_MINUTES).contains(&minutes).then(|| Report {
        minutes,
        explicit: !durations.is_empty() || count.is_some() || has(&["another"]),
        assumed: durations.is_empty(),
        yesterday: has(&["yesterday"]) || text.to_lowercase().contains("last night"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: Duration = Duration::from_secs(60);

    #[test]
    fn runs_pauses_resumes() {
        let t0 = Instant::now();
        let mut v = Vigil::new(25 * M);
        assert_eq!(v.remaining(t0), 25 * M);
        v.start(t0);
        assert_eq!(v.remaining(t0 + 10 * M), 15 * M);
        v.pause(t0 + 10 * M);
        // paused: time passing does not count
        assert_eq!(v.remaining(t0 + 20 * M), 15 * M);
        v.start(t0 + 20 * M);
        assert_eq!(v.remaining(t0 + 25 * M), 10 * M);
    }

    #[test]
    fn finishes_once_and_stops() {
        let t0 = Instant::now();
        let mut v = Vigil::new(M);
        v.start(t0);
        assert!(!v.poll_finished(t0 + M / 2));
        assert!(v.poll_finished(t0 + M + Duration::from_secs(3)));
        assert!(!v.running());
        assert!(!v.poll_finished(t0 + 2 * M));
        assert_eq!(v.remaining(t0 + 5 * M), Duration::ZERO);
        // starting again begins a fresh vigil
        v.start(t0 + 5 * M);
        assert_eq!(v.remaining(t0 + 5 * M), M);
    }

    #[test]
    fn reset_clears() {
        let t0 = Instant::now();
        let mut v = Vigil::new(M);
        v.start(t0);
        v.reset();
        assert!(!v.running());
        assert_eq!(v.remaining(t0 + M), M);
    }

    #[test]
    fn spoken_vigil_commands() {
        use VigilCmd::*;
        for (said, want) in [
            ("Begin the vigil.", Some(Start { minutes: None })),
            ("Okay, start the vigil now.", Some(Start { minutes: None })),
            ("Let's begin a fifty minute vigil.", Some(Start { minutes: Some(50) })),
            ("Start a vigil of 40 minutes.", Some(Start { minutes: Some(40) })),
            ("Begin an hour vigil", Some(Start { minutes: Some(60) })),
            ("Resume the virgil.", Some(Start { minutes: None })),
            ("Start the timer.", Some(Start { minutes: None })),
            ("Pause the vigil.", Some(Pause)),
            ("Hold the vigil, please.", Some(Pause)),
            ("Reset the vigil.", Some(Reset)),
            ("Begin.", None),
            ("Should I start the vigil?", None),
            ("I want to start the vigil after dinner.", None),
            ("Start the vigil with the EVM chapter.", None),
            ("Pause the 20 minute vigil", None),
            ("I start a 10 minute vigil", Some(Start { minutes: Some(10) })),
            ("I'm going to start a vigil.", Some(Start { minutes: None })),
            ("We'll begin a thirty minute vigil.", Some(Start { minutes: Some(30) })),
            ("I want to start a vigil", Some(Start { minutes: None })),
            ("Start a vigil?", None),
        ] {
            assert_eq!(vigil_command(said), want, "{said:?}");
        }
    }

    #[test]
    fn reports_of_vigils_kept_elsewhere() {
        // (minutes, explicit, assumed, yesterday)
        let r = |minutes, explicit, assumed, yesterday| Some(Report { minutes, explicit, assumed, yesterday });
        for (said, want) in [
            ("I completed a vigil.", r(25, false, true, false)),
            ("I completed it. Did my vigil while you were off.", r(25, false, true, false)),
            ("I did two vigils yesterday.", r(50, true, true, true)),
            ("Finished a forty-minute vigil.", r(40, true, false, false)),
            ("I did two 30 minute vigils.", r(60, true, false, false)),
            ("I did an hour of focused work last night.", r(60, true, false, true)),
            ("Kept another vigil.", r(25, true, true, false)),
            ("I did an hour and a half of deep work.", r(90, true, false, false)),
            ("Did you finish the vigil?", None),
            ("Have I done a vigil today", None),
            ("I didn't finish the vigil.", None),
            ("I will do a vigil tomorrow.", None),
            ("I did my homework.", None),
            ("I did forty minutes of reading.", None),
            ("I swear to complete two vigils.", None),
        ] {
            assert_eq!(vigil_report(said), want, "{said:?}");
        }
    }

    #[test]
    fn double_start_does_not_restart() {
        let t0 = Instant::now();
        let mut v = Vigil::new(10 * M);
        v.start(t0);
        v.start(t0 + 4 * M);
        assert_eq!(v.remaining(t0 + 4 * M), 6 * M);
    }
}
