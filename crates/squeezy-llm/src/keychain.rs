use squeezy_core::{Result, SqueezyError};

pub(crate) fn resolve_api_key(
    env_name: &str,
    keychain_service: Option<&str>,
    account: &str,
) -> Result<String> {
    if let Ok(value) = std::env::var(env_name)
        && !value.is_empty()
    {
        return Ok(value);
    }
    let Some(service) = keychain_service else {
        return Err(SqueezyError::ProviderNotConfigured(format!(
            "missing {env_name}"
        )));
    };
    read_keychain_password(service, account).map_err(|err| {
        SqueezyError::ProviderNotConfigured(format!(
            "missing {env_name}; keychain {service}/{account}: {err}"
        ))
    })
}

#[cfg(target_os = "macos")]
fn read_keychain_password(service: &str, account: &str) -> std::result::Result<String, String> {
    let password = security_framework::passwords::get_generic_password(service, account)
        .map_err(|err| err.to_string())?;
    String::from_utf8(password).map_err(|err| err.to_string())
}

#[cfg(target_os = "windows")]
fn read_keychain_password(service: &str, account: &str) -> std::result::Result<String, String> {
    let entry = keyring::Entry::new(service, account).map_err(|err| err.to_string())?;
    entry.get_password().map_err(|err| err.to_string())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn read_keychain_password(_service: &str, _account: &str) -> std::result::Result<String, String> {
    Err("keychain fallback is only available on macOS and Windows".to_string())
}

#[cfg(test)]
#[path = "keychain_tests.rs"]
mod tests;
