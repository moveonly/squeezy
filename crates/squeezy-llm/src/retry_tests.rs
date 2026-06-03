use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

use async_stream::try_stream;
use futures_util::StreamExt;
use squeezy_core::{CostSnapshot, ProviderTransportConfig, SqueezyError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

use super::{
    JITTER_FRACTION, RetryPolicy, apply_jitter, backoff, capped_backoff, idle_timeout,
    is_terminal_quota_error, jitter_sample, parse_retry_after, send_with_auth_retry,
    send_with_retry, split_delta_prefix, with_stream_retry,
};
use crate::credentials::{ApiKeyFuture, ApiKeySource};
use crate::{LlmEvent, LlmStream, LlmToolCall};

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

#[test]
fn split_delta_prefix_forwards_whole_delta_when_nothing_to_skip() {
    let (consumed, forwarded) = split_delta_prefix("hello", 0);
    assert_eq!(consumed, 0);
    assert_eq!(forwarded, Some("hello"));
}

#[test]
fn split_delta_prefix_splits_on_char_boundary() {
    let (consumed, forwarded) = split_delta_prefix("héllo", 1);
    assert_eq!(consumed, 1);
    assert_eq!(forwarded, Some("éllo"));

    let (consumed, forwarded) = split_delta_prefix("héllo", 5);
    assert_eq!(consumed, 5);
    assert!(forwarded.is_none());

    // Skipping past the end reports only the chars actually present so the
    // caller can tell the regenerated delta was shorter than the prefix.
    let (consumed, forwarded) = split_delta_prefix("hi", 5);
    assert_eq!(consumed, 2);
    assert!(forwarded.is_none());
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

/// X-18: a mid-stream error already classified as terminal by the
/// Anthropic error normaliser (or any provider that opts into the
/// `[non-retryable]` marker contract) must short-circuit the reconnect
/// loop. The factory must run exactly once and the marked error must
/// bubble up untouched so the upstream prose reaches the caller.
#[tokio::test]
async fn non_retryable_marker_short_circuits_stream_retry() {
    use crate::anthropic_error::NON_RETRYABLE_MARKER;

    let policy = RetryPolicy {
        max_retries: 5,
        base_delay: Duration::from_millis(1),
        retry_429: false,
        retry_5xx: false,
        retry_transport: true,
        max_retry_delay: Duration::from_secs(60),
    };
    let cancel = CancellationToken::new();
    let attempts = Arc::new(Mutex::new(0u32));
    let factory_attempts = attempts.clone();
    let marked = format!(
        "{NON_RETRYABLE_MARKER}Anthropic rejected request (invalid_request_error): bad input",
    );
    let marked_for_factory = marked.clone();

    let stream = with_stream_retry("mock", policy, cancel, move || {
        *factory_attempts.lock().expect("lock") += 1;
        let payload = marked_for_factory.clone();
        let inner = try_stream! {
            yield LlmEvent::Started;
            Err::<LlmEvent, SqueezyError>(SqueezyError::ProviderStream(payload))?;
            unreachable!("error returned above");
        };
        Box::pin(inner) as LlmStream
    });

    let collected = stream.collect::<Vec<_>>().await;
    let final_attempts = *attempts.lock().expect("lock");
    assert_eq!(
        final_attempts, 1,
        "non-retryable marker must skip the reconnect loop entirely",
    );

    let last = collected
        .last()
        .expect("at least one yielded result")
        .as_ref();
    let err = last.expect_err("final yield must be the terminal error");
    match err {
        SqueezyError::ProviderStream(message) => {
            assert!(
                message.starts_with(NON_RETRYABLE_MARKER),
                "marked message must propagate verbatim to the caller (saw: {message:?})",
            );
            assert!(
                message.contains("invalid_request_error"),
                "upstream prose must survive the short-circuit",
            );
        }
        other => panic!("expected ProviderStream, saw {other:?}"),
    }
}

/// Sanity-check the marker contract on `ProviderRequest` errors too:
/// providers like Anthropic format their pre-stream HTTP errors with
/// the same prefix, so the stream-retry classifier must respect it for
/// both variants.
#[tokio::test]
async fn non_retryable_marker_short_circuits_on_provider_request_variant() {
    use crate::anthropic_error::NON_RETRYABLE_MARKER;

    let policy = RetryPolicy {
        max_retries: 5,
        base_delay: Duration::from_millis(1),
        retry_429: false,
        retry_5xx: false,
        retry_transport: true,
        max_retry_delay: Duration::from_secs(60),
    };
    let cancel = CancellationToken::new();
    let attempts = Arc::new(Mutex::new(0u32));
    let factory_attempts = attempts.clone();
    let marked = format!(
        "{NON_RETRYABLE_MARKER}Anthropic rejected request (authentication_error): invalid key",
    );
    let marked_for_factory = marked.clone();

    let stream = with_stream_retry("mock", policy, cancel, move || {
        *factory_attempts.lock().expect("lock") += 1;
        let payload = marked_for_factory.clone();
        let inner = try_stream! {
            if false {
                yield LlmEvent::Started;
            }
            Err(SqueezyError::ProviderRequest(payload))?;
            unreachable!("error returned above");
        };
        Box::pin(inner) as LlmStream
    });

    let collected = stream.collect::<Vec<_>>().await;
    let final_attempts = *attempts.lock().expect("lock");
    assert_eq!(
        final_attempts, 1,
        "marked ProviderRequest must also skip the reconnect loop",
    );

    let err = collected
        .last()
        .expect("at least one yielded result")
        .as_ref()
        .expect_err("final yield must be the terminal error");
    assert!(matches!(err, SqueezyError::ProviderRequest(_)));
}

/// Plain (unmarked) `ProviderStream` errors must still retry so the
/// fix does not widen the non-retryable window beyond the marker
/// contract. Pairs with the two short-circuit tests above.
#[tokio::test]
async fn unmarked_provider_stream_error_still_retries() {
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
        *factory_attempts.lock().expect("lock") += 1;
        let inner = try_stream! {
            if false {
                yield LlmEvent::Started;
            }
            Err(SqueezyError::ProviderStream(
                "connection reset".to_string(),
            ))?;
            unreachable!("error returned above");
        };
        Box::pin(inner) as LlmStream
    });

    let _ = stream.collect::<Vec<_>>().await;
    let final_attempts = *attempts.lock().expect("lock");
    assert_eq!(
        final_attempts, 2,
        "unmarked transient errors must still trigger the reconnect path",
    );
}

