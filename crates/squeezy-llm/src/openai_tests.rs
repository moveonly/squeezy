use super::*;
use crate::{CacheSpec, LlmInputItem, LlmOutputSchema, LlmToolCall, LlmToolSpec};
use serde_json::json;
use std::sync::Arc;

#[test]
fn request_body_uses_responses_streaming_shape() {
    let request = LlmRequest {
        model: "gpt-test".to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        max_output_tokens: Some(32),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: Some("resp_123".to_string()),
        cache_key: None,
        cache: CacheSpec::default(),
        tools: Arc::from(vec![
            LlmToolSpec {
                name: "grep".to_string(),
                description: "search files".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {"pattern": {"type": "string"}},
                    "required": ["pattern"]
                }),
                strict: true,
            }
            .into(),
        ]),
        store: true,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };

    let body = OpenAiProvider::request_body(&request, "openai");

    assert_eq!(body["model"], "gpt-test");
    assert_eq!(body["instructions"], "be brief");
    // User text serializes as the typed array form (audit MEDIUM:
    // `UserText` content shape) so multi-item turns stay uniform.
    assert_eq!(body["input"][0]["role"], "user");
    assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
    assert_eq!(body["input"][0]["content"][0]["text"], "hello");
    assert_eq!(body["stream"], true);
    assert_eq!(body["store"], true);
    assert_eq!(body["max_output_tokens"], 32);
    assert_eq!(body["previous_response_id"], "resp_123");
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["name"], "grep");
    assert_eq!(body["tools"][0]["strict"], true);
}

#[test]
fn request_body_forwards_tool_choice_when_tools_empty() {
    // M-03: a Responses replay continuation re-attaches tools via
    // `previous_response_id`; the caller still needs to set
    // `tool_choice: "none"` on the follow-up turn even with an empty
    // local tools list.
    let request = LlmRequest {
        model: "gpt-test".to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: Some("resp_replay".to_string()),
        cache_key: None,
        cache: CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: Some("none".to_string()),
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };

    let body = OpenAiProvider::request_body(&request, "openai");
    assert_eq!(body["tool_choice"], "none");
    assert!(
        body.get("tools").is_none(),
        "empty tools list must not produce a `tools` field",
    );
}

#[test]
fn request_body_omits_empty_instructions() {
    // M-02: an empty `instructions` field would overwrite the stored
    // default on a `previous_response_id` chain. Skip when empty so the
    // server-stored default survives.
    let request = LlmRequest {
        model: "gpt-test".to_string().into(),
        instructions: String::new().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: Some("resp_123".to_string()),
        cache_key: None,
        cache: CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };

    let body = OpenAiProvider::request_body(&request, "openai");
    assert!(
        body.get("instructions").is_none(),
        "empty instructions must not serialize",
    );
}

