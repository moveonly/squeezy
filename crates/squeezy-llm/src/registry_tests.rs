use squeezy_core::{
    DEFAULT_ANTHROPIC_MODEL, DEFAULT_BEDROCK_MODEL, DEFAULT_CEREBRAS_MODEL, DEFAULT_DEEPSEEK_MODEL,
    DEFAULT_FIREWORKS_MODEL, DEFAULT_GOOGLE_MODEL, DEFAULT_GROQ_MODEL, DEFAULT_MISTRAL_MODEL,
    DEFAULT_OPENAI_MODEL, DEFAULT_OPENROUTER_MODEL, DEFAULT_PORTKEY_MODEL, DEFAULT_VERCEL_AI_MODEL,
    DEFAULT_VERTEX_MODEL, DEFAULT_XAI_MODEL, resolve_model_alias,
};

use super::{MODEL_REGISTRY, estimate_json_tokens, estimate_text_tokens, model_info_for};

#[test]
fn resolve_opus_alias_for_anthropic() {
    assert_eq!(
        resolve_model_alias("anthropic", "opus"),
        Some("claude-opus-4-7"),
    );
    assert_eq!(
        resolve_model_alias("anthropic", "OPUS"),
        Some("claude-opus-4-7")
    );
    assert_eq!(
        resolve_model_alias("anthropic", " opus "),
        Some("claude-opus-4-7"),
    );
    assert_eq!(
        resolve_model_alias("anthropic", "sonnet"),
        Some(DEFAULT_ANTHROPIC_MODEL),
    );
    assert_eq!(
        resolve_model_alias("anthropic", "haiku"),
        Some("claude-haiku-4-5-20251001"),
    );
}

#[test]
fn resolve_opus_alias_for_openai_returns_flagship() {
    assert_eq!(
        resolve_model_alias("openai", "opus"),
        Some(DEFAULT_OPENAI_MODEL)
    );
    assert_eq!(
        resolve_model_alias("openai", "best"),
        Some(DEFAULT_OPENAI_MODEL)
    );
    assert_eq!(
        resolve_model_alias("openai", "sonnet"),
        Some("gpt-5.4-mini")
    );
    assert_eq!(resolve_model_alias("openai", "haiku"), Some("gpt-5.4-nano"));
}

#[test]
fn resolve_alias_passes_through_full_ids_and_unknown_inputs() {
    assert_eq!(resolve_model_alias("anthropic", "claude-opus-4-7"), None);
    assert_eq!(resolve_model_alias("openai", "gpt-5.5"), None);
    assert_eq!(resolve_model_alias("anthropic", "opusplan"), None);
    assert_eq!(resolve_model_alias("ollama", "opus"), None);
    assert_eq!(resolve_model_alias("openrouter", "opus"), None);
}

#[test]
fn resolve_alias_for_bedrock_and_google() {
    assert_eq!(
        resolve_model_alias("bedrock", "opus"),
        Some(DEFAULT_BEDROCK_MODEL)
    );
    assert_eq!(
        resolve_model_alias("bedrock", "haiku"),
        Some(DEFAULT_BEDROCK_MODEL)
    );
    assert_eq!(
        resolve_model_alias("google", "opus"),
        Some(DEFAULT_GOOGLE_MODEL)
    );
    assert_eq!(
        resolve_model_alias("google", "haiku"),
        Some("gemini-2.5-flash-lite")
    );
}

#[test]
fn unknown_model_fallback_metadata_is_memoized() {
    let first = model_info_for("openai", "custom-unlisted-model").expect("fallback model info");
    let second = model_info_for("openai", "custom-unlisted-model").expect("fallback model info");
    let other = model_info_for("openai", "another-custom-model").expect("fallback model info");

    assert!(
        std::ptr::eq(first, second),
        "repeated unknown model lookups must reuse one fallback allocation"
    );
    assert!(!std::ptr::eq(first, other));
    assert_eq!(first.metadata_source, "fallback");
    assert_eq!(first.provider, "openai");
    assert_eq!(first.id, "custom-unlisted-model");
}

