//! Windows OS-level shell sandbox for Squeezy.
//!
//! Two tiers, both designed against Squeezy's `ShellSandboxPlan` contract (this
//! crate carries its own [`WinSandboxSpec`] so it has no dependency on
//! `squeezy-core`; `squeezy-tools` builds the spec):
//!
//! * **Restricted-token tier** (default, no admin): each command runs under a
//!   `CreateRestrictedToken` token whose write access is gated by per-workspace
//!   capability SIDs + on-disk ACLs. Enforces filesystem *writes* and write
//!   carve-outs. `WRITE_RESTRICTED` tokens do not gate reads, so read-deny and
//!   network blocking are not available on this tier.
//! * **Elevated tier** (opt-in, one-time UAC): provisions hidden local sandbox
//!   users, installs persistent WFP egress filters, and runs commands as the
//!   sandbox user via `CreateProcessWithLogonW` + named-pipe IPC. Adds full
//!   read-deny and network egress control.
//!
//! Every Win32 module is `#[cfg(windows)]`; on other platforms the entry points
//! return [`WinSandboxError::Unsupported`] so the crate still builds as a
//! workspace member.

mod types;
pub use types::*;

#[cfg(windows)]
use std::collections::HashMap;
use std::path::Path;

#[cfg(windows)]
mod child;
#[cfg(windows)]
pub use child::WinSandboxChild;

#[cfg(windows)]
mod sys;

// Re-export items needed by the squeezy-sandbox-setup helper binary.
#[cfg(windows)]
pub use sys::SETUP_VERSION;
#[cfg(windows)]
pub use sys::run_setup_privileged;

/// Whether the restricted-token tier can be used. Always true on Windows (it
/// needs no privileges); false elsewhere.
pub fn restricted_token_available() -> bool {
    cfg!(windows)
}

/// Whether the elevated tier has been provisioned for `state_dir` (sandbox
/// users + setup marker present and version-matched).
pub fn elevated_setup_is_complete(state_dir: &Path) -> bool {
    #[cfg(windows)]
    {
        sys::elevated_setup_is_complete(state_dir)
    }
    #[cfg(not(windows))]
    {
        let _ = state_dir;
        false
    }
}

/// Spawn `argv` under a restricted token built from `spec`, returning an async
/// child the caller can capture, wait on, and kill. Windows-only — callers
/// reference this only under `#[cfg(windows)]`.
#[cfg(windows)]
pub fn spawn_restricted_token(
    spec: &WinSandboxSpec,
    argv: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
    stdin_open: bool,
) -> Result<WinSandboxChild> {
    let handles = sys::spawn_restricted_token(spec, argv, cwd, env, stdin_open)?;
    Ok(WinSandboxChild::from_handles(handles))
}

/// Run the one-time elevated setup (UAC prompt): provision sandbox users, ACLs,
/// and WFP filters for `spec.state_dir`.
pub fn run_elevated_setup(spec: &WinSandboxSpec) -> Result<()> {
    #[cfg(windows)]
    {
        sys::run_elevated_setup(spec)
    }
    #[cfg(not(windows))]
    {
        let _ = spec;
        Err(WinSandboxError::Unsupported)
    }
}

/// Re-apply per-run ACLs for the elevated tier (non-elevated; adapts to changed
/// read/write roots).
pub fn run_setup_refresh(spec: &WinSandboxSpec) -> Result<()> {
    #[cfg(windows)]
    {
        sys::run_setup_refresh(spec)
    }
    #[cfg(not(windows))]
    {
        let _ = spec;
        Err(WinSandboxError::Unsupported)
    }
}

/// Spawn `argv` as the elevated-tier sandbox user via the runner IPC, returning
/// an async child. Windows-only — callers reference this only under
/// `#[cfg(windows)]`.
#[cfg(windows)]
pub fn spawn_elevated(
    spec: &WinSandboxSpec,
    argv: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
    stdin_open: bool,
) -> Result<WinSandboxChild> {
    let handles = sys::spawn_elevated(spec, argv, cwd, env, stdin_open)?;
    Ok(WinSandboxChild::from_handles(handles))
}

/// Remove all persistent machine state created by the elevated tier (sandbox
/// users, WFP filters, registry hide entries, secrets/marker, stale ACLs).
pub fn teardown_machine_state(state_dir: &Path) -> Result<TeardownReport> {
    #[cfg(windows)]
    {
        sys::teardown_machine_state(state_dir)
    }
    #[cfg(not(windows))]
    {
        let _ = state_dir;
        Err(WinSandboxError::Unsupported)
    }
}
