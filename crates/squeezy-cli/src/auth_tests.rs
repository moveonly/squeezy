use std::{
    collections::BTreeMap,
    sync::{Mutex, PoisonError},
};

use squeezy_llm::KeyringCredentialStore;

use super::{AuthSetArgs, handle_auth_set_with_store};

#[derive(Debug, Default)]
struct MockCredentialStore {
    values: Mutex<BTreeMap<String, String>>,
}

impl MockCredentialStore {
    fn snapshot(&self) -> BTreeMap<String, String> {
        self.values
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
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
fn auth_set_stores_key_for_known_provider() {
    let store = MockCredentialStore::default();
    let args = AuthSetArgs {
        provider: "openai".to_string(),
        value: Some("sk-test".to_string()),
        env: None,
    };

    handle_auth_set_with_store(&args, &store, || {
        panic!("stdin must not be consulted when --value is provided")
    })
    .expect("save");

    assert_eq!(
        store.snapshot().get("OPENAI_API_KEY").map(String::as_str),
        Some("sk-test"),
    );
}

#[test]
fn auth_set_reads_from_stdin_when_value_is_absent() {
    let store = MockCredentialStore::default();
    let args = AuthSetArgs {
        provider: "anthropic".to_string(),
        value: None,
        env: None,
    };

    handle_auth_set_with_store(&args, &store, || Ok("sk-ant-test".to_string())).expect("save");

    assert_eq!(
        store
            .snapshot()
            .get("ANTHROPIC_API_KEY")
            .map(String::as_str),
        Some("sk-ant-test"),
    );
}

#[test]
fn auth_set_honors_env_override() {
    let store = MockCredentialStore::default();
    let args = AuthSetArgs {
        provider: "custom".to_string(),
        value: Some("xyz".to_string()),
        env: Some("CUSTOM_KEY_ENV".to_string()),
    };

    handle_auth_set_with_store(&args, &store, || unreachable!()).expect("save");

    assert_eq!(
        store.snapshot().get("CUSTOM_KEY_ENV").map(String::as_str),
        Some("xyz"),
    );
}

#[test]
fn auth_set_rejects_bedrock_provider_without_env_override() {
    let store = MockCredentialStore::default();
    let args = AuthSetArgs {
        provider: "bedrock".to_string(),
        value: Some("anything".to_string()),
        env: None,
    };

    let err = handle_auth_set_with_store(&args, &store, || unreachable!())
        .expect_err("bedrock uses AWS chain");
    assert!(err.to_string().contains("aws configure"), "{err}");
    assert!(store.snapshot().is_empty());
}
