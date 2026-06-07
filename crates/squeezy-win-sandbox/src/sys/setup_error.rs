//! Setup error reporting: structured error codes written by the elevated helper
//! and read by the orchestrator to surface a precise failure message.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Categorised failure codes for the elevated sandbox setup pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SetupErrorCode {
    // Orchestrator-side failures.
    /// Cannot determine whether the current process is elevated.
    ElevationCheckFailed,
    /// The setup payload could not be serialised/encoded before launching the helper.
    PayloadSerializeFailed,
    /// `ShellExecuteExW` (or process spawn) failed to launch the helper.
    HelperLaunchFailed,
    /// The user cancelled the UAC prompt.
    ElevationDeclined,
    /// Helper exited non-zero and no structured report was available.
    HelperExitNonzero,
    /// Helper exited non-zero and reading `setup_error.json` failed.
    HelperReportReadFailed,
    // Helper-side failures.
    /// The payload argument could not be decoded or parsed.
    PayloadDecodeFailed,
    /// `create_dir_all` for the state directory failed.
    StateDirCreateFailed,
    /// Creating or updating a sandbox user account failed.
    UserCreateFailed,
    /// DPAPI `CryptProtectData` failed.
    DpapiProtectFailed,
    /// Writing `sandbox_users.json` failed.
    UsersFileWriteFailed,
    /// Writing `setup_marker.json` failed.
    MarkerWriteFailed,
    /// Resolving a SID for a sandbox user failed.
    SidResolveFailed,
    /// Applying an ACL entry failed.
    AclApplyFailed,
    /// An unexpected error without a more specific code.
    UnknownError,
}

/// A structured failure report written to `<state_dir>/setup_error.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SetupErrorReport {
    pub(crate) code: SetupErrorCode,
    pub(crate) message: String,
}

// ── Path helper ───────────────────────────────────────────────────────────────

pub(crate) fn setup_error_path(state_dir: &Path) -> PathBuf {
    state_dir.join("setup_error.json")
}

// ── I/O helpers ───────────────────────────────────────────────────────────────

/// Serialise `report` and write it to `<state_dir>/setup_error.json`.
pub(crate) fn write_setup_error_report(
    state_dir: &Path,
    report: &SetupErrorReport,
) -> std::result::Result<(), std::io::Error> {
    std::fs::create_dir_all(state_dir)?;
    let path = setup_error_path(state_dir);
    let json = serde_json::to_vec_pretty(report).map_err(std::io::Error::other)?;
    std::fs::write(path, json)
}

/// Read and deserialise `<state_dir>/setup_error.json`.  Returns `Ok(None)` if
/// the file does not exist.
pub(crate) fn read_setup_error_report(
    state_dir: &Path,
) -> std::result::Result<Option<SetupErrorReport>, std::io::Error> {
    let path = setup_error_path(state_dir);
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let report = serde_json::from_slice::<SetupErrorReport>(&bytes)
        .map_err(|e| std::io::Error::new(ErrorKind::InvalidData, e))?;
    Ok(Some(report))
}

/// Delete `<state_dir>/setup_error.json`, ignoring "not found".
pub(crate) fn clear_setup_error_report(
    state_dir: &Path,
) -> std::result::Result<(), std::io::Error> {
    match std::fs::remove_file(setup_error_path(state_dir)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}
