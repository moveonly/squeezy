use super::*;
use crate::{LlmEvent, LlmInputItem, LlmToolSpec};
use serde_json::{Value, json};
use squeezy_core::OpenAiCompatiblePreset;
use std::sync::Arc;

fn sample_request() -> LlmRequest {
    LlmRequest {
        model: "anthropic/claude-opus-4-7".to_string().into(),
        instructions: "be brief".to_string().into(),
        input: Arc::from(vec![
            LlmInputItem::UserText("hello".to_string()),
            LlmInputItem::AssistantText("hi there".to_string()),
            LlmInputItem::FunctionCallOutput {
                call_id: "call_42".to_string(),
                output: r#"{"result":"ok"}"#.to_string(),
            },
        ]),
        max_output_tokens: Some(128),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        tools: Arc::from(vec![
            LlmToolSpec {
                name: "grep".to_string(),
                description: "search files".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {"pattern": {"type": "string"}},
                    "required": ["pattern"]
                }),
                strict: true,
            }
            .into(),
        ]),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
    }
}

#[test]
fn request_body_omits_tool_choice_when_unset() {
    // Default behavior: no tool_choice field in the body so the
    // provider applies its default (typically `auto`).
    let body = OpenAiCompatibleProvider::request_body(&sample_request());
    assert!(
        body.get("tool_choice").is_none(),
        "tool_choice must be absent when LlmRequest.tool_choice is None: {body}"
    );
}

#[test]
fn request_body_emits_tool_choice_required_when_set() {
    // The fix for tool-shy chat-completions models (Qwen via OpenRouter,
    // smaller MoEs): when `tool_choice = "required"` is configured under
    // [model], it must be forwarded to the provider so the model is
    // forced to emit at least one tool call per turn.
    let mut request = sample_request();
    request.tool_choice = Some("required".to_string());
    let body = OpenAiCompatibleProvider::request_body(&request);
    assert_eq!(body["tool_choice"], "required");
}

#[test]
fn request_body_omits_tool_choice_when_no_tools_advertised() {
    // No tools → no `tool_choice` field, even when set, since the field
    // is meaningless without tools and some providers reject it.
    let mut request = sample_request();
    request.tools = Arc::from(Vec::<Arc<LlmToolSpec>>::new());
    request.tool_choice = Some("required".to_string());
    let body = OpenAiCompatibleProvider::request_body(&request);
    assert!(
        body.get("tools").is_none(),
        "tools field must be omitted when empty"
    );
    assert!(
        body.get("tool_choice").is_none(),
        "tool_choice must be omitted when no tools are advertised"
    );
}

