use std::sync::Arc;

use serde_json::{Value, json};

use super::{
    CachePolicy, CacheRetention, CacheSpec, MessageStrategy, json_markers, last_stable_tool_index,
    should_apply_caching,
};
use crate::{AnthropicProvider, LlmInputItem, LlmRequest, LlmToolSpec, OpenAiProvider};

fn request_with_cache_key(model: &str, cache_key: Option<&str>) -> LlmRequest {
    LlmRequest {
        model: Arc::from(model),
        instructions: Arc::from("system"),
        input: Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: cache_key.map(str::to_string),
        cache: CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: Arc::from(Vec::new()),
        ..LlmRequest::default()
    }
}

fn request_with_cache(model: &str, cache: CacheSpec) -> LlmRequest {
    LlmRequest {
        model: Arc::from(model),
        instructions: Arc::from("system"),
        input: Arc::from(vec![LlmInputItem::UserText("hi".to_string())]),
        max_output_tokens: None,
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        cache,
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: Arc::from(Vec::new()),
        ..LlmRequest::default()
    }
}

#[test]
fn auto_policy_marks_tools_system_and_latest_user_message() {
    let policy = CachePolicy::AUTO;
    assert!(policy.tools);
    assert!(policy.system);
    assert_eq!(policy.messages, MessageStrategy::LatestUserMessage);
}

#[test]
fn should_apply_caching_requires_both_cache_key_and_capability() {
    let model = squeezy_core::DEFAULT_ANTHROPIC_MODEL;

    // cache_key set + registry reports prompt_caching for Claude Sonnet.
    let with_key = request_with_cache_key(model, Some("squeezy::session-1"));
    assert!(should_apply_caching("anthropic", &with_key));

    // No cache_key → no markers regardless of capability.
    let no_key = request_with_cache_key(model, None);
    assert!(!should_apply_caching("anthropic", &no_key));

    // Unknown model with cache_key → no markers (fallback capability lookup
    // returns prompt_caching=false, which is the safe default).
    let unknown = request_with_cache_key("not-a-real-model", Some("k"));
    assert!(!should_apply_caching("anthropic", &unknown));
}

#[test]
fn should_apply_caching_gates_on_registry_capability_flag() {
    // Ollama models in the registry carry prompt_caching=false (local models
    // have no provider-side cache layer). The helper must report false even
    // when a cache_key is set so the adapter does not synthesize an
    // ineffective directive.
    let request = request_with_cache_key("qwen3-coder", Some("k"));
    assert!(!should_apply_caching("ollama", &request));
}

#[test]
fn system_array_with_marker_wraps_string_in_anthropic_form() {
    let value = json_markers::system_array_with_marker("hello system", CacheRetention::Short);
    assert_eq!(value[0]["type"], "text");
    assert_eq!(value[0]["text"], "hello system");
    assert_eq!(value[0]["cache_control"]["type"], "ephemeral");
    assert!(
        value[0]["cache_control"].get("ttl").is_none(),
        "Short retention must not emit a ttl override (provider default = 5m)"
    );
}

#[test]
fn system_array_with_marker_emits_one_hour_ttl_for_long_retention() {
    let value = json_markers::system_array_with_marker("hello system", CacheRetention::Long);
    assert_eq!(value[0]["cache_control"]["type"], "ephemeral");
    assert_eq!(value[0]["cache_control"]["ttl"], "1h");
}

#[test]
fn mark_last_user_block_marks_only_the_most_recent_user_message() {
    let mut messages = vec![
        json!({
            "role": "user",
            "content": [{ "type": "text", "text": "first" }],
        }),
        json!({
            "role": "assistant",
            "content": [{ "type": "text", "text": "ack" }],
        }),
        json!({
            "role": "user",
            "content": [{ "type": "text", "text": "second" }],
        }),
    ];

    json_markers::mark_last_user_block(&mut messages, CacheRetention::Short);

    let first_content = &messages[0]["content"][0];
    assert!(
        first_content.get("cache_control").is_none(),
        "earlier user turn must not be marked"
    );
    let second_content = &messages[2]["content"][0];
    assert_eq!(second_content["cache_control"]["type"], "ephemeral");
}

#[test]
fn mark_last_user_block_is_noop_when_no_user_message() {
    let mut messages = vec![json!({
        "role": "assistant",
        "content": [{ "type": "text", "text": "only" }],
    })];

    json_markers::mark_last_user_block(&mut messages, CacheRetention::Short);

    assert!(
        messages[0]["content"][0].get("cache_control").is_none(),
        "no user turn → no markers"
    );
}

