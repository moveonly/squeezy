//! Plan-mode prompt overlay.
//!
//! Squeezy's Plan mode blocks mutating tools at the permission boundary
//! (see `mode_refuses_capability` in `lib.rs`) while leaving non-mutating
//! discovery commands on the normal permission path. That stops the model from
//! editing files but still lets it run targeted shell/git/build probes that
//! improve the plan.
//!
//! The overlay below is appended to the per-turn instructions when the
//! session is in Plan mode. The text is intentionally tiny — a multi-KB
//! plan-mode preamble is overkill for Squeezy's cost thesis.
//!
//! Design anti-pattern: do not promote Plan mode to a model-callable
//! `EnterPlanMode` tool. A tool that ships ~165 lines of "when to use"
//! prompt with the spec every turn it is in scope pays the overlay cost
//! on every turn whether Plan mode is wanted or not. The session-level
//! switch driven by the user (Shift+Tab / `/plan`) pays the overlay cost
//! only while Plan mode is active and keeps mode entry out of the
//! model's hands.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use squeezy_core::SessionMode;

/// Workspace-relative directory the TUI writes proposed plans into. Per
/// session the layout is `<PLAN_DIR>/<session_id>/<plan_id>.md`; the
/// agent looks up the active plan via the session's `current` pointer
/// file with an mtime-scan fallback. Kept in sync with
/// `crates/squeezy-tui/src/proposed_plan.rs::PLAN_DIR`.
pub(crate) const PLAN_DIR: &str = ".squeezy/plans";

/// Opening delimiter for the proposed-plan block the model emits at the
/// end of a Plan-mode turn. Single source of truth shared with the TUI
/// extractor; both crates must agree exactly on the spelling.
pub const PROPOSED_PLAN_OPEN_TAG: &str = "<proposed_plan>";

/// Closing delimiter for the proposed-plan block.
pub const PROPOSED_PLAN_CLOSE_TAG: &str = "</proposed_plan>";

/// File name (inside a per-session subdir) that holds the id of the
/// active plan. Mirrors `proposed_plan::CURRENT_POINTER_FILE`.
pub(crate) const CURRENT_POINTER_FILE: &str = "current";

/// Plan-mode behavioural overlay. Three-phase nudge that steers the
/// model away from the two failure modes the v2 ship-out exposed:
/// leaping straight to a plan without grounding, and over-asking the
/// user instead of exploring the codebase. Capped at ≤3500 chars by
/// test to keep token cost low.
pub(crate) const PLAN_MODE_INSTRUCTIONS: &str = "Plan mode is active. The user wants a plan before implementation. You may explore and execute non-mutating actions that improve the plan: Read/Search tools, static inspection, read-only shell/git commands, and checks or builds that only write normal caches/artifacts. Do not edit repo files, apply patches, run formatters/codegen/migrations, or execute commands whose purpose is to implement the plan.\n\nWork through three phases. They can interleave; phase labels are guidance, not a script.\n\nPHASE 1 — Ground in the environment.\nBefore asking the user anything, perform at least one targeted exploration pass: open the files the request names, grep for the symbols and patterns involved, run non-mutating probes when they reduce ambiguity, and inspect adjacent code and tests. Resolve as much as you can from the codebase itself.\n\nPHASE 2 — Clarify intent.\nUse the request_user_input tool when you hit a question that materially changes the plan: an architectural fork, an ambiguous scope boundary, an assumption that needs locking. Ask only high-impact questions — one or two well-chosen questions beat a long survey. Skip the tool entirely when the request is unambiguous.\n\nPHASE 3 — Propose the plan.\nEnd your turn with a single <proposed_plan>...</proposed_plan> block. The opening tag must start a new line. Inside the block: a short Context section (why), a numbered list of concrete steps (what), the critical file paths involved, and a Verification section (how to confirm the change works). Keep it scannable — the user reads it as the canonical artifact.\n\nRefining a previous plan: refer to the Active plan file noted below (if one exists). You may edit that file directly with apply_patch when plan-file write access has been granted; otherwise emit a complete replacement <proposed_plan> block — not a diff.\n\nIf the user asks to implement, mutate files, or apply the plan from inside Plan mode, do not edit anything; tell them to press Shift+Tab (or accept the post-plan prompt) to switch to Build mode.";

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
/// optional refinement hint pointing at the active plan file for the
/// current session (looked up via the `current` pointer with mtime
/// fallback).
pub(crate) fn instructions_for_mode(
    base: &str,
    mode: SessionMode,
    workspace_root: &Path,
    session_id: Option<&str>,
) -> String {
    match mode {
        SessionMode::Plan => {
            let mut out = String::with_capacity(base.len() + 2 + PLAN_MODE_INSTRUCTIONS.len());
            out.push_str(base);
            out.push_str("\n\n");
            out.push_str(PLAN_MODE_INSTRUCTIONS);
            if let Some(plan_path) = latest_plan_path(workspace_root, session_id) {
                out.push_str(&refinement_hint(&plan_path));
            }
            out
        }
        SessionMode::Build => base.to_string(),
    }
}

