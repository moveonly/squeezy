//! Cross-platform data types for the Windows shell sandbox.
//!
//! These types are deliberately free of any Win32 FFI so they compile on every
//! platform (the crate is a workspace member built on macOS/Linux even though
//! it only does real work on Windows). `squeezy-tools` builds a
//! [`WinSandboxSpec`] from its `ShellSandboxConfig` and hands it to the spawn
//! entry points; the resulting raw OS handles in [`WinSandboxChildHandles`] are
//! adopted by `squeezy-tools`' async capture pipeline.

use std::path::PathBuf;

/// Errors surfaced by the Windows sandbox. Kept small and `std`-friendly so the
/// caller can map it onto `squeezy-tools`' own shell error taxonomy.
#[derive(Debug, thiserror::Error)]
pub enum WinSandboxError {
    /// The current platform is not Windows, or the requested tier is not
    /// available in this build.
    #[error("windows sandbox is not supported on this platform")]
    Unsupported,
    /// A Win32 call failed; carries a human-readable, already-formatted reason
    /// (typically including the `GetLastError` code + message).
    #[error("windows sandbox: {0}")]
    Win32(String),
    /// The elevated tier has not been provisioned (run `squeezy doctor
    /// --sandbox-setup`).
    #[error("windows sandbox elevated tier not provisioned: {0}")]
    NotProvisioned(String),
    /// Wraps an underlying I/O error (pipe creation, file ops, etc.).
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl WinSandboxError {
    /// Convenience constructor for a formatted Win32 failure.
    pub fn win32(msg: impl Into<String>) -> Self {
        Self::Win32(msg.into())
    }
}

pub type Result<T> = std::result::Result<T, WinSandboxError>;

/// A raw OS handle value carried across the crate boundary as a plain integer.
///
/// Storing the handle as `isize` (rather than a `*mut c_void`) keeps the
/// containing structs `Send`/`Sync` so they can move into `spawn_blocking`, and
/// lets the same struct definition compile on non-Windows targets. On Windows
/// the value is a `HANDLE` reinterpreted as `isize`; `0` is the null sentinel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawHandle(pub isize);

impl RawHandle {
    pub const NULL: RawHandle = RawHandle(0);

    pub fn is_null(self) -> bool {
        self.0 == 0
    }
}

/// Which restricted-token family a spawn needs.
///
/// Mirrors Codex's `WindowsSandboxTokenMode`: a read-only sandbox has no
/// writable roots, whereas a workspace-write sandbox grants write through a
/// per-root capability SID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WinTokenMode {
    /// No writable roots: a single read-only capability SID.
    ReadOnlyCapability,
    /// One or more writable roots: per-root write-capable capability SIDs.
    WritableRootsCapability,
}

/// A writable root plus any read-only carve-outs beneath it (e.g. a vendored
/// dependency directory inside an otherwise-writable workspace).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WinWritableRoot {
    pub root: PathBuf,
    pub read_only_subpaths: Vec<PathBuf>,
}

impl WinWritableRoot {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            read_only_subpaths: Vec::new(),
        }
    }
}

/// The network posture for a spawned command.
///
/// `Offline`/`Online` are only *enforced* on the elevated tier (the offline
/// sandbox user has WFP egress-block filters; the online user does not). On the
/// restricted-token tier network is never enforced and the spec carries
/// `Unenforced`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WinNetwork {
    /// Run under the offline identity with WFP egress blocking (elevated tier).
    Offline,
    /// Run under the online identity with no egress block (elevated tier).
    Online,
    /// Network is not enforced by the sandbox (restricted-token tier).
    Unenforced,
}

/// The fully-resolved description of a sandboxed spawn, built by `squeezy-tools`
/// from its `ShellSandboxConfig` + per-command analysis.
#[derive(Debug, Clone)]
pub struct WinSandboxSpec {
    pub token_mode: WinTokenMode,
    /// Writable roots (workspace + configured write roots + temp), each with
    /// optional read-only carve-outs.
    pub writable_roots: Vec<WinWritableRoot>,
    /// Additional read-only roots (authoritative only on the elevated tier;
    /// informational on the restricted tier, where reads are unrestricted).
    pub read_roots: Vec<PathBuf>,
    /// Concrete paths whose *reads* must be denied (sensitive files). Only
    /// enforceable on the elevated tier.
    pub deny_read_paths: Vec<PathBuf>,
    /// Directory/file names under each writable root that must stay write-denied
    /// (agent + VCS metadata, e.g. `.git`, `.squeezy`, `.agents`).
    pub protected_metadata_names: Vec<String>,
    /// Sensitive-path glob patterns (e.g. `.ssh/**`, `.aws/**`, `.env*`) resolved
    /// against `$HOME` / the workspace into concrete deny-read targets. Enforced
    /// only on the elevated tier (the restricted tier cannot gate reads).
    pub sensitive_path_patterns: Vec<String>,
    pub network: WinNetwork,
    /// Squeezy state directory that owns the capability-SID map, sandbox-user
    /// secrets, setup marker, and deny-read ACL state.
    pub state_dir: PathBuf,
}

/// Raw OS handles for a spawned sandboxed child, adopted by `squeezy-tools`'
/// async pipeline. The reader wraps `stdout_read`/`stderr_read` via
/// `tokio::fs::File::from_std`, waits on `process`, and assigns `pid` to the
/// Job Object (restricted tier only).
#[derive(Debug)]
pub struct WinSandboxChildHandles {
    pub pid: u32,
    pub process: RawHandle,
    pub stdout_read: RawHandle,
    pub stderr_read: RawHandle,
    pub stdin_write: Option<RawHandle>,
}

/// Summary of what `teardown_machine_state` removed, for `doctor` reporting.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct TeardownReport {
    pub users_removed: Vec<String>,
    pub wfp_filters_removed: usize,
    pub registry_entries_removed: usize,
    pub state_files_removed: Vec<PathBuf>,
    pub notes: Vec<String>,
}