#[test]
fn parser_treats_non_string_event_type_as_unhandled() {
    // LOW: a malformed proxy could ship `{"type": null}`; the parser
    // must not error, must not match a real branch, just log + skip.
    let mut acc = ReasoningAccumulator::default();
    let event = parse_openai_event(r#"{"type":null,"delta":"hello"}"#, &mut acc).expect("ok");
    assert!(event.is_none());
}

#[test]
fn parser_treats_missing_event_type_as_unhandled() {
    let mut acc = ReasoningAccumulator::default();
    let event = parse_openai_event(r#"{"delta":"hello"}"#, &mut acc).expect("ok");
    assert!(event.is_none());
}

#[test]
fn parser_extracts_text_delta() {
    let mut acc = ReasoningAccumulator::default();
    let event = parse_openai_event(
        r#"{"type":"response.output_text.delta","delta":"hello"}"#,
        &mut acc,
    )
    .expect("valid event");

    assert_eq!(event, Some(LlmEvent::TextDelta("hello".to_string())));
}

#[test]
fn request_body_serializes_tool_outputs_as_input_items() {
    let request = LlmRequest {
        model: "gpt-test".to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![
            LlmInputItem::FunctionCall {
                call_id: "call_1".to_string(),
                name: "grep".to_string(),
                arguments: json!({"pattern": "needle"}),
            },
            LlmInputItem::FunctionCallOutput {
                call_id: "call_1".to_string(),
                output: "{\"status\":\"success\"}".to_string(),
                content_parts: None,
                is_error: false,
            },
        ]),
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        cache: CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };

    let body = OpenAiProvider::request_body(&request, "openai");

    assert_eq!(body["input"][0]["type"], "function_call");
    assert_eq!(body["input"][0]["arguments"], r#"{"pattern":"needle"}"#);
    assert_eq!(body["input"][1]["type"], "function_call_output");
}

#[test]
fn request_body_preserves_function_tool_order() {
    let request = LlmRequest {
        model: "gpt-test".to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        cache: CacheSpec::default(),
        tools: Arc::from(vec![
            LlmToolSpec {
                name: "write_file".to_string(),
                description: "write".to_string(),
                parameters: json!({"type": "object"}),
                strict: true,
            }
            .into(),
            LlmToolSpec {
                name: "grep".to_string(),
                description: "search".to_string(),
                parameters: json!({"type": "object"}),
                strict: true,
            }
            .into(),
        ]),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };

    let body = OpenAiProvider::request_body(&request, "openai");

    assert!(body.get("max_output_tokens").is_none());
    assert_eq!(body["tools"][0]["name"], "write_file");
    assert_eq!(body["tools"][1]["name"], "grep");
}

#[test]
fn request_body_includes_reasoning_and_text_verbosity_when_set() {
    let request = LlmRequest {
        model: "gpt-test".to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        max_output_tokens: None,
        response_verbosity: Some(squeezy_core::ResponseVerbosity::Verbose),
        reasoning_effort: Some(squeezy_core::ReasoningEffort::High),
        previous_response_id: None,
        cache_key: None,
        cache: CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };

    let body = OpenAiProvider::request_body(&request, "openai");

    assert_eq!(body["text"]["verbosity"], "high");
    assert_eq!(body["reasoning"]["effort"], "high");
    assert_eq!(body["reasoning"]["summary"], "auto");
    // store=false → request encrypted_content so replay works statelessly.
    assert_eq!(body["include"][0], "reasoning.encrypted_content");
}

#[test]
fn request_body_maps_squeezy_verbosity_to_openai_values() {
    for (squeezy, openai) in [
        (squeezy_core::ResponseVerbosity::Concise, "low"),
        (squeezy_core::ResponseVerbosity::Normal, "medium"),
        (squeezy_core::ResponseVerbosity::Verbose, "high"),
    ] {
        let request = LlmRequest {
            model: "gpt-test".to_string().into(),
            instructions: "be brief".to_string().into(),
            input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
            max_output_tokens: None,
            response_verbosity: Some(squeezy),
            reasoning_effort: None,
            previous_response_id: None,
            cache_key: None,
            cache: CacheSpec::default(),
            tools: Arc::from(Vec::new()),
            store: false,
            tool_choice: None,
            output_schema: None,
            parallel_tool_calls: None,
            beta_headers: std::sync::Arc::from(Vec::new()),
            ..LlmRequest::default()
        };

        let body = OpenAiProvider::request_body(&request, "openai");

        assert_eq!(body["text"]["verbosity"], openai);
    }
}

#[test]
fn request_body_emits_prompt_cache_key_when_set() {
    let request = LlmRequest {
        model: "gpt-test".to_string().into(),
        instructions: "hi".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: Some("squeezy::session-1".to_string()),
        cache: CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };

    let body = OpenAiProvider::request_body(&request, "openai");
    assert_eq!(body["prompt_cache_key"], "squeezy::session-1");
}

#[test]
fn request_body_omits_prompt_cache_key_when_unset() {
    let request = LlmRequest {
        model: "gpt-test".to_string().into(),
        instructions: "hi".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        cache: CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };

    let body = OpenAiProvider::request_body(&request, "openai");
    assert!(body.get("prompt_cache_key").is_none());
    assert!(body.get("prompt_cache_retention").is_none());
}

#[test]
fn request_body_emits_prompt_cache_retention_24h_for_long_retention() {
    // F11: `CacheRetention::Long` must surface on the OpenAI Responses
    // wire as the top-level `prompt_cache_retention: "24h"` field.
    let request = LlmRequest {
        model: "gpt-test".to_string().into(),
        instructions: "hi".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        cache: crate::CacheSpec {
            key: Some("squeezy::session-long".to_string()),
            retention: crate::CacheRetention::Long,
        },
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };

    let body = OpenAiProvider::request_body(&request, "openai");
    assert_eq!(body["prompt_cache_key"], "squeezy::session-long");
    assert_eq!(
        body["prompt_cache_retention"], "24h",
        "Long retention must extend OpenAI's cached-prefix lifetime to 24h"
    );
}

#[test]
fn request_body_clamps_prompt_cache_key_to_sixty_four_codepoints() {
    // F11 reproducer: a 100-codepoint session id (e.g. a namespaced UUID
    // chain) must clamp to 64 codepoints in the request body. OpenAI
    // silently drops the field server-side when it exceeds the limit,
    // turning every cached turn into a cache miss with zero visible
    // error.
    let long_key: String = "a".repeat(100);
    let request = LlmRequest {
        model: "gpt-test".to_string().into(),
        instructions: "hi".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: Some(long_key.clone()),
        cache: CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };

    let body = OpenAiProvider::request_body(&request, "openai");
    let emitted = body["prompt_cache_key"]
        .as_str()
        .expect("prompt_cache_key must be emitted");
    assert_eq!(emitted.chars().count(), 64);
    assert_eq!(emitted, "a".repeat(64));
}

#[test]
fn request_body_preserves_multibyte_prompt_cache_key_under_codepoint_limit() {
    // Multibyte regression guard: 64 two-byte codepoints is 128 bytes —
    // well over a naive byte clamp — but only 64 codepoints, so the key
    // must round-trip unchanged.
    let key: String = "α".repeat(64);
    assert_eq!(key.len(), 128, "two-byte UTF-8 sanity check");
    let request = LlmRequest {
        model: "gpt-test".to_string().into(),
        instructions: "hi".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: Some(key.clone()),
        cache: CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };

    let body = OpenAiProvider::request_body(&request, "openai");
    assert_eq!(body["prompt_cache_key"], key);
}

#[test]
fn request_body_clamps_multibyte_prompt_cache_key_at_codepoint_boundary() {
    // 65 two-byte codepoints must clamp to 64 codepoints (128 bytes),
    // never mid-character.
    let key: String = "α".repeat(65);
    let request = LlmRequest {
        model: "gpt-test".to_string().into(),
        instructions: "hi".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: Some(key),
        cache: CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };

    let body = OpenAiProvider::request_body(&request, "openai");
    let emitted = body["prompt_cache_key"]
        .as_str()
        .expect("prompt_cache_key must be emitted");
    assert_eq!(emitted.chars().count(), 64);
    assert_eq!(emitted, "α".repeat(64));
}

#[test]
fn affinity_headers_emitted_with_cache_key_carry_full_unclamped_value() {
    // The body field is clamped to 64 codepoints (above), but the
    // routing headers carry up to 256 bytes (LOW: defensive cap to keep
    // hyper's 8KB header line cap unreachable) so OpenAI's load
    // balancer can still pin repeat turns to the warmed backend.
    let long_key: String = "a".repeat(100);
    let request = LlmRequest {
        model: "gpt-test".to_string().into(),
        instructions: "hi".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: Some(long_key.clone()),
        cache: CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };

    let headers = OpenAiProvider::affinity_headers(&request);
    assert_eq!(headers.len(), 2);
    let by_name: std::collections::BTreeMap<&str, &str> = headers
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();
    assert_eq!(by_name.get("session_id"), Some(&long_key.as_str()));
    assert_eq!(by_name.get("x-client-request-id"), Some(&long_key.as_str()));
}

#[test]
fn affinity_headers_present_when_cache_spec_carries_key() {
    // Headers must surface regardless of which slot (legacy `cache_key`
    // vs the universal `cache.key`) carried the affinity hint.
    let request = LlmRequest {
        model: "gpt-test".to_string().into(),
        instructions: "hi".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        cache: crate::CacheSpec {
            key: Some("squeezy::session-affinity".to_string()),
            retention: crate::CacheRetention::Long,
        },
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };

    let headers = OpenAiProvider::affinity_headers(&request);
    assert_eq!(headers.len(), 2);
    for (_, value) in &headers {
        assert_eq!(value, "squeezy::session-affinity");
    }
}

#[test]
fn affinity_header_values_are_clamped_to_two_fifty_six_bytes() {
    // LOW: adversarial inputs (multi-MB cache keys propagated from
    // user-controlled session ids) would otherwise panic the request
    // builder. Clamp keeps the byte length well under hyper's 8KB
    // header line cap.
    let huge = "x".repeat(4096);
    let request = LlmRequest {
        model: "gpt-test".to_string().into(),
        instructions: String::new().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        cache: crate::CacheSpec {
            key: Some(huge.clone()),
            retention: crate::CacheRetention::Short,
        },
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };

    for (name, value) in OpenAiProvider::affinity_headers(&request) {
        assert!(
            value.len() <= 256,
            "{name} value must be ≤ 256 bytes, got {} bytes",
            value.len(),
        );
    }
}

#[test]
fn affinity_header_clamps_at_utf8_codepoint_boundary() {
    // The 256-byte clamp MUST land on a codepoint boundary so the
    // resulting String stays valid UTF-8 even for multibyte inputs.
    // Build a key that crosses the 256-byte mark mid-codepoint.
    let multibyte = "🚀".repeat(80); // 4 bytes each = 320 bytes
    let request = LlmRequest {
        model: "gpt-test".to_string().into(),
        instructions: String::new().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        cache: crate::CacheSpec {
            key: Some(multibyte.clone()),
            retention: crate::CacheRetention::Short,
        },
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };

    let headers = OpenAiProvider::affinity_headers(&request);
    for (name, value) in headers {
        assert!(
            value.len() <= 256,
            "{name} value must be ≤ 256 bytes, got {} bytes",
            value.len(),
        );
        // Round-trip through `String` (already valid by construction);
        // ensure split landed on a 🚀-codepoint boundary (4 bytes each).
        assert_eq!(
            value.len() % 4,
            0,
            "{name} value must split on a codepoint boundary (multiples of 4 bytes for 🚀)",
        );
    }
}

#[test]
fn affinity_headers_absent_when_no_cache_key() {
    // No cache key → no affinity headers. Without this gate the OpenAI
    // load balancer would see empty header values on every uncached
    // request, which is meaningless overhead.
    let request = LlmRequest {
        model: "gpt-test".to_string().into(),
        instructions: "hi".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        cache: CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };

    assert!(OpenAiProvider::affinity_headers(&request).is_empty());
}

#[test]
fn request_body_omits_prompt_cache_retention_for_short_retention() {
    // Regression guard for the legacy-field migration path: callers that
    // still set the deprecated `cache_key` slot get `Short` retention via
    // `effective_cache_spec()`, which must leave `prompt_cache_retention`
    // off the wire so the default short window applies.
    let request = LlmRequest {
        model: "gpt-test".to_string().into(),
        instructions: "hi".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: Some("squeezy::session-1".to_string()),
        cache: CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };

    let body = OpenAiProvider::request_body(&request, "openai");
    assert_eq!(body["prompt_cache_key"], "squeezy::session-1");
    assert!(body.get("prompt_cache_retention").is_none());
}

#[test]
fn parser_extracts_function_call_from_output_item_done() {
    let mut acc = ReasoningAccumulator::default();
    let event = parse_openai_event(
        r#"{
          "type": "response.output_item.done",
          "item": {
            "type": "function_call",
            "call_id": "call_123",
            "name": "grep",
            "arguments": "{\"pattern\":\"needle\"}"
          }
        }"#,
        &mut acc,
    )
    .expect("valid event");

    assert_eq!(
        event,
        Some(LlmEvent::ToolCall(LlmToolCall {
            call_id: "call_123".to_string(),
            name: "grep".to_string(),
            arguments: json!({"pattern": "needle"}),
        }))
    );
}

#[test]
fn parser_preserves_malformed_function_arguments_as_tool_error_payload() {
    let mut acc = ReasoningAccumulator::default();
    let event = parse_openai_event(
        r#"{
          "type": "response.output_item.done",
          "item": {
            "type": "function_call",
            "call_id": "call_123",
            "name": "definition_search",
            "arguments": "{\"query\":\"getFoo"
          }
        }"#,
        &mut acc,
    )
    .expect("malformed arguments stay recoverable");

    let Some(LlmEvent::ToolCall(call)) = event else {
        panic!("expected tool call");
    };
    assert_eq!(call.call_id, "call_123");
    assert_eq!(call.name, "definition_search");
    assert_eq!(
        call.arguments[crate::INVALID_TOOL_ARGUMENTS_KEY],
        json!(true)
    );
    assert!(
        call.arguments[crate::INVALID_TOOL_ARGUMENTS_ERROR_KEY]
            .as_str()
            .unwrap()
            .contains("EOF"),
        "{}",
        call.arguments
    );
}

