use serde_json::json;
use std::sync::Arc;

use super::*;
use crate::{CacheSpec, LlmInputItem, LlmToolCall, LlmToolSpec};

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
        cache: CacheSpec::default(),
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
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
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

    let body = GoogleProvider::request_body(&request);

    assert_eq!(
        body["contents"][1]["parts"][0]["functionResponse"]["name"],
        "grep"
    );
}

#[test]
fn parser_extracts_text_tool_calls_and_usage() {
    let mut cost = CostSnapshot::default();
    let mut last_finish_reason: Option<String> = None;
    let mut reasoning_buf = GoogleReasoningBuffer::default();
    let mut server_model_slot: Option<String> = None;
    let mut tool_call_counter: usize = 0;
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
          },
          "modelVersion":"gemini-2.5-pro-002"
        }"#,
        &mut cost,
        &mut last_finish_reason,
        &mut reasoning_buf,
        &mut server_model_slot,
        &mut tool_call_counter,
    )
    .expect("valid event");

    assert_eq!(events[0], LlmEvent::TextDelta("hi".to_string()));
    assert_eq!(
        events[1],
        LlmEvent::ToolCall(LlmToolCall {
            call_id: "google_call_0".to_string(),
            name: "grep".to_string(),
            arguments: json!({"pattern": "needle"}),
        })
    );
    assert_eq!(cost.input_tokens, Some(10));
    assert_eq!(cost.output_tokens, Some(3));
    assert_eq!(cost.cached_input_tokens, Some(2));
    assert_eq!(server_model_slot.as_deref(), Some("gemini-2.5-pro-002"));
}

#[test]
fn function_response_uses_error_key_when_is_error_set() {
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
                output: "permission denied".to_string(),
                content_parts: None,
                is_error: true,
            },
        ]),
        ..LlmRequest::default()
    };
    let body = GoogleProvider::request_body(&request);
    let resp = &body["contents"][1]["parts"][0]["functionResponse"]["response"];
    assert!(
        resp.get("error").is_some(),
        "is_error=true must produce `error` key, got {resp}"
    );
    assert!(
        resp.get("output").is_none(),
        "is_error=true must not produce `output` key, got {resp}"
    );
    assert_eq!(resp["error"], "permission denied");
}

#[test]
fn function_response_uses_output_key_when_not_error() {
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
                output: "match: foo".to_string(),
                content_parts: None,
                is_error: false,
            },
        ]),
        ..LlmRequest::default()
    };
    let body = GoogleProvider::request_body(&request);
    let resp = &body["contents"][1]["parts"][0]["functionResponse"]["response"];
    assert_eq!(resp["output"], "match: foo");
    assert!(resp.get("error").is_none());
}

#[test]
fn sanitize_for_gemini_drops_unsupported_keys_and_synthesizes_properties() {
    let raw = json!({
        "type": "object",
        "additionalProperties": false,
        "$schema": "http://json-schema.org/draft-07/schema",
        "$ref": "#/definitions/Foo"
    });
    let out = sanitize_for_gemini(&raw);
    assert!(
        out.get("additionalProperties").is_none(),
        "additionalProperties must be stripped, got {out}"
    );
    assert!(out.get("$schema").is_none(), "$schema must be stripped");
    assert!(out.get("$ref").is_none(), "$ref must be stripped");
    // Empty object schema must gain an empty `properties` map so Gemini
    // doesn't reject "should be non-empty for OBJECT type".
    assert_eq!(out["type"], "object");
    assert!(
        out["properties"].is_object(),
        "object schema must have properties, got {out}"
    );
}

#[test]
fn sanitize_for_gemini_coerces_nullable_union_to_nullable_flag() {
    let raw = json!({
        "type": "object",
        "properties": {
            "name": {"type": ["string", "null"]}
        }
    });
    let out = sanitize_for_gemini(&raw);
    let name = &out["properties"]["name"];
    assert_eq!(name["type"], "string");
    assert_eq!(name["nullable"], true);
}

#[test]
fn sanitize_for_gemini_recurses_into_nested_properties_and_items() {
    let raw = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "items": {
                "type": "array",
                "items": {
                    "type": "object",
                    "$ref": "#/x"
                }
            }
        }
    });
    let out = sanitize_for_gemini(&raw);
    let inner = &out["properties"]["items"]["items"];
    assert!(
        inner.get("$ref").is_none(),
        "nested $ref must be stripped, got {inner}"
    );
    assert!(
        inner["properties"].is_object(),
        "nested object must synthesize properties, got {inner}"
    );
}

#[test]
fn request_body_runs_sanitize_pass_on_tool_parameters() {
    let request = LlmRequest {
        model: "gemini-test".to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
        tools: Arc::from(vec![
            LlmToolSpec {
                name: "read".to_string(),
                description: "read".to_string(),
                parameters: json!({"type": "object", "additionalProperties": false}),
                strict: true,
            }
            .into(),
        ]),
        ..LlmRequest::default()
    };
    let body = GoogleProvider::request_body(&request);
    let params = &body["tools"][0]["functionDeclarations"][0]["parameters"];
    assert!(
        params.get("additionalProperties").is_none(),
        "request body must strip additionalProperties, got {params}"
    );
    assert!(
        params["properties"].is_object(),
        "empty object schema must gain a `properties` map"
    );
}

