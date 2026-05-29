//! Deterministic in-process faux provider for tests and the eval harness.
//!
//! Squeezy's test surface historically relied on ad-hoc mocks — the
//! `ScriptedProvider` in `squeezy-harness`, bespoke in-test `LlmProvider`
//! impls scattered across crates, and string-matched fake-response
//! producers in `mock_events`. [`FauxProvider`] consolidates the pattern
//! into a first-class provider implementing [`LlmProvider`] that accepts
//! a scripted sequence of responses and replays them as
//! [`LlmEvent`] streams.
//!
//! Pi's `packages/ai/src/providers/faux.ts` is the architectural
//! reference. Squeezy keeps the conceptual surface — pre-scripted
//! responses, optional error injection, multi-turn replay — but ports
//! the API to Rust idioms: programmatic builders for in-test wiring and
//! a TOML script format for eval fixtures.
//!
//! # Configuration wiring
//!
//! Eval and integration tests can target the faux provider through the
//! standard configuration entry point. A `[providers.faux]` section in
//! `settings.toml` (or any tier-level override) is loaded by
//! `provider_from_config` into a [`FauxProvider`] populated from the
//! script path:
//!
//! ```toml
//! [model]
//! provider = "faux"
//!
//! [providers.faux]
//! script = "tests/fixtures/faux-session.toml"
//! ```
//!
//! Programmatic construction is also supported and is the typical path
//! for in-process integration tests:
//!
//! ```ignore
//! use squeezy_llm::{FauxProvider, FauxTurn};
//!
//! let provider = FauxProvider::new("faux-anthropic");
//! provider.push_step(FauxTurn::text("hello").into_step());
//! ```
//!
//! # Script format
//!
//! The TOML script file is a list of `[[turn]]` entries. Each turn
//! becomes one `stream_response` call:
//!
//! ```toml
//! [[turn]]
//! text = "Hello, world!"
//! response_id = "resp_1"
//! input_tokens = 12
//! output_tokens = 3
//!
//! [[turn]]
//! thinking = "let me think about this"
//! text = "yes"
//!
//! [[turn]]
//! error = "rate limit exceeded"
//! ```

use std::{
    collections::VecDeque,
    path::Path,
    sync::{Arc, Mutex},
};

use futures_util::stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use squeezy_core::{CostSnapshot, FauxConfig, ReasoningKind, Result, SqueezyError};
use tokio_util::sync::CancellationToken;

use crate::{LlmEvent, LlmProvider, LlmRequest, LlmStream, LlmToolCall, StopReason};

/// Provider name returned by [`LlmProvider::name`] when the caller does
/// not override it via [`FauxConfig::name`] or
/// [`FauxProvider::with_name`].
pub const DEFAULT_FAUX_NAME: &str = "faux";

/// Default error message surfaced when a scripted response is exhausted.
/// Mirrors pi's "No more faux responses queued" wording so cross-stack
/// debuggers see a familiar signal.
const NO_MORE_RESPONSES_MESSAGE: &str = "faux: no scripted response remaining for this request";

/// One scripted response from the faux provider. Each entry in
/// [`FauxProvider`]'s queue is one [`FauxStep`]; one step is consumed
/// per `stream_response` call.
///
/// `Events` replays the listed [`LlmEvent`] values as-is, in order, so
/// tests can hand-craft the exact stream shape (including reasoning,
/// tool calls, server-model echoes, etc.) without going through the
/// convenience [`FauxTurn`] helper.
///
/// `Error` short-circuits the stream with a [`SqueezyError::ProviderRequest`]
/// — the typical pattern for "the upstream rejected this request"
/// regressions where the agent must see a structured error.
#[derive(Debug, Clone)]
pub enum FauxStep {
    /// Emit the listed events verbatim, then close the stream.
    Events(Vec<LlmEvent>),
    /// Yield a single `Err(ProviderRequest(message))` and close.
    Error(String),
}

impl FauxStep {
    /// Convenience for [`FauxTurn::text`] (a single TextDelta + Completed).
    pub fn text(text: impl Into<String>) -> Self {
        FauxTurn::text(text).into_step()
    }

    /// Convenience for an error response.
    pub fn error(message: impl Into<String>) -> Self {
        FauxStep::Error(message.into())
    }

    /// Convert the step into the `Result<LlmEvent>` sequence that the
    /// provider streams. `Error` collapses to a single `Err`; `Events`
    /// wraps each event in `Ok`.
    fn into_results(self) -> Vec<Result<LlmEvent>> {
        match self {
            FauxStep::Events(events) => events.into_iter().map(Ok).collect(),
            FauxStep::Error(message) => {
                vec![Err(SqueezyError::ProviderRequest(message))]
            }
        }
    }
}

