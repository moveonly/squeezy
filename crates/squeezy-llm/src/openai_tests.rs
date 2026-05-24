use super::*;
use crate::{LlmInputItem, LlmToolCall, LlmToolSpec};
use serde_json::json;

#[test]
fn request_body_uses_responses_streaming_shape() {
    let request = LlmRequest {
        model: "gpt-test".to_string(),
        instructions: "be brief".to_string(),
        input: vec![LlmInputItem::UserText("hello".to_string())],
        max_output_tokens: Some(32),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: Some("resp_123".to_string()),
        tools: vec![LlmToolSpec {
            name: "grep".to_string(),
            description: "search files".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {"pattern": {"type": "string"}},
                "required": ["pattern"]
            }),
            strict: true,
        }],
        store: true,
    };

    let body = OpenAiProvider::request_body(&request);

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
fn sse_decoder_collects_data_events_across_chunks() {
    let mut decoder = SseDecoder::default();

    assert!(
        decoder
            .push(b"event: message\ndata: {\"type\":\"response.")
            .is_empty()
    );
    let events = decoder.push(b"output_text.delta\",\"delta\":\"hi\"}\n\n");

    assert_eq!(
        events,
        vec![r#"{"type":"response.output_text.delta","delta":"hi"}"#]
    );
}

#[test]
fn sse_decoder_accepts_crlf_event_delimiters() {
    let mut decoder = SseDecoder::default();
    let events = decoder.push(
        b"event: message\r\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\r\n\r\n",
    );

    assert_eq!(
        events,
        vec![r#"{"type":"response.output_text.delta","delta":"hi"}"#]
    );
}

#[test]
fn sse_decoder_splits_multiple_crlf_events() {
    let mut decoder = SseDecoder::default();
    let events = decoder.push(
        b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"one\"}\r\n\r\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"two\"}\r\n\r\n",
    );

    assert_eq!(
        events,
        vec![
            r#"{"type":"response.output_text.delta","delta":"one"}"#,
            r#"{"type":"response.output_text.delta","delta":"two"}"#
        ]
    );
}

#[test]
fn parser_extracts_text_delta() {
    let event = parse_openai_event(r#"{"type":"response.output_text.delta","delta":"hello"}"#)
        .expect("valid event");

    assert_eq!(event, Some(LlmEvent::TextDelta("hello".to_string())));
}

#[test]
fn request_body_serializes_tool_outputs_as_input_items() {
    let request = LlmRequest {
        model: "gpt-test".to_string(),
        instructions: "be brief".to_string(),
        input: vec![
            LlmInputItem::FunctionCall {
                call_id: "call_1".to_string(),
                name: "grep".to_string(),
                arguments: json!({"pattern": "needle"}),
            },
            LlmInputItem::FunctionCallOutput {
                call_id: "call_1".to_string(),
                output: "{\"status\":\"success\"}".to_string(),
            },
        ],
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        tools: Vec::new(),
        store: false,
    };

    let body = OpenAiProvider::request_body(&request);

    assert_eq!(body["input"][0]["type"], "function_call");
    assert_eq!(body["input"][0]["arguments"], r#"{"pattern":"needle"}"#);
    assert_eq!(body["input"][1]["type"], "function_call_output");
}

#[test]
fn request_body_preserves_function_tool_order() {
    let request = LlmRequest {
        model: "gpt-test".to_string(),
        instructions: "be brief".to_string(),
        input: vec![LlmInputItem::UserText("hello".to_string())],
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        tools: vec![
            LlmToolSpec {
                name: "write_file".to_string(),
                description: "write".to_string(),
                parameters: json!({"type": "object"}),
                strict: true,
            },
            LlmToolSpec {
                name: "grep".to_string(),
                description: "search".to_string(),
                parameters: json!({"type": "object"}),
                strict: true,
            },
        ],
        store: false,
    };

    let body = OpenAiProvider::request_body(&request);

    assert_eq!(body["tools"][0]["name"], "write_file");
    assert_eq!(body["tools"][1]["name"], "grep");
}

#[test]
fn request_body_includes_reasoning_and_text_verbosity_when_set() {
    let request = LlmRequest {
        model: "gpt-test".to_string(),
        instructions: "be brief".to_string(),
        input: vec![LlmInputItem::UserText("hello".to_string())],
        max_output_tokens: None,
        response_verbosity: Some(squeezy_core::ResponseVerbosity::Verbose),
        reasoning_effort: Some(squeezy_core::ReasoningEffort::High),
        previous_response_id: None,
        tools: Vec::new(),
        store: false,
    };

    let body = OpenAiProvider::request_body(&request);

    assert_eq!(body["text"]["verbosity"], "verbose");
    assert_eq!(body["reasoning"]["effort"], "high");
}

#[test]
fn parser_extracts_function_call_from_output_item_done() {
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
fn parser_extracts_completed_response_id_and_usage() {
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
        })
    );
}

#[test]
fn parser_surfaces_error_events() {
    let err = parse_openai_event(r#"{"type":"error","error":{"message":"bad request"}}"#)
        .expect_err("stream error");

    assert!(err.to_string().contains("bad request"));
}

#[test]
fn parser_surfaces_incomplete_events() {
    let err = parse_openai_event(
        r#"{
          "type":"response.incomplete",
          "response":{
            "incomplete_details":{"reason":"max_output_tokens"}
          }
        }"#,
    )
    .expect_err("incomplete response is a stream error");

    assert!(err.to_string().contains("max_output_tokens"));
}
