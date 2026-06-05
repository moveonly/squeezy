use super::*;
use crate::anthropic_betas::{anthropic_header_value, bedrock_extra_body_betas};
use crate::{CacheSpec, LlmInputItem, LlmToolCall, LlmToolSpec};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

static ANTHROPIC_OAUTH_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn temp_home(label: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "squeezy-anthropic-provider-{label}-{}-{nanos}",
        std::process::id()
    ))
}

#[test]
fn request_body_uses_messages_streaming_shape() {
    let request = LlmRequest {
        model: "claude-test".to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        max_output_tokens: Some(32),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: Some("ignored".to_string()),
        cache_key: None,
        cache: CacheSpec::default(),
        tools: Arc::from(vec![
            LlmToolSpec {
                name: "read_file".to_string(),
                description: "Read a file".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" }
                    },
                    "required": ["path"]
                }),
                strict: true,
            }
            .into(),
        ]),
        store: true,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };

    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);

    assert_eq!(body["model"], "claude-test");
    assert_eq!(body["system"], "be brief");
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["messages"][0]["content"][0]["type"], "text");
    assert_eq!(body["messages"][0]["content"][0]["text"], "hello");
    assert_eq!(body["tools"][0]["name"], "read_file");
    assert_eq!(body["tools"][0]["input_schema"]["required"][0], "path");
    assert!(body["tools"][0].get("strict").is_none());
    assert_eq!(body["max_tokens"], 32);
    assert_eq!(body["stream"], true);
    assert!(body.get("previous_response_id").is_none());
    assert!(body.get("store").is_none());
}

#[tokio::test]
async fn from_config_loads_oauth_when_static_key_missing() {
    let home = temp_home("oauth-load");
    let auth_dir = home.join(".squeezy").join("auth");
    std::fs::create_dir_all(&auth_dir).expect("create auth dir");
    let auth_path = auth_dir.join("anthropic.json");
    let tokens = crate::oauth::PersistedTokens {
        access_token: "sk-ant-oat-provider-test".to_string(),
        refresh_token: "sk-ant-rfr-provider-test".to_string(),
        expires_at_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0)
            + 3_600_000,
        scope: None,
        provider: "anthropic-oauth".to_string(),
    };
    crate::oauth::anthropic_write_tokens(&auth_path, &tokens).expect("write oauth token file");

    let provider = {
        let _guard = ANTHROPIC_OAUTH_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let prev_home = std::env::var_os("HOME");
        let missing_env = "SQUEEZY_TEST_MISSING_ANTHROPIC_KEY";
        let prev_missing = std::env::var_os(missing_env);
        // SAFETY: ANTHROPIC_OAUTH_ENV_LOCK serializes process-env mutations in
        // this module.
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::remove_var(missing_env);
        }

        let config = AnthropicConfig {
            api_key: None,
            api_key_env: missing_env.to_string(),
            base_url: squeezy_core::DEFAULT_ANTHROPIC_BASE_URL.to_string(),
            transport: squeezy_core::ProviderTransportConfig::default(),
        };
        let provider = AnthropicProvider::from_config(&config).expect("oauth provider");

        unsafe {
            match prev_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match prev_missing {
                Some(value) => std::env::set_var(missing_env, value),
                None => std::env::remove_var(missing_env),
            }
        }

        provider
    };
    let key = provider.api_key.current_key().await.expect("oauth key");
    assert_eq!(key, "sk-ant-oat-provider-test");
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn request_body_preserves_function_tool_order() {
    let request = LlmRequest {
        model: "claude-test".to_string().into(),
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
                parameters: serde_json::json!({"type": "object"}),
                strict: true,
            }
            .into(),
            LlmToolSpec {
                name: "grep".to_string(),
                description: "search".to_string(),
                parameters: serde_json::json!({"type": "object"}),
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

    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);

    assert_eq!(body["tools"][0]["name"], "write_file");
    assert_eq!(body["tools"][1]["name"], "grep");
}

#[test]
fn request_body_uses_model_limit_when_output_cap_unset() {
    let request = LlmRequest {
        model: squeezy_core::DEFAULT_ANTHROPIC_MODEL.to_string().into(),
        instructions: "be brief".to_string().into(),
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
        ..LlmRequest::default()
    };

    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);

    assert_eq!(body["max_tokens"], 64_000);
}

#[test]
fn request_body_maps_tool_roundtrip_messages() {
    let request = LlmRequest {
        model: "claude-test".to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![
            LlmInputItem::UserText("read config".to_string()),
            LlmInputItem::FunctionCall {
                call_id: "toolu_1".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({ "path": "squeezy.toml" }),
            },
            LlmInputItem::FunctionCallOutput {
                call_id: "toolu_1".to_string(),
                output: "model = 'haiku'".to_string(),
                content_parts: None,
                is_error: false,
            },
        ]),
        max_output_tokens: Some(32),
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

    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);

    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["messages"][0]["content"][0]["text"], "read config");
    assert_eq!(body["messages"][1]["role"], "assistant");
    assert_eq!(body["messages"][1]["content"][0]["type"], "tool_use");
    // Tool-call ids get canonicalized to `call_<N>` (1-indexed by
    // first occurrence) so a mid-session model switch can replay
    // them on any provider — the original `toolu_…` shape would
    // be rejected by OpenAI/Google, and a 450-char OpenAI id would
    // be rejected by Anthropic.
    assert_eq!(body["messages"][1]["content"][0]["id"], "call_1");
    assert_eq!(
        body["messages"][1]["content"][0]["input"]["path"],
        "squeezy.toml"
    );
    assert_eq!(body["messages"][2]["role"], "user");
    assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
    assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "call_1");
}

#[test]
fn request_body_adds_cache_control_markers_when_cache_key_and_capability_enable_caching() {
    // Use a real Anthropic model id so the registry capability lookup
    // reports prompt_caching=true; pair it with a cache_key so the
    // anthropic adapter inserts ephemeral cache markers.
    let request = LlmRequest {
        model: squeezy_core::DEFAULT_ANTHROPIC_MODEL.to_string().into(),
        instructions: "system prompt".to_string().into(),
        input: Arc::from(vec![
            LlmInputItem::UserText("first turn".to_string()),
            LlmInputItem::AssistantText("ack".to_string()),
            LlmInputItem::UserText("second turn".to_string()),
        ]),
        max_output_tokens: Some(32),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: Some("squeezy::session-1".to_string()),
        cache: CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };

    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);

    // System tail carries an ephemeral cache_control marker.
    assert_eq!(body["system"][0]["type"], "text");
    assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");

    // Only the last user block (the second turn) gets the cache marker.
    let messages = body["messages"].as_array().expect("messages array");
    let last_user = messages
        .iter()
        .rev()
        .find(|msg| msg["role"] == "user")
        .expect("user message");
    let last_block = last_user["content"]
        .as_array()
        .expect("content")
        .last()
        .expect("last block");
    assert_eq!(last_block["cache_control"]["type"], "ephemeral");

    // Older user turn is not marked.
    let first_user = messages
        .iter()
        .find(|msg| msg["role"] == "user")
        .expect("first user message");
    let first_block = &first_user["content"][0];
    assert!(first_block.get("cache_control").is_none());
}

#[test]
fn request_body_omits_all_cache_control_when_prompt_cache_disabled() {
    // Identical setup to the marker-insertion test (real model + cache_key,
    // which normally force ephemeral markers), but with the hard off-switch
    // set: not a single cache_control marker may appear anywhere in the body.
    let request = LlmRequest {
        model: squeezy_core::DEFAULT_ANTHROPIC_MODEL.to_string().into(),
        instructions: "system prompt".to_string().into(),
        input: Arc::from(vec![
            LlmInputItem::UserText("first turn".to_string()),
            LlmInputItem::AssistantText("ack".to_string()),
            LlmInputItem::UserText("second turn".to_string()),
        ]),
        max_output_tokens: Some(32),
        cache_key: Some("squeezy::session-1".to_string()),
        cache: CacheSpec::default(),
        disable_prompt_cache: true,
        ..LlmRequest::default()
    };

    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);
    let serialized = serde_json::to_string(&body).expect("serialize");
    assert!(
        !serialized.contains("cache_control"),
        "disable_prompt_cache must suppress every cache_control marker: {serialized}"
    );
}

