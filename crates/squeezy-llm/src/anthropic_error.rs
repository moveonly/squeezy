//! Anthropic provider-error normalizer.
//!
//! Anthropic surfaces request failures as a JSON envelope:
//!
//! ```text
//! {
//!   "type": "error",
//!   "error": { "type": "invalid_request_error", "message": "…" },
//!   "request_id": "req_…"
//! }
//! ```
//!
//! Dumping that verbatim into the TUI status line buries the actionable
//! prose (`error.message`) inside JSON jargon that wraps off a 120-column
//! terminal. This module parses the envelope and produces a short human
//! line plus a `retryable` verdict the TUI uses to decide whether to
//! suffix a retry hint.
//!
//! The classifier intentionally errs toward `retryable = true` for any
//! unrecognised shape so genuine 5xx / 429 paths keep their existing
//! retry guidance; the only path that flips to `retryable = false` is a
//! 400 with an `invalid_request_error` / `not_found_error` /
//! `authentication_error` / `permission_error` body, where retrying the
//! identical request fails identically.
//!
//! Hard-coded field hints live in [`next_step_hint`]; they cover the
//! 400s squeezy itself can trip into (most notably the
//! `thinking.enabled.budget_tokens` floor) and degrade to no hint for
//! everything else. The raw provider body still rides through
//! `trace.jsonl` for triage; this module only shapes the user-facing
//! prose.
//!
//! See `docs/internal/eval-findings/wave2-17-error-and-failure-messages.md`
//! Finding 3 for the motivating evidence.

use reqwest::StatusCode;
use serde_json::Value;

/// Sentinel prefix the TUI strips before rendering. Encodes the
/// "this error is not retryable" verdict on the wire so a single
/// `SqueezyError::ProviderRequest` string can carry both the human
/// prose and the retry classification without widening the error
/// enum. See [`crate::anthropic_error::format_for_provider_error`].
pub const NON_RETRYABLE_MARKER: &str = "[non-retryable] ";

/// Structured view of an Anthropic error envelope. Returned by
/// [`parse`]; callers usually consume [`format_for_provider_error`]
/// instead, which composes the human one-liner directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedAnthropicError {
    /// One-line human-readable summary, e.g.
    /// `"Anthropic rejected request (invalid_request_error): thinking.enabled.budget_tokens must be >= 1024"`.
    pub human: String,
    /// Optional next-step hint naming the squeezy config knob to adjust.
    /// `None` when the error class has no hard-coded mapping.
    pub hint: Option<String>,
    /// Anthropic's `request_id` if present; surfaced so a support
    /// contact can be filed against the call.
    pub request_id: Option<String>,
    /// `false` for 4xx invalid-request / auth / permission errors where
    /// retrying the identical request will fail identically. `true` for
    /// 5xx / 429 / unknown shapes that benefit from a retry prompt.
    pub retryable: bool,
}

/// Compose the wire-format error string used by
/// [`SqueezyError::ProviderRequest`]. Non-retryable errors are prefixed
/// with [`NON_RETRYABLE_MARKER`] so the TUI can suppress the
/// "retry or check provider/network status" suffix.
///
/// Falls back to `"{status}: {body}"` (the historical shape) when the
/// body does not look like an Anthropic JSON envelope, so unknown
/// providers / mocked responses behave identically to before this
/// normaliser landed.
pub fn format_for_provider_error(status: StatusCode, body: &str) -> String {
    let Some(parsed) = parse(status, body) else {
        return format!("{status}: {body}");
    };
    let mut out = String::new();
    if !parsed.retryable {
        out.push_str(NON_RETRYABLE_MARKER);
    }
    out.push_str(&parsed.human);
    if let Some(hint) = parsed.hint.as_deref() {
        out.push_str(". ");
        out.push_str(hint);
    }
    if let Some(req) = parsed.request_id.as_deref() {
        out.push_str(" (request_id ");
        out.push_str(req);
        out.push(')');
    }
    out
}

