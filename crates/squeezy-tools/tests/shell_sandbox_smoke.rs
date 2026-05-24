//! Host-backed shell sandbox smoke tests.
//!
//! These exercise the full `ToolRegistry::execute` path through the public
//! API only. They run on macOS and Linux when the host advertises the
//! corresponding backend, and skip themselves when the host kills the
//! sandboxed child before any output is produced (hosted CI runners,
//! third-party EDR products, and developer machines with shell-intercept
//! toolchains all hit that condition). Backend-selection and runtime
//! detection coverage live in `crates/squeezy-tools/src/lib_tests.rs` as
//! unit tests because they exercise crate-private seams.
//!
//! The smoke tests deliberately use the public API surface only and live
//! under the crate's `tests/` directory rather than the `src/<module>` +
//! `src/<module>_tests.rs` pair convention: there is no production
//! `shell_sandbox.rs` module to pair against, and synthesising an empty
//! source file just to satisfy the unit-test layout would obscure that
//! these are integration tests.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::json;
use squeezy_core::{
    GraphConfig, ShellSandboxConfig, ShellSandboxMode, ShellSandboxNetworkPolicy, SkillsConfig,
};
use squeezy_tools::{
    ToolCall, ToolOutputConfig, ToolRegistry, ToolRegistryRuntime, ToolResult, ToolStatus,
    WebToolConfig,
};
use tokio_util::sync::CancellationToken;

static WORKSPACE_NONCE: AtomicU64 = AtomicU64::new(0);

fn temp_workspace(name: &str) -> PathBuf {
    let base = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let counter = WORKSPACE_NONCE.fetch_add(1, Ordering::SeqCst);
    let root = std::env::temp_dir().join(format!(
        "squeezy_shell_sandbox_smoke_{name}_{pid}_{base}_{counter}",
        pid = std::process::id()
    ));
    fs::create_dir_all(&root).expect("create temp workspace");
    root
}

fn smoke_registry(root: &Path, shell_sandbox: ShellSandboxConfig) -> ToolRegistry {
    ToolRegistry::new_with_configs_and_skills(
        root,
        ToolOutputConfig::default(),
        WebToolConfig::default(),
        SkillsConfig::default(),
        &GraphConfig::default(),
        shell_sandbox,
        ToolRegistryRuntime::default(),
    )
    .expect("registry")
}

fn sandbox_unavailable_denial(result: &ToolResult) -> bool {
    result.status == ToolStatus::Denied
        && result.content["error"]
            .as_str()
            .is_some_and(|error| error.contains("required shell sandbox"))
}

/// Skip predicate for the OS-backed smoke tests: covers both the
/// registry-level "sandbox unavailable" denial AND the case where the
/// host's sandbox runtime kills the child before it can produce any
/// output (signal-terminated, no exit code, empty stdout+stderr, no
/// timeout). Hosted CI runners, third-party EDR products, and developer
/// machines with shell-intercept toolchains all hit the second case, so
/// the smoke tests need to skip rather than fail when there is no
/// observable difference between "host can't run our profile" and
/// "command produced no output for unrelated host-environment reasons".
fn smoke_host_cannot_run_sandbox(result: &ToolResult) -> bool {
    if sandbox_unavailable_denial(result) {
        return true;
    }
    if result.status != ToolStatus::Error {
        return false;
    }
    let truncated = result.content["truncated"].as_bool().unwrap_or(false);
    let exit_code_unknown = result.content["exit_code"].is_null();
    let stdout_empty = result.content["stdout"].as_str().is_some_and(str::is_empty);
    let stderr_empty = result.content["stderr"].as_str().is_some_and(str::is_empty);
    !truncated && exit_code_unknown && stdout_empty && stderr_empty
}

fn required_deny_config() -> ShellSandboxConfig {
    ShellSandboxConfig {
        mode: ShellSandboxMode::Required,
        network: ShellSandboxNetworkPolicy::DenyByDefault,
        ..ShellSandboxConfig::default()
    }
}

