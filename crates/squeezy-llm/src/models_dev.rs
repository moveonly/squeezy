//! Runtime catalog refresh from [models.dev](https://models.dev).
//!
//! `models.dev/api.json` ships a unified catalog covering every major
//! provider — `{ "<provider>": { name, env: [..], models: { "<id>": { cost,
//! limit, modalities, ... } } } }`. We mirror it locally so the cost broker
//! and the model picker see fresh prices and context windows without rebuilds.
//!
//! Cache lives at `~/.squeezy/cache/models.json` with a 24-hour TTL — Squeezy
//! has no SaaS org-catalog layer pushing aggressive updates, so daily refresh
//! is plenty. Concurrent processes (`squeezy doctor`, `squeezy tui`, `squeezy
//! ask`) serialize the refresh through an advisory lock on a sibling
//! `models.lock` file, so two CLIs starting together don't both spend a
//! network round-trip and don't corrupt the cache mid-write.
//!
//! The curated entries in `models.json` remain authoritative for `ModelInfo`
//! capability flags; this module supplies the *catalog of slugs + pricing +
//! limits* that supplements the bundled defaults.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use squeezy_core::{Result, SqueezyError};

/// Default upstream URL. Override with `SQUEEZY_MODELS_URL` for staging /
/// air-gapped mirrors.
pub const DEFAULT_MODELS_URL: &str = "https://models.dev/api.json";

/// 24 hours. Pricing and capability rows churn slowly enough that a
/// daily refresh keeps the local catalog current without re-fetching
/// on every cold start.
pub const CACHE_TTL_SECS: u64 = 24 * 60 * 60;

const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-model record extracted from `models.dev`. Optional fields mirror the
/// upstream schema's permissiveness — older entries lack `cost` / `limit`,
/// and local-only providers have neither.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelsDevModel {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output: Option<u64>,
    /// Input pricing in USD-micros per Mtok (matches `TokenPricing`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_usd_micros_per_mtok: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_usd_micros_per_mtok: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_usd_micros_per_mtok: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_usd_micros_per_mtok: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelsDevProvider {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub env: Vec<String>,
    pub models: Vec<ModelsDevModel>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelsDevCatalog {
    pub fetched_at: u64,
    pub source_url: String,
    pub providers: Vec<ModelsDevProvider>,
}

impl ModelsDevCatalog {
    pub fn is_fresh(&self) -> bool {
        self.fetched_at.saturating_add(CACHE_TTL_SECS) > now_secs()
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Default cache location: `~/.squeezy/cache/models.json`.
pub fn default_cache_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(home.join(".squeezy").join("cache").join("models.json"))
}

/// Upstream URL, honouring `SQUEEZY_MODELS_URL` for staging / air-gapped
/// mirrors. We intentionally only read the env var (no config-file knob) so
/// CI can't accidentally point production at a malicious endpoint via
/// committed config.
pub fn source_url() -> String {
    std::env::var("SQUEEZY_MODELS_URL").unwrap_or_else(|_| DEFAULT_MODELS_URL.to_string())
}

fn user_agent() -> String {
    format!("squeezy/{}", env!("CARGO_PKG_VERSION"))
}

/// Best-effort read of the on-disk cache. Returns `None` on any IO / parse
/// failure — callers fall back to refresh or to the bundled registry.
pub fn read_cached(path: &Path) -> Option<ModelsDevCatalog> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<ModelsDevCatalog>(&text).ok()
}