#[test]
fn request_body_uses_chat_completions_shape() {
    let body = OpenAiCompatibleProvider::request_body(&sample_request());

    assert_eq!(body["model"], "anthropic/claude-opus-4-7");
    assert_eq!(body["stream"], true);
    assert_eq!(body["max_tokens"], 128);
    assert_eq!(body["stream_options"]["include_usage"], true);

    let messages = body["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 4, "system + 3 input items");
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], "be brief");
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], "hello");
    assert_eq!(messages[2]["role"], "assistant");
    assert_eq!(messages[2]["content"], "hi there");
    assert_eq!(messages[3]["role"], "tool");
    assert_eq!(messages[3]["tool_call_id"], "call_42");
    assert_eq!(messages[3]["content"], r#"{"result":"ok"}"#);

    let tools = body["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["type"], "function");
    assert_eq!(tools[0]["function"]["name"], "grep");
    assert_eq!(tools[0]["function"]["description"], "search files");
    assert_eq!(
        tools[0]["function"]["parameters"]["properties"]["pattern"]["type"],
        "string"
    );
}

#[test]
fn request_body_skips_empty_system_message() {
    let mut request = sample_request();
    request.instructions = "   ".to_string().into();
    let body = OpenAiCompatibleProvider::request_body(&request);

    let messages = body["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["role"], "user");
}

#[test]
fn request_body_serialises_assistant_function_call_history() {
    let request = LlmRequest {
        model: "groq/llama-3.3-70b".to_string().into(),
        instructions: "ok".to_string().into(),
        input: Arc::from(vec![LlmInputItem::FunctionCall {
            call_id: "call_99".to_string(),
            name: "grep".to_string(),
            arguments: json!({"pattern": "todo"}),
        }]),
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
        beta_headers: std::sync::Arc::from(Vec::new()),
    };
    let body = OpenAiCompatibleProvider::request_body(&request);
    let messages = body["messages"].as_array().expect("messages array");
    let assistant_call = &messages[1];
    assert_eq!(assistant_call["role"], "assistant");
    let tool_call = &assistant_call["tool_calls"][0];
    assert_eq!(tool_call["id"], "call_99");
    assert_eq!(tool_call["type"], "function");
    assert_eq!(tool_call["function"]["name"], "grep");
    let arguments_text = tool_call["function"]["arguments"]
        .as_str()
        .expect("arguments serialised as string");
    let parsed: Value = serde_json::from_str(arguments_text).unwrap();
    assert_eq!(parsed["pattern"], "todo");
}

#[test]
fn parse_chat_event_emits_text_delta() {
    let mut state = StreamState::default();
    let events = parse_chat_event(
        r#"{"id":"resp_1","choices":[{"delta":{"content":"hello"}}]}"#,
        &mut state,
    )
    .expect("valid event");
    assert_eq!(events, vec![LlmEvent::TextDelta("hello".to_string())]);
    assert_eq!(state.response_id.as_deref(), Some("resp_1"));
}

#[test]
fn parse_chat_event_emits_text_delta_for_array_shape_content() {
    // Some aggregator routes (notably Qwen via OpenRouter/PortKey) stream
    // `content` as an array of content parts instead of a bare string.
    // The old parser silently dropped these — see the regression caught
    // on `portkey:@openrouter/qwen/qwen3.6-35b-a3b` where every assistant
    // turn billed output tokens but the stored text was empty.
    let mut state = StreamState::default();
    let events = parse_chat_event(
        r#"{"id":"r","choices":[{"delta":{"content":[{"type":"text","text":"hel"},{"type":"text","text":"lo"}]}}]}"#,
        &mut state,
    )
    .expect("valid event");
    assert_eq!(events, vec![LlmEvent::TextDelta("hello".to_string())]);
}

#[test]
fn parse_chat_event_emits_text_delta_for_part_with_delta_key() {
    // Responses-style content parts use `delta` rather than `text` for the
    // streamed increment. Accept both.
    let mut state = StreamState::default();
    let events = parse_chat_event(
        r#"{"choices":[{"delta":{"content":[{"type":"output_text_delta","delta":"world"}]}}]}"#,
        &mut state,
    )
    .expect("valid event");
    assert_eq!(events, vec![LlmEvent::TextDelta("world".to_string())]);
}

#[test]
fn parse_chat_event_emits_reasoning_delta_for_array_shape() {
    let mut state = StreamState::default();
    let events = parse_chat_event(
        r#"{"choices":[{"delta":{"reasoning_content":[{"type":"reasoning","text":"think"}]}}]}"#,
        &mut state,
    )
    .expect("valid event");
    assert_eq!(
        events,
        vec![LlmEvent::ReasoningDelta {
            text: "think".to_string(),
            kind: ReasoningKind::Summary,
        }]
    );
}

#[test]
fn reasoning_only_stop_emits_done_and_visible_notice() {
    // Qwen3/DeepSeek-R1-via-aggregator failure mode: the model emits only
    // `reasoning_content` deltas and finishes with `stop` — no `content`,
    // no `tool_calls`. Without the fallback the agent loop builds an empty
    // assistant message and the user sees the spinner stop with nothing
    // new in the transcript. The parser must (1) drain the streamed
    // reasoning into a `ReasoningDone` so it persists, and (2) inject a
    // visible `TextDelta` so the empty completion is *seen*.
    let mut state = StreamState::default();
    parse_chat_event(
        r#"{"choices":[{"delta":{"reasoning_content":"thinking hard..."}}]}"#,
        &mut state,
    )
    .expect("reasoning delta");
    let events = parse_chat_event(
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        &mut state,
    )
    .expect("stop");

    assert!(
        events
            .iter()
            .any(|e| matches!(e, LlmEvent::ReasoningDone(_))),
        "expected ReasoningDone to flush the streamed thinking: {events:?}"
    );
    let notice = events
        .iter()
        .find_map(|e| match e {
            LlmEvent::TextDelta(text) => Some(text.clone()),
            _ => None,
        })
        .expect("synthetic notice TextDelta");
    assert!(
        notice.contains("finish_reason=stop"),
        "notice must call out the reason: {notice}"
    );
}

#[test]
fn finish_stop_with_content_does_not_emit_notice() {
    // Regression guard: a normal completion that produced a real
    // assistant message must not get the empty-completion notice tacked
    // on. `saw_visible_output` latches on the first non-empty content
    // delta to suppress it.
    let mut state = StreamState::default();
    parse_chat_event(
        r#"{"choices":[{"delta":{"content":"hello world"}}]}"#,
        &mut state,
    )
    .expect("content delta");
    let events = parse_chat_event(
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        &mut state,
    )
    .expect("stop");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, LlmEvent::TextDelta(text) if text.contains("[squeezy]"))),
        "synthetic notice should NOT appear when the model produced content: {events:?}"
    );
}

