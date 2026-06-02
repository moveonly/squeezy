use std::cell::Cell;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_stream::try_stream;
use futures_util::StreamExt;
use reqwest::{RequestBuilder, Response, StatusCode};
use squeezy_core::{ProviderTransportConfig, Result, SqueezyError};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::credentials::ApiKeySource;
use crate::{LlmEvent, LlmStream};

/// Fractional jitter applied to each backoff delay so concurrent clients hitting
/// a shared throttle don't retry in lockstep. A ±25% spread keeps the expected
/// delay close to the exponential schedule while spreading bursts wide enough
/// that two clients launched at the same instant land on different retry slots.
const JITTER_FRACTION: f64 = 0.25;

#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_retries: u8,
    pub base_delay: Duration,
    pub retry_429: bool,
    pub retry_5xx: bool,
    pub retry_transport: bool,
    /// Hard ceiling on any single inter-retry sleep, including a
    /// `Retry-After` hint from the upstream. Defends against a
    /// hostile or buggy header pinning the agent for hours.
    pub max_retry_delay: Duration,
}

impl RetryPolicy {
    pub fn provider_requests(config: ProviderTransportConfig) -> Self {
        Self {
            max_retries: config.request_max_retries,
            base_delay: Duration::from_millis(200),
            retry_429: true,
            retry_5xx: true,
            retry_transport: true,
            max_retry_delay: Duration::from_millis(config.max_retry_delay_ms),
        }
    }

    pub fn provider_stream(config: ProviderTransportConfig) -> Self {
        Self {
            max_retries: config.stream_max_retries,
            base_delay: Duration::from_millis(200),
            retry_429: false,
            retry_5xx: false,
            retry_transport: true,
            max_retry_delay: Duration::from_millis(config.max_retry_delay_ms),
        }
    }
}

/// Run [`send_with_retry`] under an outer auth-refresh layer.
///
/// Resolves the API key from `source` once, dispatches the request
/// through the existing transport/throttle retry path, and — only if
/// the upstream comes back with `401`/`403` — calls
/// [`ApiKeySource::invalidate`] and retries the request a single
/// time with a freshly fetched key. A still-`401`/`403` on the
/// second attempt is returned to the caller for the provider's
/// existing error handler to surface.
///
/// The closure receives the resolved key as a `&str` so the provider
/// client can stamp it onto the request the same way it does today
/// (`x-api-key`, `bearer_auth`, `api-key` for Azure, etc.). Cloning
/// the key inside the closure is fine — it's a short-lived string.
///
/// Layering note: this sits *outside* `send_with_retry` because
/// 401/403 are not retryable on the same key. The existing policy
/// (`retry_429`, `retry_5xx`, `retry_transport`) keeps owning
/// transport-level recoveries; this helper just adds a one-shot
/// "token rotated, try again" pass so OAuth-backed sources can
/// refresh mid-session without bouncing the provider client.
pub async fn send_with_auth_retry<F>(
    source: &Arc<dyn ApiKeySource>,
    policy: RetryPolicy,
    cancel: &CancellationToken,
    mut make_request: F,
) -> Result<Response>
where
    F: FnMut(&str) -> RequestBuilder,
{
    let key = source.current_key().await?;
    let response = send_with_retry(policy, cancel, || make_request(&key)).await?;
    if !is_auth_failure(response.status()) {
        return Ok(response);
    }
    // Only sources that can actually rotate the credential without
    // operator intervention (OAuth, refreshable tokens) benefit from
    // the retry-on-401 dance. A `StaticApiKey` has nothing to rotate
    // to: `invalidate` just clears the only known value and the next
    // `current_key` call hands back the same already-rejected string
    // (or worse, an empty placeholder). Surface the original 401/403
    // response so the provider's existing error formatter renders an
    // honest "your key is bad" message instead of looping us into a
    // second guaranteed rejection.
    if !source.can_rotate() {
        tracing::debug!(
            target: "squeezy_llm::auth_retry",
            provider = source.provider_label(),
            status = response.status().as_u16(),
            "skipping auth retry: source cannot rotate credentials",
        );
        return Ok(response);
    }
    tracing::warn!(
        target: "squeezy_llm::auth_retry",
        provider = source.provider_label(),
        status = response.status().as_u16(),
        "upstream rejected api key; invalidating source and retrying once",
    );
    source.invalidate().await?;
    let refreshed = source.current_key().await?;
    send_with_retry(policy, cancel, || make_request(&refreshed)).await
}

