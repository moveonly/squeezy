//! Credential and marker persistence for the elevated sandbox tier.
//!
//! Manages two files under `state_dir`:
//! - `setup_marker.json`: lightweight completion marker (version + usernames).
//! - `sandbox_users.json`: DPAPI-encrypted passwords for the two sandbox users.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};

use crate::WinNetwork;

/// Bump when the setup format or user policy changes in an incompatible way.
pub const SETUP_VERSION: u32 = 1;

// ── File-path helpers ─────────────────────────────────────────────────────────

pub(crate) fn marker_path(state_dir: &Path) -> PathBuf {
    state_dir.join("setup_marker.json")
}

pub(crate) fn users_path(state_dir: &Path) -> PathBuf {
    state_dir.join("sandbox_users.json")
}

// ── On-disk types ─────────────────────────────────────────────────────────────

/// Lightweight marker written after a successful elevated setup run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SetupMarker {
    pub(crate) version: u32,
    pub(crate) offline_username: String,
    pub(crate) online_username: String,
}

/// Per-user record: username + DPAPI-encrypted password base64-encoded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct UserRecord {
    pub(crate) username: String,
    /// `base64(DPAPI_ciphertext(password_bytes))`.
    pub(crate) password_dpapi_b64: String,
}

/// Both sandbox-user records stored together.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SandboxUsersFile {
    pub(crate) version: u32,
    pub(crate) offline: UserRecord,
    pub(crate) online: UserRecord,
}

/// Decrypted credentials ready for `CreateProcessWithLogonW`.
#[derive(Debug, Clone)]
pub(crate) struct SandboxCreds {
    #[allow(dead_code)] // consumed by the runner phase (later)
    pub(crate) username: String,
    #[allow(dead_code)] // consumed by the runner phase (later)
    pub(crate) password: String,
}

// ── Completion check ──────────────────────────────────────────────────────────

/// Return `true` iff both the marker and the users file exist, parse, and have
/// `version == SETUP_VERSION`.
pub(crate) fn elevated_setup_is_complete(state_dir: &Path) -> bool {
    let marker_ok = match load_marker(state_dir) {
        Ok(Some(m)) => m.version == SETUP_VERSION,
        _ => false,
    };
    if !marker_ok {
        return false;
    }
    matches!(
        load_users(state_dir),
        Ok(Some(u)) if u.version == SETUP_VERSION
    )
}

// ── Credential loader ─────────────────────────────────────────────────────────

/// Load credentials for `network` from `state_dir`, DPAPI-decrypting the
/// stored password.
///
/// Returns `NotProvisioned` if the users file is absent or version-mismatched.
/// The runner phase consumes this function.
#[allow(dead_code)]
pub(crate) fn sandbox_creds(
    state_dir: &Path,
    network: WinNetwork,
) -> crate::Result<SandboxCreds> {
    let users = load_users(state_dir)?.ok_or_else(|| {
        crate::WinSandboxError::NotProvisioned(
            "sandbox_users.json missing; run elevated setup first".into(),
        )
    })?;
    if users.version != SETUP_VERSION {
        return Err(crate::WinSandboxError::NotProvisioned(format!(
            "sandbox_users.json version {} does not match expected {}",
            users.version, SETUP_VERSION
        )));
    }
    let record = match network {
        WinNetwork::Offline => users.offline,
        _ => users.online,
    };
    let blob = BASE64
        .decode(record.password_dpapi_b64.as_bytes())
        .map_err(|e| {
            crate::WinSandboxError::win32(format!("base64 decode password for '{}': {e}", record.username))
        })?;
    let plaintext = super::dpapi::unprotect(&blob)?;
    let password = String::from_utf8(plaintext).map_err(|e| {
        crate::WinSandboxError::win32(format!("password UTF-8 decode for '{}': {e}", record.username))
    })?;
    Ok(SandboxCreds {
        username: record.username,
        password,
    })
}

// ── Writers (called from setup.rs) ───────────────────────────────────────────

/// Serialise and write `SandboxUsersFile` to `<state_dir>/sandbox_users.json`.
pub(crate) fn write_users_file(
    state_dir: &Path,
    file: &SandboxUsersFile,
) -> crate::Result<()> {
    std::fs::create_dir_all(state_dir)?;
    let json = serde_json::to_vec_pretty(file).map_err(|e| {
        crate::WinSandboxError::win32(format!("serialise sandbox_users.json: {e}"))
    })?;
    std::fs::write(users_path(state_dir), json)?;
    Ok(())
}

/// Serialise and write `SetupMarker` to `<state_dir>/setup_marker.json`.
pub(crate) fn write_marker(
    state_dir: &Path,
    marker: &SetupMarker,
) -> crate::Result<()> {
    std::fs::create_dir_all(state_dir)?;
    let json = serde_json::to_vec_pretty(marker).map_err(|e| {
        crate::WinSandboxError::win32(format!("serialise setup_marker.json: {e}"))
    })?;
    std::fs::write(marker_path(state_dir), json)?;
    Ok(())
}

/// Delete `sandbox_users.json` (best-effort, ignores not-found).
#[allow(dead_code)] // used by teardown (later)
pub(crate) fn delete_users_file(state_dir: &Path) -> crate::Result<()> {
    match std::fs::remove_file(users_path(state_dir)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Delete `setup_marker.json` (best-effort, ignores not-found).
#[allow(dead_code)] // used by teardown (later)
pub(crate) fn delete_marker(state_dir: &Path) -> crate::Result<()> {
    match std::fs::remove_file(marker_path(state_dir)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

// ── Private loaders ───────────────────────────────────────────────────────────

fn load_marker(state_dir: &Path) -> crate::Result<Option<SetupMarker>> {
    let path = marker_path(state_dir);
    let contents = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            tracing::debug!(path = %path.display(), err = %e, "read setup_marker.json failed");
            return Ok(None);
        }
    };
    match serde_json::from_str::<SetupMarker>(&contents) {
        Ok(m) => Ok(Some(m)),
        Err(e) => {
            tracing::debug!(path = %path.display(), err = %e, "parse setup_marker.json failed");
            Ok(None)
        }
    }
}

fn load_users(state_dir: &Path) -> crate::Result<Option<SandboxUsersFile>> {
    let path = users_path(state_dir);
    let contents = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            tracing::debug!(path = %path.display(), err = %e, "read sandbox_users.json failed");
            return Ok(None);
        }
    };
    match serde_json::from_str::<SandboxUsersFile>(&contents) {
        Ok(u) => Ok(Some(u)),
        Err(e) => {
            tracing::debug!(path = %path.display(), err = %e, "parse sandbox_users.json failed");
            Ok(None)
        }
    }
}
