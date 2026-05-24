use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

use super::*;

#[tokio::test]
async fn unavailable_provider_reports_configuration_error() {
    let provider = UnavailableProvider::new("openai", "missing OPENAI_API_KEY");
    let request = LlmRequest {
        model: "test-model".to_string(),
        instructions: "test".to_string(),
        input: vec![LlmInputItem::UserText("hello".to_string())],
        max_output_tokens: Some(16),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        tools: Vec::new(),
        store: false,
    };

    let mut stream = provider.stream_response(request, CancellationToken::new());
    let err = stream.next().await.expect("one event").expect_err("error");

    assert!(err.to_string().contains("missing OPENAI_API_KEY"));
    assert!(stream.next().await.is_none());
}

#[test]
fn registry_estimates_known_model_costs() {
    let cost = CostSnapshot {
        input_tokens: Some(1_000_000),
        output_tokens: Some(1_000_000),
        reasoning_output_tokens: None,
        cached_input_tokens: Some(1_000_000),
        cache_write_input_tokens: None,
        estimated_usd_micros: None,
    };

    let estimate = estimate_cost("openai", squeezy_core::DEFAULT_OPENAI_MODEL, &cost);

    assert_eq!(estimate, Some(405_000));
}

#[test]
fn registry_does_not_double_subtract_anthropic_cached_input() {
    // Anthropic's Messages API reports `input_tokens` as already-uncached,
    // so the OpenAI-style `input - cached - cache_write` math under-counts
    // the standard share when prompt caching is active.
    let cached = CostSnapshot {
        input_tokens: Some(200),
        output_tokens: Some(50),
        reasoning_output_tokens: None,
        cached_input_tokens: Some(5_000),
        cache_write_input_tokens: Some(0),
        estimated_usd_micros: None,
    };
    let uncached = CostSnapshot {
        input_tokens: Some(200),
        output_tokens: Some(50),
        reasoning_output_tokens: None,
        cached_input_tokens: Some(0),
        cache_write_input_tokens: Some(0),
        estimated_usd_micros: None,
    };
    let cached_estimate =
        estimate_cost("anthropic", squeezy_core::DEFAULT_ANTHROPIC_MODEL, &cached);
    let uncached_estimate = estimate_cost(
        "anthropic",
        squeezy_core::DEFAULT_ANTHROPIC_MODEL,
        &uncached,
    );

    // The 200 standard input tokens must still be billed at the standard rate
    // even when cache_read is large; the only delta should be the cache_read
    // surcharge.
    assert!(cached_estimate.is_some());
    assert!(uncached_estimate.is_some());
    assert!(
        cached_estimate.unwrap() >= uncached_estimate.unwrap(),
        "cached cost {:?} must be >= uncached cost {:?} (cache_read is additive, not a discount)",
        cached_estimate,
        uncached_estimate,
    );
}

#[test]
fn registry_lists_ollama_as_zero_cost_local_provider() {
    let model = models_for_provider("ollama").next().expect("ollama model");

    assert_eq!(model.provider, "ollama");
    assert_eq!(model.pricing.unwrap().input_usd_micros_per_mtok, 0);
    assert!(model.capabilities.streaming);
}
