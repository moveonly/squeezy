use super::is_responses_capable;

#[test]
fn xai_uses_responses_api_for_grok_3_and_newer() {
    // Grok 3 onward exposes the OpenAI-Responses-compatible endpoint; the
    // routing predicate must select Responses for every supported variant
    // so callers do not silently degrade to Chat Completions.
    let responses_models = [
        "grok-3",
        "grok-3-mini",
        "grok-3-fast",
        "grok-4",
        "grok-4-fast-reasoning",
        "grok-4-fast-non-reasoning",
        "grok-code-fast-1",
        "GROK-4",
    ];
    for model in responses_models {
        assert!(
            is_responses_capable(model),
            "{model} must route via Responses API"
        );
    }
}

#[test]
fn xai_uses_chat_completions_for_grok_2_and_earlier() {
    // Grok 2 and grok-beta predate the Responses launch and only answer
    // Chat Completions. Mis-routing them onto Responses would 404 every
    // turn, so the predicate must return false.
    let chat_models = [
        "grok-2",
        "grok-2-mini",
        "grok-2-vision",
        "grok-beta",
        "grok-1",
    ];
    for model in chat_models {
        assert!(
            !is_responses_capable(model),
            "{model} must route via Chat Completions"
        );
    }
}

#[test]
fn xai_routes_unknown_models_to_chat_completions() {
    // Conservative default: if we cannot recognise the generation, fall
    // back to Chat Completions because every Grok model speaks it but
    // older generations 404 on `/responses`.
    assert!(!is_responses_capable(""));
    assert!(!is_responses_capable("grok-"));
    assert!(!is_responses_capable("not-a-grok"));
}

#[test]
fn xai_strips_aggregator_namespace_prefix() {
    // A `vendor/model` prefix appears when a model id is forwarded from an
    // aggregator (OpenRouter, Vercel AI Gateway) but the caller pointed the
    // xAI provider at a base_url that still serves the vendor route. Honour
    // the namespace so routing tracks the underlying generation.
    assert!(is_responses_capable("xai/grok-4"));
    assert!(!is_responses_capable("xai/grok-2"));
}
