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
    let mut response_id_slot: Option<String> = None;
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
          "modelVersion":"gemini-2.5-pro-002",
          "responseId":"resp-abc123"
        }"#,
        &mut cost,
        &mut last_finish_reason,
        &mut reasoning_buf,
        &mut server_model_slot,
        &mut tool_call_counter,
        &mut response_id_slot,
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
    assert_eq!(response_id_slot.as_deref(), Some("resp-abc123"));
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
    let mut response_id_slot: Option<String> = None;
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
        &mut response_id_slot,
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
        &mut response_id_slot,
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
fn output_schema_forwards_response_mime_type_and_schema() {
    use crate::LlmOutputSchema;
    let request = LlmRequest {
        model: "gemini-test".to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
        output_schema: Some(LlmOutputSchema {
            name: "Answer".to_string(),
            schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "answer": {"type": "string"}
                }
            }),
            strict: true,
        }),
        ..LlmRequest::default()
    };
    let body = GoogleProvider::request_body(&request);
    assert_eq!(
        body["generationConfig"]["responseMimeType"], "application/json",
        "output_schema must set responseMimeType"
    );
    let schema = &body["generationConfig"]["responseSchema"];
    assert_eq!(schema["type"], "object");
    assert!(
        schema.get("additionalProperties").is_none(),
        "responseSchema must run sanitize pass, got {schema}"
    );
}

#[test]
fn output_schema_unset_omits_response_fields() {
    let request = LlmRequest {
        model: "gemini-test".to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
        ..LlmRequest::default()
    };
    let body = GoogleProvider::request_body(&request);
    assert!(
        body["generationConfig"].get("responseMimeType").is_none(),
        "no output_schema => no responseMimeType, got {body}"
    );
    assert!(body["generationConfig"].get("responseSchema").is_none());
}

#[test]
fn tool_choice_required_maps_to_any_mode() {
    let request = LlmRequest {
        model: "gemini-test".to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
        tool_choice: Some("required".to_string()),
        tools: Arc::from(vec![
            LlmToolSpec {
                name: "grep".to_string(),
                description: "search".to_string(),
                parameters: json!({"type": "object"}),
                strict: true,
            }
            .into(),
        ]),
        ..LlmRequest::default()
    };
    let body = GoogleProvider::request_body(&request);
    assert_eq!(
        body["toolConfig"]["functionCallingConfig"]["mode"], "ANY",
        "tool_choice=required must map to mode=ANY, got body={body}"
    );
}

#[test]
fn tool_choice_auto_and_none_map_to_modes() {
    for (choice, want) in [("auto", "AUTO"), ("none", "NONE")] {
        let request = LlmRequest {
            model: "gemini-test".to_string().into(),
            instructions: "be brief".to_string().into(),
            input: Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
            tool_choice: Some(choice.to_string()),
            tools: Arc::from(vec![
                LlmToolSpec {
                    name: "grep".to_string(),
                    description: "search".to_string(),
                    parameters: json!({"type": "object"}),
                    strict: true,
                }
                .into(),
            ]),
            ..LlmRequest::default()
        };
        let body = GoogleProvider::request_body(&request);
        assert_eq!(
            body["toolConfig"]["functionCallingConfig"]["mode"], want,
            "tool_choice={choice} must map to mode={want}"
        );
    }
}