#[test]
fn request_body_marks_last_tool_with_cache_control_when_caching_enabled() {
    let request = LlmRequest {
        model: squeezy_core::DEFAULT_ANTHROPIC_MODEL.to_string().into(),
        instructions: "system prompt".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
        max_output_tokens: Some(32),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: Some("squeezy::session-1".to_string()),
        cache: CacheSpec::default(),
        tools: Arc::from(vec![
            LlmToolSpec {
                name: "tool_a".to_string(),
                description: "first".to_string(),
                parameters: serde_json::json!({"type": "object"}),
                strict: false,
            }
            .into(),
            LlmToolSpec {
                name: "tool_b".to_string(),
                description: "second".to_string(),
                parameters: serde_json::json!({"type": "object"}),
                strict: false,
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

    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);
    let tools = body["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 2);
    assert!(
        tools[0].get("cache_control").is_none(),
        "earlier tool must not carry a cache breakpoint"
    );
    assert_eq!(tools[1]["cache_control"]["type"], "ephemeral");
}

#[test]
fn request_body_places_tool_cache_control_on_last_first_party_tool_when_mcp_tools_trail() {
    // Two first-party tools followed by two MCP tools (the partition the
    // tool registry enforces). The breakpoint must sit on the last first
    // -party tool so the cached prefix survives an MCP `tools/list`
    // refresh that mutates only the trailing MCP block.
    let request = LlmRequest {
        model: squeezy_core::DEFAULT_ANTHROPIC_MODEL.to_string().into(),
        instructions: "system prompt".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
        max_output_tokens: Some(32),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: Some("squeezy::session-1".to_string()),
        cache: CacheSpec::default(),
        tools: Arc::from(vec![
            LlmToolSpec {
                name: "apply_patch".to_string(),
                description: "edit".to_string(),
                parameters: serde_json::json!({"type": "object"}),
                strict: false,
            }
            .into(),
            LlmToolSpec {
                name: "read_file".to_string(),
                description: "read".to_string(),
                parameters: serde_json::json!({"type": "object"}),
                strict: false,
            }
            .into(),
            LlmToolSpec {
                name: "mcp__linear__create_issue".to_string(),
                description: "create issue".to_string(),
                parameters: serde_json::json!({"type": "object"}),
                strict: false,
            }
            .into(),
            LlmToolSpec {
                name: "mcp__linear__list_issues".to_string(),
                description: "list issues".to_string(),
                parameters: serde_json::json!({"type": "object"}),
                strict: false,
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

    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);
    let tools = body["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 4);
    assert!(tools[0].get("cache_control").is_none());
    assert_eq!(
        tools[1]["cache_control"]["type"], "ephemeral",
        "breakpoint must sit on the last first-party tool"
    );
    assert!(tools[2].get("cache_control").is_none());
    assert!(tools[3].get("cache_control").is_none());
}

#[test]
fn request_body_falls_back_to_last_tool_when_all_advertised_tools_are_mcp() {
    // Edge case: only MCP tools are advertised (no stable first-party
    // prefix to anchor). Fall back to the unconditional last tool so
    // caching is still attempted — losing the cache on every MCP refresh
    // is no worse than the pre-change behavior, but caching the prefix
    // when MCP tools are stable for many turns is still a win.
    let request = LlmRequest {
        model: squeezy_core::DEFAULT_ANTHROPIC_MODEL.to_string().into(),
        instructions: "system prompt".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
        max_output_tokens: Some(32),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: Some("squeezy::session-1".to_string()),
        cache: CacheSpec::default(),
        tools: Arc::from(vec![
            LlmToolSpec {
                name: "mcp__linear__create_issue".to_string(),
                description: "create issue".to_string(),
                parameters: serde_json::json!({"type": "object"}),
                strict: false,
            }
            .into(),
            LlmToolSpec {
                name: "mcp__linear__list_issues".to_string(),
                description: "list issues".to_string(),
                parameters: serde_json::json!({"type": "object"}),
                strict: false,
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

    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);
    let tools = body["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 2);
    assert!(tools[0].get("cache_control").is_none());
    assert_eq!(tools[1]["cache_control"]["type"], "ephemeral");
}

#[test]
fn request_body_omits_tool_cache_control_when_caching_disabled() {
    let request = LlmRequest {
        model: squeezy_core::DEFAULT_ANTHROPIC_MODEL.to_string().into(),
        instructions: "system".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
        max_output_tokens: Some(32),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        cache: CacheSpec::default(),
        tools: Arc::from(vec![
            LlmToolSpec {
                name: "tool_a".to_string(),
                description: "first".to_string(),
                parameters: serde_json::json!({"type": "object"}),
                strict: false,
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

    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);
    let tools = body["tools"].as_array().expect("tools array");
    assert!(
        tools[0].get("cache_control").is_none(),
        "cache breakpoint must not be emitted without a cache_key"
    );
}

#[test]
fn request_body_emits_one_hour_ttl_marker_for_long_retention() {
    // F11: `CacheRetention::Long` must surface on the Anthropic Messages
    // wire as `cache_control: { type: "ephemeral", ttl: "1h" }` on every
    // breakpoint (system, last user, last stable tool) so the cached
    // prefix survives Anthropic's default ~5m short window.
    let request = LlmRequest {
        model: squeezy_core::DEFAULT_ANTHROPIC_MODEL.to_string().into(),
        instructions: "system prompt".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("first turn".to_string())]),
        max_output_tokens: Some(32),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        cache: crate::CacheSpec {
            key: Some("squeezy::session-long".to_string()),
            retention: crate::CacheRetention::Long,
        },
        tools: Arc::from(vec![
            LlmToolSpec {
                name: "tool_a".to_string(),
                description: "first".to_string(),
                parameters: serde_json::json!({"type": "object"}),
                strict: false,
            }
            .into(),
            LlmToolSpec {
                name: "tool_b".to_string(),
                description: "second".to_string(),
                parameters: serde_json::json!({"type": "object"}),
                strict: false,
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

    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);

    // System tail: `ttl: "1h"` rides alongside the `ephemeral` marker.
    assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(
        body["system"][0]["cache_control"]["ttl"], "1h",
        "Long retention must extend Anthropic's cached prefix lifetime via ttl=1h"
    );

    // Last user block: same marker shape.
    let messages = body["messages"].as_array().expect("messages array");
    let last_user = messages
        .iter()
        .rev()
        .find(|msg| msg["role"] == "user")
        .expect("user message");
    let last_block = last_user["content"]
        .as_array()
        .expect("content")
        .last()
        .expect("last block");
    assert_eq!(last_block["cache_control"]["ttl"], "1h");

    // Last stable tool: same marker shape.
    let tools = body["tools"].as_array().expect("tools array");
    assert_eq!(tools[1]["cache_control"]["ttl"], "1h");
    assert!(tools[0].get("cache_control").is_none());
}

#[test]
fn request_body_omits_ttl_for_short_retention_via_legacy_cache_key() {
    // Regression guard for the `From<Option<String>>` lift in
    // `effective_cache_spec()`: setting only the deprecated `cache_key`
    // must yield Short retention, leaving the marker bare so Anthropic
    // applies its built-in short window (~5m).
    let request = LlmRequest {
        model: squeezy_core::DEFAULT_ANTHROPIC_MODEL.to_string().into(),
        instructions: "system prompt".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
        max_output_tokens: Some(32),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: Some("squeezy::session-1".to_string()),
        cache: CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };

    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);
    assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    assert!(
        body["system"][0]["cache_control"].get("ttl").is_none(),
        "Short retention (from the legacy cache_key migration path) must not emit a ttl field"
    );
}

#[test]
fn request_body_skips_cache_control_when_cache_key_is_absent() {
    let request = LlmRequest {
        model: squeezy_core::DEFAULT_ANTHROPIC_MODEL.to_string().into(),
        instructions: "system".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        max_output_tokens: Some(32),
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

    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);

    // Without a cache_key the system field stays a plain string and no
    // cache_control markers appear in messages.
    assert_eq!(body["system"], "system");
    let messages = body["messages"].as_array().expect("messages");
    for message in messages {
        for block in message["content"].as_array().expect("blocks") {
            assert!(block.get("cache_control").is_none(), "{block}");
        }
    }
}

/// Anthropic rejects `thinking.budget_tokens < 1024`. When the caller
/// sets a small `max_output_tokens` (here 256), the adapter must
/// suppress the `thinking` block entirely rather than emit a sub-floor
/// budget that would 400 every turn.
#[test]
fn request_body_omits_thinking_when_max_output_tokens_below_1024_floor() {
    // Use a non-adaptive model: the budget-floor logic only runs on the
    // explicit-budget path; sonnet/opus 4.6+ go through adaptive thinking
    // (no budget_tokens, no floor) and would skip this assertion.
    let request = LlmRequest {
        model: "claude-haiku-4-5-20251001".to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        max_output_tokens: Some(256),
        response_verbosity: None,
        reasoning_effort: Some(squeezy_core::ReasoningEffort::Low),
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

    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);

    assert_eq!(body["max_tokens"], 256);
    assert!(
        body.get("thinking").is_none(),
        "thinking block must be omitted when max_output_tokens cannot satisfy the 1024 \
         budget floor + reply headroom, got {body}",
    );
}

/// When `max_output_tokens` clears the 1024-budget + 1024-reply floor,
/// the `thinking.budget_tokens` we emit must (1) be `>= 1024`,
/// (2) leave room for the reply, and (3) honor the requested effort
/// when the headroom allows it.
#[test]
fn request_body_emits_thinking_clamped_to_max_output_minus_reply_headroom() {
    let request = LlmRequest {
        // Non-adaptive model so the explicit-budget clamp logic runs.
        model: "claude-haiku-4-5-20251001".to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        // Tight output cap: clears the 2048 gate but smaller than the
        // Low effort budget (4096). The emitted budget should be
        // clamped down to `max_output_tokens - 1024 = 1024`.
        max_output_tokens: Some(2_048),
        response_verbosity: None,
        reasoning_effort: Some(squeezy_core::ReasoningEffort::Low),
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

    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);

    assert_eq!(body["max_tokens"], 2_048);
    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(body["thinking"]["budget_tokens"], 1_024);

    // Ample output cap: emitted budget should match the effort default.
    let request = LlmRequest {
        max_output_tokens: Some(32_000),
        ..request
    };
    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);
    assert_eq!(body["thinking"]["budget_tokens"], 4_096);
}

#[test]
fn model_uses_adaptive_thinking_covers_4_6_and_later_opus_and_sonnet() {
    // 4.6+ opus/sonnet must opt in, including unreleased versions
    // (forward-compat: a new opus-4-8 / sonnet-5-0 shouldn't need a
    // squeezy update to pick the right wire shape).
    for model in [
        "claude-opus-4-6",
        "claude-opus-4-7",
        "claude-opus-4-8",
        "claude-opus-5-0",
        "claude-sonnet-4-6",
        "claude-sonnet-5-2",
        "anthropic/claude-opus-4-7",
    ] {
        assert!(model_uses_adaptive_thinking(model), "{model}");
    }

    // Pre-4.6 opus/sonnet and haiku (any version) must keep the
    // explicit-budget shape.
    for model in [
        "claude-opus-4-5",
        "claude-opus-3-5",
        "claude-sonnet-4-5",
        "claude-sonnet-3-7",
        "claude-haiku-4-5-20251001",
        "claude-haiku-5-0",
    ] {
        assert!(!model_uses_adaptive_thinking(model), "{model}");
    }
}

/// H-04: the detection heuristic must not fire for non-Claude model
/// ids that happen to contain the `opus-N-M` / `sonnet-N-M` substring
/// (a custom proxy literal like `opus-4-7`, a third-party model named
/// `myorg/opus-4-7`, or any future aggregator that uses the same
/// family tag for a non-Anthropic model). Aggregator routes that DO
/// wrap a real Claude model (Vertex region tag `@001`, OpenRouter
/// route tag `:nitro`, OpenRouter prefix `anthropic/`) must still
/// activate adaptive thinking so the OAuth quota path keeps working.
#[test]
fn model_uses_adaptive_thinking_requires_claude_prefix_and_anchors_version_segment() {
    // Non-Claude proxies and lookalikes: must NOT activate adaptive
    // thinking.
    for model in [
        "opus-4-7",
        "opus-4-7-instruct",
        "myorg/opus-4-7",
        "sonnet-5-0",
        "anthropic/opus-4-7",
        "vertex/anthropic/opus-4-7",
    ] {
        assert!(
            !model_uses_adaptive_thinking(model),
            "non-Claude id `{model}` must not activate adaptive thinking",
        );
    }

    // Aggregator wrappers around real Claude models: MUST activate.
    for model in [
        "anthropic/claude-opus-4-7",
        "anthropic/claude-opus-4-7:nitro",
        "vertex/anthropic/claude-opus-4-7@001",
        "openrouter/anthropic/claude-sonnet-4-6:beta",
    ] {
        assert!(
            model_uses_adaptive_thinking(model),
            "aggregator wrapper `{model}` must still activate adaptive thinking",
        );
    }
}

#[test]
fn request_body_uses_adaptive_thinking_for_claude_4_6_and_4_7() {
    use squeezy_core::ReasoningEffort;

    for (model, effort, label) in [
        ("claude-opus-4-7", ReasoningEffort::High, "high"),
        ("claude-sonnet-4-6", ReasoningEffort::Medium, "medium"),
        ("claude-opus-4-7", ReasoningEffort::XHigh, "max"),
    ] {
        let request = LlmRequest {
            model: model.to_string().into(),
            instructions: "be brief".to_string().into(),
            input: Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
            max_output_tokens: Some(32_000),
            response_verbosity: None,
            reasoning_effort: Some(effort),
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

        let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);

        assert_eq!(body["thinking"]["type"], "adaptive", "{model} {effort:?}");
        assert!(
            body["thinking"].get("budget_tokens").is_none(),
            "adaptive thinking must not carry budget_tokens ({model})"
        );
        assert_eq!(body["output_config"]["effort"], label, "{model} {effort:?}");
    }
}

#[test]
fn request_body_keeps_enabled_thinking_for_legacy_anthropic_models() {
    use squeezy_core::ReasoningEffort;

    let request = LlmRequest {
        model: "claude-haiku-4-5-20251001".to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
        max_output_tokens: Some(64_000),
        response_verbosity: None,
        reasoning_effort: Some(ReasoningEffort::Medium),
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

    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);

    assert_eq!(body["thinking"]["type"], "enabled");
    assert!(body["thinking"]["budget_tokens"].is_number());
    assert!(body.get("output_config").is_none());
}

#[test]
fn sse_decoder_collects_data_events_across_chunks() {
    let mut decoder = SseDecoder::default();

    assert!(
        decoder
            .push(b"event: content_block_delta\ndata: {\"type\":\"content_")
            .is_empty()
    );
    let events =
        decoder.push(b"block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n");

    assert_eq!(
        events,
        vec![r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"hi"}}"#]
    );
}

#[test]
fn parser_extracts_text_delta() {
    let mut state = AnthropicStreamState::default();
    let event = parse_anthropic_event(
        r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"hello"}}"#,
        &mut state,
    )
    .expect("valid event");

    assert_eq!(event, vec![LlmEvent::TextDelta("hello".to_string())]);
}

#[test]
fn parser_extracts_streamed_tool_call() {
    let mut state = AnthropicStreamState::default();

    parse_anthropic_event(
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"read_file","input":{}}}"#,
        &mut state,
    )
    .expect("start");
    parse_anthropic_event(
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"src/"}}"#,
        &mut state,
    )
    .expect("delta");
    parse_anthropic_event(
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"lib.rs\"}"}}"#,
        &mut state,
    )
    .expect("delta");
    let event = parse_anthropic_event(r#"{"type":"content_block_stop","index":1}"#, &mut state)
        .expect("stop");

    assert_eq!(
        event,
        vec![LlmEvent::ToolCall(LlmToolCall {
            call_id: "toolu_1".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::json!({ "path": "src/lib.rs" }),
        })]
    );
}

#[test]
fn parser_extracts_completed_response_id_and_usage() {
    let mut state = AnthropicStreamState::default();

    parse_anthropic_event(
        r#"{
          "type":"message_start",
          "message":{
            "id":"msg_123",
            "usage":{
              "input_tokens":10,
              "output_tokens":1,
              "cache_read_input_tokens":3
            }
          }
        }"#,
        &mut state,
    )
    .expect("start");
    parse_anthropic_event(
        r#"{
          "type":"message_delta",
          "delta":{"stop_reason":"end_turn"},
          "usage":{"output_tokens":4}
        }"#,
        &mut state,
    )
    .expect("delta");
    let event = parse_anthropic_event(r#"{"type":"message_stop"}"#, &mut state).expect("stop");

    assert_eq!(
        event,
        vec![LlmEvent::Completed {
            response_id: Some("msg_123".to_string()),
            cost: CostSnapshot {
                // Normalised across providers: total prompt the model saw
                // = 10 uncached + 3 cache read = 13. The breakdown lives
                // in `cached_input_tokens`.
                input_tokens: Some(13),
                output_tokens: Some(4),
                reasoning_output_tokens: None,
                cached_input_tokens: Some(3),
                cache_write_input_tokens: None,
                estimated_usd_micros: None,
            },
            stop_reason: Some(crate::StopReason::EndTurn),
            reasoning_only_stop: false,
        }]
    );
}

#[test]
fn parser_populates_both_cache_counters_from_usage() {
    let mut state = AnthropicStreamState::default();

    parse_anthropic_event(
        r#"{
          "type":"message_start",
          "message":{
            "id":"msg_cache",
            "usage":{
              "input_tokens":42,
              "output_tokens":1,
              "cache_read_input_tokens":17,
              "cache_creation_input_tokens":29
            }
          }
        }"#,
        &mut state,
    )
    .expect("start");
    parse_anthropic_event(
        r#"{
          "type":"message_delta",
          "delta":{"stop_reason":"end_turn"},
          "usage":{"output_tokens":8}
        }"#,
        &mut state,
    )
    .expect("delta");
    let event = parse_anthropic_event(r#"{"type":"message_stop"}"#, &mut state).expect("stop");

    assert_eq!(
        event,
        vec![LlmEvent::Completed {
            response_id: Some("msg_cache".to_string()),
            cost: CostSnapshot {
                // Normalised total = 42 uncached + 17 cache read + 29
                // cache creation = 88. Standard rate is paid only on
                // the uncached 42; cache_read + cache_write are billed
                // at their own pricing tiers in `estimate_cost`.
                input_tokens: Some(88),
                output_tokens: Some(8),
                reasoning_output_tokens: None,
                cached_input_tokens: Some(17),
                cache_write_input_tokens: Some(29),
                estimated_usd_micros: None,
            },
            stop_reason: Some(crate::StopReason::EndTurn),
            reasoning_only_stop: false,
        }]
    );
}

#[test]
fn parser_surfaces_error_events() {
    let mut state = AnthropicStreamState::default();
    let err = parse_anthropic_event(
        r#"{"type":"error","error":{"message":"bad request"}}"#,
        &mut state,
    )
    .expect_err("stream error");

    // Default (unknown) error type stays on the ProviderStream
    // path so the retry layer still attempts a reconnect for
    // genuinely transient transport shapes.
    assert!(matches!(err, squeezy_core::SqueezyError::ProviderStream(_)));
    assert!(err.to_string().contains("bad request"));
    // No overflow signal latched for an unrecognised error shape.
    assert!(state.pending_overflow_signal.is_none());
}

/// C-01: a mid-stream `event: error` carrying
/// `model_context_window_exceeded` must (a) latch an additive
/// `ContextOverflow` signal on the stream state and (b) surface
/// as a non-retryable `ProviderRequest` so the retry layer doesn't
/// reconnect against an immutable failure.
#[test]
fn parser_classifies_mid_stream_context_window_exceeded_as_non_retryable() {
    let mut state = AnthropicStreamState::default();
    let err = parse_anthropic_event(
        r#"{
          "type":"error",
          "error":{
            "type":"model_context_window_exceeded",
            "message":"prompt is too long for this model"
          }
        }"#,
        &mut state,
    )
    .expect_err("error event surfaces error");
    assert!(
        matches!(err, squeezy_core::SqueezyError::ProviderRequest(_)),
        "context-window errors must surface as ProviderRequest, got {err:?}",
    );
    assert!(
        err.to_string()
            .contains(crate::anthropic_error::NON_RETRYABLE_MARKER),
        "context-window errors must carry the non-retryable marker, got {}",
        err
    );
    let signal = state
        .pending_overflow_signal
        .take()
        .expect("overflow signal must be latched for context-window errors");
    assert!(matches!(
        signal,
        crate::overflow::OverflowSignal::ErrorPattern(_)
    ));
}

/// C-01: `overloaded_error` and `rate_limit_error` mid-stream errors
/// must surface as `ProviderRequest` (so the pre/post-200 paths use the
/// same shape), without latching an overflow signal.
#[test]
fn parser_classifies_mid_stream_overloaded_as_provider_request() {
    for (raw, want_type) in [
        (
            r#"{"type":"error","error":{"type":"overloaded_error","message":"overloaded"}}"#,
            "overloaded",
        ),
        (
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#,
            "slow down",
        ),
        (
            r#"{"type":"error","error":{"type":"api_error","message":"generic 5xx"}}"#,
            "generic 5xx",
        ),
    ] {
        let mut state = AnthropicStreamState::default();
        let err = parse_anthropic_event(raw, &mut state).expect_err("stream error");
        assert!(
            matches!(err, squeezy_core::SqueezyError::ProviderRequest(_)),
            "transient errors must surface as ProviderRequest, got {err:?} for {raw}",
        );
        assert!(
            err.to_string().contains(want_type),
            "error message must round-trip the upstream prose, got {err} for {raw}",
        );
        assert!(
            !err.to_string()
                .contains(crate::anthropic_error::NON_RETRYABLE_MARKER),
            "transient errors must not carry the non-retryable marker: {err}",
        );
        assert!(
            state.pending_overflow_signal.is_none(),
            "transient errors must not latch an overflow signal",
        );
    }
}

/// C-01: `invalid_request_error` (e.g. budget_tokens too small) must
/// surface non-retryable so the retry layer can short-circuit instead
/// of replaying the identical bad request five times.
#[test]
fn parser_classifies_mid_stream_invalid_request_as_non_retryable() {
    let mut state = AnthropicStreamState::default();
    let err = parse_anthropic_event(
        r#"{
          "type":"error",
          "error":{
            "type":"invalid_request_error",
            "message":"thinking.enabled.budget_tokens must be >= 1024"
          }
        }"#,
        &mut state,
    )
    .expect_err("invalid request surfaces error");
    assert!(matches!(
        err,
        squeezy_core::SqueezyError::ProviderRequest(_)
    ));
    assert!(
        err.to_string()
            .contains(crate::anthropic_error::NON_RETRYABLE_MARKER),
    );
    assert!(state.pending_overflow_signal.is_none());
}

#[test]
fn parser_surfaces_max_tokens_stop() {
    let mut state = AnthropicStreamState::default();
    parse_anthropic_event(
        r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"}}"#,
        &mut state,
    )
    .expect("delta");

    let events = parse_anthropic_event(r#"{"type":"message_stop"}"#, &mut state).expect(
        "message_stop is no longer an early stream error; stop_reason is surfaced to the agent",
    );

    let completed = events
        .iter()
        .find_map(|event| match event {
            LlmEvent::Completed { stop_reason, .. } => Some(stop_reason.clone()),
            _ => None,
        })
        .expect("Completed event emitted");
    assert_eq!(completed, Some(crate::StopReason::MaxTokens));
}

#[test]
fn parser_normalizes_end_turn_stop_reason() {
    let mut state = AnthropicStreamState::default();
    parse_anthropic_event(
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
        &mut state,
    )
    .expect("delta");
    let events =
        parse_anthropic_event(r#"{"type":"message_stop"}"#, &mut state).expect("stop event");
    let completed = events
        .iter()
        .find_map(|event| match event {
            LlmEvent::Completed { stop_reason, .. } => Some(stop_reason.clone()),
            _ => None,
        })
        .expect("Completed event emitted");
    assert_eq!(completed, Some(crate::StopReason::EndTurn));
}

#[test]
fn parser_normalizes_refusal_stop_reason() {
    let mut state = AnthropicStreamState::default();
    parse_anthropic_event(
        r#"{"type":"message_delta","delta":{"stop_reason":"refusal"}}"#,
        &mut state,
    )
    .expect("delta");
    let events =
        parse_anthropic_event(r#"{"type":"message_stop"}"#, &mut state).expect("stop event");
    let completed = events
        .iter()
        .find_map(|event| match event {
            LlmEvent::Completed { stop_reason, .. } => Some(stop_reason.clone()),
            _ => None,
        })
        .expect("Completed event emitted");
    assert_eq!(completed, Some(crate::StopReason::Refusal));
}

#[test]
fn parser_accumulates_thinking_block_with_signature() {
    let mut state = AnthropicStreamState::default();
    parse_anthropic_event(
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
        &mut state,
    )
    .expect("start");
    let delta = parse_anthropic_event(
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"weigh"}}"#,
        &mut state,
    )
    .expect("delta");
    assert_eq!(
        delta,
        vec![LlmEvent::ReasoningDelta {
            text: "weigh".to_string(),
            kind: crate::ReasoningKind::Text,
        }]
    );
    parse_anthropic_event(
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"SIG"}}"#,
        &mut state,
    )
    .expect("signature");
    parse_anthropic_event(r#"{"type":"content_block_stop","index":0}"#, &mut state).expect("stop");

    let events = parse_anthropic_event(r#"{"type":"message_stop"}"#, &mut state).expect("stop");
    let payload = match events.first() {
        Some(LlmEvent::ReasoningDone(payload)) => payload.clone(),
        other => panic!("expected ReasoningDone first, got {other:?}"),
    };
    match payload {
        crate::ReasoningPayload::Anthropic { blocks } => {
            assert_eq!(blocks.len(), 1);
            assert_eq!(blocks[0].text, "weigh");
            assert_eq!(blocks[0].signature.as_deref(), Some("SIG"));
        }
        other => panic!("expected Anthropic payload, got {other:?}"),
    }
}

/// H-03: an `end_turn` finish with no visible text/tool output and a
/// populated reasoning buffer is the canonical "thinking-only" turn
/// the agent loop should retry. The streamer must set
/// `reasoning_only_stop=true` so the agent's retry branch fires.
#[test]
fn parser_marks_reasoning_only_stop_when_endturn_after_thinking_with_no_output() {
    let mut state = AnthropicStreamState::default();
    parse_anthropic_event(
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
        &mut state,
    )
    .expect("start");
    parse_anthropic_event(
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"weigh"}}"#,
        &mut state,
    )
    .expect("delta");
    parse_anthropic_event(r#"{"type":"content_block_stop","index":0}"#, &mut state).expect("stop");
    parse_anthropic_event(
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
        &mut state,
    )
    .expect("message_delta");
    let events = parse_anthropic_event(r#"{"type":"message_stop"}"#, &mut state).expect("stop");
    let completed = events
        .iter()
        .find_map(|event| match event {
            LlmEvent::Completed {
                reasoning_only_stop,
                stop_reason,
                ..
            } => Some((reasoning_only_stop, stop_reason.clone())),
            _ => None,
        })
        .expect("Completed event emitted");
    assert_eq!(completed.1, Some(crate::StopReason::EndTurn));
    assert!(
        *completed.0,
        "reasoning-only EndTurn with non-empty thinking buffer must set the flag",
    );
}

/// H-03 happy path: an `end_turn` finish with visible text output
/// must keep `reasoning_only_stop=false` even when the model
/// previously emitted reasoning.
#[test]
fn parser_does_not_mark_reasoning_only_stop_when_visible_text_was_emitted() {
    let mut state = AnthropicStreamState::default();
    parse_anthropic_event(
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
        &mut state,
    )
    .expect("thinking start");
    parse_anthropic_event(
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"weigh"}}"#,
        &mut state,
    )
    .expect("thinking delta");
    parse_anthropic_event(r#"{"type":"content_block_stop","index":0}"#, &mut state)
        .expect("thinking stop");
    parse_anthropic_event(
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"hello"}}"#,
        &mut state,
    )
    .expect("text delta");
    parse_anthropic_event(
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
        &mut state,
    )
    .expect("message_delta");
    let events = parse_anthropic_event(r#"{"type":"message_stop"}"#, &mut state).expect("stop");
    let reasoning_only = events
        .iter()
        .find_map(|event| match event {
            LlmEvent::Completed {
                reasoning_only_stop,
                ..
            } => Some(*reasoning_only_stop),
            _ => None,
        })
        .expect("Completed event emitted");
    assert!(
        !reasoning_only,
        "visible text output must clear reasoning_only_stop",
    );
}