#[test]
fn parser_extracts_completed_response_id_and_usage() {
    let mut acc = ReasoningAccumulator::default();
    let event = parse_openai_event(
        r#"{
          "type":"response.completed",
          "response":{
            "id":"resp_123",
            "usage":{
              "input_tokens":10,
              "output_tokens":4,
              "output_tokens_details":{"reasoning_tokens":2},
              "input_tokens_details":{"cached_tokens":3}
            }
          }
        }"#,
        &mut acc,
    )
    .expect("valid event");

    assert_eq!(
        event,
        Some(LlmEvent::Completed {
            response_id: Some("resp_123".to_string()),
            cost: CostSnapshot {
                input_tokens: Some(10),
                output_tokens: Some(4),
                reasoning_output_tokens: Some(2),
                cached_input_tokens: Some(3),
                cache_write_input_tokens: None,
                estimated_usd_micros: None,
            },
            // `response.completed` without `incomplete_details` is a
            // successful end-of-turn signal in the Responses API; the
            // provider normalizes this to `EndTurn` so the agent's loop
            // sees a uniform stop reason across providers.
            stop_reason: Some(crate::StopReason::EndTurn),
            reasoning_only_stop: false,
        })
    );
}

#[test]
fn parser_surfaces_error_events() {
    let mut acc = ReasoningAccumulator::default();
    let err = parse_openai_event(
        r#"{"type":"error","error":{"message":"bad request"}}"#,
        &mut acc,
    )
    .expect_err("stream error");

    assert!(err.to_string().contains("bad request"));
}

#[test]
fn parser_classifies_context_length_exceeded_and_queues_overflow_event() {
    // H-06: `response.failed` with `error.code = context_length_exceeded`
    // MUST queue a `ContextOverflow` LlmEvent on the accumulator (the
    // outer loop drains it before propagating the error) and surface a
    // prefixed error string so the agent's overflow recovery sees the
    // canonical signal instead of a bare provider error.
    let mut acc = ReasoningAccumulator::default();
    let err = parse_openai_event(
        r#"{
          "type":"response.failed",
          "response":{
            "id":"resp_overflow",
            "error":{"code":"context_length_exceeded","message":"This model's maximum context length is 200000 tokens."}
          }
        }"#,
        &mut acc,
    )
    .expect_err("response.failed surfaces ProviderStream");
    let err_str = err.to_string();
    assert!(
        err_str.contains("context_length_exceeded"),
        "expected prefixed code in error, got {err_str}",
    );

    let queued: Vec<LlmEvent> = acc.drain_pre_yield().collect();
    assert_eq!(
        queued.len(),
        1,
        "exactly one ContextOverflow event must be queued"
    );
    match &queued[0] {
        LlmEvent::ContextOverflow { provider, signal } => {
            assert_eq!(provider, "openai");
            assert!(matches!(signal, crate::OverflowSignal::ErrorPattern(_)));
        }
        other => panic!("expected ContextOverflow, got {other:?}"),
    }
}