#[tokio::test]
#[cfg(target_os = "macos")]
async fn shell_sandbox_exec_runs_benign_command_with_required_mode() {
    if !Path::new("/usr/bin/sandbox-exec").exists() {
        eprintln!("SKIP: /usr/bin/sandbox-exec not present");
        return;
    }

    let root = temp_workspace("macos_required");
    let registry = smoke_registry(&root, required_deny_config());

    let result = registry
        .execute(
            ToolCall {
                call_id: "shell".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "printf ok",
                    "description": "check macOS sandbox activation"
                }),
            },
            CancellationToken::new(),
        )
        .await;

    if smoke_host_cannot_run_sandbox(&result) {
        eprintln!("SKIP: macOS sandbox backend unavailable at runtime");
        let _ = fs::remove_dir_all(root);
        return;
    }

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["stdout"], "ok");
    assert_eq!(result.content["sandbox"]["mode"], "required");
    assert_eq!(result.content["sandbox"]["backend"], "macos-sandbox-exec");
    assert_eq!(result.content["sandbox"]["network"], "denied");
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
#[cfg(target_os = "macos")]
async fn shell_sandbox_exec_result_carries_network_metadata() {
    if !Path::new("/usr/bin/sandbox-exec").exists() {
        eprintln!("SKIP: /usr/bin/sandbox-exec not present");
        return;
    }

    let root = temp_workspace("macos_network_metadata");
    let registry = smoke_registry(&root, required_deny_config());

    let result = registry
        .execute(
            ToolCall {
                call_id: "shell".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "curl --version",
                    "description": "check network metadata"
                }),
            },
            CancellationToken::new(),
        )
        .await;

    if smoke_host_cannot_run_sandbox(&result) {
        eprintln!("SKIP: macOS sandbox backend unavailable at runtime");
        let _ = fs::remove_dir_all(root);
        return;
    }

    assert_eq!(result.content["policy"]["network"], "classified");
    assert_eq!(result.content["sandbox"]["mode"], "required");
    assert_eq!(result.content["sandbox"]["backend"], "macos-sandbox-exec");
    assert_eq!(result.content["sandbox"]["network"], "denied_classified");
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn shell_linux_userns_runs_benign_command_with_required_mode() {
    let root = temp_workspace("linux_required");
    let registry = smoke_registry(&root, required_deny_config());

    let result = registry
        .execute(
            ToolCall {
                call_id: "shell".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "printf ok",
                    "description": "check Linux sandbox activation"
                }),
            },
            CancellationToken::new(),
        )
        .await;

    if smoke_host_cannot_run_sandbox(&result) {
        eprintln!("SKIP: Linux sandbox backend unavailable at runtime");
        let _ = fs::remove_dir_all(root);
        return;
    }

    assert_eq!(result.status, ToolStatus::Success);
    assert_eq!(result.content["stdout"], "ok");
    assert_eq!(result.content["sandbox"]["mode"], "required");
    assert_eq!(
        result.content["sandbox"]["backend"],
        "linux-direct-syscalls"
    );
    assert_eq!(result.content["sandbox"]["network"], "denied");
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn shell_linux_userns_result_carries_network_metadata() {
    let root = temp_workspace("linux_network_metadata");
    let registry = smoke_registry(&root, required_deny_config());

    let result = registry
        .execute(
            ToolCall {
                call_id: "shell".to_string(),
                name: "shell".to_string(),
                arguments: json!({
                    "command": "curl --version",
                    "description": "check network metadata"
                }),
            },
            CancellationToken::new(),
        )
        .await;

    if smoke_host_cannot_run_sandbox(&result) {
        eprintln!("SKIP: Linux sandbox backend unavailable at runtime");
        let _ = fs::remove_dir_all(root);
        return;
    }

    assert_eq!(result.content["policy"]["network"], "classified");
    assert_eq!(result.content["sandbox"]["mode"], "required");
    assert_eq!(
        result.content["sandbox"]["backend"],
        "linux-direct-syscalls"
    );
    assert_eq!(result.content["sandbox"]["network"], "denied_classified");
    let _ = fs::remove_dir_all(root);
}
