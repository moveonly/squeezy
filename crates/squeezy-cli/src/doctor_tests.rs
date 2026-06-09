use super::*;
use squeezy_core::{McpPermissionConfig, McpServerConfig, McpTransport, ProviderSettings};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

// env::set_var/remove_var is process-global; serialize these tests so a parallel
// runner does not let them race.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Point `SQUEEZY_CREDENTIALS_FILE` at a guaranteed-absent path so the
/// credentials.json tier returns None and the developer's real
/// `~/.squeezy/credentials.json` cannot shadow the env tier under test.
fn isolate_credentials_file() {
    unsafe {
        env::set_var(
            "SQUEEZY_CREDENTIALS_FILE",
            std::env::temp_dir().join("squeezy-doctor-no-such-creds.json"),
        );
        env::remove_var("SQUEEZY_CREDENTIALS_JSON");
    }
}

fn with_session_env<R>(
    home: Option<&Path>,
    xdg_state_home: Option<&Path>,
    body: impl FnOnce() -> R,
) -> R {
    let _guard = ENV_LOCK.lock().expect("env lock");
    let previous_home = env::var_os("HOME");
    let previous_xdg = env::var_os("XDG_STATE_HOME");
    unsafe {
        match home {
            Some(path) => env::set_var("HOME", path),
            None => env::remove_var("HOME"),
        }
        match xdg_state_home {
            Some(path) => env::set_var("XDG_STATE_HOME", path),
            None => env::remove_var("XDG_STATE_HOME"),
        }
    }
    let result = body();
    unsafe {
        match previous_home {
            Some(value) => env::set_var("HOME", value),
            None => env::remove_var("HOME"),
        }
        match previous_xdg {
            Some(value) => env::set_var("XDG_STATE_HOME", value),
            None => env::remove_var("XDG_STATE_HOME"),
        }
    }
    result
}

#[test]
fn credential_check_reports_ok_when_env_set() {
    let _guard = ENV_LOCK.lock().expect("env lock");
    isolate_credentials_file();
    // SAFETY: the lock above serializes mutations to the process env.
    unsafe {
        env::set_var("SQUEEZY_DOCTOR_TEST_KEY", "1");
    }
    let (status, detail) = credential_check(None, "SQUEEZY_DOCTOR_TEST_KEY");
    unsafe {
        env::remove_var("SQUEEZY_DOCTOR_TEST_KEY");
    }
    assert_eq!(status, Status::Ok);
    assert!(detail.contains("SQUEEZY_DOCTOR_TEST_KEY"));
}

#[test]
fn status_filter_does_not_make_hidden_failures_exit_zero() {
    let mut args = DoctorArgs::default();
    args.status.push(DoctorStatusFilter::Ok);
    let checks = vec![
        Check {
            name: "config".to_string(),
            status: Status::Fail,
            detail: "broken".to_string(),
            extra: None,
        },
        Check {
            name: "sandbox".to_string(),
            status: Status::Ok,
            detail: "available".to_string(),
            extra: None,
        },
    ];

    assert_eq!(exit_code_for_checks(&checks), 1);
    let visible = filter_checks(&args, checks);
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].name, "sandbox");
}

#[test]
fn json_summary_counts_full_result_even_when_rows_are_filtered() {
    let report = DoctorReport {
        exit_code: 1,
        warnings: 0,
        failures: 1,
        checks: vec![Check {
            name: "sandbox".to_string(),
            status: Status::Ok,
            detail: "available".to_string(),
            extra: None,
        }],
        version: "test",
        target: "test-target",
        json: true,
        paths: DoctorPaths::default(),
    };

    let body = report.json_body();
    assert_eq!(body["ok"], false);
    assert_eq!(body["failures"], 1);
    assert_eq!(body["checks"].as_array().expect("checks").len(), 1);
    assert_eq!(body["checks"][0]["name"], "sandbox");
}

#[test]
fn unmatched_only_selector_becomes_visible_failure() {
    // Mimics the surfacing-after-status-filter contract that the
    // `run()` post-pass enforces: an unknown selector must remain
    // visible regardless of what `--status` allows through. We
    // exercise `unmatched_selector_checks` directly because that is
    // the unit producing the Fail row; the unconditional re-include
    // step in `run()` consumes its output.
    let mut args = DoctorArgs::default();
    args.only.push("sesion_store".to_string());
    let checks = vec![Check {
        name: "session_store".to_string(),
        status: Status::Ok,
        detail: "ok".to_string(),
        extra: None,
    }];

    let selector_failures = unmatched_selector_checks(&args, &checks, false);
    assert_eq!(selector_failures.len(), 1);
    assert_eq!(selector_failures[0].name, "selector");
    assert_eq!(selector_failures[0].status, Status::Fail);
    assert!(selector_failures[0].detail.contains("sesion_store"));
}

