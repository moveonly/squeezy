use super::*;
use serde_json::json;

#[test]
fn parses_models_dev_api_shape() {
    let payload = json!({
        "anthropic": {
            "name": "Anthropic",
            "env": ["ANTHROPIC_API_KEY"],
            "models": {
                "claude-opus-4-7": {
                    "name": "Claude Opus 4.7",
                    "limit": {"context": 200000, "output": 64000},
                    "cost": {"input": 15.0, "output": 75.0, "cache_read": 1.5, "cache_write": 18.75},
                    "reasoning": true,
                    "tool_call": true
                }
            }
        },
        "openai": {
            "name": "OpenAI",
            "env": ["OPENAI_API_KEY"],
            "models": {
                "gpt-5.4": {
                    "limit": {"context": 272000, "output": 64000},
                    "cost": {"input": 5.0, "output": 15.0}
                }
            }
        }
    });
    let providers = parse_catalog(&payload);
    assert_eq!(providers.len(), 2);
    let anthropic = providers
        .iter()
        .find(|p| p.id == "anthropic")
        .expect("anthropic provider parsed");
    assert_eq!(anthropic.name.as_deref(), Some("Anthropic"));
    assert_eq!(anthropic.env, vec!["ANTHROPIC_API_KEY".to_string()]);
    assert_eq!(anthropic.models.len(), 1);
    let opus = &anthropic.models[0];
    assert_eq!(opus.id, "claude-opus-4-7");
    assert_eq!(opus.context_window, Some(200_000));
    assert_eq!(opus.max_output, Some(64_000));
    // 15.0 USD/Mtok → 15_000_000 USD-micros/Mtok.
    assert_eq!(opus.input_usd_micros_per_mtok, Some(15_000_000));
    assert_eq!(opus.output_usd_micros_per_mtok, Some(75_000_000));
    assert_eq!(opus.cache_read_usd_micros_per_mtok, Some(1_500_000));
    assert_eq!(opus.cache_write_usd_micros_per_mtok, Some(18_750_000));
    assert_eq!(opus.reasoning, Some(true));
    assert_eq!(opus.tool_call, Some(true));
}

#[test]
fn parse_tolerates_partial_entries() {
    let payload = json!({
        "ollama": {
            "name": "Ollama",
            "env": [],
            "models": {
                "llama-3.3-70b": {}
            }
        }
    });
    let providers = parse_catalog(&payload);
    assert_eq!(providers.len(), 1);
    let model = &providers[0].models[0];
    assert_eq!(model.id, "llama-3.3-70b");
    assert!(model.context_window.is_none());
    assert!(model.input_usd_micros_per_mtok.is_none());
}

#[test]
fn parse_returns_empty_for_non_object() {
    assert!(parse_catalog(&json!([])).is_empty());
    assert!(parse_catalog(&json!("nope")).is_empty());
}

#[test]
fn catalog_freshness_honors_ttl() {
    let now = now_secs();
    let fresh = ModelsDevCatalog {
        fetched_at: now,
        source_url: DEFAULT_MODELS_URL.to_string(),
        providers: vec![],
    };
    assert!(fresh.is_fresh());
    let stale = ModelsDevCatalog {
        fetched_at: now.saturating_sub(CACHE_TTL_SECS + 60),
        source_url: DEFAULT_MODELS_URL.to_string(),
        providers: vec![],
    };
    assert!(!stale.is_fresh());
}

#[test]
fn source_url_respects_env_override() {
    // Save + restore so test ordering does not leak.
    let prev = std::env::var("SQUEEZY_MODELS_URL").ok();
    // SAFETY: tests in this crate run on the same process; the assignment is
    // wrapped by save/restore so it does not bleed to other tests.
    unsafe {
        std::env::set_var("SQUEEZY_MODELS_URL", "https://example.test/api.json");
    }
    assert_eq!(source_url(), "https://example.test/api.json");
    unsafe {
        match prev {
            Some(v) => std::env::set_var("SQUEEZY_MODELS_URL", v),
            None => std::env::remove_var("SQUEEZY_MODELS_URL"),
        }
    }
}

#[test]
fn read_cached_returns_none_for_missing_file() {
    let tmp = std::env::temp_dir().join(format!(
        "squeezy-models-dev-missing-{}.json",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&tmp);
    assert!(read_cached(&tmp).is_none());
}

#[test]
fn write_then_read_round_trips_catalog() {
    let dir = std::env::temp_dir().join(format!(
        "squeezy-models-dev-rt-{}-{}",
        std::process::id(),
        now_secs()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("models.json");
    let catalog = ModelsDevCatalog {
        fetched_at: now_secs(),
        source_url: DEFAULT_MODELS_URL.to_string(),
        providers: vec![ModelsDevProvider {
            id: "openai".to_string(),
            name: Some("OpenAI".to_string()),
            env: vec!["OPENAI_API_KEY".to_string()],
            models: vec![ModelsDevModel {
                id: "gpt-5.4".to_string(),
                name: None,
                context_window: Some(272_000),
                max_output: Some(64_000),
                input_usd_micros_per_mtok: Some(5_000_000),
                output_usd_micros_per_mtok: Some(15_000_000),
                cache_read_usd_micros_per_mtok: None,
                cache_write_usd_micros_per_mtok: None,
                reasoning: Some(true),
                tool_call: Some(true),
            }],
        }],
    };
    write_atomic(&path, &catalog).unwrap();
    let loaded = read_cached(&path).expect("cache reads back");
    assert_eq!(loaded, catalog);
    let _ = std::fs::remove_dir_all(&dir);
}
