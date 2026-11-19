//! What was said. Phase 1 keeps the session transcript in memory only;
//! phase 3 moves this to SQLite and adds facts, sessions and search.

use std::time::SystemTime;

use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Speaker {
    Me,
    Her,
}

#[derive(Clone, Debug)]
#[allow(dead_code)] // read by history (phase 2) and SQLite storage (phase 3)
pub struct Line {
    pub speaker: Speaker,
    pub text: String,
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
}
