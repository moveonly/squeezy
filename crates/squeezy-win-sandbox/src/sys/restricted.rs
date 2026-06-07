//! High-level orchestrator for the restricted-token tier spawn.

use std::collections::HashMap;
use std::path::Path;

use super::{acl, cap, path_norm, process, token, winutil, world_writable};
use crate::{WinSandboxChildHandles, WinSandboxSpec, WinTokenMode};

/// Build the capability-SID set, create the restricted token, apply ACLs,
/// and spawn the child process.
pub(crate) fn spawn(
    spec: &WinSandboxSpec,
    argv: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
    stdin_open: bool,
) -> crate::Result<WinSandboxChildHandles> {
    let state_dir = &spec.state_dir;

    tracing::debug!("restricted spawn: building capability SID set");

    // Always include the read-only capability SID.
    let mut all_cap_sids: Vec<String> = Vec::new();
    let ro_sid = cap::readonly_cap_sid(state_dir)?;
    all_cap_sids.push(ro_sid);

    // For writable-roots mode, add per-root write capability SIDs and apply ACLs.
    if spec.token_mode == WinTokenMode::WritableRootsCapability {
        tracing::debug!(
            "restricted spawn: applying writable-root ACLs for {} root(s)",
            spec.writable_roots.len()
        );

        let state_key = path_norm::canonical_key(state_dir);

        for wr in &spec.writable_roots {
            let root = &wr.root;
            let write_sid = cap::writable_root_cap_sid(state_dir, root)?;

            tracing::debug!(
                "restricted spawn: add_allow_ace on '{}' for SID {}",
                root.display(),
                &write_sid
            );
            acl::add_allow_ace(root, &write_sid)?;

            // Deny write on read-only carve-outs beneath this root.
            for ro_sub in &wr.read_only_subpaths {
                tracing::debug!(
                    "restricted spawn: add_deny_write_ace on '{}' (read-only subpath)",
                    ro_sub.display()
                );
                acl::add_deny_write_ace(ro_sub, &write_sid)?;
            }

            // Deny write on protected metadata names joined under this root.
            for name in &spec.protected_metadata_names {
                let meta_path = root.join(name);
                if meta_path.exists() {
                    tracing::debug!(
                        "restricted spawn: add_deny_write_ace on '{}' (protected metadata)",
                        meta_path.display()
                    );
                    acl::add_deny_write_ace(&meta_path, &write_sid)?;
                }
            }

            // If state_dir falls inside this writable root, deny write to it.
            let root_key = path_norm::canonical_key(root);
            if state_key.starts_with(&root_key) {
                tracing::debug!(
                    "restricted spawn: add_deny_write_ace on state_dir '{}' (inside writable root)",
                    state_dir.display()
                );
                acl::add_deny_write_ace(state_dir, &write_sid)?;
            }

            all_cap_sids.push(write_sid);
        }
    }

    // World-writable escape-vector audit: deny the first writable-root cap SID on
    // any world-writable directories found outside the writable roots.  This
    // prevents the sandboxed process (which carries World as a restricting SID)
    // from writing to pre-existing world-writable directories.  Best-effort —
    // failures are logged but never propagate.
    if spec.token_mode == WinTokenMode::WritableRootsCapability
        && !spec.writable_roots.is_empty()
    {
        // all_cap_sids[0] = readonly cap SID; [1] = first writable-root cap SID.
        // The token carries every cap SID as a restricting SID, so denying any
        // one of them blocks the sandbox (all restricting SIDs must pass).
        let deny_sid = &all_cap_sids[1];
        tracing::debug!(
            "restricted spawn: running world-writable audit (deny_cap_sid={})",
            deny_sid
        );
        world_writable::apply_world_writable_denies(spec, cwd, env, deny_sid);
    }

    tracing::debug!(
        "restricted spawn: creating sandbox token with {} cap SID(s)",
        all_cap_sids.len()
    );
    let tok = token::create_sandbox_token(&all_cap_sids)?;

    tracing::debug!("restricted spawn: building command line and env block");
    let mut command_line = winutil::build_command_line(argv);
    let env_block = winutil::make_env_block(env);

    tracing::debug!("restricted spawn: launching child process");
    process::spawn_with_token(tok.as_raw(), &mut command_line, cwd, &env_block, stdin_open)
}
