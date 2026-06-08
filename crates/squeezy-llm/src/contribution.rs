//! User-extension surface for adding LLM providers via typed TOML config.
//!
//! [`ProviderContribution`] is the user-facing trait: ship a Rust crate
//! that defines a `Config: DeserializeOwned`, a stable id matching the
//! TOML section name, and a factory that constructs a `Box<dyn
//! LlmProvider>`. Register the contribution into a
//! [`ProviderContributions`] registry and the loader will pick up the
//! corresponding `[providers.<id>]` block from
//! `~/.squeezy/settings.toml`.
//!
//! The surface is **additive**: built-in providers still resolve through
//! [`crate::provider_from_config`] and the existing
//! `[model] + [providers.<preset>]` schema. The wrappers below
//! ([`OpenAiContribution`], [`AnthropicContribution`],
//! [`GoogleContribution`], [`OllamaContribution`]) demonstrate that the
//! trait is expressive enough to cover the built-ins; they are not
//! registered automatically because the built-in resolver already owns
//! that schema. A user that wants the contribution surface to drive a
//! built-in can register the wrapper explicitly.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::Deserialize;
use serde::de::DeserializeOwned;
use squeezy_core::{
    AnthropicConfig, DEFAULT_ANTHROPIC_BASE_URL, DEFAULT_GOOGLE_BASE_URL, DEFAULT_OLLAMA_BASE_URL,
    DEFAULT_OPENAI_BASE_URL, GoogleConfig, OllamaConfig, OllamaRoute, OpenAiConfig,
    ProviderTransportConfig, Result, SqueezyError,
};

use crate::{AnthropicProvider, GoogleProvider, LlmProvider, OllamaProvider, OpenAiProvider};

/// User-extension trait for shipping a new LLM provider via TOML config
/// without modifying squeezy-llm.
///
/// Implementations declare a stable `id` (matching the TOML section name
/// `[providers.<id>]`) and a typed `Config` schema. The factory [`build`]
/// consumes the deserialized config and returns a `Box<dyn LlmProvider>`
/// that the rest of the agent uses through the existing
/// [`LlmProvider`](crate::LlmProvider) trait.
///
/// Trait-shaped instead of a closure pair so a third-party crate can
/// implement the contribution with a normal `struct + impl` block.
///
/// [`build`]: ProviderContribution::build
pub trait ProviderContribution: Send + Sync + 'static {
    /// Typed payload deserialized from `[providers.<id>]`. Must be
    /// `DeserializeOwned` so the registry can decode an owned
    /// [`toml::Value`] into it without holding a borrow on the source
    /// TOML text.
    type Config: DeserializeOwned + 'static;

    /// Stable identifier matching `[providers.<id>]`. Pick a fresh id (no
    /// dots, no spaces) when adding a new provider; reuse a built-in id
    /// (`"openai"`, `"anthropic"`, …) only when the wrapper deliberately
    /// replaces the built-in schema at the call site.
    fn id() -> &'static str
    where
        Self: Sized;

    /// Construct a provider instance from the deserialized config. The
    /// returned box is type-erased into `Arc<dyn LlmProvider>` inside the
    /// registry so callers can plumb it through the same code path as
    /// the built-in providers.
    fn build(config: Self::Config) -> Result<Box<dyn LlmProvider>>
    where
        Self: Sized;
}

type ErasedBuild = Arc<dyn Fn(toml::Value) -> Result<Arc<dyn LlmProvider>> + Send + Sync + 'static>;

/// Registry mapping `[providers.<id>]` section names to factory closures
/// that decode the TOML payload and construct a provider instance.
///
/// The registry is intentionally lean. Built-in providers do **not**
/// auto-register here — they continue to flow through
/// [`provider_from_config`](crate::provider_from_config). This surface
/// exists for user-supplied contributions and for callers that want a
/// uniform "load everything from `settings.toml`" entry point.
#[derive(Clone, Default)]
pub struct ProviderContributions {
    entries: BTreeMap<&'static str, ErasedBuild>,
}

impl std::fmt::Debug for ProviderContributions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderContributions")
            .field("ids", &self.entries.keys().copied().collect::<Vec<_>>())
            .finish()
    }
}

