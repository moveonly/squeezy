//! Live model catalog discovery for OpenAI-compatible providers.
//!
//! Most aggregators (OpenRouter, Vercel AI Gateway, PortKey, Groq, xAI,
//! DeepSeek, etc.) expose `GET {base_url}/models` returning a JSON catalog.
//! This module:
//!
//!   * fetches that catalog,
//!   * caches it to `~/.squeezy/cache/models/<provider>.json` with a TTL,
//!   * exposes synchronous lookup of the cached catalog for the startup
//!     picker (so startup stays fast),
//!   * exposes async refresh so the cache can be warmed in the background.
//!
//! The curated entries in `models.json` remain the source of truth for cost
//! accounting and capability flags — live discovery just tells us *what
//! models exist* on a given provider so the picker can show fresh listings
//! without a release.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use squeezy_core::{Result, SqueezyError};

use crate::registry::{MODEL_REGISTRY, ModelCapabilities};

pub const CACHE_TTL_SECS: u64 = 24 * 60 * 60;
const DEFAULT_FETCH_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredModel {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_tools: Option<bool>,
    /// Vendor pricing for input tokens, in USD-micros per million tokens.
    /// `None` when the upstream catalog did not advertise pricing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing_input_usd_micros_per_mtok: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing_output_usd_micros_per_mtok: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCatalog {
    pub fetched_at: u64,
    pub provider: String,
    pub models: Vec<DiscoveredModel>,
}

impl ModelCatalog {
    pub fn is_fresh(&self) -> bool {
        let now = now_secs();
        self.fetched_at.saturating_add(CACHE_TTL_SECS) > now
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn cache_path(provider: &str) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(
        home.join(".squeezy")
            .join("cache")
            .join("models")
            .join(format!("{provider}.json")),
    )
}

pub fn read_cached(provider: &str) -> Option<ModelCatalog> {
    let path = cache_path(provider)?;
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<ModelCatalog>(&text).ok()
}

pub fn write_cache(catalog: &ModelCatalog) -> Result<()> {
    let path = cache_path(&catalog.provider).ok_or_else(|| {
        SqueezyError::Config(
            "model discovery: cannot determine home directory for cache write".to_string(),
        )
    })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(catalog)
        .map_err(|err| SqueezyError::Config(err.to_string()))?;
    std::fs::write(path, text)?;
    Ok(())
}

pub async fn fetch_remote(base_url: &str, api_key: Option<&str>) -> Result<Vec<DiscoveredModel>> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(DEFAULT_FETCH_TIMEOUT)
        .build()
        .map_err(|err| SqueezyError::ProviderRequest(err.to_string()))?;
    let mut request = client.get(&url);
    if let Some(key) = api_key
        && !key.is_empty()
    {
        request = request.bearer_auth(key);
    }
    let response = request
        .send()
        .await
        .map_err(|err| SqueezyError::ProviderRequest(err.to_string()))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(SqueezyError::ProviderRequest(format!(
            "model discovery returned {status}: {body}"
        )));
    }
    let value: Value = response
        .json()
        .await
        .map_err(|err| SqueezyError::ProviderRequest(err.to_string()))?;
    Ok(parse_catalog(&value))
}

/// Best-effort parse of an OpenAI-compatible model-list response. Different
/// aggregators ship slightly different shapes (top-level `data` array, with
/// id + optional context_length + optional pricing + optional capability
/// hints); we accept any object with an `id` and ignore everything else.
pub fn parse_catalog(value: &Value) -> Vec<DiscoveredModel> {
    let Some(data) = value.get("data").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    data.iter().filter_map(parse_entry).collect()
}