/// Fetch the upstream catalog and persist it under an exclusive advisory
/// lock so two concurrent CLIs don't both spend the round-trip and don't
/// race on the file write. Re-checks TTL under the lock so the second waiter
/// returns the freshly-written cache.
pub async fn refresh_models(cache_path: &Path) -> Result<ModelsDevCatalog> {
    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Hold an exclusive lock on a sibling `.lock` file rather than the cache
    // itself — that way the holder can rewrite the cache atomically without
    // releasing its serialization guarantee.
    let lock_path = cache_path.with_extension("lock");
    // `_lock_guard` holds the advisory lock for the rest of the function;
    // the lock is released when the file is dropped on return.
    let _lock_guard = {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        file.lock()
            .map_err(|err| SqueezyError::Config(format!("models.dev cache lock failed: {err}")))?;
        file
    };

    // Under the lock, re-check freshness — another process may have refreshed
    // while we were waiting.
    if let Some(cached) = read_cached(cache_path)
        && cached.is_fresh()
    {
        return Ok(cached);
    }

    let url = source_url();
    // Route through the shared hardened client so the metadata-blocking
    // resolver applies even when the source URL is overridden via env. The
    // catalog-specific User-Agent and fetch deadline are set per-request.
    let client = crate::transport::shared_client(&squeezy_core::ProviderTransportConfig::default());
    let response = client
        .get(&url)
        .timeout(FETCH_TIMEOUT)
        .header(reqwest::header::USER_AGENT, user_agent())
        .send()
        .await
        .map_err(|err| SqueezyError::ProviderRequest(err.to_string()))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(SqueezyError::ProviderRequest(format!(
            "models.dev fetch returned {status}: {body}"
        )));
    }
    let payload: Value = response
        .json()
        .await
        .map_err(|err| SqueezyError::ProviderRequest(err.to_string()))?;
    let providers = parse_catalog(&payload);
    let catalog = ModelsDevCatalog {
        fetched_at: now_secs(),
        source_url: url,
        providers,
    };
    write_atomic(cache_path, &catalog)?;
    Ok(catalog)
}

