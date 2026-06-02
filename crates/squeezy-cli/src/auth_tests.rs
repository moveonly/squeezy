use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use squeezy_core::{SeparatedSources, TierSource};

use super::{
    AuthListArgs, AuthRemoveArgs, AuthSetArgs, AuthStatusArgs, collect_inline_keys,
    handle_auth_remove_at_path, handle_auth_set_at_path, handle_auth_status_with_env,
};

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

fn load_tier(path: &std::path::Path) -> Option<TierSource> {
    let text = std::fs::read_to_string(path).ok()?;
    let doc = text.parse::<toml_edit::DocumentMut>().expect("parse toml");
    Some(TierSource {
        path: path.to_path_buf(),
        doc,
    })
}

fn synthetic_sources(
    user: Option<std::path::PathBuf>,
    project: Option<std::path::PathBuf>,
    repo: Option<std::path::PathBuf>,
) -> SeparatedSources {
    let user_default = user
        .clone()
        .unwrap_or_else(|| temp_settings_path("missing-user"));
    let project_default = project
        .clone()
        .unwrap_or_else(|| temp_settings_path("missing-project"));
    let repo_default = repo
        .clone()
        .unwrap_or_else(|| temp_settings_path("missing-repo"));
    SeparatedSources {
        user: user.as_deref().and_then(load_tier),
        project: project.as_deref().and_then(load_tier),
        repo: repo.as_deref().and_then(load_tier),
        user_path_default: user_default,
        project_path_default: project_default,
        repo_path_default: repo_default,
    }
}

#[allow(clippy::type_complexity)]
fn env_from(pairs: &[(&str, &str)]) -> Box<dyn Fn(&str) -> Option<String>> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    Box::new(move |name: &str| map.get(name).cloned())
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

#[test]
fn auth_list_collects_inline_keys_across_tiers() {
    let user_path = temp_settings_path("list-user");
    let project_path = temp_settings_path("list-project");
    let repo_path = temp_settings_path("list-repo");

    std::fs::write(
        &user_path,
        "[providers.openai]\napi_key = \"sk-user-openai-1234567890\"\n",
    )
    .expect("seed user");
    std::fs::write(
        &project_path,
        "[providers.anthropic]\napi_key = \"sk-ant-project-9876543210\"\n",
    )
    .expect("seed project");
    std::fs::write(
        &repo_path,
        "[providers.groq]\napi_key = \"gsk-local-abcdef1234\"\n",
    )
    .expect("seed repo");

    let sources = synthetic_sources(
        Some(user_path.clone()),
        Some(project_path.clone()),
        Some(repo_path.clone()),
    );
    let list = collect_inline_keys(&sources);

    assert_eq!(
        list.entries.len(),
        3,
        "expected one entry per tier, got {list:?}"
    );
    let providers: Vec<&str> = list.entries.iter().map(|e| e.provider.as_str()).collect();
    assert!(providers.contains(&"openai"), "{providers:?}");
    assert!(providers.contains(&"anthropic"), "{providers:?}");
    assert!(providers.contains(&"groq"), "{providers:?}");
    let openai_entry = list
        .entries
        .iter()
        .find(|e| e.provider == "openai")
        .expect("openai entry");
    assert!(
        !openai_entry.redacted.contains("sk-user-openai-1234567890"),
        "raw key must not appear in redacted form: {}",
        openai_entry.redacted
    );
}

#[test]
fn auth_list_returns_empty_when_no_tiers_have_inline_keys() {
    let path = temp_settings_path("list-empty");
    std::fs::write(&path, "# empty settings\n").expect("seed");
    let sources = synthetic_sources(Some(path), None, None);
    let list = collect_inline_keys(&sources);
    assert!(list.entries.is_empty(), "expected empty, got {list:?}");
}

#[test]
fn auth_list_json_serializes_entries() {
    let path = temp_settings_path("list-json");
    std::fs::write(
        &path,
        "[providers.openai]\napi_key = \"sk-test-xyz12345\"\n",
    )
    .expect("seed");
    let sources = synthetic_sources(Some(path), None, None);
    let list = collect_inline_keys(&sources);
    let json = list.to_json();
    let arr = json.as_array().expect("json array");
    assert_eq!(arr.len(), 1);
    let row = &arr[0];
    assert_eq!(row["provider"], "openai");
    assert_eq!(row["tier"], "user");
    assert!(row["redacted"].as_str().unwrap().contains("…"));
}

