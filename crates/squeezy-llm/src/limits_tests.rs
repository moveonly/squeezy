use super::{
    ContextLimitInput, LimitConfidence, LimitSource, SYNTHETIC_FALLBACK_CONTEXT_WINDOW,
    effective_window_tokens, resolve_context_limits,
};
use crate::models_dev::{ModelsDevCatalog, ModelsDevModel, ModelsDevProvider, ModelsDevView};

fn models_dev_model(id: &str, context: Option<u64>, output: Option<u64>) -> ModelsDevModel {
    ModelsDevModel {
        id: id.to_string(),
        name: None,
        context_window: context,
        max_output: output,
        input_usd_micros_per_mtok: None,
        output_usd_micros_per_mtok: None,
        cache_read_usd_micros_per_mtok: None,
        cache_write_usd_micros_per_mtok: None,
        reasoning: None,
        tool_call: None,
    }
}

fn view(provider: &str, models: Vec<ModelsDevModel>) -> ModelsDevView {
    let catalog = ModelsDevCatalog {
        fetched_at: 0,
        source_url: "test".to_string(),
        providers: vec![ModelsDevProvider {
            id: provider.to_string(),
            name: None,
            env: Vec::new(),
            models,
        }],
    };
    ModelsDevView::from_catalog(&catalog)
}

#[test]
fn user_override_beats_every_other_layer() {
    let dev = view(
        "openai",
        vec![models_dev_model("custom-x", Some(50_000), None)],
    );
    let mut input = ContextLimitInput::new("openai", "custom-x");
    input.user_override = Some(999_000);
    input.provider_live_window = Some(123_000);
    input.models_dev = Some(&dev);

    let resolved = resolve_context_limits(&input);
    assert_eq!(resolved.context_window_tokens, Some(999_000));
    assert_eq!(resolved.source, LimitSource::UserOverride);
    assert_eq!(resolved.confidence, LimitConfidence::Exact);
}

#[test]
fn provider_live_beats_curated_and_models_dev() {
    // `custom-x` is not curated, so live wins over models.dev.
    let dev = view(
        "openai",
        vec![models_dev_model("custom-x", Some(50_000), None)],
    );
    let mut input = ContextLimitInput::new("openai", "custom-x");
    input.provider_live_window = Some(321_000);
    input.models_dev = Some(&dev);

    let resolved = resolve_context_limits(&input);
    assert_eq!(resolved.context_window_tokens, Some(321_000));
    assert_eq!(resolved.source, LimitSource::ProviderLive);
}

#[test]
fn curated_beats_models_dev() {
    // The bundled openai default is curated; a conflicting models.dev value
    // must not win, but it should still be reported for display.
    let dev = view(
        "openai",
        vec![models_dev_model(
            squeezy_core::DEFAULT_OPENAI_MODEL,
            Some(111_111),
            None,
        )],
    );
    let mut input = ContextLimitInput::new("openai", squeezy_core::DEFAULT_OPENAI_MODEL);
    input.models_dev = Some(&dev);

    let resolved = resolve_context_limits(&input);
    assert_eq!(resolved.source, LimitSource::CuratedBundle);
    assert_eq!(resolved.confidence, LimitConfidence::High);
    assert_eq!(resolved.context_window_tokens, Some(400_000));
    assert_eq!(resolved.models_dev_window_tokens, Some(111_111));
}

#[test]
fn models_dev_fills_gap_before_synthetic_fallback() {
    let dev = view(
        "openai",
        vec![models_dev_model(
            "brand-new-model",
            Some(640_000),
            Some(40_000),
        )],
    );
    let mut input = ContextLimitInput::new("openai", "brand-new-model");
    input.models_dev = Some(&dev);

    let resolved = resolve_context_limits(&input);
    assert_eq!(resolved.source, LimitSource::ModelsDevCache);
    assert_eq!(resolved.confidence, LimitConfidence::Medium);
    assert_eq!(resolved.context_window_tokens, Some(640_000));
    assert_eq!(resolved.max_output_tokens, Some(40_000));
}

