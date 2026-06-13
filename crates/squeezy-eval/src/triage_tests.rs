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

#[test]
fn tail_text_does_not_panic_on_multibyte_boundary() {
    // Each '世' is 3 bytes. With no newlines and a budget that forces the
    // raw `len - budget` offset to land inside one of these scalars, the
    // old `data[start..]` slice would panic ("not a char boundary").
    let data = "世".repeat(100); // 300 bytes, no '\n'
    let path = std::env::temp_dir().join(format!(
        "squeezy_tail_text_{}_{}.txt",
        std::process::id(),
        line!()
    ));
    std::fs::write(&path, &data).expect("write fixture");

    // 300 - 200 = 100, and byte 100 is a continuation byte of a 3-byte char.
    let out = tail_text(&path, 200);
    let _ = std::fs::remove_file(&path);

    let out = out.expect("tail_text should succeed");
    // Result must be valid UTF-8 (it is a String) and no longer than budget.
    assert!(out.len() <= 200);
    // Tail of repeated identical chars should still be a run of '世'.
    assert!(out.chars().all(|c| c == '世'));
    assert!(!out.is_empty());
}
