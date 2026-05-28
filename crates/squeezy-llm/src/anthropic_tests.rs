use super::*;
use crate::anthropic_betas::{anthropic_header_value, bedrock_extra_body_betas};
use crate::{LlmInputItem, LlmToolCall, LlmToolSpec};
use std::sync::Arc;

#[test]
fn request_body_uses_messages_streaming_shape() {
    let request = LlmRequest {
        model: "claude-test".to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        max_output_tokens: Some(32),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: Some("ignored".to_string()),
        cache_key: None,
        tools: Arc::from(vec![
            LlmToolSpec {
                name: "read_file".to_string(),
                description: "Read a file".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" }
                    },
                    "required": ["path"]
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

    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);

    assert_eq!(body["model"], "claude-test");
    assert_eq!(body["system"], "be brief");
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["messages"][0]["content"][0]["type"], "text");
    assert_eq!(body["messages"][0]["content"][0]["text"], "hello");
    assert_eq!(body["tools"][0]["name"], "read_file");
    assert_eq!(body["tools"][0]["input_schema"]["required"][0], "path");
    assert!(body["tools"][0].get("strict").is_none());
    assert_eq!(body["max_tokens"], 32);
    assert_eq!(body["stream"], true);
    assert!(body.get("previous_response_id").is_none());
    assert!(body.get("store").is_none());
}

#[test]
fn request_body_preserves_function_tool_order() {
    let request = LlmRequest {
        model: "claude-test".to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        tools: Arc::from(vec![
            LlmToolSpec {
                name: "write_file".to_string(),
                description: "write".to_string(),
                parameters: serde_json::json!({"type": "object"}),
                strict: true,
            }
            .into(),
            LlmToolSpec {
                name: "grep".to_string(),
                description: "search".to_string(),
                parameters: serde_json::json!({"type": "object"}),
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

    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);

    assert_eq!(body["tools"][0]["name"], "write_file");
    assert_eq!(body["tools"][1]["name"], "grep");
}

#[test]
fn request_body_uses_model_limit_when_output_cap_unset() {
    let request = LlmRequest {
        model: squeezy_core::DEFAULT_ANTHROPIC_MODEL.to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
    };

    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);

    assert_eq!(body["max_tokens"], 64_000);
}

#[test]
fn request_body_maps_tool_roundtrip_messages() {
    let request = LlmRequest {
        model: "claude-test".to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![
            LlmInputItem::UserText("read config".to_string()),
            LlmInputItem::FunctionCall {
                call_id: "toolu_1".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({ "path": "squeezy.toml" }),
            },
            LlmInputItem::FunctionCallOutput {
                call_id: "toolu_1".to_string(),
                output: "model = 'haiku'".to_string(),
            },
        ]),
        max_output_tokens: Some(32),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
    };

    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);

    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["messages"][0]["content"][0]["text"], "read config");
    assert_eq!(body["messages"][1]["role"], "assistant");
    assert_eq!(body["messages"][1]["content"][0]["type"], "tool_use");
    // Tool-call ids get canonicalized to `call_<N>` (1-indexed by
    // first occurrence) so a mid-session model switch can replay
    // them on any provider — the original `toolu_…` shape would
    // be rejected by OpenAI/Google, and a 450-char OpenAI id would
    // be rejected by Anthropic.
    assert_eq!(body["messages"][1]["content"][0]["id"], "call_1");
    assert_eq!(
        body["messages"][1]["content"][0]["input"]["path"],
        "squeezy.toml"
    );
    assert_eq!(body["messages"][2]["role"], "user");
    assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
    assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "call_1");
}

#[test]
fn request_body_adds_cache_control_markers_when_cache_key_and_capability_enable_caching() {
    // Use a real Anthropic model id so the registry capability lookup
    // reports prompt_caching=true; pair it with a cache_key so the
    // anthropic adapter inserts ephemeral cache markers.
    let request = LlmRequest {
        model: squeezy_core::DEFAULT_ANTHROPIC_MODEL.to_string().into(),
        instructions: "system prompt".to_string().into(),
        input: Arc::from(vec![
            LlmInputItem::UserText("first turn".to_string()),
            LlmInputItem::AssistantText("ack".to_string()),
            LlmInputItem::UserText("second turn".to_string()),
        ]),
        max_output_tokens: Some(32),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: Some("squeezy::session-1".to_string()),
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
    };

    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);

    // System tail carries an ephemeral cache_control marker.
    assert_eq!(body["system"][0]["type"], "text");
    assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");

    // Only the last user block (the second turn) gets the cache marker.
    let messages = body["messages"].as_array().expect("messages array");
    let last_user = messages
        .iter()
        .rev()
        .find(|msg| msg["role"] == "user")
        .expect("user message");
    let last_block = last_user["content"]
        .as_array()
        .expect("content")
        .last()
        .expect("last block");
    assert_eq!(last_block["cache_control"]["type"], "ephemeral");

    // Older user turn is not marked.
    let first_user = messages
        .iter()
        .find(|msg| msg["role"] == "user")
        .expect("first user message");
    let first_block = &first_user["content"][0];
    assert!(first_block.get("cache_control").is_none());
}

