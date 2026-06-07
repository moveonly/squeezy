//! Windows-only implementation root.

use std::collections::HashMap;
use std::path::Path;

use crate::{Result, TeardownReport, WinSandboxChildHandles, WinSandboxSpec};

mod acl;
mod wfp;
mod cap;
mod deny_read_resolver;
mod desktop;
mod dpapi;
mod elevated_exec;
mod elevated_process;
mod helper_materialization;
mod identity;
mod path_norm;
mod proc_thread_attr;
mod process;
mod restricted;
mod setup;
mod setup_error;
mod ssh_config;
mod token;
mod users;
mod winutil;
mod world_writable;

/// Re-export for the helper binary.
pub use identity::SETUP_VERSION;
/// Re-export for the helper binary.
pub use setup::run_setup_privileged;

pub(crate) fn elevated_setup_is_complete(state_dir: &Path) -> bool {
    identity::elevated_setup_is_complete(state_dir)
}

pub(crate) fn spawn_restricted_token(
    spec: &WinSandboxSpec,
    argv: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
    stdin_open: bool,
) -> Result<WinSandboxChildHandles> {
    restricted::spawn(spec, argv, cwd, env, stdin_open)
}

pub(crate) fn run_elevated_setup(spec: &WinSandboxSpec) -> Result<()> {
    setup::run_elevated_setup(spec)
}

pub(crate) fn run_setup_refresh(spec: &WinSandboxSpec) -> Result<()> {
    setup::run_setup_refresh(spec)
}

pub(crate) fn spawn_elevated(
    spec: &WinSandboxSpec,
    argv: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
    stdin_open: bool,
) -> Result<WinSandboxChildHandles> {
    elevated_exec::spawn(spec, argv, cwd, env, stdin_open)
}

pub(crate) fn teardown_machine_state(state_dir: &Path) -> Result<TeardownReport> {
    setup::teardown_machine_state(state_dir)
}