/// A reconnect that diverges *before any visible output is committed* — the
/// stream drops mid-reasoning and the regenerated reasoning differs — must
/// recover by restarting the stream from scratch rather than failing the whole
/// turn. Before this fix a flaky transport that reset early sampled a divergent
/// continuation on every reconnect and surfaced "stream reconnect diverged",
/// turning the turn into a $0 failure; that is the dominant variance/$0 source
/// under load.
#[tokio::test]
async fn early_reconnect_divergence_restarts_instead_of_failing() {
    let policy = RetryPolicy {
        max_retries: 5,
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
        let which = {
            let mut n = factory_attempts.lock().expect("lock");
            *n += 1;
            *n
        };
        let inner = try_stream! {
            match which {
                1 => {
                    // Drops mid-reasoning, nothing visible committed.
                    yield LlmEvent::Started;
                    yield LlmEvent::ReasoningDelta {
                        text: "Thinking variant A".to_string(),
                        kind: crate::ReasoningKind::Text,
                    };
                    Err(SqueezyError::ProviderStream("connection reset".to_string()))?;
                }
                2 => {
                    // Reconnect samples *different* reasoning -> divergence.
                    yield LlmEvent::Started;
                    yield LlmEvent::ReasoningDelta {
                        text: "Completely different B".to_string(),
                        kind: crate::ReasoningKind::Text,
                    };
                    Err(SqueezyError::ProviderStream("connection reset".to_string()))?;
                }
                _ => {
                    // Clean restart finally lands a full response.
                    yield LlmEvent::Started;
                    yield LlmEvent::ReasoningDelta {
                        text: "Final reasoning C".to_string(),
                        kind: crate::ReasoningKind::Text,
                    };
                    yield LlmEvent::TextDelta("the answer".to_string());
                    yield LlmEvent::completed(Some("resp".to_string()), CostSnapshot::default());
                }
            }
        };
        Box::pin(inner) as LlmStream
    });

    let collected: Vec<LlmEvent> = stream
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<_, _>>()
        .expect("early divergence must recover via a clean restart, not surface an error");

    let mut text = String::new();
    let mut completed = 0;
    let mut started = 0;
    for event in &collected {
        match event {
            LlmEvent::TextDelta(delta) => text.push_str(delta),
            LlmEvent::Completed { .. } => completed += 1,
            LlmEvent::Started => started += 1,
            _ => {}
        }
    }
    assert_eq!(
        text, "the answer",
        "the successful restart's text must reach the caller"
    );
    assert_eq!(completed, 1, "exactly one completion reaches the caller");
    assert_eq!(
        started, 1,
        "Started is a one-shot latch even across restarts"
    );
    assert_eq!(
        *attempts.lock().expect("lock"),
        3,
        "two drops then a clean third attempt"
    );
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

