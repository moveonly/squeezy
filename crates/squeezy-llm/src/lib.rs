use std::{pin::Pin, sync::Arc};

use futures_core::Stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
pub use squeezy_core::{
    AnthropicThinkingBlock, AnthropicThinkingKind, ReasoningKind, ReasoningPayload,
    ReasoningSnapshot,
};
use squeezy_core::{CostSnapshot, ReasoningEffort, ResponseVerbosity, Result, SqueezyError};
use tokio_util::sync::CancellationToken;

pub const INVALID_TOOL_ARGUMENTS_KEY: &str = "__squeezy_invalid_tool_arguments";
pub const INVALID_TOOL_ARGUMENTS_ERROR_KEY: &str = "__squeezy_parse_error";
pub const INVALID_TOOL_ARGUMENTS_RAW_KEY: &str = "__squeezy_raw_arguments";

mod anthropic;
mod bedrock;
mod compatible;
mod credentials;
mod google;
pub mod model_discovery;
mod ollama;
mod openai;
mod registry;
mod retry;
mod sse;
pub mod tokens;
pub use tokens::{
    DEFAULT_BYTES_PER_TOKEN, DEFAULT_EMA_ALPHA, ProviderCalibration, TokenCalibration,
    default_bytes_per_token, estimate_tokens,
};

pub use anthropic::AnthropicProvider;
pub use bedrock::BedrockProvider;
pub use compatible::OpenAiCompatibleProvider;
pub use credentials::{KeySource, ResolvedKey, resolve_api_key, resolve_api_key_with_inline};
pub use google::GoogleProvider;
pub use ollama::{OllamaProvider, fetch_ollama_context_window, fetch_ollama_model_names};
pub use openai::OpenAiProvider;
pub use registry::{
    MODEL_REGISTRY, ModelCapabilities, ModelInfo, ModelLifecycle, ModelLimits, PROVIDERS,
    RequestTokenEstimate, TokenPricing, TokenizerKind, capabilities_for, estimate_cost,
    estimate_request_context, estimate_request_context_calibrated, model_info_for,
    models_for_provider, provider_from_config, provider_name,
};

pub type LlmStream = Pin<Box<dyn Stream<Item = Result<LlmEvent>> + Send>>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmRequest {
    pub model: Arc<str>,
    pub instructions: Arc<str>,
    pub input: Arc<[LlmInputItem]>,
    pub max_output_tokens: Option<u32>,
    pub response_verbosity: Option<ResponseVerbosity>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub previous_response_id: Option<String>,
    pub cache_key: Option<String>,
    pub tools: Arc<[Arc<LlmToolSpec>]>,
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
            model: Arc::from(model),
            instructions: Arc::from(instructions),
            input: Arc::from(vec![LlmInputItem::UserText(input)]),
            max_output_tokens,
            response_verbosity: None,
            reasoning_effort: None,
            previous_response_id: None,
            cache_key: None,
            tools: Arc::from(Vec::new()),
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
    Reasoning(ReasoningPayload),
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
    ReasoningDelta {
        text: String,
        kind: ReasoningKind,
    },
    ReasoningDone(ReasoningPayload),
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
