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

use squeezy_core::SessionMode;

/// Plan-mode behavioural overlay. Kept ≤700 chars by design.
pub(crate) const PLAN_MODE_INSTRUCTIONS: &str = "Plan mode: investigate non-mutatively (Read/Search tools only), ask clarifying multi-choice questions via the request_user_input tool when the spec is ambiguous, and finish with a single <proposed_plan>...</proposed_plan> block listing the agreed steps. Do not edit files, run shells, or call mutating tools — those are off the table in this mode. If the user asks to execute, run, or apply the plan, do not attempt the edits; instead tell them to press Shift+Tab to switch to Build mode (the same prompt then runs in Build) or to refine the plan further.";

/// Compose per-turn instructions for the active session mode. Build mode
/// returns the base instructions verbatim so existing behaviour is
/// unchanged; Plan mode appends [`PLAN_MODE_INSTRUCTIONS`].
pub(crate) fn instructions_for_mode(base: &str, mode: SessionMode) -> String {
    match mode {
        SessionMode::Plan => format!("{base}\n\n{PLAN_MODE_INSTRUCTIONS}"),
        SessionMode::Build => base.to_string(),
    }
}

#[cfg(test)]
#[path = "plan_mode_tests.rs"]
mod tests;