/// Regression: a *single* clean attempt (no reconnect) that emits several
/// consecutive fresh text/reasoning deltas must forward every one of them.
///
/// `SkipCursor::filter` advances its per-attempt `seen` cursor while
/// `StreamSkipState::observe_yielded` grows the recorded `emitted_*`
/// prefix as each delta is forwarded. If the cursor only advanced by the
/// re-validated portion (and not the freshly-forwarded suffix), it would
/// fall behind the recorded prefix: the *next* fresh delta of the same
/// attempt would then be re-validated against the text the *previous*
/// delta just emitted and spuriously fail with "stream reconnect
/// diverged" — on the very first attempt, with no reconnect in sight.
/// This reproduced as an Anthropic Haiku stream failing after one
/// reasoning token with cost $0.0000.
#[tokio::test]
async fn first_attempt_forwards_consecutive_fresh_deltas_without_divergence() {
    let policy = RetryPolicy {
        max_retries: 2,
        base_delay: Duration::from_millis(1),
        retry_429: false,
        retry_5xx: false,
        retry_transport: true,
        max_retry_delay: Duration::from_secs(60),
    };
    let cancel = CancellationToken::new();

    let stream = with_stream_retry("mock", policy, cancel, move || {
        mock_full_stream(vec![
            LlmEvent::Started,
            LlmEvent::ReasoningDelta {
                text: "The user wants me to ".to_string(),
                kind: crate::ReasoningKind::Text,
            },
            LlmEvent::ReasoningDelta {
                text: "analyze the Session class.".to_string(),
                kind: crate::ReasoningKind::Text,
            },
            LlmEvent::TextDelta("hello ".to_string()),
            LlmEvent::TextDelta("world".to_string()),
            LlmEvent::completed(Some("resp_1".to_string()), CostSnapshot::default()),
        ])
    });

    let collected: Vec<LlmEvent> = stream
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<_, _>>()
        .expect("a single clean attempt must never report divergence");

    let reasoning: String = collected
        .iter()
        .filter_map(|event| match event {
            LlmEvent::ReasoningDelta { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    let text: String = collected
        .iter()
        .filter_map(|event| match event {
            LlmEvent::TextDelta(delta) => Some(delta.clone()),
            _ => None,
        })
        .collect();

    assert_eq!(
        reasoning, "The user wants me to analyze the Session class.",
        "every fresh reasoning delta of a single attempt must be forwarded intact",
    );
    assert_eq!(
        text, "hello world",
        "every fresh text delta of a single attempt must be forwarded intact",
    );
    assert!(
        collected
            .iter()
            .any(|event| matches!(event, LlmEvent::Completed { .. })),
        "the clean attempt must complete",
    );
}

/// F1: the retry layer forwards additive `#[non_exhaustive]` variants
/// it does not track (here `Refusal`) verbatim through the `SkipCursor`
/// wildcard arm. This documents the current contract — these variants
/// pass through unchanged — for variants whose prefix a reconnect would
/// not replay.
#[tokio::test]
async fn untracked_additive_variant_passes_through_unchanged() {
    let policy = RetryPolicy {
        max_retries: 0,
        base_delay: Duration::from_millis(1),
        retry_429: false,
        retry_5xx: false,
        retry_transport: true,
        max_retry_delay: Duration::from_secs(60),
    };
    let cancel = CancellationToken::new();

    let stream = with_stream_retry("mock", policy, cancel, move || {
        mock_full_stream(vec![
            LlmEvent::Started,
            LlmEvent::Refusal {
                content: "I can't help with that.".to_string(),
            },
            LlmEvent::completed(Some("resp_1".to_string()), CostSnapshot::default()),
        ])
    });

    let collected: Vec<LlmEvent> = stream
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<_, _>>()
        .expect("clean stream");

    let saw_refusal = collected
        .iter()
        .any(|event| matches!(event, LlmEvent::Refusal { content } if content == "I can't help with that."));
    assert!(
        saw_refusal,
        "untracked additive variant must pass through unchanged"
    );
}

/// F1: `ToolCallDelta` carries an incremental prefix that a mid-stream
/// reconnect would replay, and `SkipCursor` has no per-`call_id`
/// accounting for it yet. The wildcard arm therefore trips a
/// `debug_assert` to gate any future adoption of `with_stream_retry` on
/// a `ToolCallDelta`-emitting provider. Lock that gate in: routing a
/// `ToolCallDelta` through the retry layer must panic in a debug/test
/// build rather than silently risk a double-emit.
#[cfg(debug_assertions)]
#[tokio::test]
#[should_panic(expected = "ToolCallDelta")]
async fn tool_call_delta_trips_skip_accounting_gate() {
    let policy = RetryPolicy {
        max_retries: 0,
        base_delay: Duration::from_millis(1),
        retry_429: false,
        retry_5xx: false,
        retry_transport: true,
        max_retry_delay: Duration::from_secs(60),
    };
    let cancel = CancellationToken::new();

    let stream = with_stream_retry("mock", policy, cancel, move || {
        mock_full_stream(vec![
            LlmEvent::Started,
            LlmEvent::ToolCallDelta {
                call_id: "call_1".to_string(),
                name: "search".to_string(),
                arguments_chunk: "{\"q\":".to_string(),
            },
        ])
    });

    let _ = stream.collect::<Vec<_>>().await;
}

fn tool_call(call_id: &str) -> LlmToolCall {
    LlmToolCall {
        call_id: call_id.to_string(),
        name: "do_thing".to_string(),
        arguments: serde_json::json!({}),
    }
}

#[tokio::test]
async fn mock_transport_surfaces_error_when_reconnect_diverges_from_emitted_prefix() {
    // Attempt 1 streams a long text prefix and tool call `A`, then drops.
    // Attempt 2 is an independent sample: shorter text and a different tool
    // call `B`. The count/positional skip would silently splice attempt 1's
    // prefix + `A` onto attempt 2's `Completed` (a phantom tool call the final
    // generation never asked for). The harness must instead surface a stream
    // error rather than corrupt the turn.
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
            mock_stream_attempt(
                vec![
                    LlmEvent::Started,
                    LlmEvent::TextDelta("Let me look that up for you ".to_string()),
                    LlmEvent::ToolCall(tool_call("call_A")),
                ],
                3, // drop after the tool call, before any Completed
            )
        } else {
            mock_full_stream(vec![
                LlmEvent::Started,
                LlmEvent::TextDelta("Sure!".to_string()),
                LlmEvent::ToolCall(tool_call("call_B")),
                LlmEvent::completed(Some("resp_2".to_string()), CostSnapshot::default()),
            ])
        }
    });

    let collected = stream.collect::<Vec<_>>().await;

    // The divergence must be reported as an error, not silently swallowed.
    let err = collected
        .last()
        .expect("at least one yielded result")
        .as_ref()
        .expect_err("a diverging reconnect must surface as a stream error");
    assert!(
        matches!(err, SqueezyError::ProviderStream(_)),
        "divergence must be a ProviderStream error, saw {err:?}",
    );

    // The corrupted turn must never reach the consumer: no Completed, and the
    // mismatched attempt-2 tool call `call_B` must not be spliced behind the
    // already-emitted `call_A`.
    let yielded: Vec<&LlmEvent> = collected.iter().filter_map(|r| r.as_ref().ok()).collect();
    assert!(
        !yielded
            .iter()
            .any(|event| matches!(event, LlmEvent::Completed { .. })),
        "a spliced Completed must not be forwarded",
    );
    assert!(
        !yielded.iter().any(|event| matches!(
            event,
            LlmEvent::ToolCall(call) if call.call_id == "call_B"
        )),
        "the diverging tool call must not be spliced behind the emitted prefix",
    );
}

