use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use squeezy_core::{OpenAiCompatibleConfig, OpenAiCompatiblePreset, ProviderTransportConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use super::{XaiRoute, classify_route, is_responses_capable};
use crate::{LlmProvider, LlmRequest, XaiProvider};

#[test]
fn xai_uses_responses_api_for_grok_3_and_newer() {
    // Grok 3 onward exposes the OpenAI-Responses-compatible endpoint; the
    // routing predicate must select Responses for every supported variant
    // so callers do not silently degrade to Chat Completions.
    let responses_models = [
        "grok-3",
        "grok-3-mini",
        "grok-3-fast",
        "grok-4",
        "grok-4-fast-reasoning",
        "grok-4-fast-non-reasoning",
        "grok-code-fast-1",
        "GROK-4",
    ];
    for model in responses_models {
        assert!(
            is_responses_capable(model),
            "{model} must route via Responses API"
        );
    }
}

#[test]
fn xai_uses_chat_completions_for_grok_2_and_earlier() {
    // grok-2 / grok-beta / grok-1 predate the Responses launch and only
    // answer Chat Completions. Mis-routing them onto Responses would 404
    // every turn, so the predicate must return false.
    let chat_models = [
        "grok-2",
        "grok-2-mini",
        "grok-2-vision",
        "grok-beta",
        "grok-1",
    ];
    for model in chat_models {
        assert!(
            !is_responses_capable(model),
            "{model} must route via Chat Completions"
        );
    }
}

#[test]
fn xai_routes_unknown_grok_generations_to_responses() {
    // xAI treats Responses as the canonical surface as of the May 2026
    // catalog refresh: any unrecognised `grok-…` SKU must default to
    // Responses so future generations work without a code change.
    assert!(is_responses_capable("grok-5"));
    assert!(is_responses_capable("grok-5-mini"));
    assert!(is_responses_capable("grok-omega-2027"));
}

#[test]
fn xai_routes_non_grok_ids_to_chat_completions() {
    // Defensive fallback: non-grok ids and empty strings stay on Chat
    // Completions because that endpoint accepts arbitrary model strings
    // a user might have routed through a base_url override.
    assert!(!is_responses_capable(""));
    assert!(!is_responses_capable("not-a-grok"));
    // `grok-` with no generation suffix is ambiguous; route to Responses
    // because it lands under the "unknown grok" branch.
    assert!(is_responses_capable("grok-"));
}

#[test]
fn xai_strips_aggregator_namespace_prefix() {
    // A `vendor/model` prefix appears when a model id is forwarded from an
    // aggregator (OpenRouter, Vercel AI Gateway) but the caller pointed the
    // xAI provider at a base_url that still serves the vendor route. Honour
    // the namespace so routing tracks the underlying generation.
    assert!(is_responses_capable("xai/grok-4"));
    assert!(!is_responses_capable("xai/grok-2"));
}

#[test]
fn xai_classify_route_covers_new_grok_families_c09() {
    // C-09: explicit allow-list of Grok families xAI ships on Responses
    // as of the May 2026 catalog refresh. The parser must classify each
    // family correctly even for dotted minor versions and date-stamped
    // SKUs that the legacy digit-range matcher could not express.
    let responses_models = [
        "grok-4.3",
        "grok-4.3-0309",
        "grok-4.20-multi-agent-0309",
        "grok-4.20-0309-reasoning",
        "grok-4.20-0309-non-reasoning",
        "grok-build-0.1",
        "grok-build-1.0-256k",
        "grok-code-fast-1",
    ];
    for model in responses_models {
        assert_eq!(
            classify_route(model),
            XaiRoute::Responses,
            "{model} must classify as Responses"
        );
    }
}

/// SSE body the OpenAI Responses parser tolerates: a single
/// `response.completed` event with usage carries the stream past the
/// terminator without any deltas. Mirrors the minimum surface the
/// dispatcher needs to validate which route received the request.
const RESPONSES_SSE_BODY: &str = concat!(
    "event: response.completed\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_xai_dispatch\",\"model\":\"grok-4.3\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
    "data: [DONE]\n\n",
);

/// Minimum chat-completions SSE that ends cleanly: one `stop` finish
/// reason with a `usage` block, no content deltas required.
const CHAT_SSE_BODY: &str = concat!(
    "data: {\"id\":\"chatcmpl_xai_dispatch\",\"choices\":[{\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\n",
    "data: [DONE]\n\n",
);

/// Records the first request path the loopback server sees so the
/// dispatcher tests can assert which endpoint each request hit.
#[derive(Default)]
struct DispatcherRecorder {
    paths: tokio::sync::Mutex<Vec<String>>,
    requests: AtomicUsize,
}

impl DispatcherRecorder {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    async fn record(&self, path: String) {
        self.requests.fetch_add(1, Ordering::SeqCst);
        self.paths.lock().await.push(path);
    }

    async fn first_path(&self) -> Option<String> {
        self.paths.lock().await.first().cloned()
    }
}

/// Spin a loopback TCP server that replies to every POST with an SSE
/// body chosen by inspecting the request path. The recorder captures
/// the path so the test can assert which endpoint the dispatcher chose.
async fn spawn_xai_dispatcher_server(recorder: Arc<DispatcherRecorder>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        loop {
            let (mut stream, _peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => return,
            };
            let recorder = recorder.clone();
            tokio::spawn(async move {
                let mut buf = Vec::with_capacity(8192);
                let mut tmp = [0u8; 4096];
                loop {
                    match stream.read(&mut tmp).await {
                        Ok(0) => break,
                        Ok(n) => {
                            buf.extend_from_slice(&tmp[..n]);
                            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                        Err(_) => return,
                    }
                }
                let head = String::from_utf8_lossy(&buf);
                let request_line = head.lines().next().unwrap_or("").to_string();
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .to_string();
                recorder.record(path.clone()).await;
                let body = if path.ends_with("/responses") {
                    RESPONSES_SSE_BODY
                } else {
                    CHAT_SSE_BODY
                };
                let bytes = body.as_bytes();
                let headers = format!(
                    "HTTP/1.1 200 OK\r\n\
                     Content-Type: text/event-stream\r\n\
                     Cache-Control: no-cache\r\n\
                     Content-Length: {}\r\n\
                     \r\n",
                    bytes.len()
                );
                if stream.write_all(headers.as_bytes()).await.is_err() {
                    return;
                }
                let _ = stream.write_all(bytes).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    addr
}

fn dispatcher_xai_config(addr: SocketAddr) -> OpenAiCompatibleConfig {
    OpenAiCompatibleConfig {
        preset: OpenAiCompatiblePreset::XAi,
        api_key_env: "XAI_API_KEY".to_string(),
        api_key: Some("test-key".to_string()),
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
    }
}

fn dispatcher_request(model: &str) -> LlmRequest {
    // `LlmRequest::user_text` keeps the test pinned to the public
    // constructor — when new optional fields land via the type-system
    // expansion in Phase 1, they pick up sensible defaults from the
    // canonical builder rather than needing to be enumerated here.
    LlmRequest::user_text(
        model.to_string(),
        "dispatcher test".to_string(),
        "ping".to_string(),
        Some(8),
    )
}

#[tokio::test]
async fn xai_dispatcher_routes_grok_4_3_to_responses_h21() {
    // H-21: prove the dispatcher hands a Responses-capable Grok 4.3
    // request to `/v1/responses`. The mock server returns a minimum
    // SSE body for whichever path it sees, and the recorder asserts
    // the dispatcher's path choice. Regressions in `classify_route`
    // that drop grok-4.3 from the Responses branch would silently
    // pass before this test landed.
    let recorder = DispatcherRecorder::new();
    let addr = spawn_xai_dispatcher_server(recorder.clone()).await;
    let provider = XaiProvider::from_config(&dispatcher_xai_config(addr))
        .expect("XaiProvider builds against mock server");

    let stream = provider.stream_response(dispatcher_request("grok-4.3"), CancellationToken::new());
    let _events = tokio::time::timeout(Duration::from_secs(5), stream.collect::<Vec<_>>())
        .await
        .expect("dispatcher stream must complete within timeout");

    assert_eq!(recorder.requests.load(Ordering::SeqCst), 1);
    let path = recorder.first_path().await.expect("recorder captured path");
    assert_eq!(path, "/v1/responses", "grok-4.3 must reach /v1/responses");
}

#[tokio::test]
async fn xai_dispatcher_routes_grok_2_to_chat_completions_h21() {
    // H-21 companion: legacy grok-2 must keep landing on
    // `/v1/chat/completions`. Without an explicit assertion, a refactor
    // that flipped `classify_route`'s fallback to Responses would 404
    // every grok-2 request and only surface in production.
    let recorder = DispatcherRecorder::new();
    let addr = spawn_xai_dispatcher_server(recorder.clone()).await;
    let provider = XaiProvider::from_config(&dispatcher_xai_config(addr))
        .expect("XaiProvider builds against mock server");

    let stream = provider.stream_response(dispatcher_request("grok-2"), CancellationToken::new());
    let _events = tokio::time::timeout(Duration::from_secs(5), stream.collect::<Vec<_>>())
        .await
        .expect("dispatcher stream must complete within timeout");

    assert_eq!(recorder.requests.load(Ordering::SeqCst), 1);
    let path = recorder.first_path().await.expect("recorder captured path");
    assert_eq!(
        path, "/v1/chat/completions",
        "grok-2 must reach /v1/chat/completions"
    );
}

#[test]
fn xai_classify_route_rejects_imagine_family_c09() {
    // C-09: `grok-imagine-*` is image-only and lives on
    // `/v1/images/generations`. Neither sub-provider knows that
    // endpoint, so the dispatcher must surface a structured rejection
    // rather than route to chat (where the parser would 404).
    let imagine_models = [
        "grok-imagine",
        "grok-imagine-image",
        "grok-imagine-1",
        "GROK-IMAGINE-IMAGE",
    ];
    for model in imagine_models {
        assert_eq!(
            classify_route(model),
            XaiRoute::ImageNotRouted,
            "{model} must classify as ImageNotRouted"
        );
        assert!(
            !is_responses_capable(model),
            "{model} must not be classified as Responses-capable"
        );
    }
}