#[test]
fn request_body_marks_last_tool_with_cache_control_when_caching_enabled() {
    let request = LlmRequest {
        model: squeezy_core::DEFAULT_ANTHROPIC_MODEL.to_string().into(),
        instructions: "system prompt".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
        max_output_tokens: Some(32),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: Some("squeezy::session-1".to_string()),
        tools: Arc::from(vec![
            LlmToolSpec {
                name: "tool_a".to_string(),
                description: "first".to_string(),
                parameters: serde_json::json!({"type": "object"}),
                strict: false,
            }
            .into(),
            LlmToolSpec {
                name: "tool_b".to_string(),
                description: "second".to_string(),
                parameters: serde_json::json!({"type": "object"}),
                strict: false,
            }
            .into(),
        ]),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
    };

    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);
    let tools = body["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 2);
    assert!(
        tools[0].get("cache_control").is_none(),
        "earlier tool must not carry a cache breakpoint"
    );
    assert_eq!(tools[1]["cache_control"]["type"], "ephemeral");
}

#[test]
fn request_body_places_tool_cache_control_on_last_first_party_tool_when_mcp_tools_trail() {
    // Two first-party tools followed by two MCP tools (the partition the
    // tool registry enforces). The breakpoint must sit on the last first
    // -party tool so the cached prefix survives an MCP `tools/list`
    // refresh that mutates only the trailing MCP block.
    let request = LlmRequest {
        model: squeezy_core::DEFAULT_ANTHROPIC_MODEL.to_string().into(),
        instructions: "system prompt".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
        max_output_tokens: Some(32),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: Some("squeezy::session-1".to_string()),
        tools: Arc::from(vec![
            LlmToolSpec {
                name: "apply_patch".to_string(),
                description: "edit".to_string(),
                parameters: serde_json::json!({"type": "object"}),
                strict: false,
            }
            .into(),
            LlmToolSpec {
                name: "read_file".to_string(),
                description: "read".to_string(),
                parameters: serde_json::json!({"type": "object"}),
                strict: false,
            }
            .into(),
            LlmToolSpec {
                name: "mcp__linear__create_issue".to_string(),
                description: "create issue".to_string(),
                parameters: serde_json::json!({"type": "object"}),
                strict: false,
            }
            .into(),
            LlmToolSpec {
                name: "mcp__linear__list_issues".to_string(),
                description: "list issues".to_string(),
                parameters: serde_json::json!({"type": "object"}),
                strict: false,
            }
            .into(),
        ]),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
    };

    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);
    let tools = body["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 4);
    assert!(tools[0].get("cache_control").is_none());
    assert_eq!(
        tools[1]["cache_control"]["type"], "ephemeral",
        "breakpoint must sit on the last first-party tool"
    );
    assert!(tools[2].get("cache_control").is_none());
    assert!(tools[3].get("cache_control").is_none());
}

#[test]
fn request_body_falls_back_to_last_tool_when_all_advertised_tools_are_mcp() {
    // Edge case: only MCP tools are advertised (no stable first-party
    // prefix to anchor). Fall back to the unconditional last tool so
    // caching is still attempted — losing the cache on every MCP refresh
    // is no worse than the pre-change behavior, but caching the prefix
    // when MCP tools are stable for many turns is still a win.
    let request = LlmRequest {
        model: squeezy_core::DEFAULT_ANTHROPIC_MODEL.to_string().into(),
        instructions: "system prompt".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
        max_output_tokens: Some(32),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: Some("squeezy::session-1".to_string()),
        tools: Arc::from(vec![
            LlmToolSpec {
                name: "mcp__linear__create_issue".to_string(),
                description: "create issue".to_string(),
                parameters: serde_json::json!({"type": "object"}),
                strict: false,
            }
            .into(),
            LlmToolSpec {
                name: "mcp__linear__list_issues".to_string(),
                description: "list issues".to_string(),
                parameters: serde_json::json!({"type": "object"}),
                strict: false,
            }
            .into(),
        ]),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
    };

    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);
    let tools = body["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 2);
    assert!(tools[0].get("cache_control").is_none());
    assert_eq!(tools[1]["cache_control"]["type"], "ephemeral");
}

