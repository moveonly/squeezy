//! Integration smoke tests for the Windows restricted-token sandbox.
//!
//! These tests spawn real processes and check the sandbox access policy, so
//! they ONLY RUN on Windows.  On non-Windows hosts the entire file compiles to
//! nothing (the `#![cfg(windows)]` attribute gates every item).  The test
//! binary is still compiled during `cargo check --all-targets --target
//! x86_64-pc-windows-msvc` to keep it type-checked in CI.
//!
//! If the host cannot create restricted tokens (e.g. a CI container that lacks
//! the required privilege) each test prints a skip message and returns early
//! rather than panicking.
//!
//! Every test here is `#[ignore]`d: GitHub's hosted `windows-2022` runners
//! create restricted tokens but do not enforce the capability-SID write grant
//! (workspace writes are denied) or hang on reads, so these tests are flaky on
//! CI even though they pass on real Windows hosts. They are the runtime
//! acceptance gate documented in `docs/internal/windows-sandbox-qa.md` and are
//! meant to be run explicitly (`cargo nextest run --run-ignored all` /
//! `cargo test -- --ignored`) on a real Windows host. CI still compiles them
//! (via `cargo check --all-targets --target x86_64-pc-windows-msvc`) so they
//! stay type-checked.

#![cfg(windows)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use squeezy_win_sandbox::{
    WinNetwork, WinSandboxSpec, WinTokenMode, WinWritableRoot, spawn_restricted_token,
};
use tokio::io::AsyncReadExt;

// ── helpers ───────────────────────────────────────────────────────────────────

/// Build a [`WinSandboxSpec`] for a given workspace directory.
///
/// Uses the restricted-token (no-admin) tier with a single writable root and
/// no additional read restrictions or network enforcement.
fn make_spec(workspace: &Path) -> WinSandboxSpec {
    let state_dir = std::env::temp_dir().join("squeezy-wsbx-state");
    std::fs::create_dir_all(&state_dir).ok();

    WinSandboxSpec {
        token_mode: WinTokenMode::WritableRootsCapability,
        writable_roots: vec![WinWritableRoot::new(workspace)],
        read_roots: vec![],
        deny_read_paths: vec![],
        protected_metadata_names: vec![".git".into()],
        sensitive_path_patterns: vec![],
        network: WinNetwork::Unenforced,
        state_dir,
    }
}

/// Create a fresh temp workspace directory with a unique name.
fn fresh_workspace(tag: &str) -> PathBuf {
    // Use a counter derived from the thread id for uniqueness within a test run.
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("squeezy-wsbx-test-{tag}-{unique}"));
    std::fs::create_dir_all(&dir).expect("create workspace dir");
    dir
}

struct CommandOutcome {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

fn ps_quote_path(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "''"))
}

/// Run a PowerShell command inside the sandbox rooted at `workspace`. Returns
/// the child's exit status and captured output, or `None` if the spawn was
/// skipped because the host can't create restricted tokens.
fn run_powershell(workspace: &Path, command: &str) -> Option<CommandOutcome> {
    let spec = make_spec(workspace);
    let argv = vec![
        "powershell.exe".to_string(),
        "-NoLogo".to_string(),
        "-NoProfile".to_string(),
        "-Command".to_string(),
        command.to_string(),
    ];
    let env: HashMap<String, String> = std::env::vars().collect();

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let mut child = match spawn_restricted_token(&spec, &argv, workspace, &env, false) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[skip] spawn_restricted_token returned error: {e}");
            return None;
        }
    };

    let stdout = child.take_stdout();
    let stderr = child.take_stderr();
    let output = rt.block_on(async move {
        let stdout_task = tokio::spawn(async move {
            let mut bytes = Vec::new();
            if let Some(mut stdout) = stdout {
                stdout.read_to_end(&mut bytes).await?;
            }
            std::io::Result::Ok(bytes)
        });
        let stderr_task = tokio::spawn(async move {
            let mut bytes = Vec::new();
            if let Some(mut stderr) = stderr {
                stderr.read_to_end(&mut bytes).await?;
            }
            std::io::Result::Ok(bytes)
        });

        let status = match tokio::time::timeout(Duration::from_secs(30), child.wait()).await {
            Ok(status) => status?,
            Err(_) => {
                child.kill();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "sandbox child timed out",
                ));
            }
        };
        let stdout = stdout_task.await.map_err(std::io::Error::other)??;
        let stderr = stderr_task.await.map_err(std::io::Error::other)??;
        Ok::<_, std::io::Error>((status, stdout, stderr))
    });

    let (status, stdout, stderr) = output.expect("wait/capture failed");
    Some(CommandOutcome {
        status,
        stdout: String::from_utf8_lossy(&stdout).to_string(),
        stderr: String::from_utf8_lossy(&stderr).to_string(),
    })
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// A write inside the workspace must succeed.
#[test]
#[ignore = "runtime restricted-token gate; run on a real Windows host (see docs/internal/windows-sandbox-qa.md)"]
fn write_inside_workspace_allowed() {
    let workspace = fresh_workspace("write_inside");
    let out_file = workspace.join("out.txt");

    let command = format!(
        "Set-Content -LiteralPath {} -Value hi",
        ps_quote_path(&out_file)
    );
    let Some(outcome) = run_powershell(&workspace, &command) else {
        return;
    };

    assert!(
        outcome.status.success(),
        "write inside workspace should succeed; exit={:?}; stdout={:?}; stderr={:?}",
        outcome.status,
        outcome.stdout,
        outcome.stderr
    );
    assert!(
        out_file.exists(),
        "output file should exist after write inside workspace"
    );

    let _ = std::fs::remove_dir_all(&workspace);
}