#[test]
fn mark_last_stable_tool_marks_trailing_tool_definition_only() {
    let mut tools: Vec<Value> = vec![
        json!({ "name": "first" }),
        json!({ "name": "second" }),
        json!({ "name": "third" }),
    ];

    json_markers::mark_last_stable_tool(&mut tools, CacheRetention::Short);

    assert!(
        tools[0].get("cache_control").is_none(),
        "earlier tool must remain unmarked"
    );
    assert!(tools[1].get("cache_control").is_none());
    assert_eq!(tools[2]["cache_control"]["type"], "ephemeral");
}

#[test]
fn mark_last_stable_tool_skips_trailing_dynamic_tools() {
    // Tool registry orders first-party tools before MCP-sourced ones. The
    // breakpoint must sit on the last first-party tool so the cached
    // prefix survives an MCP `tools/list` refresh that reorders or
    // replaces the trailing dynamic entries.
    let mut tools: Vec<Value> = vec![
        json!({ "name": "grep" }),
        json!({ "name": "read" }),
        json!({ "name": "mcp__github__list_issues" }),
        json!({ "name": "mcp__linear__create" }),
    ];

    json_markers::mark_last_stable_tool(&mut tools, CacheRetention::Short);

    assert!(tools[0].get("cache_control").is_none());
    assert_eq!(
        tools[1]["cache_control"]["type"], "ephemeral",
        "breakpoint must sit on the last first-party tool, not on an MCP tool"
    );
    assert!(tools[2].get("cache_control").is_none());
    assert!(tools[3].get("cache_control").is_none());
}

#[test]
fn mark_last_stable_tool_falls_back_to_literal_last_when_only_dynamic_tools_present() {
    // Degenerate case: every advertised tool is dynamic. We still need a
    // breakpoint somewhere when caching is enabled, so anchor to the
    // literal last entry. The next turn that re-advertises the same set
    // will hit the cache; a turn that mutates the dynamic set will miss
    // (acceptable — there is no stable suffix to anchor to).
    let mut tools: Vec<Value> = vec![
        json!({ "name": "mcp__github__list_issues" }),
        json!({ "name": "mcp__linear__create" }),
    ];

    json_markers::mark_last_stable_tool(&mut tools, CacheRetention::Short);

    assert!(tools[0].get("cache_control").is_none());
    assert_eq!(tools[1]["cache_control"]["type"], "ephemeral");
}

#[test]
fn mark_last_stable_tool_is_noop_on_empty_slice() {
    let mut tools: Vec<Value> = Vec::new();
    json_markers::mark_last_stable_tool(&mut tools, CacheRetention::Short);
    assert!(tools.is_empty());
}

#[test]
fn last_stable_tool_index_picks_trailing_first_party_index() {
    // The primitive every adapter routes through: take an iterator of
    // tool names, return the index where the cache breakpoint belongs.
    // Adapters with non-Anthropic JSON shapes (Chat Completions nests
    // `name` under `function.name`) call this directly so they don't
    // have to round-trip through a JSON projection.
    let names = ["grep", "read", "mcp__github__list_issues"];
    assert_eq!(last_stable_tool_index(names.iter().copied()), Some(1));
}

#[test]
fn last_stable_tool_index_falls_back_to_last_when_all_dynamic() {
    let names = ["mcp__a__one", "mcp__b__two"];
    assert_eq!(last_stable_tool_index(names.iter().copied()), Some(1));
}

#[test]
fn last_stable_tool_index_returns_none_on_empty_iterator() {
    let names: [&str; 0] = [];
    assert_eq!(last_stable_tool_index(names.iter().copied()), None);
}

// F11-cache-retention-universal-policy tests --------------------------------

fn tool_spec(name: &str) -> Arc<LlmToolSpec> {
    Arc::new(LlmToolSpec {
        name: name.to_string(),
        description: format!("{name} tool"),
        parameters: json!({ "type": "object", "properties": {} }),
        strict: true,
    })
}

fn extract_tool_cache_control(body: &Value) -> Option<&Value> {
    body.get("tools")
        .and_then(Value::as_array)
        .and_then(|tools| tools.iter().find_map(|tool| tool.get("cache_control")))
}

fn extract_system_cache_control(body: &Value) -> Option<&Value> {
    body.get("system")
        .and_then(Value::as_array)
        .and_then(|blocks| blocks.last())
        .and_then(|block| block.get("cache_control"))
}

