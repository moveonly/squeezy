use serde_json::json;
use std::sync::Arc;

use super::*;
use crate::{LlmInputItem, LlmToolCall, LlmToolSpec};

#[test]
fn stream_url_does_not_contain_api_key() {
    let url = google_stream_url(
        "https://generativelanguage.googleapis.com/v1beta",
        "gemini-test",
    );
    assert_eq!(
        url,
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-test:streamGenerateContent?alt=sse"
    );
    assert!(
        !url.contains("key="),
        "API key must travel in the x-goog-api-key header, not the URL"
    );
}

#[test]
fn request_body_uses_generate_content_shape() {
    let request = LlmRequest {
        model: "gemini-test".to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        max_output_tokens: Some(32),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        tools: Arc::from(vec![
            LlmToolSpec {
                name: "read_file".to_string(),
                description: "read".to_string(),
                parameters: json!({"type": "object"}),
                strict: true,
            }
            .into(),
        ]),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
    };

    let body = GoogleProvider::request_body(&request);

    assert_eq!(body["systemInstruction"]["parts"][0]["text"], "be brief");
    assert_eq!(body["contents"][0]["role"], "user");
    assert_eq!(body["contents"][0]["parts"][0]["text"], "hello");
    assert_eq!(body["generationConfig"]["maxOutputTokens"], 32);
    assert_eq!(
        body["tools"][0]["functionDeclarations"][0]["name"],
        "read_file"
    );
}

#[test]
fn request_body_preserves_function_tool_order() {
    let request = LlmRequest {
        model: "gemini-test".to_string().into(),
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
    };

    let body = GoogleProvider::request_body(&request);

    assert_eq!(
        body["tools"][0]["functionDeclarations"][0]["name"],
        "write_file"
    );
    assert_eq!(body["tools"][0]["functionDeclarations"][1]["name"], "grep");
}

#[test]
fn request_body_preserves_function_response_name() {
    let request = LlmRequest {
        model: "gemini-test".to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![
            LlmInputItem::FunctionCall {
                call_id: "call-1".to_string(),
                name: "grep".to_string(),
                arguments: json!({"pattern": "needle"}),
            },
            LlmInputItem::FunctionCallOutput {
                call_id: "call-1".to_string(),
                output: "match".to_string(),
            },
        ]),
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
    };

    let body = GoogleProvider::request_body(&request);

    assert_eq!(
        body["contents"][1]["parts"][0]["functionResponse"]["name"],
        "grep"
    );
}

#[test]
fn parser_extracts_text_tool_calls_and_usage() {
    let mut cost = CostSnapshot::default();
    let mut reasoning_buf = GoogleReasoningBuffer::default();
    let events = parse_google_event(
        r#"{
          "candidates":[{
            "content":{"parts":[
              {"text":"hi"},
              {"functionCall":{"name":"grep","args":{"pattern":"needle"}}}
            ]}
          }],
          "usageMetadata":{
            "promptTokenCount":10,
            "candidatesTokenCount":3,
            "cachedContentTokenCount":2
          }
        }"#,
        &mut cost,
        &mut reasoning_buf,
    )
    .expect("valid event");

    assert_eq!(events[0], LlmEvent::TextDelta("hi".to_string()));
    assert_eq!(
        events[1],
        LlmEvent::ToolCall(LlmToolCall {
            call_id: "google_call_1".to_string(),
            name: "grep".to_string(),
            arguments: json!({"pattern": "needle"}),
        })
    );
    assert_eq!(cost.input_tokens, Some(10));
    assert_eq!(cost.output_tokens, Some(3));
    assert_eq!(cost.cached_input_tokens, Some(2));
}
