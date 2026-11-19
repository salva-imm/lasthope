//! Settings from environment variables, with an optional `.env` file in the
//! working directory. Real environment variables win over `.env`.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Result, bail};

#[derive(Clone, Debug)]
pub struct LlmConfig {
    /// OpenAI-compatible base URL, e.g. `https://api.groq.com/openai/v1`.
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub llm: LlmConfig,
    /// Kokoro model directory (model.onnx, voices.bin, tokens.txt).
    pub kokoro_dir: PathBuf,
    /// Directory holding silero_vad.onnx and the sherpa-onnx Whisper model.
    pub models_dir: PathBuf,
    /// Her character: the system prompt, re-read on every request.
    pub persona_path: PathBuf,
    /// Sampling temperature (variety of her replies).
    pub temperature: f32,
    /// Microphone name (substring) or "default"; unset avoids a headset's mic.
    pub mic_device: Option<String>,
    /// Silence that ends my turn.
    pub end_of_turn: std::time::Duration,
    /// Her memory: the SQLite database file.
    pub db_path: PathBuf,
    /// The web UI: address and port to listen on, and where its files are.
    pub ui_bind: std::net::IpAddr,
    pub ui_port: u16,
    pub ui_dir: PathBuf,
    /// The grimoire: a folder of PDFs.
    pub books_dir: PathBuf,
}

impl Config {
    pub fn load() -> Result<Self> {
        let dotenv = read_dotenv(".env");
        let get = |key: &str| -> Option<String> {
            std::env::var(key)
                .ok()
                .or_else(|| dotenv.get(key).cloned())
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };

        let required = ["LLM_BASE_URL", "LLM_API_KEY", "LLM_MODEL"];
        let missing: Vec<&str> = required.iter().copied().filter(|k| get(k).is_none()).collect();
        if !missing.is_empty() {
            bail!(
                "missing settings: {}. Set them in the environment or in a .env file \
                 (see .env.example).",
                missing.join(", ")
            );
        }

        Ok(Self {
            llm: LlmConfig {
                base_url: get("LLM_BASE_URL").unwrap().trim_end_matches('/').to_string(),
                api_key: get("LLM_API_KEY").unwrap(),
                model: get("LLM_MODEL").unwrap(),
            },
            kokoro_dir: get("KOKORO_DIR").unwrap_or("./kokoro-multi-lang-v1_0".into()).into(),
            models_dir: get("MODELS_DIR").unwrap_or("./models".into()).into(),
            persona_path: get("PERSONA_PATH").unwrap_or("./persona.md".into()).into(),
            temperature: match get("LLM_TEMPERATURE") {
                None => 0.8,
                Some(t) => t
                    .parse()
                    .ok()
                    .filter(|t: &f32| (0.0..=2.0).contains(t))
                    .ok_or_else(|| anyhow::anyhow!("LLM_TEMPERATURE must be a number from 0 to 2, got {t:?}"))?,
            },
            mic_device: get("MIC_DEVICE"),
            ui_bind: match get("UI_BIND") {
                None => std::net::Ipv4Addr::LOCALHOST.into(),
                Some(a) => a
                    .parse()
                    .map_err(|_| anyhow::anyhow!("UI_BIND must be an IP address (127.0.0.1, 0.0.0.0, …), got {a:?}"))?,
            },
            ui_port: match get("UI_PORT") {
                None => 7700,
                Some(p) => p.parse().map_err(|_| anyhow::anyhow!("UI_PORT must be a port number, got {p:?}"))?,
            },
            ui_dir: get("UI_DIR").unwrap_or("./ui".into()).into(),
            books_dir: get("BOOKS_DIR").unwrap_or("./books".into()).into(),
            db_path: get("DB_PATH").unwrap_or("./lasthope.db".into()).into(),
            end_of_turn: match get("END_OF_TURN_MS") {
                None => std::time::Duration::from_millis(1200),
                Some(ms) => ms
                    .parse()
                    .ok()
                    .filter(|ms: &u64| (300..=5000).contains(ms))
                    .map(std::time::Duration::from_millis)
                    .ok_or_else(|| anyhow::anyhow!("END_OF_TURN_MS must be 300 to 5000, got {ms:?}"))?,
            },
        })
    }
}

/// Minimal `.env` reader: `KEY=VALUE` lines, `#` comments, optional quotes.
fn read_dotenv(path: &str) -> HashMap<String, String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    parse_dotenv(&text)
}

fn parse_dotenv(text: &str) -> HashMap<String, String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let l = l.strip_prefix("export ").unwrap_or(l);
            let (k, v) = l.split_once('=')?;
            let v = v.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
                .unwrap_or(v);
            Some((k.trim().to_string(), v.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotenv_parsing() {
        let m = parse_dotenv(
            "# comment\nLLM_MODEL=abc\nexport LLM_API_KEY=\"sk-1\"\n\nLLM_BASE_URL='http://x/v1'\nbad line\n",
        );
        assert_eq!(m["LLM_MODEL"], "abc");
        assert_eq!(m["LLM_API_KEY"], "sk-1");
        assert_eq!(m["LLM_BASE_URL"], "http://x/v1");
        assert_eq!(m.len(), 3);
    }
}