#[test]
fn parser_classifies_rate_limit_exceeded_with_prefix() {
    let mut acc = ReasoningAccumulator::default();
    let err = parse_openai_event(
        r#"{
          "type":"response.failed",
          "response":{
            "error":{"code":"rate_limit_exceeded","message":"Rate limit reached. Please try again in 3s."}
          }
        }"#,
        &mut acc,
    )
    .expect_err("rate-limit surfaces error");
    let err_str = err.to_string();
    assert!(
        err_str.contains("rate_limit_exceeded"),
        "expected rate_limit_exceeded prefix, got {err_str}",
    );
    assert!(err_str.contains("3s"));
}

#[test]
fn parser_classifies_azure_content_filter_error_and_queues_refusal_event() {
    // C-14: Azure prompt-time content_filter envelope. The parser must
    // queue a `Refusal` event carrying the category/severity summary so
    // the agent can show the user *why* the refusal happened.
    let mut acc = ReasoningAccumulator::default();
    let err = parse_openai_event(
        r#"{
          "type":"response.failed",
          "response":{
            "error":{
              "code":"content_filter",
              "message":"The response was filtered due to content policy.",
              "innererror":{
                "code":"ResponsibleAIPolicyViolation",
                "content_filter_result":{
                  "hate":{"filtered":true,"severity":"high"},
                  "sexual":{"filtered":false,"severity":"safe"},
                  "violence":{"filtered":true,"severity":"medium"}
                }
              }
            }
          }
        }"#,
        &mut acc,
    )
    .expect_err("content_filter surfaces error");
    let err_str = err.to_string();
    assert!(err_str.contains("content_filter"));
    assert!(err_str.contains("hate:high"));
    assert!(err_str.contains("violence:medium"));

    let queued: Vec<LlmEvent> = acc.drain_pre_yield().collect();
    assert_eq!(queued.len(), 1);
    match &queued[0] {
        LlmEvent::Refusal { content } => {
            assert!(content.contains("hate:high"));
            assert!(content.contains("violence:medium"));
        }
        other => panic!("expected Refusal, got {other:?}"),
    }
    assert!(acc.refusal_latched);
}

#[test]
fn parser_emits_refusal_for_mid_stream_response_incomplete_content_filter() {
    // C-14 mid-stream: when the *output* filter blocks a response after
    // streaming starts, Azure ships `response.incomplete` with
    // `incomplete_details.content_filter_result` carrying per-category
    // severity. Surface the categories via a `Refusal` event before the
    // `Completed` event.
    let mut acc = ReasoningAccumulator::default();
    let event = parse_openai_event(
        r#"{
          "type":"response.incomplete",
          "response":{
            "id":"resp_blocked",
            "incomplete_details":{
              "reason":"content_filter",
              "content_filter_result":{
                "hate":{"filtered":true,"severity":"high"}
              }
            }
          }
        }"#,
        &mut acc,
    )
    .expect("event ok")
    .expect("must emit Completed");
    match event {
        LlmEvent::Completed { stop_reason, .. } => assert_eq!(
            stop_reason,
            Some(crate::StopReason::Refusal),
            "content_filter reason MUST normalize to Refusal",
        ),
        other => panic!("expected Completed, got {other:?}"),
    }
    let queued: Vec<LlmEvent> = acc.drain_pre_yield().collect();
    assert_eq!(queued.len(), 1);
    match &queued[0] {
        LlmEvent::Refusal { content } => {
            assert!(content.contains("hate:high"));
        }
        other => panic!("expected Refusal, got {other:?}"),
    }
}

#[test]
fn parser_classifies_previous_response_not_found_with_marker_prefix() {
    // M-05: stale `previous_response_id` 404 MUST surface with a
    // `previous_response_not_found:` marker prefix so the agent layer
    // can detect it without a SqueezyError schema add.
    let mut acc = ReasoningAccumulator::default();
    let err = parse_openai_event(
        r#"{
          "type":"response.failed",
          "response":{
            "error":{"code":"previous_response_not_found","message":"Response resp_old not found."}
          }
        }"#,
        &mut acc,
    )
    .expect_err("stale id surfaces error");
    assert!(
        err.to_string().contains("previous_response_not_found"),
        "expected marker prefix, got {err}",
    );
}

#[test]
fn parser_attaches_error_param_when_present() {
    let mut acc = ReasoningAccumulator::default();
    let err = parse_openai_event(
        r#"{
          "type":"response.failed",
          "response":{
            "error":{"code":"invalid_request","message":"bad","param":"input[0]"}
          }
        }"#,
        &mut acc,
    )
    .expect_err("error");
    assert!(err.to_string().contains("(param: input[0])"));
}

#[test]
fn parser_reconciles_output_text_done_with_no_divergence() {
    // H-08: the common case — every `output_text.delta` was observed,
    // the `output_text.done` text matches the cumulative buffer, no
    // corrective event emitted.
    let mut acc = ReasoningAccumulator::default();
    parse_openai_event(
        r#"{"type":"response.output_text.delta","delta":"hello "}"#,
        &mut acc,
    )
    .expect("delta 1");
    parse_openai_event(
        r#"{"type":"response.output_text.delta","delta":"world"}"#,
        &mut acc,
    )
    .expect("delta 2");

    let done = parse_openai_event(
        r#"{"type":"response.output_text.done","text":"hello world"}"#,
        &mut acc,
    )
    .expect("done");
    assert!(done.is_none(), "matched done emits nothing");
}

#[test]
fn parser_emits_corrective_text_delta_when_output_text_done_diverges() {
    // H-08: when a delta was dropped mid-stream, the `output_text.done`
    // event carries the authoritative full text. Emit a corrective
    // `TextDelta` for the missing suffix so the persisted transcript
    // matches what the model actually said.
    let mut acc = ReasoningAccumulator::default();
    parse_openai_event(
        r#"{"type":"response.output_text.delta","delta":"hello "}"#,
        &mut acc,
    )
    .expect("delta 1");

    let done = parse_openai_event(
        r#"{"type":"response.output_text.done","text":"hello world"}"#,
        &mut acc,
    )
    .expect("done")
    .expect("must emit corrective delta");
    assert_eq!(done, LlmEvent::TextDelta("world".to_string()));
}