#[tokio::test]
async fn mock_transport_surfaces_error_when_reconnect_text_is_shorter() {
    // Attempt 1 emits a longer text prefix than attempt 2 regenerates. The
    // positional skip would discard all of attempt 2's text and glue attempt
    // 1's truncated fragment to attempt 2's Completed. Detect the truncation
    // and surface it instead.
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
            mock_stream_attempt(
                vec![
                    LlmEvent::Started,
                    LlmEvent::TextDelta("a fairly long first answer".to_string()),
                ],
                2,
            )
        } else {
            mock_full_stream(vec![
                LlmEvent::Started,
                LlmEvent::TextDelta("short".to_string()),
                LlmEvent::completed(Some("resp_2".to_string()), CostSnapshot::default()),
            ])
        }
    });

    let collected = stream.collect::<Vec<_>>().await;
    let err = collected
        .last()
        .expect("at least one yielded result")
        .as_ref()
        .expect_err("a shorter regenerated text must surface as a stream error");
    assert!(matches!(err, SqueezyError::ProviderStream(_)));
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
fn capped_backoff_never_exceeds_max_retry_delay() {
    // A small ceiling paired with a large `max_retries` is exactly the
    // configuration the `max_retry_delay` contract exists to bound: every
    // inter-retry sleep — including the late attempts whose raw exponential
    // backoff saturates into hours — must clamp to the policy ceiling, even
    // after jitter widens the schedule by +JITTER_FRACTION.
    let policy = RetryPolicy {
        max_retries: 40,
        base_delay: Duration::from_millis(200),
        retry_429: false,
        retry_5xx: false,
        retry_transport: true,
        max_retry_delay: Duration::from_millis(5_000),
    };
    for attempt in 0u8..policy.max_retries {
        for _ in 0..16 {
            let delay = capped_backoff(policy, attempt);
            assert!(
                delay <= policy.max_retry_delay,
                "attempt {attempt}: capped delay {delay:?} exceeds ceiling {:?}",
                policy.max_retry_delay,
            );
        }
    }
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
fn parse_retry_after_handles_float_seconds() {
    // OpenAI's Responses endpoint occasionally returns sub-second
    // throttles as `Retry-After: 0.5`. The parser must keep the
    // fractional precision (rounded to milliseconds) instead of
    // returning `None` and collapsing to the zero-jitter exponential
    // backoff.
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::RETRY_AFTER,
        "0.5".parse().expect("header value"),
    );
    assert_eq!(
        parse_retry_after(&headers),
        Some(Duration::from_millis(500))
    );

    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::RETRY_AFTER,
        "1.25".parse().expect("header value"),
    );
    assert_eq!(
        parse_retry_after(&headers),
        Some(Duration::from_millis(1_250)),
    );
}