fn is_auth_failure(status: StatusCode) -> bool {
    matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
}

pub async fn send_with_retry(
    policy: RetryPolicy,
    cancel: &CancellationToken,
    mut make_request: impl FnMut() -> RequestBuilder,
) -> Result<Response> {
    let mut attempt = 0u8;
    loop {
        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(SqueezyError::ProviderStream("cancelled".to_string())),
            response = make_request().send() => response,
        };
        match response {
            Ok(response)
                if should_retry_status(policy, response.status())
                    && attempt < policy.max_retries =>
            {
                // Inspect the response body before deciding to retry. A
                // hard-quota / billing error wears a retryable status code
                // (Anthropic returns `429` for "monthly usage limit reached"
                // and OpenAI returns `429` for `insufficient_quota`) but it
                // will not recover within any reasonable retry window — sleep
                // / retry just burns the agent's remaining attempts. Read the
                // body once, classify it, and either surface a reconstructed
                // response so the provider's existing error formatter sees the
                // status + body, or fall through to the retry path with a
                // tracing breadcrumb.
                let status = response.status();
                let headers = response.headers().clone();
                let retry_after = parse_retry_after(&headers);
                let body_bytes = response.bytes().await.unwrap_or_default();
                if is_terminal_quota_error(&body_bytes) {
                    tracing::warn!(
                        target: "squeezy_llm::retry",
                        status = status.as_u16(),
                        attempt,
                        "terminal quota error detected on retryable status; skipping retry",
                    );
                    return Ok(reconstruct_response(status, headers, body_bytes));
                }
                // Clamp the chosen delay to `policy.max_retry_delay`
                // so a malicious or buggy `Retry-After: 999999` from
                // the upstream cannot pin the agent for hours.
                let delay = retry_after
                    .unwrap_or_else(|| backoff(policy.base_delay, attempt))
                    .min(policy.max_retry_delay);
                sleep_or_cancel(cancel, delay).await?;
            }
            Ok(response) => return Ok(response),
            Err(error) if policy.retry_transport && attempt < policy.max_retries => {
                let _ = error;
                sleep_or_cancel(cancel, backoff(policy.base_delay, attempt)).await?;
            }
            Err(error) => return Err(SqueezyError::ProviderRequest(error.to_string())),
        }
        attempt = attempt.saturating_add(1);
    }
}

/// Rebuilds a `reqwest::Response` from the parts we already pulled off the
/// wire after the body inspection in [`send_with_retry`]. Used on the
/// terminal-quota path so callers can still read `.status()`, `.headers()`,
/// and `.text()` exactly as if the original response had not been consumed.
fn reconstruct_response<B>(
    status: StatusCode,
    headers: reqwest::header::HeaderMap,
    body: B,
) -> Response
where
    B: Into<reqwest::Body>,
{
    let mut http_response = http::Response::new(body);
    *http_response.status_mut() = status;
    *http_response.headers_mut() = headers;
    Response::from(http_response)
}

/// Returns `true` if `body` looks like a hard-quota / billing error that the
/// upstream will not retire on the retry timeline — typically a "monthly
/// usage limit reached" message from Anthropic or an `insufficient_quota`
/// error from OpenAI returned with a `429` status. Sleeping and retrying
/// those just burns the remaining attempt budget; the agent should surface
/// the failure to the user immediately.
///
/// Matches in two passes:
///
/// 1. A short list of well-known substrings (`monthly_usage_limit`,
///    `Monthly usage limit reached`, `insufficient_quota`,
///    `billing_hard_limit_reached`, `quota_exceeded`). This covers raw
///    text bodies and JSON bodies alike without paying a parser tax.
/// 2. A JSON shape check for the two providers whose error envelopes are
///    documented and stable: Anthropic (`error.type == "permission_error"`)
///    and OpenAI (`error.code == "insufficient_quota" |
///    "billing_hard_limit_reached" | "quota_exceeded"`). The substring pass
///    already catches the literal codes; the JSON pass is the durable
///    contract — provider-specific error shapes won't silently slip past
///    a future copywriting tweak.
///
/// Non-UTF-8 bodies are treated as non-terminal so the existing transient
/// retry path keeps running rather than skipping retries on an unrelated
/// binary garble.
pub(crate) fn is_terminal_quota_error(body: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(body) else {
        return false;
    };
    for keyword in TERMINAL_QUOTA_KEYWORDS {
        if text.contains(keyword) {
            return true;
        }
    }
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text)
        && has_terminal_provider_error_shape(&value)
    {
        return true;
    }
    false
}

