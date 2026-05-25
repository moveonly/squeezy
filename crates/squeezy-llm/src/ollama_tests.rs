use serde_json::json;

use super::*;
use crate::{LlmInputItem, LlmToolSpec};

#[test]
fn request_body_uses_chat_stream_shape() {
    let request = LlmRequest {
        model: "qwen3".to_string(),
        instructions: "be brief".to_string(),
        input: vec![LlmInputItem::UserText("hello".to_string())],
        max_output_tokens: Some(16),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        tools: vec![LlmToolSpec {
            name: "grep".to_string(),
            description: "search".to_string(),
            parameters: json!({"type": "object"}),
            strict: true,
        }],
        store: false,
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
        model: "qwen3".to_string(),
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