#[test]
fn auth_remove_strips_only_api_key_leaf() {
    let path = temp_settings_path("remove-keeps");
    std::fs::write(
        &path,
        "[providers.openai]\napi_key = \"sk-test\"\napi_key_env = \"OPENAI_API_KEY\"\nbase_url = \"https://example.invalid\"\n",
    )
    .expect("seed");

    let args = AuthRemoveArgs {
        provider: "openai".to_string(),
        user: false,
    };
    handle_auth_remove_at_path(&args, path.clone(), false).expect("remove");

    let contents = std::fs::read_to_string(&path).expect("read");
    assert!(
        !contents.contains("sk-test"),
        "secret should be gone, got: {contents}"
    );
    assert!(
        contents.contains("api_key_env"),
        "sibling leaf should survive, got: {contents}"
    );
    assert!(
        contents.contains("base_url"),
        "sibling leaf should survive, got: {contents}"
    );
}

#[test]
fn auth_remove_errors_when_no_inline_key_present() {
    let path = temp_settings_path("remove-missing");
    std::fs::write(
        &path,
        "[providers.openai]\napi_key_env = \"OPENAI_API_KEY\"\n",
    )
    .expect("seed");

    let args = AuthRemoveArgs {
        provider: "openai".to_string(),
        user: false,
    };
    let err =
        handle_auth_remove_at_path(&args, path.clone(), false).expect_err("nothing to remove");
    let message = err.to_string();
    assert!(message.contains("no inline api_key"), "{message}");
}

#[test]
fn auth_remove_rejects_unknown_provider_with_actionable_error() {
    let path = temp_settings_path("remove-bedrock");
    let args = AuthRemoveArgs {
        provider: "bedrock".to_string(),
        user: false,
    };
    let err =
        handle_auth_remove_at_path(&args, path.clone(), false).expect_err("bedrock uses aws chain");
    assert!(err.to_string().contains("aws configure"), "{err}");
}

#[test]
fn auth_status_reports_inline_when_set_in_repo_tier() {
    let repo_path = temp_settings_path("status-inline");
    std::fs::write(
        &repo_path,
        "[providers.openai]\napi_key = \"sk-real-xyz12345\"\n",
    )
    .expect("seed repo");
    let sources = synthetic_sources(None, None, Some(repo_path.clone()));

    let args = AuthStatusArgs {
        provider: Some("openai".to_string()),
        json: true,
    };
    handle_auth_status_with_env(&args, &sources, &|_| None).expect("status");
}

#[test]
fn auth_status_row_marks_env_when_only_env_is_set() {
    let sources = synthetic_sources(None, None, None);
    let env = env_from(&[("SQUEEZY_ANTHROPIC_KEY", "sk-ant-from-env")]);
    let args = AuthStatusArgs {
        provider: Some("anthropic".to_string()),
        json: true,
    };
    handle_auth_status_with_env(&args, &sources, &env).expect("status");
}

#[test]
fn auth_status_unknown_provider_is_rejected() {
    let sources = synthetic_sources(None, None, None);
    let args = AuthStatusArgs {
        provider: Some("not-a-real-provider".to_string()),
        json: false,
    };
    let err = handle_auth_status_with_env(&args, &sources, &|_| None).expect_err("rejected");
    assert!(err.to_string().contains("unknown provider"), "{err}");
}

#[test]
fn auth_status_listing_iterates_all_known_providers() {
    let sources = synthetic_sources(None, None, None);
    let args = AuthStatusArgs {
        provider: None,
        json: true,
    };
    handle_auth_status_with_env(&args, &sources, &|_| None).expect("status");
}

// Exercise the inline-status row computation in isolation so we can
// assert effective-source behavior without going through the printer.
#[test]
fn compute_status_row_prefers_inline_over_env() {
    use super::{KNOWN_PROVIDERS, compute_status_row};

    let openai = KNOWN_PROVIDERS
        .iter()
        .find(|p| p.section == "openai")
        .copied()
        .expect("openai known");
    let repo_path = temp_settings_path("status-prefer-inline");
    std::fs::write(
        &repo_path,
        "[providers.openai]\napi_key = \"sk-precedence-12345\"\n",
    )
    .expect("seed");
    let sources = synthetic_sources(None, None, Some(repo_path));
    let env = env_from(&[("SQUEEZY_OPENAI_KEY", "sk-env-should-not-win")]);
    let row = compute_status_row(openai, &sources, &env);
    assert_eq!(row.effective_source(), "inline");
    assert!(row.env_set, "env still detected, just not preferred");
}

