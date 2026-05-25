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
    // F07 audit caps Plan-mode overlay at ≤500 chars; trip-wire so a future
    // edit can't silently bloat the prompt.
    assert!(
        PLAN_MODE_INSTRUCTIONS.len() <= 500,
        "PLAN_MODE_INSTRUCTIONS length {} > 500",
        PLAN_MODE_INSTRUCTIONS.len()
    );
}

#[test]
fn plan_mode_mentions_proposed_plan_contract() {
    assert!(PLAN_MODE_INSTRUCTIONS.contains("<proposed_plan>"));
    assert!(PLAN_MODE_INSTRUCTIONS.contains("request_user_input"));
}
