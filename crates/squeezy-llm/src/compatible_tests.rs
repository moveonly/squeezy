use super::*;
use crate::{LlmEvent, LlmInputItem, LlmToolSpec};
use serde_json::json;
use squeezy_core::OpenAiCompatiblePreset;
use std::sync::Arc;

fn sample_request() -> LlmRequest {
    LlmRequest {
        model: "anthropic/claude-opus-4-7".to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![
            LlmInputItem::UserText("hello".to_string()),
            LlmInputItem::AssistantText("hi there".to_string()),
            LlmInputItem::FunctionCallOutput {
                call_id: "call_42".to_string(),
                output: r#"{"result":"ok"}"#.to_string(),
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
    }
}

#[test]
fn request_body_uses_chat_completions_shape() {
    let body = OpenAiCompatibleProvider::request_body(&sample_request());

    assert_eq!(body["model"], "anthropic/claude-opus-4-7");
    assert_eq!(body["stream"], true);
    assert_eq!(body["max_tokens"], 128);
    assert_eq!(body["stream_options"]["include_usage"], true);

    let messages = body["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 4, "system + 3 input items");
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], "be brief");
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], "hello");
    assert_eq!(messages[2]["role"], "assistant");
    assert_eq!(messages[2]["content"], "hi there");
    assert_eq!(messages[3]["role"], "tool");
    assert_eq!(messages[3]["tool_call_id"], "call_42");
    assert_eq!(messages[3]["content"], r#"{"result":"ok"}"#);

    let tools = body["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["type"], "function");
    assert_eq!(tools[0]["function"]["name"], "grep");
    assert_eq!(tools[0]["function"]["description"], "search files");
    assert_eq!(
        tools[0]["function"]["parameters"]["properties"]["pattern"]["type"],
        "string"
    );
}

#[test]
fn request_body_skips_empty_system_message() {
    let mut request = sample_request();
    request.instructions = "   ".to_string().into();
    let body = OpenAiCompatibleProvider::request_body(&request);

    let messages = body["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["role"], "user");
}

#[test]
fn request_body_serialises_assistant_function_call_history() {
    let request = LlmRequest {
        model: "groq/llama-3.3-70b".to_string().into(),
        instructions: "ok".to_string().into(),
        input: Arc::from(vec![LlmInputItem::FunctionCall {
            call_id: "call_99".to_string(),
            name: "grep".to_string(),
            arguments: json!({"pattern": "todo"}),
        }]),
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        tools: Arc::from(Vec::new()),
        store: false,
    };
    let body = OpenAiCompatibleProvider::request_body(&request);
    let messages = body["messages"].as_array().expect("messages array");
    let assistant_call = &messages[1];
    assert_eq!(assistant_call["role"], "assistant");
    let tool_call = &assistant_call["tool_calls"][0];
    assert_eq!(tool_call["id"], "call_99");
    assert_eq!(tool_call["type"], "function");
    assert_eq!(tool_call["function"]["name"], "grep");
    let arguments_text = tool_call["function"]["arguments"]
        .as_str()
        .expect("arguments serialised as string");
    let parsed: Value = serde_json::from_str(arguments_text).unwrap();
    assert_eq!(parsed["pattern"], "todo");
}

#[test]
fn parse_chat_event_emits_text_delta() {
    let mut state = StreamState::default();
    let events = parse_chat_event(
        r#"{"id":"resp_1","choices":[{"delta":{"content":"hello"}}]}"#,
        &mut state,
    )
    .expect("valid event");
    assert_eq!(events, vec![LlmEvent::TextDelta("hello".to_string())]);
    assert_eq!(state.response_id.as_deref(), Some("resp_1"));
}

#[test]
fn parse_chat_event_accumulates_tool_call_across_deltas() {
    let mut state = StreamState::default();
    parse_chat_event(
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_x","type":"function","function":{"name":"grep"}}]}}]}"#,
        &mut state,
    )
    .expect("first delta");
    parse_chat_event(
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"pat"}}]}}]}"#,
        &mut state,
    )
    .expect("partial args");
    parse_chat_event(
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"tern\":\"todo\"}"}}]}}]}"#,
        &mut state,
    )
    .expect("more args");
    let events = parse_chat_event(
        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        &mut state,
    )
    .expect("finish");

    assert_eq!(events.len(), 1);
    let LlmEvent::ToolCall(call) = &events[0] else {
        panic!("expected tool call, got {:?}", events[0]);
    };
    assert_eq!(call.call_id, "call_x");
    assert_eq!(call.name, "grep");
    assert_eq!(call.arguments["pattern"], "todo");
}

