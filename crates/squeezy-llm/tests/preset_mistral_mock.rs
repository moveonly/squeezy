//! Mistral-specific mock-server tests pinning the wire-shape quirks
//! enumerated in `.audit/providers/preset-mistral.md`.
//!
//! Tickets pinned by this file (per `.audit/TICKETS.md` §6):
//!
//! * **T-67 / MIS-3 / H-56** — `prompt_cache_retention` used to be
//!   unconditionally emitted by the shared chat-completions adapter, but
//!   Mistral's schema rejects unknown top-level fields with
//!   `extra_forbidden` 422. The shipped per-preset gate drops the field
//!   for Mistral; this test asserts the field is absent and guards the
//!   gate against regression.
//! * **T-67 / MIS-2 / H-55** — the adapter used to emit both
//!   `reasoning_effort` and the nested `reasoning: { effort }`; Mistral
//!   accepts only `reasoning_effort` and 422s on the nested object. The
//!   shipped fix gates off the nested object for Mistral; this test
//!   asserts only the flat field survives and guards against regression.
//! * **MIS-6 / MS-1** — `tool_choice = "any"` and `tool_choice = "required"`
//!   are both accepted by Mistral's June-2026 schema and pass through
//!   the shared adapter verbatim. This is the resolved-by-vendor case
//!   from the audit — keep it green so a future per-vendor map does
//!   not regress the round-trip.
//! * **MIS-11 / H-6** — Mistral's 422 error envelope shape
//!   (`{ object: "error", message: { detail: [...] }, type: "invalid_request_error" }`)
//!   should surface as a [`squeezy_core::SqueezyError::ProviderRequest`]
//!   carrying the upstream's structured detail. Pinned at the
//!   `default_message` path today; a future MIS-11 fix lifts the
//!   detail array into the surfaced message.
//!
//! The mock server captures the request body so the test can assert
//! body shape without depending on `pub(crate)` accessors.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::Value;
use squeezy_core::{OpenAiCompatiblePreset, ProviderTransportConfig, ReasoningEffort};
use squeezy_llm::{
    CacheRetention, CacheSpec, LlmEvent, LlmProvider, LlmRequest, OpenAiCompatibleProvider,
    static_api_key_source,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

const SSE_HELLO_DONE: &str = concat!(
    "data: {\"id\":\"mistral-1\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"ok\"}}]}\n\n",
    "data: {\"choices\":[{\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\n",
    "data: [DONE]\n\n",
);

/// Mistral's verified 422 error envelope. Shape sourced from
/// `open-webui/open-webui#10167` per the audit cross-reference.
const MISTRAL_422_ENVELOPE: &str = concat!(
    "{",
    "\"object\":\"error\",",
    "\"message\":{\"detail\":[{",
    "\"type\":\"extra_forbidden\",",
    "\"loc\":[\"body\",\"prompt_cache_retention\"],",
    "\"msg\":\"Extra inputs are not permitted\",",
    "\"input\":\"24h\"",
    "}]},",
    "\"type\":\"invalid_request_error\",",
    "\"raw_status_code\":422",
    "}"
);

/// Per-request capture: headers + request body + path. Read by tests
/// after the stream drains.
#[derive(Default, Clone)]
struct CapturedRequest {
    inner: Arc<Mutex<CapturedInner>>,
}

#[derive(Default)]
struct CapturedInner {
    headers: BTreeMap<String, String>,
    body: String,
    path: String,
}

impl CapturedRequest {
    fn snapshot(&self) -> (BTreeMap<String, String>, String, String) {
        let inner = self.inner.lock().expect("captured request mutex");
        (
            inner.headers.clone(),
            inner.body.clone(),
            inner.path.clone(),
        )
    }
}

/// Spin a loopback TCP server that:
///   1. Reads the full HTTP request (headers + body) into `captured`.
///   2. Writes either the SSE script `body` (when `status = 200`) or an
///      HTTP error envelope (`status = 422`) so the test can drive both
///      success and failure paths from one harness.
async fn spawn_server(
    body: &'static str,
    status: u16,
    response_content_type: &'static str,
    captured: CapturedRequest,
) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        loop {
            let (mut stream, _peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => return,
            };
            let mut buf = Vec::with_capacity(16_384);
            let mut chunk = [0u8; 4096];
            // Read until end-of-headers AND we've consumed any
            // declared Content-Length payload.
            let mut content_length: Option<usize> = None;
            let mut header_end: Option<usize> = None;
            loop {
                match stream.read(&mut chunk).await {
                    Ok(0) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        if header_end.is_none()
                            && let Some(pos) =
                                buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
                        {
                            header_end = Some(pos);
                            let headers_text = std::str::from_utf8(&buf[..pos]).unwrap_or_default();
                            for line in headers_text.split("\r\n") {
                                let normalized = line.to_ascii_lowercase();
                                if let Some(rest) = normalized.strip_prefix("content-length: ")
                                    && let Ok(v) = rest.parse()
                                {
                                    content_length = Some(v);
                                }
                            }
                        }
                        if let (Some(end), Some(cl)) = (header_end, content_length)
                            && buf.len() >= end + cl
                        {
                            break;
                        }
                    }
                    Err(_) => return,
                }
            }
            if let Ok(text) = std::str::from_utf8(&buf) {
                let mut headers = BTreeMap::new();
                let mut path = String::new();
                let header_end_idx = header_end.unwrap_or(text.len());
                let header_block = &text[..header_end_idx];
                for (i, line) in header_block.split("\r\n").enumerate() {
                    if i == 0 {
                        // Request line: `POST /v1/chat/completions HTTP/1.1`.
                        if let Some(p) = line.split_whitespace().nth(1) {
                            path = p.to_string();
                        }
                    } else if line.is_empty() {
                        break;
                    } else if let Some((name, value)) = line.split_once(": ") {
                        headers.insert(name.to_ascii_lowercase(), value.to_string());
                    }
                }
                let body_text = if let Some(end) = header_end {
                    std::str::from_utf8(buf.get(end..).unwrap_or(&[]))
                        .unwrap_or_default()
                        .to_string()
                } else {
                    String::new()
                };
                let mut inner = captured.inner.lock().expect("captured request mutex");
                inner.headers = headers;
                inner.body = body_text;
                inner.path = path;
            }
            let body_bytes = body.as_bytes();
            let status_line = match status {
                200 => "200 OK",
                422 => "422 Unprocessable Entity",
                _ => "500 Internal Server Error",
            };
            let response_headers = format!(
                "HTTP/1.1 {status_line}\r\n\
                 Content-Type: {response_content_type}\r\n\
                 Cache-Control: no-cache\r\n\
                 Content-Length: {}\r\n\
                 \r\n",
                body_bytes.len()
            );
            if stream.write_all(response_headers.as_bytes()).await.is_err() {
                continue;
            }
            let _ = stream.write_all(body_bytes).await;
            let _ = stream.shutdown().await;
        }
    });
    addr
}