/// A write to a path outside the workspace must be denied by the restricted token.
#[test]
#[ignore = "runtime restricted-token gate; run on a real Windows host (see docs/internal/windows-sandbox-qa.md)"]
fn write_outside_workspace_denied() {
    let workspace = fresh_workspace("write_outside_ws");

    // Pick a sibling directory that is NOT the workspace (and doesn't overlap
    // with it), so the restricted token's capability SID denies writes there.
    let outside_dir = std::env::temp_dir().join("squeezy-wsbx-outside-target");
    std::fs::create_dir_all(&outside_dir).ok();
    let evil_file = outside_dir.join("evil.txt");
    // Remove any leftover from a previous run.
    let _ = std::fs::remove_file(&evil_file);

    let command = format!(
        "Set-Content -LiteralPath {} -Value x",
        ps_quote_path(&evil_file)
    );
    // Ignore the exit status here: the authoritative signal is whether the
    // outside file was created.
    let Some(_outcome) = run_powershell(&workspace, &command) else {
        return;
    };

    let file_was_created = evil_file.exists();
    assert!(
        !file_was_created,
        "write outside workspace must be denied; file unexpectedly exists"
    );

    let _ = std::fs::remove_file(&evil_file);
    let _ = std::fs::remove_dir_all(&workspace);
}

/// Appending to a file inside the workspace must succeed.
#[test]
#[ignore = "runtime restricted-token gate; run on a real Windows host (see docs/internal/windows-sandbox-qa.md)"]
fn append_inside_allowed() {
    let workspace = fresh_workspace("append_inside");
    let target = workspace.join("append.txt");
    std::fs::write(&target, "line1\n").expect("seed file");

    let command = format!(
        "Add-Content -LiteralPath {} -Value line2",
        ps_quote_path(&target)
    );
    let Some(outcome) = run_powershell(&workspace, &command) else {
        return;
    };

    assert!(
        outcome.status.success(),
        "append inside workspace should succeed; exit={:?}; stdout={:?}; stderr={:?}",
        outcome.status,
        outcome.stdout,
        outcome.stderr
    );
    let contents = std::fs::read_to_string(&target).expect("read file after append");
    assert!(
        contents.contains("line2"),
        "file should contain appended text; got: {contents:?}"
    );

    let _ = std::fs::remove_dir_all(&workspace);
}

/// Deleting a file inside the workspace must succeed.
#[test]
#[ignore = "runtime restricted-token gate; run on a real Windows host (see docs/internal/windows-sandbox-qa.md)"]
fn delete_inside_allowed() {
    let workspace = fresh_workspace("delete_inside");
    let target = workspace.join("delme.txt");
    std::fs::write(&target, "x").expect("seed file");

    let command = format!("Remove-Item -LiteralPath {} -Force", ps_quote_path(&target));
    let Some(outcome) = run_powershell(&workspace, &command) else {
        return;
    };

    assert!(
        outcome.status.success(),
        "delete inside workspace should succeed; exit={:?}; stdout={:?}; stderr={:?}",
        outcome.status,
        outcome.stdout,
        outcome.stderr
    );
    assert!(
        !target.exists(),
        "file should be gone after delete inside workspace"
    );

    let _ = std::fs::remove_dir_all(&workspace);
}

/// Deleting inside protected metadata must be denied even when the workspace
/// root itself is writable.
#[test]
fn protected_metadata_delete_denied() {
    let workspace = fresh_workspace("metadata_delete");
    let git_dir = workspace.join(".git");
    std::fs::create_dir_all(&git_dir).expect("seed metadata dir");
    let config = git_dir.join("config");
    std::fs::write(&config, "keep").expect("seed metadata file");

    let command = format!("Remove-Item -LiteralPath {} -Force", ps_quote_path(&config));
    let Some(_outcome) = run_powershell(&workspace, &command) else {
        return;
    };

    assert!(
        config.exists(),
        "protected metadata file should survive sandboxed delete attempt"
    );

    let _ = std::fs::remove_dir_all(&workspace);
}

/// Reads from a system directory must still work (the restricted tier does not
/// gate reads).
#[test]
#[ignore = "runtime restricted-token gate; run on a real Windows host (see docs/internal/windows-sandbox-qa.md)"]
fn read_system_still_works() {
    let workspace = fresh_workspace("read_system");

    let Some(outcome) = run_powershell(
        &workspace,
        r"if (Test-Path -LiteralPath 'C:\Windows') { exit 0 } else { exit 1 }",
    ) else {
        return;
    };

    assert!(
        outcome.status.success(),
        "reading C:\\Windows should succeed on restricted-token tier; exit={:?}; stdout={:?}; stderr={:?}",
        outcome.status,
        outcome.stdout,
        outcome.stderr
    );

    let _ = std::fs::remove_dir_all(&workspace);
}