#[test]
fn long_retention_routes_to_anthropic_one_hour_ttl_marker() {
    // F11: When the caller asks for `CacheRetention::Long` *and* the
    // destination model supports prompt caching, every `cache_control`
    // marker the Anthropic adapter emits (system tail, last user block,
    // last stable tool) must carry `ttl: "1h"` instead of the implicit
    // 5m default. Anthropic charges a separate write rate for the 1h
    // band, so the ttl override is the only path to extended retention.
    let model = squeezy_core::DEFAULT_ANTHROPIC_MODEL;
    let mut request = request_with_cache(
        model,
        CacheSpec {
            key: Some("squeezy::long-session".to_string()),
            retention: CacheRetention::Long,
        },
    );
    request.tools = Arc::from(vec![tool_spec("grep"), tool_spec("read")]);

    let body =
        AnthropicProvider::request_body(&request, crate::anthropic::AnthropicAuthScheme::ApiKey);

    let system_cc = extract_system_cache_control(&body)
        .expect("system tail must carry a cache_control marker on Long retention");
    assert_eq!(system_cc["type"], "ephemeral");
    assert_eq!(
        system_cc["ttl"], "1h",
        "Long retention must emit ttl=1h on the system marker: {body}"
    );

    let messages = body["messages"]
        .as_array()
        .expect("messages must be an array");
    let last_user = messages
        .iter()
        .rev()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        .expect("at least one user message expected");
    let user_cc = last_user["content"][0]
        .get("cache_control")
        .expect("last user block must be marked");
    assert_eq!(user_cc["ttl"], "1h");

    let tool_cc = extract_tool_cache_control(&body)
        .expect("trailing stable tool must be marked on Long retention");
    assert_eq!(tool_cc["ttl"], "1h");
}

#[test]
fn long_retention_routes_to_openai_prompt_cache_retention_24h() {
    // F11: On the OpenAI Responses adapter, `CacheRetention::Long` lifts
    // into a top-level `prompt_cache_retention: "24h"` field — the
    // documented opt-in for extended prompt caching. `Short` and `None`
    // must omit the field so the provider falls back to its short-lived
    // in-memory default. `prompt_cache_key` continues to track the
    // caller's affinity key regardless of retention.
    let request = request_with_cache(
        "gpt-5.1",
        CacheSpec {
            key: Some("squeezy::long-session".to_string()),
            retention: CacheRetention::Long,
        },
    );

    let body = OpenAiProvider::request_body(&request, "openai");

    assert_eq!(
        body["prompt_cache_retention"], "24h",
        "Long retention must set prompt_cache_retention=24h on OpenAI: {body}"
    );
    assert_eq!(body["prompt_cache_key"], "squeezy::long-session");
}

#[test]
fn none_retention_emits_no_cache_markers_on_either_provider() {
    // F11: `CacheRetention::None` is the hard "do not cache" signal. The
    // Anthropic adapter must not emit a `cache_control` object anywhere
    // in the body (system stays a plain string, user content stays
    // unmarked, tools stay unmarked). The OpenAI adapter must omit
    // `prompt_cache_retention` *and* `prompt_cache_key`. This is the
    // default for any request that touches neither the new `cache`
    // field nor the legacy `cache_key`.
    let model = squeezy_core::DEFAULT_ANTHROPIC_MODEL;
    let mut anthropic_request = request_with_cache(model, CacheSpec::default());
    anthropic_request.tools = Arc::from(vec![tool_spec("grep"), tool_spec("read")]);

    let anthropic_body = AnthropicProvider::request_body(
        &anthropic_request,
        crate::anthropic::AnthropicAuthScheme::ApiKey,
    );
    assert!(
        anthropic_body["system"].is_string(),
        "None retention must leave system as a plain string (no marker array): {anthropic_body}"
    );
    let anthropic_messages = anthropic_body["messages"]
        .as_array()
        .expect("messages must be an array");
    for msg in anthropic_messages {
        if let Some(content) = msg.get("content").and_then(Value::as_array) {
            for block in content {
                assert!(
                    block.get("cache_control").is_none(),
                    "None retention must not emit any cache_control on Anthropic message blocks: {block}"
                );
            }
        }
    }
    assert!(
        extract_tool_cache_control(&anthropic_body).is_none(),
        "None retention must not mark any tool on Anthropic: {anthropic_body}"
    );

    let openai_request = request_with_cache("gpt-5.1", CacheSpec::default());
    let openai_body = OpenAiProvider::request_body(&openai_request, "openai");
    assert!(
        openai_body.get("prompt_cache_retention").is_none(),
        "None retention must not set prompt_cache_retention on OpenAI: {openai_body}"
    );
    assert!(
        openai_body.get("prompt_cache_key").is_none(),
        "None retention with no key must not set prompt_cache_key on OpenAI: {openai_body}"
    );
}