#[test]
fn parser_emits_tool_call_delta_for_function_call_arguments_streaming() {
    // H-07: `response.function_call_arguments.delta` MUST surface as
    // `LlmEvent::ToolCallDelta` so the UI shows progress before the
    // full `output_item.done` lands. The function name is captured from
    // the preceding `response.output_item.added` event.
    let mut acc = ReasoningAccumulator::default();

    let added = parse_openai_event(
        r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"item_42","call_id":"call_42","name":"apply_patch"}}"#,
        &mut acc,
    )
    .expect("valid added event");
    assert!(added.is_none(), "added event is internal-only");

    let first_chunk = parse_openai_event(
        r#"{"type":"response.function_call_arguments.delta","item_id":"item_42","delta":"{\"patch\":\""}"#,
        &mut acc,
    )
    .expect("valid delta")
    .expect("delta event must emit");
    assert_eq!(
        first_chunk,
        LlmEvent::ToolCallDelta {
            call_id: "item_42".to_string(),
            name: "apply_patch".to_string(),
            arguments_chunk: "{\"patch\":\"".to_string(),
        },
    );

    let second_chunk = parse_openai_event(
        r#"{"type":"response.function_call_arguments.delta","item_id":"item_42","delta":"diff..."}"#,
        &mut acc,
    )
    .expect("valid second delta")
    .expect("delta event must emit");
    assert_eq!(
        second_chunk,
        LlmEvent::ToolCallDelta {
            call_id: "item_42".to_string(),
            name: "apply_patch".to_string(),
            arguments_chunk: "diff...".to_string(),
        },
    );
}

#[test]
fn parser_tool_call_delta_falls_back_to_empty_name_when_added_event_missed() {
    // Defensive — if reconnect-without-skip drops the
    // `response.output_item.added` event the deltas still surface, just
    // with an empty function-name string. Better than dropping the
    // chunk on the floor.
    let mut acc = ReasoningAccumulator::default();
    let event = parse_openai_event(
        r#"{"type":"response.function_call_arguments.delta","item_id":"item_orphan","delta":"...]}"}"#,
        &mut acc,
    )
    .expect("valid event")
    .expect("event must emit");
    assert_eq!(
        event,
        LlmEvent::ToolCallDelta {
            call_id: "item_orphan".to_string(),
            name: String::new(),
            arguments_chunk: "...]}".to_string(),
        },
    );
}

#[test]
fn parser_emits_refusal_event_and_latches_refusal_stop_reason() {
    // C-02: `response.refusal.delta` events MUST surface as visible
    // `LlmEvent::Refusal` chunks, and the terminal `response.completed`
    // (which arrives without `incomplete_details` because the refusal
    // IS the completion) MUST normalize to `StopReason::Refusal`.
    let mut acc = ReasoningAccumulator::default();

    let first = parse_openai_event(
        r#"{"type":"response.refusal.delta","delta":"I'm sorry, I can't help with that."}"#,
        &mut acc,
    )
    .expect("valid refusal delta")
    .expect("refusal delta must emit");
    assert_eq!(
        first,
        LlmEvent::Refusal {
            content: "I'm sorry, I can't help with that.".to_string(),
        },
    );

    let done = parse_openai_event(
        r#"{"type":"response.refusal.done","refusal":"I'm sorry, I can't help with that."}"#,
        &mut acc,
    )
    .expect("valid refusal done");
    assert!(done.is_none(), "refusal.done is internal-only");

    let completed = parse_openai_event(
        r#"{"type":"response.completed","response":{"id":"resp_ref","usage":{"input_tokens":4,"output_tokens":12}}}"#,
        &mut acc,
    )
    .expect("valid completed")
    .expect("completion must emit");
    match completed {
        LlmEvent::Completed {
            stop_reason,
            response_id,
            ..
        } => {
            assert_eq!(stop_reason, Some(crate::StopReason::Refusal));
            assert_eq!(response_id.as_deref(), Some("resp_ref"));
        }
        other => panic!("expected Completed event, got {other:?}"),
    }
}

#[test]
fn parser_surfaces_incomplete_events() {
    let mut acc = ReasoningAccumulator::default();
    let event = parse_openai_event(
        r#"{
          "type":"response.incomplete",
          "response":{
            "incomplete_details":{"reason":"max_output_tokens"}
          }
        }"#,
        &mut acc,
    )
    .expect("incomplete response normalises to Completed with StopReason::MaxTokens")
    .expect("incomplete event must emit");

    match event {
        LlmEvent::Completed { stop_reason, .. } => assert_eq!(
            stop_reason,
            Some(crate::StopReason::MaxTokens),
            "max_output_tokens must surface as StopReason::MaxTokens",
        ),
        other => panic!("expected Completed event for incomplete response, got {other:?}"),
    }
}

#[test]
fn parser_extracts_reasoning_summary_delta_and_done_with_encrypted_blob() {
    let mut acc = ReasoningAccumulator::default();
    let summary_delta = parse_openai_event(
        r#"{"type":"response.reasoning_summary_text.delta","delta":"weighing options"}"#,
        &mut acc,
    )
    .expect("valid summary delta");
    assert_eq!(
        summary_delta,
        Some(LlmEvent::ReasoningDelta {
            text: "weighing options".to_string(),
            kind: crate::ReasoningKind::Summary,
        })
    );

    let done = parse_openai_event(
        r#"{
          "type":"response.output_item.done",
          "item":{
            "type":"reasoning",
            "id":"rs_abc",
            "summary":[{"type":"summary_text","text":"weighed options"}],
            "encrypted_content":"OPAQUE"
          }
        }"#,
        &mut acc,
    )
    .expect("valid done");
    assert_eq!(
        done,
        Some(LlmEvent::ReasoningDone(crate::ReasoningPayload::OpenAi {
            item_id: "rs_abc".to_string(),
            summary: vec!["weighed options".to_string()],
            encrypted_content: Some("OPAQUE".to_string()),
        }))
    );
}

#[test]
fn parser_backfills_empty_summary_from_streamed_deltas() {
    let mut acc = ReasoningAccumulator::default();
    // Stream two summary deltas (no `summary_text` will land in the item).
    parse_openai_event(
        r#"{"type":"response.reasoning_summary_text.delta","delta":"weighing "}"#,
        &mut acc,
    )
    .expect("valid summary delta");
    parse_openai_event(
        r#"{"type":"response.reasoning_summary_text.delta","delta":"options"}"#,
        &mut acc,
    )
    .expect("valid summary delta");

    // `output_item.done` arrives with `summary: []` (Responses sometimes ships
    // the close event without the aggregated summary parts).
    let done = parse_openai_event(
        r#"{
          "type":"response.output_item.done",
          "item":{
            "type":"reasoning",
            "id":"rs_abc",
            "summary":[],
            "encrypted_content":"OPAQUE"
          }
        }"#,
        &mut acc,
    )
    .expect("valid done");
    assert_eq!(
        done,
        Some(LlmEvent::ReasoningDone(crate::ReasoningPayload::OpenAi {
            item_id: "rs_abc".to_string(),
            summary: vec!["weighing options".to_string()],
            encrypted_content: Some("OPAQUE".to_string()),
        }))
    );

    // Accumulator must be drained so the next item starts clean.
    let next_done = parse_openai_event(
        r#"{
          "type":"response.output_item.done",
          "item":{
            "type":"reasoning",
            "id":"rs_def",
            "summary":[],
            "encrypted_content":null
          }
        }"#,
        &mut acc,
    )
    .expect("valid done");
    assert_eq!(
        next_done,
        Some(LlmEvent::ReasoningDone(crate::ReasoningPayload::OpenAi {
            item_id: "rs_def".to_string(),
            summary: Vec::new(),
            encrypted_content: None,
        }))
    );
}

