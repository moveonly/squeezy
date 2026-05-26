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

#[test]
fn inline_value_beats_env_and_keychain() {
    let store = MockCredentialStore::default();
    save_api_key_with_store("SQUEEZY_TEST_INLINE_KEY", "keychain-loser", &store).expect("save");
    unsafe {
        std::env::set_var("SQUEEZY_TEST_INLINE_KEY", "env-loser");
    }
    let resolved = resolve_api_key_with_inline_and_store(
        Some("inline-winner"),
        "SQUEEZY_TEST_INLINE_KEY",
        &store,
    )
    .expect("inline wins");
    unsafe {
        std::env::remove_var("SQUEEZY_TEST_INLINE_KEY");
    }
    assert_eq!(resolved.value, "inline-winner");
    assert_eq!(resolved.source, KeySource::Inline);
}

#[test]
fn empty_inline_falls_through_to_env() {
    let store = MockCredentialStore::default();
    unsafe {
        std::env::set_var("SQUEEZY_TEST_EMPTY_INLINE_KEY", "env-fallback");
    }
    let resolved =
        resolve_api_key_with_inline_and_store(Some("   "), "SQUEEZY_TEST_EMPTY_INLINE_KEY", &store)
            .expect("env fallback");
    unsafe {
        std::env::remove_var("SQUEEZY_TEST_EMPTY_INLINE_KEY");
    }
    assert_eq!(resolved.value, "env-fallback");
    assert_eq!(resolved.source, KeySource::Env);
}

#[test]
fn keychain_hit_is_tagged_as_legacy_for_migration() {
    let store = MockCredentialStore::default();
    save_api_key_with_store("SQUEEZY_TEST_LEGACY_KEY", "from-keychain", &store).expect("save");
    let resolved = resolve_api_key_with_inline_and_store(None, "SQUEEZY_TEST_LEGACY_KEY", &store)
        .expect("keychain");
    assert_eq!(resolved.value, "from-keychain");
    assert_eq!(resolved.source, KeySource::KeychainLegacy);
}

#[test]
fn fallback_env_var_promotes_source_tag() {
    // SQUEEZY_TEST_FALLBACK_KEY ↔ TEST_FALLBACK_API_KEY via fallback_env_var
    // (no real translation, but the explicit pair stays in sync since the
    // mapping is purely lexical). Use the actual pair from the helper.
    let store = MockCredentialStore::default();
    save_api_key_with_store("OPENAI_API_KEY", "from-vendor-name", &store).expect("save");
    let resolved = resolve_api_key_with_inline_and_store(None, "SQUEEZY_OPENAI_KEY", &store)
        .expect("fallback");
    assert_eq!(resolved.value, "from-vendor-name");
    assert_eq!(resolved.source, KeySource::FallbackKeychainLegacy);
}
