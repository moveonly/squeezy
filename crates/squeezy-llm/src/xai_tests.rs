use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use squeezy_core::{
    OpenAiCompatibleConfig, OpenAiCompatiblePreset, ProviderTransportConfig, SqueezyError,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use super::{XaiRoute, classify_route, is_responses_capable};
use crate::{LlmEvent, LlmProvider, LlmRequest, XaiProvider};

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
fn xai_strips_multi_segment_aggregator_prefix_low() {
    // Low/Nit: Vercel AI Gateway, OpenRouter, and PortKey integrations
    // sometimes layer two or three namespace segments before the
    // underlying model id. Walk to the trailing segment with
    // `rsplit_once('/')` so each layer chews cleanly and the route
    // classifier still sees the Grok slug.
    assert_eq!(
        classify_route("vercel/xai/grok-4"),
        XaiRoute::Responses,
        "vercel/xai/grok-4 must resolve to grok-4 → Responses"
    );
    assert_eq!(
        classify_route("@openrouter/xai/grok-4.3"),
        XaiRoute::Responses,
        "openrouter prefix must not block grok-4.3 → Responses"
    );
    assert_eq!(
        classify_route("portkey/integration/xai/grok-build-0.1"),
        XaiRoute::Responses,
        "three-layer portkey prefix must resolve to grok-build → Responses"
    );
    assert_eq!(
        classify_route("portkey/integration/xai/grok-2"),
        XaiRoute::Chat,
        "three-layer portkey prefix must still route grok-2 to Chat"
    );
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
///
/// NOTE: the Responses API does NOT emit a `[DONE]` sentinel (unlike
/// Chat Completions); the stream ends after `response.completed`.
/// Appending one makes `parse_openai_event` fail with "invalid SSE
/// JSON", which the `.expect("…must not error")` mapping in the H-21
/// routing tests would surface as a panic.
const RESPONSES_SSE_BODY: &str = concat!(
    "event: response.completed\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_xai_dispatch\",\"model\":\"grok-4.3\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
);

/// Minimum chat-completions SSE that ends cleanly: one `stop` finish
/// reason with a `usage` block, no content deltas required.
const CHAT_SSE_BODY: &str = concat!(
    "data: {\"id\":\"chatcmpl_xai_dispatch\",\"choices\":[{\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\n",
    "data: [DONE]\n\n",
);

/// Captured request line plus headers from a single dispatcher request.
#[derive(Debug, Default, Clone)]
struct CapturedRequest {
    path: String,
    headers: BTreeMap<String, String>,
}

/// Records every request the loopback server sees so the dispatcher and
/// SSE-replay tests can assert which endpoint the dispatcher chose, and
/// which headers (if any) the sub-provider forwarded.
#[derive(Default)]
struct DispatcherRecorder {
    captured: tokio::sync::Mutex<Vec<CapturedRequest>>,
    requests: AtomicUsize,
}

impl DispatcherRecorder {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    async fn record(&self, captured: CapturedRequest) {
        self.requests.fetch_add(1, Ordering::SeqCst);
        self.captured.lock().await.push(captured);
    }

    async fn first(&self) -> Option<CapturedRequest> {
        self.captured.lock().await.first().cloned()
    }

    async fn first_path(&self) -> Option<String> {
        self.first().await.map(|c| c.path)
    }
}

/// Selects which SSE body the loopback server returns for each route.
#[derive(Clone)]
struct RouteBodies {
    responses: &'static str,
    chat: &'static str,
}

impl RouteBodies {
    const fn defaults() -> Self {
        Self {
            responses: RESPONSES_SSE_BODY,
            chat: CHAT_SSE_BODY,
        }
    }
}

/// Spin a loopback TCP server that replies to every POST with an SSE
/// body chosen by inspecting the request path. The recorder captures
/// the path AND headers so tests can assert which endpoint the
/// dispatcher chose and whether per-route headers reached the wire.
async fn spawn_xai_dispatcher_server(
    recorder: Arc<DispatcherRecorder>,
    bodies: RouteBodies,
) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        loop {
            let (mut stream, _peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => return,
            };
            let recorder = recorder.clone();
            let bodies = bodies.clone();
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
                let mut lines = head.lines();
                let request_line = lines.next().unwrap_or("").to_string();
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("")
                    .to_string();
                let mut headers = BTreeMap::new();
                for line in lines {
                    if line.is_empty() {
                        break;
                    }
                    if let Some((key, value)) = line.split_once(':') {
                        headers.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
                    }
                }
                recorder
                    .record(CapturedRequest {
                        path: path.clone(),
                        headers,
                    })
                    .await;
                let body = if path.ends_with("/responses") {
                    bodies.responses
                } else {
                    bodies.chat
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
        deployment_id: None,
        cf_ai_gateway: None,
        use_oauth: false,
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
    let addr = spawn_xai_dispatcher_server(recorder.clone(), RouteBodies::defaults()).await;
    let provider = XaiProvider::from_config(&dispatcher_xai_config(addr))
        .expect("XaiProvider builds against mock server");

    let stream = provider.stream_response(dispatcher_request("grok-4.3"), CancellationToken::new());
    let events: Vec<LlmEvent> =
        tokio::time::timeout(Duration::from_secs(5), stream.collect::<Vec<_>>())
            .await
            .expect("dispatcher stream must complete within timeout")
            .into_iter()
            .map(|res| res.expect("Responses route stream must not error"))
            .collect();

    // A parse error on the routed stream must not slip past: surface it
    // via `.expect` above and prove the parser carried the SSE through
    // to a single terminal `Completed`.
    assert!(
        events
            .iter()
            .any(|event| matches!(event, LlmEvent::Completed { .. })),
        "Responses route must yield an LlmEvent::Completed"
    );
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
    let addr = spawn_xai_dispatcher_server(recorder.clone(), RouteBodies::defaults()).await;
    let provider = XaiProvider::from_config(&dispatcher_xai_config(addr))
        .expect("XaiProvider builds against mock server");

    let stream = provider.stream_response(dispatcher_request("grok-2"), CancellationToken::new());
    let events: Vec<LlmEvent> =
        tokio::time::timeout(Duration::from_secs(5), stream.collect::<Vec<_>>())
            .await
            .expect("dispatcher stream must complete within timeout")
            .into_iter()
            .map(|res| res.expect("Chat route stream must not error"))
            .collect();

    // A parse error on the routed stream must not slip past: surface it
    // via `.expect` above and prove the parser carried the SSE through
    // to a single terminal `Completed`.
    assert!(
        events
            .iter()
            .any(|event| matches!(event, LlmEvent::Completed { .. })),
        "Chat route must yield an LlmEvent::Completed"
    );
    assert_eq!(recorder.requests.load(Ordering::SeqCst), 1);
    let path = recorder.first_path().await.expect("recorder captured path");
    assert_eq!(
        path, "/v1/chat/completions",
        "grok-2 must reach /v1/chat/completions"
    );
}

#[tokio::test]
async fn xai_dispatcher_rejects_grok_imagine_without_request_m33() {
    // M-33: `grok-imagine-*` is image-only and lives on
    // `/v1/images/generations`, which neither sub-provider knows. The
    // dispatcher must short-circuit with a structured
    // `ProviderNotConfigured` error *before* touching the wire, so the
    // recorder must observe zero requests. Without this the request
    // would fall through to a sub-provider and 404 in production.
    let recorder = DispatcherRecorder::new();
    let addr = spawn_xai_dispatcher_server(recorder.clone(), RouteBodies::defaults()).await;
    let provider = XaiProvider::from_config(&dispatcher_xai_config(addr))
        .expect("XaiProvider builds against mock server");

    let stream =
        provider.stream_response(dispatcher_request("grok-imagine"), CancellationToken::new());
    let events = tokio::time::timeout(Duration::from_secs(5), stream.collect::<Vec<_>>())
        .await
        .expect("image-rejection stream must complete within timeout");

    assert_eq!(
        events.len(),
        1,
        "grok-imagine must yield exactly one terminal event"
    );
    match &events[0] {
        Err(SqueezyError::ProviderNotConfigured(msg)) => {
            assert!(
                msg.contains("/v1/images/generations") && msg.contains("M-33"),
                "rejection must cite the unrouted image endpoint (M-33): {msg}"
            );
        }
        other => panic!("expected ProviderNotConfigured rejection, got {other:?}"),
    }

    assert_eq!(
        recorder.requests.load(Ordering::SeqCst),
        0,
        "image-only model must be rejected before any wire request"
    );
}

/// SSE that exercises the Responses parser end-to-end:
/// `response.output_text.delta` carries text, the terminal
/// `response.completed` event reports both reasoning_tokens and a
/// cached_tokens hit. The replay verifies the dispatcher surfaces the
/// model's full cost picture (text + reasoning + cache hit) instead of
/// dropping the reasoning/cache columns silently.
const RESPONSES_REASONING_SSE_BODY: &str = concat!(
    "event: response.created\n",
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_xai_m30\",\"model\":\"grok-4.3\"}}\n\n",
    "event: response.output_text.delta\n",
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
    "event: response.output_text.delta\n",
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\" xai\"}\n\n",
    "event: response.completed\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_xai_m30\",\"model\":\"grok-4.3\",\"usage\":{\"input_tokens\":12,\"input_tokens_details\":{\"cached_tokens\":4},\"output_tokens\":3,\"output_tokens_details\":{\"reasoning_tokens\":5},\"total_tokens\":15}}}\n\n",
    // NOTE: the Responses API does NOT emit a `[DONE]` sentinel (unlike Chat
    // Completions); the stream ends after `response.completed`. Appending one
    // here previously made parse_openai_event fail with "invalid SSE JSON".
);

/// Chat-completions SSE with text deltas and a `usage` block that
/// reports `prompt_tokens_details.cached_tokens` plus
/// `completion_tokens_details.reasoning_tokens`. The replay locks in
/// the canonical xAI Chat shape so a regression in the cost parser
/// (e.g. moving the cached-tokens lookup) surfaces immediately. M-31
/// (top-level `cached_tokens` fallback) lives in `compatible.rs` and
/// stays out of this commit.
const CHAT_REASONING_SSE_BODY: &str = concat!(
    "data: {\"id\":\"chatcmpl_xai_m30\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"}}]}\n\n",
    "data: {\"id\":\"chatcmpl_xai_m30\",\"choices\":[{\"delta\":{\"content\":\" grok\"}}]}\n\n",
    "data: {\"id\":\"chatcmpl_xai_m30\",\"choices\":[{\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":2,\"prompt_tokens_details\":{\"cached_tokens\":3},\"completion_tokens_details\":{\"reasoning_tokens\":7}}}\n\n",
    "data: [DONE]\n\n",
);

