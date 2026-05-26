use super::*;

#[test]
fn resolver_prefers_inline_over_env_and_fallback() {
    let key_name = "SQUEEZY_RESOLVER_TEST_INLINE";
    unsafe {
        std::env::set_var(key_name, "env-loser");
    }
    let resolved =
        resolve_api_key_with_inline(Some("inline-winner"), key_name).expect("inline wins");
    unsafe {
        std::env::remove_var(key_name);
    }
    assert_eq!(resolved.value, "inline-winner");
    assert_eq!(resolved.source, KeySource::Inline);
}

#[test]
fn empty_inline_falls_through_to_env() {
    let key_name = "SQUEEZY_RESOLVER_TEST_EMPTY_INLINE";
    unsafe {
        std::env::set_var(key_name, "env-fallback");
    }
    let resolved = resolve_api_key_with_inline(Some("   "), key_name).expect("env fallback");
    unsafe {
        std::env::remove_var(key_name);
    }
    assert_eq!(resolved.value, "env-fallback");
    assert_eq!(resolved.source, KeySource::Env);
}

#[test]
fn resolver_falls_back_to_vendor_env_var() {
    // Squeezy-prefixed env var is the canonical name in code; the
    // vendor-style `<X>_API_KEY` is the fallback. Setting only the
    // fallback should still resolve and be tagged FallbackEnv.
    unsafe {
        std::env::set_var("RESOLVER_TEST_FALLBACK_API_KEY", "from-vendor-name");
    }
    let resolved =
        resolve_api_key_with_inline(None, "SQUEEZY_RESOLVER_TEST_FALLBACK_KEY").expect("fallback");
    unsafe {
        std::env::remove_var("RESOLVER_TEST_FALLBACK_API_KEY");
    }
    assert_eq!(resolved.value, "from-vendor-name");
    assert_eq!(resolved.source, KeySource::FallbackEnv);
}

#[test]
fn missing_key_message_mentions_env_and_toml() {
    let error =
        resolve_api_key_with_inline(None, "SQUEEZY_RESOLVER_TEST_MISSING").expect_err("missing");
    let message = error.to_string();
    assert!(
        message.contains("SQUEEZY_RESOLVER_TEST_MISSING"),
        "{message}"
    );
    assert!(message.contains("api_key"), "{message}");
}

#[test]
fn fallback_env_var_translation_round_trips() {
    assert_eq!(
        fallback_env_var("SQUEEZY_OPENAI_KEY"),
        Some("OPENAI_API_KEY".to_string())
    );
    assert_eq!(
        fallback_env_var("OPENAI_API_KEY"),
        Some("SQUEEZY_OPENAI_KEY".to_string())
    );
    assert_eq!(fallback_env_var("UNRELATED"), None);
}
