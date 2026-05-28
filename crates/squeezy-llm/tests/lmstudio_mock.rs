//! End-to-end test for `LMStudioProvider` against a loopback TCP server
//! that emulates LM Studio's OpenAI-compatible `/v1/chat/completions`
//! streaming endpoint.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use squeezy_core::ProviderTransportConfig;
use squeezy_llm::{
    CacheSpec, LMStudioConfig, LMStudioProvider, LlmEvent, LlmInputItem, LlmProvider, LlmRequest,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

const SSE_BODY: &str = concat!(
    "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"hello\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\n",
    "data: {\"choices\":[{\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":2}}\n\n",
    "data: [DONE]\n\n",
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
        input: Arc::from(vec![LlmInputItem::UserText("say hello world".to_string())]),
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
        beta_headers: std::sync::Arc::from(Vec::new()),
        tool_choice: None,
    }
}

#[tokio::test]
async fn lmstudio_streaming_completion_against_mock_server() {
    let addr = spawn_chat_server(SSE_BODY).await;
    let provider = LMStudioProvider::from_config(&LMStudioConfig {
        base_url: format!("http://{addr}/v1"),
        api_key: None,
        transport: ProviderTransportConfig {
            request_max_retries: 0,
            stream_max_retries: 0,
            stream_idle_timeout_ms: 5_000,
            ..ProviderTransportConfig::default()
        },
    });

    let stream = provider.stream_response(
        build_request("openai/gpt-oss-20b"),
        CancellationToken::new(),
    );
    let events: Vec<LlmEvent> =
        tokio::time::timeout(Duration::from_secs(5), stream.collect::<Vec<_>>())
            .await
            .expect("stream must complete within timeout")
            .into_iter()
            .map(|res| res.expect("stream must not surface an error"))
            .collect();

    let text: String = events
        .iter()
        .filter_map(|event| match event {
            LlmEvent::TextDelta(delta) => Some(delta.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "hello world");

    let completed = events
        .iter()
        .filter(|event| matches!(event, LlmEvent::Completed { .. }))
        .count();
    assert_eq!(completed, 1, "Completed must be emitted exactly once");

    let started = events
        .iter()
        .filter(|event| matches!(event, LlmEvent::Started))
        .count();
    assert_eq!(started, 1, "Started must be emitted exactly once");

    let Some(LlmEvent::Completed {
        cost, response_id, ..
    }) = events
        .iter()
        .find(|event| matches!(event, LlmEvent::Completed { .. }))
    else {
        unreachable!("checked above");
    };
    assert_eq!(response_id.as_deref(), Some("chatcmpl-1"));
    assert_eq!(cost.input_tokens, Some(7));
    assert_eq!(cost.output_tokens, Some(2));
}
