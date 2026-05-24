use std::{pin::Pin, sync::Arc};

use futures_core::Stream;
use serde_json::Value;
use squeezy_core::{CostSnapshot, ReasoningEffort, ResponseVerbosity, Result, SqueezyError};
use tokio_util::sync::CancellationToken;

mod anthropic;
mod bedrock;
mod google;
mod ollama;
mod openai;
mod registry;

pub use anthropic::AnthropicProvider;
pub use bedrock::BedrockProvider;
pub use google::GoogleProvider;
pub use ollama::{OllamaProvider, fetch_ollama_context_window};
pub use openai::OpenAiProvider;
pub use registry::{
    MODEL_REGISTRY, ModelCapabilities, ModelInfo, ModelLifecycle, ModelLimits, PROVIDERS,
    RequestTokenEstimate, TokenPricing, TokenizerKind, capabilities_for, estimate_cost,
    estimate_request_context, model_info_for, models_for_provider, provider_from_config,
    provider_name,
};

pub type LlmStream = Pin<Box<dyn Stream<Item = Result<LlmEvent>> + Send>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmRequest {
    pub model: String,
    pub instructions: String,
    pub input: Vec<LlmInputItem>,
    pub max_output_tokens: Option<u32>,
    pub response_verbosity: Option<ResponseVerbosity>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub previous_response_id: Option<String>,
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
            tools: Vec::new(),
            store: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub strict: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmToolCall {
    pub call_id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
