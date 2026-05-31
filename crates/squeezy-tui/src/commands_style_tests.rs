use super::*;

#[test]
fn group_thousands_handles_typical_sizes() {
    assert_eq!(group_thousands(0), "0");
    assert_eq!(group_thousands(999), "999");
    assert_eq!(group_thousands(1_000), "1,000");
    assert_eq!(group_thousands(22_341), "22,341");
    assert_eq!(group_thousands(1_000_000), "1,000,000");
}

#[test]
fn helpers_emit_reset_when_color_resolved() {
    // In test context the active theme is the default, so palette
    // tokens resolve to Color::Rgb and we should see SGR + reset.
    let out = header("X");
    assert!(out.contains("\x1b["), "expected SGR prefix: {out:?}");
    assert!(out.ends_with(RESET), "expected SGR reset: {out:?}");
}

#[test]
fn headroom_picks_status_bands() {
    // Just confirm each band returns a non-empty string with a reset.
    for pct in [5.0, 20.0, 80.0] {
        let s = headroom(pct, "label");
        assert!(s.ends_with(RESET), "pct {pct}: {s:?}");
    }
}
