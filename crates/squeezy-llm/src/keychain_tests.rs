use super::*;

#[test]
fn env_api_key_wins_before_keychain() {
    unsafe {
        std::env::set_var("SQUEEZY_TEST_KEYCHAIN_ENV", "from-env");
    }
    let value =
        resolve_api_key("SQUEEZY_TEST_KEYCHAIN_ENV", Some("missing"), "openai").expect("env key");
    assert_eq!(value, "from-env");
    unsafe {
        std::env::remove_var("SQUEEZY_TEST_KEYCHAIN_ENV");
    }
}