/// One turn in a faux script — a high-level description that compiles
/// down into an `LlmEvent` stream. Hand-authorable in TOML and ergonomic
/// for in-test builders.
///
/// Field semantics:
/// - `error`: when set, the turn is treated as an error injection; all
///   other fields are ignored and the turn produces a [`FauxStep::Error`].
/// - `thinking`: emitted as a single [`LlmEvent::ReasoningDelta`] with
///   kind [`ReasoningKind::Text`] before any text or tool-call deltas.
/// - `text`: emitted as a single [`LlmEvent::TextDelta`].
/// - `tool_calls`: each entry emitted as one [`LlmEvent::ToolCall`].
/// - Usage fields (`input_tokens`, `output_tokens`, `cached_input_tokens`)
///   ride on the terminal [`LlmEvent::Completed`].
/// - `response_id` and `stop_reason` ride on the terminal `Completed`
///   event.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FauxTurn {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub thinking: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<FauxToolCall>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub response_id: Option<String>,
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub cached_input_tokens: Option<u64>,
    #[serde(default)]
    pub stop_reason: Option<StopReason>,
}

/// Tool call entry on a [`FauxTurn`]. Arguments are an arbitrary JSON
/// blob so scripts can encode the full provider-side shape without a
/// per-tool schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FauxToolCall {
    pub call_id: String,
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

impl FauxTurn {
    /// Construct a single-text turn that emits one TextDelta + Completed.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
            ..Default::default()
        }
    }

    /// Construct an error turn. Equivalent to `FauxStep::Error` once
    /// compiled.
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            error: Some(message.into()),
            ..Default::default()
        }
    }

    /// Compile the high-level turn description into a [`FauxStep`].
    pub fn into_step(self) -> FauxStep {
        if let Some(message) = self.error {
            return FauxStep::Error(message);
        }
        let mut events = Vec::with_capacity(4 + self.tool_calls.len());
        events.push(LlmEvent::Started);
        if let Some(thinking) = self.thinking
            && !thinking.is_empty()
        {
            events.push(LlmEvent::ReasoningDelta {
                text: thinking,
                kind: ReasoningKind::Text,
            });
        }
        if let Some(text) = self.text
            && !text.is_empty()
        {
            events.push(LlmEvent::TextDelta(text));
        }
        for call in self.tool_calls {
            events.push(LlmEvent::ToolCall(LlmToolCall {
                call_id: call.call_id,
                name: call.name,
                arguments: call.arguments,
            }));
        }
        let has_usage = self.input_tokens.is_some()
            || self.output_tokens.is_some()
            || self.cached_input_tokens.is_some();
        let cost = if has_usage {
            CostSnapshot {
                input_tokens: self.input_tokens,
                output_tokens: self.output_tokens,
                cached_input_tokens: self.cached_input_tokens,
                ..CostSnapshot::default()
            }
        } else {
            CostSnapshot::default()
        };
        events.push(LlmEvent::Completed {
            response_id: self.response_id,
            cost,
            stop_reason: self.stop_reason,
            reasoning_only_stop: false,
        });
        FauxStep::Events(events)
    }
}

/// Top-level TOML script shape. `turns` (or its aliases) is a list of
/// [`FauxTurn`] entries replayed in order, one per `stream_response`
/// call.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FauxScript {
    /// Accept `[[turn]]`, `[[turns]]`, or `[[responses]]` for the most
    /// natural reading regardless of which convention the script author
    /// reaches for.
    #[serde(default, alias = "turn", alias = "responses", alias = "response")]
    pub turns: Vec<FauxTurn>,
}

impl FauxScript {
    /// Read and parse a script file.
    pub fn from_path(path: &Path) -> Result<Self> {
        let body = std::fs::read_to_string(path).map_err(|err| {
            SqueezyError::Config(format!(
                "faux: failed to read script {}: {err}",
                path.display()
            ))
        })?;
        Self::from_toml_str(&body, &path.display().to_string())
    }

    /// Parse a TOML script body. `label` is used in error messages so
    /// callers can surface where the script came from (file path, inline
    /// fixture name, etc.).
    pub fn from_toml_str(text: &str, label: &str) -> Result<Self> {
        toml::from_str::<FauxScript>(text)
            .map_err(|err| SqueezyError::Config(format!("faux: failed to parse {label}: {err}")))
    }

    /// Compile the parsed script into the queue of [`FauxStep`]s that
    /// the provider will replay.
    pub fn into_steps(self) -> Vec<FauxStep> {
        self.turns.into_iter().map(FauxTurn::into_step).collect()
    }
}

