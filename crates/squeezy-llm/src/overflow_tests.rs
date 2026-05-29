use super::{OverflowSignal, Usage, classify_terminal};

/// Path 1: an explicit overflow error message — Anthropic's
/// canonical `prompt is too long: N tokens > M maximum` body —
/// promotes to `ErrorPattern` regardless of usage/finish so the
/// agent can surface the upstream phrasing as-is.
#[test]
fn error_pattern_fires_on_overflow_message() {
    let signal = classify_terminal(
        "anthropic",
        Some("end_turn"),
        Some("400: prompt is too long: 250000 tokens > 200000 maximum"),
        None,
        true,
    );

    match signal {
        Some(OverflowSignal::ErrorPattern(msg)) => {
            assert!(
                msg.contains("prompt is too long"),
                "expected the upstream phrasing to round-trip verbatim, got {msg:?}",
            );
        }
        other => panic!("expected ErrorPattern, got {other:?}"),
    }
}

/// Path 2: usage saturation with no error and a clean `end_turn`
/// finish still raises `SilentUsage` so the agent compacts before
/// the next call instead of looping into another overflow.
#[test]
fn silent_usage_fires_when_reported_used_reaches_max() {
    let signal = classify_terminal(
        "anthropic",
        Some("end_turn"),
        None,
        Some(&Usage {
            used: 200_000,
            max: 200_000,
        }),
        true,
    );

    assert_eq!(
        signal,
        Some(OverflowSignal::SilentUsage {
            used: 200_000,
            max: 200_000,
        }),
    );
}

/// Path 3: an OpenAI-style `length` finish with no visible output
/// matches `LengthStopZeroOutput`. The classifier deliberately
/// requires `output_was_empty=true`; a length finish with real
/// output is plain truncation, not overflow, and gets `None`.
#[test]
fn length_stop_zero_output_fires_on_max_tokens_with_no_output() {
    let overflow_signal = classify_terminal("openai", Some("length"), None, None, true);
    assert_eq!(overflow_signal, Some(OverflowSignal::LengthStopZeroOutput));

    let healthy_truncation = classify_terminal("openai", Some("length"), None, None, false);
    assert_eq!(
        healthy_truncation, None,
        "length finish with visible output is not overflow",
    );
}