#[test]
fn parse_retry_after_rejects_negative_and_nan_floats() {
    // A negative or NaN float must fall through to `None` rather than
    // wrap around to a giant Duration. The outer policy clamp would
    // still defend us, but failing fast keeps the breadcrumb honest.
    for input in ["-1", "-0.5", "NaN", "inf"] {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            input.parse().expect("header value"),
        );
        assert_eq!(
            parse_retry_after(&headers),
            None,
            "input {input:?} must not parse to a Duration",
        );
    }
}

#[test]
fn parse_retry_after_handles_http_date() {
    // RFC 7231 permits an HTTP-date in `Retry-After`. CDN gateways and
    // some Vertex / Bedrock proxies forward the upstream's date string
    // verbatim. Construct a target ~5s in the future, format it with
    // `httpdate::fmt_http_date`, and assert the parser produces a
    // Duration in the [0s, 6s] window around that target (HTTP-date is
    // second-granularity so the elapsed-since-construction noise stays
    // sub-second).
    let target = SystemTime::now() + Duration::from_secs(5);
    let formatted = httpdate::fmt_http_date(target);
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::RETRY_AFTER,
        formatted.parse().expect("header value"),
    );
    let parsed = parse_retry_after(&headers).expect("http-date must parse");
    assert!(
        parsed <= Duration::from_secs(6),
        "expected ~5s remaining, saw {parsed:?}",
    );
}

#[test]
fn parse_retry_after_clamps_past_http_date_to_zero() {
    // A date already in the past (clock skew, slow request, etc.)
    // must clamp to zero rather than panic on the negative
    // `duration_since`. The retry then runs immediately, which is the
    // intended semantics for "wait until <past>".
    let target = SystemTime::now() - Duration::from_secs(120);
    let formatted = httpdate::fmt_http_date(target);
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::RETRY_AFTER,
        formatted.parse().expect("header value"),
    );
    assert_eq!(parse_retry_after(&headers), Some(Duration::ZERO));
}

