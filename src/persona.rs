//! Who she is. The character lives in `persona.md` (re-read on every request,
//! so edits apply immediately, no restart). This module builds the messages
//! for each request and cleans what comes back.
//!
//! The prompt asks for short spoken replies, but the model is never trusted to
//! comply: `clean_reply` strips reasoning blocks and markdown and caps length.

use std::path::{Path, PathBuf};

use crate::llm::{ChatMessage, Role, Sampling};
use crate::memory::{Line, Speaker};
use crate::speech_out::split_sentences;

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

    /// System prompt + recent conversation (which ends with what I just said).
    pub fn messages(&mut self, history: &[Line]) -> Vec<ChatMessage> {
        let mut out = vec![ChatMessage::new(Role::System, self.system_prompt())];
        out.extend(history.iter().map(|l| {
            let role = match l.speaker {
                Speaker::Me => Role::User,
                Speaker::Her => Role::Assistant,
            };
            ChatMessage::new(role, l.text.clone())
        }));
        out
    }
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
    use std::time::SystemTime;

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
        let line = |speaker, text: &str| Line { speaker, text: text.into(), at: SystemTime::now() };
        let msgs = p.messages(&[line(Speaker::Me, "hi"), line(Speaker::Her, "Speak."), line(Speaker::Me, "ok")]);
        let roles: Vec<Role> = msgs.iter().map(|m| m.role).collect();
        assert_eq!(roles, [Role::System, Role::User, Role::Assistant, Role::User]);
        assert_eq!(msgs[0].content, "You are her.");
        std::fs::remove_file(dir).unwrap();
    }
}
