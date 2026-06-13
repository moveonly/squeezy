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

    let mut all_cap_sids: Vec<String> = Vec::new();
    if spec.token_mode == WinTokenMode::ReadOnlyCapability {
        all_cap_sids.push(cap::readonly_cap_sid(state_dir)?);
    }

    // For writable-roots mode, add per-root write capability SIDs and apply ACLs.
    // Do not also include the read-only cap: write-restricted tokens AND every
    // restricting SID against the requested access, so a cap that has no write
    // ACE on a workspace root (the read-only cap, by definition) would always
    // fail the AND and block legitimate workspace writes.
    //
    // Previously this branch also pushed `cap::readonly_cap_sid(...)` alongside
    // each per-root write SID; with both present the AND was unsatisfiable
    // for writes and the sandboxed child could not write to its workspace
    // root at all. The world-writable audit's `deny_sid` index moves with
    // this change (now `[0]`, the first writable-root SID) — both halves
    // are required and must stay in sync.
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
            acl::add_allow_ace_recursive(root, &write_sid)?;

            // Deny write on read-only carve-outs beneath this root.
            for ro_sub in &wr.read_only_subpaths {
                tracing::debug!(
                    "restricted spawn: add_deny_write_ace on '{}' (read-only subpath)",
                    ro_sub.display()
                );
                acl::add_deny_write_ace_recursive(ro_sub, &write_sid)?;
            }

            // Deny write on protected metadata names joined under this root.
            for name in &spec.protected_metadata_names {
                let meta_path = root.join(name);
                if meta_path.exists() {
                    tracing::debug!(
                        "restricted spawn: add_deny_write_ace on '{}' (protected metadata)",
                        meta_path.display()
                    );
                    acl::add_deny_write_ace_recursive(&meta_path, &write_sid)?;
                }
            }

            // If state_dir falls inside this writable root, deny write to it.
            // Use a path-boundary comparison rather than a raw string prefix:
            // canonical keys carry no trailing separator, so `starts_with`
            // alone would match across path components (e.g. root
            // "c:/proj/app" vs state "c:/proj/app-state/..."), causing a
            // spurious deny on a sibling directory. Mirrors the
            // `is_under_writable_root` semantics in world_writable.rs.
            let root_key = path_norm::canonical_key(root);
            if state_key == root_key || state_key.starts_with(&format!("{root_key}/")) {
                tracing::debug!(
                    "restricted spawn: add_deny_write_ace on state_dir '{}' (inside writable root)",
                    state_dir.display()
                );
                acl::add_deny_write_ace_recursive(state_dir, &write_sid)?;
            }

            all_cap_sids.push(write_sid);
        }
    }

    // World-writable escape-vector audit: deny the first writable-root cap SID on
    // any world-writable directories found outside the writable roots.  This
    // prevents the sandboxed process (which carries World as a restricting SID)
    // from writing to pre-existing world-writable directories.  Best-effort —
    // failures are logged but never propagate.
    if spec.token_mode == WinTokenMode::WritableRootsCapability && !spec.writable_roots.is_empty() {
        // The token carries every cap SID as a restricting SID, so denying any
        // one of them blocks the sandbox (all restricting SIDs must pass).
        let deny_sid = &all_cap_sids[0];
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
