//! Elevated-tier one-time setup orchestration.
//!
//! Entry points wired from `sys::mod`:
//! - `run_elevated_setup`  — UAC prompt if needed, then elevated provisioning.
//! - `run_setup_privileged` — pure elevated work (users, DPAPI, files, ACLs).
//! - `run_setup_refresh`   — non-elevated ACL refresh.
//! - `teardown_machine_state` — remove all persistent machine state.

use std::ffi::c_void;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError};
use windows_sys::Win32::Security::{
    AllocateAndInitializeSid, CheckTokenMembership, FreeSid, SECURITY_NT_AUTHORITY,
};
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
use windows_sys::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};

use crate::{TeardownReport, WinSandboxError, WinSandboxSpec};

use super::acl::{
    add_allow_ace_recursive, add_allow_read_ace_recursive, add_deny_read_ace_recursive,
    add_deny_write_ace_recursive,
};
use super::deny_read_resolver::resolve_deny_read_paths;
use super::helper_materialization::setup_helper_exe;
use super::identity::{
    SETUP_VERSION, SandboxUsersFile, SetupMarker, UserRecord, write_marker, write_users_file,
};
use super::setup_error::{
    SetupErrorCode, SetupErrorReport, clear_setup_error_report, read_setup_error_report,
    write_setup_error_report,
};
use super::ssh_config::ssh_config_dependency_paths;
use super::users::{
    account_sid_string, create_or_update_user, delete_user, generate_password,
    hide_user_from_login, unhide_user_from_login,
};
use super::winutil::to_wide;

// ── Constants ─────────────────────────────────────────────────────────────────

pub(crate) const OFFLINE_USERNAME: &str = "SqueezySandboxOffline";
pub(crate) const ONLINE_USERNAME: &str = "SqueezySandboxOnline";

const SECURITY_BUILTIN_DOMAIN_RID: u32 = 0x0000_0020;
const DOMAIN_ALIAS_RID_ADMINS: u32 = 0x0000_0220;
const ERROR_CANCELLED: u32 = 1223;

// ── Elevation check ───────────────────────────────────────────────────────────

/// Returns `true` when the current process token is a member of the built-in
/// Administrators group (i.e. running elevated or as a built-in admin).
fn is_elevated() -> crate::Result<bool> {
    unsafe {
        let mut admins_sid: *mut c_void = std::ptr::null_mut();
        let ok = AllocateAndInitializeSid(
            &SECURITY_NT_AUTHORITY,
            2,
            SECURITY_BUILTIN_DOMAIN_RID,
            DOMAIN_ALIAS_RID_ADMINS,
            0,
            0,
            0,
            0,
            0,
            0,
            &mut admins_sid,
        );
        if ok == 0 {
            return Err(WinSandboxError::win32(format!(
                "AllocateAndInitializeSid (Administrators): {}",
                GetLastError()
            )));
        }
        let mut is_member: i32 = 0;
        // Pass null handle so CheckTokenMembership uses the thread/process token.
        let check =
            CheckTokenMembership(std::ptr::null_mut(), admins_sid, &mut is_member as *mut _);
        FreeSid(admins_sid);
        if check == 0 {
            return Err(WinSandboxError::win32(format!(
                "CheckTokenMembership: {}",
                GetLastError()
            )));
        }
        Ok(is_member != 0)
    }
}

// ── Elevation payload ─────────────────────────────────────────────────────────

