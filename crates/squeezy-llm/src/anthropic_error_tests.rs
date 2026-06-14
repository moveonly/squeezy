use super::*;

#[test]
fn parses_thinking_budget_400_with_actionable_hint() {
    let body = r#"{
        "type": "error",
        "error": {
            "type": "invalid_request_error",
            "message": "thinking.enabled.budget_tokens: Input should be greater than or equal to 1024"
        },
        "request_id": "req_011CbYmqWtAUtMpWMuCNYwTS"
    }"#;
    let parsed = parse(StatusCode::BAD_REQUEST, body).expect("envelope parses");

    assert!(
        !parsed.retryable,
        "400 invalid_request_error is non-transient"
    );
    assert_eq!(
        parsed.request_id.as_deref(),
        Some("req_011CbYmqWtAUtMpWMuCNYwTS"),
    );
    assert!(
        parsed
            .human
            .starts_with("Anthropic rejected request (invalid_request_error):"),
        "got: {}",
        parsed.human,
    );
    assert!(
        parsed.human.contains("budget_tokens"),
        "human prose must carry the offending field name: {}",
        parsed.human,
    );
    let hint = parsed.hint.as_deref().expect("hint for thinking budget");
    assert!(
        hint.contains("max_output_tokens"),
        "hint must name the squeezy knob, got: {hint}",
    );
}

#[test]
fn formats_thinking_budget_400_with_marker_and_hint_inline() {
    let body = r#"{
        "type": "error",
        "error": {
            "type": "invalid_request_error",
            "message": "thinking.enabled.budget_tokens: Input should be greater than or equal to 1024"
        },
        "request_id": "req_011CbYmqWtAUtMpWMuCNYwTS"
    }"#;
    let formatted = format_for_provider_error(StatusCode::BAD_REQUEST, body);

    assert!(
        formatted.starts_with(NON_RETRYABLE_MARKER),
        "non-retryable verdict must ride on the wire: {formatted}",
    );
    let (non_retryable, stripped) = strip_non_retryable_marker(&formatted);
    assert!(non_retryable);
    assert!(stripped.contains("Anthropic rejected request"));
    assert!(stripped.contains("max_output_tokens"));
    assert!(stripped.contains("request_id req_011CbYmqWtAUtMpWMuCNYwTS"));
    // The raw JSON must not bleed into the user-facing prose.
    assert!(
        !stripped.contains("{\""),
        "raw JSON braces leaked into prose: {stripped}",
    );
}

#[test]
fn generic_400_invalid_request_marks_non_retryable_without_hint() {
    let body = r#"{
        "type": "error",
        "error": {
            "type": "invalid_request_error",
            "message": "Unexpected role 'user' after assistant turn"
        }
    }"#;
    let parsed = parse(StatusCode::BAD_REQUEST, body).expect("envelope parses");
    assert!(!parsed.retryable);
    assert!(parsed.hint.is_none(), "no hint for unrecognised 400 prose");
    assert!(parsed.request_id.is_none());
    assert!(
        parsed.human.contains("Unexpected role"),
        "must surface the provider message verbatim: {}",
        parsed.human,
    );

    let formatted = format_for_provider_error(StatusCode::BAD_REQUEST, body);
    assert!(formatted.starts_with(NON_RETRYABLE_MARKER));
    assert!(!formatted.contains("request_id"));
}

#[test]
fn rate_limit_429_stays_retryable() {
    let body = r#"{
        "type": "error",
        "error": {
            "type": "rate_limit_error",
            "message": "Number of request tokens has exceeded your per-minute rate limit"
        }
    }"#;
    let parsed = parse(StatusCode::TOO_MANY_REQUESTS, body).expect("envelope parses");
    assert!(parsed.retryable, "429 must remain retryable");
    let formatted = format_for_provider_error(StatusCode::TOO_MANY_REQUESTS, body);
    assert!(
        !formatted.starts_with(NON_RETRYABLE_MARKER),
        "retryable errors must not carry the non-retryable marker: {formatted}",
    );
}

#[test]
fn server_overloaded_500_stays_retryable() {
    let body = r#"{
        "type": "error",
        "error": {
            "type": "overloaded_error",
            "message": "Overloaded"
        }
    }"#;
    let parsed = parse(StatusCode::INTERNAL_SERVER_ERROR, body).expect("envelope parses");
    assert!(parsed.retryable);
}

#[test]
fn authentication_error_is_non_retryable_with_credential_hint() {
    let body = r#"{
        "type": "error",
        "error": {
            "type": "authentication_error",
            "message": "invalid x-api-key"
        }
    }"#;
    let parsed = parse(StatusCode::UNAUTHORIZED, body).expect("envelope parses");
    assert!(!parsed.retryable);
    let hint = parsed.hint.as_deref().expect("auth hint");
    assert!(hint.to_ascii_lowercase().contains("api"));
}

#[test]
fn malformed_body_returns_none_and_passthrough_format() {
    let body = "not json at all";
    assert!(parse(StatusCode::BAD_REQUEST, body).is_none());
    let formatted = format_for_provider_error(StatusCode::BAD_REQUEST, body);
    assert!(
        formatted.contains("not json at all"),
        "passthrough must preserve the original body: {formatted}",
    );
    assert!(
        !formatted.starts_with(NON_RETRYABLE_MARKER),
        "unknown shapes default to retryable: {formatted}",
    );
}

#[test]
fn strip_marker_handles_clean_messages_unchanged() {
    let (non_retryable, stripped) = strip_non_retryable_marker("plain message");
    assert!(!non_retryable);
    assert_eq!(stripped, "plain message");
}

#[test]
fn status_is_retryable_only_for_5xx_and_429() {
    assert!(status_is_retryable(StatusCode::INTERNAL_SERVER_ERROR));
    assert!(status_is_retryable(StatusCode::SERVICE_UNAVAILABLE));
    assert!(status_is_retryable(StatusCode::TOO_MANY_REQUESTS));
    assert!(!status_is_retryable(StatusCode::BAD_REQUEST));
    assert!(!status_is_retryable(StatusCode::UNAUTHORIZED));
    assert!(!status_is_retryable(StatusCode::FORBIDDEN));
    assert!(!status_is_retryable(StatusCode::NOT_FOUND));
}
