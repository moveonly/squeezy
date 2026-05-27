use std::sync::Arc;

use serde_json::{Value, json};

use super::{CachePolicy, MessageStrategy, json_markers, should_apply_caching};
use crate::{LlmInputItem, LlmRequest};

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
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
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
    let value = json_markers::system_array_with_marker("hello system");
    assert_eq!(value[0]["type"], "text");
    assert_eq!(value[0]["text"], "hello system");
    assert_eq!(value[0]["cache_control"]["type"], "ephemeral");
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

    json_markers::mark_last_user_block(&mut messages);

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

    json_markers::mark_last_user_block(&mut messages);

    assert!(
        messages[0]["content"][0].get("cache_control").is_none(),
        "no user turn → no markers"
    );
}

#[test]
fn mark_last_tool_marks_trailing_tool_definition_only() {
    let mut tools: Vec<Value> = vec![
        json!({ "name": "first" }),
        json!({ "name": "second" }),
        json!({ "name": "third" }),
    ];

    json_markers::mark_last_tool(&mut tools);

    assert!(
        tools[0].get("cache_control").is_none(),
        "earlier tool must remain unmarked"
    );
    assert!(tools[1].get("cache_control").is_none());
    assert_eq!(tools[2]["cache_control"]["type"], "ephemeral");
}
