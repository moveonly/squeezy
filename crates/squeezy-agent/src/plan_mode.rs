//! Plan-mode prompt overlay.
//!
//! Squeezy's Plan mode already strips mutating tools at spec-build time
//! (see `mode_refuses_capability` in `lib.rs`). That stops the model from
//! editing files but does not *tell* the model it is in Plan mode, so it
//! often retries blocked tools or skips the discussion stage entirely.
//!
//! The overlay below is appended to the per-turn instructions when the
//! session is in Plan mode. The text is intentionally tiny — the F07 audit
//! flagged Codex's 4.5 KB `plan.md` as overkill for Squeezy's cost thesis.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use squeezy_core::SessionMode;

/// Workspace-relative directory the TUI writes proposed plans into. The
/// agent uses this to locate the active plan file for refinement turns.
/// Kept in sync with `crates/squeezy-tui/src/proposed_plan.rs::PLAN_DIR`.
pub(crate) const PLAN_DIR: &str = ".squeezy/plans";

/// Plan-mode behavioural overlay. Kept ≤700 chars by design.
pub(crate) const PLAN_MODE_INSTRUCTIONS: &str = "Plan mode: investigate non-mutatively (Read/Search tools only), ask clarifying multi-choice questions via the request_user_input tool when the spec is ambiguous, and finish with a single <proposed_plan>...</proposed_plan> block listing the agreed steps. Do not edit files, run shells, or call mutating tools — those are off the table in this mode. If the user asks to execute, run, or apply the plan, do not attempt the edits; instead tell them to press Shift+Tab to switch to Build mode (the same prompt then runs in Build) or to refine the plan further.";

/// Suffix appended to [`PLAN_MODE_INSTRUCTIONS`] when an active plan file
/// already exists on disk. It tells the model exactly which file holds
/// the previous proposal so a refinement turn can anchor on it instead
/// of regenerating from scratch.
fn refinement_hint(plan_path: &Path) -> String {
    format!(
        " Active plan file: {}. If the user asks to refine, adjust, or revise the plan, read this file first with read_file and let your next <proposed_plan> block be a refinement of it rather than a fresh draft.",
        plan_path.display()
    )
}

/// Compose per-turn instructions for the active session mode.
/// Build mode returns the base instructions verbatim so existing behaviour
/// is unchanged; Plan mode appends [`PLAN_MODE_INSTRUCTIONS`] plus an
/// optional refinement hint pointing at the most recently persisted plan
/// file under `<workspace>/.squeezy/plans/`.
pub(crate) fn instructions_for_mode(
    base: &str,
    mode: SessionMode,
    workspace_root: &Path,
) -> String {
    match mode {
        SessionMode::Plan => {
            let mut out = format!("{base}\n\n{PLAN_MODE_INSTRUCTIONS}");
            if let Some(plan_path) = latest_plan_path(workspace_root) {
                out.push_str(&refinement_hint(&plan_path));
            }
            out
        }
        SessionMode::Build => base.to_string(),
    }
}

/// Newest `.md` file under `<workspace>/.squeezy/plans/`, by mtime. Returns
/// `None` when the directory does not exist or holds no plan files.
pub(crate) fn latest_plan_path(workspace_root: &Path) -> Option<PathBuf> {
    let dir = workspace_root.join(PLAN_DIR);
    let entries = std::fs::read_dir(&dir).ok()?;
    let mut newest: Option<(SystemTime, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        match &newest {
            Some((current, _)) if *current >= modified => {}
            _ => newest = Some((modified, path)),
        }
    }
    newest.map(|(_, path)| path)
}

#[cfg(test)]
#[path = "plan_mode_tests.rs"]
mod tests;