#[test]
fn request_body_omits_tool_cache_control_when_caching_disabled() {
    let request = LlmRequest {
        model: squeezy_core::DEFAULT_ANTHROPIC_MODEL.to_string().into(),
        instructions: "system".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
        max_output_tokens: Some(32),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        tools: Arc::from(vec![
            LlmToolSpec {
                name: "tool_a".to_string(),
                description: "first".to_string(),
                parameters: serde_json::json!({"type": "object"}),
                strict: false,
            }
            .into(),
        ]),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
    };

    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);
    let tools = body["tools"].as_array().expect("tools array");
    assert!(
        tools[0].get("cache_control").is_none(),
        "cache breakpoint must not be emitted without a cache_key"
    );
}

#[test]
fn request_body_skips_cache_control_when_cache_key_is_absent() {
    let request = LlmRequest {
        model: squeezy_core::DEFAULT_ANTHROPIC_MODEL.to_string().into(),
        instructions: "system".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        max_output_tokens: Some(32),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
    };

    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);

    // Without a cache_key the system field stays a plain string and no
    // cache_control markers appear in messages.
    assert_eq!(body["system"], "system");
    let messages = body["messages"].as_array().expect("messages");
    for message in messages {
        for block in message["content"].as_array().expect("blocks") {
            assert!(block.get("cache_control").is_none(), "{block}");
        }
    }
}

#[test]
fn sse_decoder_collects_data_events_across_chunks() {
    let mut decoder = SseDecoder::default();

    assert!(
        decoder
            .push(b"event: content_block_delta\ndata: {\"type\":\"content_")
            .is_empty()
    );
    let events =
        decoder.push(b"block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n");

    assert_eq!(
        events,
        vec![r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"hi"}}"#]
    );
}

#[test]
fn parser_extracts_text_delta() {
    let mut state = AnthropicStreamState::default();
    let event = parse_anthropic_event(
        r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"hello"}}"#,
        &mut state,
    )
    .expect("valid event");

    assert_eq!(event, vec![LlmEvent::TextDelta("hello".to_string())]);
}

#[test]
fn parser_extracts_streamed_tool_call() {
    let mut state = AnthropicStreamState::default();

    parse_anthropic_event(
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"read_file","input":{}}}"#,
        &mut state,
    )
    .expect("start");
    parse_anthropic_event(
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"src/"}}"#,
        &mut state,
    )
    .expect("delta");
    parse_anthropic_event(
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"lib.rs\"}"}}"#,
        &mut state,
    )
    .expect("delta");
    let event = parse_anthropic_event(r#"{"type":"content_block_stop","index":1}"#, &mut state)
        .expect("stop");

    assert_eq!(
        event,
        vec![LlmEvent::ToolCall(LlmToolCall {
            call_id: "toolu_1".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::json!({ "path": "src/lib.rs" }),
        })]
    );
}

#[test]
fn parser_extracts_completed_response_id_and_usage() {
    let mut state = AnthropicStreamState::default();

    parse_anthropic_event(
        r#"{
          "type":"message_start",
          "message":{
            "id":"msg_123",
            "usage":{
              "input_tokens":10,
              "output_tokens":1,
              "cache_read_input_tokens":3
            }
          }
        }"#,
        &mut state,
    )
    .expect("start");
    parse_anthropic_event(
        r#"{
          "type":"message_delta",
          "delta":{"stop_reason":"end_turn"},
          "usage":{"output_tokens":4}
        }"#,
        &mut state,
    )
    .expect("delta");
    let event = parse_anthropic_event(r#"{"type":"message_stop"}"#, &mut state).expect("stop");

    assert_eq!(
        event,
        vec![LlmEvent::Completed {
            response_id: Some("msg_123".to_string()),
            cost: CostSnapshot {
                input_tokens: Some(10),
                output_tokens: Some(4),
                reasoning_output_tokens: None,
                cached_input_tokens: Some(3),
                cache_write_input_tokens: None,
                estimated_usd_micros: None,
            },
            stop_reason: Some(crate::StopReason::EndTurn),
            reasoning_only_stop: false,
        }]
    );
}