/// H-03 negative case: an `end_turn` finish with no thinking and no
/// visible output must NOT mark `reasoning_only_stop` (the model
/// just produced an empty turn — distinct from the thinking-only
/// pattern the flag targets).
#[test]
fn parser_does_not_mark_reasoning_only_stop_when_no_thinking_was_seen() {
    let mut state = AnthropicStreamState::default();
    parse_anthropic_event(
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
        &mut state,
    )
    .expect("message_delta");
    let events = parse_anthropic_event(r#"{"type":"message_stop"}"#, &mut state).expect("stop");
    let reasoning_only = events
        .iter()
        .find_map(|event| match event {
            LlmEvent::Completed {
                reasoning_only_stop,
                ..
            } => Some(*reasoning_only_stop),
            _ => None,
        })
        .expect("Completed event emitted");
    assert!(
        !reasoning_only,
        "empty-thinking buffer must not trip reasoning_only_stop",
    );
}

/// H-02: Anthropic streams a redacted-thinking block's encrypted
/// payload over `signature_delta` frames (the `content_block_start`
/// frame's `data` field is empty until those land). Accumulate them
/// into `block.data` so the multi-turn round-trip ships the full
/// blob; otherwise Anthropic rejects the continuation with
/// `invalid_request_error` or silently breaks reasoning continuity.
#[test]
fn parser_accumulates_redacted_thinking_data_via_signature_delta() {
    let mut state = AnthropicStreamState::default();
    parse_anthropic_event(
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":""}}"#,
        &mut state,
    )
    .expect("start");
    parse_anthropic_event(
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"REDACTED_"}}"#,
        &mut state,
    )
    .expect("delta-1");
    parse_anthropic_event(
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"BLOB"}}"#,
        &mut state,
    )
    .expect("delta-2");
    parse_anthropic_event(r#"{"type":"content_block_stop","index":0}"#, &mut state).expect("stop");
    let events = parse_anthropic_event(r#"{"type":"message_stop"}"#, &mut state).expect("stop");
    let payload = match events.first() {
        Some(LlmEvent::ReasoningDone(payload)) => payload.clone(),
        other => panic!("expected ReasoningDone first, got {other:?}"),
    };
    match payload {
        crate::ReasoningPayload::Anthropic { blocks } => {
            assert_eq!(blocks.len(), 1);
            assert_eq!(blocks[0].kind, crate::AnthropicThinkingKind::Redacted);
            assert_eq!(
                blocks[0].data.as_deref(),
                Some("REDACTED_BLOB"),
                "signature_delta deltas must accumulate into `data` for redacted blocks",
            );
            assert!(blocks[0].signature.is_none());
        }
        other => panic!("expected Anthropic payload, got {other:?}"),
    }
}

/// H-02 replay: when a redacted block's encrypted blob lives in
/// `block.data`, the wire JSON must emit it under `data`. When it
/// instead lives in `block.signature` (e.g. a future provider build
/// that decides to route the field there), the round-trip helper
/// must still populate `data` so Anthropic accepts the continuation.
#[test]
fn anthropic_messages_redacted_thinking_populates_data_from_either_field() {
    use crate::AnthropicThinkingBlock;
    // Canonical: `data` populated.
    let payload = crate::ReasoningPayload::Anthropic {
        blocks: vec![AnthropicThinkingBlock {
            kind: crate::AnthropicThinkingKind::Redacted,
            text: String::new(),
            signature: None,
            data: Some("ENCRYPTED".to_string()),
        }],
    };
    let messages = anthropic_messages(
        &[LlmInputItem::Reasoning(payload)],
        false,
        false,
        CachePolicy::AUTO,
        crate::CacheRetention::None,
    );
    let arr = messages.as_array().expect("array");
    assert_eq!(arr[0]["content"][0]["type"], "redacted_thinking");
    assert_eq!(arr[0]["content"][0]["data"], "ENCRYPTED");

    // Fallback: only `signature` populated (e.g. older parser path
    // where the streamer accumulated into signature). The replay
    // helper falls back to it so the data field still ships
    // populated.
    let payload = crate::ReasoningPayload::Anthropic {
        blocks: vec![AnthropicThinkingBlock {
            kind: crate::AnthropicThinkingKind::Redacted,
            text: String::new(),
            signature: Some("FALLBACK_ENC".to_string()),
            data: None,
        }],
    };
    let messages = anthropic_messages(
        &[LlmInputItem::Reasoning(payload)],
        false,
        false,
        CachePolicy::AUTO,
        crate::CacheRetention::None,
    );
    let arr = messages.as_array().expect("array");
    assert_eq!(arr[0]["content"][0]["type"], "redacted_thinking");
    assert_eq!(arr[0]["content"][0]["data"], "FALLBACK_ENC");
}

#[test]
fn anthropic_messages_attach_thinking_blocks_to_assistant_turn() {
    let payload = crate::ReasoningPayload::Anthropic {
        blocks: vec![crate::AnthropicThinkingBlock {
            kind: crate::AnthropicThinkingKind::Thinking,
            text: "deliberated".to_string(),
            signature: Some("SIG".to_string()),
            data: None,
        }],
    };
    let input = vec![
        LlmInputItem::Reasoning(payload),
        LlmInputItem::AssistantText("answer".to_string()),
    ];
    let messages = anthropic_messages(
        &input,
        false,
        false,
        CachePolicy::AUTO,
        crate::CacheRetention::None,
    );
    let arr = messages.as_array().expect("array");
    assert_eq!(arr.len(), 1, "thinking + text fold into one assistant turn");
    let content = arr[0]["content"].as_array().expect("content array");
    assert_eq!(content[0]["type"], "thinking");
    assert_eq!(content[0]["signature"], "SIG");
    assert_eq!(content[1]["type"], "text");
    assert_eq!(content[1]["text"], "answer");
}

#[test]
fn beta_headers_route_into_http_header() {
    // The Anthropic provider does not embed beta headers in the JSON
    // body — they go on the `anthropic-beta` HTTP header. We assert the
    // routing helper produces the comma-joined value the provider
    // attaches on the outbound request.
    let betas: Arc<[Arc<str>]> = Arc::from(vec![
        Arc::<str>::from("context-1m-2025-08-07"),
        Arc::<str>::from("interleaved-thinking-2025-05-14"),
    ]);
    let header = anthropic_header_value(&betas).expect("non-empty betas yield a header value");
    assert_eq!(
        header,
        "context-1m-2025-08-07,interleaved-thinking-2025-05-14"
    );

    // The request body must not carry the betas — they belong on the
    // header for the 1P Anthropic transport.
    let request = LlmRequest {
        model: squeezy_core::DEFAULT_ANTHROPIC_MODEL.to_string().into(),
        instructions: "sys".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
        max_output_tokens: Some(32),
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
        beta_headers: betas,
        ..LlmRequest::default()
    };
    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);
    assert!(
        body.get("anthropic_beta").is_none(),
        "1P Anthropic transport sends betas via header, never inside the JSON body",
    );
}

