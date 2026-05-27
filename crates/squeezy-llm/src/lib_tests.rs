use futures_util::StreamExt;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use super::*;

#[tokio::test]
async fn unavailable_provider_reports_configuration_error() {
    let provider = UnavailableProvider::new("openai", "missing OPENAI_API_KEY");
    let request = LlmRequest {
        model: "test-model".to_string().into(),
        instructions: "test".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        max_output_tokens: Some(16),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
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
        model: squeezy_core::DEFAULT_OPENAI_MODEL.to_string().into(),
        instructions: "short system prompt".to_string().into(),
        input: Arc::from(vec![LlmInputItem::UserText("hello".to_string())]),
        max_output_tokens: Some(128),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
    };

    let estimate =
        estimate_request_context("openai", squeezy_core::DEFAULT_OPENAI_MODEL, &request, None);

    assert!(estimate.input_tokens > 0);
    assert_eq!(estimate.context_window_tokens, Some(400_000));
    // 95% of the raw window, minus a fixed 12_000-token baseline reserved
    // for system framing.
    assert_eq!(estimate.effective_context_window_tokens, Some(368_000));
    // Headroom = raw window - effective window.
    assert_eq!(estimate.headroom_tokens, Some(32_000));
    assert_eq!(estimate.max_output_tokens, Some(128));
    // Effective window minus max_output_tokens.
    assert_eq!(estimate.input_budget_tokens, Some(367_872));
    assert!(estimate.remaining_input_tokens.unwrap() < 367_872);
    assert!(estimate.used_input_percent_x100.is_some());
}

#[test]
fn calibrated_request_context_estimate_uses_provided_bytes_per_token() {
    // The same request must produce fewer estimated input tokens when we hand
    // in a calibration with a *higher* bytes/token ratio: the estimator
    // divides bytes by the ratio, so a bigger ratio means fewer tokens. This
    // is the contract rjr.105 relies on - a calibrated session that learns
    // its provider packs more bytes per token shows a smaller projected
    // input usage.
    let request = LlmRequest {
        model: squeezy_core::DEFAULT_OPENAI_MODEL.to_string().into(),
        instructions: "a moderately long system prompt with enough text to estimate"
            .to_string()
            .into(),
        input: Arc::from(vec![LlmInputItem::UserText(
            "another moderately long user message with several words".to_string(),
        )]),
        max_output_tokens: Some(128),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
    };

    let default_estimate =
        estimate_request_context("openai", squeezy_core::DEFAULT_OPENAI_MODEL, &request, None);

    // Seed a calibration claiming each token costs *eight* bytes - double
    // the default 4.0 - so the estimator should report roughly half the
    // input tokens.
    let mut calibration = crate::tokens::TokenCalibration::default();
    calibration.record_sample("openai", 8000, 1000);
    let calibrated_estimate = estimate_request_context_calibrated(
        "openai",
        squeezy_core::DEFAULT_OPENAI_MODEL,
        &request,
        None,
        Some(&calibration),
    );

    assert!(
        calibrated_estimate.input_tokens < default_estimate.input_tokens,
        "calibrated estimate ({}) must be smaller than default ({})",
        calibrated_estimate.input_tokens,
        default_estimate.input_tokens,
    );
    assert!(
        calibrated_estimate.input_tokens > 0,
        "calibrated estimate should still cover the structural overhead"
    );
}

#[test]
fn request_context_estimate_uses_fallback_metadata_for_unknown_models() {
    // The bundled registry now ships a fallback metadata path so unknown
    // model ids still get useful headroom/budget figures instead of empty
    // optionals.
    let request = LlmRequest {
        model: "custom-model".to_string().into(),
        instructions: "system".to_string().into(),
        input: Arc::from(Vec::<LlmInputItem>::new()),
        max_output_tokens: Some(128),
        response_verbosity: None,
        reasoning_effort: None,
        previous_response_id: None,
        cache_key: None,
        tools: Arc::from(Vec::new()),
        store: false,
        tool_choice: None,
        output_schema: None,
        parallel_tool_calls: None,
    };

    let estimate = estimate_request_context("openai", "custom-model", &request, None);

    assert!(estimate.input_tokens > 0);
    assert_eq!(estimate.context_window_tokens, Some(272_000));
    assert!(estimate.effective_context_window_tokens.unwrap() < 272_000);
    assert!(estimate.headroom_tokens.unwrap() > 0);
    assert!(estimate.input_budget_tokens.unwrap() > 0);
    assert!(estimate.remaining_input_tokens.is_some());
    assert!(estimate.used_input_percent_x100.is_some());
}
