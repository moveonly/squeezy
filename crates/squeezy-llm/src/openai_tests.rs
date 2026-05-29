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
    };

    let body = OpenAiProvider::request_body(&request, "openai");

    assert_eq!(body["model"], "gpt-test");
    assert_eq!(body["instructions"], "be brief");
    assert_eq!(body["input"], "hello");
    assert_eq!(body["stream"], true);
    assert_eq!(body["store"], true);
    assert_eq!(body["max_output_tokens"], 32);
    assert_eq!(body["previous_response_id"], "resp_123");
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["name"], "grep");
    assert_eq!(body["tools"][0]["strict"], true);
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
    };

    let body = OpenAiProvider::request_body(&request, "openai");
    assert!(body.get("prompt_cache_key").is_none());
    assert!(body.get("prompt_cache_retention").is_none());
}

#[test]
fn request_body_emits_prompt_cache_retention_24h_for_long_retention() {
    // F11: `CacheRetention::Long` must surface on the OpenAI Responses
    // wire as the top-level `prompt_cache_retention: "24h"` field. Mirrors
    // pi's `getPromptCacheRetention`
    // (`others/pi/packages/ai/src/providers/openai-responses.ts:48-53`).
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
    // routing headers do not share that limit — they carry the full
    // session id so OpenAI's load balancer can pin repeat turns to the
    // backend that warmed the cached prefix.
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
    };

    let headers = OpenAiProvider::affinity_headers(&request);
    assert_eq!(headers.len(), 2);
    for (_, value) in &headers {
        assert_eq!(value, "squeezy::session-affinity");
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
    };

    let body = OpenAiProvider::request_body(&request, "openai");
    let input = body["input"].as_array().expect("input array (text+image)");
    assert_eq!(input.len(), 2);
    // First entry: plain user-text message.
    assert_eq!(input[0]["role"], "user");
    assert_eq!(input[0]["content"], "what is this?");
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
