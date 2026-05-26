use std::{env, fmt::Debug};

use keyring::Entry;
use squeezy_core::{Result, SqueezyError};

pub const KEYRING_SERVICE: &str = "dev.squeezy.providers";

pub trait KeyringCredentialStore: Debug + Send + Sync {
    fn load(&self, account: &str) -> std::result::Result<Option<String>, String>;
    fn save(&self, account: &str, value: &str) -> std::result::Result<(), String>;
}

#[derive(Debug, Clone, Copy)]
pub struct DefaultCredentialStore;

impl KeyringCredentialStore for DefaultCredentialStore {
    fn load(&self, account: &str) -> std::result::Result<Option<String>, String> {
        let entry = Entry::new(KEYRING_SERVICE, account).map_err(|error| error.to_string())?;
        match entry.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    fn save(&self, account: &str, value: &str) -> std::result::Result<(), String> {
        let entry = Entry::new(KEYRING_SERVICE, account).map_err(|error| error.to_string())?;
        match entry.set_password(value) {
            Ok(()) => Ok(()),
            Err(first_err) if is_duplicate_item_error(&first_err) => {
                // The macOS keychain backend surfaces `errSecDuplicateItem`
                // when an existing generic-password item was created with
                // attributes (label, access group) that don't line up with
                // what `keyring` writes, so `SecItemAdd` fires instead of
                // an in-place update. Delete the stale item and retry once.
                if let Err(delete_err) = entry.delete_credential()
                    && !matches!(delete_err, keyring::Error::NoEntry)
                {
                    return Err(format!(
                        "{first_err}; cleanup before retry failed: {delete_err}"
                    ));
                }
                let entry =
                    Entry::new(KEYRING_SERVICE, account).map_err(|error| error.to_string())?;
                entry
                    .set_password(value)
                    .map_err(|retry_err| format!("retry after delete failed: {retry_err}"))
            }
            Err(error) => Err(error.to_string()),
        }
    }
}

fn is_duplicate_item_error(err: &keyring::Error) -> bool {
    // The keyring crate doesn't expose a dedicated variant for
    // errSecDuplicateItem on macOS, so fall back to a substring match on
    // the platform-supplied message — the apple-native backend always
    // includes "already exists in the keychain" for code -25299.
    let lowered = err.to_string().to_ascii_lowercase();
    lowered.contains("already exists")
}

pub fn resolve_api_key(env_var: &str) -> Result<String> {
    resolve_api_key_with_store(env_var, &DefaultCredentialStore)
}

pub fn save_api_key(env_var: &str, value: &str) -> Result<()> {
    save_api_key_with_store(env_var, value, &DefaultCredentialStore)
}

pub fn resolve_api_key_with_store(
    env_var: &str,
    store: &dyn KeyringCredentialStore,
) -> Result<String> {
    // Try the configured env var first (env then keychain), then the
    // common-vendor fallback (e.g. SQUEEZY_OPENAI_KEY → OPENAI_API_KEY)
    // so users who only have the upstream-vendor name set still work.
    if let Some(value) = lookup_env_or_keychain(env_var, store)? {
        return Ok(value);
    }
    if let Some(fallback) = fallback_env_var(env_var)
        && let Some(value) = lookup_env_or_keychain(&fallback, store)?
    {
        return Ok(value);
    }
    let fallback_note = fallback_env_var(env_var)
        .map(|name| format!(" or {name}"))
        .unwrap_or_default();
    Err(SqueezyError::ProviderNotConfigured(format!(
        "missing {env_var}{fallback_note}; \
         also checked keyring service {KEYRING_SERVICE} account {env_var}"
    )))
}

fn lookup_env_or_keychain(
    env_var: &str,
    store: &dyn KeyringCredentialStore,
) -> Result<Option<String>> {
    if let Ok(value) = env::var(env_var)
        && !value.trim().is_empty()
    {
        return Ok(Some(value));
    }
    match store.load(env_var) {
        Ok(Some(value)) if !value.trim().is_empty() => Ok(Some(value)),
        Ok(_) => Ok(None),
        Err(error) => Err(SqueezyError::ProviderNotConfigured(format!(
            "keyring service {KEYRING_SERVICE} account {env_var} failed: {error}"
        ))),
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

pub fn save_api_key_with_store(
    env_var: &str,
    value: &str,
    store: &dyn KeyringCredentialStore,
) -> Result<()> {
    if value.trim().is_empty() {
        return Err(SqueezyError::Config(
            "API key must not be empty".to_string(),
        ));
    }
    store.save(env_var, value).map_err(|error| {
        SqueezyError::Config(format!("failed to save {env_var} to keyring: {error}"))
    })
}

#[cfg(test)]
#[path = "credentials_tests.rs"]
mod tests;