const TERMINAL_QUOTA_KEYWORDS: &[&str] = &[
    "monthly_usage_limit",
    "Monthly usage limit reached",
    "insufficient_quota",
    "billing_hard_limit_reached",
    "quota_exceeded",
];

/// Recognizes the provider-specific terminal-error envelopes documented at
/// Anthropic and OpenAI. Each provider can extend this list as new
/// hard-quota codes appear without changing the substring fallback.
fn has_terminal_provider_error_shape(value: &serde_json::Value) -> bool {
    let Some(error) = value.get("error") else {
        return false;
    };
    if let Some(error_type) = error.get("type").and_then(|t| t.as_str())
        && error_type == "permission_error"
    {
        return true;
    }
    if let Some(error_code) = error.get("code").and_then(|c| c.as_str())
        && matches!(
            error_code,
            "insufficient_quota" | "billing_hard_limit_reached" | "quota_exceeded"
        )
    {
        return true;
    }
    false
}

pub fn idle_timeout(config: ProviderTransportConfig) -> Duration {
    Duration::from_millis(config.stream_idle_timeout_ms)
}

fn should_retry_status(policy: RetryPolicy, status: StatusCode) -> bool {
    policy.retry_429 && status == StatusCode::TOO_MANY_REQUESTS
        || policy.retry_5xx && status.is_server_error()
}

fn backoff(base: Duration, attempt: u8) -> Duration {
    let factor = 2u32.saturating_pow(u32::from(attempt));
    let scaled = base.saturating_mul(factor);
    apply_jitter(scaled, jitter_sample())
}

/// Scales `delay` by `(1 + sample * JITTER_FRACTION)` where `sample` is in
/// `[-1.0, 1.0]`, producing a value in `[delay * (1 - JITTER_FRACTION),
/// delay * (1 + JITTER_FRACTION)]`. Extracted so tests can drive the jitter
/// deterministically.
fn apply_jitter(delay: Duration, sample: f64) -> Duration {
    let clamped = sample.clamp(-1.0, 1.0);
    let multiplier = 1.0 + clamped * JITTER_FRACTION;
    let nanos = delay.as_nanos() as f64 * multiplier;
    if nanos <= 0.0 {
        Duration::ZERO
    } else {
        Duration::from_nanos(nanos as u64)
    }
}

/// Draws a jitter sample in `[-1.0, 1.0]` from a thread-local xorshift64* PRNG
/// seeded on first use from a wall-clock timestamp mixed with the thread's
/// stack address. Keeps `squeezy-llm` free of external rng dependencies.
fn jitter_sample() -> f64 {
    thread_local! {
        static STATE: Cell<u64> = const { Cell::new(0) };
    }
    STATE.with(|cell| {
        let mut state = cell.get();
        if state == 0 {
            state = seed();
        }
        // xorshift64* — small, fast, and good enough for jitter.
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        cell.set(state);
        let mantissa = (state >> 11) as f64; // 53-bit mantissa
        let unit = mantissa / ((1u64 << 53) as f64); // [0.0, 1.0)
        unit * 2.0 - 1.0
    })
}

fn seed() -> u64 {
    // Mix a wall-clock nanosecond count with the address of a stack-local
    // marker so each thread starts from a distinct seed even if the clock has
    // coarse resolution on the host. Never returns zero (xorshift fixed point).
    let marker: u8 = 0;
    let addr = std::ptr::from_ref(&marker) as usize as u64;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mixed = addr
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(nanos.wrapping_mul(0xBF58_476D_1CE4_E5B9))
        .wrapping_add(0xD1B5_4A32_D192_ED03);
    if mixed == 0 {
        0x9E37_79B9_7F4A_7C15
    } else {
        mixed
    }
}

