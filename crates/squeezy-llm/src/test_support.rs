use std::sync::Arc;

use squeezy_core::ReasoningEffort;

use crate::{LlmInputItem, LlmRequest, LlmToolSpec};

pub(crate) struct LlmRequestBuilder {
    request: LlmRequest,
}

impl LlmRequestBuilder {
    pub(crate) fn new(model: impl Into<String>) -> Self {
        Self {
            request: LlmRequest {
                model: Arc::from(model.into()),
                ..LlmRequest::default()
            },
        }
    }

    pub(crate) fn instructions(mut self, instructions: impl Into<String>) -> Self {
        self.request.instructions = Arc::from(instructions.into());
        self
    }

    pub(crate) fn user_text(mut self, text: impl Into<String>) -> Self {
        self.request.input = Arc::from(vec![LlmInputItem::UserText(text.into())]);
        self
    }

    pub(crate) fn input_items(mut self, input: Vec<LlmInputItem>) -> Self {
        self.request.input = Arc::from(input);
        self
    }

    pub(crate) fn max_output_tokens(mut self, max_output_tokens: u32) -> Self {
        self.request.max_output_tokens = Some(max_output_tokens);
        self
    }

    pub(crate) fn reasoning_effort(mut self, effort: ReasoningEffort) -> Self {
        self.request.reasoning_effort = Some(effort);
        self
    }

    pub(crate) fn tool(mut self, tool: LlmToolSpec) -> Self {
        let mut tools: Vec<_> = self.request.tools.iter().cloned().collect();
        tools.push(Arc::new(tool));
        self.request.tools = Arc::from(tools);
        self
    }

    pub(crate) fn temperature(mut self, temperature: f32) -> Self {
        self.request.temperature = Some(temperature);
        self
    }

    pub(crate) fn top_p(mut self, top_p: f32) -> Self {
        self.request.top_p = Some(top_p);
        self
    }

    pub(crate) fn seed(mut self, seed: u64) -> Self {
        self.request.seed = Some(seed);
        self
    }

    pub(crate) fn stop(mut self, stop: impl Into<String>) -> Self {
        self.request.stop.push(stop.into());
        self
    }

    pub(crate) fn build(self) -> LlmRequest {
        self.request
    }
}
