use super::*;
use crate::{LlmInputItem, LlmToolCall, LlmToolSpec};

#[test]
fn request_body_uses_messages_streaming_shape() {
    let request = LlmRequest {
        model: "claude-test".to_string(),
        instructions: "be brief".to_string(),
        input: vec![LlmInputItem::UserText("hello".to_string())],
        max_output_tokens: Some(32),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: Some("ignored".to_string()),
        cache_key: None,
        tools: vec![LlmToolSpec {
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
        }],
        store: true,
    };

    let body = AnthropicProvider::request_body(&request);

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
        model: "claude-test".to_string(),
        instructions: "be brief".to_string(),
        input: vec![LlmInputItem::UserText("hello".to_string())],
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        tools: vec![
            LlmToolSpec {
                name: "write_file".to_string(),
                description: "write".to_string(),
                parameters: serde_json::json!({"type": "object"}),
                strict: true,
            },
            LlmToolSpec {
                name: "grep".to_string(),
                description: "search".to_string(),
                parameters: serde_json::json!({"type": "object"}),
                strict: true,
            },
        ],
        store: false,
    };

    let body = AnthropicProvider::request_body(&request);

    assert_eq!(body["tools"][0]["name"], "write_file");
    assert_eq!(body["tools"][1]["name"], "grep");
}

#[test]
fn request_body_uses_model_limit_when_output_cap_unset() {
    let request = LlmRequest {
        model: squeezy_core::DEFAULT_ANTHROPIC_MODEL.to_string(),
        instructions: "be brief".to_string(),
        input: vec![LlmInputItem::UserText("hello".to_string())],
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        tools: Vec::new(),
        store: false,
    };

    let body = AnthropicProvider::request_body(&request);

    assert_eq!(body["max_tokens"], 64_000);
}

#[test]
fn request_body_maps_tool_roundtrip_messages() {
    let request = LlmRequest {
        model: "claude-test".to_string(),
        instructions: "be brief".to_string(),
        input: vec![
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
        ],
        max_output_tokens: Some(32),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        tools: Vec::new(),
        store: false,
    };

    let body = AnthropicProvider::request_body(&request);

    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["messages"][0]["content"][0]["text"], "read config");
    assert_eq!(body["messages"][1]["role"], "assistant");
    assert_eq!(body["messages"][1]["content"][0]["type"], "tool_use");
    assert_eq!(body["messages"][1]["content"][0]["id"], "toolu_1");
    assert_eq!(
        body["messages"][1]["content"][0]["input"]["path"],
        "squeezy.toml"
    );
    assert_eq!(body["messages"][2]["role"], "user");
    assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
    assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "toolu_1");
}

#[test]
fn request_body_adds_cache_control_markers_when_cache_key_and_capability_enable_caching() {
    // Use a real Anthropic model id so the registry capability lookup
    // reports prompt_caching=true; pair it with a cache_key so the
    // anthropic adapter inserts ephemeral cache markers.
    let request = LlmRequest {
        model: squeezy_core::DEFAULT_ANTHROPIC_MODEL.to_string(),
        instructions: "system prompt".to_string(),
        input: vec![
            LlmInputItem::UserText("first turn".to_string()),
            LlmInputItem::AssistantText("ack".to_string()),
            LlmInputItem::UserText("second turn".to_string()),
        ],
        max_output_tokens: Some(32),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: Some("squeezy::session-1".to_string()),
        tools: Vec::new(),
        store: false,
    };

    let body = AnthropicProvider::request_body(&request);

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
fn request_body_skips_cache_control_when_cache_key_is_absent() {
    let request = LlmRequest {
        model: squeezy_core::DEFAULT_ANTHROPIC_MODEL.to_string(),
        instructions: "system".to_string(),
        input: vec![LlmInputItem::UserText("hello".to_string())],
        max_output_tokens: Some(32),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        tools: Vec::new(),
        store: false,
    };

    let body = AnthropicProvider::request_body(&request);

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

    assert_eq!(event, Some(LlmEvent::TextDelta("hello".to_string())));
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
        Some(LlmEvent::ToolCall(LlmToolCall {
            call_id: "toolu_1".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::json!({ "path": "src/lib.rs" }),
        }))
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
        Some(LlmEvent::Completed {
            response_id: Some("msg_123".to_string()),
            cost: CostSnapshot {
                input_tokens: Some(10),
                output_tokens: Some(4),
                reasoning_output_tokens: None,
                cached_input_tokens: Some(3),
                cache_write_input_tokens: None,
                estimated_usd_micros: None,
            },
        })
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

    let err = parse_anthropic_event(r#"{"type":"message_stop"}"#, &mut state)
        .expect_err("max_tokens is a stream error");

    assert!(err.to_string().contains("max_tokens"));
}
