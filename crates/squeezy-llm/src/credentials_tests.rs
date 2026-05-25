use std::{
    collections::BTreeMap,
    sync::{Mutex, PoisonError},
};

use super::*;

#[derive(Debug, Default)]
struct MockCredentialStore {
    values: Mutex<BTreeMap<String, String>>,
}

impl KeyringCredentialStore for MockCredentialStore {
    fn load(&self, account: &str) -> std::result::Result<Option<String>, String> {
        Ok(self
            .values
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(account)
            .cloned())
    }

    fn save(&self, account: &str, value: &str) -> std::result::Result<(), String> {
        self.values
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(account.to_string(), value.to_string());
        Ok(())
    }
}

#[test]
fn resolves_from_env_first_then_keyring() {
    let store = MockCredentialStore::default();
    save_api_key_with_store("SQUEEZY_TEST_API_KEY", "keyring-value", &store).expect("save");

    unsafe {
        std::env::set_var("SQUEEZY_TEST_API_KEY", "env-value");
    }
    assert_eq!(
        resolve_api_key_with_store("SQUEEZY_TEST_API_KEY", &store).expect("env"),
        "env-value"
    );

    unsafe {
        std::env::remove_var("SQUEEZY_TEST_API_KEY");
    }
    assert_eq!(
        resolve_api_key_with_store("SQUEEZY_TEST_API_KEY", &store).expect("keyring"),
        "keyring-value"
    );
}

#[test]
fn missing_key_mentions_env_and_keyring_service() {
    let error =
        resolve_api_key_with_store("SQUEEZY_TEST_MISSING_KEY", &MockCredentialStore::default())
            .expect_err("missing");
    let message = error.to_string();
    assert!(message.contains("SQUEEZY_TEST_MISSING_KEY"), "{message}");
    assert!(message.contains(KEYRING_SERVICE), "{message}");
}