#[test]
fn parse_chat_event_marks_invalid_tool_arguments() {
    let mut state = StreamState::default();
    parse_chat_event(
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"f","arguments":"{bad"}}]}}]}"#,
        &mut state,
    )
    .expect("ok");
    let events = parse_chat_event(
        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        &mut state,
    )
    .expect("finish");
    let LlmEvent::ToolCall(call) = &events[0] else {
        panic!("expected tool call");
    };
    assert_eq!(
        call.arguments[crate::INVALID_TOOL_ARGUMENTS_KEY],
        Value::Bool(true)
    );
    assert_eq!(
        call.arguments[crate::INVALID_TOOL_ARGUMENTS_RAW_KEY],
        Value::String("{bad".to_string())
    );
}

#[test]
fn parse_chat_event_captures_usage_for_cost() {
    let mut state = StreamState::default();
    parse_chat_event(
        r#"{"usage":{"prompt_tokens":120,"completion_tokens":80,"prompt_tokens_details":{"cached_tokens":40},"completion_tokens_details":{"reasoning_tokens":12}}}"#,
        &mut state,
    )
    .expect("usage");
    assert_eq!(state.cost.input_tokens, Some(120));
    assert_eq!(state.cost.output_tokens, Some(80));
    assert_eq!(state.cost.cached_input_tokens, Some(40));
    assert_eq!(state.cost.reasoning_output_tokens, Some(12));
}

#[test]
fn parse_chat_event_handles_done_sentinel() {
    let mut state = StreamState {
        response_id: Some("resp_2".to_string()),
        cost: squeezy_core::CostSnapshot {
            input_tokens: Some(10),
            output_tokens: Some(5),
            ..Default::default()
        },
        ..StreamState::default()
    };
    let events = parse_chat_event("[DONE]", &mut state).expect("done");
    assert_eq!(events.len(), 1);
    let LlmEvent::Completed { response_id, cost } = &events[0] else {
        panic!("expected completed event");
    };
    assert_eq!(response_id.as_deref(), Some("resp_2"));
    assert_eq!(cost.input_tokens, Some(10));
    assert_eq!(cost.output_tokens, Some(5));
    assert!(state.completed_emitted);
}

#[test]
fn parse_chat_event_propagates_stream_error() {
    let mut state = StreamState::default();
    let err = parse_chat_event(
        r#"{"error":{"message":"rate limited","type":"rate_limit_error"}}"#,
        &mut state,
    )
    .expect_err("must surface error");
    let message = err.to_string();
    assert!(message.contains("rate limited"), "got: {message}");
}

#[test]
fn preset_defaults_round_trip() {
    for preset in OpenAiCompatiblePreset::all() {
        let canonical = preset.as_str();
        let parsed = OpenAiCompatiblePreset::parse(canonical)
            .unwrap_or_else(|| panic!("preset {canonical} must round-trip via parse"));
        assert_eq!(parsed, preset);
    }
}

#[test]
fn preset_default_headers_include_openrouter_attribution() {
    let headers = preset_default_headers(OpenAiCompatiblePreset::OpenRouter);
    assert_eq!(
        headers.get("HTTP-Referer").map(String::as_str),
        Some("https://github.com/esqueezy/squeezy"),
    );
    assert_eq!(headers.get("X-Title").map(String::as_str), Some("Squeezy"));

    let no_headers = preset_default_headers(OpenAiCompatiblePreset::Vercel);
    assert!(no_headers.is_empty());
}

#[test]
fn preset_full_tier_matches_documented_set() {
    let full: Vec<_> = OpenAiCompatiblePreset::all()
        .iter()
        .copied()
        .filter(|p| p.is_full_tier())
        .collect();
    assert_eq!(
        full,
        vec![
            OpenAiCompatiblePreset::OpenRouter,
            OpenAiCompatiblePreset::Vercel,
            OpenAiCompatiblePreset::PortKey,
            OpenAiCompatiblePreset::Groq,
            OpenAiCompatiblePreset::XAi,
            OpenAiCompatiblePreset::DeepSeek,
        ]
    );
}
