use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_stream::try_stream;
use futures_util::StreamExt;
use squeezy_core::{CostSnapshot, ProviderTransportConfig, SqueezyError};
use tokio_util::sync::CancellationToken;

use super::{RetryPolicy, idle_timeout, with_stream_retry};
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
        LlmEvent::Completed {
            response_id: Some("resp_1".to_string()),
            cost: CostSnapshot::default(),
        },
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