#[test]
fn parser_populates_both_cache_counters_from_usage() {
    let mut state = AnthropicStreamState::default();

    parse_anthropic_event(
        r#"{
          "type":"message_start",
          "message":{
            "id":"msg_cache",
            "usage":{
              "input_tokens":42,
              "output_tokens":1,
              "cache_read_input_tokens":17,
              "cache_creation_input_tokens":29
            }
          }
        }"#,
        &mut state,
    )
    .expect("start");
    parse_anthropic_event(
        r#"{
          "type":"message_delta",
          "delta":{"stop_reason":"end_turn"},
          "usage":{"output_tokens":8}
        }"#,
        &mut state,
    )
    .expect("delta");
    let event = parse_anthropic_event(r#"{"type":"message_stop"}"#, &mut state).expect("stop");

    assert_eq!(
        event,
        vec![LlmEvent::Completed {
            response_id: Some("msg_cache".to_string()),
            cost: CostSnapshot {
                input_tokens: Some(42),
                output_tokens: Some(8),
                reasoning_output_tokens: None,
                cached_input_tokens: Some(17),
                cache_write_input_tokens: Some(29),
                estimated_usd_micros: None,
            },
            stop_reason: Some(crate::StopReason::EndTurn),
            reasoning_only_stop: false,
        }]
    );
}

#[test]
fn parser_surfaces_error_events() {
    let mut state = AnthropicStreamState::default();
    let err = parse_anthropic_event(
        r#"{"type":"error","error":{"message":"bad request"}}"#,
        &mut state,
    )
    .expect_err("stream error");

    assert!(err.to_string().contains("bad request"));
}

#[test]
fn parser_surfaces_max_tokens_stop() {
    let mut state = AnthropicStreamState::default();
    parse_anthropic_event(
        r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"}}"#,
        &mut state,
    )
    .expect("delta");

    let events = parse_anthropic_event(r#"{"type":"message_stop"}"#, &mut state).expect(
        "message_stop is no longer an early stream error; stop_reason is surfaced to the agent",
    );

    let completed = events
        .iter()
        .find_map(|event| match event {
            LlmEvent::Completed { stop_reason, .. } => Some(stop_reason.clone()),
            _ => None,
        })
        .expect("Completed event emitted");
    assert_eq!(completed, Some(crate::StopReason::MaxTokens));
}