#[test]
fn beta_headers_route_into_extra_body_params_on_bedrock() {
    // Bedrock uses converse_stream and the AWS gateway strips
    // non-standard HTTP headers, so the routing helper partitions out
    // the body-param-eligible subset for `additional_model_request_fields`.
    let betas: Arc<[Arc<str>]> = Arc::from(vec![
        Arc::<str>::from("context-1m-2025-08-07"),
        Arc::<str>::from("claude-code-20250219"),
    ]);
    let body_betas = bedrock_extra_body_betas(&betas);
    let body_strs: Vec<&str> = body_betas.iter().map(|b| b.as_ref()).collect();
    assert_eq!(
        body_strs,
        vec!["context-1m-2025-08-07"],
        "header-only betas (claude-code-*) must be dropped before reaching Bedrock",
    );
}

#[test]
fn beta_headers_dedup_when_capability_and_request_overlap() {
    // Future capability-derived betas can overlap with the
    // caller-supplied list; the routing helper must deduplicate so the
    // wire value carries each beta exactly once.
    let betas: Arc<[Arc<str>]> = Arc::from(vec![
        Arc::<str>::from("a"),
        Arc::<str>::from("b"),
        Arc::<str>::from("b"),
        Arc::<str>::from("c"),
    ]);
    let header = anthropic_header_value(&betas).expect("non-empty betas yield a header value");
    assert_eq!(header, "a,b,c");
}

