use squeezy_core::{
    DEFAULT_ANTHROPIC_MODEL, DEFAULT_BEDROCK_MODEL, DEFAULT_CEREBRAS_MODEL, DEFAULT_DEEPSEEK_MODEL,
    DEFAULT_FIREWORKS_MODEL, DEFAULT_GOOGLE_MODEL, DEFAULT_GROQ_MODEL, DEFAULT_MISTRAL_MODEL,
    DEFAULT_OPENAI_MODEL, DEFAULT_OPENROUTER_MODEL, DEFAULT_PORTKEY_MODEL, DEFAULT_VERCEL_AI_MODEL,
    DEFAULT_VERTEX_MODEL, DEFAULT_XAI_MODEL, GitHubCopilotConfig, ProviderConfig,
    ProviderTransportConfig, resolve_model_alias,
};

use super::{
    MODEL_REGISTRY, PROVIDERS, estimate_json_tokens, estimate_text_tokens,
    is_text_model_picker_eligible, model_info_for, provider_from_config,
    provider_honors_output_schema,
};

static GITHUB_COPILOT_AUTH_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn resolve_opus_alias_for_anthropic() {
    assert_eq!(
        resolve_model_alias("anthropic", "opus"),
        Some("claude-opus-4-8"),
    );
    assert_eq!(
        resolve_model_alias("anthropic", "OPUS"),
        Some("claude-opus-4-8")
    );
    assert_eq!(
        resolve_model_alias("anthropic", " opus "),
        Some("claude-opus-4-8"),
    );
    assert_eq!(
        resolve_model_alias("anthropic", "opus-4.8"),
        Some("claude-opus-4-8"),
    );
    assert_eq!(
        resolve_model_alias("anthropic", "best"),
        Some("claude-opus-4-8"),
    );
    assert_eq!(
        resolve_model_alias("anthropic", "opus-4-7"),
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
    assert_eq!(resolve_model_alias("anthropic", "claude-opus-4-8"), None);
    assert_eq!(resolve_model_alias("openai", "gpt-5.5"), None);
    assert_eq!(resolve_model_alias("anthropic", "opusplan"), None);
    assert_eq!(resolve_model_alias("ollama", "opus"), None);
}

#[test]
fn resolve_alias_for_bedrock_and_google() {
    assert_eq!(
        resolve_model_alias("bedrock", "opus"),
        Some("anthropic.claude-opus-4-8")
    );
    assert_eq!(
        resolve_model_alias("bedrock", "opus-4.8"),
        Some("anthropic.claude-opus-4-8")
    );
    assert_eq!(
        resolve_model_alias("bedrock", "sonnet"),
        Some(DEFAULT_BEDROCK_MODEL)
    );
    assert_eq!(
        resolve_model_alias("bedrock", "haiku"),
        Some(squeezy_core::BEDROCK_SMALL_FAST_MODEL)
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
fn resolve_opus_alias_for_anthropic_gateways() {
    assert_eq!(
        resolve_model_alias("openrouter", "opus"),
        Some("anthropic/claude-opus-4.8")
    );
    assert_eq!(
        resolve_model_alias("openrouter", "opus-4.8"),
        Some("anthropic/claude-opus-4.8")
    );
    assert_eq!(
        resolve_model_alias("openrouter", "opus-4-7"),
        Some("anthropic/claude-opus-4-7")
    );
    assert_eq!(
        resolve_model_alias("openrouter", "sonnet"),
        Some(DEFAULT_OPENROUTER_MODEL)
    );
    assert_eq!(
        resolve_model_alias("vercel", "best"),
        Some("anthropic/claude-opus-4.8")
    );
    assert_eq!(
        resolve_model_alias("vercel", "opus-4-8"),
        Some("anthropic/claude-opus-4.8")
    );
    assert_eq!(
        resolve_model_alias("vercel", "opus-4.7"),
        Some("anthropic/claude-opus-4.7")
    );
    assert_eq!(
        resolve_model_alias("vercel", "sonnet"),
        Some(DEFAULT_VERCEL_AI_MODEL)
    );
    assert_eq!(
        resolve_model_alias("portkey", "opus"),
        Some("anthropic/claude-opus-4-8")
    );
    assert_eq!(
        resolve_model_alias("portkey", "opus-4.8"),
        Some("anthropic/claude-opus-4-8")
    );
    assert_eq!(
        resolve_model_alias("portkey", "opus-4-7"),
        Some("anthropic/claude-opus-4-7")
    );
    assert_eq!(
        resolve_model_alias("portkey", "sonnet"),
        Some(DEFAULT_PORTKEY_MODEL)
    );
    assert_eq!(
        resolve_model_alias("cloudflare_workers_ai", "opus-4.8"),
        Some("anthropic/claude-opus-4.8")
    );
    assert_eq!(
        resolve_model_alias("cloudflare_ai_gateway", "opus-4-8"),
        Some("anthropic/claude-opus-4.8")
    );
    assert_eq!(resolve_model_alias("cloudflare_ai_gateway", "opus"), None);
}

#[test]
fn xai_image_models_are_not_text_picker_eligible() {
    assert!(!is_text_model_picker_eligible("xai", "grok-imagine"));
    assert!(!is_text_model_picker_eligible("xai", "grok-imagine-1"));
    assert!(!is_text_model_picker_eligible(
        "xai",
        "xai/grok-imagine-image"
    ));
    assert!(is_text_model_picker_eligible("xai", "grok-4.3"));
    assert!(is_text_model_picker_eligible("openai", "grok-imagine"));
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
fn providers_list_includes_every_curated_model_provider() {
    let surfaced = PROVIDERS
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    let curated = MODEL_REGISTRY
        .iter()
        .map(|entry| entry.provider)
        .collect::<std::collections::BTreeSet<_>>();

    let missing = curated.difference(&surfaced).copied().collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "PROVIDERS omits curated models.json provider(s): {missing:?}"
    );
}

#[test]
fn providers_list_exposes_github_copilot_oauth_provider() {
    assert!(PROVIDERS.contains(&"github_copilot"));
}

#[test]
fn provider_from_config_reports_missing_github_copilot_auth_file() {
    let _guard = GITHUB_COPILOT_AUTH_ENV_LOCK
        .lock()
        .expect("github copilot auth env lock");
    let missing_path = std::env::temp_dir().join(format!(
        "squeezy-missing-github-copilot-auth-{}.json",
        std::process::id()
    ));
    let previous = std::env::var("SQUEEZY_GITHUB_COPILOT_AUTH_FILE").ok();

    // SAFETY: guarded by GITHUB_COPILOT_AUTH_ENV_LOCK in this module.
    unsafe {
        std::env::set_var("SQUEEZY_GITHUB_COPILOT_AUTH_FILE", &missing_path);
    }
    let config = ProviderConfig::GitHubCopilot(GitHubCopilotConfig {
        transport: ProviderTransportConfig::default(),
    });
    let result = provider_from_config(&config);
    // SAFETY: guarded by GITHUB_COPILOT_AUTH_ENV_LOCK in this module.
    unsafe {
        match previous {
            Some(value) => std::env::set_var("SQUEEZY_GITHUB_COPILOT_AUTH_FILE", value),
            None => std::env::remove_var("SQUEEZY_GITHUB_COPILOT_AUTH_FILE"),
        }
    }

    let err = match result {
        Ok(_) => panic!("expected missing github-copilot auth file to fail"),
        Err(err) => err,
    };
    let message = err.to_string();
    assert!(message.contains("github-copilot OAuth credentials"));
    assert!(message.contains("squeezy auth github-copilot login"));
}

#[test]
fn curated_default_models_resolve_without_fallback() {
    // `(provider key in models.json, DEFAULT_*_MODEL)` for every default that is
    // expected to carry curated metadata. The synthetic `faux-1` is excluded
    // per the finding (it never consults the registry), and a handful of
    // defaults intentionally fall back rather than ship curated rows:
    //   - `openai_codex` reuses the OpenAI protocol but its default id is
    //     curated under the `openai` provider, not `openai_codex`.
    //   - `together` / `cloudflare_ai_gateway` default to light-preset ids
    //     that `models.json` deliberately leaves uncurated (see the
    //     squeezy-core default-constant comments).
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
fn catalog_includes_claude_opus_4_8_across_anthropic_routes() {
    let expected = [
        ("anthropic", "claude-opus-4-8", Some(5_000_000)),
        ("bedrock", "anthropic.claude-opus-4-8", Some(5_000_000)),
        ("openrouter", "anthropic/claude-opus-4.8", None),
        ("portkey", "anthropic/claude-opus-4-8", None),
        ("vercel", "anthropic/claude-opus-4.8", None),
        ("cloudflare_workers_ai", "anthropic/claude-opus-4.8", None),
        ("cloudflare_ai_gateway", "anthropic/claude-opus-4.8", None),
        ("vertex", "claude-opus-4-8", Some(5_000_000)),
    ];

    for (provider, model, input_rate) in expected {
        let info = model_info_for(provider, model)
            .unwrap_or_else(|| panic!("missing Claude Opus 4.8 row for {provider}/{model}"));
        assert_ne!(info.metadata_source, "fallback");
        assert_eq!(info.profile, squeezy_core::ModelProfile::Strong);
        assert_eq!(info.tokenizer.as_str(), "anthropic_estimate");
        assert_eq!(info.limits.unwrap().context_window_tokens, 1_000_000);
        assert_eq!(info.limits.unwrap().max_output_tokens, 128_000);
        assert_eq!(
            info.pricing
                .map(|pricing| pricing.input_usd_micros_per_mtok),
            input_rate
        );
    }
}

#[test]
fn default_anthropic_routes_are_sonnet() {
    let expected = [
        ("anthropic", DEFAULT_ANTHROPIC_MODEL),
        ("bedrock", DEFAULT_BEDROCK_MODEL),
        ("openrouter", DEFAULT_OPENROUTER_MODEL),
        ("portkey", DEFAULT_PORTKEY_MODEL),
        ("vercel", DEFAULT_VERCEL_AI_MODEL),
    ];

    for (provider, model) in expected {
        let info = model_info_for(provider, model)
            .unwrap_or_else(|| panic!("missing default Sonnet row for {provider}/{model}"));
        assert_ne!(info.metadata_source, "fallback");
        assert_eq!(info.profile, squeezy_core::ModelProfile::Balanced);
        assert_eq!(info.tokenizer.as_str(), "anthropic_estimate");
    }
}

#[test]
fn catalog_keeps_claude_opus_4_7_across_existing_routes() {
    let expected = [
        ("anthropic", "claude-opus-4-7"),
        ("openrouter", "anthropic/claude-opus-4-7"),
        ("openrouter", "anthropic/claude-opus-4.7"),
        ("portkey", "anthropic/claude-opus-4-7"),
        ("vercel", "anthropic/claude-opus-4.7"),
    ];

    for (provider, model) in expected {
        let info = model_info_for(provider, model)
            .unwrap_or_else(|| panic!("missing Claude Opus 4.7 row for {provider}/{model}"));
        assert_ne!(info.metadata_source, "fallback");
        assert_eq!(info.profile, squeezy_core::ModelProfile::Strong);
        assert_eq!(info.tokenizer.as_str(), "anthropic_estimate");
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

#[test]
fn provider_honors_output_schema_gates_on_provider_wire_path() {
    // Providers that forward output_schema with a json_mode model.
    assert!(provider_honors_output_schema("openai", "gpt-5.5"));
    assert!(provider_honors_output_schema(
        "google",
        DEFAULT_GOOGLE_MODEL
    ));
    assert!(provider_honors_output_schema("xai", DEFAULT_XAI_MODEL));
    assert!(provider_honors_output_schema(
        "openrouter",
        DEFAULT_OPENROUTER_MODEL
    ));

    // Providers whose wire path silently drops the schema — must report
    // false even though their models advertise json_mode, so callers keep
    // the historical free-form request.
    assert!(!provider_honors_output_schema(
        "anthropic",
        DEFAULT_ANTHROPIC_MODEL
    ));
    assert!(!provider_honors_output_schema(
        "bedrock",
        DEFAULT_BEDROCK_MODEL
    ));
    assert!(!provider_honors_output_schema("ollama", "qwen3-coder"));
}

#[test]
fn provider_honors_output_schema_false_for_unknown_provider() {
    // An unknown provider has no capabilities entry beyond the synthetic
    // fallback; the gate must not panic and must default to attaching
    // nothing rather than guessing support.
    assert!(!provider_honors_output_schema(
        "not-a-provider",
        "mystery-model"
    ));
}

#[test]
fn provider_honors_output_schema_false_for_unknown_model() {
    assert!(
        !provider_honors_output_schema("openai", "custom-unlisted-model"),
        "fallback metadata must not guess strict schema support for unknown models"
    );
}