fn write_atomic(path: &Path, catalog: &ModelsDevCatalog) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_string_pretty(catalog)
        .map_err(|err| SqueezyError::Config(err.to_string()))?;
    {
        let mut file = File::create(&tmp)?;
        file.write_all(body.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Parse a `models.dev/api.json` payload into our internal shape. The
/// upstream schema is a `{ providerId: { name, env, models: { modelId: {...}
/// } } }` map; we flatten into `Vec<ModelsDevProvider> { Vec<ModelsDevModel>
/// }`. Tolerant of missing fields so a partial upstream still yields useful
/// data.
pub fn parse_catalog(value: &Value) -> Vec<ModelsDevProvider> {
    let Some(obj) = value.as_object() else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(obj.len());
    for (provider_id, provider_value) in obj {
        let Some(provider_obj) = provider_value.as_object() else {
            continue;
        };
        let name = provider_obj
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let env = provider_obj
            .get("env")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let models = provider_obj
            .get("models")
            .and_then(|v| v.as_object())
            .map(|models_obj| {
                models_obj
                    .iter()
                    .map(|(id, entry)| parse_model(id, entry))
                    .collect()
            })
            .unwrap_or_default();
        out.push(ModelsDevProvider {
            id: provider_id.clone(),
            name,
            env,
            models,
        });
    }
    out
}

fn parse_model(id: &str, value: &Value) -> ModelsDevModel {
    let limit = value.get("limit");
    let cost = value.get("cost");
    ModelsDevModel {
        id: id.to_string(),
        name: value
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        context_window: limit
            .and_then(|v| v.get("context"))
            .and_then(|v| v.as_u64()),
        max_output: limit.and_then(|v| v.get("output")).and_then(|v| v.as_u64()),
        input_usd_micros_per_mtok: cost
            .and_then(|v| v.get("input"))
            .and_then(price_to_micros_per_mtok),
        output_usd_micros_per_mtok: cost
            .and_then(|v| v.get("output"))
            .and_then(price_to_micros_per_mtok),
        cache_read_usd_micros_per_mtok: cost
            .and_then(|v| v.get("cache_read"))
            .and_then(price_to_micros_per_mtok),
        cache_write_usd_micros_per_mtok: cost
            .and_then(|v| v.get("cache_write"))
            .and_then(price_to_micros_per_mtok),
        reasoning: value.get("reasoning").and_then(|v| v.as_bool()),
        tool_call: value.get("tool_call").and_then(|v| v.as_bool()),
    }
}

/// models.dev publishes prices as USD per Mtok as a JSON number (e.g. `3.0`
/// for $3/Mtok). Convert to USD-micros per Mtok so it round-trips through
/// `TokenPricing`: usd/mtok × 1_000_000.
fn price_to_micros_per_mtok(value: &Value) -> Option<u64> {
    let usd_per_mtok = value.as_f64()?;
    if !usd_per_mtok.is_finite() || usd_per_mtok < 0.0 {
        return None;
    }
    Some((usd_per_mtok * 1_000_000.0).round() as u64)
}

/// Context/output limits for a single model, extracted from the cached
/// models.dev catalog for the runtime resolver. Kept separate from
/// [`ModelsDevModel`] so the resolver depends only on the two numbers it
/// reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelsDevLimits {
    pub context_window: Option<u64>,
    pub max_output: Option<u64>,
}

/// Indexed, read-only view of the cached models.dev catalog keyed by
/// `(models.dev provider id, model id)`. Built once from the on-disk cache so
/// the hot-path resolver does a hash lookup and never touches the network or
/// the filesystem.
#[derive(Debug, Clone)]
pub struct ModelsDevView {
    index: HashMap<(String, String), ModelsDevLimits>,
}

impl ModelsDevView {
    pub fn from_catalog(catalog: &ModelsDevCatalog) -> Self {
        let mut index = HashMap::new();
        for provider in &catalog.providers {
            for model in &provider.models {
                index.insert(
                    (provider.id.clone(), model.id.clone()),
                    ModelsDevLimits {
                        context_window: model.context_window,
                        max_output: model.max_output,
                    },
                );
            }
        }
        Self { index }
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Look up limits for a squeezy `(provider, model)` pair. Tries each
    /// models.dev provider id mapped from the squeezy provider plus the raw
    /// provider name, and for each tries the full model id then the
    /// vendor-namespaced suffix (`anthropic/claude-…` → `claude-…`) so
    /// aggregator routes resolve to the underlying vendor's entry.
    pub fn lookup(&self, squeezy_provider: &str, model: &str) -> Option<ModelsDevLimits> {
        let suffix = model.split_once('/').map(|(_, id)| id);
        let provider_ids = models_dev_provider_ids(squeezy_provider)
            .iter()
            .copied()
            .chain(std::iter::once(squeezy_provider));
        for provider_id in provider_ids {
            for candidate in std::iter::once(model).chain(suffix) {
                if let Some(limits) = self
                    .index
                    .get(&(provider_id.to_string(), candidate.to_string()))
                {
                    return Some(*limits);
                }
            }
        }
        None
    }
}

/// Map a squeezy provider name (see `registry::PROVIDERS`) to the models.dev
/// provider id(s) carrying the same models. Most are 1:1; aggregators and
/// re-hosted vendors differ, and a few squeezy providers (Codex, Copilot,
/// Azure) proxy another vendor's models, so they fall through to that vendor.
/// Unknown providers return an empty slice — [`ModelsDevView::lookup`] still
/// tries the raw provider name, which covers exact matches not listed here.
pub fn models_dev_provider_ids(squeezy_provider: &str) -> &'static [&'static str] {
    match squeezy_provider {
        "openai" => &["openai"],
        "openai_codex" => &["openai"],
        "github_copilot" => &["github-copilot", "openai"],
        "anthropic" => &["anthropic"],
        "google" => &["google"],
        "azure_openai" => &["azure", "openai"],
        "bedrock" => &["amazon-bedrock"],
        "ollama" => &["ollama"],
        "openrouter" => &["openrouter"],
        "vercel" => &["vercel"],
        "portkey" => &["portkey"],
        "groq" => &["groq"],
        "xai" => &["xai"],
        "deepseek" => &["deepseek"],
        "vertex" => &["google-vertex", "google"],
        "mistral" => &["mistral"],
        "together" => &["togetherai"],
        "fireworks" => &["fireworks-ai"],
        "cerebras" => &["cerebras"],
        "deepinfra" => &["deepinfra"],
        "baseten" => &["baseten"],
        "lmstudio" => &["lmstudio"],
        "vllm" => &["vllm"],
        "llamacpp" => &["llamacpp"],
        "cloudflare_workers_ai" => &["cloudflare-workers-ai"],
        "cloudflare_ai_gateway" => &["cloudflare-ai-gateway"],
        _ => &[],
    }
}

static MODELS_DEV_VIEW: LazyLock<Option<ModelsDevView>> = LazyLock::new(|| {
    let path = default_cache_path()?;
    let catalog = read_cached(&path)?;
    Some(ModelsDevView::from_catalog(&catalog))
});

/// Process-lifetime view of the cached models.dev catalog, or `None` when no
/// cache file exists yet. Read-only on the hot path; the daily `refresh-models`
/// job is what writes the cache the view is built from.
pub fn cached_models_dev_view() -> Option<&'static ModelsDevView> {
    MODELS_DEV_VIEW.as_ref()
}

#[cfg(test)]
#[path = "models_dev_tests.rs"]
mod tests;