fn parse_entry(value: &Value) -> Option<DiscoveredModel> {
    let id = value.get("id").and_then(|v| v.as_str())?.to_string();
    let context_length = value
        .get("context_length")
        .or_else(|| value.get("context_window"))
        .and_then(|v| v.as_u64())
        .or_else(|| {
            // OpenRouter nests provider-specific limits under `top_provider`.
            value
                .get("top_provider")
                .and_then(|tp| tp.get("context_length"))
                .and_then(|v| v.as_u64())
        });
    let max_output_tokens = value
        .get("max_completion_tokens")
        .or_else(|| value.get("max_tokens"))
        .or_else(|| {
            value
                .get("top_provider")
                .and_then(|tp| tp.get("max_completion_tokens"))
        })
        .and_then(|v| v.as_u64());
    let supports_tools = value
        .get("supported_parameters")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().any(|s| s.as_str() == Some("tools")));
    Some(DiscoveredModel {
        id,
        context_length,
        max_output_tokens,
        supports_tools,
        pricing_input_usd_micros_per_mtok: parse_price(value, "prompt").or_else(|| {
            value
                .get("pricing")
                .and_then(|p| p.get("input"))
                .and_then(|v| v.as_str())
                .and_then(parse_price_string)
        }),
        pricing_output_usd_micros_per_mtok: parse_price(value, "completion").or_else(|| {
            value
                .get("pricing")
                .and_then(|p| p.get("output"))
                .and_then(|v| v.as_str())
                .and_then(parse_price_string)
        }),
    })
}

fn parse_price(value: &Value, key: &str) -> Option<u64> {
    let raw = value
        .get("pricing")
        .and_then(|p| p.get(key))
        .and_then(|v| v.as_str())?;
    parse_price_string(raw)
}

fn parse_price_string(raw: &str) -> Option<u64> {
    // OpenRouter publishes prices as a decimal string of USD per token, e.g.
    // `"0.000005"` for $5/Mtok. Convert to USD-micros per Mtok (a u64) so it
    // round-trips through the registry's existing pricing types: usd-per-token
    // × 10^12.
    let usd_per_token = raw.parse::<f64>().ok()?;
    if !usd_per_token.is_finite() || usd_per_token < 0.0 {
        return None;
    }
    let micros_per_mtok = usd_per_token * 1_000_000.0 * 1_000_000.0;
    Some(micros_per_mtok.round() as u64)
}

/// Convenience: fetch + persist a fresh catalog for `provider`. Returns the
/// catalog so callers can use it immediately if they want.
pub async fn refresh(
    provider: &str,
    base_url: &str,
    api_key: Option<&str>,
) -> Result<ModelCatalog> {
    let models = fetch_remote(base_url, api_key).await?;
    let catalog = ModelCatalog {
        fetched_at: now_secs(),
        provider: provider.to_string(),
        models,
    };
    // Best-effort persist; cache miss does not fail the refresh.
    let _ = write_cache(&catalog);
    Ok(catalog)
}

/// Conservative capability defaults for `(provider, model)` pairs that are
/// absent from both the bundled `models.json` registry and the live
/// `/models` discovery cache. With arbitrary OpenAI-compatible aggregators
/// serving any subset of models, optimistically advertising tool calling
/// causes a 4xx (Groq, Cerebras) or a silent drop where the provider
/// strips `tools` and emits a chatty no-tool response. Until we have
/// positive evidence the model supports a feature, leave the feature
/// off. Streaming and JSON mode stay on because every Chat-Completions
/// host implements them.
pub const CONSERVATIVE_FALLBACK_CAPABILITIES: ModelCapabilities = ModelCapabilities {
    streaming: true,
    tool_calling: false,
    json_mode: true,
    vision: false,
    response_state: false,
    reasoning_tokens: false,
    reasoning_effort: false,
    text_verbosity: false,
    prompt_caching: false,
};