#[test]
fn finish_length_emits_truncation_notice_and_drains_reasoning() {
    let mut state = StreamState::default();
    parse_chat_event(
        r#"{"choices":[{"delta":{"reasoning_content":"long thought..."}}]}"#,
        &mut state,
    )
    .expect("reasoning delta");
    let events = parse_chat_event(
        r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#,
        &mut state,
    )
    .expect("length");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, LlmEvent::ReasoningDone(_)))
    );
    let notice = events
        .iter()
        .find_map(|e| match e {
            LlmEvent::TextDelta(text) => Some(text.clone()),
            _ => None,
        })
        .expect("truncation notice");
    assert!(notice.contains("max_output_tokens"), "notice: {notice}");
}

#[test]
fn drain_tool_calls_skips_incomplete_entries_without_erroring() {
    // PortKey / OpenRouter sometimes ship a tool-call delta whose
    // `function.name` chunk goes missing or whose stream cuts mid-call.
    // The legacy hard-error killed the entire turn. We now skip the
    // incomplete entry and complete the stream cleanly.
    let mut state = StreamState::default();
    parse_chat_event(
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"x\":1}"}}]}}]}"#,
        &mut state,
    )
    .expect("partial tool call (no name)");
    // No name accumulated, but stream proceeds.
    let events = parse_chat_event(
        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        &mut state,
    )
    .expect("finish must not error");
    assert!(
        !events.iter().any(|e| matches!(e, LlmEvent::ToolCall(_))),
        "incomplete tool call must be skipped, not emitted: {events:?}"
    );
}

#[test]
fn parse_chat_event_ignores_unknown_delta_shapes_without_panic() {
    let mut state = StreamState::default();
    let events = parse_chat_event(r#"{"choices":[{"delta":{"content":42}}]}"#, &mut state)
        .expect("valid event");
    assert!(events.is_empty());
}

#[test]
fn parse_chat_event_accumulates_tool_call_across_deltas() {
    let mut state = StreamState::default();
    parse_chat_event(
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_x","type":"function","function":{"name":"grep"}}]}}]}"#,
        &mut state,
    )
    .expect("first delta");
    parse_chat_event(
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"pat"}}]}}]}"#,
        &mut state,
    )
    .expect("partial args");
    parse_chat_event(
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"tern\":\"todo\"}"}}]}}]}"#,
        &mut state,
    )
    .expect("more args");
    let events = parse_chat_event(
        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        &mut state,
    )
    .expect("finish");

    assert_eq!(events.len(), 1);
    let LlmEvent::ToolCall(call) = &events[0] else {
        panic!("expected tool call, got {:?}", events[0]);
    };
    assert_eq!(call.call_id, "call_x");
    assert_eq!(call.name, "grep");
    assert_eq!(call.arguments["pattern"], "todo");
}

#[test]
fn parse_chat_event_marks_invalid_tool_arguments() {
    let mut state = StreamState::default();
    parse_chat_event(
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"f","arguments":"{bad"}}]}}]}"#,
        &mut state,
    )
    .expect("ok");
    let events = parse_chat_event(
        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        &mut state,
    )
    .expect("finish");
    let LlmEvent::ToolCall(call) = &events[0] else {
        panic!("expected tool call");
    };
    assert_eq!(
        call.arguments[crate::INVALID_TOOL_ARGUMENTS_KEY],
        Value::Bool(true)
    );
    assert_eq!(
        call.arguments[crate::INVALID_TOOL_ARGUMENTS_RAW_KEY],
        Value::String("{bad".to_string())
    );
}

