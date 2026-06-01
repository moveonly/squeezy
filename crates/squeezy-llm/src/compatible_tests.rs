use super::*;
use crate::{CacheSpec, LlmEvent, LlmInputItem, LlmToolSpec};
use serde_json::{Value, json};
use squeezy_core::{
    DEFAULT_CLOUDFLARE_AI_GATEWAY_BASE_URL, DEFAULT_CLOUDFLARE_WORKERS_AI_BASE_URL,
    OpenAiCompatibleConfig, OpenAiCompatiblePreset, ProviderTransportConfig,
};
use std::sync::{Arc, Mutex, OnceLock};

/// Serialize tests that mutate process-wide env vars. The
/// Cloudflare AI Gateway dual-auth path peeks at `CF_UPSTREAM_KEY`
/// and Rust's test runner runs cases in parallel by default, so
/// without this lock two cases racing on the same env var would
/// silently observe each other's writes.
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

fn sample_request() -> LlmRequest {
    LlmRequest {
        model: "anthropic/claude-opus-4-7".to_string().into(),
        instructions: "be brief".to_string().into(),
        // The orphan `FunctionCallOutput` (no preceding
        // `FunctionCall` with the same `call_id`) is the cross-model
        // hazard the F11 normalization handles: after the
        // `request_body` runs the input through
        // `normalize_tool_ids_for_replay`, a placeholder
        // `model_switched` `FunctionCall` is synthesized in front of
        // this output so the wire format stays well-formed for every
        // destination provider. Tests assert on the *normalized*
        // shape (4 input messages, with `tool_call_id = "call_1"`).
        input: Arc::from(vec![
            LlmInputItem::UserText("hello".to_string()),
            LlmInputItem::AssistantText("hi there".to_string()),
            LlmInputItem::FunctionCallOutput {
                call_id: "call_42".to_string(),
                output: r#"{"result":"ok"}"#.to_string(),
                content_parts: None,
                is_error: false,
            },
        ]),
        max_output_tokens: Some(128),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        cache: CacheSpec::default(),
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
        ..LlmRequest::default()
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
    // Normalization inserts a synthetic `model_switched` assistant
    // tool_call ahead of the orphan tool result, so the body now
    // carries system + user + assistant text + synthetic assistant
    // tool_calls + tool result = 5 messages.
    assert_eq!(
        messages.len(),
        5,
        "system + 3 input items + synthetic tool call"
    );
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], "be brief");
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], "hello");
    assert_eq!(messages[2]["role"], "assistant");
    assert_eq!(messages[2]["content"], "hi there");
    assert_eq!(messages[3]["role"], "assistant");
    assert_eq!(
        messages[3]["tool_calls"][0]["function"]["name"],
        crate::MODEL_SWITCHED_PLACEHOLDER_NAME,
    );
    assert_eq!(messages[3]["tool_calls"][0]["id"], "call_1");
    assert_eq!(messages[4]["role"], "tool");
    assert_eq!(messages[4]["tool_call_id"], "call_1");
    assert_eq!(messages[4]["content"], r#"{"result":"ok"}"#);

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
    // No system message + 3 original input items + 1 synthetic
    // `model_switched` assistant tool_call inserted ahead of the
    // orphan tool result = 4 messages.
    assert_eq!(messages.len(), 4);
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
        cache: CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: std::sync::Arc::from(Vec::new()),
        ..LlmRequest::default()
    };
    let body = OpenAiCompatibleProvider::request_body(&request);
    let messages = body["messages"].as_array().expect("messages array");
    let assistant_call = &messages[1];
    assert_eq!(assistant_call["role"], "assistant");
    let tool_call = &assistant_call["tool_calls"][0];
    // The original `call_99` is canonicalized to `call_1` so a
    // mid-session model switch can replay this turn against a
    // provider with stricter id-shape rules (Anthropic regex,
    // Bedrock pairing, etc.) without rewriting the persisted
    // history.
    assert_eq!(tool_call["id"], "call_1");
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
fn finish_content_filter_emits_notice_and_drains_reasoning() {
    // The content-filter exit path (`finish_reason="content_filter"`) lands
    // when an upstream guardrail rejects the in-flight assistant output.
    // The parser must (1) flush any reasoning streamed up to the block
    // into a `ReasoningDone` so the partial thinking persists, and (2)
    // inject a visible `TextDelta` so the user sees *why* the turn
    // truncated instead of a silent empty assistant message — local
    // self-hosted servers behind a moderation reverse-proxy hit this
    // path most often.
    let mut state = StreamState::default();
    parse_chat_event(
        r#"{"choices":[{"delta":{"reasoning_content":"weighing options"}}]}"#,
        &mut state,
    )
    .expect("reasoning delta");
    let events = parse_chat_event(
        r#"{"choices":[{"delta":{},"finish_reason":"content_filter"}]}"#,
        &mut state,
    )
    .expect("content_filter");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, LlmEvent::ReasoningDone(_))),
        "expected ReasoningDone to flush thinking before the filter exit notice: {events:?}"
    );
    let notice = events
        .iter()
        .find_map(|e| match e {
            LlmEvent::TextDelta(text) => Some(text.clone()),
            _ => None,
        })
        .expect("content filter notice");
    assert!(
        notice.contains("content_filter"),
        "notice must call out the filter exit: {notice}"
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
fn drain_tool_calls_emits_null_arguments_when_function_args_empty() {
    // M-29: when the model commits to a zero-arg tool call the upstream
    // streams `function.arguments` as an empty string. The drain must
    // surface that as `Value::Null` so the tool dispatch layer can
    // disambiguate "no arguments" from "arguments was the empty object".
    // The legacy behavior fabricated `{}` and stripped the distinction.
    let mut state = StreamState::default();
    parse_chat_event(
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"now"}}]}}]}"#,
        &mut state,
    )
    .expect("zero-arg tool call delta");
    let events = parse_chat_event(
        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
        &mut state,
    )
    .expect("finish");
    let LlmEvent::ToolCall(call) = &events[0] else {
        panic!("expected ToolCall, got {events:?}");
    };
    assert_eq!(call.name, "now");
    assert_eq!(
        call.arguments,
        Value::Null,
        "empty function.arguments must surface as Value::Null, not {{}}"
    );
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
fn request_body_pins_n_to_one() {
    // H-24: pin n: 1 explicitly so an upstream server that
    // defaults to n > 1 (rare but legal under Chat Completions)
    // cannot silently double-bill. The streamed parser only honors
    // choices[0]; any additional choices and their tool calls
    // would be dropped.
    let body = OpenAiCompatibleProvider::request_body(&sample_request());
    assert_eq!(body["n"], 1, "n must always be pinned to 1 in the body");
}

