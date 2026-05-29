use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use serde_json::json;

use super::*;

static TEMP_NONCE: AtomicU64 = AtomicU64::new(0);

fn temp_cache_dir(prefix: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "squeezy-model-discovery-{}-{}-{}",
        prefix,
        std::process::id(),
        TEMP_NONCE.fetch_add(1, Ordering::SeqCst),
    ));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    dir
}

fn sample_catalog(provider: &str, marker: &str) -> ModelCatalog {
    ModelCatalog {
        fetched_at: now_secs(),
        provider: provider.to_string(),
        models: vec![DiscoveredModel {
            id: marker.to_string(),
            context_length: Some(8_192),
            max_output_tokens: Some(1_024),
            supports_tools: Some(true),
            pricing_input_usd_micros_per_mtok: None,
            pricing_output_usd_micros_per_mtok: None,
        }],
    }
}

#[test]
fn parses_openai_style_minimal_catalog() {
    let value = json!({
        "data": [
            {"id": "gpt-5.5", "object": "model"},
            {"id": "claude-opus-4-7", "object": "model"},
        ]
    });
    let parsed = parse_catalog(&value);
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].id, "gpt-5.5");
    assert!(parsed[0].context_length.is_none());
}

#[test]
fn parses_openrouter_style_with_pricing_and_context() {
    let value = json!({
        "data": [
            {
                "id": "anthropic/claude-opus-4-7",
                "context_length": 200000,
                "pricing": {"prompt": "0.000015", "completion": "0.000075"},
                "supported_parameters": ["temperature", "tools", "tool_choice"],
                "top_provider": {"max_completion_tokens": 64000}
            }
        ]
    });
    let parsed = parse_catalog(&value);
    assert_eq!(parsed.len(), 1);
    let entry = &parsed[0];
    assert_eq!(entry.id, "anthropic/claude-opus-4-7");
    assert_eq!(entry.context_length, Some(200_000));
    assert_eq!(entry.max_output_tokens, Some(64_000));
    assert_eq!(entry.supports_tools, Some(true));
    // 0.000015 USD/token = 15 USD/Mtok = 15_000_000 USD-micros/Mtok.
    assert_eq!(entry.pricing_input_usd_micros_per_mtok, Some(15_000_000));
    assert_eq!(entry.pricing_output_usd_micros_per_mtok, Some(75_000_000));
}

#[test]
fn parses_groq_style_context_window_field() {
    let value = json!({
        "data": [
            {
                "id": "llama-3.3-70b-versatile",
                "object": "model",
                "owned_by": "Meta",
                "active": true,
                "context_window": 131072,
                "max_completion_tokens": 32768,
            }
        ]
    });
    let parsed = parse_catalog(&value);
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].context_length, Some(131_072));
    assert_eq!(parsed[0].max_output_tokens, Some(32_768));
}

#[test]
fn skips_entries_without_id() {
    let value = json!({
        "data": [
            {"object": "model"},
            {"id": "ok-model"},
            {"id": 42},
        ]
    });
    let parsed = parse_catalog(&value);
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].id, "ok-model");
}

#[test]
fn missing_data_array_returns_empty() {
    assert!(parse_catalog(&json!({})).is_empty());
    assert!(parse_catalog(&json!({"models": []})).is_empty());
}

#[test]
fn catalog_freshness_respects_ttl() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let fresh = ModelCatalog {
        fetched_at: now,
        provider: "openrouter".to_string(),
        models: Vec::new(),
    };
    assert!(fresh.is_fresh());

    let stale = ModelCatalog {
        fetched_at: now.saturating_sub(CACHE_TTL_SECS + 1),
        provider: "openrouter".to_string(),
        models: Vec::new(),
    };
    assert!(!stale.is_fresh());
}

#[test]
fn parse_price_string_rejects_invalid_and_negative_values() {
    assert_eq!(parse_price_string("0.000005"), Some(5_000_000));
    assert!(parse_price_string("not-a-number").is_none());
    assert!(parse_price_string("-1").is_none());
}