#[test]
fn parser_normalizes_end_turn_stop_reason() {
    let mut state = AnthropicStreamState::default();
    parse_anthropic_event(
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
        &mut state,
    )
    .expect("delta");
    let events =
        parse_anthropic_event(r#"{"type":"message_stop"}"#, &mut state).expect("stop event");
    let completed = events
        .iter()
        .find_map(|event| match event {
            LlmEvent::Completed { stop_reason, .. } => Some(stop_reason.clone()),
            _ => None,
        })
        .expect("Completed event emitted");
    assert_eq!(completed, Some(crate::StopReason::EndTurn));
}

#[test]
fn parser_normalizes_refusal_stop_reason() {
    let mut state = AnthropicStreamState::default();
    parse_anthropic_event(
        r#"{"type":"message_delta","delta":{"stop_reason":"refusal"}}"#,
        &mut state,
    )
    .expect("delta");
    let events =
        parse_anthropic_event(r#"{"type":"message_stop"}"#, &mut state).expect("stop event");
    let completed = events
        .iter()
        .find_map(|event| match event {
            LlmEvent::Completed { stop_reason, .. } => Some(stop_reason.clone()),
            _ => None,
        })
        .expect("Completed event emitted");
    assert_eq!(completed, Some(crate::StopReason::Refusal));
}

#[test]
fn parser_accumulates_thinking_block_with_signature() {
    let mut state = AnthropicStreamState::default();
    parse_anthropic_event(
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
        &mut state,
    )
    .expect("start");
    let delta = parse_anthropic_event(
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"weigh"}}"#,
        &mut state,
    )
    .expect("delta");
    assert_eq!(
        delta,
        vec![LlmEvent::ReasoningDelta {
            text: "weigh".to_string(),
            kind: crate::ReasoningKind::Text,
        }]
    );
    parse_anthropic_event(
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"SIG"}}"#,
        &mut state,
    )
    .expect("signature");
    parse_anthropic_event(r#"{"type":"content_block_stop","index":0}"#, &mut state).expect("stop");

    let events = parse_anthropic_event(r#"{"type":"message_stop"}"#, &mut state).expect("stop");
    let payload = match events.first() {
        Some(LlmEvent::ReasoningDone(payload)) => payload.clone(),
        other => panic!("expected ReasoningDone first, got {other:?}"),
    };
    match payload {
        crate::ReasoningPayload::Anthropic { blocks } => {
            assert_eq!(blocks.len(), 1);
            assert_eq!(blocks[0].text, "weigh");
            assert_eq!(blocks[0].signature.as_deref(), Some("SIG"));
        }
        other => panic!("expected Anthropic payload, got {other:?}"),
    }
}

#[test]
fn anthropic_messages_attach_thinking_blocks_to_assistant_turn() {
    let payload = crate::ReasoningPayload::Anthropic {
        blocks: vec![crate::AnthropicThinkingBlock {
            kind: crate::AnthropicThinkingKind::Thinking,
            text: "deliberated".to_string(),
            signature: Some("SIG".to_string()),
            data: None,
        }],
    };
    let input = vec![
        LlmInputItem::Reasoning(payload),
        LlmInputItem::AssistantText("answer".to_string()),
    ];
    let messages = anthropic_messages(&input, false, CachePolicy::AUTO);
    let arr = messages.as_array().expect("array");
    assert_eq!(arr.len(), 1, "thinking + text fold into one assistant turn");
    let content = arr[0]["content"].as_array().expect("content array");
    assert_eq!(content[0]["type"], "thinking");
    assert_eq!(content[0]["signature"], "SIG");
    assert_eq!(content[1]["type"], "text");
    assert_eq!(content[1]["text"], "answer");
}

#[test]
fn beta_headers_route_into_http_header() {
    // The Anthropic provider does not embed beta headers in the JSON
    // body — they go on the `anthropic-beta` HTTP header. We assert the
    // routing helper produces the comma-joined value the provider
    // attaches on the outbound request.
    let betas: Arc<[Arc<str>]> = Arc::from(vec![
        Arc::<str>::from("context-1m-2025-08-07"),
        Arc::<str>::from("interleaved-thinking-2025-05-14"),
    ]);
    let header = anthropic_header_value(&betas).expect("non-empty betas yield a header value");
    assert_eq!(
        header,
        "context-1m-2025-08-07,interleaved-thinking-2025-05-14"
    );

    // The request body must not carry the betas — they belong on the
    // header for the 1P Anthropic transport.
    let request = LlmRequest {
        model: squeezy_core::DEFAULT_ANTHROPIC_MODEL.to_string().into(),
        instructions: "sys".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
        max_output_tokens: Some(32),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: betas,
    };
    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);
    assert!(
        body.get("anthropic_beta").is_none(),
        "1P Anthropic transport sends betas via header, never inside the JSON body",
    );
}

#[test]
fn beta_headers_route_into_extra_body_params_on_bedrock() {
    // Bedrock uses converse_stream and the AWS gateway strips
    // non-standard HTTP headers, so the routing helper partitions out
    // the body-param-eligible subset for `additional_model_request_fields`.
    let betas: Arc<[Arc<str>]> = Arc::from(vec![
        Arc::<str>::from("context-1m-2025-08-07"),
        Arc::<str>::from("claude-code-20250219"),
    ]);
    let body_betas = bedrock_extra_body_betas(&betas);
    let body_strs: Vec<&str> = body_betas.iter().map(|b| b.as_ref()).collect();
    assert_eq!(
        body_strs,
        vec!["context-1m-2025-08-07"],
        "header-only betas (claude-code-*) must be dropped before reaching Bedrock",
    );
}

#[test]
fn beta_headers_dedup_when_capability_and_request_overlap() {
    // Future capability-derived betas can overlap with the
    // caller-supplied list; the routing helper must deduplicate so the
    // wire value carries each beta exactly once.
    let betas: Arc<[Arc<str>]> = Arc::from(vec![
        Arc::<str>::from("a"),
        Arc::<str>::from("b"),
        Arc::<str>::from("b"),
        Arc::<str>::from("c"),
    ]);
    let header = anthropic_header_value(&betas).expect("non-empty betas yield a header value");
    assert_eq!(header, "a,b,c");
}

