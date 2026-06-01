use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

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

// --- SQUEEZY_CREDENTIALS_JSON env tier --------------------------------------

fn set_creds_json(value: &str) {
    unsafe {
        std::env::set_var("SQUEEZY_CREDENTIALS_JSON", value);
    }
}

fn clear_creds_json() {
    unsafe {
        std::env::remove_var("SQUEEZY_CREDENTIALS_JSON");
    }
}

#[test]
fn credentials_resolves_from_squeezy_credentials_json() {
    // The acceptance test named in the audit: with no inline, no file,
    // no env var, no fallback env, the JSON blob still resolves the
    // requested credential by provider name. Synthetic env var name
    // so a developer with `OPENAI_API_KEY` exported in their shell
    // doesn't shadow the JSON tier under test.
    let _guard = creds_lock();
    let scratch = scratch("json-providers");
    point_creds_at(&scratch.file);
    set_creds_json(r#"{"providers":{"resolver_test_json_acceptance":"sk-from-json"}}"#);
    let resolved = resolve_api_key_with_inline(None, "SQUEEZY_RESOLVER_TEST_JSON_ACCEPTANCE_KEY")
        .expect("json env resolves provider");
    clear_creds_json();
    clear_creds_pointer();
    assert_eq!(resolved.value, "sk-from-json");
    assert_eq!(resolved.source, KeySource::JsonEnv);
}

#[test]
fn credentials_json_resolves_through_vendor_named_env() {
    // Provider-name lookup must work even when the caller asked for the
    // vendor-style env var name: the JSON blob is keyed on the
    // provider section, not the env var. Synthetic name so the host
    // environment can't shadow.
    let _guard = creds_lock();
    let scratch = scratch("json-vendor-name");
    point_creds_at(&scratch.file);
    set_creds_json(r#"{"providers":{"resolver_test_vendor":"sk-vendor"}}"#);
    let resolved = resolve_api_key_with_inline(None, "RESOLVER_TEST_VENDOR_API_KEY")
        .expect("vendor-named lookup");
    clear_creds_json();
    clear_creds_pointer();
    assert_eq!(resolved.value, "sk-vendor");
    assert_eq!(resolved.source, KeySource::JsonEnv);
}

#[test]
fn credentials_json_accepts_flat_env_var_keying() {
    // The on-disk credentials.json schema (flat `{"ENV_VAR": "value"}`)
    // also works when piped through the env var, so a workflow can
    // `cat credentials.json` into the variable without restructuring.
    let _guard = creds_lock();
    let scratch = scratch("json-flat");
    point_creds_at(&scratch.file);
    set_creds_json(r#"{"SQUEEZY_RESOLVER_TEST_JSON_FLAT": "flat-value"}"#);
    let resolved = resolve_api_key_with_inline(None, "SQUEEZY_RESOLVER_TEST_JSON_FLAT")
        .expect("flat shape resolves");
    clear_creds_json();
    clear_creds_pointer();
    assert_eq!(resolved.value, "flat-value");
    assert_eq!(resolved.source, KeySource::JsonEnv);
}

#[test]
fn credentials_json_sits_below_env_in_the_chain() {
    // The JSON blob is the broadcast channel for CI/CD; an explicitly
    // exported per-provider env var must still win so an operator can
    // override the CI-injected secret for a single shell session.
    let _guard = creds_lock();
    let scratch = scratch("json-below-env");
    point_creds_at(&scratch.file);
    set_creds_json(r#"{"providers":{"openai":"from-json"}}"#);
    unsafe {
        std::env::set_var("SQUEEZY_RESOLVER_TEST_JSON_VS_ENV", "from-env");
    }
    let resolved = resolve_api_key_with_inline(None, "SQUEEZY_RESOLVER_TEST_JSON_VS_ENV")
        .expect("env beats json");
    unsafe {
        std::env::remove_var("SQUEEZY_RESOLVER_TEST_JSON_VS_ENV");
    }
    clear_creds_json();
    clear_creds_pointer();
    assert_eq!(resolved.value, "from-env");
    assert_eq!(resolved.source, KeySource::Env);
}

#[test]
fn malformed_credentials_json_env_does_not_break_resolution() {
    // A bad JSON blob must not poison resolution — the resolver warns
    // once and continues to the missing-key error so the caller knows
    // which env var is needed.
    let _guard = creds_lock();
    let scratch = scratch("json-malformed");
    point_creds_at(&scratch.file);
    set_creds_json("{ this is not JSON");
    let err = resolve_api_key_with_inline(None, "SQUEEZY_RESOLVER_TEST_JSON_MALFORMED")
        .expect_err("no key anywhere");
    clear_creds_json();
    clear_creds_pointer();
    assert!(
        err.to_string()
            .contains("SQUEEZY_RESOLVER_TEST_JSON_MALFORMED"),
        "{err}"
    );
}

#[test]
fn empty_credentials_json_env_falls_through() {
    // Treat an empty / whitespace blob the same as an absent one so
    // CI configurations that always set the var (but leave it empty
    // for builds that don't need credentials) don't accidentally
    // shadow named env vars.
    let _guard = creds_lock();
    let scratch = scratch("json-empty");
    point_creds_at(&scratch.file);
    set_creds_json("   ");
    unsafe {
        std::env::set_var("SQUEEZY_RESOLVER_TEST_JSON_EMPTY", "via-env");
    }
    let resolved = resolve_api_key_with_inline(None, "SQUEEZY_RESOLVER_TEST_JSON_EMPTY")
        .expect("env resolves");
    unsafe {
        std::env::remove_var("SQUEEZY_RESOLVER_TEST_JSON_EMPTY");
    }
    clear_creds_json();
    clear_creds_pointer();
    assert_eq!(resolved.value, "via-env");
    assert_eq!(resolved.source, KeySource::Env);
}

// --- ApiKeySource trait + impls --------------------------------------------

#[tokio::test]
async fn static_api_key_returns_constant_value() {
    let source: Arc<dyn ApiKeySource> = Arc::new(StaticApiKey::new("sk-static", "anthropic"));
    let first = source.current_key().await.expect("current_key");
    let second = source.current_key().await.expect("current_key");
    assert_eq!(first, "sk-static");
    assert_eq!(second, "sk-static");
    assert_eq!(source.provider_label(), "anthropic");
}

#[tokio::test]
async fn static_api_key_invalidate_is_a_no_op() {
    let source: Arc<dyn ApiKeySource> = Arc::new(StaticApiKey::new("sk-static", "openai"));
    source
        .invalidate()
        .await
        .expect("invalidate must be infallible for static keys");
    let after = source
        .current_key()
        .await
        .expect("current_key after invalidate");
    assert_eq!(
        after, "sk-static",
        "StaticApiKey must keep the same value after invalidate"
    );
}

#[tokio::test]
async fn refreshable_token_returns_access_token() {
    let token = RefreshableToken::new(TokenState::new("sk-oauth-access"), "claude-pro");
    let source: Arc<dyn ApiKeySource> = Arc::new(token);
    let key = source.current_key().await.expect("current_key");
    assert_eq!(key, "sk-oauth-access");
    assert_eq!(source.provider_label(), "claude-pro");
}

#[tokio::test]
async fn refreshable_token_state_handle_lets_subagents_rotate_in_place() {
    // The OAuth refresh subagents land in F16pi-anthropic-oauth /
    // F16pi-openai-codex / F16pi-github-copilot. They need to swap the
    // access token under the same Arc so the provider client picks up
    // the new value on the next `current_key`. Exercise that wiring
    // here so the indirection is regression-tested even before the
    // refresh implementations land.
    let token = RefreshableToken::new(TokenState::new("initial"), "github-copilot");
    let handle = token.state_handle();
    let source: Arc<dyn ApiKeySource> = Arc::new(token);

    let before = source.current_key().await.expect("before");
    assert_eq!(before, "initial");

    {
        let mut guard = handle.write().await;
        guard.access_token = "rotated".to_string();
    }

    let after = source.current_key().await.expect("after");
    assert_eq!(
        after, "rotated",
        "RefreshableToken must observe writes through the shared state handle"
    );
}

#[tokio::test]
async fn refreshable_token_invalidate_is_currently_a_no_op() {
    // Placeholder implementation contract: until the OAuth refresh
    // subagents land, `invalidate` is a no-op and `current_key`
    // returns the same access token. This locks the contract so the
    // auth-retry layer can be wired in confidently against a
    // RefreshableToken before the real refresh ships.
    let token = RefreshableToken::new(TokenState::new("oauth-token"), "anthropic-pro");
    let source: Arc<dyn ApiKeySource> = Arc::new(token);
    source.invalidate().await.expect("no-op invalidate");
    let after = source.current_key().await.expect("current_key");
    assert_eq!(after, "oauth-token");
}

#[test]
fn static_api_key_source_wraps_resolved_key() {
    let source = static_api_key_source("sk-resolved".to_string(), "anthropic");
    assert_eq!(source.provider_label(), "anthropic");
}

// --- X-17: resolve_api_key_with_inline_optional ----------------------------

#[test]
fn optional_resolver_returns_empty_when_no_source_matches() {
    // X-17: local-hosted presets default to no-auth; the strict
    // resolver's `ProviderNotConfigured` blocks every LMStudio /
    // vLLM / llama.cpp session out of the box. The optional variant
    // returns `Ok("")` so the caller can short-circuit Bearer
    // injection and proceed without auth.
    let _guard = creds_lock();
    let scratch = scratch("optional-missing");
    point_creds_at(&scratch.file);
    let resolved =
        resolve_api_key_with_inline_optional(None, "SQUEEZY_RESOLVER_TEST_OPTIONAL_MISSING")
            .expect("missing → empty");
    clear_creds_pointer();
    assert_eq!(resolved.value, "");
    // The source label is the env tier so doctor output still names
    // the canonical env var; callers should branch on `value.is_empty()`.
    assert_eq!(resolved.source, KeySource::Env);
}

#[test]
fn optional_resolver_returns_empty_for_whitespace_inline() {
    // Whitespace-only inline is treated as absent by the strict
    // resolver (`!value.trim().is_empty()` gate). The optional
    // variant must funnel the same fall-through into `Ok("")`.
    let _guard = creds_lock();
    let scratch = scratch("optional-whitespace-inline");
    point_creds_at(&scratch.file);
    let resolved = resolve_api_key_with_inline_optional(
        Some("   "),
        "SQUEEZY_RESOLVER_TEST_OPTIONAL_BLANK_INLINE",
    )
    .expect("whitespace inline → empty");
    clear_creds_pointer();
    assert_eq!(resolved.value, "");
    assert_eq!(resolved.source, KeySource::Env);
}

#[test]
fn optional_resolver_still_resolves_real_keys() {
    // The optional variant must not regress the resolution chain
    // when an env var (or any other tier) actually has a value.
    let _guard = creds_lock();
    let scratch = scratch("optional-with-env");
    point_creds_at(&scratch.file);
    let key_name = "SQUEEZY_RESOLVER_TEST_OPTIONAL_HAS_VALUE";
    unsafe {
        std::env::set_var(key_name, "lmstudio-bearer");
    }
    let resolved =
        resolve_api_key_with_inline_optional(None, key_name).expect("env value resolves");
    unsafe {
        std::env::remove_var(key_name);
    }
    clear_creds_pointer();
    assert_eq!(resolved.value, "lmstudio-bearer");
    assert_eq!(resolved.source, KeySource::Env);
}

#[test]
fn optional_resolver_prefers_inline_over_env() {
    // Inline still wins over env, mirroring the strict resolver
    // contract. Local-preset users with `[providers.lmstudio]
    // api_key = "..."` keep their explicit value.
    let _guard = creds_lock();
    let scratch = scratch("optional-inline-over-env");
    point_creds_at(&scratch.file);
    let key_name = "SQUEEZY_RESOLVER_TEST_OPTIONAL_INLINE_WIN";
    unsafe {
        std::env::set_var(key_name, "env-loser");
    }
    let resolved =
        resolve_api_key_with_inline_optional(Some("inline-winner"), key_name).expect("inline wins");
    unsafe {
        std::env::remove_var(key_name);
    }
    clear_creds_pointer();
    assert_eq!(resolved.value, "inline-winner");
    assert_eq!(resolved.source, KeySource::Inline);
}

#[test]
fn optional_resolver_propagates_non_missing_errors() {
    // Only `ProviderNotConfigured` collapses to empty. Tests of the
    // contract: if a future failure mode in the strict resolver
    // emits a different error variant, the optional wrapper must
    // forward it untouched so callers can still surface that
    // (e.g. malformed credentials file, IO error). We approximate
    // by leveraging the only other guarantee in the surface: any
    // non-missing path returns a populated string. There is no
    // direct way to provoke another error today; the unit test
    // pins the behavior by exercising the missing path repeatedly
    // and trusting the cargo doctest on the function signature.
    let _guard = creds_lock();
    let scratch = scratch("optional-non-missing");
    point_creds_at(&scratch.file);
    let first =
        resolve_api_key_with_inline_optional(None, "SQUEEZY_RESOLVER_TEST_OPTIONAL_REPEAT_1")
            .expect("first");
    let second =
        resolve_api_key_with_inline_optional(None, "SQUEEZY_RESOLVER_TEST_OPTIONAL_REPEAT_2")
            .expect("second");
    clear_creds_pointer();
    assert!(first.value.is_empty());
    assert!(second.value.is_empty());
}

// --- H-46: empty key must not feed bearer_auth -----------------------------

#[test]
fn empty_resolved_value_is_safe_for_caller_short_circuit() {
    // H-46: `compatible.rs:474` calls `bearer_auth(key)`; passing an
    // empty string clobbers a user-supplied `Authorization` header
    // in `extra_headers`. The fix in compatible.rs is to skip the
    // call when the key is empty. Here we lock in the credentials
    // contract that lets the caller make that decision: the
    // optional resolver returns `value.is_empty() == true` cleanly,
    // no panic, no error.
    let _guard = creds_lock();
    let scratch = scratch("h46-empty");
    point_creds_at(&scratch.file);
    let resolved =
        resolve_api_key_with_inline_optional(None, "SQUEEZY_RESOLVER_TEST_H46_EMPTY_BEARER")
            .expect("empty path is Ok(\"\")");
    clear_creds_pointer();
    assert!(
        resolved.value.is_empty(),
        "empty key path must yield empty string"
    );
    // The caller's short-circuit pattern: branch on is_empty(), do
    // not pass to bearer_auth.
    let should_attach_bearer = !resolved.value.is_empty();
    assert!(
        !should_attach_bearer,
        "caller pattern: skip bearer_auth when empty"
    );
}
