use serde_json::json;
use std::sync::Arc;

use super::*;
use crate::{CacheSpec, LlmInputItem, LlmToolSpec};

#[test]
fn request_body_uses_chat_stream_shape() {
    let request = LlmRequest {
        model: "qwen3".to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        max_output_tokens: Some(16),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        cache: CacheSpec::default(),
        tools: Arc::from(vec![
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

    let body = OllamaProvider::request_body(&request);

    assert_eq!(body["model"], "qwen3");
    assert_eq!(body["stream"], true);
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][1]["role"], "user");
    assert_eq!(body["options"]["num_predict"], 16);
    assert_eq!(body["options"]["num_ctx"], DEFAULT_NUM_CTX);
    assert_eq!(body["tools"][0]["function"]["name"], "grep");
}

#[test]
fn request_body_emits_keep_alive_when_set() {
    let request = LlmRequest {
        model: "qwen3".to_string().into(),
        instructions: String::new().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
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

    let with_value = OllamaProvider::request_body_with(&request, Some("24h"));
    assert_eq!(with_value["keep_alive"], "24h");

    let without_value = OllamaProvider::request_body_with(&request, None);
    assert!(without_value.get("keep_alive").is_none());
}

#[test]
fn with_keep_alive_sets_field_for_plumbing() {
    let provider = OllamaProvider::from_config(&squeezy_core::OllamaConfig {
        base_url: "http://localhost:11434/api".to_string(),
        route_style: squeezy_core::OllamaRoute::Native,
        transport: squeezy_core::ProviderTransportConfig::default(),
    })
    .with_keep_alive("-1")
    .with_api_key("secret-token");
    assert_eq!(provider.keep_alive.as_deref(), Some("-1"));
    assert_eq!(provider.api_key.as_deref(), Some("secret-token"));
}

#[test]
fn request_body_always_sets_num_ctx_default() {
    let request = LlmRequest {
        model: "qwen3".to_string().into(),
        instructions: String::new().into(),
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

    let body = OllamaProvider::request_body(&request);

    // Ollama's server default `num_ctx` is 4096, which silently truncates
    // agent prompts. The provider must stamp the explicit default on
    // every request, even when the caller asked for nothing else.
    assert_eq!(body["options"]["num_ctx"], DEFAULT_NUM_CTX);
    assert!(body["options"]["num_predict"].is_null());
}

#[test]
fn request_body_preserves_function_tool_order() {
    let request = LlmRequest {
        model: "qwen3".to_string().into(),
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

    let body = OllamaProvider::request_body(&request);

    assert_eq!(body["tools"][0]["function"]["name"], "write_file");
    assert_eq!(body["tools"][1]["function"]["name"], "grep");
}

#[test]
fn parser_extracts_text_tool_calls_and_usage() {
    let mut server_model_slot: Option<String> = None;
    let events = parse_ollama_line(
        r#"{"model":"llama3:8b-instruct-q4_0","message":{"content":"hi","tool_calls":[{"function":{"name":"grep","arguments":{"pattern":"needle"}}}]},"done":true,"prompt_eval_count":10,"eval_count":2}"#,
        &mut server_model_slot,
    )
    .expect("valid event");

    assert_eq!(
        server_model_slot.as_deref(),
        Some("llama3:8b-instruct-q4_0")
    );
    assert_eq!(events[0], LlmEvent::TextDelta("hi".to_string()));
    assert_eq!(
        events[1],
        LlmEvent::ToolCall(LlmToolCall {
            call_id: "ollama_call_0".to_string(),
            name: "grep".to_string(),
            arguments: json!({"pattern": "needle"}),
        })
    );
    assert_eq!(
        events[2],
        LlmEvent::Completed {
            response_id: None,
            cost: CostSnapshot {
                input_tokens: Some(10),
                output_tokens: Some(2),
                reasoning_output_tokens: None,
                cached_input_tokens: None,
                cache_write_input_tokens: None,
                estimated_usd_micros: Some(0),
            },
            // Ollama omits `done_reason` on natural completions; the
            // provider falls back to `EndTurn` so the agent loop sees a
            // populated stop reason on every Ollama turn.
            stop_reason: Some(crate::StopReason::EndTurn),
            reasoning_only_stop: false,
        }
    );
}

#[test]
fn request_body_sets_think_true_for_reasoning_effort() {
    let request = LlmRequest {
        model: "qwen3:8b".to_string().into(),
        instructions: String::new().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: Some(squeezy_core::ReasoningEffort::Medium),
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
    let body = OllamaProvider::request_body(&request);
    assert_eq!(body["think"], true);
}

#[test]
fn request_body_sets_think_string_for_gpt_oss() {
    let request = LlmRequest {
        model: "gpt-oss:20b".to_string().into(),
        instructions: String::new().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
        max_output_tokens: None,
        response_verbosity: None,
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
    let body = OllamaProvider::request_body(&request);
    assert_eq!(body["think"], "high");
}

#[test]
fn request_body_skips_think_for_non_reasoning_models() {
    let request = LlmRequest {
        model: "llama3.3:70b".to_string().into(),
        instructions: String::new().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
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
    let body = OllamaProvider::request_body(&request);
    assert!(body.get("think").is_none());
}

#[test]
fn parser_emits_reasoning_delta_from_message_thinking() {
    let mut server_model_slot: Option<String> = None;
    let events = parse_ollama_line(
        r#"{"model":"qwen3:8b","message":{"thinking":"let me ponder","content":""}}"#,
        &mut server_model_slot,
    )
    .expect("thinking chunk parses");
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0],
        LlmEvent::ReasoningDelta {
            text: "let me ponder".to_string(),
            kind: crate::ReasoningKind::Text,
        }
    );
}

#[test]
fn parser_decodes_string_encoded_tool_arguments() {
    let mut server_model_slot: Option<String> = None;
    let events = parse_ollama_line(
        r#"{"model":"qwen3","message":{"tool_calls":[{"function":{"name":"grep","arguments":"{\"pattern\":\"needle\"}"}}]}}"#,
        &mut server_model_slot,
    )
    .expect("string arguments parse");
    assert_eq!(
        events[0],
        LlmEvent::ToolCall(LlmToolCall {
            call_id: "ollama_call_0".to_string(),
            name: "grep".to_string(),
            arguments: json!({"pattern": "needle"}),
        })
    );
}

#[test]
fn parser_marks_invalid_string_encoded_tool_arguments() {
    let mut server_model_slot: Option<String> = None;
    let events = parse_ollama_line(
        r#"{"model":"qwen3","message":{"tool_calls":[{"function":{"name":"grep","arguments":"{not-json"}}]}}"#,
        &mut server_model_slot,
    )
    .expect("unparseable string still surfaces a call");
    let LlmEvent::ToolCall(call) = &events[0] else {
        panic!("expected ToolCall event, got {events:?}");
    };
    let obj = call.arguments.as_object().expect("marker is an object");
    assert_eq!(obj[crate::INVALID_TOOL_ARGUMENTS_KEY], true);
    assert_eq!(obj[crate::INVALID_TOOL_ARGUMENTS_RAW_KEY], "{not-json");
    assert!(obj.contains_key(crate::INVALID_TOOL_ARGUMENTS_ERROR_KEY));
}

#[test]
fn parser_treats_load_and_unload_done_reasons_as_noop() {
    // Ollama emits `{"done":true,"done_reason":"load"}` and `"unload"`
    // housekeeping frames around model lifecycle. They are not turn
    // terminals; the actual generation chunks follow. The parser must
    // swallow them so the stream loop keeps polling instead of closing
    // the turn with zero tokens.
    for reason in ["load", "unload"] {
        let mut server_model_slot: Option<String> = None;
        let line = format!(r#"{{"model":"qwen3:0.6b","done":true,"done_reason":"{reason}"}}"#);
        let events =
            parse_ollama_line(&line, &mut server_model_slot).expect("housekeeping frame parses");
        assert!(
            events.is_empty(),
            "expected no events for done_reason={reason}, got {events:?}",
        );
    }

    // Following the housekeeping frame, real content chunks must still
    // surface a TextDelta plus the genuine terminal Completed event.
    let mut server_model_slot: Option<String> = None;
    let text_events = parse_ollama_line(
        r#"{"model":"qwen3:0.6b","message":{"content":"after-load"}}"#,
        &mut server_model_slot,
    )
    .expect("content chunk parses");
    assert_eq!(text_events.len(), 1);
    assert_eq!(
        text_events[0],
        LlmEvent::TextDelta("after-load".to_string())
    );

    let terminal_events = parse_ollama_line(
        r#"{"model":"qwen3:0.6b","done":true,"done_reason":"stop","prompt_eval_count":3,"eval_count":4}"#,
        &mut server_model_slot,
    )
    .expect("terminal stop frame parses");
    assert!(matches!(
        terminal_events.last(),
        Some(LlmEvent::Completed {
            stop_reason: Some(crate::StopReason::EndTurn),
            ..
        })
    ));
}

#[test]
fn show_metadata_extracts_context_window_from_model_info() {
    let value = json!({
        "model_info": {
            "qwen3.context_length": 32768,
            "qwen3.embedding_length": 4096
        }
    });

    assert_eq!(ollama_context_window_from_show(&value), Some(32_768));
}

#[test]
fn show_metadata_extracts_context_window_from_parameters_fallback() {
    let value = json!({
        "parameters": "temperature 0.7\nnum_ctx 8192\n"
    });

    assert_eq!(ollama_context_window_from_show(&value), Some(8_192));
}

#[test]
fn show_metadata_parses_quoted_num_ctx_fallback() {
    // Some Modelfile parameter strings quote the value (`num_ctx "8192"`)
    // — the fallback must strip the quotes before parsing.
    let value = json!({
        "parameters": "temperature 0.7\nnum_ctx \"16384\"\n"
    });
    assert_eq!(ollama_context_window_from_show(&value), Some(16_384));

    let value = json!({
        "parameters": "num_ctx '4096'\n"
    });
    assert_eq!(ollama_context_window_from_show(&value), Some(4_096));
}

#[test]
fn show_metadata_extracts_capabilities_array() {
    let value = json!({
        "capabilities": ["completion", "tools", "thinking", "vision"]
    });
    assert_eq!(
        ollama_capabilities_from_show(&value),
        Some(vec![
            "completion".to_string(),
            "tools".to_string(),
            "thinking".to_string(),
            "vision".to_string(),
        ])
    );

    // Missing capabilities field: helper returns `None` so callers treat as
    // "unknown" rather than "no capabilities".
    let value = json!({});
    assert_eq!(ollama_capabilities_from_show(&value), None);
}

#[test]
fn tags_metadata_extracts_installed_model_names() {
    let value = json!({
        "models": [
            {"name": "qwen3-coder:latest"},
            {"name": "llama3.3:70b"},
            {"missing": "name"}
        ]
    });

    assert_eq!(
        ollama_model_names_from_tags(&value),
        vec!["qwen3-coder:latest", "llama3.3:70b"]
    );
}

#[test]
fn pull_parser_maps_status_progress_and_success_lines() {
    let status = parse_pull_line(r#"{"status":"pulling manifest"}"#)
        .expect("status line parses")
        .expect("status emits an event");
    assert_eq!(status, PullEvent::Status("pulling manifest".to_string()));

    let progress = parse_pull_line(
        r#"{"status":"downloading","digest":"sha256:abc","total":1000,"completed":250}"#,
    )
    .expect("progress line parses")
    .expect("progress emits an event");
    assert_eq!(
        progress,
        PullEvent::Progress {
            digest: "sha256:abc".to_string(),
            total: Some(1000),
            completed: Some(250),
        }
    );

    let progress_partial = parse_pull_line(r#"{"digest":"sha256:abc","total":1000}"#)
        .expect("partial progress parses")
        .expect("emits a progress event");
    assert_eq!(
        progress_partial,
        PullEvent::Progress {
            digest: "sha256:abc".to_string(),
            total: Some(1000),
            completed: None,
        }
    );

    let success = parse_pull_line(r#"{"status":"success"}"#)
        .expect("success parses")
        .expect("emits an event");
    assert_eq!(success, PullEvent::Success);
}

#[test]
fn pull_parser_surfaces_server_errors_as_stream_errors() {
    let err = parse_pull_line(r#"{"error":"pull model manifest: file does not exist"}"#)
        .expect_err("server error surfaces");
    let SqueezyError::ProviderStream(message) = err else {
        panic!("expected ProviderStream");
    };
    assert!(
        message.contains("file does not exist"),
        "got message: {message}"
    );
}

#[test]
fn pull_parser_ignores_empty_keepalive_frames() {
    let empty = parse_pull_line(r#"{}"#).expect("empty frame parses");
    assert!(empty.is_none(), "empty frame emits no event");
}

#[test]
fn pull_parser_rejects_invalid_json() {
    let err = parse_pull_line("not-json").expect_err("invalid JSON surfaces");
    assert!(matches!(err, SqueezyError::ProviderStream(_)));
}

#[test]
fn host_root_normalizes_all_input_shapes() {
    assert_eq!(
        ollama_host_root("http://localhost:11434"),
        "http://localhost:11434"
    );
    assert_eq!(
        ollama_host_root("http://localhost:11434/"),
        "http://localhost:11434"
    );
    assert_eq!(
        ollama_host_root("http://localhost:11434/api"),
        "http://localhost:11434"
    );
    assert_eq!(
        ollama_host_root("http://localhost:11434/api/"),
        "http://localhost:11434"
    );
    assert_eq!(
        ollama_host_root("http://localhost:11434/v1"),
        "http://localhost:11434"
    );
    assert_eq!(
        ollama_host_root("http://localhost:11434/v1/"),
        "http://localhost:11434"
    );
}

#[test]
fn api_endpoint_url_always_includes_api_segment() {
    for base in [
        "http://localhost:11434",
        "http://localhost:11434/",
        "http://localhost:11434/api",
        "http://localhost:11434/api/",
        "http://localhost:11434/v1",
        "http://localhost:11434/v1/",
    ] {
        for endpoint in ["chat", "show", "pull", "tags"] {
            assert_eq!(
                api_endpoint_url(base, endpoint),
                format!("http://localhost:11434/api/{endpoint}"),
                "base={base} endpoint={endpoint}",
            );
            assert_eq!(
                api_endpoint_url(base, &format!("/{endpoint}")),
                format!("http://localhost:11434/api/{endpoint}"),
                "leading-slash endpoint, base={base}",
            );
        }
    }
}

#[test]
fn openai_compat_base_url_swaps_api_for_v1() {
    assert_eq!(
        openai_compat_base_url("http://localhost:11434/api"),
        "http://localhost:11434/v1"
    );
}

#[test]
fn openai_compat_base_url_appends_v1_when_unsuffixed() {
    assert_eq!(
        openai_compat_base_url("http://localhost:11434"),
        "http://localhost:11434/v1"
    );
}

#[test]
fn openai_compat_base_url_preserves_existing_v1() {
    assert_eq!(
        openai_compat_base_url("http://localhost:11434/v1/"),
        "http://localhost:11434/v1"
    );
}

#[test]
fn route_style_compat_builds_lmstudio_delegate() {
    let provider = OllamaProvider::from_config(&squeezy_core::OllamaConfig {
        base_url: "http://localhost:11434/api".to_string(),
        route_style: squeezy_core::OllamaRoute::OpenAiCompatible,
        transport: squeezy_core::ProviderTransportConfig::default(),
    });
    assert!(
        provider.compat.is_some(),
        "OpenAiCompatible route must instantiate the LM Studio delegate",
    );
}

#[test]
fn route_style_native_leaves_compat_delegate_unset() {
    let provider = OllamaProvider::from_config(&squeezy_core::OllamaConfig {
        base_url: "http://localhost:11434/api".to_string(),
        route_style: squeezy_core::OllamaRoute::Native,
        transport: squeezy_core::ProviderTransportConfig::default(),
    });
    assert!(
        provider.compat.is_none(),
        "Native route must keep the proprietary /api/chat path",
    );
}

#[test]
fn ollama_route_parse_recognises_canonical_aliases() {
    use squeezy_core::OllamaRoute;
    assert_eq!(OllamaRoute::parse("native"), Some(OllamaRoute::Native));
    assert_eq!(
        OllamaRoute::parse("openai_compatible"),
        Some(OllamaRoute::OpenAiCompatible)
    );
    assert_eq!(
        OllamaRoute::parse("v1"),
        Some(OllamaRoute::OpenAiCompatible)
    );
    assert_eq!(OllamaRoute::parse("nope"), None);
}

#[test]
fn request_body_emits_image_in_native_images_array() {
    let bytes: Arc<[u8]> = Arc::from(vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    let request = LlmRequest {
        model: "llava".to_string().into(),
        instructions: "be brief".to_string().into(),
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

    let body = OllamaProvider::request_body(&request);
    let messages = body["messages"].as_array().expect("messages array");
    // system + user(text + image) — image attaches to the preceding
    // user-text turn so vision models pair them as the same prompt.
    assert_eq!(messages.len(), 2);
    let prompt_msg = &messages[1];
    assert_eq!(prompt_msg["role"], "user");
    assert_eq!(prompt_msg["content"], "what is this?");
    let images = prompt_msg["images"].as_array().expect("images array");
    assert_eq!(images.len(), 1);
    use base64::Engine as _;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(images[0].as_str().expect("base64 string"))
        .expect("valid base64");
    assert_eq!(decoded.as_slice(), bytes.as_ref());
}

#[test]
fn request_body_image_first_falls_back_to_standalone_user_message() {
    // When there is no preceding user-text message to attach the image
    // to, the helper still emits a standalone image-only user turn so
    // bare-image inputs round-trip.
    let bytes: Arc<[u8]> = Arc::from(vec![0x89, b'P', b'N', b'G']);
    let request = LlmRequest {
        model: "llava".to_string().into(),
        instructions: String::new().into(),
        input: Arc::from(vec![LlmInputItem::Image {
            media_type: "image/png".to_string(),
            bytes,
        }]),
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
    let body = OllamaProvider::request_body(&request);
    let messages = body["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"], "");
    assert!(messages[0]["images"].is_array());
}