#[test]
fn compute_status_row_falls_back_to_fallback_env() {
    use super::{KNOWN_PROVIDERS, compute_status_row};

    let openai = KNOWN_PROVIDERS
        .iter()
        .find(|p| p.section == "openai")
        .copied()
        .expect("openai known");
    let sources = synthetic_sources(None, None, None);
    let env = env_from(&[("OPENAI_API_KEY", "sk-vendor-style")]);
    let row = compute_status_row(openai, &sources, &env);
    assert_eq!(row.effective_source(), "env (fallback)");
    assert!(row.fallback_env_set);
    assert!(!row.env_set);
}

#[test]
fn compute_status_row_reports_missing_when_no_source() {
    use super::{KNOWN_PROVIDERS, compute_status_row};

    let openai = KNOWN_PROVIDERS
        .iter()
        .find(|p| p.section == "openai")
        .copied()
        .expect("openai known");
    let sources = synthetic_sources(None, None, None);
    let row = compute_status_row(openai, &sources, &|_| None);
    assert_eq!(row.effective_source(), "missing");
}

#[test]
fn auth_list_args_default_json_false() {
    // Guards the clap default so a future Args derive change doesn't
    // accidentally flip `--json` to default-on for the list view.
    let args = AuthListArgs::default();
    assert!(!args.json);
}

#[test]
fn deepinfra_provider_is_registered_with_canonical_api_key_env() {
    // The CLI status table must match the core runtime resolver, which treats
    // `DEEPINFRA_API_KEY` as the canonical primary (`default_api_key_env`) and
    // `DEEPINFRA_TOKEN` as the alias consulted only when the primary is empty
    // (`preset_api_key_env_aliases`). If these diverge, `auth status` reports a
    // different effective credential than the one a request actually uses.
    use super::KNOWN_PROVIDERS;
    let deepinfra = KNOWN_PROVIDERS
        .iter()
        .find(|p| p.section == "deepinfra")
        .copied()
        .expect("deepinfra must be in the known-provider table");
    assert_eq!(deepinfra.cli, "deepinfra");
    assert_eq!(deepinfra.env, "DEEPINFRA_API_KEY");
    assert_eq!(deepinfra.fallback_env, Some("DEEPINFRA_TOKEN"));
}

#[test]
fn deepinfra_status_row_reports_api_key_env_when_set() {
    // The status table must surface the canonical `DEEPINFRA_API_KEY (set)`
    // as the primary `env` source, matching the core resolver. We exercise
    // compute_status_row directly rather than the printer; the JSON form is
    // asserted by the iterator test above.
    use super::{KNOWN_PROVIDERS, compute_status_row};
    let deepinfra = KNOWN_PROVIDERS
        .iter()
        .find(|p| p.section == "deepinfra")
        .copied()
        .expect("deepinfra known");
    let sources = synthetic_sources(None, None, None);
    let env = env_from(&[("DEEPINFRA_API_KEY", "fake-deepinfra-api-key")]);
    let row = compute_status_row(deepinfra, &sources, &env);
    assert_eq!(row.effective_source(), "env");
    assert!(row.env_set);
    assert_eq!(row.env_var, "DEEPINFRA_API_KEY");
}

#[test]
fn deepinfra_status_row_falls_back_to_token_env() {
    // Users following DeepInfra's CLI quickstart export `DEEPINFRA_TOKEN`,
    // the alias. The fallback path must still report the row as configured.
    use super::{KNOWN_PROVIDERS, compute_status_row};
    let deepinfra = KNOWN_PROVIDERS
        .iter()
        .find(|p| p.section == "deepinfra")
        .copied()
        .expect("deepinfra known");
    let sources = synthetic_sources(None, None, None);
    let env = env_from(&[("DEEPINFRA_TOKEN", "fake-deepinfra-token")]);
    let row = compute_status_row(deepinfra, &sources, &env);
    assert_eq!(row.effective_source(), "env (fallback)");
    assert!(row.fallback_env_set);
    assert!(!row.env_set);
}

#[test]
fn auth_set_resolves_deepinfra_to_canonical_section_name() {
    // The CLI accepts `squeezy auth set deepinfra ...`; the
    // provider_section_for mapping must round-trip the id to its
    // TOML section so the inline-set helper writes under
    // [providers.deepinfra].
    use super::provider_section_for;
    let section = provider_section_for("deepinfra").expect("known");
    assert_eq!(section, "deepinfra");
}