#[test]
fn tool_choice_unset_omits_tool_config() {
    let request = LlmRequest {
        model: "gemini-test".to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
        tool_choice: None,
        tools: Arc::from(vec![
            LlmToolSpec {
                name: "grep".to_string(),
                description: "search".to_string(),
                parameters: json!({"type": "object"}),
                strict: true,
            }
            .into(),
        ]),
        ..LlmRequest::default()
    };
    let body = GoogleProvider::request_body(&request);
    assert!(
        body.get("toolConfig").is_none(),
        "tool_choice=None must omit toolConfig, got {body}"
    );
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
fn reasoning_effort_clamps_thinking_budget_to_model_max() {
    // XHigh's raw budget (60_000) exceeds every Gemini 2.5 maximum, so
    // request_body must run it through clamp_thinking_budget against the
    // per-model registry caps. Assert the clamped *value* (not just "is a
    // number") so the test fails if the clamp ever gets bypassed.
    use squeezy_core::ReasoningEffort;

    // gemini-2.5-pro: thinking_budget_max == 32_768 (models.json).
    let pro = LlmRequest {
        model: "gemini-2.5-pro".to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        reasoning_effort: Some(ReasoningEffort::XHigh),
        ..LlmRequest::default()
    };
    let pro_body = GoogleProvider::request_body(&pro);
    assert_eq!(
        pro_body["generationConfig"]["thinkingConfig"]["thinkingBudget"], 32_768,
        "XHigh on gemini-2.5-pro must clamp to the Pro maximum"
    );

    // gemini-2.5-flash: thinking_budget_max == 24_576 (models.json).
    let flash = LlmRequest {
        model: "gemini-2.5-flash".to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        reasoning_effort: Some(ReasoningEffort::XHigh),
        ..LlmRequest::default()
    };
    let flash_body = GoogleProvider::request_body(&flash);
    assert_eq!(
        flash_body["generationConfig"]["thinkingConfig"]["thinkingBudget"], 24_576,
        "XHigh on gemini-2.5-flash must clamp to the Flash maximum"
    );
}

#[test]
fn image_with_unknown_mime_falls_back_to_inferred_type() {
    // PNG magic bytes shipped with a wrong media_type. google_contents
    // should infer from the bytes and ship `image/png` over the wire.
    let png_bytes: Arc<[u8]> = Arc::from(vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    let request = LlmRequest {
        model: "gemini-test".to_string().into(),
        instructions: "describe".to_string().into(),
        input: Arc::from(vec![LlmInputItem::Image {
            media_type: "application/octet-stream".to_string(),
            bytes: png_bytes,
        }]),
        ..LlmRequest::default()
    };
    let body = GoogleProvider::request_body(&request);
    let mime = &body["contents"][0]["parts"][0]["inlineData"]["mimeType"];
    assert_eq!(
        mime, "image/png",
        "unknown MIME should be replaced with inferred image/png, got {mime}"
    );
}

#[test]
fn image_with_supported_mime_passes_through() {
    let png_bytes: Arc<[u8]> = Arc::from(vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    let request = LlmRequest {
        model: "gemini-test".to_string().into(),
        instructions: "describe".to_string().into(),
        input: Arc::from(vec![LlmInputItem::Image {
            media_type: "image/jpeg".to_string(),
            bytes: png_bytes,
        }]),
        ..LlmRequest::default()
    };
    let body = GoogleProvider::request_body(&request);
    let mime = &body["contents"][0]["parts"][0]["inlineData"]["mimeType"];
    assert_eq!(
        mime, "image/jpeg",
        "supported MIME must pass through unchanged, got {mime}"
    );
}

#[test]
fn validate_base_url_accepts_versioned_paths() {
    validate_google_base_url("https://generativelanguage.googleapis.com/v1beta").unwrap();
    validate_google_base_url("https://generativelanguage.googleapis.com/v1").unwrap();
    validate_google_base_url("https://example.com/v1alpha").unwrap();
}

#[test]
fn validate_base_url_rejects_bare_host() {
    let err = validate_google_base_url("https://example.com").expect_err("bare host must error");
    let message = err.to_string();
    assert!(
        message.contains("/v* API version"),
        "error should hint at /v* segment, got `{message}`"
    );
}

#[test]
fn parser_includes_error_status_and_code_in_message() {
    let mut cost = CostSnapshot::default();
    let mut last_finish_reason: Option<String> = None;
    let mut reasoning_buf = GoogleReasoningBuffer::default();
    let mut server_model_slot: Option<String> = None;
    let mut tool_call_counter: usize = 0;
    let mut response_id_slot: Option<String> = None;
    let err = parse_google_event(
        r#"{
          "error":{
            "code":429,
            "message":"Quota exceeded for quota metric",
            "status":"RESOURCE_EXHAUSTED"
          }
        }"#,
        &mut cost,
        &mut last_finish_reason,
        &mut reasoning_buf,
        &mut server_model_slot,
        &mut tool_call_counter,
        &mut response_id_slot,
    )
    .expect_err("error envelope must surface as ProviderStream error");
    let message = err.to_string();
    assert!(
        message.contains("RESOURCE_EXHAUSTED"),
        "status must reach the error message, got `{message}`"
    );
    assert!(
        message.contains("429"),
        "code must reach the error message, got `{message}`"
    );
    assert!(
        message.contains("Quota exceeded"),
        "original message must still be present, got `{message}`"
    );
}

#[test]
fn check_inline_image_cap_rejects_oversize_payload() {
    // 16 MB of raw bytes -> ~21.3 MB base64-encoded, just over Gemini's
    // 20 MB inline cap.
    let big: Arc<[u8]> = Arc::from(vec![0u8; 16 * 1024 * 1024]);
    let request = LlmRequest {
        model: "gemini-test".to_string().into(),
        instructions: "describe images".to_string().into(),
        input: Arc::from(vec![LlmInputItem::Image {
            media_type: "image/png".to_string(),
            bytes: big,
        }]),
        ..LlmRequest::default()
    };
    let err = check_inline_image_cap(&request).expect_err("oversize image must error");
    let message = err.to_string();
    assert!(
        message.contains("20 MB"),
        "error should reference the 20 MB cap, got `{message}`"
    );
    assert!(
        message.contains("File API"),
        "error should suggest File API, got `{message}`"
    );
}

#[test]
fn check_inline_image_cap_accepts_small_payload() {
    let small: Arc<[u8]> = Arc::from(vec![0u8; 1024]);
    let request = LlmRequest {
        model: "gemini-test".to_string().into(),
        instructions: "describe images".to_string().into(),
        input: Arc::from(vec![LlmInputItem::Image {
            media_type: "image/png".to_string(),
            bytes: small,
        }]),
        ..LlmRequest::default()
    };
    check_inline_image_cap(&request).expect("small image must pass");
}

#[test]
fn token_split_pins_visible_vs_thoughts() {
    // Pins the convention: output_tokens = candidatesTokenCount
    // (visible), reasoning_output_tokens = thoughtsTokenCount; they
    // are exclusive and a billed-output cost reporter must sum them.
    let mut cost = CostSnapshot::default();
    let mut last_finish_reason: Option<String> = None;
    let mut reasoning_buf = GoogleReasoningBuffer::default();
    let mut server_model_slot: Option<String> = None;
    let mut tool_call_counter: usize = 0;
    let mut response_id_slot: Option<String> = None;
    parse_google_event(
        r#"{
          "candidates":[{"content":{"parts":[{"text":"final"}]}}],
          "usageMetadata":{
            "promptTokenCount":100,
            "candidatesTokenCount":40,
            "thoughtsTokenCount":250
          }
        }"#,
        &mut cost,
        &mut last_finish_reason,
        &mut reasoning_buf,
        &mut server_model_slot,
        &mut tool_call_counter,
        &mut response_id_slot,
    )
    .expect("valid event");
    assert_eq!(cost.input_tokens, Some(100));
    assert_eq!(
        cost.output_tokens,
        Some(40),
        "output_tokens must equal candidatesTokenCount (visible only)"
    );
    assert_eq!(
        cost.reasoning_output_tokens,
        Some(250),
        "reasoning_output_tokens must equal thoughtsTokenCount"
    );
    // A billed-output reporter sums the two.
    let billed = cost.output_tokens.unwrap() + cost.reasoning_output_tokens.unwrap();
    assert_eq!(billed, 290, "billed output = visible + thoughts");
}

#[test]
fn parser_surfaces_prompt_feedback_block_reason_as_error() {
    let mut cost = CostSnapshot::default();
    let mut last_finish_reason: Option<String> = None;
    let mut reasoning_buf = GoogleReasoningBuffer::default();
    let mut server_model_slot: Option<String> = None;
    let mut tool_call_counter: usize = 0;
    let mut response_id_slot: Option<String> = None;
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
        &mut response_id_slot,
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
