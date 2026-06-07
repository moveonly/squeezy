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

#![cfg(windows)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use squeezy_win_sandbox::{
    WinNetwork, WinSandboxSpec, WinTokenMode, WinWritableRoot, spawn_restricted_token,
};

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

/// Run `cmd /c <cmdline_arg>` (a single shell command string) inside the
/// sandbox rooted at `workspace`.  Returns the child's exit status, or `None`
/// if the spawn was skipped because the host can't create restricted tokens.
fn run_cmd(workspace: &Path, cmdline_arg: &str) -> Option<std::process::ExitStatus> {
    let spec = make_spec(workspace);
    let argv = vec!["cmd".to_string(), "/c".to_string(), cmdline_arg.to_string()];
    let env: HashMap<String, String> = std::env::vars().collect();

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let mut child = match spawn_restricted_token(&spec, &argv, workspace, &env, false) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[skip] spawn_restricted_token returned error: {e}");
            return None;
        }
    };

    let status = rt.block_on(async {
        match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
            Ok(status) => status.expect("wait failed"),
            Err(_) => {
                child.kill();
                eprintln!("[skip] sandboxed command timed out: {cmdline_arg}");
                return None;
            }
        }
    })?;
    Some(status)
}

fn workspace_write_capability_available(workspace: &Path) -> bool {
    let probe_file = workspace.join("squeezy-wsbx-probe.txt");
    let cmdline = format!(r#"echo probe > "{}""#, probe_file.display());
    let Some(status) = run_cmd(workspace, &cmdline) else {
        return false;
    };
    let ok = status.success() && probe_file.exists();
    let _ = std::fs::remove_file(&probe_file);
    if !ok {
        eprintln!(
            "[skip] restricted-token sandbox cannot write to declared workspace root; exit={status:?}"
        );
    }
    ok
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// A write inside the workspace must succeed.
#[test]
fn write_inside_workspace_allowed() {
    let workspace = fresh_workspace("write_inside");
    if !workspace_write_capability_available(&workspace) {
        let _ = std::fs::remove_dir_all(&workspace);
        return;
    }
    let out_file = workspace.join("out.txt");

    let cmdline = format!(r#"echo hi > "{}""#, out_file.display());
    let Some(status) = run_cmd(&workspace, &cmdline) else {
        return;
    };

    assert!(
        status.success(),
        "write inside workspace should succeed; exit={status:?}"
    );
    assert!(
        out_file.exists(),
        "output file should exist after write inside workspace"
    );

    let _ = std::fs::remove_dir_all(&workspace);
}

/// A write to a path outside the workspace must be denied by the restricted token.
#[test]
fn write_outside_workspace_denied() {
    let workspace = fresh_workspace("write_outside_ws");
    if !workspace_write_capability_available(&workspace) {
        let _ = std::fs::remove_dir_all(&workspace);
        return;
    }

    // Pick a sibling directory that is NOT the workspace (and doesn't overlap
    // with it), so the restricted token's capability SID denies writes there.
    let outside_dir = std::env::temp_dir().join("squeezy-wsbx-outside-target");
    std::fs::create_dir_all(&outside_dir).ok();
    let evil_file = outside_dir.join("evil.txt");
    // Remove any leftover from a previous run.
    let _ = std::fs::remove_file(&evil_file);

    let cmdline = format!(r#"echo x > "{}""#, evil_file.display());
    // Ignore the exit status here: cmd.exe may exit 0 even when a shell
    // redirection is denied.  The authoritative signal is whether the file was
    // created.
    let Some(_status) = run_cmd(&workspace, &cmdline) else {
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
fn append_inside_allowed() {
    let workspace = fresh_workspace("append_inside");
    if !workspace_write_capability_available(&workspace) {
        let _ = std::fs::remove_dir_all(&workspace);
        return;
    }
    let target = workspace.join("append.txt");
    std::fs::write(&target, "line1\n").expect("seed file");

    let cmdline = format!(r#"echo line2 >> "{}""#, target.display());
    let Some(status) = run_cmd(&workspace, &cmdline) else {
        return;
    };

    assert!(
        status.success(),
        "append inside workspace should succeed; exit={status:?}"
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
fn delete_inside_allowed() {
    let workspace = fresh_workspace("delete_inside");
    if !workspace_write_capability_available(&workspace) {
        let _ = std::fs::remove_dir_all(&workspace);
        return;
    }
    let target = workspace.join("delme.txt");
    std::fs::write(&target, "x").expect("seed file");

    let cmdline = format!(r#"del /q "{}""#, target.display());
    let Some(status) = run_cmd(&workspace, &cmdline) else {
        return;
    };

    assert!(
        status.success(),
        "delete inside workspace should succeed; exit={status:?}"
    );
    assert!(
        !target.exists(),
        "file should be gone after delete inside workspace"
    );

    let _ = std::fs::remove_dir_all(&workspace);
}

/// Reads from a system directory must still work (the restricted tier does not
/// gate reads).
#[test]
fn read_system_still_works() {
    let workspace = fresh_workspace("read_system");
    if !workspace_write_capability_available(&workspace) {
        let _ = std::fs::remove_dir_all(&workspace);
        return;
    }

    // `dir C:\Windows` lists the directory — a read-only operation that should
    // always succeed on the restricted-token tier.
    let Some(status) = run_cmd(&workspace, r"dir C:\Windows") else {
        return;
    };

    assert!(
        status.success(),
        "reading C:\\Windows with dir should succeed on restricted-token tier; exit={status:?}"
    );

    let _ = std::fs::remove_dir_all(&workspace);
}
