use std::cell::Cell;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::StatusCode;
use squeezy_core::{ProviderTransportConfig, Result, SqueezyError};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

/// Fractional jitter applied to each backoff delay so concurrent clients hitting
/// a shared throttle don't retry in lockstep. A +/-25% spread keeps the expected
/// delay close to the exponential schedule while spreading bursts wide enough
/// that two clients launched at the same instant land on different retry slots.
pub(crate) const JITTER_FRACTION: f64 = 0.25;

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

pub fn idle_timeout(config: ProviderTransportConfig) -> Duration {
    Duration::from_millis(config.stream_idle_timeout_ms)
}

pub(super) fn should_retry_status(policy: RetryPolicy, status: StatusCode) -> bool {
    policy.retry_429 && status == StatusCode::TOO_MANY_REQUESTS
        || policy.retry_5xx && status.is_server_error()
}

pub(crate) fn backoff(base: Duration, attempt: u8) -> Duration {
    let factor = 2u32.saturating_pow(u32::from(attempt));
    let scaled = base.saturating_mul(factor);
    apply_jitter(scaled, jitter_sample())
}

/// Exponential `backoff` clamped to `policy.max_retry_delay` so every
/// inter-retry sleep honors the documented ceiling. Routing all sleep
/// sites through this helper keeps a large `max_retries` from parking
/// the agent for hours on a flaky link, the guarantee `max_retry_delay`
/// is meant to provide.
pub(crate) fn capped_backoff(policy: RetryPolicy, attempt: u8) -> Duration {
    backoff(policy.base_delay, attempt).min(policy.max_retry_delay)
}

/// Scales `delay` by `(1 + sample * JITTER_FRACTION)` where `sample` is in
/// `[-1.0, 1.0]`, producing a value in `[delay * (1 - JITTER_FRACTION),
/// delay * (1 + JITTER_FRACTION)]`. Extracted so tests can drive the jitter
/// deterministically.
pub(crate) fn apply_jitter(delay: Duration, sample: f64) -> Duration {
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
pub(crate) fn jitter_sample() -> f64 {
    thread_local! {
        static STATE: Cell<u64> = const { Cell::new(0) };
    }
    STATE.with(|cell| {
        let mut state = cell.get();
        if state == 0 {
            state = seed();
        }
        // xorshift64* - small, fast, and good enough for jitter.
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        cell.set(state);
        let mantissa = (state >> 11) as f64;
        let unit = mantissa / ((1u64 << 53) as f64);
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

pub(super) async fn sleep_or_cancel(cancel: &CancellationToken, duration: Duration) -> Result<()> {
    tokio::select! {
        _ = cancel.cancelled() => Err(SqueezyError::ProviderStream("cancelled".to_string())),
        _ = sleep(duration) => Ok(()),
    }
}
