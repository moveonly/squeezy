use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use squeezy_core::settings_writer::SettingsScope;

use super::*;

static NONCE: AtomicU64 = AtomicU64::new(0);

// `SQUEEZY_CREDENTIALS_FILE` is a single process-wide override and the
// credentials file is also resolved from `$HOME/.squeezy` when the
// override is unset. Every test below either consults the file tier or
// must guarantee it doesn't shadow the env tier under it, so they all
// serialize through `creds_lock()` and explicitly point the override
// at a tempdir entry — either populated for the file-path tests or
// left absent for the env-path tests so the file tier returns None.

fn creds_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

fn temp_settings_path(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "squeezy-credentials-{}-{}-{}",
        prefix,
        std::process::id(),
        NONCE.fetch_add(1, Ordering::SeqCst),
    ));
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir.join("settings.toml")
}

#[test]
fn delete_removes_inline_api_key_for_provider() {
    let path = temp_settings_path("openai");
    std::fs::write(
        &path,
        "[providers.openai]\napi_key = \"sk-test\"\nbase_url = \"https://example.com\"\n",
    )
    .expect("seed file");

    let removed =
        delete_api_key("openai", &SettingsScope::user(path.clone())).expect("delete returns Ok");
    assert!(removed, "expected the api_key field to be removed");

    let contents = std::fs::read_to_string(&path).expect("read settings");
    assert!(
        !contents.contains("api_key"),
        "api_key should be gone: {contents}"
    );
    assert!(
        contents.contains("base_url"),
        "non-secret fields must survive: {contents}"
    );
    assert!(
        contents.contains("[providers.openai]"),
        "section header should survive: {contents}"
    );
}

#[test]
fn delete_is_idempotent_when_no_inline_key_present() {
    let path = temp_settings_path("idempotent");
    std::fs::write(
        &path,
        "[providers.openai]\nbase_url = \"https://example.com\"\n",
    )
    .expect("seed file");

    let removed =
        delete_api_key("openai", &SettingsScope::user(path.clone())).expect("delete returns Ok");
    assert!(!removed, "no api_key to remove → reports false");

    let contents = std::fs::read_to_string(&path).expect("read settings");
    assert!(contents.contains("base_url"), "{contents}");
}

#[test]
fn delete_leaves_other_provider_sections_untouched() {
    let path = temp_settings_path("merge");
    std::fs::write(
        &path,
        "[providers.openai]\napi_key = \"sk-openai\"\n\n[providers.anthropic]\napi_key = \"sk-ant\"\n",
    )
    .expect("seed file");

    delete_api_key("openai", &SettingsScope::user(path.clone())).expect("delete openai");

    let contents = std::fs::read_to_string(&path).expect("read settings");
    assert!(
        !contents.contains("sk-openai"),
        "deleted key still present: {contents}"
    );
    assert!(
        contents.contains("sk-ant"),
        "untouched provider got clobbered: {contents}"
    );
}

#[test]
fn delete_round_trips_with_set_table_entry() {
    use squeezy_core::settings_writer::{EditOp, SettingsEdit, apply_edits};

    let path = temp_settings_path("roundtrip");
    let scope = SettingsScope::user(path.clone());

    // Stage 1: write via the same `apply_edits` surface that auth set uses.
    apply_edits(
        &scope,
        &[SettingsEdit {
            path: &[],
            op: EditOp::SetTableEntry {
                table_path: &["providers"],
                key: "openai".to_string(),
                fields: vec![("api_key", EditOp::SetString("sk-roundtrip".to_string()))],
            },
        }],
    )
    .expect("set");

    let after_set = std::fs::read_to_string(&path).expect("read after set");
    assert!(
        after_set.contains("api_key = \"sk-roundtrip\""),
        "{after_set}"
    );

    // Stage 2: delete via the new helper.
    let removed = delete_api_key("openai", &scope).expect("delete");
    assert!(removed, "round-trip delete should report removal");

    let after_delete = std::fs::read_to_string(&path).expect("read after delete");
    assert!(
        !after_delete.contains("sk-roundtrip"),
        "round-trip delete failed: {after_delete}"
    );
}

#[test]
fn delete_refuses_committed_project_scope() {
    let path = temp_settings_path("project");
    let err = delete_api_key("openai", &SettingsScope::project(path.clone()))
        .expect_err("project scope must refuse");
    assert!(
        err.to_string().contains("project TOML"),
        "expected refusal message, got: {err}"
    );
    assert!(
        !path.exists(),
        "refusing to write should not create the file: {}",
        path.display()
    );
}

#[test]
fn delete_rejects_empty_provider_section() {
    let path = temp_settings_path("empty-section");
    let err = delete_api_key("", &SettingsScope::user(path.clone()))
        .expect_err("empty section must error");
    assert!(err.to_string().contains("must not be empty"), "{err}");
}