#[tokio::test]
async fn xai_responses_sse_replay_surfaces_text_and_reasoning_cost_m30() {
    // M-30: replay the Responses SSE through `XaiProvider` and verify
    // (a) text deltas arrive in order, (b) `LlmEvent::Completed` lands
    // exactly once, (c) `cost.cached_input_tokens` and
    // `cost.reasoning_output_tokens` survive parsing. Without this
    // test, a regression that dropped either column would silently
    // zero out xAI's reasoning telemetry.
    let recorder = DispatcherRecorder::new();
    let bodies = RouteBodies {
        responses: RESPONSES_REASONING_SSE_BODY,
        chat: CHAT_SSE_BODY,
    };
    let addr = spawn_xai_dispatcher_server(recorder.clone(), bodies).await;
    let provider = XaiProvider::from_config(&dispatcher_xai_config(addr))
        .expect("XaiProvider builds against mock server");

    let stream = provider.stream_response(dispatcher_request("grok-4.3"), CancellationToken::new());
    let events: Vec<LlmEvent> =
        tokio::time::timeout(Duration::from_secs(5), stream.collect::<Vec<_>>())
            .await
            .expect("Responses SSE must complete within timeout")
            .into_iter()
            .map(|res| res.expect("Responses SSE must not error"))
            .collect();

    let text: String = events
        .iter()
        .filter_map(|event| match event {
            LlmEvent::TextDelta(delta) => Some(delta.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "hello xai");

    let Some(LlmEvent::Completed { cost, .. }) = events
        .iter()
        .find(|event| matches!(event, LlmEvent::Completed { .. }))
    else {
        panic!("expected LlmEvent::Completed in Responses replay");
    };
    assert_eq!(cost.input_tokens, Some(12));
    assert_eq!(cost.output_tokens, Some(3));
    assert_eq!(
        cost.cached_input_tokens,
        Some(4),
        "Responses path must surface input_tokens_details.cached_tokens"
    );
    assert_eq!(
        cost.reasoning_output_tokens,
        Some(5),
        "Responses path must surface output_tokens_details.reasoning_tokens"
    );
    assert_eq!(
        recorder.first_path().await.as_deref(),
        Some("/v1/responses"),
        "Responses replay must reach the Responses route"
    );
}

#[tokio::test]
async fn xai_chat_sse_replay_surfaces_text_and_reasoning_cost_m30() {
    // M-30: replay the Chat Completions SSE through `XaiProvider` so
    // grok-2-style requests retain their reasoning + cached-token
    // telemetry. The fixture mirrors xAI's documented usage shape; a
    // regression in `parse_chat_usage` that lost the
    // `prompt_tokens_details.cached_tokens` lookup would fail this test
    // immediately.
    let recorder = DispatcherRecorder::new();
    let bodies = RouteBodies {
        responses: RESPONSES_SSE_BODY,
        chat: CHAT_REASONING_SSE_BODY,
    };
    let addr = spawn_xai_dispatcher_server(recorder.clone(), bodies).await;
    let provider = XaiProvider::from_config(&dispatcher_xai_config(addr))
        .expect("XaiProvider builds against mock server");

    let stream = provider.stream_response(dispatcher_request("grok-2"), CancellationToken::new());
    let events: Vec<LlmEvent> =
        tokio::time::timeout(Duration::from_secs(5), stream.collect::<Vec<_>>())
            .await
            .expect("Chat SSE must complete within timeout")
            .into_iter()
            .map(|res| res.expect("Chat SSE must not error"))
            .collect();

    let text: String = events
        .iter()
        .filter_map(|event| match event {
            LlmEvent::TextDelta(delta) => Some(delta.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "hi grok");

    let Some(LlmEvent::Completed { cost, .. }) = events
        .iter()
        .find(|event| matches!(event, LlmEvent::Completed { .. }))
    else {
        panic!("expected LlmEvent::Completed in Chat replay");
    };
    assert_eq!(cost.input_tokens, Some(9));
    assert_eq!(cost.output_tokens, Some(2));
    assert_eq!(
        cost.cached_input_tokens,
        Some(3),
        "Chat path must surface prompt_tokens_details.cached_tokens"
    );
    assert_eq!(
        cost.reasoning_output_tokens,
        Some(7),
        "Chat path must surface completion_tokens_details.reasoning_tokens"
    );
    assert_eq!(
        recorder.first_path().await.as_deref(),
        Some("/v1/chat/completions"),
        "Chat replay must reach the Chat Completions route"
    );
}

#[tokio::test]
async fn xai_chat_and_responses_routes_include_extra_headers_m30() {
    // M-30/H-22: user-supplied headers (telemetry / proxy attribution)
    // must reach both xAI sub-clients. Current Grok models route to
    // Responses, so dropping headers there loses the main path.
    let mut extra = BTreeMap::new();
    extra.insert(
        "helicone-property-tag".to_string(),
        "squeezy-xai-m30".to_string(),
    );

    // One mock server serves both routes; the SSE bodies are
    // route-specific so the parser terminates cleanly.
    let recorder = DispatcherRecorder::new();
    let addr = spawn_xai_dispatcher_server(recorder.clone(), RouteBodies::defaults()).await;
    let mut config = dispatcher_xai_config_with_extra_headers(extra);
    config.base_url = format!("http://{addr}/v1");

    let provider = XaiProvider::from_config(&config)
        .expect("XaiProvider builds with extra_headers across both routes");

    let stream_chat =
        provider.stream_response(dispatcher_request("grok-2"), CancellationToken::new());
    let _ = tokio::time::timeout(Duration::from_secs(5), stream_chat.collect::<Vec<_>>())
        .await
        .expect("chat-route replay must complete");

    let stream_responses =
        provider.stream_response(dispatcher_request("grok-4.3"), CancellationToken::new());
    let _ = tokio::time::timeout(Duration::from_secs(5), stream_responses.collect::<Vec<_>>())
        .await
        .expect("responses-route replay must complete");

    let captured = recorder.captured.lock().await.clone();
    assert_eq!(
        captured.len(),
        2,
        "expected one chat request and one responses request"
    );
    let chat = captured
        .iter()
        .find(|req| req.path == "/v1/chat/completions")
        .expect("chat route captured");
    let responses = captured
        .iter()
        .find(|req| req.path == "/v1/responses")
        .expect("responses route captured");

    assert_eq!(
        chat.headers
            .get("helicone-property-tag")
            .map(String::as_str),
        Some("squeezy-xai-m30"),
        "Chat route must forward extra_headers"
    );
    assert_eq!(
        responses
            .headers
            .get("helicone-property-tag")
            .map(String::as_str),
        Some("squeezy-xai-m30"),
        "Responses route must forward extra_headers"
    );
}

fn dispatcher_xai_config_with_extra_headers(
    extra_headers: BTreeMap<String, String>,
) -> OpenAiCompatibleConfig {
    OpenAiCompatibleConfig {
        preset: OpenAiCompatiblePreset::XAi,
        api_key_env: "XAI_API_KEY".to_string(),
        api_key: Some("test-key".to_string()),
        // Caller overrides this with the actual loopback address before
        // building the provider.
        base_url: "http://127.0.0.1:0/v1".to_string(),
        extra_headers,
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
    }
}

/// xAI Chat-Completions SSE that reports prompt cache hits at the
/// top-level `usage.cached_tokens` field (per xAI's chat docs
/// examples). The squeezy chat parser today only looks at
/// `prompt_tokens_details.cached_tokens` and
/// `prompt_cache_hit_tokens`, so this shape silently zeros out the
/// cached-tokens column. The fixture lives here so M-31's
/// `compatible.rs` fix can flip the assertion from `None` to
/// `Some(42)` in lockstep.
const CHAT_TOPLEVEL_CACHED_TOKENS_SSE_BODY: &str = concat!(
    "data: {\"id\":\"chatcmpl_xai_m31\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"ok\"}}]}\n\n",
    "data: {\"id\":\"chatcmpl_xai_m31\",\"choices\":[{\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":1,\"cached_tokens\":42}}\n\n",
    "data: [DONE]\n\n",
);

#[tokio::test]
async fn xai_chat_top_level_cached_tokens_gap_marker_m31() {
    // M-31: regression marker for xAI's top-level
    // `usage.cached_tokens` shape. The chat parser in `compatible.rs`
    // currently only consults `prompt_tokens_details.cached_tokens`
    // and `prompt_cache_hit_tokens`, so this fixture surfaces
    // `cached_input_tokens = None`. The asymmetry is documented at
    // `.audit/providers/xai.md` (M-31); once `compatible.rs` learns
    // the third fallback, flip this expectation to `Some(42)` to
    // guard against losing the fix.
    //
    // M-31 STATUS: OPEN. This `== None` assertion is a tracked
    // placeholder for an out-of-scope fix that lives in
    // `compatible.rs` (the `parse_chat_usage` top-level
    // `usage.cached_tokens` fallback). It is intentionally left as-is
    // here: when that fallback lands, this assertion MUST flip to
    // `Some(42)` in the same change so the test guards the fix rather
    // than the gap.
    let recorder = DispatcherRecorder::new();
    let bodies = RouteBodies {
        responses: RESPONSES_SSE_BODY,
        chat: CHAT_TOPLEVEL_CACHED_TOKENS_SSE_BODY,
    };
    let addr = spawn_xai_dispatcher_server(recorder.clone(), bodies).await;
    let provider = XaiProvider::from_config(&dispatcher_xai_config(addr))
        .expect("XaiProvider builds against mock server");

    let stream = provider.stream_response(dispatcher_request("grok-2"), CancellationToken::new());
    let events: Vec<LlmEvent> =
        tokio::time::timeout(Duration::from_secs(5), stream.collect::<Vec<_>>())
            .await
            .expect("Chat M-31 SSE must complete within timeout")
            .into_iter()
            .map(|res| res.expect("Chat M-31 SSE must not error"))
            .collect();

    let Some(LlmEvent::Completed { cost, .. }) = events
        .iter()
        .find(|event| matches!(event, LlmEvent::Completed { .. }))
    else {
        panic!("expected LlmEvent::Completed in M-31 replay");
    };
    assert_eq!(cost.input_tokens, Some(20));
    assert_eq!(cost.output_tokens, Some(1));
    assert_eq!(
        cost.cached_input_tokens, None,
        "M-31 gap marker: xAI's top-level `usage.cached_tokens` is not yet picked up by `parse_chat_usage` in compatible.rs. Flip to `Some(42)` once that fix lands."
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
