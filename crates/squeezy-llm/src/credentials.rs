use std::env;

use squeezy_core::{Result, SqueezyError};

/// Where a resolved API key came from. Used by doctor to surface the
/// resolution source and by future migration code to act on legacy state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    /// Inline `api_key` from a TOML layer (user or project-local).
    Inline,
    /// Environment variable named by `api_key_env`.
    Env,
    /// Fallback env var pair (e.g. `SQUEEZY_OPENAI_KEY` ↔ `OPENAI_API_KEY`).
    FallbackEnv,
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
    // settings file. Env vars are still honored so existing shells and
    // CI exports keep working.
    if let Some(value) = inline
        && !value.trim().is_empty()
    {
        return Ok(ResolvedKey {
            value: value.to_string(),
            source: KeySource::Inline,
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

#[cfg(test)]
#[path = "credentials_tests.rs"]
mod tests;
