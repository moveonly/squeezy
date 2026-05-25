use super::*;

#[test]
fn returns_input_untouched_when_under_cap() {
    let (out, truncated) = truncate_middle_bytes("hello", 100);
    assert_eq!(out, "hello");
    assert!(!truncated);
}

#[test]
fn returns_input_untouched_when_equal_to_cap() {
    let (out, truncated) = truncate_middle_bytes("hello", 5);
    assert_eq!(out, "hello");
    assert!(!truncated);
}

#[test]
fn keeps_tail_under_byte_cap() {
    let mut input = String::from("HEAD_SENTINEL_");
    input.push_str(&"x".repeat(5_000));
    input.push_str("_TAIL_SENTINEL");
    let cap = 200;
    let (out, truncated) = truncate_middle_bytes(&input, cap);
    assert!(truncated);
    assert!(
        out.len() <= cap,
        "truncated len {} > cap {}",
        out.len(),
        cap
    );
    assert!(
        out.contains("HEAD_SENTINEL_"),
        "head sentinel missing: {out:?}"
    );
    assert!(
        out.contains("_TAIL_SENTINEL"),
        "tail sentinel missing: {out:?}"
    );
    assert!(out.contains("chars truncated"), "marker missing: {out:?}");
}

#[test]
fn handles_multibyte_boundaries() {
    // Each Japanese char is 3 bytes, each emoji is 4.
    let mut input = String::from("こんにちは"); // 15 bytes
    for _ in 0..200 {
        input.push('🎉'); // 4 bytes
    }
    input.push_str("さようなら"); // 15 bytes
    let cap = 64;
    let (out, _) = truncate_middle_bytes(&input, cap);
    assert!(out.len() <= cap);
    // Output must be valid UTF-8 (Rust enforces; this just confirms no panic).
    assert!(!out.is_empty());
}

#[test]
fn marker_is_present_when_truncated() {
    let input = "a".repeat(1000);
    let (out, truncated) = truncate_middle_bytes(&input, 80);
    assert!(truncated);
    assert!(out.contains("chars truncated"));
}

#[test]
fn extremely_small_cap_falls_back_to_prefix() {
    // Cap too small to hold the marker — should still return ≤ cap bytes.
    let input = "abcdefghijklmnopqrstuvwxyz";
    let (out, _) = truncate_middle_bytes(input, 5);
    assert!(out.len() <= 5);
}

#[test]
fn truncate_middle_chars_returns_at_or_under_cap() {
    let input = "x".repeat(10_000);
    for cap in [100usize, 500, 1_000, 4_000] {
        let out = truncate_middle_chars(&input, cap);
        assert!(out.len() <= cap, "cap={cap} out.len={}", out.len());
    }
}
