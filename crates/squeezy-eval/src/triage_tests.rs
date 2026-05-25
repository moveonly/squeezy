use super::*;

#[test]
fn instructions_include_focus_when_set() {
    let body = build_instructions(Some("/compact behavior"), None);
    assert!(body.contains("Focus area: /compact behavior"));
    assert!(body.contains("Drop findings unrelated"));
}

#[test]
fn instructions_omit_focus_when_blank() {
    let body = build_instructions(Some("   "), None);
    assert!(!body.contains("Focus area:"));
}

#[test]
fn instructions_include_extra_prompt() {
    let body = build_instructions(None, Some("Be terse."));
    assert!(body.ends_with("Be terse."));
}