#[test]
fn parse_chat_event_captures_usage_for_cost() {
    let mut state = StreamState::default();
    parse_chat_event(
        r#"{"usage":{"prompt_tokens":120,"completion_tokens":80,"prompt_tokens_details":{"cached_tokens":40},"completion_tokens_details":{"reasoning_tokens":12}}}"#,
        &mut state,
    )
    .expect("usage");
    assert_eq!(state.cost.input_tokens, Some(120));
    assert_eq!(state.cost.output_tokens, Some(80));
    assert_eq!(state.cost.cached_input_tokens, Some(40));
    assert_eq!(state.cost.reasoning_output_tokens, Some(12));
}

#[test]
fn parse_chat_event_handles_done_sentinel() {
    let mut state = StreamState {
        response_id: Some("resp_2".to_string()),
        cost: squeezy_core::CostSnapshot {
            input_tokens: Some(10),
            output_tokens: Some(5),
            ..Default::default()
        },
        ..StreamState::default()
    };
    let events = parse_chat_event("[DONE]", &mut state).expect("done");
    assert_eq!(events.len(), 1);
    let LlmEvent::Completed {
        response_id, cost, ..
    } = &events[0]
    else {
        panic!("expected completed event");
    };
    assert_eq!(response_id.as_deref(), Some("resp_2"));
    assert_eq!(cost.input_tokens, Some(10));
    assert_eq!(cost.output_tokens, Some(5));
    assert!(state.completed_emitted);
}

#[test]
fn parse_chat_event_propagates_stream_error() {
    let mut state = StreamState::default();
    let err = parse_chat_event(
        r#"{"error":{"message":"rate limited","type":"rate_limit_error","code":"rate_limit_exceeded"}}"#,
        &mut state,
    )
    .expect_err("must surface error");
    let message = err.to_string();
    assert!(message.contains("rate limited"), "got: {message}");
    assert!(
        message.contains("type=rate_limit_error"),
        "must surface error.type for callers distinguishing retryable failures from auth bugs: {message}"
    );
    assert!(
        message.contains("code=rate_limit_exceeded"),
        "must surface error.code: {message}"
    );
}