fn provider_for(addr: SocketAddr, api_key: &str) -> OpenAiCompatibleProvider {
    OpenAiCompatibleProvider::with_api_key_source(
        OpenAiCompatiblePreset::Mistral,
        static_api_key_source(api_key.to_string(), "mistral"),
        format!("http://{addr}/v1"),
        BTreeMap::new(),
        ProviderTransportConfig {
            request_max_retries: 0,
            stream_max_retries: 0,
            stream_idle_timeout_ms: 5_000,
            ..ProviderTransportConfig::default()
        },
    )
}

fn build_request_with(adjust: impl FnOnce(&mut LlmRequest)) -> LlmRequest {
    let mut request = LlmRequest::user_text(
        "mistral-large-2512".to_string(),
        "be brief".to_string(),
        "hi".to_string(),
        Some(32),
    );
    adjust(&mut request);
    request
}

async fn drain_stream(provider: &OpenAiCompatibleProvider, request: LlmRequest) {
    let stream = provider.stream_response(request, CancellationToken::new());
    let _events: Vec<_> = tokio::time::timeout(Duration::from_secs(5), stream.collect::<Vec<_>>())
        .await
        .expect("stream must complete");
}

/// MIS-2 / H-55 (shipped): the shared adapter emits the top-level
/// `reasoning_effort` verbatim but gates off the nested
/// `reasoning: { effort }` object for the Mistral preset, since Mistral
/// 422s on the nested form with `extra_forbidden`. This test asserts the
/// post-fix shape — `reasoning_effort == "high"` with no nested
/// `reasoning.effort` — and guards against a regression that reintroduces
/// the nested object.
#[tokio::test]
async fn mistral_drops_nested_reasoning_effort() {
    let captured = CapturedRequest::default();
    let addr = spawn_server(SSE_HELLO_DONE, 200, "text/event-stream", captured.clone()).await;
    let provider = provider_for(addr, "test-key");
    let request = build_request_with(|req| {
        req.reasoning_effort = Some(ReasoningEffort::High);
    });
    drain_stream(&provider, request).await;

    let (_, body_text, path) = captured.snapshot();
    assert!(
        path.ends_with("/chat/completions"),
        "Mistral must POST to /chat/completions, got {path}"
    );
    let body: Value = serde_json::from_str(&body_text).expect("body is JSON");
    assert_eq!(
        body.get("reasoning_effort").and_then(Value::as_str),
        Some("high"),
        "top-level reasoning_effort sent verbatim"
    );
    // MIS-2 / H-55 fix: nested reasoning.effort dropped for Mistral.
    assert_eq!(
        body.get("reasoning")
            .and_then(|r| r.get("effort"))
            .and_then(Value::as_str),
        None,
        "nested reasoning.effort gated off for Mistral (H-55)",
    );
}

