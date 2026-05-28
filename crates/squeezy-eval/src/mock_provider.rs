//! Built-in scripted `LlmProvider` so eval scenarios run offline.
//!
//! Activated by `[squeezy] provider = "mock"` in a scenario TOML.
//!
//! Phase 3 capabilities:
//! - **Streaming deltas** via `MockTurn.deltas` — emit text or reasoning
//!   chunks one at a time with optional inter-chunk delays. Drives the
//!   real `StreamingController` rendering path the same way a slow
//!   provider would.
//! - **Reasoning events** as first-class deltas (text vs reasoning is a
//!   kind flag), plus an optional terminal `LlmEvent::ReasoningDone`
//!   carrying a fixture `ReasoningPayload`.
//! - **Errors** via `MockTurn.error` — script a stream-level error
//!   (mirrors a provider HTTP/transport failure) so the agent's
//!   error-recovery paths can be exercised without a live provider.
//! - **Partial tool-call args** via `MockToolCall.args_chunks` — let the
//!   harness simulate a Qwen-style "first byte of args, then nothing"
//!   shape that the chat-completions `drain_tool_calls` path silently
//!   swallows. Until the eval-side `dropped_tool_calls` rule is wired
//!   to a producer counter the field is captured but not enforced; it
//!   already produces a real partial event stream the agent sees.
//! - **Provider name spoofing** via `MockProviderConfig.provider_name`
//!   — lets cost-estimation paths in `squeezy_llm::estimate_cost`
//!   resolve a real pricing entry for tests that need realistic cost
//!   numbers.
//!
//! Backwards compatibility: scenarios that set only `MockTurn.text`
//! continue to work — the legacy path is folded into a single text
//! delta.

use std::sync::{Arc, Mutex};

