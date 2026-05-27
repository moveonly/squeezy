use std::cell::Cell;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_stream::try_stream;
use futures_util::StreamExt;
use reqwest::{RequestBuilder, Response, StatusCode};
use squeezy_core::{ProviderTransportConfig, Result, SqueezyError};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

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
}

impl RetryPolicy {
    pub fn provider_requests(config: ProviderTransportConfig) -> Self {
        Self {
            max_retries: config.request_max_retries,
            base_delay: Duration::from_millis(200),
            retry_429: true,
            retry_5xx: true,
            retry_transport: true,
        }
    }

    pub fn provider_stream(config: ProviderTransportConfig) -> Self {
        Self {
            max_retries: config.stream_max_retries,
            base_delay: Duration::from_millis(200),
            retry_429: false,
            retry_5xx: false,
            retry_transport: true,
        }
    }
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
                let retry_after = retry_after_delay(&response).await;
                sleep_or_cancel(
                    cancel,
                    retry_after.unwrap_or_else(|| backoff(policy.base_delay, attempt)),
                )
                .await?;
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

async fn retry_after_delay(response: &Response) -> Option<Duration> {
    parse_retry_after(response.headers())
}

/// Honors `Retry-After-Ms` (preferred, millisecond precision used by Anthropic
/// and some OpenAI Responses endpoints under sub-second throttling) before
/// falling back to the seconds-granularity `Retry-After`. Without the
/// preference we'd sleep a full second when the server is telling us 250 ms
/// is enough.
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    if let Some(value) = headers.get("retry-after-ms")
        && let Some(millis) = value.to_str().ok().and_then(|s| s.parse::<u64>().ok())
    {
        return Some(Duration::from_millis(millis));
    }
    let value = headers.get(reqwest::header::RETRY_AFTER)?;
    let seconds = value.to_str().ok()?.parse::<u64>().ok()?;
    Some(Duration::from_secs(seconds))
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
            LlmEvent::Completed { .. } | LlmEvent::Cancelled => {}
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
                let chars: Vec<char> = text.chars().collect();
                let already = skip.emitted_text_chars.saturating_sub(self.seen_text_chars);
                self.seen_text_chars += chars.len();
                if already >= chars.len() {
                    None
                } else {
                    let suffix: String = chars[already..].iter().collect();
                    Some(LlmEvent::TextDelta(suffix))
                }
            }
            LlmEvent::ReasoningDelta { text, kind } => {
                let chars: Vec<char> = text.chars().collect();
                let already = skip
                    .emitted_reasoning_chars
                    .saturating_sub(self.seen_reasoning_chars);
                self.seen_reasoning_chars += chars.len();
                if already >= chars.len() {
                    None
                } else {
                    let suffix: String = chars[already..].iter().collect();
                    Some(LlmEvent::ReasoningDelta { text: suffix, kind })
                }
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
            } => Some(LlmEvent::Completed {
                response_id,
                cost,
                stop_reason,
            }),
            LlmEvent::Cancelled => Some(LlmEvent::Cancelled),
        }
    }
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

fn is_retryable_stream_error(err: &SqueezyError) -> bool {
    matches!(
        err,
        SqueezyError::ProviderStream(_) | SqueezyError::ProviderRequest(_)
    )
}

#[cfg(test)]
#[path = "retry_tests.rs"]
mod tests;
