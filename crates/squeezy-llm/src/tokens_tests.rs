use super::*;

#[test]
fn default_bytes_per_token_falls_back_for_unknown_provider() {
    assert_eq!(default_bytes_per_token("mystery"), DEFAULT_BYTES_PER_TOKEN);
}

#[test]
fn estimate_tokens_returns_zero_for_empty_text() {
    assert_eq!(estimate_tokens("", 4.0), 0);
}

#[test]
fn estimate_tokens_rounds_up_for_short_text() {
    assert_eq!(estimate_tokens("abc", 4.0), 1);
    assert_eq!(estimate_tokens("abcd", 4.0), 1);
    assert_eq!(estimate_tokens("abcde", 4.0), 2);
}

#[test]
fn calibration_seeds_from_provider_default_on_first_sample() {
    let mut calibration = TokenCalibration::default();
    calibration.record_sample("anthropic", 800, 200);
    assert_eq!(calibration.bytes_per_token("anthropic"), 4.0);
    assert_eq!(calibration.providers["anthropic"].samples, 1);
}

#[test]
fn calibration_blends_subsequent_samples_via_ema() {
    let mut calibration = TokenCalibration::default();
    calibration.record_sample("openai", 4000, 1000); // ratio 4.0
    calibration.record_sample("openai", 5000, 1000); // ratio 5.0
    let value = calibration.bytes_per_token("openai");
    assert!(
        (value - (4.0 * (1.0 - DEFAULT_EMA_ALPHA) + 5.0 * DEFAULT_EMA_ALPHA)).abs() < 1e-9,
        "expected EMA blend, got {value}"
    );
    assert_eq!(calibration.providers["openai"].samples, 2);
}

#[test]
fn calibration_ignores_zero_sample_inputs() {
    let mut calibration = TokenCalibration::default();
    calibration.record_sample("openai", 0, 100);
    calibration.record_sample("openai", 100, 0);
    assert!(calibration.providers.is_empty());
}
