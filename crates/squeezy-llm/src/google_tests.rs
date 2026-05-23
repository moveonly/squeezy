use serde_json::json;

use super::*;
use crate::{LlmInputItem, LlmToolCall, LlmToolSpec};

#[test]
fn request_body_uses_generate_content_shape() {
    let request = LlmRequest {
        model: "gemini-test".to_string(),
        instructions: "be brief".to_string(),
        input: vec![LlmInputItem::UserText("hello".to_string())],
        max_output_tokens: Some(32),
        previous_response_id: None,
        tools: vec![LlmToolSpec {
            name: "read_file".to_string(),
            description: "read".to_string(),
            parameters: json!({"type": "object"}),
            strict: true,
        }],
        store: false,
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
fn request_body_preserves_function_response_name() {
    let request = LlmRequest {
        model: "gemini-test".to_string(),
        instructions: "be brief".to_string(),
        input: vec![
            LlmInputItem::FunctionCall {
                call_id: "call-1".to_string(),
                name: "grep".to_string(),
                arguments: json!({"pattern": "needle"}),
            },
            LlmInputItem::FunctionCallOutput {
                call_id: "call-1".to_string(),
                output: "match".to_string(),
            },
        ],
        max_output_tokens: None,
        previous_response_id: None,
        tools: Vec::new(),
        store: false,
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
