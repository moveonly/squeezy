//! Built-in scripted `LlmProvider` so eval scenarios run offline.
//!
//! Activated by `[squeezy] provider = "mock"` in a scenario TOML. The
//! mock pops a turn from a queue on each request and emits a
//! `Started` -> `TextDelta(text)` -> `Completed` event stream. Optional
//! `tool_calls` per turn let scenarios exercise the approval/tool-call
//! path without hitting a real provider.

use std::sync::{Arc, Mutex};

use futures_util::stream::{self};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use squeezy_core::CostSnapshot;
use squeezy_llm::{LlmEvent, LlmProvider, LlmRequest, LlmStream, LlmToolCall};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MockProviderConfig {
    /// Default text to return when the turn queue is empty.
    #[serde(default)]
    pub default_text: Option<String>,
    /// Scripted per-turn responses, popped in order.
    #[serde(default)]
    pub turns: Vec<MockTurn>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MockTurn {
    /// Streamed assistant text, sent as a single `TextDelta`.
    #[serde(default)]
    pub text: Option<String>,
    /// Optional scripted tool calls. The agent will fire each one as a
    /// real tool call against the local workspace, so use tool names
    /// you trust here (or pair with `approve` actions).
    #[serde(default)]
    pub tool_calls: Vec<MockToolCall>,
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MockToolCall {
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

pub struct MockProvider {
    state: Arc<Mutex<State>>,
}

struct State {
    config: MockProviderConfig,
    cursor: usize,
}

impl MockProvider {
    pub fn new(config: MockProviderConfig) -> Self {
        Self {
            state: Arc::new(Mutex::new(State { config, cursor: 0 })),
        }
    }

    pub fn shared(config: MockProviderConfig) -> Arc<dyn LlmProvider> {
        Arc::new(Self::new(config))
    }
}

impl LlmProvider for MockProvider {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn stream_response(&self, _request: LlmRequest, _cancel: CancellationToken) -> LlmStream {
        let mut guard = self.state.lock().expect("mock provider lock");
        let turn = if guard.cursor < guard.config.turns.len() {
            let turn = guard.config.turns[guard.cursor].clone();
            guard.cursor += 1;
            turn
        } else {
            MockTurn {
                text: guard
                    .config
                    .default_text
                    .clone()
                    .or_else(|| Some("(mock provider: no turn scripted)".into())),
                ..Default::default()
            }
        };
        drop(guard);

        let mut events: Vec<Result<LlmEvent, squeezy_core::SqueezyError>> = Vec::new();
        events.push(Ok(LlmEvent::Started));
        if let Some(text) = turn.text
            && !text.is_empty()
        {
            events.push(Ok(LlmEvent::TextDelta(text)));
        }
        for (idx, call) in turn.tool_calls.into_iter().enumerate() {
            events.push(Ok(LlmEvent::ToolCall(LlmToolCall {
                call_id: format!("mock-{idx}"),
                name: call.name,
                arguments: if call.arguments.is_null() {
                    json!({})
                } else {
                    call.arguments
                },
            })));
        }
        let cost = CostSnapshot {
            input_tokens: turn.input_tokens,
            output_tokens: turn.output_tokens,
            ..CostSnapshot::default()
        };
        events.push(Ok(LlmEvent::Completed {
            response_id: None,
            cost,
        }));
        Box::pin(stream::iter(events))
    }
}