#[test]
fn json_token_estimate_matches_serialized_byte_count() {
    let value = serde_json::json!({
        "name": "tool",
        "parameters": {
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "limit": { "type": "integer" }
            }
        }
    });
    let serialized = serde_json::to_string(&value).expect("serialize json");

    assert_eq!(
        estimate_json_tokens(&value, 4.0),
        estimate_text_tokens(&serialized, 4.0),
        "counting writer must preserve the old byte-based estimate"
    );
}

// Catalog-integrity guard. The bundled `models.json` is hand-maintained, so a
// fat-fingered price or a typoed id silently degrades cost reporting (this is
// the exact class of bug that let a DeepSeek `cache_read` rate ship at 10x its
// real value). These assertions pin the structural invariants the catalog is
// expected to uphold and fail loudly on the next slip.
#[test]
fn catalog_has_no_duplicate_provider_id_pairs() {
    let mut seen = std::collections::HashSet::new();
    for entry in MODEL_REGISTRY.iter() {
        assert!(
            seen.insert((entry.provider, entry.id)),
            "duplicate catalog entry for ({}, {})",
            entry.provider,
            entry.id
        );
    }
}

#[test]
fn curated_default_models_resolve_without_fallback() {
    // `(provider key in models.json, DEFAULT_*_MODEL)` for every default that is
    // expected to carry curated metadata. The synthetic `faux-1` is excluded
    // per the finding (it never consults the registry), and a handful of
    // defaults intentionally fall back rather than ship curated rows:
    //   - `openai_codex` reuses the OpenAI protocol but its default id is
    //     curated under the `openai` provider, not `openai_codex`.
    //   - `together` / `cloudflare_ai_gateway` are light-preset tiers that
    //     `models.json` deliberately leaves uncurated (see the squeezy-core
    //     default-constant comments).
    let curated = [
        ("openai", DEFAULT_OPENAI_MODEL),
        ("anthropic", DEFAULT_ANTHROPIC_MODEL),
        ("google", DEFAULT_GOOGLE_MODEL),
        ("bedrock", DEFAULT_BEDROCK_MODEL),
        ("openrouter", DEFAULT_OPENROUTER_MODEL),
        ("vercel", DEFAULT_VERCEL_AI_MODEL),
        ("portkey", DEFAULT_PORTKEY_MODEL),
        ("groq", DEFAULT_GROQ_MODEL),
        ("xai", DEFAULT_XAI_MODEL),
        ("deepseek", DEFAULT_DEEPSEEK_MODEL),
        ("vertex", DEFAULT_VERTEX_MODEL),
        ("mistral", DEFAULT_MISTRAL_MODEL),
        ("fireworks", DEFAULT_FIREWORKS_MODEL),
        ("cerebras", DEFAULT_CEREBRAS_MODEL),
    ];
    for (provider, model) in curated {
        let info = model_info_for(provider, model)
            .unwrap_or_else(|| panic!("default model ({provider}, {model}) must resolve"));
        assert_ne!(
            info.metadata_source, "fallback",
            "default model ({provider}, {model}) resolved to synthetic fallback metadata \
             instead of a curated models.json row"
        );
    }
}

#[test]
fn cache_read_rate_is_a_plausible_fraction_of_input_rate() {
    // A `cache_read` rate that is a tiny sliver (<5%) of the input rate almost
    // always means a missing or extra zero rather than a genuine discount —
    // real cached-read discounts across providers land in the 10%-25% band.
    for entry in MODEL_REGISTRY.iter() {
        let Some(pricing) = entry.pricing else {
            continue;
        };
        let (Some(cache_read), input) = (
            pricing.cache_read_usd_micros_per_mtok,
            pricing.input_usd_micros_per_mtok,
        ) else {
            continue;
        };
        if input == 0 {
            continue;
        }
        // Compare 20 * cache_read against input to test `cache_read / input < 5%`
        // without floating point. `saturating_mul` keeps absurdly large rates
        // from wrapping into a false pass.
        assert!(
            cache_read.saturating_mul(20) >= input,
            "({}, {}) cache_read rate is under 5% of the input rate \
             (cache_read={cache_read}, input={input}); likely a unit/zero error",
            entry.provider,
            entry.id
        );
    }
}
