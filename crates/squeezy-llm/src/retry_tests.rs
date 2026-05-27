use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_stream::try_stream;
use futures_util::StreamExt;
use squeezy_core::{CostSnapshot, ProviderTransportConfig, SqueezyError};
use tokio_util::sync::CancellationToken;

use super::{
    JITTER_FRACTION, RetryPolicy, apply_jitter, backoff, idle_timeout, jitter_sample,
    parse_retry_after, with_stream_retry,
};
use crate::{LlmEvent, LlmStream};

#[test]
fn provider_requests_policy_inherits_transport_max_retries() {
    let transport = ProviderTransportConfig {
        request_max_retries: 7,
        stream_max_retries: 3,
        stream_idle_timeout_ms: 1_000,
    };
    let policy = RetryPolicy::provider_requests(transport);
    assert_eq!(policy.max_retries, 7);
    assert!(policy.retry_429);
    assert!(policy.retry_5xx);
    assert!(policy.retry_transport);
    assert_eq!(policy.base_delay, Duration::from_millis(200));
}

#[test]
fn provider_stream_policy_inherits_transport_stream_retries() {
    let transport = ProviderTransportConfig {
        request_max_retries: 1,
        stream_max_retries: 4,
        stream_idle_timeout_ms: 1_000,
    };
    let policy = RetryPolicy::provider_stream(transport);
    assert_eq!(policy.max_retries, 4);
    assert!(!policy.retry_429);
    assert!(!policy.retry_5xx);
    assert!(policy.retry_transport);
}

#[test]
fn idle_timeout_reflects_transport_setting() {
    let transport = ProviderTransportConfig {
        request_max_retries: 0,
        stream_max_retries: 0,
        stream_idle_timeout_ms: 250,
    };
    assert_eq!(idle_timeout(transport), Duration::from_millis(250));
}

/// Builds a mock provider stream that emits a deterministic prefix of
/// `events`, then either errors or completes depending on the attempt
/// number. This mimics a mid-stream transport failure on the first
/// `fail_attempts` attempts and a clean stream on attempt `fail_attempts + 1`.
fn mock_stream_attempt(events: Vec<LlmEvent>, fail_after: usize) -> LlmStream {
    let stream = try_stream! {
        for (emitted, event) in events.into_iter().enumerate() {
            if emitted >= fail_after {
                Err(SqueezyError::ProviderStream(
                    "mock transport: simulated mid-stream drop".to_string(),
                ))?;
                unreachable!("returned above");
            }
            yield event;
        }
    };
    Box::pin(stream)
}

fn mock_full_stream(events: Vec<LlmEvent>) -> LlmStream {
    let stream = try_stream! {
        for event in events {
            yield event;
        }
    };
    Box::pin(stream)
}

fn full_event_sequence() -> Vec<LlmEvent> {
    vec![
        LlmEvent::Started,
        LlmEvent::TextDelta("hello ".to_string()),
        LlmEvent::TextDelta("world".to_string()),
        LlmEvent::completed(Some("resp_1".to_string()), CostSnapshot::default()),
    ]
}

#[tokio::test]
async fn mock_transport_drops_mid_stream_for_two_attempts_then_succeeds_on_third() {
    let policy = RetryPolicy {
        max_retries: 3,
        base_delay: Duration::from_millis(1),
        retry_429: false,
        retry_5xx: false,
        retry_transport: true,
    };
    let cancel = CancellationToken::new();
    let attempts = Arc::new(Mutex::new(0u32));
    let factory_attempts = attempts.clone();

    let stream = with_stream_retry("mock", policy, cancel, move || {
        let mut guard = factory_attempts.lock().expect("lock");
        *guard += 1;
        let attempt = *guard;
        drop(guard);
        match attempt {
            1 => mock_stream_attempt(full_event_sequence(), 2),
            2 => mock_stream_attempt(full_event_sequence(), 3),
            _ => mock_full_stream(full_event_sequence()),
        }
    });

    let collected: Vec<LlmEvent> = stream
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<_, _>>()
        .expect("retry harness must yield a clean stream");

    let final_attempts = *attempts.lock().expect("lock");
    assert_eq!(final_attempts, 3, "exactly three attempts should be made");

    let mut text = String::new();
    let mut saw_started = 0;
    let mut saw_completed = 0;
    for event in &collected {
        match event {
            LlmEvent::Started => saw_started += 1,
            LlmEvent::TextDelta(delta) => text.push_str(delta),
            LlmEvent::Completed { .. } => saw_completed += 1,
            _ => {}
        }
    }
    assert_eq!(
        text, "hello world",
        "downstream text must contain no duplicate prefix"
    );
    assert_eq!(saw_started, 1, "Started must be emitted exactly once");
    assert_eq!(saw_completed, 1, "Completed must be emitted exactly once");
}