/// Minimal data we pass to the elevated helper via base64-JSON.
#[derive(Serialize, Deserialize)]
struct ElevationPayload {
    version: u32,
    state_dir: PathBuf,
    writable_roots: Vec<SerWritableRoot>,
    read_roots: Vec<PathBuf>,
    deny_read_paths: Vec<PathBuf>,
    protected_metadata_names: Vec<String>,
    #[serde(default)]
    sensitive_path_patterns: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct SerWritableRoot {
    root: PathBuf,
    read_only_subpaths: Vec<PathBuf>,
}

impl From<&crate::WinWritableRoot> for SerWritableRoot {
    fn from(w: &crate::WinWritableRoot) -> Self {
        Self {
            root: w.root.clone(),
            read_only_subpaths: w.read_only_subpaths.clone(),
        }
    }
}

fn payload_from_spec(spec: &WinSandboxSpec) -> ElevationPayload {
    ElevationPayload {
        version: SETUP_VERSION,
        state_dir: spec.state_dir.clone(),
        writable_roots: spec
            .writable_roots
            .iter()
            .map(SerWritableRoot::from)
            .collect(),
        read_roots: spec.read_roots.clone(),
        deny_read_paths: spec.deny_read_paths.clone(),
        protected_metadata_names: spec.protected_metadata_names.clone(),
        sensitive_path_patterns: spec.sensitive_path_patterns.clone(),
    }
}

// ── UAC re-launch ─────────────────────────────────────────────────────────────

fn launch_elevated_helper(payload: &ElevationPayload, state_dir: &Path) -> crate::Result<()> {
    let helper = setup_helper_exe().map_err(|e| {
        let _ = write_setup_error_report(
            state_dir,
            &SetupErrorReport {
                code: SetupErrorCode::HelperLaunchFailed,
                message: format!("locate helper: {e}"),
            },
        );
        e
    })?;

    let json = serde_json::to_string(payload)
        .map_err(|e| WinSandboxError::win32(format!("serialise elevation payload: {e}")))?;
    let b64 = BASE64.encode(json.as_bytes());

    let exe_w: Vec<u16> = to_wide(&helper);
    let params_w: Vec<u16> = to_wide(&b64);
    let verb_w: Vec<u16> = to_wide("runas");

    let _ = clear_setup_error_report(state_dir);

    let mut sei: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    sei.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    sei.fMask = SEE_MASK_NOCLOSEPROCESS;
    sei.lpVerb = verb_w.as_ptr();
    sei.lpFile = exe_w.as_ptr();
    sei.lpParameters = params_w.as_ptr();
    sei.nShow = 0; // SW_HIDE

    let ok = unsafe { ShellExecuteExW(&mut sei) };
    // sei.hProcess is `*mut c_void`; a failed launch yields null.
    let launched_ok = ok != 0 && !sei.hProcess.is_null();
    if !launched_ok {
        let last_err = unsafe { GetLastError() };
        let code = if last_err == ERROR_CANCELLED {
            SetupErrorCode::ElevationDeclined
        } else {
            SetupErrorCode::HelperLaunchFailed
        };
        let msg = format!(
            "ShellExecuteExW failed (code={last_err}); {}",
            if last_err == ERROR_CANCELLED {
                "user cancelled UAC prompt"
            } else {
                "could not launch helper"
            }
        );
        let _ = write_setup_error_report(
            state_dir,
            &SetupErrorReport {
                code,
                message: msg.clone(),
            },
        );
        return Err(WinSandboxError::win32(msg));
    }

    // Wait for the helper and collect its exit code.
    let exit_code = unsafe {
        WaitForSingleObject(sei.hProcess, INFINITE);
        let mut code: u32 = 1;
        GetExitCodeProcess(sei.hProcess, &mut code);
        CloseHandle(sei.hProcess);
        code
    };

    if exit_code != 0 {
        return Err(read_helper_error(state_dir, exit_code));
    }
    let _ = clear_setup_error_report(state_dir);
    Ok(())
}

fn read_helper_error(state_dir: &Path, exit_code: u32) -> WinSandboxError {
    match read_setup_error_report(state_dir) {
        Ok(Some(r)) => WinSandboxError::win32(format!(
            "elevated setup helper failed ({}): {}",
            r.code.as_str(),
            r.message
        )),
        _ => WinSandboxError::win32(format!(
            "elevated setup helper exited with code {exit_code}"
        )),
    }
}

// ── Public entry points ───────────────────────────────────────────────────────

/// Run the one-time elevated setup.
///
/// If the process is already elevated, calls `run_setup_privileged` directly.
/// Otherwise serialises a payload and re-launches via `ShellExecuteExW` with
/// the `"runas"` verb (UAC prompt).
pub(crate) fn run_elevated_setup(spec: &WinSandboxSpec) -> crate::Result<()> {
    std::fs::create_dir_all(&spec.state_dir)?;
    let elevated =
        is_elevated().map_err(|e| WinSandboxError::win32(format!("elevation check: {e}")))?;
    if elevated {
        return run_setup_privileged(spec);
    }
    let payload = payload_from_spec(spec);
    launch_elevated_helper(&payload, &spec.state_dir)
}

/// Run elevated provisioning in-process (already admin, or called from the
/// helper binary).  Generates passwords, creates users, writes secrets, applies
/// ACLs.
pub fn run_setup_privileged(spec: &WinSandboxSpec) -> crate::Result<()> {
    std::fs::create_dir_all(&spec.state_dir)?;

    let result = provision_privileged(spec);
    match &result {
        Err(e) => {
            let _ = write_setup_error_report(
                &spec.state_dir,
                &SetupErrorReport {
                    code: error_to_code(e),
                    message: e.to_string(),
                },
            );
        }
        Ok(()) => {
            let _ = clear_setup_error_report(&spec.state_dir);
        }
    }
    result
}

fn error_to_code(e: &WinSandboxError) -> SetupErrorCode {
    match e {
        WinSandboxError::NotProvisioned(_) => SetupErrorCode::SidResolveFailed,
        WinSandboxError::Io(_) => SetupErrorCode::MarkerWriteFailed,
        _ => SetupErrorCode::UnknownError,
    }
}

fn provision_privileged(spec: &WinSandboxSpec) -> crate::Result<()> {
    // Generate passwords.
    let offline_pwd = generate_password();
    let online_pwd = generate_password();

    // Create / update users.
    create_or_update_user(OFFLINE_USERNAME, &offline_pwd)
        .map_err(|e| WinSandboxError::win32(format!("create offline user: {e}")))?;
    create_or_update_user(ONLINE_USERNAME, &online_pwd)
        .map_err(|e| WinSandboxError::win32(format!("create online user: {e}")))?;

    // Hide from login screen.
    hide_user_from_login(OFFLINE_USERNAME)?;
    hide_user_from_login(ONLINE_USERNAME)?;

    // DPAPI-encrypt passwords.
    let offline_blob = super::dpapi::protect(offline_pwd.as_bytes())
        .map_err(|e| WinSandboxError::win32(format!("DPAPI protect offline: {e}")))?;
    let online_blob = super::dpapi::protect(online_pwd.as_bytes())
        .map_err(|e| WinSandboxError::win32(format!("DPAPI protect online: {e}")))?;

    // Write users file.
    let users_file = SandboxUsersFile {
        version: SETUP_VERSION,
        offline: UserRecord {
            username: OFFLINE_USERNAME.to_string(),
            password_dpapi_b64: BASE64.encode(&offline_blob),
        },
        online: UserRecord {
            username: ONLINE_USERNAME.to_string(),
            password_dpapi_b64: BASE64.encode(&online_blob),
        },
    };
    write_users_file(&spec.state_dir, &users_file)
        .map_err(|e| WinSandboxError::win32(format!("write sandbox_users.json: {e}")))?;

    // Write marker.
    let marker = SetupMarker {
        version: SETUP_VERSION,
        offline_username: OFFLINE_USERNAME.to_string(),
        online_username: ONLINE_USERNAME.to_string(),
    };
    write_marker(&spec.state_dir, &marker)
        .map_err(|e| WinSandboxError::win32(format!("write setup_marker.json: {e}")))?;

    // Resolve SIDs and apply ACLs.
    let offline_sid = account_sid_string(OFFLINE_USERNAME)
        .map_err(|e| WinSandboxError::win32(format!("SID for offline user: {e}")))?;
    let online_sid = account_sid_string(ONLINE_USERNAME)
        .map_err(|e| WinSandboxError::win32(format!("SID for online user: {e}")))?;

    apply_root_acls(spec, &offline_sid, &online_sid)?;

    // Install WFP egress-block filters for the offline sandbox user.
    // Network blocking is part of the elevated-tier guarantee; fail hard.
    super::wfp::install_block_filters(&offline_sid)
        .map_err(|e| WinSandboxError::win32(format!("WFP install_block_filters: {e}")))?;

    Ok(())
}

/// Non-elevated re-apply of ACLs (adapts to changed roots).
///
/// Looks up existing user SIDs and re-runs `apply_root_acls`.  Returns
/// `NotProvisioned` if the users do not exist.
pub(crate) fn run_setup_refresh(spec: &WinSandboxSpec) -> crate::Result<()> {
    let offline_sid = account_sid_string(OFFLINE_USERNAME).map_err(|_| {
        WinSandboxError::NotProvisioned(
            "SqueezySandboxOffline does not exist; run elevated setup first".into(),
        )
    })?;
    let online_sid = account_sid_string(ONLINE_USERNAME).map_err(|_| {
        WinSandboxError::NotProvisioned(
            "SqueezySandboxOnline does not exist; run elevated setup first".into(),
        )
    })?;
    apply_root_acls(spec, &offline_sid, &online_sid)
}

/// Remove all persistent machine state created by the elevated tier.
pub(crate) fn teardown_machine_state(state_dir: &Path) -> crate::Result<TeardownReport> {
    let mut report = TeardownReport::default();

    for username in &[OFFLINE_USERNAME, ONLINE_USERNAME] {
        match delete_user(username) {
            Ok(true) => {
                report.users_removed.push(username.to_string());
                tracing::info!(username, "sandbox user deleted");
            }
            Ok(false) => {
                tracing::debug!(username, "sandbox user not found, skipping");
            }
            Err(e) => {
                tracing::warn!(username, err = %e, "failed to delete sandbox user");
                report.notes.push(format!("delete {username}: {e}"));
            }
        }
        unhide_user_from_login(username);
        report.registry_entries_removed += 1;
    }

    // Remove state files.
    let state_files = [
        state_dir.join("sandbox_users.json"),
        state_dir.join("setup_marker.json"),
        state_dir.join("setup_error.json"),
    ];
    for path in &state_files {
        match std::fs::remove_file(path) {
            Ok(()) => {
                report.state_files_removed.push(path.clone());
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                report.notes.push(format!("remove {}: {e}", path.display()));
            }
        }
    }

    // Remove WFP egress-block filters (best-effort; continue on error).
    match super::wfp::remove_filters() {
        Ok(n) => {
            report.wfp_filters_removed = n;
        }
        Err(e) => {
            tracing::warn!(err = %e, "failed to remove WFP filters during teardown");
            report.notes.push(format!("WFP remove_filters: {e}"));
        }
    }

    Ok(report)
}

// ── ACL application ───────────────────────────────────────────────────────────

/// Apply all ACL grants and denies for both sandbox users.
fn apply_root_acls(
    spec: &WinSandboxSpec,
    offline_sid: &str,
    online_sid: &str,
) -> crate::Result<()> {
    let home_str = std::env::var("USERPROFILE").ok();
    let home_path: Option<PathBuf> = home_str.as_deref().map(PathBuf::from);
    let workspace = spec
        .writable_roots
        .first()
        .map(|r| r.root.clone())
        .unwrap_or_else(|| spec.state_dir.clone());

    for sid in &[offline_sid, online_sid] {
        // Grant read on read_roots.
        for root in &spec.read_roots {
            if root.exists()
                && let Err(e) = add_allow_read_ace_recursive(root, sid)
            {
                tracing::warn!(
                    path = %root.display(),
                    sid,
                    err = %e,
                    "add_allow_read_ace failed; skipping"
                );
            }
        }

        // Grant read+write on writable roots; deny-write on subpaths.
        for wr in &spec.writable_roots {
            if wr.root.exists()
                && let Err(e) = add_allow_ace_recursive(&wr.root, sid)
            {
                tracing::warn!(
                    path = %wr.root.display(),
                    sid,
                    err = %e,
                    "add_allow_ace (writable root) failed; skipping"
                );
            }

            // Deny-write on read-only subpaths.
            for ro_sub in &wr.read_only_subpaths {
                if ro_sub.exists()
                    && let Err(e) = add_deny_write_ace_recursive(ro_sub, sid)
                {
                    tracing::warn!(
                        path = %ro_sub.display(),
                        sid,
                        err = %e,
                        "add_deny_write_ace (read_only_subpath) failed; skipping"
                    );
                }
            }

            // Deny-write on protected metadata names under each writable root.
            for name in &spec.protected_metadata_names {
                let meta_path = wr.root.join(name);
                if meta_path.exists()
                    && let Err(e) = add_deny_write_ace_recursive(&meta_path, sid)
                {
                    tracing::warn!(
                        path = %meta_path.display(),
                        sid,
                        err = %e,
                        "add_deny_write_ace (metadata) failed; skipping"
                    );
                }
            }
        }

        // Deny-read on sensitive paths: resolve the configured glob patterns
        // (`.ssh/**`, `.aws/**`, …) against $HOME / the workspace, unioned with
        // any explicit deny-read paths.
        let mut deny_paths = resolve_deny_read_paths(
            &spec.sensitive_path_patterns,
            &spec.deny_read_paths,
            home_path.as_deref(),
            &workspace,
        );
        // Also deny-read on ssh config dependency paths.
        let ssh_paths = ssh_config_dependency_paths(home_path.as_deref());
        for p in ssh_paths {
            if !deny_paths.contains(&p) {
                deny_paths.push(p);
            }
        }

        for path in &deny_paths {
            if path.exists()
                && let Err(e) = add_deny_read_ace_recursive(path, sid)
            {
                tracing::warn!(
                    path = %path.display(),
                    sid,
                    err = %e,
                    "add_deny_read_ace failed; skipping"
                );
            }
        }
    }

    Ok(())
}

// ── SetupErrorCode::as_str ────────────────────────────────────────────────────

impl SetupErrorCode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ElevationCheckFailed => "elevation_check_failed",
            Self::PayloadSerializeFailed => "payload_serialize_failed",
            Self::HelperLaunchFailed => "helper_launch_failed",
            Self::ElevationDeclined => "elevation_declined",
            Self::HelperExitNonzero => "helper_exit_nonzero",
            Self::HelperReportReadFailed => "helper_report_read_failed",
            Self::PayloadDecodeFailed => "payload_decode_failed",
            Self::StateDirCreateFailed => "state_dir_create_failed",
            Self::UserCreateFailed => "user_create_failed",
            Self::DpapiProtectFailed => "dpapi_protect_failed",
            Self::UsersFileWriteFailed => "users_file_write_failed",
            Self::MarkerWriteFailed => "marker_write_failed",
            Self::SidResolveFailed => "sid_resolve_failed",
            Self::AclApplyFailed => "acl_apply_failed",
            Self::UnknownError => "unknown_error",
        }
    }
}