#[test]
fn resolve_capabilities_prefers_bundled_registry_over_catalog() {
    // Pick any model that ships in `models.json` — the bundled entry must
    // win even if the live catalog disagrees about tool support.
    let bundled = MODEL_REGISTRY
        .iter()
        .find(|m| m.provider == "openai")
        .expect("bundled openai model exists");
    let catalog = ModelCatalog {
        fetched_at: now_secs(),
        provider: "openai".to_string(),
        models: vec![DiscoveredModel {
            id: bundled.id.to_string(),
            context_length: None,
            max_output_tokens: None,
            // Catalog (incorrectly) says no tools — bundled registry wins.
            supports_tools: Some(false),
            pricing_input_usd_micros_per_mtok: None,
            pricing_output_usd_micros_per_mtok: None,
        }],
    };
    let resolved = resolve_capabilities_with(bundled.provider, bundled.id, Some(&catalog));
    assert_eq!(resolved.source, CapabilitySource::Bundled);
    assert_eq!(resolved.capabilities, bundled.capabilities);
}

#[test]
fn unknown_model_probes_models_endpoint_first_use() {
    // The acceptance test from the audit: a model absent from
    // `models.json` resolves from the cached `/models` catalog if the
    // catalog advertises tool support.
    let catalog = ModelCatalog {
        fetched_at: now_secs(),
        provider: "openrouter".to_string(),
        models: vec![DiscoveredModel {
            id: "mystery-org/unknown-llama-99b".to_string(),
            context_length: Some(131_072),
            max_output_tokens: Some(8_192),
            supports_tools: Some(true),
            pricing_input_usd_micros_per_mtok: None,
            pricing_output_usd_micros_per_mtok: None,
        }],
    };
    let resolved = resolve_capabilities_with(
        "openrouter",
        "mystery-org/unknown-llama-99b",
        Some(&catalog),
    );
    assert_eq!(resolved.source, CapabilitySource::LiveCatalog);
    assert!(resolved.capabilities.tool_calling);
    // Conservative defaults still apply to the rest — only the positively-
    // advertised field flips on.
    assert!(!resolved.capabilities.vision);
    assert!(!resolved.capabilities.reasoning_effort);
}

#[test]
fn unknown_model_with_no_catalog_falls_back_conservatively() {
    let resolved = resolve_capabilities_with("openrouter", "no-such-model", None);
    assert_eq!(resolved.source, CapabilitySource::ConservativeFallback);
    assert!(!resolved.capabilities.tool_calling);
    assert!(!resolved.capabilities.vision);
    assert!(!resolved.capabilities.reasoning_effort);
    // Streaming and JSON mode stay on — every OpenAI-compatible host
    // implements them.
    assert!(resolved.capabilities.streaming);
    assert!(resolved.capabilities.json_mode);
}

#[test]
fn unknown_model_with_silent_catalog_entry_falls_back_conservatively() {
    // Catalog entry exists but has no `supports_tools` flag — we can't
    // prove tools work, so refuse to advertise them.
    let catalog = ModelCatalog {
        fetched_at: now_secs(),
        provider: "openrouter".to_string(),
        models: vec![DiscoveredModel {
            id: "quiet-model".to_string(),
            context_length: Some(8_192),
            max_output_tokens: None,
            supports_tools: None,
            pricing_input_usd_micros_per_mtok: None,
            pricing_output_usd_micros_per_mtok: None,
        }],
    };
    let resolved = resolve_capabilities_with("openrouter", "quiet-model", Some(&catalog));
    assert_eq!(resolved.source, CapabilitySource::ConservativeFallback);
    assert!(!resolved.capabilities.tool_calling);
}

#[test]
fn catalog_explicitly_disables_tools_when_supports_tools_is_false() {
    let catalog = ModelCatalog {
        fetched_at: now_secs(),
        provider: "groq".to_string(),
        models: vec![DiscoveredModel {
            id: "small-mistral-instruct".to_string(),
            context_length: Some(32_768),
            max_output_tokens: None,
            supports_tools: Some(false),
            pricing_input_usd_micros_per_mtok: None,
            pricing_output_usd_micros_per_mtok: None,
        }],
    };
    let resolved = resolve_capabilities_with("groq", "small-mistral-instruct", Some(&catalog));
    assert_eq!(resolved.source, CapabilitySource::LiveCatalog);
    assert!(!resolved.capabilities.tool_calling);
}

#[test]
fn capability_source_as_str_is_stable() {
    // Stable JSON-friendly identifiers for the `squeezy doctor --json`
    // row that surfaces the handshake result.
    assert_eq!(CapabilitySource::Bundled.as_str(), "bundled");
    assert_eq!(CapabilitySource::LiveCatalog.as_str(), "live_catalog");
    assert_eq!(
        CapabilitySource::ConservativeFallback.as_str(),
        "conservative_fallback"
    );
}

