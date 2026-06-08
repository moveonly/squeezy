//! Legacy MCP HTTP+SSE client transport.
//!
//! Implements the 2024-11-05 spec where the server publishes an SSE stream
//! over GET that carries:
//!
//! * `event: endpoint` — `data:` is the URL the client must POST JSON-RPC
//!   messages to. Emitted once at the start of the stream.
//! * `event: message` (or an unlabeled event) — `data:` is a JSON-RPC payload
//!   the server is sending back to the client.
//!
//! Outbound client→server messages go via `POST <endpoint_url>` with the
//! JSON-RPC body; the server replies `202 Accepted` and later delivers the
//! matching response over the same SSE stream.
//!
//! This is distinct from the 2025-03-26 streamable-HTTP transport, where SSE
//! framing is an *internal* detail of `StreamableHttpClientTransport`. The two
//! are wire-incompatible at the endpoint level (one URL vs two; client knows
//! the POST URL up front vs. learns it from `event: endpoint`), so an SSE
//! server cannot be driven by the streamable-HTTP client and vice versa.

use std::{collections::HashMap, time::Duration};

use futures_util::StreamExt;
use http::{HeaderName, HeaderValue};
use reqwest::{Client, Response, Url};
use rmcp::{
    RoleClient,
    model::{ClientJsonRpcMessage, ServerJsonRpcMessage},
    transport::worker::{Worker, WorkerConfig, WorkerContext, WorkerQuitReason, WorkerSendRequest},
};
use tracing::{debug, warn};

/// Initial delay between automatic reconnect attempts when the SSE stream ends.
const SSE_RECONNECT_DELAY_INITIAL: Duration = Duration::from_millis(1000);
/// Maximum delay between reconnect attempts (exponential back-off ceiling).
const SSE_RECONNECT_DELAY_MAX: Duration = Duration::from_secs(30);
/// Number of consecutive reconnect failures before the worker gives up.
/// A Windows service that closes immediately would otherwise spin forever.
const SSE_RECONNECT_MAX_ATTEMPTS: u32 = 10;

/// Errors raised by the SSE transport worker.
#[derive(Debug, thiserror::Error)]
pub enum SseTransportError {
    #[error("transport channel closed")]
    Closed,
    #[error("join error: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("invalid url {url:?}: {message}")]
    InvalidUrl { url: String, message: String },
    #[error("sse stream ended before endpoint event")]
    MissingEndpointEvent,
    #[error("sse server returned status {status} when opening stream")]
    StreamOpenStatus { status: reqwest::StatusCode },
    #[error("sse server returned status {status} for posted message")]
    PostStatus { status: reqwest::StatusCode },
    #[error("invalid sse line: {0:?}")]
    InvalidLine(String),
}

/// Driver for the legacy MCP HTTP+SSE client transport.
///
/// The worker holds open a GET against `sse_url` and routes outbound messages
/// from the rmcp service handler through POSTs to the endpoint URL the server
/// advertises via `event: endpoint`. The worker drops automatically when the
/// rmcp transport is closed.
pub struct SseClientWorker {
    /// SSE stream URL (the one the user configured under `url`).
    pub sse_url: String,
    /// HTTP client; carries auth/custom headers via the request builders.
    pub client: Client,
    /// Optional `Authorization: Bearer ...` header value (raw token, the
    /// builder prepends `Bearer `). Mirrors `StreamableHttpClientTransportConfig`.
    pub auth_header: Option<String>,
    /// Static headers applied to every request on this transport.
    pub custom_headers: HashMap<HeaderName, HeaderValue>,
}

impl Worker for SseClientWorker {
    type Error = SseTransportError;
    type Role = RoleClient;

    fn err_closed() -> Self::Error {
        SseTransportError::Closed
    }

    fn err_join(error: tokio::task::JoinError) -> Self::Error {
        SseTransportError::Join(error)
    }

