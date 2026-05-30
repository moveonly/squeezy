use std::collections::HashMap;
use std::env;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use squeezy_core::settings_writer::{EditOp, SettingsEdit, SettingsScope, apply_edits};
use squeezy_core::{Result, SqueezyError};
use tokio::sync::RwLock;

/// Where a resolved API key came from. Used by doctor to surface the
/// resolution source and by future migration code to act on legacy state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    /// Inline `api_key` from a TOML layer (user or project-local).
    Inline,
    /// `~/.squeezy/credentials.json` (mode 0o600), the file-based fallback
    /// for environments where the OS keyring is unavailable (locked
    /// Keychain, headless Linux CI without a secret-service daemon,
    /// Windows without the credential-manager backend enabled).
    File,
    /// Environment variable named by `api_key_env`.
    Env,
    /// Fallback env var pair (e.g. `SQUEEZY_OPENAI_KEY` ↔ `OPENAI_API_KEY`).
    FallbackEnv,
    /// `SQUEEZY_CREDENTIALS_JSON` env var carrying the whole credentials
    /// JSON document in one shot. The CI/CD injection point — a single
    /// secret in the runner config covers every provider the workflow
    /// touches.
    JsonEnv,
}

#[derive(Debug, Clone)]
pub struct ResolvedKey {
    pub value: String,
    pub source: KeySource,
}

pub fn resolve_api_key(env_var: &str) -> Result<String> {
    resolve_api_key_with_inline(None, env_var).map(|r| r.value)
}

pub fn resolve_api_key_with_inline(inline: Option<&str>, env_var: &str) -> Result<ResolvedKey> {
    // Inline TOML wins: it's how users own the credential in their own
    // settings file. After that we try the file-based fallback before
    // env vars so an explicit `credentials.json` entry can override a
    // stale exported env var. Env vars still resolve when neither
    // settings nor file are present so existing shells and CI exports
    // keep working. `SQUEEZY_CREDENTIALS_JSON` sits at the bottom of
    // the chain because it's a CI/CD broadcast channel — the user's
    // own settings, file, or named env vars should still win when the
    // CI runner has happened to inject the JSON blob too.
    if let Some(value) = inline
        && !value.trim().is_empty()
    {
        return Ok(ResolvedKey {
            value: value.to_string(),
            source: KeySource::Inline,
        });
    }
    if let Some(value) = read_credentials_file_for(env_var) {
        return Ok(ResolvedKey {
            value,
            source: KeySource::File,
        });
    }
    if let Some(value) = env_value(env_var) {
        return Ok(ResolvedKey {
            value,
            source: KeySource::Env,
        });
    }
    if let Some(fallback) = fallback_env_var(env_var)
        && let Some(value) = env_value(&fallback)
    {
        return Ok(ResolvedKey {
            value,
            source: KeySource::FallbackEnv,
        });
    }
    if let Some(value) = read_credentials_json_env_for(env_var) {
        return Ok(ResolvedKey {
            value,
            source: KeySource::JsonEnv,
        });
    }
    let fallback_note = fallback_env_var(env_var)
        .map(|name| format!(" or {name}"))
        .unwrap_or_default();
    Err(SqueezyError::ProviderNotConfigured(format!(
        "missing {env_var}{fallback_note}; \
         set the env var or add `[providers.<name>] api_key = \"…\"` \
         to ~/.squeezy/settings.toml or the project-local settings.toml"
    )))
}

fn env_value(env_var: &str) -> Option<String> {
    match env::var(env_var) {
        Ok(value) if !value.trim().is_empty() => Some(value),
        _ => None,
    }
}