#[test]
fn parse_retry_after_returns_none_on_unparseable_garbage() {
    // Junk values must fall through to `None` so `send_with_retry`
    // picks the exponential backoff schedule instead of treating the
    // header as a literal zero.
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::RETRY_AFTER,
        "soon-ish".parse().expect("header value"),
    );
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
/// full OAuth flow. `can_rotate` is parameterised so the same harness
/// covers both the OAuth-style refresh path (`true`) and the static-key
/// short-circuit (`false`).
#[derive(Debug)]
struct TestKeySource {
    label: String,
    keys: AsyncMutex<Vec<String>>,
    current_key_calls: AtomicUsize,
    invalidate_calls: AtomicUsize,
    can_rotate: bool,
}

impl TestKeySource {
    fn rotatable(label: &str, keys: Vec<String>) -> Arc<Self> {
        Self::with_rotation(label, keys, true)
    }

    fn static_key(label: &str, keys: Vec<String>) -> Arc<Self> {
        Self::with_rotation(label, keys, false)
    }

    fn with_rotation(label: &str, keys: Vec<String>, can_rotate: bool) -> Arc<Self> {
        Arc::new(Self {
            label: label.to_string(),
            keys: AsyncMutex::new(keys.into_iter().rev().collect()),
            current_key_calls: AtomicUsize::new(0),
            invalidate_calls: AtomicUsize::new(0),
            can_rotate,
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

    fn can_rotate(&self) -> bool {
        self.can_rotate
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
    let source = TestKeySource::rotatable("test", vec!["good-key".to_string()]);
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
    let source = TestKeySource::rotatable(
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
    let source = TestKeySource::rotatable(
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
    let source = TestKeySource::rotatable(
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

/// H-05: a `StaticApiKey`-backed source (or any source where
/// `can_rotate()` reports `false`) has no fresh credential to fall back
/// to. The auth-retry layer must surface the original 401 response
/// untouched so the provider's existing error formatter renders an
/// honest "credential rejected" message instead of looping us into a
/// second guaranteed rejection.
#[tokio::test]
async fn auth_retry_skipped_for_static_key() {
    let (addr, attempts) = spawn_status_server(vec![401, 200]).await;
    let source = TestKeySource::static_key("test", vec!["only-key".to_string()]);
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
        "static-key source must surface the original 401 without retrying",
    );
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "no second HTTP attempt: there's no fresh credential to rotate to",
    );
    assert_eq!(
        source.current_key_calls(),
        1,
        "current_key must run exactly once for the first attempt",
    );
    assert_eq!(
        source.invalidate_calls(),
        0,
        "invalidate is meaningless on a static key — skip it entirely",
    );
}

/// Same short-circuit on 403: the trait contract says `can_rotate()`
/// governs both auth-failure status codes, not just 401.
#[tokio::test]
async fn auth_retry_skipped_on_403_for_static_key() {
    let (addr, attempts) = spawn_status_server(vec![403, 200]).await;
    let source = TestKeySource::static_key("test", vec!["only-key".to_string()]);
    let source_dyn: Arc<dyn ApiKeySource> = source.clone();
    let client = reqwest::Client::new();
    let cancel = CancellationToken::new();
    let url = format!("http://{addr}");

    let response = send_with_auth_retry(&source_dyn, auth_retry_policy(), &cancel, |key| {
        client.post(&url).bearer_auth(key)
    })
    .await
    .expect("send");

    assert_eq!(response.status().as_u16(), 403);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(source.invalidate_calls(), 0);
}

// --- terminal quota classifier -------------------------------------------

/// Spin a loopback TCP server that responds to each POST with a fixed
/// `(status, body)` pair. Used to verify that `send_with_retry` reads the
/// body, runs the terminal-quota classifier, and either short-circuits
/// retries or falls through to the existing backoff path.
async fn spawn_body_server(
    responses: Vec<(u16, String)>,
) -> (std::net::SocketAddr, Arc<AtomicUsize>) {
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
            let (status, body) = responses
                .get(attempt)
                .cloned()
                .unwrap_or_else(|| responses.last().cloned().expect("non-empty response list"));

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
                429 => "Too Many Requests",
                500 => "Internal Server Error",
                _ => "Status",
            };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
    });
    (addr, attempts)
}

fn quota_retry_policy() -> RetryPolicy {
    // `max_retries` is intentionally generous so a regression that fails to
    // short-circuit terminal quotas would manifest as many attempts and a
    // long-running test rather than a silent pass.
    RetryPolicy {
        max_retries: 5,
        base_delay: Duration::from_millis(1),
        retry_429: true,
        retry_5xx: false,
        retry_transport: false,
        max_retry_delay: Duration::from_secs(60),
    }
}

#[tokio::test]
async fn send_with_retry_skips_retry_on_anthropic_permission_error() {
    // Anthropic returns "monthly usage limit reached" as a 429 wrapping a
    // `permission_error` envelope. The retry layer must read the body, run
    // the JSON-shape pass of the classifier, and surface the response to
    // the caller without burning the rest of the attempt budget on sleeps.
    let body = serde_json::json!({
        "type": "error",
        "error": {
            "type": "permission_error",
            "message": "Monthly usage limit reached"
        }
    })
    .to_string();
    let (addr, attempts) = spawn_body_server(vec![(429, body)]).await;
    let client = reqwest::Client::new();
    let cancel = CancellationToken::new();
    let url = format!("http://{addr}");

    let response = send_with_retry(quota_retry_policy(), &cancel, || client.post(&url))
        .await
        .expect("send");

    assert_eq!(response.status().as_u16(), 429);
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "Anthropic permission_error must short-circuit retries",
    );
    let body_text = response.text().await.expect("body");
    assert!(
        body_text.contains("permission_error"),
        "reconstructed response must preserve the body so the provider error \
         formatter can surface the upstream message (saw: {body_text:?})",
    );
}

#[tokio::test]
async fn send_with_retry_skips_retry_on_openai_insufficient_quota() {
    // OpenAI 429s for billing exhaustion carry `code = "insufficient_quota"`
    // in an error envelope. The classifier recognizes both the literal
    // substring and the JSON shape; either match must skip retries.
    let body = serde_json::json!({
        "error": {
            "message": "You exceeded your current quota, please check your plan.",
            "type": "insufficient_quota",
            "param": null,
            "code": "insufficient_quota",
        }
    })
    .to_string();
    let (addr, attempts) = spawn_body_server(vec![(429, body)]).await;
    let client = reqwest::Client::new();
    let cancel = CancellationToken::new();
    let url = format!("http://{addr}");

    let response = send_with_retry(quota_retry_policy(), &cancel, || client.post(&url))
        .await
        .expect("send");

    assert_eq!(response.status().as_u16(), 429);
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "OpenAI insufficient_quota must short-circuit retries",
    );
}

#[tokio::test]
async fn send_with_retry_skips_retry_on_generic_monthly_usage_limit_body() {
    // Some upstreams (and gateway shims) return a plain-text body with the
    // marketing-friendly "Monthly usage limit reached" phrase instead of a
    // structured error envelope. The substring pass of the classifier must
    // catch this so the agent stops looping on a body it cannot retry past.
    let body = "Monthly usage limit reached. Upgrade your plan to continue.";
    let (addr, attempts) = spawn_body_server(vec![(429, body.to_string())]).await;
    let client = reqwest::Client::new();
    let cancel = CancellationToken::new();
    let url = format!("http://{addr}");

    let response = send_with_retry(quota_retry_policy(), &cancel, || client.post(&url))
        .await
        .expect("send");

    assert_eq!(response.status().as_u16(), 429);
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "generic monthly_usage_limit body must short-circuit retries",
    );

