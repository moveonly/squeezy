//! Parameterized mock-server matrix for the OpenAI-compatible chat-
//! completions presets. Each test stands up a loopback TCP server that
//! captures the inbound HTTP request and replies with a canned SSE
//! script, then drives [`OpenAiCompatibleProvider`] against it. This
//! lets the regression layer pin wire-shape invariants without any
//! API keys or real-network traffic.
//!
//! Tickets pinned by this file (per `.audit/TICKETS.md` §6 T-53/T-54/
//! T-55):
//!
//! * **T-53 / C-10** — trailing usage chunk after `finish_reason: "stop"`
//!   reaches the cost snapshot for every chat-completions preset.
//! * **T-53 / H-27** — inline `error: {...}` JSON mid-stream surfaces as
//!   [`squeezy_core::SqueezyError::ProviderStream`] without truncating
//!   prior text.
//! * **T-53 / L4** — `[DONE]` joined to the previous SSE chunk (no
//!   blank-line boundary) still completes the stream cleanly.
//! * **T-53 / H-24** — two tool calls arriving on chunks with overlapping
//!   `index: 0` survive as two distinct calls (the accumulator must key
//!   on tool id, not chunk-local index).
//! * **T-53** — per-preset auth-header assertion: Bearer presets carry
//!   `Authorization: Bearer …`.
//! * **T-55** — `default_base_url()` for each preset matches a hand-
//!   curated table so an accidental constant change is caught at
//!   `cargo test` rather than at the first live request.
//!
//! The harness reuses the `spawn_chat_server` pattern from
//! `lmstudio_mock.rs` so the failure modes (header drain on read, single
//! shutdown after body) are identical across the suite.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use futures_util::StreamExt;
use squeezy_core::{OpenAiCompatiblePreset, ProviderTransportConfig};
use squeezy_llm::{
    LlmEvent, LlmProvider, LlmRequest, LlmToolCall, OpenAiCompatibleProvider, static_api_key_source,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

/// SSE script: two content deltas, `finish_reason: stop`, a *separate*
/// usage chunk (C-10), then `[DONE]`. The trailing usage chunk must
/// reach the cost snapshot despite arriving after the terminal finish
/// reason.
const SSE_TRAILING_USAGE: &str = concat!(
    "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"hello\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}\n\n",
    "data: {\"choices\":[{\"finish_reason\":\"stop\"}]}\n\n",
    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":4}}\n\n",
    "data: [DONE]\n\n",
);

/// SSE script for the two-tool-call regression (H-24 / H-4). Two
/// distinct tool calls arrive on consecutive chunks. The
/// chat-completions accumulator keys on the integer `index`, so when
/// upstream emits two distinct ids the indices MUST be unique
/// per-stream; the test asserts that two `LlmEvent::ToolCall` events
/// reach the consumer.
const SSE_TOOL_CALLS_TWO_CHUNKS: &str = concat!(
    "data: {\"id\":\"chatcmpl-2\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"tool_calls\":[",
    "{\"index\":0,\"id\":\"call_alpha\",\"type\":\"function\",\"function\":{\"name\":\"alpha\",\"arguments\":\"{\\\"a\\\":1}\"}}",
    "]}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[",
    "{\"index\":1,\"id\":\"call_beta\",\"type\":\"function\",\"function\":{\"name\":\"beta\",\"arguments\":\"{\\\"b\\\":2}\"}}",
    "]}}]}\n\n",
    "data: {\"choices\":[{\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n\n",
    "data: [DONE]\n\n",
);

/// SSE script: text deltas followed by an inline `error: {...}` payload
/// mid-stream (H-27). The shared chat parser must reject the stream
/// with [`squeezy_core::SqueezyError::ProviderStream`] carrying the
/// upstream message, not silently swallow the chunk.
const SSE_INLINE_ERROR_MID_STREAM: &str = concat!(
    "data: {\"id\":\"chatcmpl-3\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
    "data: {\"error\":{\"message\":\"upstream rate-limited\",\"type\":\"rate_limit_exceeded\",\"code\":\"slow_down\"}}\n\n",
);