#[test]
fn oauth_auth_scheme_prepends_claude_code_identity_to_system() {
    // The OAuth quota check requires every Claude Pro/Max request to
    // identify itself as Claude Code in the system block. Build a
    // request body under the OAuth auth scheme and verify the
    // identity preamble lands ahead of the user's instructions.
    let request = LlmRequest {
        model: "claude-opus-4-7".to_string().into(),
        instructions: "user-supplied instructions".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        max_output_tokens: Some(64),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
    };
    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::Oauth);
    let system = body.get("system").expect("system block must be present");
    let system = system
        .as_array()
        .expect("OAuth scheme must serialize `system` as an array");
    assert!(
        system
            .first()
            .and_then(|item| item.get("text"))
            .and_then(|text| text.as_str())
            .is_some_and(|text| text.contains("Claude Code")),
        "first system block must declare the Claude Code identity, got {system:?}"
    );
    assert!(
        system.iter().any(|item| item
            .get("text")
            .and_then(|text| text.as_str())
            .is_some_and(|text| text == "user-supplied instructions")),
        "user instructions must still ride alongside the identity preamble: {system:?}"
    );
}

#[test]
fn api_key_auth_scheme_keeps_system_string_unchanged() {
    // Static-key callers shouldn't see the identity preamble — the
    // body's `system` stays the same single string the existing tests
    // already lock in.
    let request = LlmRequest {
        model: "claude-opus-4-7".to_string().into(),
        instructions: "user-supplied instructions".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        max_output_tokens: Some(64),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
    };
    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);
    assert_eq!(body["system"], "user-supplied instructions");
}

#[test]
fn merge_oauth_beta_header_unions_caller_and_oauth_marker() {
    // OAuth-driven requests must always carry the Claude Code beta
    // marker. A caller-supplied beta value should merge without
    // duplicating entries.
    let oauth_marker = crate::oauth::anthropic_oauth_beta_header();
    let merged =
        super::merge_oauth_beta_header(Some("context-1m-2025-08-07"), AnthropicAuthScheme::Oauth)
            .expect("oauth scheme must produce a header value");
    for piece in oauth_marker.split(',') {
        assert!(
            merged.contains(piece),
            "merged header must keep oauth marker {piece}; got {merged}",
        );
    }
    assert!(
        merged.contains("context-1m-2025-08-07"),
        "merged header must keep caller beta; got {merged}",
    );
    let pieces: Vec<&str> = merged.split(',').collect();
    let mut dedup = pieces.clone();
    dedup.sort();
    dedup.dedup();
    assert_eq!(
        pieces.len(),
        dedup.len(),
        "merged header must not duplicate entries: {merged}"
    );
}

#[test]
fn merge_oauth_beta_header_returns_none_for_api_key_without_caller() {
    assert!(
        super::merge_oauth_beta_header(None, AnthropicAuthScheme::ApiKey).is_none(),
        "API-key callers with no betas should produce no header"
    );
    assert_eq!(
        super::merge_oauth_beta_header(Some("a,b"), AnthropicAuthScheme::ApiKey),
        Some("a,b".to_string()),
        "API-key path passes the caller's value through unchanged"
    );
}

#[test]
fn request_body_encodes_image_as_base64_content_block() {
    // Synthetic 8-byte PNG-magic prefix; the bytes here don't have to be
    // a real image because we're only inspecting the wire shape.
    let bytes: Arc<[u8]> = Arc::from(vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    let request = LlmRequest {
        model: "claude-test".to_string().into(),
        instructions: "describe images".to_string().into(),
        input: Arc::from(vec![
            LlmInputItem::UserText("what is this?".to_string()),
            LlmInputItem::Image {
                media_type: "image/png".to_string(),
                bytes: bytes.clone(),
            },
        ]),
        max_output_tokens: Some(32),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
    };

    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);

    // The user text and image must coalesce into one user message with
    // two content blocks (text then image) so Anthropic sees them as a
    // single multimodal turn.
    let content = body["messages"][0]["content"].as_array().expect("content");
    assert_eq!(content.len(), 2);
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], "what is this?");
    assert_eq!(content[1]["type"], "image");
    assert_eq!(content[1]["source"]["type"], "base64");
    assert_eq!(content[1]["source"]["media_type"], "image/png");
    let encoded = content[1]["source"]["data"]
        .as_str()
        .expect("base64 string");
    use base64::Engine as _;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .expect("valid base64");
    assert_eq!(decoded.as_slice(), bytes.as_ref());
}