/// Whether the model should be allowed to edit the active plan file from
/// inside Plan mode. True only when the session is in Plan mode AND an
/// active plan file already exists for this session (no point exposing
/// `apply_patch` when there is nothing yet to refine).
pub(crate) fn plan_edit_allowed_in_workspace(
    mode: SessionMode,
    workspace_root: &Path,
    session_id: Option<&str>,
) -> bool {
    mode == SessionMode::Plan && latest_plan_path(workspace_root, session_id).is_some()
}

/// Exact-match check used by the runtime permission gate to grant Plan
/// mode the right to edit *the* active plan file but nothing else.
/// Both paths are canonicalised so `..` traversal and symlink trickery
/// cannot smuggle a different file past the check. Returns `false` on
/// any canonicalisation failure (e.g. the target does not exist on disk
/// yet) — the safe default in a deny-by-default permission gate.
///
/// When the caller already holds a pre-canonicalized active plan path
/// (from [`canonicalize_active_plan_path`]), prefer
/// [`is_active_plan_path_with_canon`] to avoid re-canonicalising the
/// active side on every call.
pub(crate) fn is_active_plan_path(target: &Path, active: &Path) -> bool {
    let Ok(target_canon) = std::fs::canonicalize(target) else {
        return false;
    };
    let Ok(active_canon) = std::fs::canonicalize(active) else {
        return false;
    };
    target_canon == active_canon
}

/// Pre-canonicalize the active plan path so the result can be stored and
/// reused across multiple permission checks in the same request batch.
/// Returns `None` when `canonicalize` fails (file removed mid-turn).
pub(crate) fn canonicalize_active_plan_path(active: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(active).ok()
}

/// Like [`is_active_plan_path`] but accepts a pre-canonicalized active
/// plan path, avoiding a redundant `canonicalize` call.
pub(crate) fn is_active_plan_path_with_canon(target: &Path, active_canon: &Path) -> bool {
    let Ok(target_canon) = std::fs::canonicalize(target) else {
        return false;
    };
    target_canon == active_canon
}

/// Active plan file path for a session, preferring the session's
/// `current` pointer (single source of truth, issue 17) and falling
/// back to mtime scan when the pointer is missing (resume / first-run
/// scenarios). Returns `None` when nothing is on disk.
pub(crate) fn latest_plan_path(workspace_root: &Path, session_id: Option<&str>) -> Option<PathBuf> {
    let session_id = session_id?;
    let session_dir = workspace_root.join(PLAN_DIR).join(session_id);
    // Pointer first.
    let pointer = session_dir.join(CURRENT_POINTER_FILE);
    if let Ok(raw) = std::fs::read_to_string(&pointer) {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            let candidate = session_dir.join(format!("{trimmed}.md"));
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    // Mtime fallback.
    let entries = std::fs::read_dir(&session_dir).ok()?;
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

/// Remove every `<proposed_plan>...</proposed_plan>` block (and any
/// trailing unterminated open-tag tail) from an assistant message. The
/// structured Plan card is the canonical visualization for those bodies;
/// keeping the raw markup in the displayed/persisted transcript renders
/// the plan twice. Whitespace immediately surrounding a removed block is
/// collapsed so the residual prose reads naturally.
pub fn strip_proposed_plan_blocks(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut remaining = input;
    loop {
        let Some(open_idx) = remaining.find(PROPOSED_PLAN_OPEN_TAG) else {
            out.push_str(remaining);
            break;
        };
        out.push_str(&remaining[..open_idx]);
        let after_open = &remaining[open_idx + PROPOSED_PLAN_OPEN_TAG.len()..];
        match after_open.find(PROPOSED_PLAN_CLOSE_TAG) {
            Some(close_idx) => {
                remaining = &after_open[close_idx + PROPOSED_PLAN_CLOSE_TAG.len()..];
            }
            None => break,
        }
    }
    collapse_block_seam(&out)
}

/// Trim surrounding whitespace and collapse the run of blank lines a
/// removed block leaves behind into at most one blank line. Cheap, no
/// regex, walks the string twice.
fn collapse_block_seam(text: &str) -> String {
    let trimmed = text.trim();
    let mut out = String::with_capacity(trimmed.len());
    let mut blank_run = 0usize;
    for line in trimmed.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        if body.trim().is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                out.push_str(line);
            }
        } else {
            blank_run = 0;
            out.push_str(line);
        }
    }
    out
}

#[cfg(test)]
#[path = "plan_mode_tests.rs"]
mod tests;
