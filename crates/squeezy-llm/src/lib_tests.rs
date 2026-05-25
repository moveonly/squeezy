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

    assert_eq!(estimate, Some(30_500_000));
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

#[test]
fn registry_lists_context_limits_for_hosted_defaults() {
    let openai = model_info_for("openai", squeezy_core::DEFAULT_OPENAI_MODEL).expect("openai");
    assert_eq!(openai.limits.unwrap().context_window_tokens, 400_000);
    assert_eq!(openai.limits.unwrap().max_output_tokens, 128_000);

    let anthropic =
        model_info_for("anthropic", squeezy_core::DEFAULT_ANTHROPIC_MODEL).expect("anthropic");
    assert_eq!(squeezy_core::DEFAULT_ANTHROPIC_MODEL, "claude-opus-4-7");
    assert_eq!(anthropic.limits.unwrap().context_window_tokens, 200_000);
    assert_eq!(anthropic.limits.unwrap().max_output_tokens, 64_000);

    let bedrock = model_info_for("bedrock", squeezy_core::DEFAULT_BEDROCK_MODEL).expect("bedrock");
    assert_eq!(
        squeezy_core::DEFAULT_BEDROCK_MODEL,
        "anthropic.claude-haiku-4-5-20251001-v1:0"
    );
    assert_eq!(bedrock.limits.unwrap().context_window_tokens, 200_000);

    let google = model_info_for("google", squeezy_core::DEFAULT_GOOGLE_MODEL).expect("google");
    assert_eq!(google.limits.unwrap().context_window_tokens, 1_048_576);

    let ollama = model_info_for("ollama", squeezy_core::DEFAULT_OLLAMA_MODEL).expect("ollama");
    assert!(ollama.limits.is_none());
}

#[test]
fn registry_lists_three_tiers_for_major_hosted_providers() {
    for provider in ["openai", "anthropic", "google"] {
        let models = models_for_provider(provider).collect::<Vec<_>>();
        assert!(
            models.len() >= 3,
            "{provider} should expose at least three selectable models"
        );
        assert!(
            models
                .iter()
                .any(|model| model.profile == squeezy_core::ModelProfile::Strong)
        );
        assert!(
            models
                .iter()
                .any(|model| model.profile == squeezy_core::ModelProfile::Balanced)
        );
        assert!(
            models
                .iter()
                .any(|model| model.profile == squeezy_core::ModelProfile::Cheap)
        );
    }
}

#[test]
fn request_context_estimate_reports_budget_when_model_limit_exists() {
    let request = LlmRequest {
        model: squeezy_core::DEFAULT_OPENAI_MODEL.to_string(),
        instructions: "short system prompt".to_string(),
        input: vec![LlmInputItem::UserText("hello".to_string())],
        max_output_tokens: Some(128),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        tools: Vec::new(),
        store: false,
    };

    let estimate =
        estimate_request_context("openai", squeezy_core::DEFAULT_OPENAI_MODEL, &request, None);

    assert!(estimate.input_tokens > 0);
    assert_eq!(estimate.context_window_tokens, Some(400_000));
    assert_eq!(estimate.max_output_tokens, Some(128));
    assert_eq!(estimate.input_budget_tokens, Some(399_872));
    assert!(estimate.remaining_input_tokens.unwrap() < 399_872);
    assert!(estimate.used_input_percent_x100.is_some());
}

#[test]
fn request_context_estimate_omits_budget_when_model_limit_is_unknown() {
    let request = LlmRequest {
        model: "custom-model".to_string(),
        instructions: "system".to_string(),
        input: Vec::new(),
        max_output_tokens: Some(128),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        tools: Vec::new(),
        store: false,
    };

    let estimate = estimate_request_context("openai", "custom-model", &request, None);

    assert!(estimate.input_tokens > 0);
    assert_eq!(estimate.context_window_tokens, None);
    assert_eq!(estimate.input_budget_tokens, None);
    assert_eq!(estimate.remaining_input_tokens, None);
    assert_eq!(estimate.used_input_percent_x100, None);
}