/// Remove the inline `[providers.<section>] api_key = "..."` entry from
/// `scope`'s TOML file. Other fields under `[providers.<section>]` (base
/// url, headers, extra config) are preserved, and surrounding sections
/// and comments are left untouched. The committed `./squeezy.toml`
/// (`SettingsScopeKind::Project`) is refused — secrets do not belong in
/// version control and storing one there is already a misuse worth
/// surfacing to the caller.
///
/// Returns `Ok(true)` when an `api_key` field was actually removed,
/// `Ok(false)` when no inline key was present for that provider. Either
/// outcome means the on-disk file no longer carries that key.
pub fn delete_api_key(provider_section: &str, scope: &SettingsScope) -> Result<bool> {
    if provider_section.trim().is_empty() {
        return Err(SqueezyError::Config(
            "provider section must not be empty".to_string(),
        ));
    }
    if matches!(
        scope.kind,
        squeezy_core::settings_writer::SettingsScopeKind::Project
    ) {
        return Err(SqueezyError::Config(
            "refusing to edit the committed project TOML; secrets only live in user or local scope"
                .to_string(),
        ));
    }
    let edit = SettingsEdit {
        path: &[],
        op: EditOp::SetTableEntry {
            table_path: &["providers"],
            key: provider_section.to_string(),
            fields: vec![("api_key", EditOp::Unset)],
        },
    };
    let outcome = apply_edits(scope, &[edit]).map_err(|err| {
        SqueezyError::Config(format!("failed to write {}: {err}", scope.path.display()))
    })?;
    Ok(outcome.edits_applied > 0)
}

/// Translate between Squeezy's `SQUEEZY_<PROVIDER>_KEY` naming and each
/// vendor's traditional `<PROVIDER>_API_KEY` so either env var resolves
/// the same secret. Returns `None` when no translation is known.
pub fn fallback_env_var(env_var: &str) -> Option<String> {
    if let Some(stripped) = env_var.strip_prefix("SQUEEZY_")
        && let Some(provider) = stripped.strip_suffix("_KEY")
    {
        return Some(format!("{provider}_API_KEY"));
    }
    if let Some(provider) = env_var.strip_suffix("_API_KEY") {
        return Some(format!("SQUEEZY_{provider}_KEY"));
    }
    None
}

/// Path of the optional file-based credentials fallback. Honors
/// `SQUEEZY_CREDENTIALS_FILE` so tests (and unusual deployments) can
/// point this elsewhere without touching the user's real
/// `~/.squeezy/credentials.json`.
pub fn credentials_file_path() -> Option<PathBuf> {
    if let Ok(explicit) = env::var("SQUEEZY_CREDENTIALS_FILE")
        && !explicit.trim().is_empty()
    {
        return Some(PathBuf::from(explicit));
    }
    let home = dirs::home_dir()?;
    Some(home.join(".squeezy").join("credentials.json"))
}

/// Read the credentials file and return the value mapped to either
/// `env_var` or its `fallback_env_var` translation. Missing file is
/// silent; every other failure mode (bad mode bits, malformed JSON,
/// I/O error) emits a one-shot `tracing::warn!` and yields `None` so
/// resolution can keep walking the chain.
fn read_credentials_file_for(env_var: &str) -> Option<String> {
    let path = credentials_file_path()?;
    let entries = read_credentials_file(&path)?;
    if let Some(value) = entries.get(env_var)
        && !value.trim().is_empty()
    {
        return Some(value.clone());
    }
    if let Some(fallback) = fallback_env_var(env_var)
        && let Some(value) = entries.get(&fallback)
        && !value.trim().is_empty()
    {
        return Some(value.clone());
    }
    None
}

/// Parse the credentials file at `path`. Returns `None` when the file
/// doesn't exist; warns once and returns `None` on every other failure
/// (mode, JSON, I/O).
fn read_credentials_file(path: &Path) -> Option<HashMap<String, String>> {
    match std::fs::metadata(path) {
        Ok(meta) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = meta.permissions().mode() & 0o777;
                // Reject any file whose mode lets group or world read.
                // Keys are too sensitive to fall back through a 0o644.
                if mode & 0o077 != 0 {
                    warn_once(
                        path,
                        format!(
                            "credentials file {} has permissions {mode:o}; \
                             refusing to read (chmod 600 to enable)",
                            path.display()
                        ),
                    );
                    return None;
                }
            }
            // Suppress unused warning on non-unix without splitting the
            // function into two cfg-gated bodies.
            let _ = meta;
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            warn_once(
                path,
                format!(
                    "credentials file {} could not be opened: {err}",
                    path.display()
                ),
            );
            return None;
        }
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(err) => {
            warn_once(
                path,
                format!("credentials file {} read failed: {err}", path.display()),
            );
            return None;
        }
    };
    match serde_json::from_str::<HashMap<String, String>>(&text) {
        Ok(map) => Some(map),
        Err(err) => {
            warn_once(
                path,
                format!(
                    "credentials file {} is not a flat {{ \"ENV_VAR\": \"value\" }} JSON object: {err}",
                    path.display()
                ),
            );
            None
        }
    }
}

