use super::*;
use crate::findings::{Finding, Severity};

#[test]
fn instructions_include_focus_when_set() {
    let body = build_instructions(Some("/compact behavior"), None, &[]);
    assert!(body.contains("Focus area: /compact behavior"));
    assert!(body.contains("Drop findings unrelated"));
}

#[test]
fn instructions_omit_focus_when_blank() {
    let body = build_instructions(Some("   "), None, &[]);
    assert!(!body.contains("Focus area:"));
}

#[test]
fn instructions_include_extra_prompt() {
    let body = build_instructions(None, Some("Be terse."), &[]);
    assert!(body.ends_with("Be terse."));
}

#[test]
fn instructions_list_known_findings_and_forbid_duplicates() {
    let findings = vec![Finding {
        rule_id: "high_tool_burst".into(),
        severity: Severity::Minor,
        summary: "Turn 3 fired 11 tool calls".into(),
        category: "perf".into(),
        evidence: vec![],
    }];
    let body = build_instructions(None, None, &findings);
    assert!(body.contains("DO NOT re-report"));
    assert!(body.contains("[high_tool_burst] Turn 3 fired 11 tool calls"));
}