#[test]
fn synthetic_fallback_when_nothing_resolves() {
    let input = ContextLimitInput::new("openai", "totally-unknown-model");
    let resolved = resolve_context_limits(&input);
    assert_eq!(resolved.source, LimitSource::SyntheticFallback);
    assert_eq!(resolved.confidence, LimitConfidence::Low);
    assert_eq!(
        resolved.context_window_tokens,
        Some(SYNTHETIC_FALLBACK_CONTEXT_WINDOW)
    );
    assert_eq!(resolved.models_dev_window_tokens, None);
}

#[test]
fn observed_ceiling_clamps_below_user_override() {
    let mut input = ContextLimitInput::new("openai", "custom-x");
    input.user_override = Some(1_050_000);
    input.observed_ceiling = Some(312_044);

    let resolved = resolve_context_limits(&input);
    assert_eq!(resolved.context_window_tokens, Some(312_044));
    assert_eq!(resolved.source, LimitSource::ObservedBound);
    assert_eq!(resolved.observed_ceiling_tokens, Some(312_044));
}

#[test]
fn observed_ceiling_above_selected_window_does_not_raise_it() {
    let mut input = ContextLimitInput::new("openai", "custom-x");
    input.user_override = Some(100_000);
    input.observed_ceiling = Some(500_000);

    let resolved = resolve_context_limits(&input);
    assert_eq!(resolved.context_window_tokens, Some(100_000));
    assert_eq!(resolved.source, LimitSource::UserOverride);
}

#[test]
fn effective_math_respects_overrides() {
    let mut input = ContextLimitInput::new("openai", "custom-x");
    input.user_override = Some(200_000);
    input.effective_percent_override = Some(90);
    input.baseline_reserve_override = Some(5_000);

    let resolved = resolve_context_limits(&input);
    // 200_000 * 90% - 5_000 = 180_000 - 5_000 = 175_000
    assert_eq!(effective_window_tokens(&resolved), Some(175_000));
    assert_eq!(resolved.effective_context_window_percent, 90);
    assert_eq!(resolved.baseline_reserve_tokens, 5_000);
}

#[test]
fn effective_percent_is_clamped_to_1_through_100() {
    let mut over = ContextLimitInput::new("openai", "custom-x");
    over.user_override = Some(200_000);
    over.effective_percent_override = Some(200); // absurd: would otherwise inflate
    let r = resolve_context_limits(&over);
    assert_eq!(r.effective_context_window_percent, 100);
    // 200_000 * 100% - 12_000 baseline = 188_000 (never larger than the raw window)
    assert_eq!(effective_window_tokens(&r), Some(188_000));

    let mut zero = ContextLimitInput::new("openai", "custom-x");
    zero.user_override = Some(200_000);
    zero.effective_percent_override = Some(0); // would otherwise zero the window
    assert_eq!(
        resolve_context_limits(&zero).effective_context_window_percent,
        1
    );
}

#[test]
fn aggregator_namespaced_id_resolves_via_vendor_suffix() {
    // models.dev lists the bare vendor id; the aggregator route uses a
    // namespaced id. lookup() must strip the namespace.
    let dev = view(
        "openrouter",
        vec![models_dev_model(
            "claude-sonnet-4-6",
            Some(200_000),
            Some(8_192),
        )],
    );
    let limits = dev.lookup("openrouter", "anthropic/claude-sonnet-4-6");
    assert_eq!(limits.map(|l| l.context_window), Some(Some(200_000)));
}

#[test]
fn provider_id_mapping_bedrock_to_amazon_bedrock() {
    let dev = view(
        "amazon-bedrock",
        vec![models_dev_model("some-bedrock-model", Some(123_000), None)],
    );
    let limits = dev.lookup("bedrock", "some-bedrock-model");
    assert_eq!(limits.map(|l| l.context_window), Some(Some(123_000)));
}