#[test]
fn parallel_tool_calls_across_chunks_get_distinct_ids() {
    let mut cost = CostSnapshot::default();
    let mut last_finish_reason: Option<String> = None;
    let mut reasoning_buf = GoogleReasoningBuffer::default();
    let mut server_model_slot: Option<String> = None;
    let mut tool_call_counter: usize = 0;
    // Two separate SSE events, each carrying functionCall at parts[0].
    // Pre-fix both got `google_call_0` because the counter was the part
    // index within a single event; canonicalization then collapsed
    // both calls and the agent dropped one.
    let first = parse_google_event(
        r#"{
          "candidates":[{
            "content":{"parts":[
              {"functionCall":{"name":"grep","args":{"pattern":"first"}}}
            ]}
          }]
        }"#,
        &mut cost,
        &mut last_finish_reason,
        &mut reasoning_buf,
        &mut server_model_slot,
        &mut tool_call_counter,
    )
    .expect("valid first event");
    let second = parse_google_event(
        r#"{
          "candidates":[{
            "content":{"parts":[
              {"functionCall":{"name":"grep","args":{"pattern":"second"}}}
            ]}
          }]
        }"#,
        &mut cost,
        &mut last_finish_reason,
        &mut reasoning_buf,
        &mut server_model_slot,
        &mut tool_call_counter,
    )
    .expect("valid second event");
    let LlmEvent::ToolCall(ref first_call) = first[0] else {
        panic!("expected first ToolCall, got {:?}", first[0]);
    };
    let LlmEvent::ToolCall(ref second_call) = second[0] else {
        panic!("expected second ToolCall, got {:?}", second[0]);
    };
    assert_ne!(
        first_call.call_id, second_call.call_id,
        "parallel tool calls in separate SSE events must get distinct ids \
         (got `{}` and `{}`)",
        first_call.call_id, second_call.call_id
    );
    assert_eq!(first_call.call_id, "google_call_0");
    assert_eq!(second_call.call_id, "google_call_1");
}

#[test]
fn clamp_thinking_budget_uses_registry_max_and_min() {
    use crate::ModelCapabilities;
    let caps = ModelCapabilities {
        thinking_budget_min: Some(128),
        thinking_budget_max: Some(32_768),
        ..ModelCapabilities::TEXT_TOOLS
    };
    // XHigh (60_000) clamps down to Pro's 32_768.
    assert_eq!(clamp_thinking_budget(Some(&caps), 60_000), 32_768);
    // A budget below the min lifts to the min.
    assert_eq!(clamp_thinking_budget(Some(&caps), 0), 128);
    // A value inside the range passes through.
    assert_eq!(clamp_thinking_budget(Some(&caps), 16_000), 16_000);
    // Off-registry models leave the raw value alone.
    assert_eq!(clamp_thinking_budget(None, 60_000), 60_000);
}

#[test]
fn explicit_reasoning_effort_emits_thinking_config_with_budget() {
    use squeezy_core::ReasoningEffort;
    let request = LlmRequest {
        model: "gemini-2.5-pro".to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        reasoning_effort: Some(ReasoningEffort::Medium),
        ..LlmRequest::default()
    };
    let body = GoogleProvider::request_body(&request);
    let thinking = &body["generationConfig"]["thinkingConfig"];
    assert_eq!(
        thinking["includeThoughts"], true,
        "explicit reasoning_effort must turn includeThoughts on"
    );
    assert!(
        thinking["thinkingBudget"].is_number(),
        "explicit reasoning_effort must carry a thinkingBudget"
    );
}

#[test]
fn parser_surfaces_prompt_feedback_block_reason_as_error() {
    let mut cost = CostSnapshot::default();
    let mut last_finish_reason: Option<String> = None;
    let mut reasoning_buf = GoogleReasoningBuffer::default();
    let mut server_model_slot: Option<String> = None;
    let mut tool_call_counter: usize = 0;
    let err = parse_google_event(
        r#"{
          "promptFeedback":{"blockReason":"SAFETY"},
          "usageMetadata":{"promptTokenCount":4}
        }"#,
        &mut cost,
        &mut last_finish_reason,
        &mut reasoning_buf,
        &mut server_model_slot,
        &mut tool_call_counter,
    )
    .expect_err("blocked prompt must surface as ProviderStream error");
    let message = err.to_string();
    assert!(
        message.contains("Google blocked prompt"),
        "expected blocked-prompt prefix, got `{message}`"
    );
    assert!(
        message.contains("SAFETY"),
        "expected block reason in message, got `{message}`"
    );
}

#[test]
fn request_body_encodes_image_as_inline_data_part() {
    let bytes: Arc<[u8]> = Arc::from(vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    let request = LlmRequest {
        model: "gemini-test".to_string().into(),
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

    let body = GoogleProvider::request_body(&request);
    let contents = body["contents"].as_array().expect("contents array");
    assert_eq!(contents.len(), 2);
    // Text turn.
    assert_eq!(contents[0]["role"], "user");
    assert_eq!(contents[0]["parts"][0]["text"], "what is this?");
    // Image turn.
    assert_eq!(contents[1]["role"], "user");
    let inline = &contents[1]["parts"][0]["inlineData"];
    assert_eq!(inline["mimeType"], "image/png");
    use base64::Engine as _;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(inline["data"].as_str().expect("base64 string"))
        .expect("valid base64");
    assert_eq!(decoded.as_slice(), bytes.as_ref());
}