#[test]
fn input_item_round_trips_openai_reasoning_blob() {
    let payload = crate::ReasoningPayload::OpenAi {
        item_id: "rs_abc".to_string(),
        summary: vec!["weighed options".to_string()],
        encrypted_content: Some("OPAQUE".to_string()),
    };
    let value = openai_input_item(&LlmInputItem::Reasoning(payload))
        .expect("OpenAI reasoning replays to OpenAI");
    assert_eq!(value["type"], "reasoning");
    assert_eq!(value["id"], "rs_abc");
    assert_eq!(value["encrypted_content"], "OPAQUE");
    assert_eq!(value["summary"][0]["text"], "weighed options");
}

#[test]
fn anthropic_reasoning_is_dropped_when_replaying_to_openai() {
    let payload = crate::ReasoningPayload::Anthropic {
        blocks: vec![crate::AnthropicThinkingBlock {
            kind: crate::AnthropicThinkingKind::Thinking,
            text: "thinking".to_string(),
            signature: Some("sig".to_string()),
            data: None,
        }],
    };
    assert!(openai_input_item(&LlmInputItem::Reasoning(payload)).is_none());
}

#[test]
fn request_body_emits_text_format_when_output_schema_set() {
    let schema = json!({
        "type": "object",
        "properties": {
            "answer": {"type": "string"},
            "confidence": {"type": "number"}
        },
        "required": ["answer", "confidence"],
        "additionalProperties": false
    });
    let request = LlmRequest {
        model: "gpt-test".to_string().into(),
        instructions: "respond in JSON".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        max_output_tokens: None,
        response_verbosity: Some(squeezy_core::ResponseVerbosity::Concise),
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        cache: CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        output_schema: Some(LlmOutputSchema {
            name: "answer_with_confidence".to_string(),
            schema: schema.clone(),
            strict: true,
        }),
        tool_choice: None,
        ..LlmRequest::default()
    };

    let body = OpenAiProvider::request_body(&request, "openai");

    assert_eq!(body["text"]["format"]["type"], "json_schema");
    assert_eq!(body["text"]["format"]["name"], "answer_with_confidence");
    assert_eq!(body["text"]["format"]["strict"], true);
    assert_eq!(body["text"]["format"]["schema"], schema);
    // verbosity must coexist with format inside the same `text` object.
    assert_eq!(body["text"]["verbosity"], "low");
}

#[test]
fn request_body_omits_text_format_when_output_schema_unset() {
    let request = LlmRequest {
        model: "gpt-test".to_string().into(),
        instructions: "hi".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        cache: CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        tool_choice: None,
        ..LlmRequest::default()
    };

    let body = OpenAiProvider::request_body(&request, "openai");
    assert!(body.get("text").is_none());
}

#[test]
fn request_body_emits_text_format_without_verbosity_when_only_schema_set() {
    let schema = json!({
        "type": "object",
        "properties": {"ok": {"type": "boolean"}},
        "required": ["ok"],
        "additionalProperties": false
    });
    let request = LlmRequest {
        model: "gpt-test".to_string().into(),
        instructions: "json".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        cache: CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false,
        output_schema: Some(LlmOutputSchema {
            name: "ok_box".to_string(),
            schema,
            strict: false,
        }),
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        tool_choice: None,
        ..LlmRequest::default()
    };

    let body = OpenAiProvider::request_body(&request, "openai");
    assert_eq!(body["text"]["format"]["type"], "json_schema");
    assert_eq!(body["text"]["format"]["strict"], false);
    assert!(body["text"].get("verbosity").is_none());
}

#[test]
fn request_body_emits_parallel_tool_calls_false_when_disabled() {
    let request = LlmRequest {
        model: "gpt-test".to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        cache: CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false,
        output_schema: None,
        parallel_tool_calls: Some(false),
        tool_choice: None,
        beta_headers: Arc::from(Vec::new()),
        ..LlmRequest::default()
    };

    let body = OpenAiProvider::request_body(&request, "openai");
    assert_eq!(body["parallel_tool_calls"], false);
}

#[test]
fn request_body_omits_parallel_tool_calls_when_unset_or_default_true() {
    for value in [None, Some(true)] {
        let request = LlmRequest {
            model: "gpt-test".to_string().into(),
            instructions: "be brief".to_string().into(),
            input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
            max_output_tokens: None,
            response_verbosity: None,
            reasoning_effort: None,
            previous_response_id: None,
            cache_key: None,
            cache: CacheSpec::default(),
            tools: Arc::from(Vec::new()),
            store: false,
            output_schema: None,
            parallel_tool_calls: value,
            tool_choice: None,
            beta_headers: Arc::from(Vec::new()),
            ..LlmRequest::default()
        };

        let body = OpenAiProvider::request_body(&request, "openai");
        assert!(
            body.get("parallel_tool_calls").is_none(),
            "parallel_tool_calls={:?} should not be emitted (OpenAI defaults to true)",
            value,
        );
    }
}

