use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_stream::try_stream;
use futures_util::StreamExt;
use squeezy_core::{CostSnapshot, ProviderTransportConfig, SqueezyError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

use super::{
    JITTER_FRACTION, RetryPolicy, apply_jitter, backoff, idle_timeout, jitter_sample,
    parse_retry_after, send_with_auth_retry, send_with_retry, with_stream_retry,
};
use crate::credentials::{ApiKeyFuture, ApiKeySource};
use crate::{LlmEvent, LlmStream};

#[test]
fn provider_requests_policy_inherits_transport_max_retries() {
    let transport = ProviderTransportConfig {
        request_max_retries: 7,
        stream_max_retries: 3,
        stream_idle_timeout_ms: 1_000,
        ..ProviderTransportConfig::default()
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
        ..ProviderTransportConfig::default()
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
        ..ProviderTransportConfig::default()
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
        max_retry_delay: Duration::from_secs(60),
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
        max_retry_delay: Duration::from_secs(60),
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
        max_retry_delay: Duration::from_secs(60),
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

#[test]
fn malicious_retry_after_header_is_clamped_to_policy_cap() {
    // A hostile upstream asking for ~11.5 days of cooldown must not
    // be able to park the agent: the default cap (60s) bounds the
    // chosen delay regardless of what the header claims.
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::RETRY_AFTER,
        "999999".parse().expect("header value"),
    );
    let parsed = parse_retry_after(&headers).expect("retry-after present");
    assert_eq!(
        parsed,
        Duration::from_secs(999_999),
        "parser must report the raw header value untouched",
    );

    let policy = RetryPolicy::provider_requests(ProviderTransportConfig::default());
    assert_eq!(
        policy.max_retry_delay,
        Duration::from_secs(60),
        "default policy must carry the 60s ceiling from ProviderTransportConfig",
    );
    assert_eq!(
        parsed.min(policy.max_retry_delay),
        Duration::from_secs(60),
        "malicious Retry-After: 999999 must clamp to 60s",
    );
}

#[test]
fn retry_policy_inherits_max_retry_delay_from_transport_config() {
    // Both the request and stream policies must carry the
    // operator-configured ceiling so callers cannot accidentally
    // bypass it by routing through a different policy constructor.
    let transport = ProviderTransportConfig {
        max_retry_delay_ms: 5_000,
        ..ProviderTransportConfig::default()
    };
    let request_policy = RetryPolicy::provider_requests(transport);
    let stream_policy = RetryPolicy::provider_stream(transport);
    assert_eq!(request_policy.max_retry_delay, Duration::from_millis(5_000));
    assert_eq!(stream_policy.max_retry_delay, Duration::from_millis(5_000));
}

/// Closes the protocol loop on the parse-level None tests above: when a
/// retryable response carries neither `Retry-After-Ms` nor `Retry-After`,
/// `send_with_retry` must fall through to the exponential `backoff` schedule
/// rather than reconnecting immediately. Measures elapsed wall-clock between
/// the first (429) and second (200) attempts and asserts it lands inside the
/// jittered backoff window for `attempt = 0`.
#[tokio::test]
async fn send_with_retry_uses_exponential_backoff_when_no_retry_after_headers() {
    let (addr, attempts) = spawn_status_server(vec![429, 200]).await;
    let client = reqwest::Client::new();
    let cancel = CancellationToken::new();
    let url = format!("http://{addr}");

    let base_delay = Duration::from_millis(120);
    let policy = RetryPolicy {
        max_retries: 1,
        base_delay,
        retry_429: true,
        retry_5xx: false,
        retry_transport: false,
        max_retry_delay: Duration::from_secs(60),
    };

    let started = Instant::now();
    let response = send_with_retry(policy, &cancel, || client.post(&url))
        .await
        .expect("send");
    let elapsed = started.elapsed();

    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        2,
        "429 then 200 = exactly two HTTP attempts"
    );

    // `backoff(base_delay, 0)` is in `[base * (1 - JITTER_FRACTION), base * (1
    // + JITTER_FRACTION)]`. The 5ms floor absorbs scheduler/timer imprecision
    // on contended CI runners; the assertion still fails loudly if a
    // regression skips the sleep entirely (elapsed would round to ~0).
    let lower_bound = base_delay.mul_f64(1.0 - JITTER_FRACTION) - Duration::from_millis(5);
    assert!(
        elapsed >= lower_bound,
        "without Retry-After headers send_with_retry must back off (elapsed = {elapsed:?}, lower bound = {lower_bound:?})",
    );
}

// --- send_with_auth_retry --------------------------------------------------

/// Test ApiKeySource that hands out a deterministic sequence of keys
/// and counts how many times the retry layer touched it. Drives the
/// `send_with_auth_retry` assertions below without spinning up the
/// full OAuth flow.
#[derive(Debug)]
struct TestKeySource {
    label: String,
    keys: AsyncMutex<Vec<String>>,
    current_key_calls: AtomicUsize,
    invalidate_calls: AtomicUsize,
}

impl TestKeySource {
    fn new(label: &str, keys: Vec<String>) -> Arc<Self> {
        Arc::new(Self {
            label: label.to_string(),
            keys: AsyncMutex::new(keys.into_iter().rev().collect()),
            current_key_calls: AtomicUsize::new(0),
            invalidate_calls: AtomicUsize::new(0),
        })
    }

    fn current_key_calls(&self) -> usize {
        self.current_key_calls.load(Ordering::SeqCst)
    }

    fn invalidate_calls(&self) -> usize {
        self.invalidate_calls.load(Ordering::SeqCst)
    }
}

impl ApiKeySource for TestKeySource {
    fn current_key<'a>(&'a self) -> ApiKeyFuture<'a, String> {
        Box::pin(async move {
            self.current_key_calls.fetch_add(1, Ordering::SeqCst);
            let mut keys = self.keys.lock().await;
            Ok(keys.pop().unwrap_or_else(|| "exhausted".to_string()))
        })
    }

    fn invalidate<'a>(&'a self) -> ApiKeyFuture<'a, ()> {
        Box::pin(async move {
            self.invalidate_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    fn provider_label(&self) -> &str {
        &self.label
    }
}

/// Spin a loopback TCP server that responds to each POST with a
/// fixed status sequence. Used to drive the auth-retry happy and
/// failure paths without depending on a live provider.
async fn spawn_status_server(statuses: Vec<u16>) -> (std::net::SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_clone = attempts.clone();
    tokio::spawn(async move {
        loop {
            let (mut stream, _peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => return,
            };
            let attempt = attempts_clone.fetch_add(1, Ordering::SeqCst);
            let status = statuses
                .get(attempt)
                .copied()
                .unwrap_or_else(|| *statuses.last().expect("non-empty status list"));

            let mut buf = [0u8; 4096];
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
            let reason = match status {
                200 => "OK",
                401 => "Unauthorized",
                403 => "Forbidden",
                500 => "Internal Server Error",
                _ => "Status",
            };
            let response = format!("HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\n\r\n");
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
    });
    (addr, attempts)
}

fn auth_retry_policy() -> RetryPolicy {
    // Disable the inner 429/5xx/transport retry budget so the test
    // observes the auth-retry layer in isolation: every 401 returned
    // by the mock server is forwarded straight to the auth path.
    RetryPolicy {
        max_retries: 0,
        base_delay: Duration::from_millis(1),
        retry_429: false,
        retry_5xx: false,
        retry_transport: true,
        max_retry_delay: Duration::from_secs(60),
    }
}

#[tokio::test]
async fn send_with_auth_retry_passes_through_on_2xx() {
    let (addr, attempts) = spawn_status_server(vec![200]).await;
    let source = TestKeySource::new("test", vec!["good-key".to_string()]);
    let source_dyn: Arc<dyn ApiKeySource> = source.clone();
    let client = reqwest::Client::new();
    let cancel = CancellationToken::new();
    let url = format!("http://{addr}");

    let response = send_with_auth_retry(&source_dyn, auth_retry_policy(), &cancel, |key| {
        client.post(&url).bearer_auth(key)
    })
    .await
    .expect("send");

    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "exactly one HTTP attempt"
    );
    assert_eq!(source.current_key_calls(), 1, "no refresh on 2xx");
    assert_eq!(
        source.invalidate_calls(),
        0,
        "invalidate must not fire on a healthy response"
    );
}

#[tokio::test]
async fn send_with_auth_retry_refreshes_once_on_401() {
    let (addr, attempts) = spawn_status_server(vec![401, 200]).await;
    let source = TestKeySource::new(
        "test",
        vec!["stale-key".to_string(), "fresh-key".to_string()],
    );
    let source_dyn: Arc<dyn ApiKeySource> = source.clone();
    let client = reqwest::Client::new();
    let cancel = CancellationToken::new();
    let url = format!("http://{addr}");

    let response = send_with_auth_retry(&source_dyn, auth_retry_policy(), &cancel, |key| {
        client.post(&url).bearer_auth(key)
    })
    .await
    .expect("send");

    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        2,
        "401 then 200 = exactly two HTTP attempts"
    );
    assert_eq!(
        source.current_key_calls(),
        2,
        "the auth-retry layer must re-read the key after invalidate"
    );
    assert_eq!(source.invalidate_calls(), 1, "invalidate fires once on 401");
}

