//! Orchestrate a `spawn_elevated` call: check provisioning, refresh ACLs,
//! resolve credentials, grant desktop access, and launch via
//! `CreateProcessWithLogonW`.

use std::collections::HashMap;
use std::path::Path;

use crate::{WinSandboxChildHandles, WinSandboxError, WinSandboxSpec};

use super::{desktop, elevated_process, identity, setup, users, winutil};

/// Spawn `argv` as the elevated-tier sandbox user and return the raw handles.
///
/// Steps:
/// 1. Verify that the elevated tier has been provisioned; return
///    `NotProvisioned` if not.
/// 2. Re-apply per-run ACLs for the current roots (non-elevated, fast).
/// 3. Decrypt sandbox credentials from the DPAPI-protected store.
/// 4. Best-effort grant of window-station / desktop access to the sandbox user.
/// 5. Build the command-line and environment block, then spawn via
///    `CreateProcessWithLogonW`.
pub(crate) fn spawn(
    spec: &WinSandboxSpec,
    argv: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
    stdin_open: bool,
) -> crate::Result<WinSandboxChildHandles> {
    // ── 1. Provisioning check ────────────────────────────────────────────────
    if !identity::elevated_setup_is_complete(&spec.state_dir) {
        return Err(WinSandboxError::NotProvisioned(
            "elevated sandbox not provisioned; run `squeezy doctor --sandbox-setup`".into(),
        ));
    }

    // ── 2. Refresh ACLs for current roots ───────────────────────────────────
    tracing::debug!("elevated_exec: refreshing sandbox ACLs");
    setup::run_setup_refresh(spec)?;

    // ── 3. Decrypt credentials ───────────────────────────────────────────────
    let creds = identity::sandbox_creds(&spec.state_dir, spec.network)?;
    tracing::debug!(username = %creds.username, "elevated_exec: credentials loaded");

    // ── 4. Grant window-station / desktop access (best-effort) ──────────────
    match users::account_sid_string(&creds.username) {
        Ok(sid_str) => {
            tracing::debug!(
                username = %creds.username,
                sid = %sid_str,
                "elevated_exec: granting desktop access"
            );
            let _ = desktop::grant_user_winsta_desktop(&sid_str);
        }
        Err(e) => {
            tracing::warn!(
                username = %creds.username,
                err = %e,
                "elevated_exec: could not resolve user SID for desktop grant; continuing"
            );
        }
    }

    // ── 5. Build command line + env block, then spawn ────────────────────────
    let mut cl = winutil::build_command_line(argv);
    let env_block = winutil::make_env_block(env);

    tracing::debug!(
        username = %creds.username,
        cwd = %cwd.display(),
        "elevated_exec: spawning via CreateProcessWithLogonW"
    );

    elevated_process::spawn_with_logon(
        &creds.username,
        &creds.password,
        &mut cl,
        cwd,
        &env_block,
        stdin_open,
    )
}
