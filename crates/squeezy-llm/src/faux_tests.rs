use std::sync::Arc;

use futures_util::StreamExt;
use squeezy_core::{FauxConfig, ProviderConfig, ProviderTransportConfig, SqueezyError};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::{
    CacheSpec, LlmEvent, LlmInputItem, LlmProvider, LlmRequest, StopReason, provider_from_config,
};

fn make_request(model: &str, input: &str) -> LlmRequest {
    LlmRequest {
        model: model.to_string().into(),
        instructions: "you are a test fixture".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText(input.to_string())]),
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
        beta_headers: Arc::from(Vec::new()),
    }
}

async fn drain(provider: &FauxProvider, prompt: &str) -> Vec<Result<LlmEvent>> {
    let mut events = Vec::new();
    let mut stream =
        provider.stream_response(make_request("faux-1", prompt), CancellationToken::new());
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    events
}

#[tokio::test]
async fn basic_scripted_exchange_replays_text_response() {
    let provider = FauxProvider::with_steps(
        "faux-anthropic",
        [FauxTurn {
            text: Some("hello, scripted world".to_string()),
            response_id: Some("resp_1".to_string()),
            input_tokens: Some(7),
            output_tokens: Some(5),
            ..Default::default()
        }
        .into_step()],
    );

    assert_eq!(provider.name(), "faux-anthropic");
    assert_eq!(provider.pending(), 1);

    let events = drain(&provider, "hi").await;
    assert_eq!(events.len(), 3, "expected Started + TextDelta + Completed");

    assert!(matches!(events[0], Ok(LlmEvent::Started)));

    match &events[1] {
        Ok(LlmEvent::TextDelta(text)) => assert_eq!(text, "hello, scripted world"),
        other => panic!("expected TextDelta, got {other:?}"),
    }

    match &events[2] {
        Ok(LlmEvent::Completed {
            response_id,
            cost,
            stop_reason,
            reasoning_only_stop,
        }) => {
            assert_eq!(response_id.as_deref(), Some("resp_1"));
            assert_eq!(cost.input_tokens, Some(7));
            assert_eq!(cost.output_tokens, Some(5));
            assert!(stop_reason.is_none());
            assert!(!reasoning_only_stop);
        }
        other => panic!("expected Completed, got {other:?}"),
    }

    assert_eq!(provider.pending(), 0, "queue consumed");
}

#[tokio::test]
async fn error_injection_surfaces_provider_request_error_and_exhaustion_falls_through() {
    let provider = FauxProvider::with_steps(
        "faux-openai",
        [FauxStep::Error("upstream rate limit".to_string())],
    );

    let events = drain(&provider, "trigger error").await;
    assert_eq!(events.len(), 1, "error injection yields a single Err");
    match &events[0] {
        Err(SqueezyError::ProviderRequest(message)) => {
            assert_eq!(message, "upstream rate limit");
        }
        other => panic!("expected ProviderRequest error, got {other:?}"),
    }

    let follow_up = drain(&provider, "after the script is empty").await;
    assert_eq!(follow_up.len(), 1, "exhaustion yields a single Err");
    match &follow_up[0] {
        Err(SqueezyError::ProviderRequest(message)) => {
            assert!(
                message.contains("no scripted response remaining"),
                "exhaustion error must mention missing response: {message}"
            );
        }
        other => panic!("expected exhaustion ProviderRequest error, got {other:?}"),
    }
}

#[tokio::test]
async fn multi_turn_exchange_replays_steps_in_fifo_order_via_config_wiring() {
    let dir = tempdir();
    let script_path = dir.path().join("script.toml");
    std::fs::write(
        &script_path,
        r#"
[[turn]]
text = "first answer"
response_id = "resp_1"

[[turn]]
thinking = "let me think"
text = "second answer"
stop_reason = { kind = "end_turn" }

[[turn]]
text = "calling a tool"
tool_calls = [
    { call_id = "call_alpha", name = "read_file", arguments = { path = "src/lib.rs" } },
]
"#,
    )
    .expect("write script");

    let config = ProviderConfig::Faux(FauxConfig {
        script: Some(script_path.to_string_lossy().into_owned()),
        name: Some("faux-script".to_string()),
        transport: ProviderTransportConfig::default(),
    });

    let provider = provider_from_config(&config).expect("build provider");
    assert_eq!(provider.name(), "faux-script");

    let mut request_count = 0;
    let mut collected_text = Vec::new();
    let mut saw_reasoning = false;
    let mut saw_tool_call = false;
    let mut saw_stop_reason = false;

    for prompt in ["first", "second", "third"] {
        request_count += 1;
        let mut stream =
            provider.stream_response(make_request("faux-1", prompt), CancellationToken::new());
        let mut turn_text = String::new();
        while let Some(event) = stream.next().await {
            match event.expect("scripted events succeed") {
                LlmEvent::Started => {}
                LlmEvent::TextDelta(text) => turn_text.push_str(&text),
                LlmEvent::ReasoningDelta { text, kind } => {
                    saw_reasoning = true;
                    assert_eq!(text, "let me think");
                    assert!(matches!(kind, squeezy_core::ReasoningKind::Text));
                }
                LlmEvent::ToolCall(call) => {
                    saw_tool_call = true;
                    assert_eq!(call.call_id, "call_alpha");
                    assert_eq!(call.name, "read_file");
                    assert_eq!(call.arguments, serde_json::json!({ "path": "src/lib.rs" }));
                }
                LlmEvent::Completed {
                    response_id,
                    stop_reason,
                    ..
                } => {
                    if request_count == 1 {
                        assert_eq!(response_id.as_deref(), Some("resp_1"));
                    }
                    if matches!(stop_reason, Some(StopReason::EndTurn)) {
                        saw_stop_reason = true;
                    }
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
        collected_text.push(turn_text);
    }

    assert_eq!(
        collected_text,
        vec![
            "first answer".to_string(),
            "second answer".to_string(),
            "calling a tool".to_string()
        ]
    );
    assert!(saw_reasoning, "thinking should emit a ReasoningDelta");
    assert!(saw_tool_call, "tool_calls should emit a ToolCall event");
    assert!(saw_stop_reason, "stop_reason should ride on Completed");
}

/// Small inline temp-directory helper. Avoids pulling in `tempfile`
/// just for this test module — the production graph already has enough
/// dependencies.
struct TempDir {
    path: std::path::PathBuf,
}

impl TempDir {
    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn tempdir() -> TempDir {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("squeezy-faux-{}-{}", std::process::id(), id));
    std::fs::create_dir_all(&path).expect("create tempdir");
    TempDir { path }
}
