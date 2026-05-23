use std::{pin::Pin, sync::Arc};

use futures_core::Stream;
use squeezy_core::{CostSnapshot, Result, SqueezyError};
use tokio_util::sync::CancellationToken;

mod openai;

pub use openai::OpenAiProvider;

pub type LlmStream = Pin<Box<dyn Stream<Item = Result<LlmEvent>> + Send>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmRequest {
    pub model: String,
    pub instructions: String,
    pub input: String,
    pub max_output_tokens: Option<u32>,
    pub previous_response_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlmEvent {
    Started,
    TextDelta(String),
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