#[test]
fn unmatched_selector_when_config_failed_warns_instead_of_silently_dropping() {
    // Regression for the "config failed → selector silently dropped"
    // behavior: when the user asks `--only providers session_store`
    // against a broken config we still need to surface that those
    // checks did not run, so the user knows to fix configuration
    // rather than wonder where their selectors went.
    let mut args = DoctorArgs::default();
    args.only.push("providers".to_string());
    args.only.push("session_store".to_string());
    let checks: Vec<Check> = Vec::new();

    let selector_failures = unmatched_selector_checks(&args, &checks, true);
    assert_eq!(
        selector_failures.len(),
        1,
        "expected a single warn row, got {selector_failures:?}"
    );
    assert_eq!(selector_failures[0].name, "selector:skipped");
    assert_eq!(selector_failures[0].status, Status::Warn);
    assert!(selector_failures[0].detail.contains("providers"));
    assert!(selector_failures[0].detail.contains("session_store"));
    assert!(selector_failures[0].detail.contains("config"));
}

#[test]
fn unmatched_selector_after_config_load_passes_still_fails_on_typo() {
    // With `config_failed = false` (the normal path) a misspelled
    // selector still goes to the hard-fail bucket regardless of which
    // checks list it would have matched. This is the half of the
    // contract that the `config_failed` warn-only fork must not
    // weaken.
    let mut args = DoctorArgs::default();
    args.only.push("sandbx".to_string());
    let checks = vec![Check {
        name: "sandbox".to_string(),
        status: Status::Ok,
        detail: "ok".to_string(),
        extra: None,
    }];

    let selector_failures = unmatched_selector_checks(&args, &checks, false);
    assert_eq!(selector_failures.len(), 1);
    assert_eq!(selector_failures[0].name, "selector");
    assert_eq!(selector_failures[0].status, Status::Fail);
    assert!(selector_failures[0].detail.contains("sandbx"));
}

#[test]
fn credential_check_warns_when_unresolved() {
    let _guard = ENV_LOCK.lock().expect("env lock");
    isolate_credentials_file();
    unsafe {
        env::remove_var("SQUEEZY_DOCTOR_TEST_MISSING");
    }
    let (status, _) = credential_check(None, "SQUEEZY_DOCTOR_TEST_MISSING");
    assert_eq!(status, Status::Warn);
}

#[test]
fn credential_check_ok_for_inline_key() {
    let _guard = ENV_LOCK.lock().expect("env lock");
    isolate_credentials_file();
    unsafe {
        env::remove_var("SQUEEZY_DOCTOR_TEST_INLINE_KEY");
    }
    let (status, detail) = credential_check(Some("sk-inline"), "SQUEEZY_DOCTOR_TEST_INLINE_KEY");
    assert_eq!(status, Status::Ok);
    assert!(detail.contains("inline"), "detail: {detail}");
}

// Regression for #255: with the squeezy-prefixed env var unset but the
// conventional vendor fallback (OPENAI_API_KEY) set, the active-provider
// row must resolve Ok and name the fallback source instead of warning.
#[test]
fn credential_check_ok_via_fallback_env_var() {
    let _guard = ENV_LOCK.lock().expect("env lock");
    isolate_credentials_file();
    unsafe {
        env::remove_var("SQUEEZY_OPENAI_KEY");
        env::set_var("OPENAI_API_KEY", "sk-from-vendor");
    }
    let (status, detail) = credential_check(None, "SQUEEZY_OPENAI_KEY");
    unsafe {
        env::remove_var("OPENAI_API_KEY");
    }
    assert_eq!(status, Status::Ok, "detail: {detail}");
    assert!(detail.contains("OPENAI_API_KEY"), "detail: {detail}");
}