#[test]
fn request_body_encodes_image_as_input_image_data_url() {
    let bytes: Arc<[u8]> = Arc::from(vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    let request = LlmRequest {
        model: "gpt-test".to_string().into(),
        instructions: "describe images".to_string().into(),
        input: Arc::from(vec![
            LlmInputItem::UserText("what is this?".to_string()),
            LlmInputItem::Image {
                media_type: "image/png".to_string(),
                bytes: bytes.clone(),
            },
        ]),
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,

        cache: crate::CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };

    let body = OpenAiProvider::request_body(&request, "openai");
    let input = body["input"].as_array().expect("input array (text+image)");
    assert_eq!(input.len(), 2);
    // First entry: user-text message in the typed-array shape.
    assert_eq!(input[0]["role"], "user");
    assert_eq!(input[0]["content"][0]["type"], "input_text");
    assert_eq!(input[0]["content"][0]["text"], "what is this?");
    // Second entry: user message with one `input_image` content part
    // carrying a data URL.
    assert_eq!(input[1]["role"], "user");
    let image_block = &input[1]["content"][0];
    assert_eq!(image_block["type"], "input_image");
    assert_eq!(image_block["detail"], "auto");
    let url = image_block["image_url"].as_str().expect("image_url string");
    assert!(
        url.starts_with("data:image/png;base64,"),
        "Responses image must use a data URL, got `{url}`"
    );
    use base64::Engine as _;
    let encoded = url
        .strip_prefix("data:image/png;base64,")
        .expect("data URL prefix");
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .expect("valid base64");
    assert_eq!(decoded.as_slice(), bytes.as_ref());
}

#[test]
fn request_body_serializes_function_call_output_content_parts_as_array_form() {
    // M-06: when the caller attaches structured `content_parts`
    // (e.g. an image return from a browser tool), serialize the array
    // form of `function_call_output.output` so the model receives the
    // image directly instead of through a stringified base64 blob.
    let png: Arc<[u8]> = Arc::from(vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    let request = LlmRequest {
        model: "gpt-test".to_string().into(),
        instructions: "describe the screenshot".to_string().into(),
        input: Arc::from(vec![
            LlmInputItem::FunctionCall {
                call_id: "call_1".to_string(),
                name: "screenshot".to_string(),
                arguments: json!({}),
            },
            LlmInputItem::FunctionCallOutput {
                call_id: "call_1".to_string(),
                output: String::new(),
                content_parts: Some(vec![
                    crate::ToolResultPart::Text {
                        text: "see attached".to_string(),
                    },
                    crate::ToolResultPart::Image {
                        media_type: "image/png".to_string(),
                        bytes: png.clone(),
                    },
                ]),
                is_error: false,
            },
        ]),
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        cache: CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };

    let body = OpenAiProvider::request_body(&request, "openai");
    let output_item = &body["input"][1];
    assert_eq!(output_item["type"], "function_call_output");
    assert_eq!(output_item["call_id"], "call_1");
    let parts = output_item["output"].as_array().expect("array form output");
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0]["type"], "input_text");
    assert_eq!(parts[0]["text"], "see attached");
    assert_eq!(parts[1]["type"], "input_image");
    let url = parts[1]["image_url"].as_str().expect("data URL");
    assert!(url.starts_with("data:image/png;base64,"));
}

#[test]
fn request_body_falls_back_to_string_output_when_content_parts_unset() {
    // Existing string-form tool result remains byte-compatible.
    let request = LlmRequest {
        model: "gpt-test".to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![
            LlmInputItem::FunctionCall {
                call_id: "call_1".to_string(),
                name: "grep".to_string(),
                arguments: json!({"pattern": "needle"}),
            },
            LlmInputItem::FunctionCallOutput {
                call_id: "call_1".to_string(),
                output: "{\"status\":\"success\"}".to_string(),
                content_parts: None,
                is_error: false,
            },
        ]),
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        cache: CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };

    let body = OpenAiProvider::request_body(&request, "openai");
    let output_item = &body["input"][1];
    assert_eq!(output_item["type"], "function_call_output");
    assert_eq!(output_item["output"], "{\"status\":\"success\"}");
}

#[test]
fn request_body_defaults_store_true_for_azure_provider() {
    // H-37: Azure Responses requires `store: true` for the multi-turn
    // `previous_response_id` flow (the prior response must persist on
    // the server). The OpenAI / xAI / Codex paths still honor the
    // caller's verbatim `request.store` value.
    let request = LlmRequest {
        model: "gpt-test".to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: Some("resp_prior".to_string()),
        cache_key: None,
        cache: CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false, // caller default — Azure must still force true
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };

    let azure_body = OpenAiProvider::request_body(&request, "azure_openai");
    assert_eq!(azure_body["store"], true, "Azure must default store=true");

    let openai_body = OpenAiProvider::request_body(&request, "openai");
    assert_eq!(
        openai_body["store"], false,
        "OpenAI must honor caller's request.store verbatim"
    );
}

#[test]
fn build_responses_url_appends_preview_api_version_for_azure() {
    // C-13: caller-supplied `?api-version=preview` MUST land verbatim on
    // the URL. The DEFAULT_AZURE_OPENAI_API_VERSION constant in
    // squeezy-core is `"v1"` (wrong for Responses) — squeezy-core is
    // outside Phase 4B scope; this test verifies the URL-build path
    // correctly carries whatever the caller chose.
    let url = super::build_responses_url(
        "https://resource.openai.azure.com/openai/v1",
        Some("preview"),
        false,
    );
    assert_eq!(
        url,
        "https://resource.openai.azure.com/openai/v1/responses?api-version=preview"
    );
}

#[test]
fn build_responses_url_percent_encodes_api_version_typos() {
    // AZ-M4 / M-56: typos like `"preview "` (trailing space) MUST be
    // percent-encoded so the URL remains valid instead of breaking the
    // HTTP request builder.
    let url = super::build_responses_url(
        "https://resource.openai.azure.com/openai/v1",
        Some("preview "),
        false,
    );
    assert!(
        url.ends_with("?api-version=preview%20"),
        "expected trailing-space percent-encoded, got `{url}`",
    );
}

#[test]
fn build_responses_url_appends_path_before_existing_query_string() {
    // AZ-M4: when the user's `base_url` already carries a query string,
    // `/responses` must be inserted into the PATH (before the `?`), and
    // `api-version` joined onto the existing query with `&`. A naive
    // concatenation would bury `/responses` inside the query value and hit
    // the wrong endpoint.
    let url = super::build_responses_url(
        "https://resource.openai.azure.com/openai/v1?subscription-key=abc",
        Some("preview"),
        false,
    );
    assert_eq!(
        url,
        "https://resource.openai.azure.com/openai/v1/responses?subscription-key=abc&api-version=preview",
        "/responses must land in the path and api-version must join the existing query with &"
    );
}

#[test]
fn build_responses_url_omits_query_when_api_version_is_none() {
    // Standard OpenAI / xAI: no api-version, plain `/responses`.
    let url = super::build_responses_url("https://api.openai.com/v1", None, false);
    assert_eq!(url, "https://api.openai.com/v1/responses");
}

#[test]
fn is_classic_azure_deployment_url_detects_old_url_shape() {
    // H-36: detect the classic `/openai/deployments/{deployment}` URL
    // shape that older Azure Government / Mooncake resources still use.
    let config = squeezy_core::AzureOpenAiConfig {
        api_key_env: "AZURE_TEST_KEY_ENV_DOES_NOT_NEED_TO_EXIST".to_string(),
        api_key: Some("test-key".to_string()),
        base_url: "https://gov.openai.azure.us/openai/deployments/my-deploy".to_string(),
        api_version: "2024-10-21".to_string(),
        deployment_name_map: std::collections::BTreeMap::new(),
        extra_headers: std::collections::BTreeMap::new(),
        use_entra_id: false,
        entra_bearer_token: None,
        transport: squeezy_core::ProviderTransportConfig::default(),
    };
    let provider = OpenAiProvider::from_azure_config(&config).expect("provider build");
    assert!(provider.is_classic_azure_deployment_url());
}

