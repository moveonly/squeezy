use serde_json::json;
use std::sync::Arc;

use super::*;
use crate::{LlmInputItem, LlmToolSpec};

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
    assert_eq!(body["tools"][0]["function"]["name"], "grep");
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
    let events = parse_ollama_line(
        r#"{"message":{"content":"hi","tool_calls":[{"function":{"name":"grep","arguments":{"pattern":"needle"}}}]},"done":true,"prompt_eval_count":10,"eval_count":2}"#,
    )
    .expect("valid event");

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
