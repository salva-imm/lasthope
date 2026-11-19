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
}

#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    temperature: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    seed: Option<u64>,
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
        let url = format!("{}/chat/completions", self.cfg.base_url);
        let body = Request {
            model: &self.cfg.model,
            messages,
            temperature: sampling.temperature,
            seed: sampling.seed,
            stream: false,
        };
        let res = self
            .http
            .post(&url)
            .bearer_auth(&self.cfg.api_key)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("request to {url} failed"))?;

        let status = res.status();
        let text = res.text().await.context("reading LLM response")?;
        if !status.is_success() {
            bail!("LLM returned {status}: {}", truncate(&text, 400));
        }
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

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}