#[test]
fn is_classic_azure_deployment_url_false_for_v1_url() {
    let config = squeezy_core::AzureOpenAiConfig {
        api_key_env: "AZURE_TEST_KEY_ENV_DOES_NOT_NEED_TO_EXIST".to_string(),
        api_key: Some("test-key".to_string()),
        base_url: "https://resource.openai.azure.com/openai/v1".to_string(),
        api_version: "preview".to_string(),
        deployment_name_map: std::collections::BTreeMap::new(),
        extra_headers: std::collections::BTreeMap::new(),
        use_entra_id: false,
        entra_bearer_token: None,
        transport: squeezy_core::ProviderTransportConfig::default(),
    };
    let provider = OpenAiProvider::from_azure_config(&config).expect("provider build");
    assert!(!provider.is_classic_azure_deployment_url());
}

#[test]
fn azure_entra_config_uses_bearer_token_without_api_key() {
    let config = squeezy_core::AzureOpenAiConfig {
        api_key_env: "AZURE_TEST_KEY_ENV_DOES_NOT_NEED_TO_EXIST".to_string(),
        api_key: None,
        base_url: "https://resource.openai.azure.com/openai/v1".to_string(),
        api_version: "preview".to_string(),
        deployment_name_map: std::collections::BTreeMap::new(),
        extra_headers: std::collections::BTreeMap::new(),
        use_entra_id: true,
        entra_bearer_token: Some("entra-token".to_string()),
        transport: squeezy_core::ProviderTransportConfig::default(),
    };

    let provider = OpenAiProvider::from_azure_config(&config).expect("provider build");

    assert_eq!(provider.auth_mode, OpenAiAuthMode::Bearer);
}

#[test]
fn azure_header_auth_does_not_require_api_key() {
    let config = squeezy_core::AzureOpenAiConfig {
        api_key_env: "AZURE_TEST_KEY_ENV_DOES_NOT_NEED_TO_EXIST".to_string(),
        api_key: None,
        base_url: "https://resource.openai.azure.com/openai/v1".to_string(),
        api_version: "preview".to_string(),
        deployment_name_map: std::collections::BTreeMap::new(),
        extra_headers: std::collections::BTreeMap::from([(
            "Ocp-Apim-Subscription-Key".to_string(),
            "apim-key".to_string(),
        )]),
        use_entra_id: false,
        entra_bearer_token: None,
        transport: squeezy_core::ProviderTransportConfig::default(),
    };

    let provider = OpenAiProvider::from_azure_config(&config).expect("provider build");

    assert_eq!(provider.auth_mode, OpenAiAuthMode::HeadersOnly);
    assert_eq!(
        provider.extra_headers.get("Ocp-Apim-Subscription-Key"),
        Some(&"apim-key".to_string()),
    );
}

#[test]
fn azure_without_key_or_auth_headers_still_reports_missing_key() {
    let config = squeezy_core::AzureOpenAiConfig {
        api_key_env: "AZURE_TEST_KEY_ENV_DOES_NOT_NEED_TO_EXIST".to_string(),
        api_key: None,
        base_url: "https://resource.openai.azure.com/openai/v1".to_string(),
        api_version: "preview".to_string(),
        deployment_name_map: std::collections::BTreeMap::new(),
        extra_headers: std::collections::BTreeMap::new(),
        use_entra_id: false,
        entra_bearer_token: None,
        transport: squeezy_core::ProviderTransportConfig::default(),
    };

    let error = OpenAiProvider::from_azure_config(&config).expect_err("missing key must fail");

    assert!(
        format!("{error}").contains("AZURE_TEST_KEY_ENV_DOES_NOT_NEED_TO_EXIST"),
        "error should name missing key env: {error}",
    );
}

#[test]
fn azure_deployment_name_map_translates_mapped_model() {
    let config = squeezy_core::AzureOpenAiConfig {
        api_key_env: "AZURE_TEST_KEY_ENV_DOES_NOT_NEED_TO_EXIST".to_string(),
        api_key: Some("test-key".to_string()),
        base_url: "https://resource.openai.azure.com/openai/v1".to_string(),
        api_version: "preview".to_string(),
        deployment_name_map: std::collections::BTreeMap::from([
            ("gpt-4o".to_string(), "my-deployment-gpt-4o".to_string()),
            ("gpt-5".to_string(), "my-deployment-gpt-5".to_string()),
        ]),
        extra_headers: std::collections::BTreeMap::new(),
        use_entra_id: false,
        entra_bearer_token: None,
        transport: squeezy_core::ProviderTransportConfig::default(),
    };
    let provider = OpenAiProvider::from_azure_config(&config).expect("provider build");

    assert_eq!(
        provider.resolve_deployment_name("gpt-4o"),
        "my-deployment-gpt-4o",
        "mapped logical id must be substituted for the Azure deployment name",
    );
    assert_eq!(
        provider.resolve_deployment_name("gpt-5"),
        "my-deployment-gpt-5",
    );
}

#[test]
fn azure_deployment_name_map_passes_unmapped_model_through() {
    let config = squeezy_core::AzureOpenAiConfig {
        api_key_env: "AZURE_TEST_KEY_ENV_DOES_NOT_NEED_TO_EXIST".to_string(),
        api_key: Some("test-key".to_string()),
        base_url: "https://resource.openai.azure.com/openai/v1".to_string(),
        api_version: "preview".to_string(),
        deployment_name_map: std::collections::BTreeMap::from([(
            "gpt-4o".to_string(),
            "my-deployment-gpt-4o".to_string(),
        )]),
        extra_headers: std::collections::BTreeMap::new(),
        use_entra_id: false,
        entra_bearer_token: None,
        transport: squeezy_core::ProviderTransportConfig::default(),
    };
    let provider = OpenAiProvider::from_azure_config(&config).expect("provider build");

    assert_eq!(
        provider.resolve_deployment_name("gpt-4o-mini"),
        "gpt-4o-mini",
        "unmapped model ids must pass through verbatim so deployments without \
         an explicit mapping keep the historical contract",
    );

    let empty = squeezy_core::AzureOpenAiConfig {
        deployment_name_map: std::collections::BTreeMap::new(),
        ..config
    };
    let provider = OpenAiProvider::from_azure_config(&empty).expect("provider build");
    assert_eq!(
        provider.resolve_deployment_name("gpt-4o"),
        "gpt-4o",
        "an empty map must not rewrite any model id",
    );
}
