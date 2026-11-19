//! Who she is: the system prompt and sampling settings.
//! Phase 1 keeps the original behaviour; phase 2 loads `persona.md`.

use crate::llm::{ChatMessage, Role, Sampling};

const SYSTEM_PROMPT: &str =
    "Please do not include any asterisks (*) in your replies. Skip or remove them entirely.";

pub const SAMPLING: Sampling = Sampling { temperature: 0.0, seed: Some(0) };

/// Messages for one request: the system prompt plus what was just said.
pub fn messages(user_text: &str) -> Vec<ChatMessage> {
    vec![
        ChatMessage::new(Role::System, SYSTEM_PROMPT),
        ChatMessage::new(Role::User, user_text),
    ]
}
