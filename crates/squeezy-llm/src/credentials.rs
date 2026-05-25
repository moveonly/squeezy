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
        entry.set_password(value).map_err(|error| error.to_string())
    }
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
    if let Ok(value) = env::var(env_var)
        && !value.trim().is_empty()
    {
        return Ok(value);
    }
    match store.load(env_var) {
        Ok(Some(value)) if !value.trim().is_empty() => Ok(value),
        Ok(_) => Err(SqueezyError::ProviderNotConfigured(format!(
            "missing {env_var}; also checked keyring service {KEYRING_SERVICE} account {env_var}"
        ))),
        Err(error) => Err(SqueezyError::ProviderNotConfigured(format!(
            "missing {env_var}; keyring service {KEYRING_SERVICE} account {env_var} failed: {error}"
        ))),
    }
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