#[test]
fn oauth_auth_scheme_prepends_claude_code_identity_to_system() {
    // The OAuth quota check requires every Claude Pro/Max request to
    // identify itself as Claude Code in the system block. Build a
    // request body under the OAuth auth scheme and verify the
    // identity preamble lands ahead of the user's instructions.
    let request = LlmRequest {
        model: "claude-opus-4-7".to_string().into(),
        instructions: "user-supplied instructions".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        max_output_tokens: Some(64),
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
    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::Oauth);
    let system = body.get("system").expect("system block must be present");
    let system = system
        .as_array()
        .expect("OAuth scheme must serialize `system` as an array");
    assert!(
        system
            .first()
            .and_then(|item| item.get("text"))
            .and_then(|text| text.as_str())
            .is_some_and(|text| text.contains("Claude Code")),
        "first system block must declare the Claude Code identity, got {system:?}"
    );
    assert!(
        system.iter().any(|item| item
            .get("text")
            .and_then(|text| text.as_str())
            .is_some_and(|text| text == "user-supplied instructions")),
        "user instructions must still ride alongside the identity preamble: {system:?}"
    );
}

#[test]
fn api_key_auth_scheme_keeps_system_string_unchanged() {
    // Static-key callers shouldn't see the identity preamble — the
    // body's `system` stays the same single string the existing tests
    // already lock in.
    let request = LlmRequest {
        model: "claude-opus-4-7".to_string().into(),
        instructions: "user-supplied instructions".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        max_output_tokens: Some(64),
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
    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);
    assert_eq!(body["system"], "user-supplied instructions");
}