/// Emit a `tracing::warn!` the first time a given credentials-file path
/// fails to load. Without the suppression a single bad file would warn
/// once per provider build on every turn.
fn warn_once(path: &Path, msg: String) {
    static GUARD: OnceLock<std::sync::Mutex<std::collections::HashSet<PathBuf>>> = OnceLock::new();
    let set = GUARD.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()));
    let mut set = match set.lock() {
        Ok(g) => g,
        // A poisoned mutex still has a valid set inside; we don't care
        // about the lock state for this once-per-path log.
        Err(poison) => poison.into_inner(),
    };
    if set.insert(path.to_path_buf()) {
        tracing::warn!("{msg}");
    }
}

/// CI/CD injection point: the whole credentials document carried in a
/// single env var. Equivalent to `~/.squeezy/credentials.json` but
/// avoids writing a file to the runner's home directory. Two shapes
/// are accepted so authors can pick whichever fits their secret store:
///
/// 1. Provider-keyed (preferred): `{"providers":{"openai":"sk-…",
///    "anthropic":"sk-…"}}`. The provider name is derived from the
///    requested env var by stripping the `SQUEEZY_…_KEY` or
///    `…_API_KEY` framing and lower-casing the remainder.
/// 2. Flat env-var-keyed: `{"OPENAI_API_KEY":"sk-…",
///    "SQUEEZY_ANTHROPIC_KEY":"sk-…"}`. Mirrors the on-disk
///    `credentials.json` schema so a workflow can `cat` the file into
///    the env var without restructuring.
fn read_credentials_json_env_for(env_var: &str) -> Option<String> {
    let raw = env::var("SQUEEZY_CREDENTIALS_JSON").ok()?;
    if raw.trim().is_empty() {
        return None;
    }
    let value: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(err) => {
            warn_json_env_once(format!("SQUEEZY_CREDENTIALS_JSON is not valid JSON: {err}"));
            return None;
        }
    };
    let object = match value.as_object() {
        Some(map) => map,
        None => {
            warn_json_env_once(
                "SQUEEZY_CREDENTIALS_JSON must be a JSON object at the top level".to_string(),
            );
            return None;
        }
    };

    // Provider-keyed shape: `{"providers": {...}}`. Take the first hit
    // among (canonical env var → provider name) and (fallback env var
    // → provider name) so either naming style resolves the same blob.
    if let Some(providers) = object.get("providers").and_then(|v| v.as_object()) {
        let candidates = [
            provider_section_for_env(env_var),
            fallback_env_var(env_var).and_then(|name| provider_section_for_env(&name)),
        ];
        for candidate in candidates.iter().flatten() {
            if let Some(value) = providers.get(candidate).and_then(|v| v.as_str())
                && !value.trim().is_empty()
            {
                return Some(value.to_string());
            }
        }
    }

    // Flat env-var-keyed shape: matches the on-disk `credentials.json`
    // schema. Tried after the nested shape so a workflow that mixes
    // both still resolves predictably.
    if let Some(value) = object.get(env_var).and_then(|v| v.as_str())
        && !value.trim().is_empty()
    {
        return Some(value.to_string());
    }
    if let Some(fallback) = fallback_env_var(env_var)
        && let Some(value) = object.get(&fallback).and_then(|v| v.as_str())
        && !value.trim().is_empty()
    {
        return Some(value.to_string());
    }
    None
}