#[test]
fn resolver_prefers_inline_over_env_and_fallback() {
    let _guard = creds_lock();
    let scratch = scratch("inline-over-env");
    point_creds_at(&scratch.file);
    let key_name = "SQUEEZY_RESOLVER_TEST_INLINE";
    unsafe {
        std::env::set_var(key_name, "env-loser");
    }
    let resolved =
        resolve_api_key_with_inline(Some("inline-winner"), key_name).expect("inline wins");
    unsafe {
        std::env::remove_var(key_name);
    }
    clear_creds_pointer();
    assert_eq!(resolved.value, "inline-winner");
    assert_eq!(resolved.source, KeySource::Inline);
}

#[test]
fn empty_inline_falls_through_to_env() {
    let _guard = creds_lock();
    let scratch = scratch("empty-inline");
    point_creds_at(&scratch.file);
    let key_name = "SQUEEZY_RESOLVER_TEST_EMPTY_INLINE";
    unsafe {
        std::env::set_var(key_name, "env-fallback");
    }
    let resolved = resolve_api_key_with_inline(Some("   "), key_name).expect("env fallback");
    unsafe {
        std::env::remove_var(key_name);
    }
    clear_creds_pointer();
    assert_eq!(resolved.value, "env-fallback");
    assert_eq!(resolved.source, KeySource::Env);
}

#[test]
fn resolver_falls_back_to_vendor_env_var() {
    // Squeezy-prefixed env var is the canonical name in code; the
    // vendor-style `<X>_API_KEY` is the fallback. Setting only the
    // fallback should still resolve and be tagged FallbackEnv.
    let _guard = creds_lock();
    let scratch = scratch("vendor-env");
    point_creds_at(&scratch.file);
    unsafe {
        std::env::set_var("RESOLVER_TEST_FALLBACK_API_KEY", "from-vendor-name");
    }
    let resolved =
        resolve_api_key_with_inline(None, "SQUEEZY_RESOLVER_TEST_FALLBACK_KEY").expect("fallback");
    unsafe {
        std::env::remove_var("RESOLVER_TEST_FALLBACK_API_KEY");
    }
    clear_creds_pointer();
    assert_eq!(resolved.value, "from-vendor-name");
    assert_eq!(resolved.source, KeySource::FallbackEnv);
}

#[test]
fn missing_key_message_mentions_env_and_toml() {
    let _guard = creds_lock();
    let scratch = scratch("missing-key");
    point_creds_at(&scratch.file);
    let error =
        resolve_api_key_with_inline(None, "SQUEEZY_RESOLVER_TEST_MISSING").expect_err("missing");
    clear_creds_pointer();
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

// --- credentials.json fallback ---------------------------------------------

struct CredsScratch {
    file: PathBuf,
    dir: PathBuf,
}

impl Drop for CredsScratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn scratch(tag: &str) -> CredsScratch {
    // Use pid + nanos + tag so concurrent test processes (and parallel
    // tests inside this process that don't share the resolver lock)
    // never collide on the same path.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "squeezy-creds-test-{}-{}-{}",
        std::process::id(),
        nanos,
        tag
    ));
    std::fs::create_dir_all(&dir).expect("mkdir scratch");
    let file = dir.join("credentials.json");
    CredsScratch { file, dir }
}

fn write_creds(path: &std::path::Path, body: &str) {
    std::fs::write(path, body).expect("write creds");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path).expect("meta").permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms).expect("chmod");
    }
}

fn point_creds_at(path: &std::path::Path) {
    unsafe {
        std::env::set_var("SQUEEZY_CREDENTIALS_FILE", path);
    }
}

fn clear_creds_pointer() {
    unsafe {
        std::env::remove_var("SQUEEZY_CREDENTIALS_FILE");
    }
}

#[test]
fn credentials_file_resolves_when_keyring_path_is_unavailable() {
    let _guard = creds_lock();
    let scratch = scratch("resolves");
    write_creds(
        &scratch.file,
        r#"{"SQUEEZY_RESOLVER_TEST_FILE_KEY": "from-credentials-json"}"#,
    );
    point_creds_at(&scratch.file);
    let resolved =
        resolve_api_key_with_inline(None, "SQUEEZY_RESOLVER_TEST_FILE_KEY").expect("file resolves");
    clear_creds_pointer();
    assert_eq!(resolved.value, "from-credentials-json");
    assert_eq!(resolved.source, KeySource::File);
}

#[test]
fn credentials_file_beats_env_when_both_present() {
    // File sits above env in the chain so an explicit credentials.json
    // entry can override a stale `export OPENAI_API_KEY=...` lingering
    // in the shell.
    let _guard = creds_lock();
    let scratch = scratch("beats-env");
    write_creds(
        &scratch.file,
        r#"{"SQUEEZY_RESOLVER_TEST_FILE_OVER_ENV": "from-file"}"#,
    );
    point_creds_at(&scratch.file);
    unsafe {
        std::env::set_var("SQUEEZY_RESOLVER_TEST_FILE_OVER_ENV", "from-env");
    }
    let resolved = resolve_api_key_with_inline(None, "SQUEEZY_RESOLVER_TEST_FILE_OVER_ENV")
        .expect("file beats env");
    unsafe {
        std::env::remove_var("SQUEEZY_RESOLVER_TEST_FILE_OVER_ENV");
    }
    clear_creds_pointer();
    assert_eq!(resolved.value, "from-file");
    assert_eq!(resolved.source, KeySource::File);
}