#[tokio::test]
async fn mock_transport_stops_retrying_once_stream_max_retries_is_exhausted() {
    let policy = RetryPolicy {
        max_retries: 1,
        base_delay: Duration::from_millis(1),
        retry_429: false,
        retry_5xx: false,
        retry_transport: true,
    };
    let cancel = CancellationToken::new();
    let attempts = Arc::new(Mutex::new(0u32));
    let factory_attempts = attempts.clone();

    let stream = with_stream_retry("mock", policy, cancel, move || {
        let mut guard = factory_attempts.lock().expect("lock");
        *guard += 1;
        drop(guard);
        mock_stream_attempt(full_event_sequence(), 2)
    });

    let collected = stream.collect::<Vec<_>>().await;
    let final_attempts = *attempts.lock().expect("lock");
    assert_eq!(
        final_attempts, 2,
        "with max_retries=1 the harness must try the initial attempt + 1 reconnect"
    );

    let last = collected
        .last()
        .expect("at least one yielded result")
        .as_ref();
    let err = last.expect_err("final yield must be the transient error");
    assert!(matches!(err, SqueezyError::ProviderStream(_)));
}

#[tokio::test]
async fn mock_transport_does_not_double_emit_when_reconnect_replays_prefix() {
    let policy = RetryPolicy {
        max_retries: 2,
        base_delay: Duration::from_millis(1),
        retry_429: false,
        retry_5xx: false,
        retry_transport: true,
    };
    let cancel = CancellationToken::new();
    let attempts = Arc::new(Mutex::new(0u32));
    let factory_attempts = attempts.clone();

    let stream = with_stream_retry("mock", policy, cancel, move || {
        let mut guard = factory_attempts.lock().expect("lock");
        *guard += 1;
        let attempt = *guard;
        drop(guard);
        if attempt == 1 {
            mock_stream_attempt(full_event_sequence(), 2)
        } else {
            mock_full_stream(full_event_sequence())
        }
    });

    let collected: Vec<LlmEvent> = stream
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<_, _>>()
        .expect("clean stream after one reconnect");

    let text: String = collected
        .iter()
        .filter_map(|event| match event {
            LlmEvent::TextDelta(delta) => Some(delta.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        text, "hello world",
        "skip-prefix must avoid double-emitting replayed tokens"
    );
}

#[test]
fn apply_jitter_stays_within_bounds() {
    let base = Duration::from_millis(1_000);
    let lower_bound_nanos = (base.as_nanos() as f64 * (1.0 - JITTER_FRACTION)).round() as u128;
    let upper_bound_nanos = (base.as_nanos() as f64 * (1.0 + JITTER_FRACTION)).round() as u128;

    for sample in [-1.0, -0.5, -0.1, 0.0, 0.1, 0.5, 1.0] {
        let jittered = apply_jitter(base, sample);
        assert!(
            jittered.as_nanos() >= lower_bound_nanos.saturating_sub(1),
            "sample {sample} produced {jittered:?} below lower bound"
        );
        assert!(
            jittered.as_nanos() <= upper_bound_nanos + 1,
            "sample {sample} produced {jittered:?} above upper bound"
        );
    }

    // Extreme samples must clamp to the documented ±25% window rather than
    // amplifying further.
    assert_eq!(apply_jitter(base, 5.0), apply_jitter(base, 1.0));
    assert_eq!(apply_jitter(base, -5.0), apply_jitter(base, -1.0));
}

#[test]
fn apply_jitter_extremes_match_window_endpoints() {
    let base = Duration::from_millis(1_600);
    let lo = apply_jitter(base, -1.0);
    let hi = apply_jitter(base, 1.0);
    assert_eq!(lo, Duration::from_millis(1_200));
    assert_eq!(hi, Duration::from_millis(2_000));
}

#[test]
fn backoff_grows_exponentially_within_jitter_window() {
    // For each attempt, the expected base delay is 200ms * 2^attempt. The
    // jittered result must land in ±JITTER_FRACTION of that base.
    let base = Duration::from_millis(200);
    for attempt in 0u8..5 {
        let expected_nanos = base.as_nanos() as u64 * (1u64 << u64::from(attempt));
        let lower = (expected_nanos as f64 * (1.0 - JITTER_FRACTION)) as u64;
        let upper = (expected_nanos as f64 * (1.0 + JITTER_FRACTION)) as u64;
        for _ in 0..32 {
            let observed = backoff(base, attempt).as_nanos() as u64;
            assert!(
                observed >= lower && observed <= upper,
                "attempt {attempt}: observed {observed}ns outside [{lower}, {upper}]"
            );
        }
    }
}

#[test]
fn backoff_produces_varying_delays_across_retries() {
    // With jitter enabled, repeated calls at the same attempt must not all
    // collapse to a single deterministic value. Drawing N samples and
    // requiring at least N/2 distinct values catches a regression that drops
    // jitter back to the deterministic 2^attempt schedule.
    let base = Duration::from_millis(200);
    const SAMPLES: usize = 16;

    for attempt in 0u8..4 {
        let delays: Vec<Duration> = (0..SAMPLES).map(|_| backoff(base, attempt)).collect();
        let distinct: std::collections::BTreeSet<_> = delays.iter().collect();
        assert!(
            distinct.len() >= SAMPLES / 2,
            "attempt {attempt}: expected jittered delays to vary, saw {} distinct in {SAMPLES} draws ({delays:?})",
            distinct.len(),
        );
    }
}

#[test]
fn n_retries_produce_n_distinct_delays() {
    // Walks a realistic retry sequence (attempts 0..N) and asserts every
    // observed delay is distinct. This is the property that prevents
    // multiple Squeezy clients from retrying in lockstep on a shared 429.
    let base = Duration::from_millis(200);
    const N: u8 = 6;
    let delays: Vec<Duration> = (0..N).map(|attempt| backoff(base, attempt)).collect();
    let distinct: std::collections::BTreeSet<_> = delays.iter().collect();
    assert_eq!(
        distinct.len(),
        usize::from(N),
        "expected {N} distinct retry delays, saw {} ({delays:?})",
        distinct.len(),
    );
}

#[test]
fn jitter_sample_stays_in_bounds() {
    for _ in 0..1024 {
        let sample = jitter_sample();
        assert!(
            (-1.0..=1.0).contains(&sample),
            "jitter sample {sample} outside [-1.0, 1.0]"
        );
    }
}

#[test]
fn jitter_sample_is_not_constant() {
    let first = jitter_sample();
    let mut saw_different = false;
    for _ in 0..64 {
        if (jitter_sample() - first).abs() > f64::EPSILON {
            saw_different = true;
            break;
        }
    }
    assert!(saw_different, "jitter sample appears deterministic");
}

#[test]
fn retry_after_ms_header_is_preferred_over_retry_after_seconds() {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("retry-after-ms", "250".parse().expect("header value"));
    headers.insert(
        reqwest::header::RETRY_AFTER,
        "1".parse().expect("header value"),
    );
    assert_eq!(
        parse_retry_after(&headers),
        Some(Duration::from_millis(250))
    );
}

#[test]
fn retry_after_seconds_used_when_ms_header_absent() {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::RETRY_AFTER,
        "2".parse().expect("header value"),
    );
    assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(2)));
}

#[test]
fn retry_after_falls_back_when_ms_header_is_unparseable() {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("retry-after-ms", "soon".parse().expect("header value"));
    headers.insert(
        reqwest::header::RETRY_AFTER,
        "3".parse().expect("header value"),
    );
    assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(3)));
}

#[test]
fn retry_after_returns_none_when_no_headers_present() {
    let headers = reqwest::header::HeaderMap::new();
    assert_eq!(parse_retry_after(&headers), None);
}