#[test]
fn parse_chat_event_partitions_tool_calls_across_distinct_choices() {
    // H-24: two choices each populating `index = 0` for their own
    // tool call must NOT collapse into a single accumulator. The
    // tool_calls map is keyed on (choice_index, tool_index) so
    // both calls survive the drain.
    let mut state = StreamState::default();
    parse_chat_event(
        r#"{"choices":[
            {"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_a","function":{"name":"grep","arguments":"{\"x\":1}"}}]}},
            {"index":1,"delta":{"tool_calls":[{"index":0,"id":"call_b","function":{"name":"read","arguments":"{\"y\":2}"}}]}}
        ]}"#,
        &mut state,
    )
    .expect("delta with two choices");
    let events = parse_chat_event(
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        &mut state,
    )
    .expect("finish");
    let tool_calls: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            LlmEvent::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        tool_calls.len(),
        2,
        "both choices' tool calls must survive partition: {tool_calls:?}"
    );
    let names: Vec<_> = tool_calls.iter().map(|c| c.name.as_str()).collect();
    assert!(names.contains(&"grep"));
    assert!(names.contains(&"read"));
    let call_ids: Vec<_> = tool_calls.iter().map(|c| c.call_id.as_str()).collect();
    assert!(call_ids.contains(&"call_a"));
    assert!(call_ids.contains(&"call_b"));
}