/// Derive the `providers.<name>` lookup key from a credential env var
/// name. `SQUEEZY_OPENAI_KEY` → `openai`, `ANTHROPIC_API_KEY` →
/// `anthropic`. Returns `None` for env var names that don't match
/// either framing — those callers must use the flat env-var-keyed
/// shape.
fn provider_section_for_env(env_var: &str) -> Option<String> {
    if let Some(stripped) = env_var.strip_prefix("SQUEEZY_")
        && let Some(provider) = stripped.strip_suffix("_KEY")
        && !provider.is_empty()
    {
        return Some(provider.to_ascii_lowercase());
    }
    if let Some(provider) = env_var.strip_suffix("_API_KEY")
        && !provider.is_empty()
    {
        return Some(provider.to_ascii_lowercase());
    }
    None
}

/// One-shot warn for `SQUEEZY_CREDENTIALS_JSON` parse failures. Same
/// suppression contract as `warn_once` but keyed on the message so
/// distinct failure modes (bad JSON, wrong top-level shape) each
/// surface once.
fn warn_json_env_once(msg: String) {
    static GUARD: OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    let set = GUARD.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()));
    let mut set = match set.lock() {
        Ok(g) => g,
        Err(poison) => poison.into_inner(),
    };
    if set.insert(msg.clone()) {
        tracing::warn!("{msg}");
    }
}

/// Future returned by [`ApiKeySource`] methods. Boxed + pinned so the
/// trait stays dyn-compatible (`Arc<dyn ApiKeySource>`); a same-shape
/// alias to keep the trait signature compact and the call sites
/// type-inferred.
pub type ApiKeyFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// Pluggable supplier of an API key for an LLM provider client.
///
/// Provider clients hold an `Arc<dyn ApiKeySource>` instead of a bare
/// `String` so a session that started with a short-lived token can
/// continue past the original expiry: the auth-retry layer in
/// [`crate::retry::send_with_auth_retry`] asks the source for the
/// current key per HTTP attempt and, on a `401`/`403`, calls
/// [`ApiKeySource::invalidate`] and retries once with a fresh value.
///
/// The default [`StaticApiKey`] implementation just hands out a
/// constant string and treats `invalidate` as a no-op; this is the
/// path every existing provider config (`api_key = "sk-…"`,
/// `OPENAI_API_KEY`, the `credentials.json` fallback) takes today.
/// [`RefreshableToken`] is the seam the OAuth subscription providers
/// (Claude Pro/Max, ChatGPT Plus/Pro, GitHub Copilot) will fill in:
/// it carries a rotating access token under an
/// `Arc<RwLock<TokenState>>` so refresh runs without rebuilding the
/// provider client.
pub trait ApiKeySource: Send + Sync + std::fmt::Debug {
    /// Resolve the API key to attach to the next HTTP request.
    fn current_key<'a>(&'a self) -> ApiKeyFuture<'a, String>;

    /// Mark the cached key as stale. Called by the auth-retry layer
    /// after the provider reported `401`/`403`. Static sources ignore
    /// this; OAuth sources clear their cached access token so the
    /// next [`current_key`] re-runs the refresh flow.
    fn invalidate<'a>(&'a self) -> ApiKeyFuture<'a, ()>;

    /// Short label used in logs and `Debug` output. Mirrors the
    /// `providers.<section>` key (e.g. `"anthropic"`, `"openai"`).
    fn provider_label(&self) -> &str;
}

/// A fixed API key that never refreshes. The path every existing
/// settings/env/credentials.json caller flows through — wraps the
/// resolved string so the provider clients can store a single
/// `Arc<dyn ApiKeySource>` shape.
#[derive(Debug, Clone)]
pub struct StaticApiKey {
    value: String,
    label: String,
}

impl StaticApiKey {
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
        }
    }

    pub fn into_source(self) -> Arc<dyn ApiKeySource> {
        Arc::new(self)
    }
}