#[test]
fn format_chat_error_handles_partial_envelopes() {
    let only_message: Value = serde_json::from_str(r#"{"error":{"message":"boom"}}"#).unwrap();
    assert_eq!(format_chat_error(&only_message, "fallback"), "boom");

    let only_type: Value =
        serde_json::from_str(r#"{"error":{"type":"invalid_request_error"}}"#).unwrap();
    assert_eq!(
        format_chat_error(&only_type, "fallback"),
        "fallback (type=invalid_request_error)"
    );

    let numeric_code: Value =
        serde_json::from_str(r#"{"error":{"message":"nope","code":429}}"#).unwrap();
    assert_eq!(
        format_chat_error(&numeric_code, "fallback"),
        "nope (code=429)"
    );

    let bare_string: Value = serde_json::from_str(r#"{"error":"insufficient quota"}"#).unwrap();
    assert_eq!(
        format_chat_error(&bare_string, "fallback"),
        "insufficient quota"
    );
}

#[test]
fn preset_defaults_round_trip() {
    for preset in OpenAiCompatiblePreset::all() {
        let canonical = preset.as_str();
        let parsed = OpenAiCompatiblePreset::parse(canonical)
            .unwrap_or_else(|| panic!("preset {canonical} must round-trip via parse"));
        assert_eq!(parsed, preset);
    }
}

#[test]
fn preset_default_headers_include_openrouter_attribution() {
    let headers = preset_default_headers(OpenAiCompatiblePreset::OpenRouter);
    assert_eq!(
        headers.get("HTTP-Referer").map(String::as_str),
        Some("https://github.com/esqueezy/squeezy"),
    );
    assert_eq!(headers.get("X-Title").map(String::as_str), Some("Squeezy"));

    let no_headers = preset_default_headers(OpenAiCompatiblePreset::Vercel);
    assert!(no_headers.is_empty());
}

#[test]
fn portkey_routing_header_present_detects_user_supplied_overrides() {
    let mut headers = BTreeMap::new();
    assert!(!portkey_routing_header_present(&headers));
    headers.insert("X-Other".to_string(), "v".to_string());
    assert!(!portkey_routing_header_present(&headers));
    headers.insert("x-portkey-virtual-key".to_string(), "vk-abc".to_string());
    assert!(portkey_routing_header_present(&headers));
    // Match is case-insensitive so user TOML casing doesn't matter.
    let mut mixed = BTreeMap::new();
    mixed.insert("X-Portkey-Config".to_string(), "cfg-1".to_string());
    assert!(portkey_routing_header_present(&mixed));
}

#[test]
fn request_body_passes_reasoning_effort_in_both_legacy_and_unified_shapes() {
    use squeezy_core::ReasoningEffort;
    let mut request = sample_request();
    request.reasoning_effort = Some(ReasoningEffort::High);
    let body = OpenAiCompatibleProvider::request_body(&request);
    assert_eq!(body["reasoning_effort"], "high");
    assert_eq!(body["reasoning"]["effort"], "high");
}

#[test]
fn request_body_omits_reasoning_when_caller_did_not_request_it() {
    let body = OpenAiCompatibleProvider::request_body(&sample_request());
    assert!(body.get("reasoning_effort").is_none());
    assert!(body.get("reasoning").is_none());
}

#[test]
fn request_body_attaches_anthropic_cache_control_when_cache_key_is_set() {
    let mut request = sample_request();
    request.cache_key = Some("repo-context".to_string());
    let body = OpenAiCompatibleProvider::request_body(&request);
    // System message becomes the array form with ephemeral cache_control.
    let system = &body["messages"][0];
    assert_eq!(system["role"], "system");
    assert_eq!(system["content"][0]["type"], "text");
    assert_eq!(system["content"][0]["cache_control"]["type"], "ephemeral");
    // Last user-text turn (the first input item in sample_request) gets the
    // breakpoint marker; later assistant/tool turns do not.
    let last_user = &body["messages"][1];
    assert_eq!(last_user["role"], "user");
    assert_eq!(
        last_user["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
    let assistant = &body["messages"][2];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(assistant["content"], "hi there");
}

#[test]
fn request_body_skips_cache_control_for_non_anthropic_routes() {
    let mut request = sample_request();
    request.model = "openai/gpt-5.5".to_string().into();
    request.cache_key = Some("repo-context".to_string());
    let body = OpenAiCompatibleProvider::request_body(&request);
    assert_eq!(body["messages"][0]["content"], "be brief");
    assert_eq!(body["messages"][1]["content"], "hello");
}

#[test]
fn request_body_skips_cache_control_when_no_cache_key() {
    let mut request = sample_request();
    request.model = "anthropic/claude-opus-4-7".to_string().into();
    request.cache_key = None;
    let body = OpenAiCompatibleProvider::request_body(&request);
    // System and user content stay as plain strings.
    assert_eq!(body["messages"][0]["content"], "be brief");
    assert_eq!(body["messages"][1]["content"], "hello");
}

#[test]
fn request_body_forwards_prompt_cache_key_to_openai_via_openrouter() {
    // OpenAI-via-OpenRouter (and any aggregator that forwards body fields
    // verbatim) honors the top-level `prompt_cache_key` for OpenAI's
    // prompt-cache layer. The Anthropic-only `cache_control` markers above
    // do not cover this case; `prompt_cache_key` carries the affinity hint
    // through to OpenAI-hosted models so cached-input billing applies.
    let mut request = sample_request();
    request.model = "openai/gpt-5.5".to_string().into();
    request.cache_key = Some("repo-context".to_string());
    let body = OpenAiCompatibleProvider::request_body(&request);
    assert_eq!(body["prompt_cache_key"], "repo-context");
}

#[test]
fn request_body_forwards_prompt_cache_key_alongside_anthropic_cache_control() {
    // Anthropic-via-OpenRouter gets the ephemeral `cache_control` markers,
    // and `prompt_cache_key` rides along as a top-level hint. Aggregators
    // that ignore unknown fields drop it harmlessly; OpenAI receives it.
    let mut request = sample_request();
    request.model = "anthropic/claude-opus-4-7".to_string().into();
    request.cache_key = Some("repo-context".to_string());
    let body = OpenAiCompatibleProvider::request_body(&request);
    assert_eq!(body["prompt_cache_key"], "repo-context");
    assert_eq!(
        body["messages"][0]["content"][0]["cache_control"]["type"],
        "ephemeral",
    );
}

#[test]
fn request_body_omits_prompt_cache_key_when_unset() {
    let body = OpenAiCompatibleProvider::request_body(&sample_request());
    assert!(body.get("prompt_cache_key").is_none());
}

#[test]
fn classify_recognizes_known_namespaces() {
    // The typed compat table is the single source of truth for namespace
    // → wire-shape decisions. Every known vendor prefix must classify to
    // its declared flavor so adding/expanding an aggregator only requires
    // a row in COMPAT_TABLE, not a fresh substring test in request_body.
    assert_eq!(
        classify("anthropic/claude-opus-4-7"),
        CompatFlavor::AnthropicCompat,
    );
    assert_eq!(classify("openai/gpt-5.5"), CompatFlavor::OpenAi);
    assert_eq!(
        classify("google/gemini-2.5-pro"),
        CompatFlavor::GoogleCompat
    );
    assert_eq!(classify("xai/grok-4"), CompatFlavor::XaiCompat);
}

#[test]
fn classify_is_case_insensitive() {
    // User-supplied model strings (config files, env overrides) can show
    // up with arbitrary casing. The match runs against the lowercased
    // form so casing never silently disables a capability flag.
    assert_eq!(
        classify("Anthropic/Claude-Opus-4-7"),
        CompatFlavor::AnthropicCompat,
    );
    assert_eq!(classify("OPENAI/GPT-5.5"), CompatFlavor::OpenAi);
}

#[test]
fn classify_falls_back_to_generic_for_unknown_namespace() {
    // Unknown namespaces (custom self-hosted ids, brand-new aggregators)
    // must fall through to Generic instead of crashing or accidentally
    // picking up Anthropic-style cache markers. Mirrors pi's behavior in
    // `others/pi/packages/ai/src/types.ts` where compat overrides default
    // to "ignore" rather than "panic" for unknown providers.
    assert_eq!(classify("groq/llama-3.3-70b"), CompatFlavor::Generic);
    assert_eq!(classify("custom-self-hosted-model"), CompatFlavor::Generic);
    assert_eq!(classify(""), CompatFlavor::Generic);
}

#[test]
fn compat_entry_exposes_capability_flags_for_anthropic() {
    // Reading the entry directly is the typed alternative to
    // `model.starts_with("anthropic/")`. Callers that need the cache
    // flag specifically can branch on the bool without re-deriving the
    // namespace.
    let entry =
        compat_entry("anthropic/claude-3.7-sonnet").expect("anthropic/ prefix must classify");
    assert_eq!(entry.flavor, CompatFlavor::AnthropicCompat);
    assert!(entry.supports_cache_control);
    assert!(entry.supports_tool_calls);
    assert!(entry.supports_reasoning);
}

#[test]
fn compat_entry_marks_non_anthropic_namespaces_as_cache_disabled() {
    // Behavior parity with the legacy `starts_with("anthropic/")`
    // substring test: every non-Anthropic namespace must report
    // `supports_cache_control == false` so request_body never attaches
    // ephemeral cache markers to a route that would silently drop them.
    for model in [
        "openai/gpt-5.5",
        "google/gemini-2.5-pro",
        "xai/grok-4",
        "groq/llama-3.3-70b",
        "unknown",
    ] {
        let cache_control = compat_entry(model).is_some_and(|e| e.supports_cache_control);
        assert!(
            !cache_control,
            "{model} must not opt into anthropic cache_control",
        );
    }
}

#[test]
fn compat_table_prefixes_are_lowercase() {
    // Invariant: prefixes must be stored lowercased because lookup
    // lowercases the input. A capitalized prefix in the table would
    // silently never match and the row would become dead code.
    for entry in COMPAT_TABLE {
        assert_eq!(
            entry.model_prefix,
            entry.model_prefix.to_ascii_lowercase(),
            "compat-table prefix must be lowercase: {}",
            entry.model_prefix,
        );
    }
}

#[test]
fn preset_full_tier_matches_documented_set() {
    let full: Vec<_> = OpenAiCompatiblePreset::all()
        .iter()
        .copied()
        .filter(|p| p.is_full_tier())
        .collect();
    assert_eq!(
        full,
        vec![
            OpenAiCompatiblePreset::OpenRouter,
            OpenAiCompatiblePreset::Vercel,
            OpenAiCompatiblePreset::PortKey,
            OpenAiCompatiblePreset::Groq,
            OpenAiCompatiblePreset::XAi,
            OpenAiCompatiblePreset::DeepSeek,
            OpenAiCompatiblePreset::Vertex,
        ]
    );
}