impl ProviderContributions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a contribution type. Returns `&mut Self` for builder-style
    /// chaining. Panics if another contribution with the same `id()` is
    /// already registered: duplicate ids would otherwise create a silent
    /// ambiguity in TOML resolution.
    pub fn register<C: ProviderContribution>(&mut self) -> &mut Self {
        let id = C::id();
        let build: ErasedBuild = Arc::new(move |value: toml::Value| {
            let config: C::Config = value.try_into().map_err(|err: toml::de::Error| {
                SqueezyError::Config(format!("providers.{id}: {err}"))
            })?;
            let provider = C::build(config)?;
            Ok(Arc::<dyn LlmProvider>::from(provider))
        });
        assert!(
            self.entries.insert(id, build).is_none(),
            "ProviderContribution {id:?} already registered"
        );
        self
    }

    /// Returns the list of registered contribution ids in lexicographic
    /// order (the registry uses a `BTreeMap`). Useful for surfacing the
    /// extension surface in error messages and `squeezy doctor`-style
    /// diagnostics.
    pub fn ids(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.entries.keys().copied()
    }

    pub fn contains(&self, id: &str) -> bool {
        self.entries.contains_key(id)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Build providers from an in-memory TOML root table. Walks the
    /// `[providers]` table, matches each entry against registered
    /// contributions, and produces `(id, provider)` pairs for the ones
    /// that resolved. Unregistered ids are returned via
    /// [`LoadedContributions::unhandled`] so the caller can fall back to
    /// the built-in resolver.
    pub fn build_from_root(&self, root: &toml::Value) -> Result<LoadedContributions> {
        let mut loaded = LoadedContributions::default();
        let Some(table) = root.get("providers").and_then(|v| v.as_table()) else {
            return Ok(loaded);
        };
        for (id, value) in table {
            match self.entries.get(id.as_str()) {
                Some(build) => {
                    let provider = (build)(value.clone())?;
                    loaded.providers.push((id.clone(), provider));
                }
                None => loaded.unhandled.push(id.clone()),
            }
        }
        Ok(loaded)
    }

    /// Parse a TOML string and build all registered contributions found
    /// in it. The string is expected to be a full settings file (i.e.
    /// the `[providers.<id>]` sections appear under a top-level
    /// `providers` table).
    pub fn build_from_toml_str(&self, toml_text: &str) -> Result<LoadedContributions> {
        let root: toml::Value = toml::from_str(toml_text)
            .map_err(|err| SqueezyError::Config(format!("failed to parse settings TOML: {err}")))?;
        self.build_from_root(&root)
    }

    /// Read a settings file at `path` and build registered contributions.
    /// A missing file returns an empty [`LoadedContributions`] — that
    /// matches the rest of squeezy-core's "absent settings = defaults"
    /// behaviour and lets callers chain user / project / repo paths
    /// without pre-checking `is_file`.
    pub fn build_from_path(&self, path: &Path) -> Result<LoadedContributions> {
        match std::fs::read_to_string(path) {
            Ok(text) => self
                .build_from_toml_str(&text)
                .map_err(|err| annotate_with_path(err, path)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                Ok(LoadedContributions::default())
            }
            Err(err) => Err(SqueezyError::Io(err)),
        }
    }

    /// Convenience wrapper around [`build_from_path`] targeting the
    /// user-level settings file (`~/.squeezy/settings.toml` on Unix).
    /// The resolved path is recorded in
    /// [`LoadedContributions::source_path`] so callers can plumb a
    /// helpful "loaded from …" string into telemetry.
    ///
    /// [`build_from_path`]: ProviderContributions::build_from_path
    pub fn build_from_default_settings(&self) -> Result<LoadedContributions> {
        let path = squeezy_core::default_settings_path();
        let mut loaded = self.build_from_path(&path)?;
        loaded.source_path = Some(path);
        Ok(loaded)
    }
}

fn annotate_with_path(err: SqueezyError, path: &Path) -> SqueezyError {
    match err {
        SqueezyError::Config(detail) => {
            SqueezyError::Config(format!("{}: {detail}", path.display()))
        }
        other => other,
    }
}

/// Result of loading provider contributions from a TOML payload.
///
/// `LlmProvider` is not `Debug`, so the manual `Debug` impl below
/// reports the `(id, provider_name)` pairs instead of the full provider
/// state — enough to confirm wiring in tests without leaking
/// credential-bearing fields into logs.
#[derive(Default, Clone)]
pub struct LoadedContributions {
    /// Successfully constructed providers, in TOML iteration order.
    pub providers: Vec<(String, Arc<dyn LlmProvider>)>,
    /// `[providers.<id>]` section ids that did not match a registered
    /// contribution. The caller can fall back to the built-in resolver
    /// for these (most of squeezy's bundled providers land here when
    /// the user hasn't registered the matching wrapper).
    pub unhandled: Vec<String>,
    /// Filesystem path the TOML was read from, when available. `None`
    /// for the in-memory string variants.
    pub source_path: Option<PathBuf>,
}

impl std::fmt::Debug for LoadedContributions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let providers: Vec<(&str, &'static str)> = self
            .providers
            .iter()
            .map(|(id, provider)| (id.as_str(), provider.name()))
            .collect();
        f.debug_struct("LoadedContributions")
            .field("providers", &providers)
            .field("unhandled", &self.unhandled)
            .field("source_path", &self.source_path)
            .finish()
    }
}

// ----- Built-in thin wrappers -----------------------------------------
//
// These exist to satisfy the "trait surface is additive, not a rewrite"
// invariant: the existing built-ins keep flowing through
// `provider_from_config`, and these wrappers prove the trait can express
// the same construction shape. Users who want the contribution path to
// drive a built-in register the wrapper explicitly:
//
// ```
// let mut contributions = ProviderContributions::new();
// contributions.register::<OpenAiContribution>();
// let loaded = contributions.build_from_default_settings()?;
// ```