    fn config(&self) -> WorkerConfig {
        let mut config = WorkerConfig::default();
        config.name = Some("mcp-sse-client".to_string());
        config.channel_buffer_capacity = 32;
        config
    }

    async fn run(
        self,
        mut context: WorkerContext<Self>,
    ) -> Result<(), WorkerQuitReason<Self::Error>> {
        let response = self
            .open_stream()
            .await
            .map_err(WorkerQuitReason::fatal_context("opening sse stream"))?;
        let mut frames = sse_frame_stream(response);
        // The server MUST advertise the message-post URL via `event: endpoint`
        // before sending any `message` events. We keep draining until we either
        // see that event or hit a recoverable end-of-stream that we drain again.
        let mut endpoint_url = match read_endpoint(&self.sse_url, &mut frames).await {
            Ok(url) => url,
            Err(error) => {
                return Err(WorkerQuitReason::fatal(error, "reading endpoint event"));
            }
        };
        debug!(
            target: "squeezy::mcp::sse",
            sse_url = %self.sse_url,
            endpoint_url = %endpoint_url,
            "established legacy MCP SSE session"
        );

        // Reconnect state: exponential back-off starting at 1 s, capped at
        // 30 s, with a hard limit on consecutive failures. A successful
        // reconnect (stream opens and delivers `event: endpoint`) resets the
        // counter. This prevents a Windows local service that exits immediately
        // from spinning at 1 s intervals indefinitely while the session is held
        // open by the agent.
        let mut reconnect_attempts: u32 = 0;
        let mut reconnect_delay = SSE_RECONNECT_DELAY_INITIAL;

        loop {
            tokio::select! {
                _ = context.cancellation_token.cancelled() => {
                    return Err(WorkerQuitReason::Cancelled);
                }
                send_req = context.from_handler_rx.recv() => {
                    let Some(send_req) = send_req else {
                        return Err(WorkerQuitReason::HandlerTerminated);
                    };
                    self.handle_send(&endpoint_url, send_req).await;
                }
                frame = frames.next() => {
                    match frame {
                        Some(SseEvent { event, data, .. }) => {
                            if !is_message_event(event.as_deref()) {
                                debug!(
                                    target: "squeezy::mcp::sse",
                                    event = ?event,
                                    "ignoring non-message sse event after handshake"
                                );
                                continue;
                            }
                            let Some(payload) = data else {
                                continue;
                            };
                            if payload.trim().is_empty() {
                                continue;
                            }
                            match serde_json::from_str::<ServerJsonRpcMessage>(&payload) {
                                Ok(message) => {
                                    context.send_to_handler(message).await?;
                                }
                                Err(err) => {
                                    warn!(
                                        target: "squeezy::mcp::sse",
                                        error = %err,
                                        data = %payload,
                                        "failed to decode jsonrpc message from sse data field"
                                    );
                                }
                            }
                        }
                        None => {
                            // Stream ended; reopen after an exponentially
                            // increasing back-off up to SSE_RECONNECT_DELAY_MAX.
                            // Each new session re-advertises its POST endpoint, so
                            // we replace the stale URL rather than reusing it.
                            reconnect_attempts += 1;
                            if reconnect_attempts > SSE_RECONNECT_MAX_ATTEMPTS {
                                return Err(WorkerQuitReason::fatal(
                                    SseTransportError::Closed,
                                    "SSE stream closed repeatedly; giving up after max reconnect attempts",
                                ));
                            }
                            warn!(
                                target: "squeezy::mcp::sse",
                                sse_url = %self.sse_url,
                                attempt = reconnect_attempts,
                                delay_ms = reconnect_delay.as_millis(),
                                "SSE stream ended; reconnecting"
                            );
                            // Race the back-off sleep against cancellation so
                            // that session shutdown is not delayed by up to the
                            // full SSE_RECONNECT_DELAY_MAX ceiling.
                            tokio::select! {
                                _ = context.cancellation_token.cancelled() => {
                                    return Err(WorkerQuitReason::Cancelled);
                                }
                                _ = tokio::time::sleep(reconnect_delay) => {}
                            }
                            reconnect_delay = (reconnect_delay * 2).min(SSE_RECONNECT_DELAY_MAX);
                            let response = match self.open_stream().await {
                                Ok(response) => response,
                                Err(error) => {
                                    return Err(WorkerQuitReason::fatal(
                                        error,
                                        "reopening sse stream",
                                    ));
                                }
                            };
                            frames = sse_frame_stream(response);
                            match read_endpoint(&self.sse_url, &mut frames).await {
                                Ok(new_endpoint) => {
                                    endpoint_url = new_endpoint;
                                    // The server is reachable again: reset the
                                    // backoff state now, not only after a
                                    // message arrives. Servers that never push
                                    // proactively (only reply to calls) would
                                    // otherwise never reset the counter.
                                    reconnect_attempts = 0;
                                    reconnect_delay = SSE_RECONNECT_DELAY_INITIAL;
                                }
                                Err(error) => {
                                    return Err(WorkerQuitReason::fatal(
                                        error,
                                        "reading endpoint event on reconnect",
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

impl SseClientWorker {
    async fn open_stream(&self) -> Result<Response, SseTransportError> {
        let mut builder = self
            .client
            .get(&self.sse_url)
            .header(reqwest::header::ACCEPT, "text/event-stream");
        if let Some(token) = &self.auth_header {
            builder = builder.bearer_auth(token);
        }
        for (name, value) in &self.custom_headers {
            builder = builder.header(name, value);
        }
        let response = builder.send().await?;
        let status = response.status();
        if !status.is_success() {
            return Err(SseTransportError::StreamOpenStatus { status });
        }
        Ok(response)
    }

    async fn handle_send(&self, endpoint_url: &str, send_req: WorkerSendRequest<Self>) {
        let WorkerSendRequest {
            message, responder, ..
        } = send_req;
        let result = self.post_message(endpoint_url, &message).await;
        // The responder may have been dropped if the handler already gave up
        // waiting; that is not a transport-level error.
        let _ = responder.send(result);
    }

    async fn post_message(
        &self,
        endpoint_url: &str,
        message: &ClientJsonRpcMessage,
    ) -> Result<(), SseTransportError> {
        let mut builder = self
            .client
            .post(endpoint_url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(message);
        if let Some(token) = &self.auth_header {
            builder = builder.bearer_auth(token);
        }
        for (name, value) in &self.custom_headers {
            builder = builder.header(name, value);
        }
        let response = builder.send().await?;
        let status = response.status();
        if status.is_success() {
            Ok(())
        } else {
            Err(SseTransportError::PostStatus { status })
        }
    }
}

/// One decoded SSE frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SseEvent {
    pub event: Option<String>,
    pub data: Option<String>,
    pub id: Option<String>,
    pub retry: Option<u64>,
}

/// Resolve `event` against the “is this a JSON-RPC message” rule. Per the
/// SSE spec, an absent or empty event name defaults to `message`.
fn is_message_event(event: Option<&str>) -> bool {
    matches!(event, None | Some("") | Some("message"))
}

/// Wait for the first `event: endpoint` frame. The frame's `data` field is
/// the URL (absolute or relative to the SSE URL) the client must POST
/// outbound messages to.
async fn read_endpoint<S>(sse_url: &str, frames: &mut S) -> Result<String, SseTransportError>
where
    S: futures_util::Stream<Item = SseEvent> + Unpin,
{
    while let Some(frame) = frames.next().await {
        if frame.event.as_deref() == Some("endpoint") {
            let Some(payload) = frame.data else {
                continue;
            };
            return resolve_endpoint_url(sse_url, payload.trim());
        }
        if is_message_event(frame.event.as_deref()) {
            warn!(
                target: "squeezy::mcp::sse",
                "received message event before endpoint event; ignoring"
            );
            continue;
        }
        debug!(
            target: "squeezy::mcp::sse",
            event = ?frame.event,
            "ignoring control event while waiting for endpoint"
        );
    }
    Err(SseTransportError::MissingEndpointEvent)
}

/// Join a relative endpoint path against the SSE URL, or return the absolute
/// URL untouched. Falls back to a typed error on malformed input.
pub(crate) fn resolve_endpoint_url(
    sse_url: &str,
    advertised: &str,
) -> Result<String, SseTransportError> {
    if advertised.is_empty() {
        return Err(SseTransportError::InvalidUrl {
            url: advertised.to_string(),
            message: "advertised endpoint url is empty".to_string(),
        });
    }
    let base = Url::parse(sse_url).map_err(|err| SseTransportError::InvalidUrl {
        url: sse_url.to_string(),
        message: err.to_string(),
    })?;
    let joined = base
        .join(advertised)
        .map_err(|err| SseTransportError::InvalidUrl {
            url: advertised.to_string(),
            message: err.to_string(),
        })?;
    Ok(joined.to_string())
}

/// Stream wrapper over a reqwest byte stream that yields decoded SSE frames.
fn sse_frame_stream(response: Response) -> impl futures_util::Stream<Item = SseEvent> + Unpin {
    let byte_stream = response.bytes_stream();
    let decoder = SseDecoder::default();
    Box::pin(futures_util::stream::unfold(
        (byte_stream, decoder),
        |(mut byte_stream, mut decoder)| async move {
            loop {
                if let Some(frame) = decoder.pop() {
                    return Some((frame, (byte_stream, decoder)));
                }
                match byte_stream.next().await {
                    Some(Ok(chunk)) => {
                        if let Err(err) = decoder.feed(&chunk) {
                            warn!(
                                target: "squeezy::mcp::sse",
                                error = %err,
                                "discarding malformed sse data"
                            );
                        }
                    }
                    Some(Err(err)) => {
                        warn!(
                            target: "squeezy::mcp::sse",
                            error = %err,
                            "sse byte stream error"
                        );
                        return None;
                    }
                    None => {
                        // EOF — return any in-progress frame the server forgot
                        // to terminate with a blank line, then signal end.
                        return decoder
                            .finish()
                            .map(|frame| (frame, (byte_stream, decoder)));
                    }
                }
            }
        },
    ))
}

/// SSE line decoder. Parses `event:` and `data:` lines from a text/event-stream
/// per the HTML living standard: empty line dispatches the accumulated event,
/// `data:` lines are joined with newline, `event:` sets the name, `id:` and
/// `retry:` are preserved but otherwise unused. Comments (lines beginning with
/// `:`) are skipped. Multi-byte UTF-8 chunks split across reads are buffered
/// at line boundaries only.
#[derive(Debug, Default)]
pub(crate) struct SseDecoder {
    /// Bytes received since the last LF that did not yet form a complete line.
    line_buffer: Vec<u8>,
    /// In-progress frame; flushed on blank line.
    current: SseEvent,
    /// Frames that have been dispatched but not yet handed to the consumer.
    ready: std::collections::VecDeque<SseEvent>,
    /// True when the last byte we observed was `\r`. The next `\n` is then a
    /// `\r\n` pair we should absorb instead of producing a second blank line.
    pending_cr: bool,
}

impl SseDecoder {
    pub fn feed(&mut self, chunk: &[u8]) -> Result<(), SseTransportError> {
        for &byte in chunk {
            // Lines may end in `\n`, `\r\n`, or `\r`. We track a one-byte
            // lookahead via `pending_cr`: on `\r` we close the current line,
            // and a `\n` that follows immediately is absorbed (rather than
            // producing a second blank line).
            match byte {
                b'\n' => {
                    if self.pending_cr {
                        self.pending_cr = false;
                        continue;
                    }
                    self.consume_line()?;
                }
                b'\r' => {
                    self.pending_cr = true;
                    self.consume_line()?;
                }
                other => {
                    self.pending_cr = false;
                    self.line_buffer.push(other);
                }
            }
        }
        Ok(())
    }

    /// Yield any partial frame that the stream left without a terminating
    /// blank line. Returns `None` if no fields were accumulated.
    pub fn finish(&mut self) -> Option<SseEvent> {
        // If there's an unfinished line in the buffer, treat it as if it had
        // arrived with a newline so we don't drop a last `data:` line.
        if !self.line_buffer.is_empty() {
            // Ignore decoder errors here — we are tearing the stream down.
            let _ = self.consume_line();
        }
        if frame_is_dispatchable(&self.current) {
            Some(std::mem::take(&mut self.current))
        } else {
            None
        }
    }

    pub fn pop(&mut self) -> Option<SseEvent> {
        self.ready.pop_front()
    }

    fn consume_line(&mut self) -> Result<(), SseTransportError> {
        if self.line_buffer.is_empty() {
            // Blank line: dispatch the current event (if any).
            if frame_is_dispatchable(&self.current) {
                self.ready.push_back(std::mem::take(&mut self.current));
            } else {
                // Reset any partial scalar fields so a stray `id:` cannot leak
                // into the next frame's identity.
                self.current = SseEvent::default();
            }
            return Ok(());
        }
        // Lines beginning with `:` are comments per the SSE grammar.
        if self.line_buffer.first() == Some(&b':') {
            self.line_buffer.clear();
            return Ok(());
        }
        let line = std::str::from_utf8(&self.line_buffer).map_err(|_| {
            SseTransportError::InvalidLine(String::from_utf8_lossy(&self.line_buffer).into_owned())
        })?;

        let (field, value) = split_field(line);
        match field {
            "event" => {
                self.current.event = Some(value.to_string());
            }
            "data" => {
                // Per spec: each `data:` line is concatenated with a `\n`.
                match &mut self.current.data {
                    Some(existing) => {
                        existing.push('\n');
                        existing.push_str(value);
                    }
                    None => self.current.data = Some(value.to_string()),
                }
            }
            "id" => {
                if !value.contains('\u{0}') {
                    self.current.id = Some(value.to_string());
                }
            }
            "retry" => {
                if let Ok(retry_ms) = value.trim().parse::<u64>() {
                    self.current.retry = Some(retry_ms);
                }
            }
            other => {
                debug!(
                    target: "squeezy::mcp::sse",
                    field = %other,
                    "ignoring unknown sse field"
                );
            }
        }
        self.line_buffer.clear();
        Ok(())
    }
}

fn frame_is_dispatchable(frame: &SseEvent) -> bool {
    frame.event.is_some() || frame.data.is_some() || frame.id.is_some() || frame.retry.is_some()
}

/// Split a parsed SSE line into `(field, value)`. Per spec: a single optional
/// space after the colon is stripped, and a line with no colon is the full
/// field name with an empty value.
fn split_field(line: &str) -> (&str, &str) {
    match line.find(':') {
        Some(idx) => {
            let field = &line[..idx];
            let mut value = &line[idx + 1..];
            if let Some(stripped) = value.strip_prefix(' ') {
                value = stripped;
            }
            (field, value)
        }
        None => (line, ""),
    }
}

/// Build an `SseClientWorker` for the given server config.
pub fn build_sse_worker(
    sse_url: String,
    auth_header: Option<String>,
    custom_headers: HashMap<HeaderName, HeaderValue>,
) -> Result<SseClientWorker, SseTransportError> {
    let client = Client::builder().build().map_err(SseTransportError::Http)?;
    Ok(SseClientWorker {
        sse_url,
        client,
        auth_header,
        custom_headers,
    })
}

#[cfg(test)]
#[path = "sse_tests.rs"]
mod tests;