/// Honors `Retry-After-Ms` (preferred, millisecond precision used by Anthropic
/// and some OpenAI Responses endpoints under sub-second throttling) before
/// falling back to the three shapes RFC 7231 allows for `Retry-After`:
///
/// 1. Integer seconds (`Retry-After: 30`) — the historical case.
/// 2. Fractional seconds (`Retry-After: 0.5`) — OpenAI sometimes returns
///    sub-second hints; without an f64 branch we'd round to zero and
///    hammer the upstream.
/// 3. An HTTP-date (`Retry-After: Wed, 21 Oct 2026 07:28:00 GMT`) —
///    proxy gateways and CDNs emit this when the upstream returns one.
///    Parsed via [`httpdate::parse_http_date`] against the current
///    wall clock; a date already in the past clamps to `Duration::ZERO`
///    so the caller still retries immediately. Wall-clock skew between
///    client and server can therefore mis-time the cooldown — that's
///    why the outer policy still clamps to `policy.max_retry_delay`.
///
/// Without the preference ladder we'd sleep a full second when the
/// server is telling us 250 ms is enough, or skip the cooldown entirely
/// on float / date headers and retry into the same throttle.
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    if let Some(value) = headers.get("retry-after-ms")
        && let Some(millis) = value.to_str().ok().and_then(|s| s.parse::<u64>().ok())
    {
        return Some(Duration::from_millis(millis));
    }
    let value = headers.get(reqwest::header::RETRY_AFTER)?;
    let text = value.to_str().ok()?.trim();
    if let Ok(seconds) = text.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    if let Ok(seconds) = text.parse::<f64>()
        && seconds.is_finite()
        && seconds >= 0.0
    {
        // Clamp to ms precision and saturate on absurd values so a buggy
        // header cannot overflow `Duration::from_millis`. The outer
        // `policy.max_retry_delay` cap still applies.
        let millis = (seconds * 1_000.0).clamp(0.0, u64::MAX as f64) as u64;
        return Some(Duration::from_millis(millis));
    }
    let target = httpdate::parse_http_date(text).ok()?;
    Some(target.duration_since(SystemTime::now()).unwrap_or_default())
}

async fn sleep_or_cancel(cancel: &CancellationToken, duration: Duration) -> Result<()> {
    tokio::select! {
        _ = cancel.cancelled() => Err(SqueezyError::ProviderStream("cancelled".to_string())),
        _ = sleep(duration) => Ok(()),
    }
}

/// Tracks already-emitted prefix of a provider stream so a restart attempt
/// can suppress duplicate events the caller has already observed.
#[derive(Debug, Default, Clone)]
pub struct StreamSkipState {
    /// Total characters of `TextDelta` emitted across attempts.
    emitted_text_chars: usize,
    /// Number of `ReasoningDelta` characters emitted across attempts.
    emitted_reasoning_chars: usize,
    /// Number of completed `ReasoningDone` events emitted.
    emitted_reasoning_done: usize,
    /// Number of completed `ToolCall` events emitted.
    emitted_tool_calls: usize,
    /// Whether `Started` has been emitted to the downstream consumer.
    started: bool,
    /// Whether a `ServerModel` event has already reached the
    /// downstream consumer on this stream. The event is at-most-once
    /// per turn — a mid-stream reconnect re-runs the provider's
    /// first-frame parsing and would otherwise yield the same echo
    /// again on attempt N+1. Suppress the duplicate so consumers
    /// (TUI, transcript writer) see one notification per turn.
    emitted_server_model: bool,
}