#[test]
fn merge_oauth_beta_header_unions_caller_and_oauth_marker() {
    // OAuth-driven requests must always carry the Claude Code beta
    // marker. A caller-supplied beta value should merge without
    // duplicating entries.
    let oauth_marker = crate::oauth::anthropic_oauth_beta_header();
    let merged =
        super::merge_oauth_beta_header(Some("context-1m-2025-08-07"), AnthropicAuthScheme::Oauth)
            .expect("oauth scheme must produce a header value");
    for piece in oauth_marker.split(',') {
        assert!(
            merged.contains(piece),
            "merged header must keep oauth marker {piece}; got {merged}",
        );
    }
    assert!(
        merged.contains("context-1m-2025-08-07"),
        "merged header must keep caller beta; got {merged}",
    );
    let pieces: Vec<&str> = merged.split(',').collect();
    let mut dedup = pieces.clone();
    dedup.sort();
    dedup.dedup();
    assert_eq!(
        pieces.len(),
        dedup.len(),
        "merged header must not duplicate entries: {merged}"
    );
}

#[test]
fn merge_oauth_beta_header_returns_none_for_api_key_without_caller() {
    assert!(
        super::merge_oauth_beta_header(None, AnthropicAuthScheme::ApiKey).is_none(),
        "API-key callers with no betas should produce no header"
    );
    assert_eq!(
        super::merge_oauth_beta_header(Some("a,b"), AnthropicAuthScheme::ApiKey),
        Some("a,b".to_string()),
        "API-key path passes the caller's value through unchanged"
    );
}

/// LOW Q6: beta token dedup must be case-insensitive — Anthropic
/// treats `Claude-code-20250219` and `claude-code-20250219` as the
/// same opt-in, so shipping both burns header space and signals
/// confusion to the platform.
#[test]
fn merge_oauth_beta_header_dedups_case_insensitively() {
    let merged = super::merge_oauth_beta_header(
        Some("Claude-Code-20250219,Context-1M-2025-08-07"),
        AnthropicAuthScheme::Oauth,
    )
    .expect("oauth scheme produces a header");
    // The oauth marker (lowercase `claude-code-20250219`) wins
    // priority since it is processed first; the caller's
    // case-variant is dropped, so the merged header contains only
    // one entry for the marker.
    let pieces: Vec<&str> = merged.split(',').collect();
    let lower_pieces: Vec<String> = pieces.iter().map(|p| p.to_ascii_lowercase()).collect();
    let mut dedup = lower_pieces.clone();
    dedup.sort();
    dedup.dedup();
    assert_eq!(
        lower_pieces.len(),
        dedup.len(),
        "merged header must not contain case-variant duplicates",
    );
    assert!(
        lower_pieces.contains(&"context-1m-2025-08-07".to_string()),
        "caller-supplied beta must still ride on the wire",
    );
}

