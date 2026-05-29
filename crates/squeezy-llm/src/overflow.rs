//! Triple-path context-overflow classifier.
//!
//! Providers reach "the prompt is too long" through three disjoint
//! terminal shapes that the agent has historically had to recognize
//! ad hoc — each in a different code path, often only by retrying
//! the same overflowing call:
//!
//! 1. **Explicit error pattern** — Anthropic, OpenAI, Bedrock, and
//!    most aggregators return a 400-ish HTTP body whose message
//!    mentions overflow (e.g. Anthropic's `prompt is too long: …` or
//!    OpenAI's `context_length_exceeded`). The stream never starts.
//! 2. **Silent usage saturation** — the upstream completes cleanly,
//!    but its reported usage already fills the model's context
//!    window. The model effectively had no room left to answer;
//!    the visible output is empty or near-empty and the user sees a
//!    blank turn unless the agent intervenes.
//! 3. **Length finish with zero visible output** — the upstream
//!    finishes with `length`/`max_tokens` but never emitted a text
//!    delta or a tool call. The prompt consumed the whole budget;
//!    the output was clamped to zero.
//!
//! [`classify_terminal`] is the single decision point that maps
//! those three shapes onto one [`OverflowSignal`]. Providers call
//! it at stream finish (or at the explicit-error short-circuit) and
//! emit an [`LlmEvent::ContextOverflow`] when it returns `Some`.
//! The agent reacts to the signal once (compact, summarize, surface
//! to the user) instead of looping into the same overflow.
//!
//! [`LlmEvent::ContextOverflow`]: crate::LlmEvent::ContextOverflow
//!
//! # Rollout
//!
//! The classifier is provider-agnostic and intentionally lives in a
//! standalone module so adopting it on a new provider is mechanical:
//! track `saw_visible_output`, capture the last error string and the
//! native finish/stop reason, and call [`classify_terminal`] at the
//! same point the provider yields `LlmEvent::Completed` (or returns
//! its terminal error). Anthropic is wired today because it surfaces
//! the most explicit overflow message; OpenAI / Google / Bedrock /
//! xAI / Ollama / LMStudio / OpenAI-compatible adapters follow the
//! same pattern and are scheduled as a follow-up.

use serde::{Deserialize, Serialize};

/// Token-usage envelope the classifier consumes for the silent-usage
/// path. `used` is the count of tokens the provider reported as
/// already consumed for the turn (typically prompt + reasoning +
/// output); `max` is the model's effective context window. Providers
/// build this from their native usage payload plus
/// [`crate::ModelLimits::context_window_tokens`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub used: u64,
    pub max: u64,
}

/// Which of the three terminal shapes fired.
///
/// `ErrorPattern` carries the matched provider message verbatim so
/// the agent can surface it to the user without re-deriving it.
/// `SilentUsage` carries the `(used, max)` pair the classifier
/// matched so the recovery path can compute a compaction target
/// without re-querying the registry. `LengthStopZeroOutput` is a
/// pure marker — the agent already knows the finish reason was
/// `length`/`max_tokens` and the visible output was empty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum OverflowSignal {
    ErrorPattern(String),
    SilentUsage { used: u64, max: u64 },
    LengthStopZeroOutput,
}

/// Classify a terminal provider observation into an [`OverflowSignal`].
///
/// `provider` is the provider name (`"anthropic"`, `"openai"`, …).
/// It is currently unused inside the classifier — every path is
/// provider-agnostic — but reserved so per-provider phrasing
/// (Anthropic's verbatim `prompt is too long` vs OpenAI's
/// `context_length_exceeded` code) can be tightened later without a
/// caller-side change.
///
/// `finish` is the native stop/finish reason string (e.g. Anthropic
/// `"end_turn"` / `"max_tokens"`, OpenAI `"length"` / `"stop"`,
/// Google `"MAX_TOKENS"`, Bedrock `"max_tokens"`). Pass `None` when
/// the stream errored before yielding a terminal reason.
///
/// `last_error` is the most recent provider error message captured
/// during this turn. Pass `Some` when the HTTP request failed or
/// when the SSE stream surfaced an `error` event.
///
/// `usage` is the token-usage envelope built from the provider's
/// reported usage plus the model's context-window limit. Pass
/// `None` when the upstream did not surface usage.
///
/// `output_was_empty` is `true` iff no [`crate::LlmEvent::TextDelta`]
/// with non-empty text and no [`crate::LlmEvent::ToolCall`] reached
/// the consumer this turn. Caller tracks this against its own stream
/// state.
///
/// Returns `Some` when a path fires, `None` when the terminal looks
/// healthy. Paths are checked in the order documented at the
/// module level (error pattern first, then silent usage, then
/// length-stop-zero-output); the first match wins so the agent
/// receives a single signal per turn.
pub fn classify_terminal(
    provider: &str,
    finish: Option<&str>,
    last_error: Option<&str>,
    usage: Option<&Usage>,
    output_was_empty: bool,
) -> Option<OverflowSignal> {
    let _ = provider;

    if let Some(err) = last_error
        && error_indicates_overflow(err)
    {
        return Some(OverflowSignal::ErrorPattern(err.to_string()));
    }

    if let Some(usage) = usage
        && usage.max > 0
        && usage.used >= usage.max
    {
        return Some(OverflowSignal::SilentUsage {
            used: usage.used,
            max: usage.max,
        });
    }

    if output_was_empty
        && let Some(finish) = finish
        && is_length_finish(finish)
    {
        return Some(OverflowSignal::LengthStopZeroOutput);
    }

    None
}

/// Substring match against the canonical overflow phrasings used by
/// the major providers. Case-insensitive so we catch Anthropic's
/// lowercase `"prompt is too long"` alongside OpenAI's
/// `"context_length_exceeded"` error code regardless of how the
/// upstream envelope cases the text. Kept as a deliberate union
/// rather than per-provider tables so a new aggregator (OpenRouter,
/// LiteLLM, Vertex) usually classifies correctly without code.
fn error_indicates_overflow(err: &str) -> bool {
    let lower = err.to_ascii_lowercase();
    const NEEDLES: &[&str] = &[
        "prompt is too long",
        "context length",
        "context_length_exceeded",
        "maximum context length",
        "context window",
        "input is too long",
        "request too large",
        "too many tokens",
        "tokens exceed",
        "exceeds the model",
    ];
    NEEDLES.iter().any(|needle| lower.contains(needle))
}

/// Match the provider strings that mean "the model stopped because
/// it hit a length cap" — Anthropic / Bedrock `"max_tokens"`,
/// OpenAI Chat Completions `"length"`, OpenAI Responses
/// `"max_output_tokens"`, Google `"MAX_TOKENS"`. The classifier
/// promotes the bare length finish to `LengthStopZeroOutput` only
/// when `output_was_empty` is also `true`; a length finish with
/// real output is plain truncation, not overflow.
fn is_length_finish(finish: &str) -> bool {
    matches!(
        finish,
        "length" | "max_tokens" | "MAX_TOKENS" | "max_output_tokens"
    )
}

#[cfg(test)]
#[path = "overflow_tests.rs"]
mod tests;