impl StreamSkipState {
    /// Update tracked counters for an event that just got yielded downstream.
    fn observe_yielded(&mut self, event: &LlmEvent) {
        match event {
            LlmEvent::Started => self.started = true,
            LlmEvent::TextDelta(text) => self.emitted_text_chars += text.chars().count(),
            LlmEvent::ReasoningDelta { text, .. } => {
                self.emitted_reasoning_chars += text.chars().count();
            }
            LlmEvent::ReasoningDone(_) => self.emitted_reasoning_done += 1,
            LlmEvent::ToolCall(_) => self.emitted_tool_calls += 1,
            LlmEvent::ServerModel(_) => self.emitted_server_model = true,
            LlmEvent::Completed { .. } | LlmEvent::Cancelled | LlmEvent::ContextOverflow { .. } => {
            }
            // `LlmEvent` is `#[non_exhaustive]`; additive variants
            // (incremental tool-args, refusal/citation deltas) don't
            // change retry skip accounting until the canonical
            // materialized event lands, so we no-op here.
            _ => {}
        }
    }
}

/// Per-attempt cursor that counts events coming from a freshly-restarted
/// provider stream and decides what should be passed through to the caller.
#[derive(Debug, Default)]
struct SkipCursor {
    seen_text_chars: usize,
    seen_reasoning_chars: usize,
    seen_reasoning_done: usize,
    seen_tool_calls: usize,
}

impl SkipCursor {
    /// Returns `Some(event)` to pass through, or `None` to suppress because
    /// the event re-covers ground a previous attempt already streamed.
    fn filter(&mut self, event: LlmEvent, skip: &StreamSkipState) -> Option<LlmEvent> {
        match event {
            LlmEvent::Started => {
                if skip.started {
                    None
                } else {
                    Some(LlmEvent::Started)
                }
            }
            LlmEvent::TextDelta(text) => {
                let already = skip.emitted_text_chars.saturating_sub(self.seen_text_chars);
                let (seen, forwarded) = skip_delta_prefix(text, already);
                self.seen_text_chars += seen;
                forwarded.map(LlmEvent::TextDelta)
            }
            LlmEvent::ReasoningDelta { text, kind } => {
                let already = skip
                    .emitted_reasoning_chars
                    .saturating_sub(self.seen_reasoning_chars);
                let (seen, forwarded) = skip_delta_prefix(text, already);
                self.seen_reasoning_chars += seen;
                forwarded.map(|text| LlmEvent::ReasoningDelta { text, kind })
            }
            LlmEvent::ReasoningDone(payload) => {
                self.seen_reasoning_done += 1;
                if self.seen_reasoning_done <= skip.emitted_reasoning_done {
                    None
                } else {
                    Some(LlmEvent::ReasoningDone(payload))
                }
            }
            LlmEvent::ToolCall(call) => {
                self.seen_tool_calls += 1;
                if self.seen_tool_calls <= skip.emitted_tool_calls {
                    None
                } else {
                    Some(LlmEvent::ToolCall(call))
                }
            }
            LlmEvent::Completed {
                response_id,
                cost,
                stop_reason,
                reasoning_only_stop,
            } => Some(LlmEvent::Completed {
                response_id,
                cost,
                stop_reason,
                reasoning_only_stop,
            }),
            LlmEvent::Cancelled => Some(LlmEvent::Cancelled),
            LlmEvent::ContextOverflow { provider, signal } => {
                Some(LlmEvent::ContextOverflow { provider, signal })
            }
            LlmEvent::ServerModel(model) => {
                // Suppress duplicates across attempts: the provider's
                // first-frame parser re-derives the echo on every
                // reconnect, but downstream consumers should only see
                // it once per turn.
                if skip.emitted_server_model {
                    None
                } else {
                    Some(LlmEvent::ServerModel(model))
                }
            }
            // `LlmEvent` is `#[non_exhaustive]`; additive variants
            // (incremental tool-args, refusal/citation deltas) pass
            // through unchanged because the retry layer doesn't track
            // them yet. Downstream consumers wildcard-skip on the same
            // grounds.
            other => Some(other),
        }
    }
}

fn skip_delta_prefix(text: String, skip_chars: usize) -> (usize, Option<String>) {
    if text.is_empty() {
        return (0, None);
    }
    if skip_chars == 0 {
        return (text.chars().count(), Some(text));
    }

    let mut total_chars = 0usize;
    let mut split_at = None;
    for (byte_index, _) in text.char_indices() {
        if total_chars == skip_chars {
            split_at = Some(byte_index);
        }
        total_chars += 1;
    }
    if skip_chars >= total_chars {
        return (total_chars, None);
    }

    let mut text = text;
    let suffix = text.split_off(split_at.expect("split point exists when skip_chars < chars"));
    (total_chars, Some(suffix))
}

