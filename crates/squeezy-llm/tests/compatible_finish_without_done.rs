//! End-to-end regression test for the OpenAI-compatible chat-completions
//! streaming path: a provider-reported `length` finish whose byte stream is
//! closed without a `[DONE]` sentinel must surface exactly one `[squeezy]`
//! notice (the truncation notice) and must not also blame a cut connection.
//!
//! Driven against a loopback TCP server through the public
//! `OpenAiCompatibleProvider` so the post-loop fallback in `stream_response`
//! is actually exercised. The DeepSeek preset is one of the aggregators that
//! can close the stream without a `[DONE]` line.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use squeezy_core::{OpenAiCompatibleConfig, OpenAiCompatiblePreset, ProviderTransportConfig};
use squeezy_llm::{
    CacheSpec, LlmEvent, LlmInputItem, LlmProvider, LlmRequest, OpenAiCompatibleProvider,
    StopReason,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// Reasoning-only output followed by a `length` finish, with the byte stream
/// closed by EOF and *no* `data: [DONE]` line — the aggregator-without-DONE
/// shape (PortKey / OpenRouter / DeepSeek / Qwen).
const SSE_LENGTH_NO_DONE: &str = concat!(
    "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"delta\":{\"reasoning_content\":\"long thought...\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
);

async fn spawn_chat_server(body: &'static str) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        loop {
            let (mut stream, _peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => return,
            };
            let mut buf = [0u8; 8192];
            loop {
                match stream.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    Err(_) => return,
                }
            }
            let body_bytes = body.as_bytes();
            let headers = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: text/event-stream\r\n\
                 Cache-Control: no-cache\r\n\
                 Content-Length: {}\r\n\
                 \r\n",
                body_bytes.len()
            );
            if stream.write_all(headers.as_bytes()).await.is_err() {
                continue;
            }
            let _ = stream.write_all(body_bytes).await;
            let _ = stream.shutdown().await;
        }
    });
    addr
}

fn build_request(model: &str) -> LlmRequest {
    LlmRequest {
        model: Arc::from(model),
        instructions: Arc::from("be brief"),
        input: Arc::from(vec![LlmInputItem::UserText("say hello".to_string())]),
        max_output_tokens: Some(32),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        cache: CacheSpec::default(),
        tools: Arc::from(Vec::new()),
        store: false,
        output_schema: None,
        parallel_tool_calls: None,
        beta_headers: Arc::from(Vec::new()),
        tool_choice: None,
        ..LlmRequest::default()
    }
}

fn build_provider(addr: SocketAddr) -> OpenAiCompatibleProvider {
    OpenAiCompatibleProvider::from_config(&OpenAiCompatibleConfig {
        preset: OpenAiCompatiblePreset::DeepSeek,
        api_key_env: "DEEPSEEK_API_KEY".to_string(),
        api_key: Some("inline-key".to_string()),
        base_url: format!("http://{addr}/v1"),
        extra_headers: BTreeMap::new(),
        transport: ProviderTransportConfig {
            request_max_retries: 0,
            stream_max_retries: 0,
            stream_idle_timeout_ms: 5_000,
            ..ProviderTransportConfig::default()
        },
        account_id: None,
        gateway_id: None,
        deployment_id: None,
        cf_ai_gateway: None,
        use_oauth: false,
    })
    .expect("compatible provider builds")
}

async fn collect_events(provider: &OpenAiCompatibleProvider) -> Vec<LlmEvent> {
    let stream =
        provider.stream_response(build_request("deepseek-reasoner"), CancellationToken::new());
    tokio::time::timeout(Duration::from_secs(5), stream.collect::<Vec<_>>())
        .await
        .expect("stream must complete within timeout")
        .into_iter()
        .map(|res| res.expect("stream must not surface an error"))
        .collect()
}

fn squeezy_notices(events: &[LlmEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match event {
            LlmEvent::TextDelta(text) if text.contains("[squeezy]") => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn length_finish_without_done_emits_single_truncation_notice() {
    let addr = spawn_chat_server(SSE_LENGTH_NO_DONE).await;
    let provider = build_provider(addr);
    let events = collect_events(&provider).await;

    let notices = squeezy_notices(&events);
    assert_eq!(
        notices.len(),
        1,
        "exactly one [squeezy] notice expected, got: {notices:?}"
    );
    assert!(
        notices[0].contains("max_output_tokens"),
        "the surviving notice must be the truncation notice: {:?}",
        notices[0]
    );
    assert!(
        !notices
            .iter()
            .any(|notice| notice.contains("cut the connection")),
        "a clean length finish must not blame a cut connection: {notices:?}"
    );

    let Some(LlmEvent::Completed { stop_reason, .. }) = events
        .iter()
        .find(|event| matches!(event, LlmEvent::Completed { .. }))
    else {
        panic!("Completed must be emitted: {events:?}");
    };
    assert_eq!(
        stop_reason.as_ref(),
        Some(&StopReason::MaxTokens),
        "length finish normalizes to MaxTokens"
    );
}

/// A genuinely cut stream — content with no finish_reason and no `[DONE]` —
/// must still warn about a possible dropped connection. Guards against the
/// fix over-suppressing the legitimate fallback.
#[tokio::test]
async fn truncated_stream_without_finish_reason_warns_about_cut_connection() {
    const SSE_CUT: &str = "data: {\"id\":\"chatcmpl-2\",\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking...\"}}]}\n\n";
    let addr = spawn_chat_server(SSE_CUT).await;
    let provider = build_provider(addr);
    let events = collect_events(&provider).await;

    let notices = squeezy_notices(&events);
    assert_eq!(
        notices.len(),
        1,
        "exactly one [squeezy] notice expected, got: {notices:?}"
    );
    assert!(
        notices[0].contains("cut the connection"),
        "a stream with no finish_reason must surface the cut-connection notice: {:?}",
        notices[0]
    );
}