impl ApiKeySource for StaticApiKey {
    fn current_key<'a>(&'a self) -> ApiKeyFuture<'a, String> {
        let value = self.value.clone();
        Box::pin(async move { Ok(value) })
    }

    fn invalidate<'a>(&'a self) -> ApiKeyFuture<'a, ()> {
        Box::pin(async move { Ok(()) })
    }

    fn provider_label(&self) -> &str {
        &self.label
    }
}

/// Wrap a resolved key string in an [`ApiKeySource`] trait object.
/// Used by every `from_config` constructor so the existing string-
/// based credential resolution keeps working unchanged.
pub fn static_api_key_source(value: String, label: impl Into<String>) -> Arc<dyn ApiKeySource> {
    Arc::new(StaticApiKey::new(value, label))
}

impl From<String> for StaticApiKey {
    fn from(value: String) -> Self {
        Self {
            value,
            label: String::new(),
        }
    }
}

/// Mutable OAuth credential state: an access token used directly on
/// the wire, an optional refresh token consumed by the provider's
/// refresh endpoint, and an optional absolute expiry the refresh flow
/// uses to decide whether the cached access token is still good.
///
/// The shape is intentionally minimal so Anthropic Pro/Max and ChatGPT
/// Plus/Pro implementations have a natural place to land the
/// device-code outputs. Additional provider-specific fields (account
/// id, scope set, etc.) can ride on the implementing OAuth source
/// rather than expanding this struct.
#[derive(Debug, Clone)]
pub struct TokenState {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<SystemTime>,
}

impl TokenState {
    pub fn new(access_token: impl Into<String>) -> Self {
        Self {
            access_token: access_token.into(),
            refresh_token: None,
            expires_at: None,
        }
    }
}

/// Placeholder OAuth-backed [`ApiKeySource`]. Holds the rotating
/// token state under an `Arc<RwLock<_>>` so a future OAuth refresh
/// implementation can swap the access token in place without
/// rebuilding the provider client. The current behavior is a no-op:
/// `current_key` returns the cached access token and `invalidate` is
/// a no-op so the trait stays compatible with provider client retry
/// hooks landing in the same commit.
///
/// The actual refresh flow (Anthropic device-code, OpenAI Codex
/// device-code, GitHub Copilot device-code, refresh-on-expiry) lands
/// in the OAuth subscription findings; this type only establishes
/// the indirection.
pub struct RefreshableToken {
    state: Arc<RwLock<TokenState>>,
    label: String,
}

impl RefreshableToken {
    pub fn new(state: TokenState, label: impl Into<String>) -> Self {
        Self {
            state: Arc::new(RwLock::new(state)),
            label: label.into(),
        }
    }

    /// Shared handle to the underlying token state. The OAuth refresh
    /// implementation needs this to swap the access token in place
    /// after a successful refresh round-trip.
    pub fn state_handle(&self) -> Arc<RwLock<TokenState>> {
        self.state.clone()
    }
}

impl std::fmt::Debug for RefreshableToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RefreshableToken")
            .field("label", &self.label)
            .field("state", &"<redacted>")
            .finish()
    }
}

impl ApiKeySource for RefreshableToken {
    fn current_key<'a>(&'a self) -> ApiKeyFuture<'a, String> {
        let state = self.state.clone();
        Box::pin(async move {
            let guard = state.read().await;
            Ok(guard.access_token.clone())
        })
    }

    fn invalidate<'a>(&'a self) -> ApiKeyFuture<'a, ()> {
        // OAuth refresh lands in F16pi-anthropic-oauth /
        // F16pi-openai-codex / F16pi-github-copilot. Until then the
        // upstream `401`/`403` propagates: the auth-retry layer calls
        // `invalidate` (no-op here), re-reads the unchanged access
        // token, retries once, and bubbles up the second `401` so
        // the user sees the auth failure instead of a silent retry
        // loop.
        Box::pin(async move { Ok(()) })
    }

    fn provider_label(&self) -> &str {
        &self.label
    }
}

#[cfg(test)]
#[path = "credentials_tests.rs"]
mod tests;
