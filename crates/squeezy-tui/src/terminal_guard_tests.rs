use super::sanitize_osc_text;

#[test]
fn sanitize_osc_text_strips_osc_terminators() {
    // A raw BEL or ESC in the workspace path must never survive into the
    // OSC 0 title string — either would terminate / escape it early.
    assert_eq!(sanitize_osc_text("~/proj\u{07}ect"), "~/project");
    assert_eq!(sanitize_osc_text("~/pro\u{1b}[2Ject"), "~/pro[2Ject");
}

#[test]
fn sanitize_osc_text_maps_whitespace_and_drops_c0() {
    // Newline / tab collapse to a space; other C0 control bytes are dropped.
    assert_eq!(sanitize_osc_text("a\nb\tc"), "a b c");
    assert_eq!(sanitize_osc_text("a\u{00}b\u{1f}c"), "abc");
}

#[test]
fn sanitize_osc_text_preserves_printable_and_unicode() {
    // Ordinary printable text (including multi-byte Unicode) is untouched.
    assert_eq!(
        sanitize_osc_text("~/squeezy ● working"),
        "~/squeezy ● working"
    );
}