/// MIS-3 / H-56 (shipped): the shared adapter no longer emits
/// `prompt_cache_retention` for the Mistral preset, since Mistral 422s on
/// the unknown top-level field. This test asserts the post-fix shape —
/// `prompt_cache_retention` absent while `prompt_cache_key` still flows
/// through — and guards against a regression that reintroduces the field.
#[tokio::test]
async fn mistral_drops_prompt_cache_retention() {
    let captured = CapturedRequest::default();
    let addr = spawn_server(SSE_HELLO_DONE, 200, "text/event-stream", captured.clone()).await;
    let provider = provider_for(addr, "test-key");
    let request = build_request_with(|req| {
        req.cache = CacheSpec {
            key: Some("affinity".to_string()),
            retention: CacheRetention::Long,
        };
    });
    drain_stream(&provider, request).await;

    let (_, body_text, _) = captured.snapshot();
    let body: Value = serde_json::from_str(&body_text).expect("body is JSON");
    // MIS-3 / H-56 fix: prompt_cache_retention dropped for Mistral.
    assert_eq!(
        body.get("prompt_cache_retention").and_then(Value::as_str),
        None,
        "prompt_cache_retention gated off for Mistral (H-56)",
    );
    assert_eq!(
        body.get("prompt_cache_key").and_then(Value::as_str),
        Some("affinity"),
        "prompt_cache_key still flows through (Mistral schema accepts it)",
    );
}

/// MIS-6 / MS-1 (resolved-by-vendor): `tool_choice = "any"` is
/// accepted by the June-2026 Mistral schema and passes through the
/// shared adapter verbatim. Keep this green so a future per-vendor
/// map for `tool_choice` does not silently project `any` to a
/// different value.
#[tokio::test]
async fn mistral_tool_choice_any_passes_through() {
    let captured = CapturedRequest::default();
    let addr = spawn_server(SSE_HELLO_DONE, 200, "text/event-stream", captured.clone()).await;
    let provider = provider_for(addr, "test-key");
    // Tool list must be non-empty — the shared adapter only emits
    // `tool_choice` when tools are advertised (otherwise the field is
    // a no-op).
    let tool = Arc::new(squeezy_llm::LlmToolSpec {
        name: "noop".to_string(),
        description: "placeholder".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {},
        }),
        strict: false,
    });
    let request = build_request_with(|req| {
        req.tools = Arc::from(vec![tool]);
        req.tool_choice = Some("any".to_string());
    });
    drain_stream(&provider, request).await;

    let (_, body_text, _) = captured.snapshot();
    let body: Value = serde_json::from_str(&body_text).expect("body is JSON");
    assert_eq!(
        body.get("tool_choice").and_then(Value::as_str),
        Some("any"),
        "tool_choice = any passes through verbatim",
    );
}

/// MIS-11 / H-6: Mistral's structured 422 envelope is a different shape
/// than OpenAI's `{"error": {...}}`. Today `format_chat_error` falls
/// through to the raw body when the `error` key is absent — the test
/// pins the current message so a future structured-detail extractor
/// lands with a passing test that asserts the detail array is surfaced.
#[tokio::test]
async fn mistral_422_envelope_surfaces_as_provider_request() {
    let captured = CapturedRequest::default();
    let addr = spawn_server(
        MISTRAL_422_ENVELOPE,
        422,
        "application/json",
        captured.clone(),
    )
    .await;
    let provider = provider_for(addr, "test-key");
    // Set CacheRetention::Long to provoke the `prompt_cache_retention`
    // field — that's the field the mock complains about. Either way,
    // the response status is 422 unconditionally.
    let request = build_request_with(|req| {
        req.cache = CacheSpec {
            key: None,
            retention: CacheRetention::Long,
        };
    });
    let stream = provider.stream_response(request, CancellationToken::new());
    let events: Vec<Result<LlmEvent, squeezy_core::SqueezyError>> =
        tokio::time::timeout(Duration::from_secs(5), stream.collect::<Vec<_>>())
            .await
            .expect("stream must complete");
    let err = events
        .into_iter()
        .find_map(|res| res.err())
        .expect("422 must surface as an error");
    let squeezy_core::SqueezyError::ProviderRequest(msg) = err else {
        panic!("expected ProviderRequest, got {err:?}");
    };
    assert!(
        msg.contains("Mistral"),
        "error names the preset display: {msg}"
    );
    assert!(msg.contains("422"), "error carries the HTTP status: {msg}");
    // Today `format_chat_error` does not parse Mistral's envelope; the
    // raw body falls through into the surfaced message. Once MIS-11
    // lands the detail array is extracted; loosen this assertion to
    // match the structured shape at that point.
    assert!(
        msg.contains("Extra inputs are not permitted")
            || msg.contains("invalid_request_error")
            || msg.contains("extra_forbidden"),
        "error preserves at least one upstream signal: {msg}"
    );
}