/// LOW Q4: `tool_choice` must lower into Anthropic's `{type, name?}`
/// shape so tool-shy models can be forced to call a specific tool.
#[test]
fn request_body_lowers_tool_choice_into_anthropic_shape() {
    use std::sync::Arc as StdArc;
    let mk = |hint: Option<&str>| -> serde_json::Value {
        let request = LlmRequest {
            model: "claude-test".to_string().into(),
            instructions: "be brief".to_string().into(),
            input: Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
            max_output_tokens: Some(32),
            response_verbosity: None,
            reasoning_effort: None,
            previous_response_id: None,
            cache_key: None,
            cache: CacheSpec::default(),
            tools: Arc::from(vec![
                LlmToolSpec {
                    name: "read_file".to_string(),
                    description: "Read".to_string(),
                    parameters: serde_json::json!({"type": "object"}),
                    strict: false,
                }
                .into(),
            ]),
            store: false,
            tool_choice: hint.map(str::to_string),
            output_schema: None,
            parallel_tool_calls: None,
            beta_headers: StdArc::from(Vec::new()),
            ..LlmRequest::default()
        };
        AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey)
    };

    // `auto` → `{type:auto}`.
    let body = mk(Some("auto"));
    assert_eq!(body["tool_choice"]["type"], "auto");

    // `required` → `{type:any}` (Anthropic's name for "must call a tool").
    let body = mk(Some("required"));
    assert_eq!(body["tool_choice"]["type"], "any");

    // `tool:NAME` → `{type:tool, name}`.
    let body = mk(Some("tool:read_file"));
    assert_eq!(body["tool_choice"]["type"], "tool");
    assert_eq!(body["tool_choice"]["name"], "read_file");

    // Unrecognised or absent: field omitted so Anthropic's default
    // (auto) applies.
    let body = mk(None);
    assert!(body.get("tool_choice").is_none());
    let body = mk(Some("nonsense"));
    assert!(body.get("tool_choice").is_none());
}

/// MEDIUM #5: `max_tokens` must clamp against the registry-known
/// per-model maximum so a user who copied an OpenAI value doesn't
/// 400 on every Anthropic turn.
#[test]
fn request_body_clamps_max_tokens_against_registry_max() {
    let request = LlmRequest {
        model: squeezy_core::DEFAULT_ANTHROPIC_MODEL.to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
        max_output_tokens: Some(128_000),
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
    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);
    let max_tokens = body["max_tokens"].as_u64().expect("u64");
    assert!(
        max_tokens <= 64_000,
        "registry max_output_tokens for the default Anthropic model is 64k; \
         caller 128k must clamp to 64k or lower, got {max_tokens}",
    );
}

/// MEDIUM #6: when the first `input_json_delta` arrives, the parser
/// must drop any seed the `content_block_start` frame populated so a
/// future server build that ships a complete initial input doesn't
/// produce `{}{"a":1}` after the delta is appended.
#[test]
fn parser_drops_initial_tool_use_input_seed_when_delta_arrives() {
    let mut state = AnthropicStreamState::default();
    // Simulate a future server build that ships a non-empty initial
    // `input` (today's Anthropic is benign — always sends `{}`). The
    // first delta must reset the accumulator so the resulting JSON
    // parses cleanly.
    parse_anthropic_event(
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"read_file","input":{"path":"old"}}}"#,
        &mut state,
    )
    .expect("start");
    parse_anthropic_event(
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"new"}}"#,
        &mut state,
    )
    .expect("delta-1");
    parse_anthropic_event(
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"}"}}"#,
        &mut state,
    )
    .expect("delta-2");
    let event = parse_anthropic_event(r#"{"type":"content_block_stop","index":1}"#, &mut state)
        .expect("stop");
    assert_eq!(
        event,
        vec![LlmEvent::ToolCall(LlmToolCall {
            call_id: "toolu_1".to_string(),
            name: "read_file".to_string(),
            arguments: serde_json::json!({ "path": "new" }),
        })],
        "first delta must reset the accumulator so initial seed cannot corrupt the parse"
    );
}

/// LOW Q4 negative: when no tools are advertised, `tool_choice` is
/// not emitted regardless of the caller hint — Anthropic rejects
/// `tool_choice` without `tools`.
#[test]
fn request_body_omits_tool_choice_when_no_tools_are_advertised() {
    let request = LlmRequest {
        model: "claude-test".to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
        max_output_tokens: Some(32),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        cache: CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: Some("required".to_string()),
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };
    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);
    assert!(body.get("tool_choice").is_none());
}

/// H-01: the 4-slot allocator must consume in tools → system →
/// messages order so a future caller marker only ever displaces the
/// most-volatile slot. Today's auto-3 policy fits comfortably under
/// the cap; once exhausted, additional consume calls drop+warn.
#[test]
fn breakpoint_budget_allocates_within_cap_and_drops_overflow() {
    let mut budget = super::BreakpointBudget::new();
    assert!(budget.consume("tools"));
    assert!(budget.consume("system"));
    assert!(budget.consume("messages"));
    assert_eq!(budget.remaining, 1);
    assert!(budget.consume("future-marker"));
    assert_eq!(budget.remaining, 0);
    assert!(!budget.consume("overflow-1"));
    assert!(!budget.consume("overflow-2"));
    assert_eq!(budget.dropped, 2);
}

/// H-01 regression guard: the auto-3 policy still emits markers on
/// system / last user block / last stable tool — the 4-slot allocator
/// must not regress the happy path.
#[test]
fn request_body_marks_all_three_auto_breakpoints_within_budget() {
    let request = LlmRequest {
        model: squeezy_core::DEFAULT_ANTHROPIC_MODEL.to_string().into(),
        instructions: "system prompt".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
        max_output_tokens: Some(32),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: Some("squeezy::session-1".to_string()),
        cache: CacheSpec::default(),
        tools: Arc::from(vec![
            LlmToolSpec {
                name: "read_file".to_string(),
                description: "read".to_string(),
                parameters: serde_json::json!({"type": "object"}),
                strict: false,
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
    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);
    assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    let tools = body["tools"].as_array().expect("tools");
    assert_eq!(tools[0]["cache_control"]["type"], "ephemeral");
    let last_user = body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .rev()
        .find(|m| m["role"] == "user")
        .expect("user message");
    let last_block = last_user["content"]
        .as_array()
        .expect("content")
        .last()
        .expect("last block");
    assert_eq!(last_block["cache_control"]["type"], "ephemeral");
}

#[test]
fn request_body_encodes_image_as_base64_content_block() {
    // Synthetic 8-byte PNG-magic prefix; the bytes here don't have to be
    // a real image because we're only inspecting the wire shape.
    let bytes: Arc<[u8]> = Arc::from(vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    let request = LlmRequest {
        model: "claude-test".to_string().into(),
        instructions: "describe images".to_string().into(),
        input: Arc::from(vec![
            LlmInputItem::UserText("what is this?".to_string()),
            LlmInputItem::Image {
                media_type: "image/png".to_string(),
                bytes: bytes.clone(),
            },
        ]),
        max_output_tokens: Some(32),
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

    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);

    // The user text and image must coalesce into one user message with
    // two content blocks (text then image) so Anthropic sees them as a
    // single multimodal turn.
    let content = body["messages"][0]["content"].as_array().expect("content");
    assert_eq!(content.len(), 2);
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], "what is this?");
    assert_eq!(content[1]["type"], "image");
    assert_eq!(content[1]["source"]["type"], "base64");
    assert_eq!(content[1]["source"]["media_type"], "image/png");
    let encoded = content[1]["source"]["data"]
        .as_str()
        .expect("base64 string");
    use base64::Engine as _;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .expect("valid base64");
    assert_eq!(decoded.as_slice(), bytes.as_ref());
}

#[test]
fn request_body_ignores_parallel_tool_calls_for_unsupported_provider() {
    // G3: `parallel_tool_calls` is OpenAI-shaped. Anthropic does not have
    // a matching wire field, so the param must never leak into the body —
    // regardless of which value the caller set. This guards the
    // "unaffected for other providers" half of the G3 contract.
    for value in [None, Some(true), Some(false)] {
        let request = LlmRequest {
            model: "claude-test".to_string().into(),
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
                    description: "Read a file".to_string(),
                    parameters: serde_json::json!({"type": "object"}),
                    strict: true,
                }
                .into(),
            ]),
            store: false,
            tool_choice: None,
            output_schema: None,
            parallel_tool_calls: value,
            beta_headers: std::sync::Arc::from(Vec::new()),
            ..LlmRequest::default()
        };

        let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);
        assert!(
            body.get("parallel_tool_calls").is_none(),
            "Anthropic body must never carry parallel_tool_calls (value={value:?}): {body}"
        );
    }
}