/// Wraps a stream-producing closure so transient mid-stream errors trigger a
/// reconnect bounded by `policy.max_retries`. Already-yielded events are
/// tracked via [`StreamSkipState`] so a fresh attempt only emits the suffix
/// the caller has not yet observed. A `tracing` event is recorded on every
/// reconnect under `target = "squeezy_llm::stream_retry"` carrying
/// `provider` and `attempt` fields.
pub fn with_stream_retry<F>(
    provider: &'static str,
    policy: RetryPolicy,
    cancel: CancellationToken,
    mut make_attempt: F,
) -> LlmStream
where
    F: FnMut() -> LlmStream + Send + 'static,
{
    let stream = try_stream! {
        let mut skip = StreamSkipState::default();
        let mut attempt: u8 = 0;
        loop {
            let mut cursor = SkipCursor::default();
            let mut inner = make_attempt();
            let mut transient_error: Option<SqueezyError> = None;
            let mut completed = false;
            'inner: loop {
                let next = tokio::select! {
                    _ = cancel.cancelled() => {
                        yield LlmEvent::Cancelled;
                        return;
                    }
                    next = inner.next() => next,
                };
                match next {
                    None => break 'inner,
                    Some(Ok(event)) => {
                        let was_completed = matches!(event, LlmEvent::Completed { .. });
                        if let Some(forwarded) = cursor.filter(event, &skip) {
                            skip.observe_yielded(&forwarded);
                            yield forwarded;
                        }
                        if was_completed {
                            completed = true;
                            break 'inner;
                        }
                    }
                    Some(Err(err)) => {
                        if is_retryable_stream_error(&err) {
                            transient_error = Some(err);
                            break 'inner;
                        }
                        Err(err)?;
                        unreachable!("stream error returned above");
                    }
                }
            }

            if completed {
                return;
            }

            let Some(err) = transient_error else {
                if attempt >= policy.max_retries {
                    Err(SqueezyError::ProviderStream(
                        "provider stream ended without completion".to_string(),
                    ))?;
                    unreachable!("returned above");
                }
                attempt += 1;
                tracing::warn!(
                    target: "squeezy_llm::stream_retry",
                    provider,
                    attempt,
                    max = policy.max_retries,
                    "provider stream truncated; reconnecting",
                );
                sleep_or_cancel(&cancel, backoff(policy.base_delay, attempt - 1)).await?;
                continue;
            };

            if attempt >= policy.max_retries {
                Err(err)?;
                unreachable!("returned above");
            }
            attempt += 1;
            tracing::warn!(
                target: "squeezy_llm::stream_retry",
                provider,
                attempt,
                max = policy.max_retries,
                error = %err,
                "provider stream error; reconnecting",
            );
            sleep_or_cancel(&cancel, backoff(policy.base_delay, attempt - 1)).await?;
        }
    };
    Box::pin(stream)
}

/// Decides whether `with_stream_retry` should reconnect on `err`.
///
/// Provider stream/request errors are the only transport-level shapes
/// the harness knows how to replay, but Anthropic's error normaliser
/// (and any future provider that opts into the same contract) prefixes
/// terminal errors with [`crate::anthropic_error::NON_RETRYABLE_MARKER`]
/// — typically `invalid_request_error`, `authentication_error`, or
/// `permission_error` responses where retrying the identical request
/// just burns the rest of the attempt budget on a guaranteed failure.
///
/// Strip-and-check the marker before classifying so a marked
/// `ProviderRequest`/`ProviderStream` returns `false` and short-circuits
/// the reconnect loop straight to the caller.
pub(crate) fn is_retryable_stream_error(err: &SqueezyError) -> bool {
    let message = match err {
        SqueezyError::ProviderStream(msg) | SqueezyError::ProviderRequest(msg) => msg.as_str(),
        _ => return false,
    };
    !message.starts_with(crate::anthropic_error::NON_RETRYABLE_MARKER)
}

#[cfg(test)]
#[path = "retry_tests.rs"]
mod tests;