/// Where the resolved `ModelCapabilities` came from.
///
/// Surfaced in `squeezy doctor` so users can see whether a provider/model
/// pair has positive capability evidence (`Bundled` / `LiveCatalog`) or is
/// running on conservative defaults (`ConservativeFallback`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilitySource {
    /// The bundled `crates/squeezy-llm/src/models.json` had an exact match
    /// for `(provider, model)`.
    Bundled,
    /// The cached `/models` catalog at
    /// `~/.squeezy/cache/models/<provider>.json` had positive evidence for
    /// the model (e.g. OpenRouter's `supported_parameters: ["tools", ...]`).
    LiveCatalog,
    /// Neither source named this model. Capabilities default to streaming
    /// + JSON mode only — tool calling, vision, reasoning are all off.
    ConservativeFallback,
}

impl CapabilitySource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bundled => "bundled",
            Self::LiveCatalog => "live_catalog",
            Self::ConservativeFallback => "conservative_fallback",
        }
    }
}

/// Capability + provenance for a `(provider, model)` pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedCapabilities {
    pub capabilities: ModelCapabilities,
    pub source: CapabilitySource,
}

/// Run the provider feature-detection handshake for `(provider, model)`.
///
/// Resolution order:
///   1. Bundled `models.json` entry (positive evidence, hand-curated).
///   2. Cached `/models` catalog entry with `supports_tools` populated
///      (positive evidence from the live aggregator).
///   3. Conservative fallback — tool calling, vision, and reasoning all
///      off — so we never silently send `tools` to a host that 4xx's or
///      drops them.
///
/// This is the entry point an OpenAI-compatible provider should call
/// before deciding whether to attach `tools` to the request body. The
/// `squeezy doctor` row uses the same function so the user sees the same
/// answer the request path sees.
pub fn resolve_capabilities(provider: &str, model: &str) -> ResolvedCapabilities {
    if let Some(info) = MODEL_REGISTRY
        .iter()
        .find(|entry| entry.provider == provider && entry.id == model)
    {
        return ResolvedCapabilities {
            capabilities: info.capabilities,
            source: CapabilitySource::Bundled,
        };
    }
    if let Some(catalog) = read_cached(provider)
        && let Some(entry) = catalog.models.iter().find(|m| m.id == model)
        && let Some(supports_tools) = entry.supports_tools
    {
        let mut capabilities = CONSERVATIVE_FALLBACK_CAPABILITIES;
        capabilities.tool_calling = supports_tools;
        return ResolvedCapabilities {
            capabilities,
            source: CapabilitySource::LiveCatalog,
        };
    }
    ResolvedCapabilities {
        capabilities: CONSERVATIVE_FALLBACK_CAPABILITIES,
        source: CapabilitySource::ConservativeFallback,
    }
}

/// Pure variant of [`resolve_capabilities`] used in tests and in code paths
/// that have already loaded a catalog (e.g. immediately after a `refresh`).
/// Identical resolution rules — bundled wins over catalog wins over
/// conservative fallback — but takes the catalog as a parameter instead of
/// reading from disk.
pub fn resolve_capabilities_with(
    provider: &str,
    model: &str,
    catalog: Option<&ModelCatalog>,
) -> ResolvedCapabilities {
    if let Some(info) = MODEL_REGISTRY
        .iter()
        .find(|entry| entry.provider == provider && entry.id == model)
    {
        return ResolvedCapabilities {
            capabilities: info.capabilities,
            source: CapabilitySource::Bundled,
        };
    }
    if let Some(catalog) = catalog
        && let Some(entry) = catalog.models.iter().find(|m| m.id == model)
        && let Some(supports_tools) = entry.supports_tools
    {
        let mut capabilities = CONSERVATIVE_FALLBACK_CAPABILITIES;
        capabilities.tool_calling = supports_tools;
        return ResolvedCapabilities {
            capabilities,
            source: CapabilitySource::LiveCatalog,
        };
    }
    ResolvedCapabilities {
        capabilities: CONSERVATIVE_FALLBACK_CAPABILITIES,
        source: CapabilitySource::ConservativeFallback,
    }
}

#[cfg(test)]
#[path = "model_discovery_tests.rs"]
mod tests;
