//! Locate the `squeezy-sandbox-setup` helper binary.
//!
//! Resolution order:
//! 1. Same directory as the current executable (`squeezy-sandbox-setup.exe`).
//! 2. `squeezy-sandbox-setup.exe` in the same directory (explicit extension
//!    variant, identical to 1 on Windows — kept for clarity).
//!
//! Returns a clear error if the binary cannot be found.

use std::path::PathBuf;

const SETUP_EXE: &str = "squeezy-sandbox-setup.exe";

/// Locate the elevated-setup helper binary.
pub(crate) fn setup_helper_exe() -> crate::Result<PathBuf> {
    let current_exe = std::env::current_exe().map_err(|e| {
        crate::WinSandboxError::win32(format!("resolve current executable: {e}"))
    })?;

    if let Some(dir) = current_exe.parent() {
        let candidate = dir.join(SETUP_EXE);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    Err(crate::WinSandboxError::win32(format!(
        "helper binary '{}' not found next to current executable ({})",
        SETUP_EXE,
        current_exe.display()
    )))
}