/// Typed TOML payload for [`OpenAiContribution`].
///
/// Mirrors the fields of [`OpenAiConfig`] but tolerates missing optional
/// fields (`api_key`, `transport`) so a user can write a minimal
/// `[providers.openai]` block with just `api_key_env = "…"`.
#[derive(Debug, Clone, Deserialize)]
pub struct OpenAiContributionConfig {
    pub api_key_env: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default = "default_openai_base_url")]
    pub base_url: String,
    #[serde(default)]
    pub transport: ProviderTransportConfig,
}

fn default_openai_base_url() -> String {
    DEFAULT_OPENAI_BASE_URL.to_string()
}

pub struct OpenAiContribution;

impl ProviderContribution for OpenAiContribution {
    type Config = OpenAiContributionConfig;

    fn id() -> &'static str {
        "openai"
    }

    fn build(config: OpenAiContributionConfig) -> Result<Box<dyn LlmProvider>> {
        let core = OpenAiConfig {
            api_key_env: config.api_key_env,
            api_key: config.api_key,
            base_url: config.base_url,
            organization: None,
            project: None,
            service_tier: None,
            transport: config.transport,
        };
        Ok(Box::new(OpenAiProvider::from_config(&core)?))
    }
}

/// Typed TOML payload for [`AnthropicContribution`].
#[derive(Debug, Clone, Deserialize)]
pub struct AnthropicContributionConfig {
    pub api_key_env: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default = "default_anthropic_base_url")]
    pub base_url: String,
    #[serde(default)]
    pub transport: ProviderTransportConfig,
}

fn default_anthropic_base_url() -> String {
    DEFAULT_ANTHROPIC_BASE_URL.to_string()
}

pub struct AnthropicContribution;

impl ProviderContribution for AnthropicContribution {
    type Config = AnthropicContributionConfig;

    fn id() -> &'static str {
        "anthropic"
    }

    fn build(config: AnthropicContributionConfig) -> Result<Box<dyn LlmProvider>> {
        let core = AnthropicConfig {
            api_key_env: config.api_key_env,
            api_key: config.api_key,
            base_url: config.base_url,
            transport: config.transport,
        };
        Ok(Box::new(AnthropicProvider::from_config(&core)?))
    }
}

/// Typed TOML payload for [`GoogleContribution`].
#[derive(Debug, Clone, Deserialize)]
pub struct GoogleContributionConfig {
    pub api_key_env: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default = "default_google_base_url")]
    pub base_url: String,
    #[serde(default)]
    pub transport: ProviderTransportConfig,
}

fn default_google_base_url() -> String {
    DEFAULT_GOOGLE_BASE_URL.to_string()
}

pub struct GoogleContribution;

impl ProviderContribution for GoogleContribution {
    type Config = GoogleContributionConfig;

    fn id() -> &'static str {
        "google"
    }

    fn build(config: GoogleContributionConfig) -> Result<Box<dyn LlmProvider>> {
        let core = GoogleConfig {
            api_key_env: config.api_key_env,
            api_key: config.api_key,
            base_url: config.base_url,
            transport: config.transport,
        };
        Ok(Box::new(GoogleProvider::from_config(&core)?))
    }
}

/// Typed TOML payload for [`OllamaContribution`].
///
/// `route_style` is accepted as a lowercase string (`"native"`,
/// `"openai_compatible"`, …) and parsed via [`OllamaRoute::parse`] so the
/// user-facing TOML matches the existing settings.toml dialect.
///
/// `api_key_env` / `api_key` / `keep_alive` map 1:1 to `OllamaConfig` and
/// let users configure Ollama Cloud / reverse-proxy auth and the idle model
/// retention window through the same contribution surface.
#[derive(Debug, Clone, Deserialize)]
pub struct OllamaContributionConfig {
    #[serde(default = "default_ollama_base_url")]
    pub base_url: String,
    #[serde(default)]
    pub route_style: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub keep_alive: Option<String>,
    #[serde(default)]
    pub transport: ProviderTransportConfig,
}

fn default_ollama_base_url() -> String {
    DEFAULT_OLLAMA_BASE_URL.to_string()
}

pub struct OllamaContribution;

impl ProviderContribution for OllamaContribution {
    type Config = OllamaContributionConfig;

    fn id() -> &'static str {
        "ollama"
    }

    fn build(config: OllamaContributionConfig) -> Result<Box<dyn LlmProvider>> {
        let route_style = config
            .route_style
            .as_deref()
            .map(|value| {
                OllamaRoute::parse(value).ok_or_else(|| {
                    SqueezyError::Config(format!(
                        "providers.ollama.route_style: unknown route {value:?} \
                         (expected `native` or `openai_compatible`)"
                    ))
                })
            })
            .transpose()?
            .unwrap_or_default();
        let core = OllamaConfig {
            base_url: config.base_url,
            route_style,
            api_key_env: config
                .api_key_env
                .unwrap_or_else(|| "OLLAMA_API_KEY".to_string()),
            api_key: config.api_key,
            keep_alive: config.keep_alive,
            transport: config.transport,
        };
        Ok(Box::new(OllamaProvider::from_config(&core)))
    }
}

#[cfg(test)]
#[path = "contribution_tests.rs"]
mod tests;
