use squeezy_core::SqueezyError;

/// Returns `true` if `body` looks like a hard-quota / billing error that the
/// upstream will not retire on the retry timeline - typically a "monthly
/// usage limit reached" message from Anthropic or an `insufficient_quota`
/// error from OpenAI returned with a `429` status. Sleeping and retrying
/// those just burns the remaining attempt budget; the agent should surface
/// the failure to the user immediately.
///
/// Matches in two passes:
///
/// 1. A short list of well-known substrings (`monthly_usage_limit`,
///    `Monthly usage limit reached`, `insufficient_quota`,
///    `billing_hard_limit_reached`, `quota_exceeded`). This covers raw
///    text bodies and JSON bodies alike without paying a parser tax.
/// 2. A JSON shape check for the two providers whose error envelopes are
///    documented and stable: Anthropic (`error.type == "permission_error"`)
///    and OpenAI (`error.code == "insufficient_quota" |
///    "billing_hard_limit_reached" | "quota_exceeded"`). The substring pass
///    already catches the literal codes; the JSON pass is the durable
///    contract - provider-specific error shapes won't silently slip past
///    a future copywriting tweak.
///
/// Non-UTF-8 bodies are treated as non-terminal so the existing transient
/// retry path keeps running rather than skipping retries on an unrelated
/// binary garble.
pub(crate) fn is_terminal_quota_error(body: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(body) else {
        return false;
    };
    for keyword in TERMINAL_QUOTA_KEYWORDS {
        if text.contains(keyword) {
            return true;
        }
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text)
        && has_terminal_provider_error_shape(&value)
    {
        return true;
    }
    false
}

const TERMINAL_QUOTA_KEYWORDS: &[&str] = &[
    "monthly_usage_limit",
    "Monthly usage limit reached",
    "insufficient_quota",
    "billing_hard_limit_reached",
    "quota_exceeded",
];

/// Recognizes the provider-specific terminal-error envelopes documented at
/// Anthropic and OpenAI. Each provider can extend this list as new
/// hard-quota codes appear without changing the substring fallback.
fn has_terminal_provider_error_shape(value: &serde_json::Value) -> bool {
    let Some(error) = value.get("error") else {
        return false;
    };
    if let Some(error_type) = error.get("type").and_then(|t| t.as_str())
        && error_type == "permission_error"
    {
        return true;
    }
    if let Some(error_code) = error.get("code").and_then(|c| c.as_str())
        && matches!(
            error_code,
            "insufficient_quota" | "billing_hard_limit_reached" | "quota_exceeded"
        )
    {
        return true;
    }
    false
}

/// Decides whether `with_stream_retry` should reconnect on `err`.
///
/// Provider stream/request errors are the only transport-level shapes
/// the harness knows how to replay, but Anthropic's error normaliser
/// (and any future provider that opts into the same contract) prefixes
/// terminal errors with [`crate::anthropic_error::NON_RETRYABLE_MARKER`]
/// - typically `invalid_request_error`, `authentication_error`, or
/// `permission_error` responses where retrying the identical request
/// just burns the rest of the attempt budget on a guaranteed failure.
///
/// Strip-and-check the marker before classifying so a marked
/// `ProviderRequest`/`ProviderStream` returns `false` and short-circuits
/// the reconnect loop straight to the caller.
///
/// Security note: keying on a string *prefix* is spoofable in theory -
/// an upstream that emitted a body literally starting with the marker
/// text could suppress our retries. The blast radius is bounded though:
/// the only outcome of a (mis)classification as non-retryable is "stop
/// reconnecting and surface the error to the caller", never an escalated
/// privilege or an extra request. A typed non-retryable field on
/// `SqueezyError` would remove the ambiguity entirely, but that refactor
/// is out of scope here; the prefix marker is set by our own normaliser
/// (`anthropic_error`) on a path the upstream body never controls.
pub(crate) fn is_retryable_stream_error(err: &SqueezyError) -> bool {
    let message = match err {
        SqueezyError::ProviderStream(msg) | SqueezyError::ProviderRequest(msg) => msg.as_str(),
        _ => return false,
    };
    !message.starts_with(crate::anthropic_error::NON_RETRYABLE_MARKER)
}
