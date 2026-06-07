//! Adapter from Squeezy's `ShellSandboxConfig` + per-command `ShellSandboxPlan`
//! to the self-contained `squeezy_win_sandbox::WinSandboxSpec`. Lives in
//! `squeezy-tools` (which already depends on `squeezy-core`) so the
//! `squeezy-win-sandbox` crate stays free of any Squeezy-config dependency.
//!
//! Windows-only: the spec types come from the windows-target-only
//! `squeezy-win-sandbox` dependency.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use squeezy_core::ShellSandboxConfig;
use squeezy_win_sandbox::{WinNetwork, WinSandboxSpec, WinTokenMode, WinWritableRoot};

use crate::shell_sandbox::ShellSandboxPlan;

/// Where the capability-SID map, sandbox-user secrets, setup marker, and
/// deny-read ACL state live. Per-user global (see
/// [`squeezy_core::default_win_sandbox_state_dir`]) so the elevated tier's
/// machine-level users + WFP filters are provisioned/torn down once, and so the
/// state dir sits outside any workspace (the sandbox naturally has no write
/// capability there).
pub(crate) fn win_state_dir() -> PathBuf {
    squeezy_core::default_win_sandbox_state_dir()
}

/// Build the resolved Windows sandbox spec for a command.
///
/// The shell always treats the workspace `root` as writable (a command's `cwd`
/// is a subdir of it and inherits write via the root's inheritable allow ACE),
/// so the token mode is `WritableRootsCapability`. Network enforcement only
/// exists on the elevated tier (the restricted-token tier cannot gate sockets),
/// so a non-elevated backend always reports `Unenforced`.
/// The writable roots the sandbox grants: the workspace `root`, configured
/// write roots, and the per-user temp dirs. Deduplicated, order-preserving.
/// Shared by [`build_win_spec`] and [`build_setup_spec`].
fn writable_roots_for(config: &ShellSandboxConfig, root: &Path) -> Vec<WinWritableRoot> {
    let mut writable_roots: Vec<WinWritableRoot> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let candidates = std::iter::once(root.to_path_buf())
        .chain(config.write_roots.iter().cloned())
        .chain(
            ["TEMP", "TMP"]
                .iter()
                .filter_map(|var| std::env::var_os(var).map(PathBuf::from)),
        );
    for path in candidates {
        if seen.insert(path.clone()) {
            writable_roots.push(WinWritableRoot::new(path));
        }
    }
    writable_roots
}

pub(crate) fn build_win_spec(
    config: &ShellSandboxConfig,
    root: &Path,
    plan: &ShellSandboxPlan,
) -> WinSandboxSpec {
    // Network is only enforced by the elevated tier (offline = WFP block,
    // online = no block). The restricted-token tier cannot enforce egress.
    let network = if plan.backend == "windows-elevated" {
        match plan.network {
            "allowed_approved" | "allowed" => WinNetwork::Online,
            _ => WinNetwork::Offline,
        }
    } else {
        WinNetwork::Unenforced
    };

    WinSandboxSpec {
        token_mode: WinTokenMode::WritableRootsCapability,
        writable_roots: writable_roots_for(config, root),
        read_roots: config.read_roots.clone(),
        // Sensitive-path read-deny is resolved + enforced on the elevated tier
        // only; empty here keeps the restricted tier honest.
        deny_read_paths: Vec::new(),
        protected_metadata_names: config.protected_metadata_names.clone(),
        sensitive_path_patterns: config.sensitive_path_patterns.clone(),
        network,
        state_dir: win_state_dir(),
    }
}

/// Build a spec for the one-time elevated provisioning (`doctor
/// --sandbox-setup`). Network posture is irrelevant here (setup provisions both
/// the offline and online identities + WFP filters regardless), so it defaults
/// to `Offline`.
pub(crate) fn build_setup_spec(config: &ShellSandboxConfig, root: &Path) -> WinSandboxSpec {
    WinSandboxSpec {
        token_mode: WinTokenMode::WritableRootsCapability,
        writable_roots: writable_roots_for(config, root),
        read_roots: config.read_roots.clone(),
        deny_read_paths: Vec::new(),
        protected_metadata_names: config.protected_metadata_names.clone(),
        sensitive_path_patterns: config.sensitive_path_patterns.clone(),
        network: WinNetwork::Offline,
        state_dir: win_state_dir(),
    }
}