// ----------------------------------------------------------------------
// Stable-tail anchor (4th cache_control breakpoint) tests.
// ----------------------------------------------------------------------

/// Recursively count every `cache_control` key anywhere in a JSON value.
/// Anthropic 400s above four breakpoints per request, so the wire body must
/// never carry more than four no matter how the markers are placed.
fn count_cache_controls(value: &Value) -> usize {
    match value {
        Value::Object(map) => {
            let here = usize::from(map.contains_key("cache_control"));
            here + map.values().map(count_cache_controls).sum::<usize>()
        }
        Value::Array(items) => items.iter().map(count_cache_controls).sum(),
        _ => 0,
    }
}

/// Build a synthetic agent loop with `rounds` user/assistant/tool turns.
/// Each round is: user text → assistant tool_call → user tool_result.
/// `FunctionCallOutput` lowers to a user-role `tool_result` block, so a long
/// loop yields many user-role messages — exactly the shape that lets the
/// stationary anchor sit several user turns behind the moving breakpoint.
fn synthetic_agent_loop(rounds: usize) -> Vec<LlmInputItem> {
    let mut input = Vec::new();
    for round in 0..rounds {
        input.push(LlmInputItem::UserText(format!("user turn {round}")));
        input.push(LlmInputItem::FunctionCall {
            call_id: format!("call_{round}"),
            name: "read_file".to_string(),
            arguments: serde_json::json!({ "path": format!("file_{round}.rs") }),
        });
        input.push(LlmInputItem::FunctionCallOutput {
            call_id: format!("call_{round}"),
            output: format!("contents of file_{round}.rs"),
            content_parts: None,
            is_error: false,
        });
    }
    input
}

/// Collect, in message order, the indices of messages whose last content
/// block carries a `cache_control` marker.
fn marked_message_indices(messages: &[Value]) -> Vec<usize> {
    messages
        .iter()
        .enumerate()
        .filter_map(|(idx, msg)| {
            let marked = msg["content"]
                .as_array()
                .and_then(|c| c.last())
                .and_then(Value::as_object)
                .is_some_and(|block| block.contains_key("cache_control"));
            marked.then_some(idx)
        })
        .collect()
}

#[test]
fn stable_anchor_sits_backoff_user_turns_behind_moving_breakpoint() {
    // Long conversation: with the stable anchor enabled the messages array
    // must carry exactly two breakpoints (moving latest-user + stationary
    // anchor) and the anchor must sit STABLE_ANCHOR_BACKOFF user turns
    // behind the moving one, on a different message.
    let input = synthetic_agent_loop(6);
    let messages = anthropic_messages(
        &input,
        true,
        true,
        CachePolicy::AUTO,
        crate::CacheRetention::Short,
    );
    let arr = messages.as_array().expect("messages array");

    let marked = marked_message_indices(arr);
    assert_eq!(
        marked.len(),
        2,
        "long conversation must carry the moving + anchor breakpoints only: {marked:?}"
    );

    // The moving breakpoint is on the last user-role message.
    let moving_breakpoint_idx = arr
        .iter()
        .enumerate()
        .rev()
        .find(|(_, m)| m["role"] == "user")
        .map(|(idx, _)| idx)
        .expect("a user-role message");
    assert!(
        marked.contains(&moving_breakpoint_idx),
        "moving breakpoint must mark the latest user-role message: {marked:?}"
    );

    // The anchor is on the user message STABLE_ANCHOR_BACKOFF user turns back.
    let backoff = crate::cache_policy::STABLE_ANCHOR_BACKOFF;
    let mut user_indices: Vec<usize> = arr
        .iter()
        .enumerate()
        .filter(|(_, m)| m["role"] == "user")
        .map(|(idx, _)| idx)
        .collect();
    user_indices.reverse();
    let expected_anchor_idx = user_indices[backoff];
    assert!(
        marked.contains(&expected_anchor_idx),
        "anchor must mark the user turn {backoff} back (idx {expected_anchor_idx}): {marked:?}"
    );

    // The two markers are on distinct messages — never a double marker.
    assert_ne!(
        moving_breakpoint_idx, expected_anchor_idx,
        "moving breakpoint and anchor must not share a message"
    );
}

#[test]
fn stable_anchor_is_noop_on_short_conversation() {
    // A single round has only one user-text turn and one tool-result turn.
    // mark_last_user_block lands on the tool_result (the latest user-role
    // message); the anchor would need STABLE_ANCHOR_BACKOFF + 1 user turns
    // behind it, which do not exist, so it must place nothing and the
    // conversation behaves exactly as it did before the 4th breakpoint.
    let input = synthetic_agent_loop(1);
    let with_anchor = anthropic_messages(
        &input,
        true,
        true,
        CachePolicy::AUTO,
        crate::CacheRetention::Short,
    );
    let without_anchor = anthropic_messages(
        &input,
        true,
        false,
        CachePolicy::AUTO,
        crate::CacheRetention::Short,
    );
    assert_eq!(
        with_anchor, without_anchor,
        "short conversation must be byte-identical with and without the anchor slot"
    );

    let arr = with_anchor.as_array().expect("messages array");
    assert_eq!(
        marked_message_indices(arr).len(),
        1,
        "short conversation carries only the single moving breakpoint"
    );
}

#[test]
fn request_body_never_emits_more_than_four_cache_controls() {
    // End-to-end through request_body with tools + system + a long agent
    // loop: tools(1) + system(1) + moving(1) + anchor(1) = 4, the hard cap.
    // The recursive count guards against any path that would push past it.
    let request = LlmRequest {
        model: squeezy_core::DEFAULT_ANTHROPIC_MODEL.to_string().into(),
        instructions: "system prompt".to_string().into(),
        input: Arc::from(synthetic_agent_loop(8)),
        max_output_tokens: Some(64),
        cache_key: Some("squeezy::session-anchor".to_string()),
        cache: CacheSpec::default(),
        tools: Arc::from(vec![
            LlmToolSpec {
                name: "read_file".to_string(),
                description: "Read a file".to_string(),
                parameters: serde_json::json!({ "type": "object" }),
                strict: true,
            }
            .into(),
            LlmToolSpec {
                name: "write_file".to_string(),
                description: "Write a file".to_string(),
                parameters: serde_json::json!({ "type": "object" }),
                strict: true,
            }
            .into(),
        ]),
        ..LlmRequest::default()
    };

    let body = AnthropicProvider::request_body(&request, AnthropicAuthScheme::ApiKey);
    let total = count_cache_controls(&body);
    assert_eq!(
        total, 4,
        "long loop must use all four breakpoints (tools+system+moving+anchor): {body}"
    );
    assert!(
        total <= 4,
        "Anthropic 400s above four cache_control breakpoints: {body}"
    );

    // The two message breakpoints must be on different messages (no block
    // carries two cache_control markers).
    let messages = body["messages"].as_array().expect("messages array");
    assert_eq!(
        marked_message_indices(messages).len(),
        2,
        "exactly the moving + anchor breakpoints in the messages array: {body}"
    );
}

#[test]
fn stable_anchor_breakpoints_use_matching_retention_shape() {
    // The anchor must reuse the exact cache_control shape the moving
    // breakpoint emits: Long retention carries the 1h ttl, Short omits it.
    let input = synthetic_agent_loop(6);
    for (retention, expect_ttl) in [
        (crate::CacheRetention::Short, false),
        (crate::CacheRetention::Long, true),
    ] {
        let messages = anthropic_messages(&input, true, true, CachePolicy::AUTO, retention);
        let arr = messages.as_array().expect("messages array");
        for idx in marked_message_indices(arr) {
            let cc = arr[idx]["content"]
                .as_array()
                .and_then(|c| c.last())
                .map(|b| &b["cache_control"])
                .expect("marked block");
            assert_eq!(cc["type"], "ephemeral");
            assert_eq!(
                cc.get("ttl").is_some(),
                expect_ttl,
                "ttl presence must match retention {retention:?} on message {idx}"
            );
        }
    }
}
