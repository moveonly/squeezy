use std::{pin::Pin, sync::Arc};

use futures_core::Stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use squeezy_core::{CostSnapshot, ReasoningEffort, ResponseVerbosity, Result, SqueezyError};
use tokio_util::sync::CancellationToken;

pub const INVALID_TOOL_ARGUMENTS_KEY: &str = "__squeezy_invalid_tool_arguments";
pub const INVALID_TOOL_ARGUMENTS_ERROR_KEY: &str = "__squeezy_parse_error";
pub const INVALID_TOOL_ARGUMENTS_RAW_KEY: &str = "__squeezy_raw_arguments";

mod anthropic;
mod bedrock;
mod credentials;
mod google;
mod keychain;
mod ollama;
mod openai;
mod registry;
mod retry;
pub mod tokens;

pub use anthropic::AnthropicProvider;
pub use bedrock::BedrockProvider;
pub use credentials::{
    DefaultCredentialStore, KeyringCredentialStore, resolve_api_key, save_api_key,
    save_api_key_with_store,
};
pub use google::GoogleProvider;
pub use ollama::{OllamaProvider, fetch_ollama_context_window, fetch_ollama_model_names};
pub use openai::OpenAiProvider;
pub use registry::{
    MODEL_REGISTRY, ModelCapabilities, ModelInfo, ModelLifecycle, ModelLimits, PROVIDERS,
    RequestTokenEstimate, TokenPricing, TokenizerKind, capabilities_for, estimate_cost,
    estimate_request_context, model_info_for, models_for_provider, provider_from_config,
    provider_name,
};

pub type LlmStream = Pin<Box<dyn Stream<Item = Result<LlmEvent>> + Send>>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmRequest {
    pub model: String,
    pub instructions: String,
    pub input: Vec<LlmInputItem>,
    pub max_output_tokens: Option<u32>,
    pub response_verbosity: Option<ResponseVerbosity>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub previous_response_id: Option<String>,
    pub cache_key: Option<String>,
    pub tools: Vec<LlmToolSpec>,
    pub store: bool,
}

impl LlmRequest {
    pub fn user_text(
        model: String,
        instructions: String,
        input: String,
        max_output_tokens: Option<u32>,
    ) -> Self {
        Self {
            model,
            instructions,
            input: vec![LlmInputItem::UserText(input)],
            max_output_tokens,
            response_verbosity: None,
            reasoning_effort: None,
            previous_response_id: None,
            cache_key: None,
            tools: Vec::new(),
            store: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum LlmInputItem {
    UserText(String),
    AssistantText(String),
    FunctionCall {
        call_id: String,
        name: String,
        arguments: Value,
    },
    FunctionCallOutput {
        call_id: String,
        output: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub strict: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmToolCall {
    pub call_id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum LlmEvent {
    Started,
    TextDelta(String),
    ToolCall(LlmToolCall),
    Completed {
        response_id: Option<String>,
        cost: CostSnapshot,
    },
    Cancelled,
}

pub trait LlmProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn stream_response(&self, request: LlmRequest, cancel: CancellationToken) -> LlmStream;
}

#[derive(Debug, Clone)]
pub struct UnavailableProvider {
    name: &'static str,
    reason: Arc<str>,
}

impl UnavailableProvider {
    pub fn new(name: &'static str, reason: impl Into<String>) -> Self {
        Self {
            name,
            reason: Arc::from(reason.into()),
        }
    }
}

impl LlmProvider for UnavailableProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn stream_response(&self, _request: LlmRequest, _cancel: CancellationToken) -> LlmStream {
        let reason = self.reason.clone();
        Box::pin(futures_util::stream::once(async move {
            Err(SqueezyError::ProviderNotConfigured(reason.to_string()))
        }))
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