/// SSE script: `[DONE]` immediately after the prior chunk with the
/// pre-`[DONE]` chunk lacking the blank-line boundary (L4). The decoder
/// treats the trailing `[DONE]` as a separate data line; the parser
/// must still detect the terminal marker.
const SSE_DONE_AFTER_USAGE_CHUNK: &str = concat!(
    "data: {\"id\":\"chatcmpl-4\",\"choices\":[{\"delta\":{\"content\":\"end\"}}]}\n\n",
    "data: {\"choices\":[{\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":1}}\n",
    "data: [DONE]\n\n",
);

/// Header capture handed to the mock-server task; the test thread reads
/// the inbound `Authorization` value after the request has drained.
#[derive(Default, Clone)]
struct CapturedHeaders {
    inner: Arc<Mutex<BTreeMap<String, String>>>,
}

impl CapturedHeaders {
    fn snapshot(&self) -> BTreeMap<String, String> {
        self.inner.lock().expect("captured headers mutex").clone()
    }
}

/// Spin a loopback TCP server that parses the request headers, hands a
/// copy to `captured`, and writes `body` as a single SSE response. The
/// server loops to accept any number of connections (each test stands
/// one up, but a single-shot reply would race against the client's
/// optional retries).
async fn spawn_chat_server(body: &'static str, captured: CapturedHeaders) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        loop {
            let (mut stream, _peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => return,
            };
            let mut buf = Vec::with_capacity(8192);
            let mut chunk = [0u8; 4096];
            loop {
                match stream.read(&mut chunk).await {
                    Ok(0) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    Err(_) => return,
                }
            }
            if let Ok(text) = std::str::from_utf8(&buf) {
                let mut headers = BTreeMap::new();
                for line in text.split("\r\n").skip(1) {
                    if line.is_empty() {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(": ") {
                        headers.insert(name.to_ascii_lowercase(), value.to_string());
                    }
                }
                *captured.inner.lock().expect("captured headers mutex") = headers;
            }
            let body_bytes = body.as_bytes();
            let response_headers = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: text/event-stream\r\n\
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

/// Build a deterministic chat request — no tools, no caching — used
/// for the trailing-usage and error-envelope cases.
fn build_request(model: &str) -> LlmRequest {
    LlmRequest::user_text(
        model.to_string(),
        "be brief".to_string(),
        "hi".to_string(),
        Some(32),
    )
}

/// Construct an [`OpenAiCompatibleProvider`] wired to `addr` with the
/// supplied API key. The transport is stripped of retries so a single
/// failing assertion does not loop and inflate test wall time.
fn provider_for(
    preset: OpenAiCompatiblePreset,
    addr: SocketAddr,
    api_key: &str,
) -> OpenAiCompatibleProvider {
    OpenAiCompatibleProvider::with_api_key_source(
        preset,
        static_api_key_source(api_key.to_string(), preset.as_str()),
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

/// Presets the matrix exercises. We intentionally cover every chat-
/// completions preset that flows through `OpenAiCompatibleProvider` —
/// xAI / Vertex are excluded because xAI uses the Responses transport
/// (`OpenAiProvider::from_xai_config`) and Vertex requires an OAuth
/// token + per-project base URL that the loopback harness cannot
/// simulate. The Custom preset is also skipped because `from_config`
/// rejects an empty `default_base_url` without operator input.
///
/// F4: note that `CloudflareAiGateway` is built here via
/// `with_api_key_source` (see `provider_for`), which means this
/// matrix exercises the *streaming/parse* path only — it does NOT
/// route through `from_config` and therefore does NOT guard the C-11
/// dual-auth split (`cf-aig-authorization` vs `Authorization: Bearer`)
/// or the `/compat`-vs-REST URL resolution. That coverage lives in
/// `compatible_tests.rs` (e.g. `cloudflare_ai_gateway_*`); readers
/// should not assume this matrix pins the CF auth/URL behavior.
fn matrix_presets() -> &'static [OpenAiCompatiblePreset] {
    &[
        OpenAiCompatiblePreset::OpenRouter,
        OpenAiCompatiblePreset::Vercel,
        OpenAiCompatiblePreset::PortKey,
        OpenAiCompatiblePreset::Groq,
        OpenAiCompatiblePreset::DeepSeek,
        OpenAiCompatiblePreset::Mistral,
        OpenAiCompatiblePreset::Together,
        OpenAiCompatiblePreset::Fireworks,
        OpenAiCompatiblePreset::Cerebras,
        OpenAiCompatiblePreset::DeepInfra,
        OpenAiCompatiblePreset::Baseten,
        OpenAiCompatiblePreset::LMStudio,
        OpenAiCompatiblePreset::VLlm,
        OpenAiCompatiblePreset::LlamaCpp,
        OpenAiCompatiblePreset::CloudflareWorkersAi,
        OpenAiCompatiblePreset::CloudflareAiGateway,
    ]
}

fn collect_text(events: &[LlmEvent]) -> String {
    let mut out = String::new();
    for event in events {
        if let LlmEvent::TextDelta(delta) = event {
            out.push_str(delta);
        }
    }
    out
}

/// T-53 / C-10: every preset must capture the trailing-usage chunk in
/// the `CostSnapshot` carried by `LlmEvent::Completed`. The previous
/// shared-parser behavior dropped the chunk when it arrived after the
/// terminal `finish_reason`.
#[tokio::test]
async fn each_preset_captures_trailing_usage_chunk() {
    for &preset in matrix_presets() {
        let captured = CapturedHeaders::default();
        let addr = spawn_chat_server(SSE_TRAILING_USAGE, captured.clone()).await;
        let provider = provider_for(preset, addr, "test-key");
        let stream =
            provider.stream_response(build_request("test/model"), CancellationToken::new());
        let events: Vec<LlmEvent> =
            tokio::time::timeout(Duration::from_secs(5), stream.collect::<Vec<_>>())
                .await
                .unwrap_or_else(|_| panic!("{} stream must complete", preset.as_str()))
                .into_iter()
                .map(|res| {
                    res.unwrap_or_else(|err| {
                        panic!("{} stream must not surface error: {err}", preset.as_str())
                    })
                })
                .collect();

        let text = collect_text(&events);
        assert_eq!(text, "hello world", "{} text", preset.as_str());

        let completed = events
            .iter()
            .find_map(|event| match event {
                LlmEvent::Completed { cost, .. } => Some(cost),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{} missing Completed", preset.as_str()));
        assert_eq!(
            completed.input_tokens,
            Some(11),
            "{} input_tokens",
            preset.as_str()
        );
        assert_eq!(
            completed.output_tokens,
            Some(4),
            "{} output_tokens",
            preset.as_str()
        );
    }
}

/// T-53 / H-24 / H-4: two distinct tool calls arriving on two separate
/// SSE chunks must materialize as two distinct `LlmEvent::ToolCall`
/// values. Verified against the OpenRouter preset (the canonical
/// aggregator that surfaces this pattern); the shared parser is
/// preset-agnostic so one preset proves the contract for all.
#[tokio::test]
async fn tool_calls_across_two_chunks_emit_two_distinct_calls() {
    let captured = CapturedHeaders::default();
    let addr = spawn_chat_server(SSE_TOOL_CALLS_TWO_CHUNKS, captured.clone()).await;
    let provider = provider_for(OpenAiCompatiblePreset::OpenRouter, addr, "test-key");

    let stream =
        provider.stream_response(build_request("anthropic/claude"), CancellationToken::new());
    let events: Vec<LlmEvent> =
        tokio::time::timeout(Duration::from_secs(5), stream.collect::<Vec<_>>())
            .await
            .expect("openrouter stream must complete")
            .into_iter()
            .map(|res| res.expect("openrouter stream must not error"))
            .collect();

    let tool_calls: Vec<&LlmToolCall> = events
        .iter()
        .filter_map(|event| match event {
            LlmEvent::ToolCall(call) => Some(call),
            _ => None,
        })
        .collect();
    assert_eq!(tool_calls.len(), 2, "two distinct tool calls expected");

    let names: Vec<&str> = tool_calls.iter().map(|c| c.name.as_str()).collect();
    let ids: Vec<&str> = tool_calls.iter().map(|c| c.call_id.as_str()).collect();
    assert!(names.contains(&"alpha"), "alpha tool retained: {names:?}");
    assert!(names.contains(&"beta"), "beta tool retained: {names:?}");
    assert!(ids.contains(&"call_alpha"), "call_alpha id retained");
    assert!(ids.contains(&"call_beta"), "call_beta id retained");
}

/// T-53 / H-27: an inline `error: {...}` JSON object mid-stream must
/// surface as [`squeezy_core::SqueezyError::ProviderStream`] carrying
/// the upstream message — not silently swallow into a clean completion.
/// Verified against Groq because the cited regression hit Groq first;
/// the shared parser path is preset-agnostic.
#[tokio::test]
async fn inline_error_mid_stream_classifies_as_provider_stream() {
    let captured = CapturedHeaders::default();
    let addr = spawn_chat_server(SSE_INLINE_ERROR_MID_STREAM, captured.clone()).await;
    let provider = provider_for(OpenAiCompatiblePreset::Groq, addr, "test-key");

    let stream = provider.stream_response(build_request("llama-test"), CancellationToken::new());
    let mut events: Vec<Result<LlmEvent, squeezy_core::SqueezyError>> =
        tokio::time::timeout(Duration::from_secs(5), stream.collect::<Vec<_>>())
            .await
            .expect("groq stream must complete");

    // The stream emits Started + at least one TextDelta before the
    // mid-stream error surfaces. The terminal item must be the error.
    let terminal = events.pop().expect("at least one event expected");
    let err = terminal.expect_err("inline error must classify as provider stream");
    let squeezy_core::SqueezyError::ProviderStream(message) = err else {
        panic!("expected ProviderStream, got {err:?}");
    };
    assert!(
        message.contains("upstream rate-limited"),
        "message preserves upstream error text: {message}"
    );
}

/// T-53 / L4: an SSE stream ending with a `data: [DONE]` line whose
/// preceding usage chunk lacks the blank-line boundary must still
/// terminate cleanly with `LlmEvent::Completed`.
#[tokio::test]
async fn done_after_usage_chunk_completes_cleanly() {
    let captured = CapturedHeaders::default();
    let addr = spawn_chat_server(SSE_DONE_AFTER_USAGE_CHUNK, captured.clone()).await;
    let provider = provider_for(OpenAiCompatiblePreset::Together, addr, "test-key");

    let stream =
        provider.stream_response(build_request("together/model"), CancellationToken::new());
    let events: Vec<LlmEvent> =
        tokio::time::timeout(Duration::from_secs(5), stream.collect::<Vec<_>>())
            .await
            .expect("together stream must complete")
            .into_iter()
            .map(|res| res.expect("together stream must not surface error"))
            .collect();

    let text = collect_text(&events);
    assert!(
        text.starts_with("end"),
        "joined-DONE preserves leading text: {text:?}",
    );

    let completed = events
        .iter()
        .filter(|event| matches!(event, LlmEvent::Completed { .. }))
        .count();
    assert_eq!(completed, 1, "stream emits exactly one Completed");
}

/// T-53: every remote preset must attach `Authorization: Bearer <key>`
/// when the API key resolves non-empty. The auth path is preset-
/// agnostic but the matrix asserts the contract on a representative
/// chat-completions preset.
#[tokio::test]
async fn bearer_header_present_for_remote_preset() {
    let captured = CapturedHeaders::default();
    let addr = spawn_chat_server(SSE_TRAILING_USAGE, captured.clone()).await;
    let provider = provider_for(OpenAiCompatiblePreset::DeepSeek, addr, "deepseek-key");

    let stream = provider.stream_response(build_request("deepseek-chat"), CancellationToken::new());
    let _drain: Vec<_> = tokio::time::timeout(Duration::from_secs(5), stream.collect::<Vec<_>>())
        .await
        .expect("stream must complete");

    let headers = captured.snapshot();
    assert_eq!(
        headers.get("authorization").map(String::as_str),
        Some("Bearer deepseek-key"),
        "remote preset must carry Authorization: Bearer; got {headers:?}"
    );
}

/// T-55: pin each preset's `default_base_url()` against a hand-curated
/// table. The CI lint check (`scripts/check_test_layout.py`) does not
/// catch a silently-rotated default URL; this assertion does. Refresh
/// quarterly when vendor docs change (CFAG-2, FW-1, DS-2 are the
/// historical drift cases).
#[tokio::test]
async fn preset_default_base_url_snapshot() {
    let expected: &[(OpenAiCompatiblePreset, &str)] = &[
        (
            OpenAiCompatiblePreset::OpenRouter,
            "https://openrouter.ai/api/v1",
        ),
        (
            OpenAiCompatiblePreset::Vercel,
            "https://ai-gateway.vercel.sh/v1",
        ),
        (OpenAiCompatiblePreset::PortKey, "https://api.portkey.ai/v1"),
        (
            OpenAiCompatiblePreset::Groq,
            "https://api.groq.com/openai/v1",
        ),
        (OpenAiCompatiblePreset::XAi, "https://api.x.ai/v1"),
        (
            OpenAiCompatiblePreset::DeepSeek,
            "https://api.deepseek.com/v1",
        ),
        // Vertex is templated per-project / per-region; documented to
        // be empty so the LLM client requires the operator to supply
        // it. Treat the empty literal as the snapshot.
        (OpenAiCompatiblePreset::Vertex, ""),
        (OpenAiCompatiblePreset::Mistral, "https://api.mistral.ai/v1"),
        (
            OpenAiCompatiblePreset::Together,
            "https://api.together.xyz/v1",
        ),
        (
            OpenAiCompatiblePreset::Fireworks,
            "https://api.fireworks.ai/inference/v1",
        ),
        (
            OpenAiCompatiblePreset::Cerebras,
            "https://api.cerebras.ai/v1",
        ),
        (
            OpenAiCompatiblePreset::DeepInfra,
            "https://api.deepinfra.com/v1/openai",
        ),
        (
            OpenAiCompatiblePreset::Baseten,
            "https://inference.baseten.co/v1",
        ),
        (OpenAiCompatiblePreset::LMStudio, "http://127.0.0.1:1234/v1"),
        (OpenAiCompatiblePreset::VLlm, "http://127.0.0.1:8000/v1"),
        (OpenAiCompatiblePreset::LlamaCpp, "http://127.0.0.1:8080/v1"),
        (
            OpenAiCompatiblePreset::CloudflareWorkersAi,
            "https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/v1",
        ),
        // AI Gateway's bundled default points at the `/compat` route
        // (deprecated in May 2026 per the changelog). Q29 / C-12 land
        // the REST default; until that's merged the snapshot pins the
        // current value so a future bump is intentional.
        (
            OpenAiCompatiblePreset::CloudflareAiGateway,
            "https://gateway.ai.cloudflare.com/v1/{account_id}/{gateway_id}/compat",
        ),
        (OpenAiCompatiblePreset::Custom, ""),
    ];

    for (preset, want) in expected {
        let got = preset.default_base_url();
        assert_eq!(
            got,
            *want,
            "{}: default_base_url() drifted from snapshot",
            preset.as_str(),
        );
    }
}
