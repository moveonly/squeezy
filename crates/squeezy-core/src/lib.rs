use std::{env, fmt, time::Duration};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_OPENAI_MODEL: &str = "gpt-5-nano";
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 128;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppConfig {
    pub provider: ProviderConfig,
    pub model: String,
    pub instructions: String,
    pub max_output_tokens: Option<u32>,
    pub tick_rate: Duration,
}

impl AppConfig {
    pub fn from_env() -> Self {
        Self::from_env_vars(|name| env::var(name).ok())
    }

    fn from_env_vars(mut var: impl FnMut(&str) -> Option<String>) -> Self {
        let model = var("SQUEEZY_MODEL").unwrap_or_else(|| DEFAULT_OPENAI_MODEL.to_string());
        let base_url =
            var("OPENAI_BASE_URL").unwrap_or_else(|| DEFAULT_OPENAI_BASE_URL.to_string());
        Self {
            provider: ProviderConfig::OpenAi(OpenAiConfig {
                api_key_env: "OPENAI_API_KEY".to_string(),
                base_url,
            }),
            model,
            instructions: DEFAULT_INSTRUCTIONS.to_string(),
            max_output_tokens: Some(DEFAULT_MAX_OUTPUT_TOKENS),
            tick_rate: Duration::from_millis(50),
        }
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self::from_env()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderConfig {
    OpenAi(OpenAiConfig),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenAiConfig {
    pub api_key_env: String,
    pub base_url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TurnId(u64);

impl TurnId {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for TurnId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "turn-{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptItem {
    pub role: Role,
    pub content: String,
}

impl TranscriptItem {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostSnapshot {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub estimated_usd_micros: Option<u64>,
}

#[derive(Debug, Error)]
pub enum SqueezyError {
    #[error("provider is not configured: {0}")]
    ProviderNotConfigured(String),
    #[error("provider request failed: {0}")]
    ProviderRequest(String),
    #[error("provider stream failed: {0}")]
    ProviderStream(String),
    #[error("terminal error: {0}")]
    Terminal(String),
    #[error("agent error: {0}")]
    Agent(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, SqueezyError>;

pub const DEFAULT_INSTRUCTIONS: &str = "You are Squeezy, a cost-aware coding agent. Keep responses concise, explicit, and grounded in local evidence.";

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
