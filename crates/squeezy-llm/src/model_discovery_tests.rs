use super::*;
use serde_json::json;

#[test]
fn parses_openai_style_minimal_catalog() {
    let value = json!({
        "data": [
            {"id": "gpt-5.5", "object": "model"},
            {"id": "claude-opus-4-7", "object": "model"},
        ]
    });
    let parsed = parse_catalog(&value);
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].id, "gpt-5.5");
    assert!(parsed[0].context_length.is_none());
}

#[test]
fn parses_openrouter_style_with_pricing_and_context() {
    let value = json!({
        "data": [
            {
                "id": "anthropic/claude-opus-4-7",
                "context_length": 200000,
                "pricing": {"prompt": "0.000015", "completion": "0.000075"},
                "supported_parameters": ["temperature", "tools", "tool_choice"],
                "top_provider": {"max_completion_tokens": 64000}
            }
        ]
    });
    let parsed = parse_catalog(&value);
    assert_eq!(parsed.len(), 1);
    let entry = &parsed[0];
    assert_eq!(entry.id, "anthropic/claude-opus-4-7");
    assert_eq!(entry.context_length, Some(200_000));
    assert_eq!(entry.max_output_tokens, Some(64_000));
    assert_eq!(entry.supports_tools, Some(true));
    // 0.000015 USD/token = 15 USD/Mtok = 15_000_000 USD-micros/Mtok.
    assert_eq!(entry.pricing_input_usd_micros_per_mtok, Some(15_000_000));
    assert_eq!(entry.pricing_output_usd_micros_per_mtok, Some(75_000_000));
}

#[test]
fn parses_groq_style_context_window_field() {
    let value = json!({
        "data": [
            {
                "id": "llama-3.3-70b-versatile",
                "object": "model",
                "owned_by": "Meta",
                "active": true,
                "context_window": 131072,
                "max_completion_tokens": 32768,
            }
        ]
    });
    let parsed = parse_catalog(&value);
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].context_length, Some(131_072));
    assert_eq!(parsed[0].max_output_tokens, Some(32_768));
}

#[test]
fn skips_entries_without_id() {
    let value = json!({
        "data": [
            {"object": "model"},
            {"id": "ok-model"},
            {"id": 42},
        ]
    });
    let parsed = parse_catalog(&value);
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].id, "ok-model");
}

#[test]
fn missing_data_array_returns_empty() {
    assert!(parse_catalog(&json!({})).is_empty());
    assert!(parse_catalog(&json!({"models": []})).is_empty());
}

#[test]
fn catalog_freshness_respects_ttl() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let fresh = ModelCatalog {
        fetched_at: now,
        provider: "openrouter".to_string(),
        models: Vec::new(),
    };
    assert!(fresh.is_fresh());

    let stale = ModelCatalog {
        fetched_at: now.saturating_sub(CACHE_TTL_SECS + 1),
        provider: "openrouter".to_string(),
        models: Vec::new(),
    };
    assert!(!stale.is_fresh());
}

#[test]
fn parse_price_string_rejects_invalid_and_negative_values() {
    assert_eq!(parse_price_string("0.000005"), Some(5_000_000));
    assert!(parse_price_string("not-a-number").is_none());
    assert!(parse_price_string("-1").is_none());
}