#[test]
fn write_cache_to_round_trips_catalog() {
    let dir = temp_cache_dir("round-trip");
    let path = dir.join("openrouter.json");
    let catalog = sample_catalog("openrouter", "round-trip-model");

    write_cache_to(&path, &catalog).expect("write succeeds");

    let text = std::fs::read_to_string(&path).expect("cache file present");
    let parsed: ModelCatalog = serde_json::from_str(&text).expect("file parses");
    assert_eq!(parsed, catalog);
}

#[test]
fn write_cache_to_concurrent_writes_do_not_interleave() {
    // F11: two concurrent squeezy invocations refreshing the same
    // `/v1/models` catalog used to interleave their writes into
    // `<provider>.json`, leaving the persisted JSON truncated or
    // duplicated. Acquiring `fs2::try_lock_exclusive` on a sidecar
    // lock file and promoting via atomic rename serialises writers
    // and keeps the on-disk payload whole.
    let dir = temp_cache_dir("concurrent");
    let path = Arc::new(dir.join("openrouter.json"));

    let catalog_a = Arc::new(sample_catalog("openrouter", "writer-a"));
    let catalog_b = Arc::new(sample_catalog("openrouter", "writer-b"));

    let attempts = Arc::new(AtomicUsize::new(0));
    let writes_returned_ok = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(std::sync::Barrier::new(8));

    let mut handles = Vec::with_capacity(8);
    for idx in 0..8 {
        let path = Arc::clone(&path);
        let catalog = if idx % 2 == 0 {
            Arc::clone(&catalog_a)
        } else {
            Arc::clone(&catalog_b)
        };
        let attempts = Arc::clone(&attempts);
        let writes_returned_ok = Arc::clone(&writes_returned_ok);
        let barrier = Arc::clone(&barrier);

        handles.push(std::thread::spawn(move || {
            barrier.wait();
            for _ in 0..16 {
                attempts.fetch_add(1, Ordering::SeqCst);
                write_cache_to(path.as_path(), catalog.as_ref()).expect("write returns Ok");
                writes_returned_ok.fetch_add(1, Ordering::SeqCst);
            }
        }));
    }

    for h in handles {
        h.join().expect("writer thread joined");
    }

    assert_eq!(attempts.load(Ordering::SeqCst), 8 * 16);
    assert_eq!(writes_returned_ok.load(Ordering::SeqCst), 8 * 16);

    // The final file must be a complete, parseable catalog matching
    // exactly one of the two payloads — never an interleaved mix.
    let text = std::fs::read_to_string(path.as_path()).expect("cache file present");
    let parsed: ModelCatalog = serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("cache file is not valid JSON: {err}\n--- contents:\n{text}"));
    assert!(
        parsed == *catalog_a || parsed == *catalog_b,
        "cache file is a mix of catalogs A and B — flock failed to serialise writers"
    );

    // No `.tmp` sibling should remain — every successful writer cleans
    // up by renaming the temp file into place.
    let tmp_siblings: Vec<_> = std::fs::read_dir(&dir)
        .expect("read tempdir")
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(
        tmp_siblings.is_empty(),
        "expected no `.tmp` files left behind, found: {:?}",
        tmp_siblings.iter().map(|e| e.path()).collect::<Vec<_>>()
    );
}

#[test]
fn write_cache_to_skips_when_lock_is_contended() {
    // When another process is holding the exclusive flock, the writer
    // returns Ok(()) without touching the cache file — startup stays
    // unblocked and the held writer is expected to publish an
    // equivalent payload on its own.
    let dir = temp_cache_dir("contended");
    let path = dir.join("openrouter.json");
    let lock_path = lock_path_for(&path);

    // Seed an initial catalog so we can verify it isn't overwritten.
    let initial = sample_catalog("openrouter", "initial");
    write_cache_to(&path, &initial).expect("initial write succeeds");

    let holder = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .expect("open lock file");
    use fs2::FileExt as _;
    holder.try_lock_exclusive().expect("acquire exclusive lock");

    let intruder = sample_catalog("openrouter", "intruder");
    write_cache_to(&path, &intruder).expect("contended write returns Ok");

    let text = std::fs::read_to_string(&path).expect("cache file present");
    let parsed: ModelCatalog = serde_json::from_str(&text).expect("file parses");
    assert_eq!(
        parsed, initial,
        "contended writer must not overwrite the existing payload"
    );

    fs2::FileExt::unlock(&holder).expect("release lock");
}