    // Sanity-check the classifier in isolation so a future change that
    // moves the keyword list cannot silently regress this path.
    assert!(is_terminal_quota_error(body.as_bytes()));
}

#[tokio::test]
async fn send_with_retry_still_retries_a_regular_transient_429() {
    // A plain rate-limit 429 with no terminal markers in the body must
    // still take the existing backoff/retry path. Verifies the classifier
    // doesn't accidentally widen its match window and starve the agent of
    // legitimate retries.
    let transient_body = serde_json::json!({
        "error": {
            "message": "Rate limit exceeded, please slow down.",
            "type": "rate_limit_error"
        }
    })
    .to_string();
    let (addr, attempts) =
        spawn_body_server(vec![(429, transient_body), (200, "{}".to_string())]).await;
    let client = reqwest::Client::new();
    let cancel = CancellationToken::new();
    let url = format!("http://{addr}");

    let policy = RetryPolicy {
        max_retries: 3,
        base_delay: Duration::from_millis(1),
        retry_429: true,
        retry_5xx: false,
        retry_transport: false,
        max_retry_delay: Duration::from_secs(60),
    };
    let response = send_with_retry(policy, &cancel, || client.post(&url))
        .await
        .expect("send");

    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        2,
        "non-terminal 429 must retry exactly once before succeeding",
    );
}