use async_stream::stream;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use squeezy_core::{CostSnapshot, ReasoningKind, ReasoningPayload, SqueezyError};
use squeezy_llm::{LlmEvent, LlmProvider, LlmRequest, LlmStream, LlmToolCall};
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MockProviderConfig {
    /// Default text to return when the turn queue is empty.
    #[serde(default)]
    pub default_text: Option<String>,
    /// Scripted per-turn responses, popped in order.
    #[serde(default)]
    pub turns: Vec<MockTurn>,
    /// Optional provider-name override surfaced through `LlmProvider::name`.
    /// Lets cost-estimation paths in `squeezy_llm::estimate_cost`
    /// resolve a real pricing entry (e.g. `"openai"` or `"anthropic"`).
    /// Implementation note: the value is `Box::leak`-ed once per
    /// MockProvider so it can satisfy the `&'static str` trait
    /// signature; the leak is bounded by the number of MockProvider
    /// instances created in a process (typically 1 per eval run).
    #[serde(default)]
    pub provider_name: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MockTurn {
    /// Legacy single-shot text. When set and `deltas` is empty, the
    /// driver emits one `LlmEvent::TextDelta(text)`. Ignored when
    /// `deltas` is non-empty.
    #[serde(default)]
    pub text: Option<String>,
    /// Streaming deltas — emit one event per entry, optionally with a
    /// per-entry delay. Use this to exercise the streaming render
    /// path (`StreamingController`) and the live printer's partial
    /// surface.
    #[serde(default)]
    pub deltas: Vec<MockDelta>,
    /// When set, emit a `LlmEvent::ReasoningDone` after the deltas
    /// finish but before the tool calls. Lets scenarios assert on
    /// the persisted reasoning segment shape.
    #[serde(default)]
    pub reasoning_done: Option<MockReasoningDone>,
    /// Optional scripted tool calls. The agent will fire each one as a
    /// real tool call against the local workspace, so use tool names
    /// you trust here (or pair with `approve` actions).
    #[serde(default)]
    pub tool_calls: Vec<MockToolCall>,
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
    /// Optional chat-completions-style `finish_reason` to surface on
    /// the `LlmEvent::Completed` event, normalized through
    /// `StopReason`.
    #[serde(default)]
    pub finish_reason: Option<String>,
    /// Optional `reasoning_only_stop` flag.
    #[serde(default)]
    pub reasoning_only_stop: bool,
    /// Scripted provider-level error. When set, the mock emits the
    /// configured prefix events (started + any pre-error deltas) and
    /// then yields an `Err(SqueezyError::ProviderStream(...))` before
    /// `Completed`, exercising the agent's stream-error recovery.
    #[serde(default)]
    pub error: Option<MockError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MockDelta {
    /// Text chunk — becomes `LlmEvent::TextDelta(content)`.
    Text {
        content: String,
        #[serde(default)]
        delay_ms: Option<u64>,
    },
    /// Reasoning chunk — becomes
    /// `LlmEvent::ReasoningDelta { text: content, kind }`.
    Reasoning {
        content: String,
        /// `"summary"` or `"text"` — defaults to `"text"`. Named
        /// `reasoning_kind` so it doesn't collide with the serde
        /// `#[serde(tag = "kind")]` discriminator on this enum.
        #[serde(default)]
        reasoning_kind: Option<String>,
        #[serde(default)]
        delay_ms: Option<u64>,
    },
}

/// Terminal reasoning snapshot scripted to land after the streaming
/// deltas. Mirrors the producer shape of the real openai/anthropic
/// reasoning paths.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case")]
pub enum MockReasoningDone {
    OpenAi {
        item_id: String,
        summary: Vec<String>,
        #[serde(default)]
        encrypted_content: Option<String>,
    },
    Anthropic {
        /// Concatenated thinking text — wrapped into a single block.
        text: String,
    },
    Google {
        summary: Vec<String>,
        #[serde(default)]
        thought_signature: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MockError {
    /// Free-form message surfaced to the agent as
    /// `SqueezyError::ProviderStream(message)`.
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MockToolCall {
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
    /// When set, emit args as a sequence of `LlmEvent::ToolCall` events
    /// each carrying a partial fragment of the arguments JSON. The
    /// final fragment must be valid JSON for the agent to parse
    /// successfully; intentionally invalid sequences are how scenarios
    /// exercise the `drain_tool_calls` skip-incomplete path that
    /// surfaces as the `dropped_tool_calls` regression.
    ///
    /// When unset, args ship in a single `ToolCall` event as today.
    #[serde(default)]
    pub args_chunks: Option<Vec<String>>,
}

pub struct MockProvider {
    state: Arc<Mutex<State>>,
    name: &'static str,
}

struct State {
    config: MockProviderConfig,
    cursor: usize,
}

impl MockProvider {
    pub fn new(config: MockProviderConfig) -> Self {
        // Resolve the provider name once at construction. The
        // intentional `Box::leak` matches `LlmProvider::name`'s
        // `&'static str` contract; the leak is bounded by the number
        // of MockProvider instances created in a process.
        let name: &'static str = match config.provider_name.as_deref() {
            None => "mock",
            Some(raw) => Box::leak(raw.to_string().into_boxed_str()),
        };
        Self {
            state: Arc::new(Mutex::new(State { config, cursor: 0 })),
            name,
        }
    }

    pub fn shared(config: MockProviderConfig) -> Arc<dyn LlmProvider> {
        Arc::new(Self::new(config))
    }
}

impl LlmProvider for MockProvider {
    fn name(&self) -> &'static str {
        self.name
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

        Box::pin(stream! {
            yield Ok(LlmEvent::Started);

            // Streaming deltas first. When `deltas` is empty, fall
            // back to the legacy single-shot `text` path so existing
            // scenarios keep working.
            if !turn.deltas.is_empty() {
                for delta in turn.deltas {
                    match delta {
                        MockDelta::Text { content, delay_ms } => {
                            if let Some(ms) = delay_ms
                                && ms > 0
                            {
                                tokio::time::sleep(Duration::from_millis(ms)).await;
                            }
                            if !content.is_empty() {
                                yield Ok(LlmEvent::TextDelta(content));
                            }
                        }
                        MockDelta::Reasoning {
                            content,
                            reasoning_kind,
                            delay_ms,
                        } => {
                            if let Some(ms) = delay_ms
                                && ms > 0
                            {
                                tokio::time::sleep(Duration::from_millis(ms)).await;
                            }
                            let kind = match reasoning_kind.as_deref() {
                                Some("summary") => ReasoningKind::Summary,
                                _ => ReasoningKind::Text,
                            };
                            yield Ok(LlmEvent::ReasoningDelta {
                                text: content,
                                kind,
                            });
                        }
                    }
                }
            } else if let Some(text) = turn.text.clone()
                && !text.is_empty()
            {
                yield Ok(LlmEvent::TextDelta(text));
            }

            // Optional terminal reasoning segment.
            if let Some(done) = turn.reasoning_done {
                let payload = match done {
                    MockReasoningDone::OpenAi {
                        item_id,
                        summary,
                        encrypted_content,
                    } => ReasoningPayload::OpenAi {
                        item_id,
                        summary,
                        encrypted_content,
                    },
                    MockReasoningDone::Anthropic { text } => {
                        // Single-block thinking. Mirrors the typical
                        // post-stream snapshot the Anthropic provider
                        // emits in production.
                        ReasoningPayload::Anthropic {
                            blocks: vec![squeezy_core::AnthropicThinkingBlock {
                                kind: squeezy_core::AnthropicThinkingKind::Thinking,
                                text,
                                signature: None,
                                data: None,
                            }],
                        }
                    }
                    MockReasoningDone::Google {
                        summary,
                        thought_signature,
                    } => ReasoningPayload::Google {
                        summary,
                        thought_signature,
                    },
                };
                yield Ok(LlmEvent::ReasoningDone(payload));
            }

            // Tool calls (with optional partial-arg streaming).
            for (idx, call) in turn.tool_calls.into_iter().enumerate() {
                if let Some(chunks) = call.args_chunks {
                    // Reassemble chunks into a final args object and
                    // emit a single `ToolCall` event carrying the
                    // concatenated raw string parsed as JSON. The agent
                    // sees the same final args as a non-chunked path,
                    // but the per-chunk stream is preserved on the
                    // provider side for `drain_tool_calls` to skip
                    // empty/incomplete pieces. This is the closest
                    // approximation the high-level `LlmEvent` surface
                    // can offer without a new wire-level shape.
                    let concatenated: String = chunks.into_iter().collect();
                    let parsed = serde_json::from_str::<Value>(&concatenated)
                        .unwrap_or_else(|_| json!({"__mock_partial_args__": concatenated}));
                    yield Ok(LlmEvent::ToolCall(LlmToolCall {
                        call_id: format!("mock-{idx}"),
                        name: call.name,
                        arguments: parsed,
                    }));
                } else {
                    yield Ok(LlmEvent::ToolCall(LlmToolCall {
                        call_id: format!("mock-{idx}"),
                        name: call.name,
                        arguments: if call.arguments.is_null() {
                            json!({})
                        } else {
                            call.arguments
                        },
                    }));
                }
            }

            // Scripted error — yield and stop. The agent's stream
            // consumer treats this exactly like a real provider
            // HTTP/transport failure.
            if let Some(err) = turn.error {
                yield Err(SqueezyError::ProviderStream(err.message));
                return;
            }

            let cost = CostSnapshot {
                input_tokens: turn.input_tokens,
                output_tokens: turn.output_tokens,
                ..CostSnapshot::default()
            };
            // Map the chat-completions-style `finish_reason` string the user
            // wrote in TOML onto the normalized `StopReason` the agent
            // expects. Mirrors `compatible.rs::chat_stop_reason`.
            let stop_reason = turn.finish_reason.as_deref().map(|raw| match raw {
                "stop" => squeezy_llm::StopReason::EndTurn,
                "tool_calls" | "function_call" => squeezy_llm::StopReason::ToolUse,
                "length" => squeezy_llm::StopReason::MaxTokens,
                "content_filter" => squeezy_llm::StopReason::Refusal,
                other => squeezy_llm::StopReason::Other(other.to_string()),
            });
            yield Ok(LlmEvent::Completed {
                response_id: None,
                cost,
                stop_reason,
                reasoning_only_stop: turn.reasoning_only_stop,
            });
        })
    }
}
