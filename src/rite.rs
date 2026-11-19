//! The rite: the shape of one session, as a pure state machine.
//!
//! open -> (she greets, asks about the last oath) -> my verdict -> talk ...
//! -> "close the rite" -> she asks tomorrow's oath -> I swear it -> she seals
//! it -> close (summary and facts are written by the core).
//!
//! The rite only decides *what happens*; the core does the I/O. Verdicts come
//! from my own words (`parse_verdict`) or the Kept/Broken commands, never from
//! anything the model says.

use crate::streak::Day;

/// The last oath, due and not yet judged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingOath {
    /// The session in which it was sworn.
    pub session: i64,
    pub text: String,
    /// The day it was sworn for.
    pub day: Day,
}

/// What her next reply is about; the core turns this into an instruction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Situation {
    /// The session just opened and she speaks first.
    Greeting { ask_verdict: bool },
    Conversation,
    /// I just declared the last oath kept or broken (already recorded).
    VerdictRecorded { kept: bool, oath: String },
    /// I answered, but not with kept or broken.
    VerdictUnclear,
    /// I want to close: ask for tomorrow's oath.
    AskOath,
    /// I just swore tomorrow's oath (already recorded): seal it and close.
    OathSworn,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Reply(Situation),
    RecordVerdict { oath_session: i64, kept: bool },
    RecordOath(String),
    /// Close the session once her current reply has been spoken.
    CloseAfterReply,
    /// Close the session now.
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    AwaitingVerdict { asked_again: bool },
    Open,
    AwaitingOath,
    Closing,
}

pub struct Rite {
    phase: Phase,
    pending: Option<PendingOath>,
    oath_sworn: bool,
}

impl Rite {
    /// Start a session. If she speaks first, she greets and (if an oath is
    /// due) asks for the verdict; my next words are read as that verdict.
    pub fn begin(pending: Option<PendingOath>, speaks_first: bool) -> (Self, Vec<Action>) {
        let ask = speaks_first && pending.is_some();
        let phase = if ask { Phase::AwaitingVerdict { asked_again: false } } else { Phase::Open };
        let actions = if speaks_first {
            vec![Action::Reply(Situation::Greeting { ask_verdict: ask })]
        } else {
            vec![]
        };
        (Self { phase, pending, oath_sworn: false }, actions)
    }

    /// The due oath still waiting for a verdict, if any.
    pub fn pending(&self) -> Option<&PendingOath> {
        self.pending.as_ref()
    }

    pub fn awaiting_verdict(&self) -> bool {
        matches!(self.phase, Phase::AwaitingVerdict { .. })
    }

    #[cfg(test)]
    fn closing(&self) -> bool {
        self.phase == Phase::Closing
    }

    /// I said something (that was not a spoken command).
    pub fn heard(&mut self, text: &str) -> Vec<Action> {
        match self.phase {
            Phase::AwaitingVerdict { asked_again } => match parse_verdict(text) {
                Some(kept) => {
                    self.phase = Phase::Open;
                    let oath = self.pending.as_ref().map(|o| o.text.clone()).unwrap_or_default();
                    let mut actions = self.record_verdict(kept);
                    actions.push(Action::Reply(Situation::VerdictRecorded { kept, oath }));
                    actions
                }
                None if !asked_again => {
                    self.phase = Phase::AwaitingVerdict { asked_again: true };
                    vec![Action::Reply(Situation::VerdictUnclear)]
                }
                // asked twice: let the conversation go on; Kept/Broken still work
                None => {
                    self.phase = Phase::Open;
                    vec![Action::Reply(Situation::Conversation)]
                }
            },
            Phase::Open => vec![Action::Reply(Situation::Conversation)],
            Phase::AwaitingOath => {
                self.oath_sworn = true;
                self.phase = Phase::Closing;
                vec![
                    Action::RecordOath(text.trim().to_string()),
                    Action::Reply(Situation::OathSworn),
                    Action::CloseAfterReply,
                ]
            }
            Phase::Closing => vec![],
        }
    }

    /// Kept/Broken given by command (text mode, later the UI buttons).
    pub fn verdict_given(&mut self, kept: bool) -> Vec<Action> {
        if self.awaiting_verdict() {
            self.phase = Phase::Open;
        }
        self.record_verdict(kept)
    }

    fn record_verdict(&mut self, kept: bool) -> Vec<Action> {
        match self.pending.take() {
            Some(oath) => vec![Action::RecordVerdict { oath_session: oath.session, kept }],
            None => vec![],
        }
    }

    /// I asked to close the rite. The first time, she asks for tomorrow's
    /// oath; asking again (or once an oath is sworn) closes.
    pub fn close_requested(&mut self) -> Vec<Action> {
        match self.phase {
            Phase::Closing => vec![],
            Phase::AwaitingOath => {
                self.phase = Phase::Closing;
                vec![Action::Close]
            }
            _ if self.oath_sworn => {
                self.phase = Phase::Closing;
                vec![Action::Close]
            }
            _ => {
                self.phase = Phase::AwaitingOath;
                vec![Action::Reply(Situation::AskOath)]
            }
        }
    }
}