/// Leak a String into a `&'static str`. Used to honour
/// [`LlmProvider::name`]'s `'static` lifetime when the user supplies a
/// runtime-provided provider name (e.g. `mock-anthropic`). The leak
/// happens once per provider construction; faux providers are
/// short-lived test fixtures so the cost is negligible.
fn leak_name(value: String) -> &'static str {
    Box::leak(value.into_boxed_str())
}

/// In-process scripted [`LlmProvider`]. The provider maintains a FIFO
/// queue of [`FauxStep`]s; each `stream_response` call pops the next
/// step and replays its events.
///
/// Thread safety: cloning the provider produces another handle pointing
/// at the same underlying queue, so a builder thread can push responses
/// while the provider streams them out. The internal `Mutex` is held only
/// briefly (pop + clone) so contention is effectively zero.
#[derive(Debug, Clone)]
pub struct FauxProvider {
    name: &'static str,
    pending: Arc<Mutex<VecDeque<FauxStep>>>,
}

impl FauxProvider {
    /// Construct an empty provider with the given name.
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            pending: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    /// Construct a provider with the [`DEFAULT_FAUX_NAME`] name.
    pub fn with_default_name() -> Self {
        Self::new(DEFAULT_FAUX_NAME)
    }

    /// Construct a provider whose name is derived from a runtime
    /// (non-static) string. The string is leaked once; suitable for
    /// test fixtures, not for long-running production code.
    pub fn with_name(name: impl Into<String>) -> Self {
        Self::new(leak_name(name.into()))
    }

    /// Construct a provider preloaded with `steps`.
    pub fn with_steps<I>(name: &'static str, steps: I) -> Self
    where
        I: IntoIterator<Item = FauxStep>,
    {
        let provider = Self::new(name);
        provider.extend(steps);
        provider
    }

    /// Construct a provider from a [`FauxConfig`]. When
    /// [`FauxConfig::script`] is set the script file is read and its
    /// turns are queued; otherwise the provider starts empty and the
    /// caller is expected to populate it programmatically.
    pub fn from_config(config: &FauxConfig) -> Result<Self> {
        let name: &'static str = match config.name.as_deref() {
            Some(raw) if !raw.is_empty() => leak_name(raw.to_string()),
            _ => DEFAULT_FAUX_NAME,
        };
        let provider = Self::new(name);
        if let Some(path) = config.script.as_deref().filter(|raw| !raw.is_empty()) {
            let script = FauxScript::from_path(Path::new(path))?;
            provider.extend(script.into_steps());
        }
        Ok(provider)
    }

    /// Push a single scripted step onto the back of the queue.
    pub fn push_step(&self, step: FauxStep) {
        self.pending
            .lock()
            .expect("faux queue poisoned")
            .push_back(step);
    }

    /// Push a plain-text response (convenience over
    /// `push_step(FauxTurn::text(text).into_step())`).
    pub fn push_text(&self, text: impl Into<String>) {
        self.push_step(FauxStep::text(text));
    }

    /// Push an error step that produces a `ProviderRequest` error when
    /// streamed.
    pub fn push_error(&self, message: impl Into<String>) {
        self.push_step(FauxStep::error(message));
    }

    /// Push a [`FauxTurn`] (compiled into events on the way in).
    pub fn push_turn(&self, turn: FauxTurn) {
        self.push_step(turn.into_step());
    }

    /// Append many steps in one acquire.
    pub fn extend<I>(&self, steps: I)
    where
        I: IntoIterator<Item = FauxStep>,
    {
        let mut pending = self.pending.lock().expect("faux queue poisoned");
        for step in steps {
            pending.push_back(step);
        }
    }

    /// Number of scripted steps still waiting to be consumed.
    pub fn pending(&self) -> usize {
        self.pending.lock().expect("faux queue poisoned").len()
    }

    /// Provider name as configured. Useful for assertions in tests
    /// that hand the provider around as `Arc<dyn LlmProvider>`.
    pub fn provider_name(&self) -> &'static str {
        self.name
    }
}

impl LlmProvider for FauxProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn stream_response(&self, _request: LlmRequest, _cancel: CancellationToken) -> LlmStream {
        let step = self
            .pending
            .lock()
            .expect("faux queue poisoned")
            .pop_front()
            .unwrap_or_else(|| FauxStep::Error(NO_MORE_RESPONSES_MESSAGE.to_string()));
        Box::pin(stream::iter(step.into_results()))
    }
}

#[cfg(test)]
#[path = "faux_tests.rs"]
mod tests;