#[test]
fn probe_writable_round_trips_in_tempdir() {
    let dir = std::env::temp_dir().join(format!("squeezy-doctor-probe-{}", std::process::id(),));
    let _ = fs::remove_dir_all(&dir);
    probe_writable(&dir).expect("probe");
    // probe file should have been cleaned up
    assert!(!dir.join(".squeezy-doctor-probe").exists());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn session_paths_check_reports_absolute_xdg_without_home() {
    let root = skills_doctor_workspace("session_paths_xdg_no_home");
    let xdg = root.join("xdg-state");
    std::fs::create_dir_all(&xdg).expect("mkdir xdg");
    with_session_env(None, Some(&xdg), || {
        let config = AppConfig {
            workspace_root: root.clone(),
            ..AppConfig::default()
        };
        let checks = session_paths_checks(&config);
        let home = checks
            .iter()
            .find(|check| check.name == "session_home")
            .expect("HOME warning");
        assert_eq!(home.status, Status::Warn);
        assert!(
            home.detail.contains("global index uses XDG_STATE_HOME"),
            "{home:?}"
        );
        let paths = checks
            .iter()
            .find(|check| check.name == "session_paths")
            .expect("session paths row");
        assert!(
            paths.detail.contains("XDG_STATE_HOME honored")
                && paths.detail.contains("memory=unavailable"),
            "{paths:?}"
        );
    });
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn session_paths_check_warns_on_relative_xdg_state_home() {
    let root = skills_doctor_workspace("session_paths_relative_xdg");
    let home = root.join("home");
    std::fs::create_dir_all(&home).expect("mkdir home");
    let relative_xdg = Path::new("relative-state");
    with_session_env(Some(&home), Some(relative_xdg), || {
        let config = AppConfig {
            workspace_root: root.clone(),
            ..AppConfig::default()
        };
        let checks = session_paths_checks(&config);
        let xdg = checks
            .iter()
            .find(|check| check.name == "session_xdg_state_home")
            .expect("XDG warning");
        assert_eq!(xdg.status, Status::Warn);
        assert!(xdg.detail.contains("not absolute"), "{xdg:?}");
        let paths = checks
            .iter()
            .find(|check| check.name == "session_paths")
            .expect("session paths row");
        assert!(
            paths.detail.contains(
                &home
                    .join(".squeezy")
                    .join("sessions")
                    .join("index.jsonl")
                    .display()
                    .to_string()
            ),
            "{paths:?}"
        );
        assert!(
            !paths.detail.contains("XDG_STATE_HOME honored"),
            "relative XDG_STATE_HOME must not be reported as honored: {paths:?}"
        );
    });
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn settings_path_is_repo_local_detects_workspace_child() {
    assert!(settings_path_is_repo_local(
        Path::new("/tmp/repo/.squeezy/settings.toml"),
        Path::new("/tmp/repo"),
    ));
}

#[test]
fn settings_path_is_repo_local_rejects_sibling_prefix() {
    assert!(!settings_path_is_repo_local(
        Path::new("/tmp/repo-other/.squeezy/settings.toml"),
        Path::new("/tmp/repo"),
    ));
}

#[test]
fn settings_path_is_repo_local_detects_relative_fallback() {
    assert!(settings_path_is_repo_local(
        Path::new("./.squeezy/settings.toml"),
        Path::new("/tmp/repo"),
    ));
}

fn mcp_fixture(enabled: bool, transport: McpTransport) -> McpServerConfig {
    McpServerConfig {
        enabled,
        transport,
        command: None,
        args: Vec::new(),
        url: None,
        timeout_ms: None,
        discovery_timeout_ms: None,
        tool_call_timeout_ms: None,
        enabled_tools: None,
        disabled_tools: Vec::new(),
        env: BTreeMap::new(),
        permissions: McpPermissionConfig::default(),
        bearer_token_env_var: None,
        http_headers: BTreeMap::new(),
        env_http_headers: BTreeMap::new(),
        cwd: None,
    }
}

#[test]
fn mcp_check_with_no_servers_is_ok() {
    let check = mcp_check(&BTreeMap::new());
    assert_eq!(check.status, Status::Ok);
    assert!(check.detail.contains("no MCP servers"));
}

#[test]
fn mcp_check_warns_on_stdio_without_command() {
    let mut servers = BTreeMap::new();
    servers.insert("noisy".to_string(), mcp_fixture(true, McpTransport::Stdio));
    let check = mcp_check(&servers);
    assert_eq!(check.status, Status::Warn);
    assert!(check.detail.contains("stdio transport without command"));
    assert!(check.detail.contains("enabled=1"));
}

#[test]
fn mcp_check_warns_on_http_without_url() {
    let mut servers = BTreeMap::new();
    let mut server = mcp_fixture(true, McpTransport::Http);
    server.url = Some("   ".to_string());
    servers.insert("remote".to_string(), server);
    let check = mcp_check(&servers);
    assert_eq!(check.status, Status::Warn);
    assert!(check.detail.contains("http transport without url"));
}

#[test]
fn mcp_check_accepts_disabled_server_without_command() {
    let mut servers = BTreeMap::new();
    servers.insert("idle".to_string(), mcp_fixture(false, McpTransport::Stdio));
    let check = mcp_check(&servers);
    assert_eq!(check.status, Status::Ok);
    assert!(check.detail.contains("disabled=1"));
    assert!(check.detail.contains("enabled=0"));
}

#[tokio::test]
async fn probe_mcp_reports_unreachable_stdio_server_as_fail() {
    let mut servers = BTreeMap::new();
    let mut broken = mcp_fixture(true, McpTransport::Stdio);
    // A command that does not exist on PATH fails to spawn immediately, so the
    // initialize handshake errors out without hanging on a timeout.
    broken.command = Some("squeezy-doctor-no-such-mcp-binary-xyzzy".to_string());
    servers.insert("broken".to_string(), broken);
    // A disabled server must be skipped entirely: no probe row.
    servers.insert("idle".to_string(), mcp_fixture(false, McpTransport::Stdio));

    let checks = probe_mcp_servers(&servers, &DoctorArgs::default()).await;

    let broken_row = checks
        .iter()
        .find(|check| check.name == "probe:mcp:broken")
        .expect("enabled server must produce a probe row");
    assert_eq!(broken_row.status, Status::Fail);
    assert!(broken_row.detail.contains("handshake failed"));
    assert!(
        checks.iter().all(|check| check.name != "probe:mcp:idle"),
        "disabled servers must not be probed"
    );
}

fn skills_doctor_workspace(name: &str) -> std::path::PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("squeezy_doctor_{name}_{nonce}"));
    std::fs::create_dir_all(&root).expect("mkdir");
    root
}

fn skills_doctor_config(root: &std::path::Path) -> AppConfig {
    AppConfig {
        workspace_root: root.to_path_buf(),
        skills: squeezy_core::SkillsConfig {
            user_dir: root.join("user"),
            compat_user_dir: root.join("compat"),
            ..Default::default()
        },
        ..Default::default()
    }
}

#[test]
fn skills_check_with_no_skills_is_ok() {
    let root = skills_doctor_workspace("skills_empty");
    let config = skills_doctor_config(&root);
    let catalog = squeezy_skills::SkillCatalog::discover(&config.workspace_root, &config.skills);

    let check = skills_check(&config, &catalog);
    assert_eq!(check.status, Status::Ok);
    assert!(check.detail.contains("no skills discovered"), "{check:?}");

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn skills_check_reports_enabled_count() {
    let root = skills_doctor_workspace("skills_counts");
    let skill_dir = root.join(".agents/skills/example");
    std::fs::create_dir_all(&skill_dir).expect("mkdir");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: example\ndescription: \"d\"\n---\n# example\n",
    )
    .expect("write skill");

    let config = skills_doctor_config(&root);
    let catalog = squeezy_skills::SkillCatalog::discover(&config.workspace_root, &config.skills);

    let check = skills_check(&config, &catalog);
    assert_eq!(check.status, Status::Ok);
    assert!(
        check.detail.contains("enabled=1") && check.detail.contains("disabled=0"),
        "{check:?}"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn skills_check_warns_on_ambiguous_same_precedence_names() {
    let root = skills_doctor_workspace("skills_ambiguous");
    let agents_dir = root.join(".agents/skills/dup");
    std::fs::create_dir_all(&agents_dir).expect("mkdir agents");
    std::fs::write(
        agents_dir.join("SKILL.md"),
        "---\nname: dup\ndescription: \"first\"\n---\n# dup-first\n",
    )
    .expect("write first dup");
    let agents_dir2 = root.join(".agents/skills/dup-other");
    std::fs::create_dir_all(&agents_dir2).expect("mkdir agents2");
    std::fs::write(
        agents_dir2.join("SKILL.md"),
        "---\nname: dup\ndescription: \"second\"\n---\n# dup-second\n",
    )
    .expect("write second dup");

    let config = skills_doctor_config(&root);
    let catalog = squeezy_skills::SkillCatalog::discover(&config.workspace_root, &config.skills);

    let check = skills_check(&config, &catalog);
    assert_eq!(check.status, Status::Warn);
    assert!(check.detail.contains("ambiguous"), "{check:?}");
    assert!(check.detail.contains("dup"), "{check:?}");

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn mcp_check_is_ok_when_fields_match_transport() {
    let mut servers = BTreeMap::new();
    let mut stdio = mcp_fixture(true, McpTransport::Stdio);
    // Use the running test binary itself — guaranteed to exist and be
    // executable on every CI platform, so the PATH/exec-bit check passes.
    let test_exe = std::env::current_exe()
        .expect("current_exe")
        .to_string_lossy()
        .into_owned();
    stdio.command = Some(test_exe);
    servers.insert("local".to_string(), stdio);
    let mut http = mcp_fixture(true, McpTransport::Sse);
    http.url = Some("https://example.test/mcp".to_string());
    servers.insert("remote".to_string(), http);
    let check = mcp_check(&servers);
    assert_eq!(check.status, Status::Ok, "detail: {}", check.detail);
    assert!(check.detail.contains("enabled=2"));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_sandbox_detail_fails_when_required_backend_unavailable() {
    let check = linux_sandbox_check_from_report(squeezy_tools::ShellSandboxDoctor {
        backend: "linux-direct-syscalls",
        available: false,
        detail: "user namespaces disabled".to_string(),
        linux_user_namespaces: Some(false),
        linux_landlock_abi: Some(0),
        linux_seccomp_available: Some(false),
        linux_ask_socket_blocked: Some(false),
        userns: Some(false),
        landlock: Some(false),
        fallback_reason: Some("user namespaces disabled".to_string()),
    });

    assert_eq!(check.name, "linux-sandbox");
    assert_eq!(check.status, Status::Fail);
    assert!(check.detail.contains("linux-direct-syscalls"));
    assert!(check.detail.contains("available=false"));
}

#[cfg(not(target_os = "linux"))]
#[test]
fn linux_sandbox_detail_warns_on_non_linux_platforms() {
    let check = linux_sandbox_check_from_report(squeezy_tools::ShellSandboxDoctor {
        backend: "test-backend",
        available: true,
        detail: "active backend detail".to_string(),
        linux_user_namespaces: None,
        linux_landlock_abi: None,
        linux_seccomp_available: None,
        linux_ask_socket_blocked: None,
        userns: None,
        landlock: None,
        fallback_reason: None,
    });

    assert_eq!(check.name, "linux-sandbox");
    assert_eq!(check.status, Status::Warn);
    assert!(check.detail.contains("only available on Linux"));
    assert!(check.detail.contains("active backend=test-backend"));
}

#[test]
fn mcp_check_warns_when_stdio_command_not_on_path() {
    let mut servers = BTreeMap::new();
    let mut server = mcp_fixture(true, McpTransport::Stdio);
    server.command = Some("squeezy-doctor-no-such-mcp-binary-xyzzy-abc".to_string());
    servers.insert("missing".to_string(), server);
    let check = mcp_check(&servers);
    assert_eq!(check.status, Status::Warn, "detail: {}", check.detail);
    assert!(
        check.detail.contains("not found on PATH"),
        "detail: {}",
        check.detail
    );
}

#[cfg(unix)]
#[test]
fn mcp_check_warns_when_stdio_command_not_executable() {
    let dir = std::env::temp_dir().join(format!(
        "squeezy-mcp-check-noexec-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    fs::create_dir_all(&dir).expect("create dir");
    let noexec = dir.join("noexec-server");
    fs::write(&noexec, b"#!/bin/sh\nexit 0\n").expect("write script");
    // Leave execute bit unset.
    let mut servers = BTreeMap::new();
    let mut server = mcp_fixture(true, McpTransport::Stdio);
    server.command = Some(noexec.to_string_lossy().into_owned());
    servers.insert("noexec".to_string(), server);
    let check = mcp_check(&servers);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(check.status, Status::Warn, "detail: {}", check.detail);
    assert!(
        check.detail.contains("not executable"),
        "detail: {}",
        check.detail
    );
}

#[cfg(unix)]
#[test]
fn mcp_check_warns_when_stdio_command_is_directory() {
    let dir = std::env::temp_dir().join(format!(
        "squeezy-mcp-check-directory-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    fs::create_dir_all(&dir).expect("create dir");
    let mut servers = BTreeMap::new();
    let mut server = mcp_fixture(true, McpTransport::Stdio);
    server.command = Some(dir.to_string_lossy().into_owned());
    servers.insert("directory".to_string(), server);
    let check = mcp_check(&servers);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(check.status, Status::Warn, "detail: {}", check.detail);
    assert!(
        check.detail.contains("not a file"),
        "detail: {}",
        check.detail
    );
}

#[test]
fn providers_check_reports_no_sections() {
    let settings = SettingsFile::default();
    let check = providers_check(&settings);
    assert_eq!(check.status, Status::Ok);
    assert!(check.detail.contains("no [providers.*]"));
}

#[test]
fn providers_check_marks_inline_api_key_configured() {
    let _guard = ENV_LOCK.lock().expect("env lock");
    let mut providers = BTreeMap::new();
    providers.insert(
        "openai".to_string(),
        ProviderSettings {
            api_key: Some("sk-test".to_string()),
            ..ProviderSettings::default()
        },
    );
    let settings = SettingsFile {
        providers: Some(providers),
        ..SettingsFile::default()
    };
    let check = providers_check(&settings);
    assert_eq!(check.status, Status::Ok);
    assert!(check.detail.contains("openai=configured"));
}

#[test]
fn providers_check_warns_when_env_unset() {
    let _guard = ENV_LOCK.lock().expect("env lock");
    unsafe {
        env::remove_var("SQUEEZY_DOCTOR_TEST_OPENAI_KEY");
    }
    let mut providers = BTreeMap::new();
    providers.insert(
        "openai".to_string(),
        ProviderSettings {
            api_key_env: Some("SQUEEZY_DOCTOR_TEST_OPENAI_KEY".to_string()),
            ..ProviderSettings::default()
        },
    );
    let settings = SettingsFile {
        providers: Some(providers),
        ..SettingsFile::default()
    };
    let check = providers_check(&settings);
    assert_eq!(check.status, Status::Warn);
    assert!(check.detail.contains("openai=missing api_key"));
}

#[test]
fn providers_check_treats_bedrock_and_ollama_as_keyless() {
    let _guard = ENV_LOCK.lock().expect("env lock");
    let mut providers = BTreeMap::new();
    providers.insert("bedrock".to_string(), ProviderSettings::default());
    providers.insert("ollama".to_string(), ProviderSettings::default());
    let settings = SettingsFile {
        providers: Some(providers),
        ..SettingsFile::default()
    };
    let check = providers_check(&settings);
    assert_eq!(check.status, Status::Ok);
    assert!(check.detail.contains("bedrock=keyless"));
    assert!(check.detail.contains("ollama=keyless"));
}

#[test]
fn state_store_check_opens_redb_in_tempdir() {
    let workspace = std::env::temp_dir().join(format!(
        "squeezy-doctor-state-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&workspace);
    fs::create_dir_all(&workspace).expect("create workspace");
    let mut config = AppConfig::from_env();
    config.workspace_root = workspace.clone();
    config.cache.root = None;
    let check = state_store_check(&config);
    let _ = fs::remove_dir_all(&workspace);
    assert_eq!(check.status, Status::Ok, "detail: {}", check.detail);
    assert!(check.detail.contains("opened"));
}

#[test]
fn graph_store_check_reports_absent_without_creating_redb() {
    let workspace = std::env::temp_dir().join(format!(
        "squeezy-doctor-graph-absent-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&workspace);
    fs::create_dir_all(&workspace).expect("create workspace");
    let mut config = AppConfig::from_env();
    config.workspace_root = workspace.clone();
    config.cache.root = None;
    let check = graph_store_check(&config);
    let graph = squeezy_store::graph_path(&workspace, None);
    let _ = fs::remove_dir_all(&workspace);
    assert_eq!(check.status, Status::Ok, "detail: {}", check.detail);
    assert!(check.detail.contains("absent"));
    assert!(
        !graph.exists(),
        "doctor graph probe must not create graph.redb"
    );
}

#[test]
fn storage_error_hint_classifies_common_failures() {
    assert_eq!(
        storage_error_hint("database lock would block"),
        "likely lock contention"
    );
    assert_eq!(
        storage_error_hint("permission denied"),
        "likely permission problem"
    );
    assert_eq!(
        storage_error_hint("No space left on device"),
        "likely disk full"
    );
    assert_eq!(
        storage_error_hint("invalid database checksum"),
        "possible redb corruption"
    );
    assert_eq!(
        storage_error_hint("operation not supported"),
        "possible unsupported filesystem behavior"
    );
}

#[test]
fn graph_store_check_opens_existing_redb_in_tempdir() {
    let workspace = std::env::temp_dir().join(format!(
        "squeezy-doctor-graph-existing-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&workspace);
    fs::create_dir_all(&workspace).expect("create workspace");
    let store = squeezy_store::GraphStore::open(&workspace, None).expect("seed graph store");
    drop(store);
    let mut config = AppConfig::from_env();
    config.workspace_root = workspace.clone();
    config.cache.root = None;
    let check = graph_store_check(&config);
    let _ = fs::remove_dir_all(&workspace);
    assert_eq!(check.status, Status::Ok, "detail: {}", check.detail);
    assert!(check.detail.contains("readable"));
}

#[test]
fn user_global_storage_warns_for_synced_workspace_with_default_cache() {
    let mut config = AppConfig::from_env();
    // Use forward slashes so the path parses into multiple components on
    // both Windows and Unix; `workspace_looks_synced` is component-based
    // and the substring `onedrive` matches case-insensitively either way.
    config.workspace_root = PathBuf::from("/home/dev/OneDrive/repo");
    config.cache.root = None;
    let check = user_global_storage_check(&config);
    assert_eq!(check.status, Status::Warn, "detail: {}", check.detail);
    assert!(check.detail.contains("synced folder"));
    assert!(check.detail.contains("[cache].root"));
}

#[test]
fn workspace_looks_synced_matches_known_cloud_clients() {
    let positive = [
        "/home/dev/OneDrive/repo",
        "/home/dev/Dropbox/work/repo",
        "/Users/dev/Library/CloudStorage/GoogleDrive-me/repo",
        "/Users/dev/Library/CloudStorage/iCloud Drive/repo",
        "/home/dev/Nextcloud/code",
        "/home/dev/Syncthing/repo",
        "/home/dev/pCloud Drive/repo",
    ];
    for path in positive {
        assert!(
            workspace_looks_synced(std::path::Path::new(path)),
            "expected sync detection for {path}",
        );
    }
    let negative = [
        "/home/dev/code/squeezy",
        "/Users/dev/Documents/repo",
        "/tmp/sandbox",
        "/home/dev/toolbox",
    ];
    for path in negative {
        assert!(
            !workspace_looks_synced(std::path::Path::new(path)),
            "did not expect sync detection for {path}",
        );
    }
}

#[test]
fn cache_check_warns_about_redb_backups() {
    let workspace = std::env::temp_dir().join(format!(
        "squeezy-doctor-cache-warn-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let cache_dir = workspace.join(".squeezy").join("cache");
    fs::create_dir_all(&cache_dir).expect("create cache dir");
    fs::write(cache_dir.join("schema-2-test.redb.bak"), b"old").expect("write backup");
    let mut config = AppConfig::from_env();
    config.workspace_root = workspace.clone();
    config.cache.root = None;

    let check = cache_check(&config, false, false);

    let _ = fs::remove_dir_all(&workspace);
    assert_eq!(check.status, Status::Warn, "detail: {}", check.detail);
    assert!(check.detail.contains("backups=1"));
    assert!(check.detail.contains("--prune-cache"));
}

#[test]
fn cache_check_prunes_redb_backups() {
    let workspace = std::env::temp_dir().join(format!(
        "squeezy-doctor-cache-prune-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let cache_dir = workspace.join(".squeezy").join("cache");
    fs::create_dir_all(&cache_dir).expect("create cache dir");
    let backup = cache_dir.join("schema-2-test.redb.bak");
    fs::write(&backup, b"old").expect("write backup");
    let mut config = AppConfig::from_env();
    config.workspace_root = workspace.clone();
    config.cache.root = None;

    let check = cache_check(&config, true, false);

    assert_eq!(check.status, Status::Ok, "detail: {}", check.detail);
    assert!(check.detail.contains("pruned 1 backups"));
    assert!(!backup.exists(), "backup should be removed");
    let _ = fs::remove_dir_all(&workspace);
}

#[cfg(target_os = "linux")]
#[test]
fn cache_check_prune_preserves_storage_warning() {
    let workspace = std::env::temp_dir().join(format!(
        "squeezy-doctor-cache-prune-storage-warn-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let cache_dir = workspace.join(".squeezy").join("cache");
    fs::create_dir_all(&cache_dir).expect("create cache dir");
    let backup = cache_dir.join("schema-2-test.redb.bak");
    fs::write(&backup, b"old").expect("write backup");
    let mut config = AppConfig::from_env();
    config.workspace_root = workspace.clone();
    config.cache.root = None;
    config.session_logs.log_dir = Some(std::path::PathBuf::from("/proc/squeezy-sessions"));

    let check = cache_check(&config, true, true);

    assert_eq!(check.status, Status::Warn, "detail: {}", check.detail);
    assert!(
        check
            .detail
            .contains("storage warning: sessions=proc(virtual)"),
        "detail: {}",
        check.detail
    );
    assert!(check.detail.contains("pruned 1 backups"));
    assert!(!backup.exists(), "backup should be removed");
    let _ = fs::remove_dir_all(&workspace);
}

#[test]
fn cache_check_storage_reports_paths_and_backup_age() {
    let workspace = std::env::temp_dir().join(format!(
        "squeezy-doctor-cache-storage-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let cache_dir = workspace.join(".squeezy").join("cache");
    fs::create_dir_all(&cache_dir).expect("create cache dir");
    let backup = cache_dir.join("schema-2-test.redb.bak");
    fs::write(&backup, b"old").expect("write backup");
    let mut config = AppConfig::from_env();
    config.workspace_root = workspace.clone();
    config.cache.root = None;

    let check = cache_check(&config, false, true);

    let _ = fs::remove_dir_all(&workspace);
    assert_eq!(check.status, Status::Warn, "detail: {}", check.detail);
    assert!(
        check.detail.contains("storage:"),
        "detail: {}",
        check.detail
    );
    assert!(
        check.detail.contains("state.redb"),
        "detail: {}",
        check.detail
    );
    assert!(check.detail.contains("probes:"), "detail: {}", check.detail);
    assert!(
        check.detail.contains("graph.redb"),
        "detail: {}",
        check.detail
    );
    assert!(
        check
            .detail
            .contains("prune command: squeezy doctor --prune-cache"),
        "detail: {}",
        check.detail
    );
}

#[cfg(unix)]
#[test]
fn state_store_check_fails_when_path_unwritable() {
    // Pointing the cache at /dev/null forces SqueezyStore::open to fail.
    let mut config = AppConfig::from_env();
    config.workspace_root = std::env::temp_dir();
    config.cache.root = Some(PathBuf::from("/dev/null/nope"));
    let check = state_store_check(&config);
    assert_eq!(check.status, Status::Fail, "detail: {}", check.detail);
}

#[test]
fn skills_roots_check_shows_resolved_paths() {
    // Skip when HOME is absent; the warn path is covered by the next test.
    if std::env::var_os("HOME").is_none() {
        return;
    }
    let root = skills_doctor_workspace("skills_roots_paths");
    let config = skills_doctor_config(&root);
    let check = skills_roots_check(&config);
    assert_eq!(check.status, Status::Ok, "detail: {}", check.detail);
    assert!(
        check.detail.contains("user="),
        "expected resolved user path in detail: {check:?}"
    );
    assert!(
        check.detail.contains("project="),
        "expected resolved project path in detail: {check:?}"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn skills_roots_check_warns_when_roots_are_relative() {
    // Simulate the condition that arises when HOME is unset and skill roots
    // default to relative paths: construct a config with relative user_dir /
    // compat_user_dir and verify the check emits a warning.
    let root = skills_doctor_workspace("skills_roots_relative_warn");
    let config = AppConfig {
        workspace_root: root.clone(),
        skills: squeezy_core::SkillsConfig {
            // Relative paths — as would result from HOME being absent at
            // config-load time.
            user_dir: std::path::PathBuf::from(".squeezy/skills"),
            compat_user_dir: std::path::PathBuf::from(".agents/skills"),
            ..Default::default()
        },
        ..Default::default()
    };
    let check = skills_roots_check(&config);
    assert_eq!(check.status, Status::Warn, "detail: {}", check.detail);
    assert!(
        check.detail.contains("relative"),
        "expected 'relative' in detail: {check:?}"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn sandbox_check_includes_structured_extra() {
    let check = sandbox_check(None);
    let extra = check.extra.as_ref().expect("sandbox check must have extra");
    let backend = extra.get("backend").expect("extra must have backend field");
    assert!(backend.is_string(), "backend must be a string");
    assert!(
        extra.get("required_mode_supported").is_some(),
        "extra must have required_mode_supported"
    );
}

#[test]
fn sandbox_check_extra_backend_matches_detail() {
    let check = sandbox_check(None);
    let extra = check.extra.as_ref().expect("sandbox check must have extra");
    let backend = extra["backend"].as_str().expect("backend is a string");
    assert!(
        check.detail.contains(backend),
        "detail should mention the backend; detail={:?} backend={backend:?}",
        check.detail
    );
}