/// Read my answer to "kept or broken?". `None` if it says neither.
/// The first decisive word wins, with negation handled:
/// "I didn't keep it" -> broken, "I didn't break it" -> kept.
pub fn parse_verdict(text: &str) -> Option<bool> {
    const KEEP: &[&str] = &[
        "kept", "keep", "yes", "yeah", "yep", "yup", "did", "done", "completed", "complete",
        "finished", "honored", "honoured", "fulfilled", "succeeded", "sealed",
    ];
    const BREAK: &[&str] =
        &["broke", "broken", "break", "failed", "fail", "missed", "miss", "skipped", "skip", "slipped"];
    const NEGATION: &[&str] =
        &["not", "didn't", "didnt", "never", "couldn't", "couldnt", "wasn't", "wasnt", "haven't", "hadn't"];

    let lower = text.to_lowercase().replace('’', "'");
    let words: Vec<&str> = lower
        .split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .filter(|w| !w.is_empty())
        .collect();
    for (i, &w) in words.iter().enumerate() {
        let negated = NEGATION.contains(&w) || (w == "did" && words.get(i + 1) == Some(&"not"));
        if negated {
            for next in words.iter().skip(i + 1).take(3) {
                if BREAK.contains(next) {
                    return Some(true); // "didn't break it"
                }
                if KEEP.contains(next) && *next != "did" {
                    return Some(false); // "didn't keep it"
                }
            }
            return Some(false); // "not really", "I did not"
        }
        if w == "no" || w == "nope" || BREAK.contains(&w) {
            return Some(false);
        }
        if KEEP.contains(&w) {
            return Some(true);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oath() -> PendingOath {
        PendingOath { session: 7, text: "Study the EVM for 25 minutes".into(), day: Day(20_000) }
    }

    #[test]
    fn verdicts_from_plain_speech() {
        for (said, expected) in [
            ("Kept.", Some(true)),
            ("I kept it.", Some(true)),
            ("Yes, I did.", Some(true)),
            ("Done, twenty five minutes.", Some(true)),
            ("I didn't break it.", Some(true)),
            ("Broken.", Some(false)),
            ("No, I broke it.", Some(false)),
            ("I didn't keep it.", Some(false)),
            ("I did not.", Some(false)),
            ("Not really.", Some(false)),
            ("I missed the gym, sorry.", Some(false)),
            ("Kept, though I failed at the gym.", Some(true)),
            ("I don’t know, it was a strange day.", None),
            ("Let me think about it.", None),
        ] {
            assert_eq!(parse_verdict(said), expected, "{said:?}");
        }
    }

    #[test]
    fn full_session_with_oath_due() {
        let (mut r, actions) = Rite::begin(Some(oath()), true);
        assert_eq!(actions, [Action::Reply(Situation::Greeting { ask_verdict: true })]);
        assert!(r.awaiting_verdict());

        assert_eq!(
            r.heard("I kept it."),
            [
                Action::RecordVerdict { oath_session: 7, kept: true },
                Action::Reply(Situation::VerdictRecorded { kept: true, oath: oath().text }),
            ]
        );
        assert!(r.pending().is_none());
        assert_eq!(r.heard("What is a storage slot?"), [Action::Reply(Situation::Conversation)]);

        assert_eq!(r.close_requested(), [Action::Reply(Situation::AskOath)]);
        assert_eq!(
            r.heard("I swear to finish the networking chapter."),
            [
                Action::RecordOath("I swear to finish the networking chapter.".into()),
                Action::Reply(Situation::OathSworn),
                Action::CloseAfterReply,
            ]
        );
        assert!(r.closing());
        assert!(r.heard("anything else").is_empty());
        assert!(r.close_requested().is_empty());
    }

    #[test]
    fn unclear_verdict_is_asked_once_more_then_dropped() {
        let (mut r, _) = Rite::begin(Some(oath()), true);
        assert_eq!(r.heard("Hmm, it was a long day."), [Action::Reply(Situation::VerdictUnclear)]);
        assert_eq!(r.heard("Let me think."), [Action::Reply(Situation::Conversation)]);
        assert!(!r.awaiting_verdict());
        // still recordable by command afterwards
        assert_eq!(r.verdict_given(false), [Action::RecordVerdict { oath_session: 7, kept: false }]);
        assert!(r.verdict_given(true).is_empty()); // only once
    }

    #[test]
    fn verdict_by_command_while_she_waits() {
        let (mut r, _) = Rite::begin(Some(oath()), true);
        assert_eq!(r.verdict_given(true), [Action::RecordVerdict { oath_session: 7, kept: true }]);
        assert_eq!(r.heard("Good."), [Action::Reply(Situation::Conversation)]);
    }

    #[test]
    fn first_session_and_silent_start() {
        let (r, actions) = Rite::begin(None, true);
        assert_eq!(actions, [Action::Reply(Situation::Greeting { ask_verdict: false })]);
        assert!(!r.awaiting_verdict());

        let (mut r, actions) = Rite::begin(Some(oath()), false);
        assert!(actions.is_empty());
        assert_eq!(r.heard("Hello."), [Action::Reply(Situation::Conversation)]);
        assert!(r.pending().is_some());
    }

    #[test]
    fn closing_twice_skips_the_oath() {
        let (mut r, _) = Rite::begin(None, true);
        assert_eq!(r.close_requested(), [Action::Reply(Situation::AskOath)]);
        assert_eq!(r.close_requested(), [Action::Close]);
    }
}