#[test]
fn parse_chat_event_treats_missing_tool_index_as_continuation_of_active_index() {
    // H-24: when an aggregator (Anthropic-via-OpenRouter relaying
    // content_block_delta, some PortKey upstreams) drops the
    // `index` field on a continuation delta, the field MUST be
    // treated as a continuation of the most-recent active index
    // on the same choice — NOT silently rewritten to `0`. The old
    // `unwrap_or(0)` collapsed parallel call 1's args into call 0.
    let mut state = StreamState::default();
    // Open two parallel tool calls (index 0 and 1) on the same choice.
    parse_chat_event(
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[
            {"index":0,"id":"call_a","function":{"name":"grep","arguments":"{\"x"}},
            {"index":1,"id":"call_b","function":{"name":"read","arguments":"{\"y"}}
        ]}}]}"#,
        &mut state,
    )
    .expect("two parallel calls");
    // Continuation delta omits `index`. The previous code path
    // would route to index 0; the H-24 fix routes to the highest
    // active index (1).
    parse_chat_event(
        r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"function":{"arguments":"\":2}"}}]}}]}"#,
        &mut state,
    )
    .expect("missing-index continuation");
    let events = parse_chat_event(
        r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
        &mut state,
    )
    .expect("finish");
    let tool_calls: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            LlmEvent::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(tool_calls.len(), 2);
    let grep = tool_calls
        .iter()
        .find(|c| c.name == "grep")
        .expect("call 0 still present");
    // call 0's args were never closed; the parser marks them
    // INVALID_TOOL_ARGUMENTS.
    assert_eq!(
        grep.arguments[crate::INVALID_TOOL_ARGUMENTS_KEY],
        Value::Bool(true),
        "call 0 should NOT have received call 1's continuation"
    );
    let read = tool_calls
        .iter()
        .find(|c| c.name == "read")
        .expect("call 1 still present");
    assert_eq!(
        read.arguments["y"], 2,
        "missing-index continuation must route to the highest active index"
    );
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
fn finish_reason_stop_followed_by_trailing_usage_chunk_captures_cost() {
    // C-10: Groq, OpenRouter-via-Groq, and native OpenAI ship the
    // final `usage` envelope in a chunk *after* the one carrying
    // `finish_reason: "stop"` and before the terminal `[DONE]`. If
    // the stop handler latches `completed_emitted = true`, the outer
    // stream loop short-circuits and the trailing usage payload is
    // discarded — cost gets reported as zero. Pin the wire order
    // here so a future refactor that accidentally re-flips the flag
    // is caught by CI instead of by a silent billing regression.
    let mut state = StreamState::default();
    parse_chat_event(
        r#"{"id":"resp_g","choices":[{"delta":{"content":"hello"}}]}"#,
        &mut state,
    )
    .expect("content delta");
    // Chunk #2 carries the terminal finish_reason but no usage.
    let stop_events = parse_chat_event(
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        &mut state,
    )
    .expect("stop");
    assert!(
        !state.completed_emitted,
        "stop arm must not flip completed_emitted — trailing usage chunks must still parse: {stop_events:?}"
    );
    assert_eq!(state.cost.input_tokens, None);
    assert_eq!(state.cost.output_tokens, None);
    // Chunk #3 carries usage but `choices: []`.
    parse_chat_event(
        r#"{"choices":[],"usage":{"prompt_tokens":123,"completion_tokens":45}}"#,
        &mut state,
    )
    .expect("usage chunk after finish_reason: stop must still parse");
    assert_eq!(
        state.cost.input_tokens,
        Some(123),
        "trailing usage chunk must update state.cost"
    );
    assert_eq!(state.cost.output_tokens, Some(45));
    // Chunk #4: `[DONE]` finally emits Completed with the captured
    // cost.
    let done_events = parse_chat_event("[DONE]", &mut state).expect("done");
    let LlmEvent::Completed { cost, .. } = done_events
        .iter()
        .find(|e| matches!(e, LlmEvent::Completed { .. }))
        .expect("Completed event")
    else {
        unreachable!()
    };
    assert_eq!(cost.input_tokens, Some(123));
    assert_eq!(cost.output_tokens, Some(45));
    assert!(state.completed_emitted, "[DONE] must latch completion");
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
fn local_jit_load_hint_attaches_for_lmstudio_400_not_loaded() {
    // LM Studio returns `400 Bad Request` with an upstream message that
    // contains "not loaded" when the user pointed `model = "<id>"` at a
    // checkpoint the server hasn't loaded into memory. The hint must
    // point at the LM Studio-specific fix (`lms load <model>`) so the
    // user does not have to guess which CLI to reach for.
    let hint = local_jit_load_hint(
        OpenAiCompatiblePreset::LMStudio,
        StatusCode::BAD_REQUEST,
        "Model 'qwen3-32b' is not loaded",
    );
    assert!(
        hint.contains("lms load"),
        "LM Studio hint must surface the `lms load` CLI guidance: {hint}"
    );
}

#[test]
fn local_jit_load_hint_attaches_for_vllm_400_no_models_loaded() {
    // vLLM surfaces "no models loaded" / "model not loaded" on a 400
    // when the served checkpoint id does not match what `vllm serve`
    // was launched with. The hint points at the `--model` startup flag.
    let hint = local_jit_load_hint(
        OpenAiCompatiblePreset::VLlm,
        StatusCode::BAD_REQUEST,
        "no models loaded; check --model startup flag",
    );
    assert!(
        hint.contains("vllm serve"),
        "vLLM hint must reference `vllm serve`: {hint}"
    );
}

#[test]
fn local_jit_load_hint_attaches_for_llamacpp_400_not_loaded() {
    // llama.cpp's HTTP server returns 400 with "model is not loaded"
    // when launched without `-m <path>`. Surface the `llama-server -m`
    // fix so the user does not have to chase the upstream README.
    let hint = local_jit_load_hint(
        OpenAiCompatiblePreset::LlamaCpp,
        StatusCode::BAD_REQUEST,
        "model is not loaded",
    );
    assert!(
        hint.contains("llama-server -m"),
        "llama.cpp hint must reference the `llama-server -m` invocation: {hint}"
    );
}

#[test]
fn local_jit_load_hint_returns_empty_for_non_400_or_unrelated_body() {
    // 401 / 500 / etc must not get the JIT-load hint — those are auth /
    // upstream-crash failures, not "checkpoint missing" failures.
    assert_eq!(
        local_jit_load_hint(
            OpenAiCompatiblePreset::LMStudio,
            StatusCode::UNAUTHORIZED,
            "Model 'qwen3-32b' is not loaded",
        ),
        ""
    );
    // 400 without the "not loaded" sentinel must also leave the hint
    // empty so unrelated bad-request errors (malformed prompt, oversized
    // input) surface without misleading guidance attached.
    assert_eq!(
        local_jit_load_hint(
            OpenAiCompatiblePreset::LMStudio,
            StatusCode::BAD_REQUEST,
            "prompt too long",
        ),
        ""
    );
}

#[test]
fn local_jit_load_hint_returns_empty_for_remote_presets() {
    // Only the three local presets get the hint — adding it to
    // OpenRouter / Vercel / etc. would mislead users when the upstream
    // (aggregator) returns 400 for an unrelated reason.
    for preset in [
        OpenAiCompatiblePreset::OpenRouter,
        OpenAiCompatiblePreset::Vercel,
        OpenAiCompatiblePreset::Groq,
        OpenAiCompatiblePreset::PortKey,
    ] {
        let hint = local_jit_load_hint(preset, StatusCode::BAD_REQUEST, "model is not loaded");
        assert_eq!(hint, "", "preset {preset:?} must not get the JIT-load hint");
    }
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
fn request_body_marks_last_tool_with_cache_control_for_anthropic_routes() {
    // Regression guard for the per-provider drift the centralized
    // cache_policy module exists to prevent: the native Anthropic
    // adapter marks the trailing tool entry with `cache_control`, and
    // Anthropic-via-aggregator routes must do the same so the cached
    // tool prefix actually hits on the next turn. Without this the
    // aggregator route bills the tool list as fresh-input tokens on
    // every multi-turn coding session.
    let mut request = sample_request();
    request.cache_key = Some("repo-context".to_string());
    let body = OpenAiCompatibleProvider::request_body(&request);
    let tools = body["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["function"]["name"], "grep");
    assert_eq!(
        tools[0]["cache_control"]["type"], "ephemeral",
        "Anthropic-via-aggregator route must mark the last tool entry, mirroring native Anthropic"
    );
}

#[test]
fn request_body_marks_last_stable_tool_skipping_trailing_dynamic_mcp_tools() {
    // The tool registry pushes MCP-sourced tools (whose names carry the
    // `mcp__` prefix) to the end of the advertised list. The cache
    // breakpoint must sit on the last first-party tool so an MCP
    // `tools/list` refresh that reorders or replaces dynamic entries
    // does not invalidate the cached tool prefix.
    let mut request = sample_request();
    request.cache_key = Some("repo-context".to_string());
    request.tools = Arc::from(vec![
        LlmToolSpec {
            name: "grep".to_string(),
            description: "search files".to_string(),
            parameters: json!({"type": "object"}),
            strict: true,
        }
        .into(),
        LlmToolSpec {
            name: "read".to_string(),
            description: "read file".to_string(),
            parameters: json!({"type": "object"}),
            strict: true,
        }
        .into(),
        LlmToolSpec {
            name: "mcp__github__list_issues".to_string(),
            description: "list github issues".to_string(),
            parameters: json!({"type": "object"}),
            strict: true,
        }
        .into(),
    ]);
    let body = OpenAiCompatibleProvider::request_body(&request);
    let tools = body["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 3);
    assert!(tools[0].get("cache_control").is_none());
    assert_eq!(
        tools[1]["cache_control"]["type"], "ephemeral",
        "breakpoint must land on the last first-party tool, not on the MCP tail"
    );
    assert!(tools[2].get("cache_control").is_none());
}

#[test]
fn request_body_omits_tool_cache_control_for_non_anthropic_routes() {
    // The Anthropic-flavoured cache_control markers must not bleed onto
    // OpenAI-via-aggregator (or any non-anthropic/* route). Those routes
    // rely on the top-level `prompt_cache_key` instead — verified by the
    // separate `request_body_forwards_prompt_cache_key_to_openai_via_openrouter`
    // test — and OpenAI rejects unknown `cache_control` fields on tool
    // entries with a 400.
    let mut request = sample_request();
    request.model = "openai/gpt-5.5".to_string().into();
    request.cache_key = Some("repo-context".to_string());
    let body = OpenAiCompatibleProvider::request_body(&request);
    let tools = body["tools"].as_array().expect("tools array");
    assert!(
        tools[0].get("cache_control").is_none(),
        "openai/* aggregator routes must not carry Anthropic-style cache_control"
    );
}

#[test]
fn request_body_omits_tool_cache_control_when_no_cache_key() {
    // No cache_key on the request → no markers anywhere, including on
    // the tool list. Avoids billing for cache writes on short, one-shot
    // calls where reads will not amortize the write cost.
    let mut request = sample_request();
    request.model = "anthropic/claude-opus-4-7".to_string().into();
    request.cache_key = None;
    let body = OpenAiCompatibleProvider::request_body(&request);
    let tools = body["tools"].as_array().expect("tools array");
    assert!(
        tools[0].get("cache_control").is_none(),
        "no cache_key → no cache_control on tools"
    );
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
    assert!(body.get("prompt_cache_retention").is_none());
}

#[test]
fn request_body_emits_prompt_cache_retention_24h_for_long_retention_openai_route() {
    // F11: OpenAI-via-OpenRouter (Chat Completions route) must surface
    // `CacheRetention::Long` as the top-level `prompt_cache_retention: "24h"`
    // body field so the cached prefix lifetime matches the native OpenAI
    // provider.
    let mut request = sample_request();
    request.model = "openai/gpt-5.5".to_string().into();
    request.cache = crate::CacheSpec {
        key: Some("repo-context".to_string()),
        retention: crate::CacheRetention::Long,
    };
    let body = OpenAiCompatibleProvider::request_body(&request);
    assert_eq!(body["prompt_cache_key"], "repo-context");
    assert_eq!(
        body["prompt_cache_retention"], "24h",
        "Long retention must propagate to the chat-completions body field"
    );
}

#[test]
fn request_body_emits_one_hour_ttl_marker_for_long_retention_anthropic_aggregator() {
    // F11: Anthropic-via-aggregator routes must mirror the native
    // Anthropic adapter — `CacheRetention::Long` upgrades every breakpoint
    // marker to `cache_control: { type: "ephemeral", ttl: "1h" }` so the
    // cached prefix survives Anthropic's default short window.
    let mut request = sample_request();
    request.model = "anthropic/claude-opus-4-7".to_string().into();
    request.cache = crate::CacheSpec {
        key: Some("repo-context".to_string()),
        retention: crate::CacheRetention::Long,
    };
    let body = OpenAiCompatibleProvider::request_body(&request);
    let system = &body["messages"][0];
    assert_eq!(system["content"][0]["cache_control"]["ttl"], "1h");
    let last_user = &body["messages"][1];
    assert_eq!(last_user["content"][0]["cache_control"]["ttl"], "1h");
    let tools = body["tools"].as_array().expect("tools array");
    assert_eq!(tools[0]["cache_control"]["ttl"], "1h");
}

#[test]
fn request_body_omits_prompt_cache_retention_for_short_retention_legacy_cache_key() {
    // Regression guard: callers using the deprecated `cache_key` field
    // get `Short` retention via `effective_cache_spec()`, which must
    // leave `prompt_cache_retention` off the wire.
    let mut request = sample_request();
    request.model = "openai/gpt-5.5".to_string().into();
    request.cache_key = Some("repo-context".to_string());
    let body = OpenAiCompatibleProvider::request_body(&request);
    assert_eq!(body["prompt_cache_key"], "repo-context");
    assert!(body.get("prompt_cache_retention").is_none());
}

#[test]
fn request_body_clamps_prompt_cache_key_for_openai_aggregator_route() {
    // F11: aggregator routes that forward `prompt_cache_key` verbatim to
    // OpenAI must also clamp to the 64-codepoint limit so the
    // OpenRouter → OpenAI hop does not silently drop the field.
    let mut request = sample_request();
    request.model = "openai/gpt-5.5".to_string().into();
    let long_key: String = "a".repeat(100);
    request.cache_key = Some(long_key);
    let body = OpenAiCompatibleProvider::request_body(&request);
    let emitted = body["prompt_cache_key"]
        .as_str()
        .expect("prompt_cache_key must be emitted");
    assert_eq!(emitted.chars().count(), 64);
    assert_eq!(emitted, "a".repeat(64));
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
    // picking up Anthropic-style cache markers. Compat overrides default
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

#[test]
fn request_body_encodes_image_as_image_url_data_url() {
    let bytes: Arc<[u8]> = Arc::from(vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    let mut request = sample_request();
    // Wipe the prior input shape so we can focus on the image encoding;
    // text-only items already have full coverage above.
    request.input = Arc::from(vec![
        LlmInputItem::UserText("what is this?".to_string()),
        LlmInputItem::Image {
            media_type: "image/png".to_string(),
            bytes: bytes.clone(),
        },
    ]);

    let body = OpenAiCompatibleProvider::request_body(&request);
    let messages = body["messages"].as_array().expect("messages array");
    // system + user text + user image
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], "what is this?");
    assert_eq!(messages[2]["role"], "user");
    let image_part = &messages[2]["content"][0];
    assert_eq!(image_part["type"], "image_url");
    let url = image_part["image_url"]["url"]
        .as_str()
        .expect("data URL string");
    assert!(
        url.starts_with("data:image/png;base64,"),
        "Chat Completions image must use a data URL: `{url}`"
    );
    use base64::Engine as _;
    let encoded = url
        .strip_prefix("data:image/png;base64,")
        .expect("data URL prefix");
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .expect("valid base64");
    assert_eq!(decoded.as_slice(), bytes.as_ref());
}

#[test]
fn cloudflare_presets_substitute_account_and_gateway_placeholders_in_base_url() {
    // Both Cloudflare presets ship templated default base URLs so the
    // configuration layer can flow `account_id` / `gateway_id` through
    // verbatim and let `OpenAiCompatibleProvider::from_config` resolve
    // the per-account / per-gateway path right before requests fire.
    // The resolved URL on the constructed provider must reflect every
    // placeholder having been replaced — including a trailing-slash
    // trim — so the chat-completions request format string in
    // `stream_response` produces a clean URL.
    let workers_ai = OpenAiCompatibleProvider::from_config(&OpenAiCompatibleConfig {
        preset: OpenAiCompatiblePreset::CloudflareWorkersAi,
        api_key_env: "CLOUDFLARE_API_KEY".to_string(),
        api_key: Some("inline-key".to_string()),
        base_url: DEFAULT_CLOUDFLARE_WORKERS_AI_BASE_URL.to_string(),
        extra_headers: BTreeMap::new(),
        transport: ProviderTransportConfig::default(),
        account_id: Some("acct-abc".to_string()),
        gateway_id: None,
        deployment_id: None,
        cf_ai_gateway: None,
        use_oauth: false,
    })
    .expect("workers AI provider builds with account_id");
    assert_eq!(
        workers_ai.base_url(),
        "https://api.cloudflare.com/client/v4/accounts/acct-abc/ai/v1",
        "the {{account_id}} placeholder must be substituted into the Workers AI URL",
    );

    let gateway = OpenAiCompatibleProvider::from_config(&OpenAiCompatibleConfig {
        preset: OpenAiCompatiblePreset::CloudflareAiGateway,
        api_key_env: "CLOUDFLARE_API_KEY".to_string(),
        api_key: Some("inline-key".to_string()),
        base_url: DEFAULT_CLOUDFLARE_AI_GATEWAY_BASE_URL.to_string(),
        extra_headers: BTreeMap::new(),
        transport: ProviderTransportConfig::default(),
        account_id: Some("acct-abc".to_string()),
        gateway_id: Some("my-gateway".to_string()),
        deployment_id: None,
        cf_ai_gateway: None,
        use_oauth: false,
    })
    .expect("AI Gateway provider builds with account_id + gateway_id");
    assert_eq!(
        gateway.base_url(),
        "https://gateway.ai.cloudflare.com/v1/acct-abc/my-gateway/compat",
        "both {{account_id}} and {{gateway_id}} must be substituted into the AI Gateway URL",
    );
}

#[test]
fn cloudflare_workers_ai_missing_account_id_fails_with_clear_error() {
    // Misconfiguration guard: when the base URL still contains a
    // placeholder but the corresponding `OpenAiCompatibleConfig` field
    // is unset, `from_config` must surface a `ProviderNotConfigured`
    // error that names both the offending placeholder and the
    // TOML / env-var the user has to set. Anything less and the
    // request would fire against a literal `{account_id}` URL and the
    // user would see only a 404 from Cloudflare's edge.
    let error = OpenAiCompatibleProvider::from_config(&OpenAiCompatibleConfig {
        preset: OpenAiCompatiblePreset::CloudflareWorkersAi,
        api_key_env: "CLOUDFLARE_API_KEY".to_string(),
        api_key: Some("inline-key".to_string()),
        base_url: DEFAULT_CLOUDFLARE_WORKERS_AI_BASE_URL.to_string(),
        extra_headers: BTreeMap::new(),
        transport: ProviderTransportConfig::default(),
        account_id: None,
        gateway_id: None,
        deployment_id: None,
        cf_ai_gateway: None,
        use_oauth: false,
    })
    .expect_err("missing account_id must fail provider construction");
    assert!(
        matches!(error, SqueezyError::ProviderNotConfigured(_)),
        "missing placeholder must map to ProviderNotConfigured, got: {error:?}"
    );
    let message = error.to_string();
    assert!(
        message.contains("{account_id}"),
        "error must name the offending placeholder so the user knows what to set: {message}"
    );
    assert!(
        message.contains("cloudflare_account_id"),
        "error must point at the TOML field the user can populate: {message}"
    );
    assert!(
        message.contains("CLOUDFLARE_ACCOUNT_ID"),
        "error must point at the env var the user can populate: {message}"
    );

    // Whitespace-only `account_id` is treated the same as missing —
    // `Some(\"   \")` would silently produce a URL with an empty
    // account segment otherwise.
    let whitespace_error = OpenAiCompatibleProvider::from_config(&OpenAiCompatibleConfig {
        preset: OpenAiCompatiblePreset::CloudflareWorkersAi,
        api_key_env: "CLOUDFLARE_API_KEY".to_string(),
        api_key: Some("inline-key".to_string()),
        base_url: DEFAULT_CLOUDFLARE_WORKERS_AI_BASE_URL.to_string(),
        extra_headers: BTreeMap::new(),
        transport: ProviderTransportConfig::default(),
        account_id: Some("   ".to_string()),
        gateway_id: None,
        deployment_id: None,
        cf_ai_gateway: None,
        use_oauth: false,
    })
    .expect_err("whitespace-only account_id must also fail");
    assert!(
        whitespace_error.to_string().contains("{account_id}"),
        "whitespace error must also name the placeholder: {whitespace_error}"
    );
}

#[test]
fn is_local_preset_classifies_lmstudio_vllm_llamacpp_as_local() {
    // X-17 hinges on this classifier: the three local-hosted presets
    // tolerate an empty resolved API key. Anything that drifts into or
    // out of this set must show up as a test failure so a future preset
    // addition cannot silently inherit the no-auth path.
    assert!(is_local_preset(OpenAiCompatiblePreset::LMStudio));
    assert!(is_local_preset(OpenAiCompatiblePreset::VLlm));
    assert!(is_local_preset(OpenAiCompatiblePreset::LlamaCpp));
    for preset in [
        OpenAiCompatiblePreset::OpenRouter,
        OpenAiCompatiblePreset::Vercel,
        OpenAiCompatiblePreset::PortKey,
        OpenAiCompatiblePreset::Groq,
        OpenAiCompatiblePreset::XAi,
        OpenAiCompatiblePreset::DeepSeek,
        OpenAiCompatiblePreset::Vertex,
        OpenAiCompatiblePreset::Mistral,
        OpenAiCompatiblePreset::Together,
        OpenAiCompatiblePreset::Fireworks,
        OpenAiCompatiblePreset::Cerebras,
        OpenAiCompatiblePreset::DeepInfra,
        OpenAiCompatiblePreset::Baseten,
        OpenAiCompatiblePreset::CloudflareWorkersAi,
        OpenAiCompatiblePreset::CloudflareAiGateway,
        OpenAiCompatiblePreset::Custom,
    ] {
        assert!(
            !is_local_preset(preset),
            "preset {preset:?} must not classify as a local self-hosted preset",
        );
    }
}

#[test]
fn local_preset_builds_without_inline_or_env_api_key() {
    // X-17: LM Studio / vLLM / llama.cpp run unauthenticated by default.
    // `from_config` must not error when neither inline nor env carries a
    // key; instead the resolved key flows as `""` and the stream path
    // short-circuits the `Authorization: Bearer` header. Construct the
    // provider with no inline key and a deliberately-not-set env var
    // name so this regression-tests on a clean process too.
    let env_var = "SQUEEZY_X17_DEFINITELY_NOT_SET_LMSTUDIO";
    // Make sure no stale value from a prior test leaks in.
    unsafe {
        std::env::remove_var(env_var);
    }
    let provider = OpenAiCompatibleProvider::from_config(&OpenAiCompatibleConfig {
        preset: OpenAiCompatiblePreset::LMStudio,
        api_key_env: env_var.to_string(),
        api_key: None,
        base_url: "http://127.0.0.1:1234/v1".to_string(),
        extra_headers: BTreeMap::new(),
        transport: ProviderTransportConfig::default(),
        account_id: None,
        gateway_id: None,
        deployment_id: None,
        cf_ai_gateway: None,
        use_oauth: false,
    })
    .expect("LM Studio provider must build without an api key configured");
    assert_eq!(provider.base_url(), "http://127.0.0.1:1234/v1");
}

#[test]
fn remote_preset_still_requires_api_key() {
    // Behavior parity guard: removing the X-17 tolerance for any
    // remote preset would surface as the strict resolver failure here.
    // Pick Groq because its env-var name is unambiguously vendor-owned
    // and unlikely to collide with anything in the developer's shell.
    let env_var = "SQUEEZY_X17_DEFINITELY_NOT_SET_GROQ";
    unsafe {
        std::env::remove_var(env_var);
    }
    let error = OpenAiCompatibleProvider::from_config(&OpenAiCompatibleConfig {
        preset: OpenAiCompatiblePreset::Groq,
        api_key_env: env_var.to_string(),
        api_key: None,
        base_url: "https://api.groq.com/openai/v1".to_string(),
        extra_headers: BTreeMap::new(),
        transport: ProviderTransportConfig::default(),
        account_id: None,
        gateway_id: None,
        deployment_id: None,
        cf_ai_gateway: None,
        use_oauth: false,
    })
    .expect_err("remote preset without api key must still fail");
    assert!(
        matches!(error, SqueezyError::ProviderNotConfigured(_)),
        "missing key must map to ProviderNotConfigured, got: {error:?}"
    );
}

#[test]
fn lmstudio_empty_api_key_omits_authorization_header() {
    // H-46 regression: the X-17 `bearer_auth(key)` guard in
    // `stream_response` must keep the `Authorization` header off the
    // wire when the resolved key is empty. `local_preset_builds_…`
    // above proves construction succeeds; this test pins the
    // post-construction request shape so a future refactor that drops
    // the `if !key.is_empty()` gate would also drop a malformed
    // `Authorization: Bearer ` (empty token) on whichever local server
    // happens to be listening — and would panic inside reqwest before
    // that, since `bearer_auth("")` is a known bad input.
    //
    // The deliberately-unset env var keeps the test deterministic on a
    // developer's machine that may already have `LMSTUDIO_API_KEY`
    // exported.
    let env_var = "SQUEEZY_H46_DEFINITELY_NOT_SET_LMSTUDIO";
    unsafe {
        std::env::remove_var(env_var);
    }
    let provider = OpenAiCompatibleProvider::from_config(&OpenAiCompatibleConfig {
        preset: OpenAiCompatiblePreset::LMStudio,
        api_key_env: env_var.to_string(),
        api_key: None,
        base_url: "http://127.0.0.1:1234/v1".to_string(),
        extra_headers: BTreeMap::new(),
        transport: ProviderTransportConfig::default(),
        account_id: None,
        gateway_id: None,
        deployment_id: None,
        cf_ai_gateway: None,
        use_oauth: false,
    })
    .expect("LM Studio provider must build with no key configured");

    // The empty resolved key path: no Authorization header anywhere.
    let unauthed = provider.build_chat_request_for_test("");
    assert!(
        !unauthed
            .headers()
            .contains_key(reqwest::header::AUTHORIZATION),
        "empty resolved key must not stamp an Authorization header; \
         got headers: {:?}",
        unauthed.headers(),
    );

    // Positive control: a non-empty key still attaches `Bearer <key>`.
    // Without this assertion a buggy implementation that *always*
    // skipped `bearer_auth` would pass the negative case silently.
    let authed = provider.build_chat_request_for_test("sk-local-test");
    let value = authed
        .headers()
        .get(reqwest::header::AUTHORIZATION)
        .expect("non-empty key must produce an Authorization header")
        .to_str()
        .expect("Bearer header value is ASCII");
    assert_eq!(value, "Bearer sk-local-test");
}

/// Build a Cloudflare AI Gateway provider under a serialized env
/// snapshot, restore the prior env, and return the built provider
/// for sync inspection. Keeps the env lock from spanning any
/// `.await` (clippy `await_holding_lock`).
fn build_with_cf_upstream(
    upstream: Option<&str>,
    config: OpenAiCompatibleConfig,
) -> Result<OpenAiCompatibleProvider> {
    let _guard = env_lock();
    let prior = std::env::var("CF_UPSTREAM_KEY").ok();
    unsafe {
        match upstream {
            Some(value) => std::env::set_var("CF_UPSTREAM_KEY", value),
            None => std::env::remove_var("CF_UPSTREAM_KEY"),
        }
    }
    let provider = OpenAiCompatibleProvider::from_config(&config);
    unsafe {
        match prior.as_deref() {
            Some(value) => std::env::set_var("CF_UPSTREAM_KEY", value),
            None => std::env::remove_var("CF_UPSTREAM_KEY"),
        }
    }
    provider
}

#[test]
fn substitute_url_placeholders_leaves_provider_for_cloudflare_ai_gateway() {
    // C-12 follow-up: the new CF REST URL shape carries the
    // upstream provider in a path segment. The provider is
    // per-request (derived from the model id at stream time), so
    // `substitute_url_placeholders` must leave `{provider}` in the
    // string for the AI Gateway preset and let `stream_response`
    // resolve it later.
    let resolved = substitute_url_placeholders(
        "https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/v1/{provider}/v1",
        OpenAiCompatiblePreset::CloudflareAiGateway,
        Some("acct"),
        None,
    )
    .expect("CF AI Gateway with {provider} placeholder must build");
    assert_eq!(
        resolved, "https://api.cloudflare.com/client/v4/accounts/acct/ai/v1/{provider}/v1",
        "{{provider}} survives construction for AI Gateway",
    );
}

#[test]
fn substitute_url_placeholders_rejects_provider_for_non_ai_gateway_presets() {
    // Misconfiguration guard: a `{provider}` placeholder in a
    // non-AI-Gateway preset is almost certainly a copy/paste
    // mistake (or a typo in `[providers.custom.base_url]`). Surface
    // it as a config-time error rather than letting the literal
    // `{provider}` segment escape to the wire.
    let error = substitute_url_placeholders(
        "https://example.com/{provider}/chat/completions",
        OpenAiCompatiblePreset::Custom,
        None,
        None,
    )
    .expect_err("non-AI-Gateway preset must reject the placeholder");
    assert!(
        matches!(error, SqueezyError::ProviderNotConfigured(_)),
        "got: {error:?}"
    );
    let message = error.to_string();
    assert!(
        message.contains("{provider}"),
        "error must name the offending placeholder: {message}"
    );
    assert!(
        message.contains("cloudflare_ai_gateway"),
        "error must point users at the right preset: {message}"
    );
}

#[test]
fn resolve_provider_segment_maps_known_model_prefixes_to_upstream_path() {
    // The function pulls the upstream segment from the namespace
    // prefix on the model id. Anthropic, OpenAI, Google, xAI all
    // classify via COMPAT_TABLE; everything else falls back to the
    // bare prefix or the Workers-AI / compat default.
    let base = "https://api.cloudflare.com/client/v4/accounts/acct/ai/v1/{provider}/v1";
    assert_eq!(
        resolve_provider_segment(base, "anthropic/claude-opus-4-7"),
        "https://api.cloudflare.com/client/v4/accounts/acct/ai/v1/anthropic/v1",
        "anthropic/ prefix must route to /anthropic"
    );
    assert_eq!(
        resolve_provider_segment(base, "openai/gpt-5.5"),
        "https://api.cloudflare.com/client/v4/accounts/acct/ai/v1/openai/v1",
    );
    assert_eq!(
        resolve_provider_segment(base, "@cf/meta/llama-3.3-70b"),
        "https://api.cloudflare.com/client/v4/accounts/acct/ai/v1/workers-ai/v1",
        "@cf/ models route through Workers AI"
    );
    assert_eq!(
        resolve_provider_segment(base, "perplexity/sonar-large"),
        "https://api.cloudflare.com/client/v4/accounts/acct/ai/v1/perplexity/v1",
        "unknown prefix passes through as the segment"
    );
    assert_eq!(
        resolve_provider_segment(base, "unprefixed-model"),
        "https://api.cloudflare.com/client/v4/accounts/acct/ai/v1/compat/v1",
        "unprefixed models default to the compat upstream"
    );
    // No-op for URLs without the placeholder so non-CF routes pay
    // nothing.
    let plain = "https://api.openai.com/v1";
    assert_eq!(resolve_provider_segment(plain, "openai/gpt-5.5"), plain);
}

#[tokio::test]
async fn cloudflare_ai_gateway_swaps_upstream_key_into_bearer_slot() {
    // C-11: When the user has set `CF_UPSTREAM_KEY` the constructed
    // provider must carry that as its Bearer credential, and the
    // resolved `CLOUDFLARE_API_KEY` (which squeezy-core feeds via
    // `api_key` / `api_key_env`) lifts into `cf-aig-authorization`.
    // Otherwise the `/compat` endpoint sees the Cloudflare key in
    // both slots and the upstream (OpenAI / Anthropic / Groq) 401s.
    let gateway = build_with_cf_upstream(
        Some("upstream-openai-key"),
        OpenAiCompatibleConfig {
            preset: OpenAiCompatiblePreset::CloudflareAiGateway,
            api_key_env: "CLOUDFLARE_API_KEY".to_string(),
            api_key: Some("cf-token".to_string()),
            base_url: DEFAULT_CLOUDFLARE_AI_GATEWAY_BASE_URL.to_string(),
            extra_headers: BTreeMap::new(),
            transport: ProviderTransportConfig::default(),
            account_id: Some("acct".to_string()),
            gateway_id: Some("gw".to_string()),
            deployment_id: None,
            cf_ai_gateway: None,
            use_oauth: false,
        },
    )
    .expect("AI Gateway provider builds");
    let bearer = gateway
        .api_key_source()
        .current_key()
        .await
        .expect("bearer key resolves");
    assert_eq!(
        bearer, "upstream-openai-key",
        "Bearer slot must carry the UPSTREAM provider's key when CF_UPSTREAM_KEY is set"
    );
    let aig = gateway
        .extra_headers()
        .iter()
        .find_map(|(k, v)| {
            k.eq_ignore_ascii_case("cf-aig-authorization")
                .then(|| v.clone())
        })
        .expect("cf-aig-authorization must be populated from the resolved Cloudflare key");
    assert_eq!(
        aig, "Bearer cf-token",
        "cf-aig-authorization must carry the Cloudflare-token Bearer"
    );
}

#[tokio::test]
async fn cloudflare_ai_gateway_lifts_upstream_api_key_from_extra_headers_fallback() {
    // The `upstream-api-key` extra-header is the TOML escape hatch
    // for callers that prefer not to set `CF_UPSTREAM_KEY` in the
    // shell. It must be lifted into the Bearer slot and stripped
    // from the outgoing wire headers (it isn't a real HTTP header).
    let mut extras = BTreeMap::new();
    extras.insert(
        "upstream-api-key".to_string(),
        "anthropic-upstream".to_string(),
    );
    let gateway = build_with_cf_upstream(
        None,
        OpenAiCompatibleConfig {
            preset: OpenAiCompatiblePreset::CloudflareAiGateway,
            api_key_env: "CLOUDFLARE_API_KEY".to_string(),
            api_key: Some("cf-token".to_string()),
            base_url: DEFAULT_CLOUDFLARE_AI_GATEWAY_BASE_URL.to_string(),
            extra_headers: extras,
            transport: ProviderTransportConfig::default(),
            account_id: Some("acct".to_string()),
            gateway_id: Some("gw".to_string()),
            deployment_id: None,
            cf_ai_gateway: None,
            use_oauth: false,
        },
    )
    .expect("AI Gateway provider builds");
    let bearer = gateway
        .api_key_source()
        .current_key()
        .await
        .expect("bearer key resolves");
    assert_eq!(
        bearer, "anthropic-upstream",
        "Bearer slot must carry the upstream-api-key extra header when env is unset"
    );
    assert!(
        !gateway
            .extra_headers()
            .keys()
            .any(|k| k.eq_ignore_ascii_case("upstream-api-key")),
        "upstream-api-key escape hatch must be stripped from wire headers after lifting"
    );
    let aig = gateway
        .extra_headers()
        .iter()
        .find_map(|(k, v)| {
            k.eq_ignore_ascii_case("cf-aig-authorization")
                .then(|| v.clone())
        })
        .expect("cf-aig-authorization must still populate from the Cloudflare key");
    assert_eq!(aig, "Bearer cf-token");
}

#[tokio::test]
async fn cloudflare_ai_gateway_preserves_user_cf_aig_authorization_override() {
    // Manual override path: when the user has explicitly set
    // `cf-aig-authorization` via TOML, the swap must not overwrite
    // it. The Bearer slot still receives the upstream key.
    let mut extras = BTreeMap::new();
    extras.insert(
        "cf-aig-authorization".to_string(),
        "Bearer manual-override".to_string(),
    );
    let gateway = build_with_cf_upstream(
        Some("upstream"),
        OpenAiCompatibleConfig {
            preset: OpenAiCompatiblePreset::CloudflareAiGateway,
            api_key_env: "CLOUDFLARE_API_KEY".to_string(),
            api_key: Some("cf-token".to_string()),
            base_url: DEFAULT_CLOUDFLARE_AI_GATEWAY_BASE_URL.to_string(),
            extra_headers: extras,
            transport: ProviderTransportConfig::default(),
            account_id: Some("acct".to_string()),
            gateway_id: Some("gw".to_string()),
            deployment_id: None,
            cf_ai_gateway: None,
            use_oauth: false,
        },
    )
    .expect("AI Gateway provider builds");
    let aig = gateway
        .extra_headers()
        .iter()
        .find_map(|(k, v)| {
            k.eq_ignore_ascii_case("cf-aig-authorization")
                .then(|| v.clone())
        })
        .expect("cf-aig-authorization must be present");
    assert_eq!(
        aig, "Bearer manual-override",
        "user-supplied cf-aig-authorization must win over the auto-lift"
    );
    let bearer = gateway
        .api_key_source()
        .current_key()
        .await
        .expect("bearer");
    assert_eq!(bearer, "upstream");
}

#[tokio::test]
async fn cloudflare_ai_gateway_falls_back_to_resolved_key_when_no_upstream_configured() {
    // Backwards-compat path: when neither `CF_UPSTREAM_KEY` nor
    // `upstream-api-key` is configured, leave the Bearer slot
    // pointing at the resolved Cloudflare key. Workers-AI-only
    // gateways that were intentionally wired against the old
    // (broken) scheme keep working until the user migrates.
    let gateway = build_with_cf_upstream(
        None,
        OpenAiCompatibleConfig {
            preset: OpenAiCompatiblePreset::CloudflareAiGateway,
            api_key_env: "CLOUDFLARE_API_KEY".to_string(),
            api_key: Some("cf-token".to_string()),
            base_url: DEFAULT_CLOUDFLARE_AI_GATEWAY_BASE_URL.to_string(),
            extra_headers: BTreeMap::new(),
            transport: ProviderTransportConfig::default(),
            account_id: Some("acct".to_string()),
            gateway_id: Some("gw".to_string()),
            deployment_id: None,
            cf_ai_gateway: None,
            use_oauth: false,
        },
    )
    .expect("AI Gateway provider builds");
    let bearer = gateway
        .api_key_source()
        .current_key()
        .await
        .expect("bearer");
    assert_eq!(
        bearer, "cf-token",
        "fallback path must keep the Cloudflare key in the Bearer slot for legacy gateways"
    );
}

#[tokio::test]
async fn workers_ai_preset_does_not_apply_dual_auth_swap() {
    // The dual-auth swap is gated on the AI Gateway preset only.
    // The Workers AI preset routes directly to Cloudflare's edge
    // and uses the Cloudflare key as the standard Bearer — no
    // gateway-token slot exists for it. Make sure setting
    // `CF_UPSTREAM_KEY` doesn't accidentally hijack the
    // Workers AI key.
    let workers = build_with_cf_upstream(
        Some("unrelated"),
        OpenAiCompatibleConfig {
            preset: OpenAiCompatiblePreset::CloudflareWorkersAi,
            api_key_env: "CLOUDFLARE_API_KEY".to_string(),
            api_key: Some("cf-token".to_string()),
            base_url: DEFAULT_CLOUDFLARE_WORKERS_AI_BASE_URL.to_string(),
            extra_headers: BTreeMap::new(),
            transport: ProviderTransportConfig::default(),
            account_id: Some("acct".to_string()),
            gateway_id: None,
            deployment_id: None,
            cf_ai_gateway: None,
            use_oauth: false,
        },
    )
    .expect("Workers AI provider builds");
    let bearer = workers
        .api_key_source()
        .current_key()
        .await
        .expect("bearer");
    assert_eq!(
        bearer, "cf-token",
        "Workers AI preset must not pick up CF_UPSTREAM_KEY"
    );
    assert!(
        workers.extra_headers().is_empty(),
        "Workers AI preset must not auto-emit a cf-aig-authorization header"
    );
}
