use squeezy_core::SessionMode;

use super::{PLAN_MODE_INSTRUCTIONS, instructions_for_mode};

#[test]
fn plan_mode_appends_overlay() {
    let out = instructions_for_mode("base", SessionMode::Plan);
    assert!(out.starts_with("base"));
    assert!(out.contains(PLAN_MODE_INSTRUCTIONS));
}

#[test]
fn build_mode_returns_base_verbatim() {
    let out = instructions_for_mode("base instructions", SessionMode::Build);
    assert_eq!(out, "base instructions");
}

#[test]
fn plan_mode_instructions_are_concise() {
    // F07 audit caps Plan-mode overlay at ≤700 chars; trip-wire so a future
    // edit can't silently bloat the prompt.
    assert!(
        PLAN_MODE_INSTRUCTIONS.len() <= 700,
        "PLAN_MODE_INSTRUCTIONS length {} > 700",
        PLAN_MODE_INSTRUCTIONS.len()
    );
}

#[test]
fn plan_mode_mentions_proposed_plan_contract() {
    assert!(PLAN_MODE_INSTRUCTIONS.contains("<proposed_plan>"));
    assert!(PLAN_MODE_INSTRUCTIONS.contains("request_user_input"));
}

#[test]
fn plan_mode_tells_user_how_to_execute() {
    // When the user signals execute/run/apply in Plan mode, the agent must
    // not silently refuse — it must point them at Shift+Tab so they can
    // unstick the conversation without guessing.
    assert!(
        PLAN_MODE_INSTRUCTIONS.contains("Shift+Tab"),
        "PLAN_MODE_INSTRUCTIONS must reference Shift+Tab to unblock execute requests"
    );
}
