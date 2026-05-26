use std::sync::atomic::{AtomicU64, Ordering};

use super::{AuthSetArgs, handle_auth_set_at_path};

static NONCE: AtomicU64 = AtomicU64::new(0);

fn temp_settings_path(prefix: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "squeezy-auth-{}-{}-{}",
        prefix,
        std::process::id(),
        NONCE.fetch_add(1, Ordering::SeqCst),
    ));
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir.join("settings.toml")
}

#[test]
fn auth_set_writes_inline_api_key_for_known_provider() {
    let path = temp_settings_path("openai");
    let args = AuthSetArgs {
        provider: "openai".to_string(),
        value: Some("sk-test".to_string()),
        user: false,
    };

    handle_auth_set_at_path(&args, path.clone(), false, || {
        panic!("stdin must not be consulted when --value is provided")
    })
    .expect("save");

    let contents = std::fs::read_to_string(&path).expect("read settings");
    assert!(
        contents.contains("[providers.openai]"),
        "expected [providers.openai] section, got: {contents}"
    );
    assert!(
        contents.contains("api_key = \"sk-test\""),
        "expected inline api_key, got: {contents}"
    );
}

#[test]
fn auth_set_reads_from_stdin_when_value_is_absent() {
    let path = temp_settings_path("anthropic");
    let args = AuthSetArgs {
        provider: "anthropic".to_string(),
        value: None,
        user: false,
    };

    handle_auth_set_at_path(&args, path.clone(), false, || Ok("sk-ant-test".to_string()))
        .expect("save");

    let contents = std::fs::read_to_string(&path).expect("read settings");
    assert!(contents.contains("[providers.anthropic]"), "{contents}");
    assert!(contents.contains("api_key = \"sk-ant-test\""), "{contents}");
}

#[test]
fn auth_set_rejects_bedrock_with_aws_chain_message() {
    let path = temp_settings_path("bedrock");
    let args = AuthSetArgs {
        provider: "bedrock".to_string(),
        value: Some("anything".to_string()),
        user: false,
    };

    let err = handle_auth_set_at_path(&args, path.clone(), false, || unreachable!())
        .expect_err("bedrock uses AWS chain");
    assert!(err.to_string().contains("aws configure"), "{err}");
    assert!(
        !path.exists(),
        "no file should be written for an unsupported provider"
    );
}

#[test]
fn auth_set_rejects_empty_key() {
    let path = temp_settings_path("empty");
    let args = AuthSetArgs {
        provider: "openai".to_string(),
        value: Some("   ".to_string()),
        user: false,
    };

    let err = handle_auth_set_at_path(&args, path.clone(), false, || unreachable!())
        .expect_err("empty key must error");
    assert!(err.to_string().contains("empty"), "{err}");
}

#[test]
fn auth_set_keeps_other_provider_sections_intact() {
    let path = temp_settings_path("merge");
    std::fs::write(
        &path,
        "[providers.anthropic]\napi_key = \"sk-ant-existing\"\n",
    )
    .expect("seed file");

    let args = AuthSetArgs {
        provider: "openai".to_string(),
        value: Some("sk-new".to_string()),
        user: false,
    };

    handle_auth_set_at_path(&args, path.clone(), false, || unreachable!()).expect("save");

    let contents = std::fs::read_to_string(&path).expect("read settings");
    assert!(
        contents.contains("sk-ant-existing"),
        "previous provider key was clobbered: {contents}"
    );
    assert!(
        contents.contains("api_key = \"sk-new\""),
        "new provider key missing: {contents}"
    );
}