#[test]
fn credentials_file_translates_through_fallback_env_name() {
    // If the file is keyed by the vendor name (OPENAI_API_KEY) but the
    // caller asked for the Squeezy name (SQUEEZY_OPENAI_KEY), the
    // translator should bridge.
    let _guard = creds_lock();
    let scratch = scratch("fallback-name");
    write_creds(
        &scratch.file,
        r#"{"RESOLVER_TEST_FILE_FALLBACK_API_KEY": "from-vendor-named-file"}"#,
    );
    point_creds_at(&scratch.file);
    let resolved = resolve_api_key_with_inline(None, "SQUEEZY_RESOLVER_TEST_FILE_FALLBACK_KEY")
        .expect("fallback name in file resolves");
    clear_creds_pointer();
    assert_eq!(resolved.value, "from-vendor-named-file");
    assert_eq!(resolved.source, KeySource::File);
}

#[test]
fn missing_credentials_file_still_falls_through_to_env() {
    // The "keyring failure" graceful-degrade path: file doesn't exist,
    // env var present, resolution succeeds via Env without surfacing
    // any error to the caller.
    let _guard = creds_lock();
    let scratch = scratch("missing-file");
    assert!(!scratch.file.exists(), "scratch file should not exist yet");
    point_creds_at(&scratch.file);
    unsafe {
        std::env::set_var("SQUEEZY_RESOLVER_TEST_MISSING_FILE", "via-env");
    }
    let resolved = resolve_api_key_with_inline(None, "SQUEEZY_RESOLVER_TEST_MISSING_FILE")
        .expect("env still works");
    unsafe {
        std::env::remove_var("SQUEEZY_RESOLVER_TEST_MISSING_FILE");
    }
    clear_creds_pointer();
    assert_eq!(resolved.value, "via-env");
    assert_eq!(resolved.source, KeySource::Env);
}

#[test]
fn malformed_credentials_file_degrades_to_env() {
    // A corrupt JSON file must not break key resolution — the warn
    // is emitted once and the env tier is consulted.
    let _guard = creds_lock();
    let scratch = scratch("malformed");
    write_creds(&scratch.file, "{ this is not JSON");
    point_creds_at(&scratch.file);
    unsafe {
        std::env::set_var("SQUEEZY_RESOLVER_TEST_MALFORMED_FILE", "env-survived");
    }
    let resolved = resolve_api_key_with_inline(None, "SQUEEZY_RESOLVER_TEST_MALFORMED_FILE")
        .expect("env fallback still works");
    unsafe {
        std::env::remove_var("SQUEEZY_RESOLVER_TEST_MALFORMED_FILE");
    }
    clear_creds_pointer();
    assert_eq!(resolved.value, "env-survived");
    assert_eq!(resolved.source, KeySource::Env);
}

#[cfg(unix)]
#[test]
fn credentials_file_with_loose_permissions_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let _guard = creds_lock();
    let scratch = scratch("loose-perms");
    std::fs::write(
        &scratch.file,
        r#"{"SQUEEZY_RESOLVER_TEST_LOOSE_PERMS": "should-be-refused"}"#,
    )
    .expect("write");
    // 0o644 is the canonical "group + world readable" mode that a key
    // file must never use.
    let mut perms = std::fs::metadata(&scratch.file)
        .expect("meta")
        .permissions();
    perms.set_mode(0o644);
    std::fs::set_permissions(&scratch.file, perms).expect("chmod 644");

    point_creds_at(&scratch.file);
    unsafe {
        std::env::set_var("SQUEEZY_RESOLVER_TEST_LOOSE_PERMS", "env-rescues");
    }
    let resolved = resolve_api_key_with_inline(None, "SQUEEZY_RESOLVER_TEST_LOOSE_PERMS")
        .expect("env rescues from refused file");
    unsafe {
        std::env::remove_var("SQUEEZY_RESOLVER_TEST_LOOSE_PERMS");
    }
    clear_creds_pointer();
    assert_eq!(resolved.value, "env-rescues");
    assert_eq!(resolved.source, KeySource::Env);
}

#[test]
fn inline_still_beats_credentials_file() {
    let _guard = creds_lock();
    let scratch = scratch("inline-over-file");
    write_creds(
        &scratch.file,
        r#"{"SQUEEZY_RESOLVER_TEST_INLINE_OVER_FILE": "from-file"}"#,
    );
    point_creds_at(&scratch.file);
    let resolved = resolve_api_key_with_inline(
        Some("inline-winner"),
        "SQUEEZY_RESOLVER_TEST_INLINE_OVER_FILE",
    )
    .expect("inline wins");
    clear_creds_pointer();
    assert_eq!(resolved.value, "inline-winner");
    assert_eq!(resolved.source, KeySource::Inline);
}
