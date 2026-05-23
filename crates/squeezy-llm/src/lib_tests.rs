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
        cached_input_tokens: Some(1_000_000),
        cache_write_input_tokens: None,
        estimated_usd_micros: None,
    };

    let estimate = estimate_cost("openai", squeezy_core::DEFAULT_OPENAI_MODEL, &cost);

    assert_eq!(estimate, Some(405_000));
}

#[test]
fn registry_lists_ollama_as_zero_cost_local_provider() {
    let model = models_for_provider("ollama").next().expect("ollama model");

    assert_eq!(model.provider, "ollama");
    assert_eq!(model.pricing.unwrap().input_usd_micros_per_mtok, 0);
    assert!(model.capabilities.streaming);
}
