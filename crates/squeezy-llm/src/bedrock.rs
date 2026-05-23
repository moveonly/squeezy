use futures_util::stream;
use squeezy_core::{BedrockConfig, Result, SqueezyError};
use tokio_util::sync::CancellationToken;

use crate::{LlmProvider, LlmRequest, LlmStream};

#[derive(Debug, Clone)]
pub struct BedrockProvider {
    region: String,
    base_url: Option<String>,
}

impl BedrockProvider {
    pub fn from_config(config: &BedrockConfig) -> Result<Self> {
        Ok(Self {
            region: config.region.clone(),
            base_url: config.base_url.clone(),
        })
    }
}

impl LlmProvider for BedrockProvider {
    fn name(&self) -> &'static str {
        "bedrock"
    }

    fn stream_response(&self, _request: LlmRequest, _cancel: CancellationToken) -> LlmStream {
        let endpoint = self
            .base_url
            .clone()
            .unwrap_or_else(|| format!("bedrock-runtime.{}.amazonaws.com", self.region));
        Box::pin(stream::once(async move {
            Err(SqueezyError::ProviderNotConfigured(format!(
                "AWS Bedrock provider is registered for {endpoint}, but signed ConverseStream transport is not enabled in this build"
            )))
        }))
    }
}
