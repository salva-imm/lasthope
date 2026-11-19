//! What was said. Phase 2 keeps the session transcript in memory and serves
//! a trimmed tail of it as conversation history; phase 3 moves this to SQLite
//! and adds facts, sessions and search.

use std::time::SystemTime;

use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Speaker {
    Me,
    Her,
}

#[derive(Clone, Debug)]
pub struct Line {
    pub speaker: Speaker,
    pub text: String,
    #[allow(dead_code)] // stored with each message in phase 3
    pub at: SystemTime,
}

#[derive(Default)]
pub struct Memory {
    transcript: Vec<Line>,
}

impl Memory {
    pub fn record(&mut self, speaker: Speaker, text: &str) {
        self.transcript.push(Line { speaker, text: text.to_string(), at: SystemTime::now() });
    }

    pub fn transcript(&self) -> &[Line] {
        &self.transcript
    }

    /// The most recent lines that fit in `max_lines` and `max_chars`, always
    /// starting with something I said (chat APIs expect user before assistant).
    /// The newest line is always included, even if it alone is over budget.
    pub fn recent(&self, max_lines: usize, max_chars: usize) -> &[Line] {
        let mut start = self.transcript.len();
        let mut chars = 0;
        while start > 0 {
            let line = &self.transcript[start - 1];
            let n = self.transcript.len() - start;
            if n >= max_lines || (n > 0 && chars + line.text.len() > max_chars) {
                break;
            }
            chars += line.text.len();
            start -= 1;
        }
        while start < self.transcript.len().saturating_sub(1)
            && self.transcript[start].speaker != Speaker::Me
        {
            start += 1;
        }
        &self.transcript[start..]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory(lines: &[(Speaker, &str)]) -> Memory {
        let mut m = Memory::default();
        for (s, t) in lines {
            m.record(*s, t);
        }
        m
    }

    fn texts(lines: &[Line]) -> Vec<&str> {
        lines.iter().map(|l| l.text.as_str()).collect()
    }

    #[test]
    fn keeps_tail_within_line_budget_starting_with_me() {
        use Speaker::*;
        let m = memory(&[(Me, "a"), (Her, "b"), (Me, "c"), (Her, "d"), (Me, "e")]);
        assert_eq!(texts(m.recent(10, 1000)), ["a", "b", "c", "d", "e"]);
        // 4 lines would start with Her "b": skip to Me "c"
        assert_eq!(texts(m.recent(4, 1000)), ["c", "d", "e"]);
    }

    #[test]
    fn respects_char_budget_but_keeps_newest() {
        use Speaker::*;
        let m = memory(&[(Me, "aaaa"), (Her, "bbbb"), (Me, "cccc")]);
        assert_eq!(texts(m.recent(10, 8)), ["cccc"]); // "bbbb"+"cccc" fits, but would start with Her
        assert_eq!(texts(m.recent(10, 12)), ["aaaa", "bbbb", "cccc"]);
        assert_eq!(texts(m.recent(10, 2)), ["cccc"]); // over budget alone, still sent
        assert!(Memory::default().recent(10, 10).is_empty());
    }
}
