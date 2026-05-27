use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use squeezy_core::settings_writer::{EditOp, SettingsEdit, SettingsScope, apply_edits};
use squeezy_core::{Result, SqueezyError};

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
    // keep working.
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

#[cfg(test)]
#[path = "credentials_tests.rs"]
mod tests;
