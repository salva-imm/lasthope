//! OpenAI-compatible chat client. Any provider that serves
//! `POST {base_url}/chat/completions` works (Groq, Cerebras, OpenAI, a local
//! llama.cpp/ollama server, ...), selected purely by `LlmConfig`.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::config::LlmConfig;

#[derive(Clone, Debug, Serialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    #[allow(dead_code)] // conversation history arrives in phase 2
    Assistant,
}

impl ChatMessage {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self { role, content: content.into() }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Sampling {
    pub temperature: f32,
    pub seed: Option<u64>,
    /// Ask for a JSON object (`response_format`); the prompt must say JSON.
    pub json: bool,
}

#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    temperature: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    seed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<serde_json::Value>,
    stream: bool,
}

#[derive(Deserialize)]
struct Response {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: Option<String>,
}

#[derive(Clone)]
pub struct LlmClient {
    http: reqwest::Client,
    cfg: LlmConfig,
}

impl LlmClient {
    pub fn new(cfg: LlmConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(45))
            .build()
            .context("building HTTP client")?;
        Ok(Self { http, cfg })
    }

    pub fn model(&self) -> &str {
        &self.cfg.model
    }

    pub async fn chat(&self, messages: &[ChatMessage], sampling: Sampling) -> Result<String> {
        self.chat_reporting(messages, sampling, |_| {}).await
    }

    /// Like `chat`, but calls `on_retry` each time a failed attempt is about
    /// to be retried, so the caller can tell the user what is going on.
    pub async fn chat_reporting(
        &self,
        messages: &[ChatMessage],
        sampling: Sampling,
        on_retry: impl Fn(Trouble),
    ) -> Result<String> {
        let url = format!("{}/chat/completions", self.cfg.base_url);
        let mut body = Request {
            model: &self.cfg.model,
            messages,
            temperature: sampling.temperature,
            seed: sampling.seed,
            response_format: sampling.json.then(|| serde_json::json!({ "type": "json_object" })),
            stream: false,
        };
        // Free tiers rate-limit by tokens per minute: wait as asked and retry.
        let mut attempt = 0;
        let text = loop {
            attempt += 1;
            let sent = self.http.post(&url).bearer_auth(&self.cfg.api_key).json(&body).send().await;
            let res = match sent {
                Ok(res) => res,
                // timeouts and dropped connections: worth another try
                Err(e) if attempt < MAX_ATTEMPTS && (e.is_timeout() || e.is_connect() || e.is_request()) => {
                    let wait = 2.0 * attempt as f64;
                    eprintln!("  · the mind is unreachable ({e}); retrying in {wait:.0}s");
                    on_retry(Trouble::Unavailable);
                    tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                    continue;
                }
                Err(e) => return Err(e).with_context(|| format!("request to {url} failed")),
            };
            let status = res.status();
            let retry_after = res
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<f64>().ok());
            let text = res.text().await.context("reading LLM response")?;
            if status.is_success() {
                break text;
            }
            // a provider without JSON mode: ask again without it (the prompt still asks for JSON)
            if status.as_u16() == 400 && body.response_format.is_some() && text.contains("response_format") {
                body.response_format = None;
                attempt -= 1;
                continue;
            }
            let trouble = match status.as_u16() {
                429 => Trouble::RateLimited,
                500 | 502 | 503 | 504 | 529 => Trouble::Unavailable,
                _ => bail!("LLM returned {status}: {}", truncate(&text, 400)),
            };
            if attempt >= MAX_ATTEMPTS {
                bail!("LLM returned {status}: {}", truncate(&text, 400));
            }
            let wait = retry_after
                .or_else(|| retry_hint(&text))
                .unwrap_or(2.0 * attempt as f64)
                .clamp(0.5, MAX_RETRY_WAIT_SECS);
            eprintln!("  · the mind is busy ({status}); retrying in {wait:.1}s");
            on_retry(trouble);
            tokio::time::sleep(Duration::from_secs_f64(wait)).await;
        };
        let parsed: Response = serde_json::from_str(&text)
            .with_context(|| format!("unexpected LLM response: {}", truncate(&text, 400)))?;
        parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("LLM returned no content"))
    }
}

/// Why a request is being retried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trouble {
    /// 429: the provider's rate limit (tokens or requests per minute).
    RateLimited,
    /// 5xx, a timeout or a dropped connection.
    Unavailable,
}

impl Trouble {
    /// What she says while retrying.
    pub fn retrying_line(self) -> &'static str {
        match self {
            Trouble::RateLimited => "Retrying, the API hit the limit.",
            Trouble::Unavailable => "Retrying, the API is not answering.",
        }
    }

    /// What she says when the retries are used up.
    pub fn gave_up_line(self) -> &'static str {
        match self {
            Trouble::RateLimited => "The API is still at its limit. Give it a minute, then speak again.",
            Trouble::Unavailable => "The API is not answering. Speak again in a moment.",
        }
    }
}

/// What she says when a request fails without being retried.
pub const FAILED_LINE: &str = "The API failed. Speak again in a moment.";

const MAX_ATTEMPTS: u32 = 4;
const MAX_RETRY_WAIT_SECS: f64 = 15.0;

/// "Please try again in 3.44s" (Groq, OpenAI) -> 3.44
fn retry_hint(body: &str) -> Option<f64> {
    let rest = &body[body.find("try again in ")? + "try again in ".len()..];
    let num: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let secs: f64 = num.parse().ok()?;
    let unit = &rest[num.len()..];
    Some(if unit.starts_with("ms") { secs / 1000.0 } else { secs })
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_retry_hints() {
        assert_eq!(retry_hint("Rate limit reached ... Please try again in 3.4425s. Need more"), Some(3.4425));
        assert_eq!(retry_hint("try again in 250ms"), Some(0.25));
        assert_eq!(retry_hint("no hint"), None);
    }
}
