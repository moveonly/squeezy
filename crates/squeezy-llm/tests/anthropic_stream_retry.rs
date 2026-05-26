//! Acceptance test for `stream_max_retries` mid-stream reconnect.
//!
//! Spins a loopback TCP server that emulates the Anthropic SSE endpoint:
//! it accepts POST `/messages`, writes a chunked HTTP response with a
//! partial SSE prefix, then drops the connection on the first N attempts
//! and finally streams a complete response on attempt N+1. The test
//! drives a real `AnthropicProvider` against the server and asserts that
//! the harness reconnects, dedupes the replayed prefix, and surfaces a
//! single completed stream.
//!
//! Pre-existing transport plumbing is exercised end-to-end: this is not a
//! parser-level mock.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use squeezy_core::{AnthropicConfig, ProviderTransportConfig};
use squeezy_llm::{AnthropicProvider, LlmEvent, LlmProvider, LlmRequest};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

const FULL_SSE_BODY: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_test\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello \"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"world\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// Returns the byte offset just after the first text_delta SSE event so a
/// drop at that boundary leaves the parser with a clean partial-prefix
/// state to deduplicate against on reconnect.
fn first_text_delta_boundary() -> usize {
    let needle = "\"text\":\"hello \"}}\n\n";
    FULL_SSE_BODY
        .find(needle)
        .map(|index| index + needle.len())
        .expect("test fixture must contain hello-prefix boundary")
}

async fn spawn_mock_server(fail_attempts: usize) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let attempts = Arc::new(AtomicUsize::new(0));
    tokio::spawn(async move {
        loop {
            let (mut stream, _peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => return,
            };
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            // Drain the request headers + body so the client's POST completes
            // before we start streaming. We don't need to parse it.
            let mut buf = [0u8; 4096];
            // Read until we've seen the end-of-headers marker; ignore the body
            // since the test request is tiny and tokio's send buffers will
            // flush in one read.
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
            let headers = "HTTP/1.1 200 OK\r\n\
                 Content-Type: text/event-stream\r\n\
                 Cache-Control: no-cache\r\n\
                 Transfer-Encoding: chunked\r\n\
                 \r\n";
            if stream.write_all(headers.as_bytes()).await.is_err() {
                continue;
            }
            let body = FULL_SSE_BODY.as_bytes();
            let drop_at = first_text_delta_boundary();
            let chunk = if attempt < fail_attempts {
                &body[..drop_at]
            } else {
                body
            };
            // Write a single chunked-encoding frame with the SSE prefix,
            // then either close mid-stream (drop the TCP connection) or
            // finalise with the zero-length terminator chunk.
            let frame = format!("{:x}\r\n", chunk.len());
            if stream.write_all(frame.as_bytes()).await.is_err() {
                continue;
            }
            if stream.write_all(chunk).await.is_err() {
                continue;
            }
            if stream.write_all(b"\r\n").await.is_err() {
                continue;
            }
            if attempt < fail_attempts {
                // Abrupt close mid-stream so the client's bytes_stream sees
                // an unexpected EOF, surfacing as ProviderStream.
                let _ = stream.shutdown().await;
                continue;
            }
            let _ = stream.write_all(b"0\r\n\r\n").await;
            let _ = stream.shutdown().await;
        }
    });
    addr
}

fn build_request(model: &str) -> LlmRequest {
    LlmRequest {
        model: Arc::from(model),
        instructions: Arc::from("you are testing reconnect"),
        input: Arc::from(vec![squeezy_llm::LlmInputItem::UserText(
            "say hello world".to_string(),
        )]),
        max_output_tokens: Some(32),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        tools: Arc::from(Vec::new()),
        store: false,
    }
}

fn provider_for(addr: SocketAddr, stream_max_retries: u8) -> AnthropicProvider {
    // Setting the env var bypasses keychain lookup so AnthropicProvider::from_config
    // succeeds in the sandboxed test environment.
    let env_var = format!("SQUEEZY_TEST_ANTHROPIC_KEY_{}", addr.port(),);
    // SAFETY: tests are single-threaded per binary process; setting a unique
    // env var per port is race-free across tests in this file.
    unsafe {
        std::env::set_var(&env_var, "test-key");
    }
    let config = AnthropicConfig {
        api_key_env: env_var,
        api_key: None,
        base_url: format!("http://{addr}"),
        transport: ProviderTransportConfig {
            request_max_retries: 0,
            stream_max_retries,
            stream_idle_timeout_ms: 5_000,
        },
    };
    AnthropicProvider::from_config(&config).expect("provider")
}

async fn collect_stream(provider: &AnthropicProvider, model: &str) -> Vec<LlmEvent> {
    let stream = provider.stream_response(build_request(model), CancellationToken::new());
    let mut events = Vec::new();
    let mut stream = stream;
    while let Some(event) = stream.next().await {
        events.push(event.expect("stream must not surface an error"));
    }
    events
}

async fn collect_stream_or_error(
    provider: &AnthropicProvider,
    model: &str,
) -> Result<Vec<LlmEvent>, squeezy_core::SqueezyError> {
    let stream = provider.stream_response(build_request(model), CancellationToken::new());
    let mut events = Vec::new();
    let mut stream = stream;
    while let Some(event) = stream.next().await {
        match event {
            Ok(event) => events.push(event),
            Err(err) => return Err(err),
        }
    }
    Ok(events)
}

#[tokio::test]
async fn anthropic_sse_drop_reconnects_within_stream_max_retries() {
    // Server drops the first two streams mid-prefix; the third completes.
    // With stream_max_retries=3 the harness has enough budget (initial + 3
    // reconnects) to survive both failures.
    let addr = spawn_mock_server(2).await;
    let provider = provider_for(addr, 3);

    let events = tokio::time::timeout(
        Duration::from_secs(10),
        collect_stream(&provider, "claude-test"),
    )
    .await
    .expect("stream must complete within timeout");

    let text: String = events
        .iter()
        .filter_map(|event| match event {
            LlmEvent::TextDelta(delta) => Some(delta.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        text, "hello world",
        "text must contain the full response with no duplicate prefix"
    );

    let started = events
        .iter()
        .filter(|event| matches!(event, LlmEvent::Started))
        .count();
    let completed = events
        .iter()
        .filter(|event| matches!(event, LlmEvent::Completed { .. }))
        .count();
    assert_eq!(started, 1, "Started must be emitted exactly once");
    assert_eq!(completed, 1, "Completed must be emitted exactly once");
}

#[tokio::test]
async fn anthropic_sse_drop_fails_when_stream_max_retries_is_exhausted() {
    // Server drops every attempt; with stream_max_retries=1 the harness
    // gives up after the initial attempt + 1 reconnect and surfaces the
    // error.
    let addr = spawn_mock_server(usize::MAX).await;
    let provider = provider_for(addr, 1);

    let err = tokio::time::timeout(
        Duration::from_secs(10),
        collect_stream_or_error(&provider, "claude-test"),
    )
    .await
    .expect("stream must surface an error within timeout")
    .expect_err("must fail once stream retries are exhausted");

    assert!(matches!(err, squeezy_core::SqueezyError::ProviderStream(_)));
}
