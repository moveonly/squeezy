use super::*;

#[test]
fn short_turn_strips_wrapper() {
    assert_eq!(short_turn("TurnId(3)"), "3");
    assert_eq!(short_turn("custom"), "custom");
}

#[test]
fn trim_oneline_collapses_newlines() {
    let out = trim_oneline("hello\nworld\nthere", 200);
    assert_eq!(out, "hello world there");
}

#[test]
fn trim_oneline_caps_length() {
    let s = "a".repeat(300);
    let out = trim_oneline(&s, 50);
    assert!(out.ends_with('…'));
    assert_eq!(out.chars().count(), 51);
}