/// Try to parse an Anthropic error envelope. Returns `None` when the
/// body does not look like JSON of the expected shape, so the caller
/// can fall back to a passthrough format. Public so the TUI can
/// inspect the parsed shape directly in tests; production callers
/// usually route through [`format_for_provider_error`].
pub fn parse(status: StatusCode, body: &str) -> Option<NormalizedAnthropicError> {
    let value: Value = serde_json::from_str(body).ok()?;
    let error = value.get("error")?;
    let error_type = error
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("error")
        .to_string();
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("(no message)")
        .trim()
        .to_string();
    let request_id = value
        .get("request_id")
        .and_then(Value::as_str)
        .map(str::to_string);

    let retryable = is_retryable(status.as_u16(), &error_type);
    let hint = next_step_hint(&error_type, &message);
    let human = format!("Anthropic rejected request ({error_type}): {message}");
    Some(NormalizedAnthropicError {
        human,
        hint,
        request_id,
        retryable,
    })
}

/// Classify a parsed Anthropic error as retryable or not.
///
/// `invalid_request_error`, `not_found_error`, `authentication_error`,
/// and `permission_error` are non-transient by definition: retrying
/// the identical request fails identically. `rate_limit_error`,
/// `overloaded_error`, and `api_error` (Anthropic's generic 5xx
/// catch-all) are transient. Unknown classes default to retryable so
/// genuine transient shapes Anthropic introduces later keep their
/// retry guidance.
///
/// The HTTP status is the tiebreaker for non-Anthropic-shaped bodies
/// that still parse as JSON: any 5xx or 429 is retryable regardless
/// of the body's `error.type`.
fn is_retryable(status: u16, error_type: &str) -> bool {
    if status >= 500 || status == 429 {
        return true;
    }
    !matches!(
        error_type,
        "invalid_request_error"
            | "not_found_error"
            | "authentication_error"
            | "permission_error"
            | "model_context_window_exceeded"
    )
}

/// Map a known Anthropic error message onto a concrete next-step hint
/// that names the squeezy config knob to adjust. Returns `None` when
/// the message does not match a known shape, so the human prose is
/// surfaced on its own.
fn next_step_hint(error_type: &str, message: &str) -> Option<String> {
    if error_type != "invalid_request_error" {
        return match error_type {
            "authentication_error" => {
                Some("Re-check ANTHROPIC_API_KEY or run squeezy login.".to_string())
            }
            "permission_error" => Some(
                "The API key lacks access to this model; pick another model or upgrade the key."
                    .to_string(),
            ),
            "not_found_error" => {
                Some("Verify the model id in the scenario or settings.toml.".to_string())
            }
            _ => None,
        };
    }
    let lower = message.to_ascii_lowercase();
    if lower.contains("thinking.enabled.budget_tokens")
        || (lower.contains("budget_tokens") && lower.contains("greater than or equal to"))
    {
        return Some(
            "Raise max_output_tokens to at least 1025 or lower reasoning_effort.".to_string(),
        );
    }
    if lower.contains("prompt is too long") || lower.contains("context length") {
        return Some("Compact the conversation or switch to a larger-context model.".to_string());
    }
    if lower.contains("max_tokens") {
        return Some("Adjust max_output_tokens in your scenario or settings.".to_string());
    }
    None
}

/// Classify an HTTP status as retryable based on the status alone,
/// for provider error paths that carry no parseable error envelope.
/// Only 5xx and 429 are transient; every other status (the 4xx
/// invalid-request / auth / permission / not-found family) is
/// deterministic and fails identically on resend.
///
/// Mirrors the status tiebreaker in [`is_retryable`] for callers that
/// have an HTTP status but no Anthropic-shaped body to inspect.
pub fn status_is_retryable(status: StatusCode) -> bool {
    status.is_server_error() || status.as_u16() == 429
}

/// If `message` carries the [`NON_RETRYABLE_MARKER`] prefix, return
/// `(true, stripped_message)`; otherwise return `(false, message)`.
/// Lets the TUI render the human prose without the on-wire sentinel
/// and decide whether to append the retry hint.
pub fn strip_non_retryable_marker(message: &str) -> (bool, &str) {
    match message.strip_prefix(NON_RETRYABLE_MARKER) {
        Some(rest) => (true, rest),
        None => (false, message),
    }
}

#[cfg(test)]
#[path = "anthropic_error_tests.rs"]
mod tests;