#[tokio::test]
async fn send_with_auth_retry_refreshes_once_on_403() {
    let (addr, attempts) = spawn_status_server(vec![403, 200]).await;
    let source = TestKeySource::new(
        "test",
        vec!["stale-key".to_string(), "fresh-key".to_string()],
    );
    let source_dyn: Arc<dyn ApiKeySource> = source.clone();
    let client = reqwest::Client::new();
    let cancel = CancellationToken::new();
    let url = format!("http://{addr}");

    let response = send_with_auth_retry(&source_dyn, auth_retry_policy(), &cancel, |key| {
        client.post(&url).bearer_auth(key)
    })
    .await
    .expect("send");

    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(source.invalidate_calls(), 1);
}

#[tokio::test]
async fn send_with_auth_retry_bubbles_up_persistent_401() {
    // Second 401 means the refresh did not actually rotate the key
    // (or the upstream still rejects). Surface the final response
    // unchanged so the provider's existing error formatter reports
    // an honest auth failure instead of looping forever.
    let (addr, attempts) = spawn_status_server(vec![401, 401]).await;
    let source = TestKeySource::new(
        "test",
        vec!["stale-key".to_string(), "still-stale".to_string()],
    );
    let source_dyn: Arc<dyn ApiKeySource> = source.clone();
    let client = reqwest::Client::new();
    let cancel = CancellationToken::new();
    let url = format!("http://{addr}");

    let response = send_with_auth_retry(&source_dyn, auth_retry_policy(), &cancel, |key| {
        client.post(&url).bearer_auth(key)
    })
    .await
    .expect("send");

    assert_eq!(
        response.status().as_u16(),
        401,
        "the second 401 must propagate so the caller sees the auth failure"
    );
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        2,
        "exactly one retry; no infinite loop"
    );
    assert_eq!(
        source.invalidate_calls(),
        1,
        "invalidate fires exactly once even when the refresh did not help"
    );
}
