use super::*;
use crate::{LlmEvent, LlmInputItem, LlmToolSpec};
use serde_json::json;
use std::sync::Arc;

fn sample_request() -> LlmRequest {
    LlmRequest {
        model: "openai/gpt-oss-20b".to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![
            LlmInputItem::UserText("hello".to_string()),
            LlmInputItem::AssistantText("hi".to_string()),
            LlmInputItem::FunctionCallOutput {
                call_id: "call_1".to_string(),
                output: r#"{"ok":true}"#.to_string(),
            },
        ]),
        max_output_tokens: Some(128),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
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
        store: false,
        output_schema: None,
        parallel_tool_calls: None,
    }
}

#[test]
fn request_body_uses_chat_completions_shape() {
    let body = LMStudioProvider::request_body(&sample_request());

    assert_eq!(body["model"], "openai/gpt-oss-20b");
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert_eq!(body["max_tokens"], 128);

    let messages = body["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 4, "system + 3 input items");
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], "be brief");
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], "hello");
    assert_eq!(messages[2]["role"], "assistant");
    assert_eq!(messages[2]["content"], "hi");
    assert_eq!(messages[3]["role"], "tool");
    assert_eq!(messages[3]["tool_call_id"], "call_1");

    let tools = body["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["type"], "function");
    assert_eq!(tools[0]["function"]["name"], "grep");
}

#[test]
fn request_body_skips_empty_system_message() {
    let mut request = sample_request();
    request.instructions = "   ".to_string().into();
    let body = LMStudioProvider::request_body(&request);

    let messages = body["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["role"], "user");
}

#[test]
fn parser_collects_text_deltas_into_completed() {
    let mut state = StreamState::default();

    let events = parse_chat_event(
        r#"{"id":"chatcmpl-1","choices":[{"delta":{"content":"hello"}}]}"#,
        &mut state,
    )
    .expect("delta");
    assert_eq!(events, vec![LlmEvent::TextDelta("hello".to_string())]);

    let events = parse_chat_event(
        r#"{"choices":[{"delta":{"content":" world"}}]}"#,
        &mut state,
    )
    .expect("delta");
    assert_eq!(events, vec![LlmEvent::TextDelta(" world".to_string())]);

    let events = parse_chat_event(
        r#"{"choices":[{"finish_reason":"stop"}],"usage":{"prompt_tokens":4,"completion_tokens":2}}"#,
        &mut state,
    )
    .expect("finish");
    // finish_reason with no pending tool calls drains nothing.
    assert!(
        events.is_empty(),
        "finish reason emits no events on its own"
    );

    let events = parse_chat_event("[DONE]", &mut state).expect("done");
    assert_eq!(events.len(), 1);
    match &events[0] {
        LlmEvent::Completed { response_id, cost } => {
            assert_eq!(response_id.as_deref(), Some("chatcmpl-1"));
            assert_eq!(cost.input_tokens, Some(4));
            assert_eq!(cost.output_tokens, Some(2));
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}

#[test]
fn parser_accumulates_tool_call_arguments_across_deltas() {
    let mut state = StreamState::default();

    parse_chat_event(
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_42","function":{"name":"grep","arguments":"{\"pattern"}}]}}]}"#,
        &mut state,
    )
    .expect("tool delta");
    parse_chat_event(
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\":\"needle\"}"}}]}}]}"#,
        &mut state,
    )
    .expect("tool delta");
    let events = parse_chat_event(
        r#"{"choices":[{"finish_reason":"tool_calls"}]}"#,
        &mut state,
    )
    .expect("finish");

    assert_eq!(events.len(), 1);
    match &events[0] {
        LlmEvent::ToolCall(call) => {
            assert_eq!(call.call_id, "call_42");
            assert_eq!(call.name, "grep");
            assert_eq!(call.arguments, json!({"pattern": "needle"}));
        }
        other => panic!("expected ToolCall, got {other:?}"),
    }
}

#[test]
fn parser_surfaces_server_errors() {
    let mut state = StreamState::default();
    let err = parse_chat_event(r#"{"error":{"message":"model not loaded"}}"#, &mut state)
        .expect_err("error event");
    let SqueezyError::ProviderStream(message) = err else {
        panic!("expected ProviderStream");
    };
    assert!(message.contains("model not loaded"), "got {message}");
}

#[test]
fn fetch_model_names_extracts_data_array_ids() {
    let value = json!({
        "data": [
            {"id": "openai/gpt-oss-20b"},
            {"id": "qwen/qwen3-32b"},
            {"name": "missing-id"}
        ]
    });

    assert_eq!(
        lmstudio_model_names_from_models(&value),
        vec!["openai/gpt-oss-20b", "qwen/qwen3-32b"]
    );
}

#[test]
fn fetch_model_names_handles_empty_response() {
    let value = json!({});
    assert!(lmstudio_model_names_from_models(&value).is_empty());
}

#[test]
fn config_default_points_at_localhost_1234() {
    let config = LMStudioConfig::default();
    assert_eq!(config.base_url, "http://localhost:1234/v1");
    assert!(config.api_key.is_none());
}
