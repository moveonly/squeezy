use super::*;

#[test]
fn is_very_large_paste_uses_char_count_threshold() {
    // Right at the threshold is not "very large" (strictly greater).
    let at = "x".repeat(VERY_LARGE_PASTE_CHAR_THRESHOLD);
    assert!(!is_very_large_paste(&at));

    let over = "x".repeat(VERY_LARGE_PASTE_CHAR_THRESHOLD + 1);
    assert!(is_very_large_paste(&over));

    assert!(!is_very_large_paste(""));
    assert!(!is_very_large_paste("a small paste"));
}

#[test]
fn is_very_large_paste_counts_chars_not_bytes() {
    // Multi-byte chars: byte length exceeds the threshold but char count does
    // not, so this must NOT count as a very large paste.
    let multibyte = "é".repeat(VERY_LARGE_PASTE_CHAR_THRESHOLD); // 2 bytes each
    assert!(multibyte.len() > VERY_LARGE_PASTE_CHAR_THRESHOLD);
    assert!(!is_very_large_paste(&multibyte));
}

#[test]
fn line_count_handles_empty_trailing_and_no_trailing_newline() {
    assert_eq!(line_count(""), 0);
    assert_eq!(line_count("abc"), 1);
    assert_eq!(line_count("a\nb"), 2);
    // A trailing newline does not add a phantom empty line.
    assert_eq!(line_count("a\n"), 1);
    assert_eq!(line_count("a\nb\n"), 2);
    // A bare newline is a single (empty) line.
    assert_eq!(line_count("\n"), 1);
}

#[test]
fn preview_stats_are_captured_on_construction() {
    let preview = PastePreview::new("hello\nworld".to_string());
    assert_eq!(preview.char_count(), 11);
    assert_eq!(preview.line_count(), 2);
    assert_eq!(preview.byte_count(), 11);
    assert_eq!(preview.text(), "hello\nworld");
}

#[test]
fn byte_count_differs_from_char_count_for_multibyte() {
    let preview = PastePreview::new("héllo".to_string());
    assert_eq!(preview.char_count(), 5);
    assert_eq!(preview.byte_count(), 6); // é is two bytes
}

#[test]
fn into_text_returns_the_pending_text_verbatim() {
    let preview = PastePreview::new("exact\ttext\n".to_string());
    assert_eq!(preview.into_text(), "exact\ttext\n");
}

#[test]
fn summary_is_singular_plural_aware_and_grouped() {
    let single_line = PastePreview::new("x".to_string());
    assert_eq!(single_line.summary(), "1 line · 1 char · 1 byte");

    let multi = PastePreview::new("ab\ncd".to_string());
    assert_eq!(multi.summary(), "2 lines · 5 chars · 5 bytes");

    // Large counts get thousands separators.
    let big = PastePreview::new("y".repeat(3_420));
    assert_eq!(big.summary(), "1 line · 3,420 chars · 3,420 bytes");
}

#[test]
fn preview_lines_clips_to_width_with_ellipsis() {
    let preview = PastePreview::new("abcdefghij".to_string());
    let lines = preview.preview_lines(5);
    assert_eq!(lines.len(), 1);
    // Width 5: keep 4 chars + ellipsis.
    assert_eq!(lines[0], "abcd…");
}

#[test]
fn preview_lines_passes_short_lines_through_unclipped() {
    let preview = PastePreview::new("hi\nthere".to_string());
    let lines = preview.preview_lines(40);
    assert_eq!(lines, vec!["hi".to_string(), "there".to_string()]);
}

#[test]
fn preview_lines_caps_at_max_lines_with_more_marker() {
    // Build PREVIEW_MAX_LINES + 5 lines.
    let total = PREVIEW_MAX_LINES + 5;
    let body: Vec<String> = (0..total).map(|i| format!("line{i}")).collect();
    let preview = PastePreview::new(body.join("\n"));
    assert_eq!(preview.line_count(), total);

    let lines = preview.preview_lines(40);
    // PREVIEW_MAX_LINES body lines + one "+N more lines" marker.
    assert_eq!(lines.len(), PREVIEW_MAX_LINES + 1);
    assert_eq!(lines[0], "line0");
    assert_eq!(
        lines[PREVIEW_MAX_LINES - 1],
        format!("line{}", PREVIEW_MAX_LINES - 1)
    );
    let remaining = total - PREVIEW_MAX_LINES;
    assert_eq!(
        lines[PREVIEW_MAX_LINES],
        format!("… +{remaining} more lines")
    );
}

#[test]
fn preview_lines_more_marker_is_singular_for_one_extra_line() {
    let total = PREVIEW_MAX_LINES + 1;
    let body: Vec<String> = (0..total).map(|i| format!("l{i}")).collect();
    let preview = PastePreview::new(body.join("\n"));
    let lines = preview.preview_lines(40);
    assert_eq!(lines.len(), PREVIEW_MAX_LINES + 1);
    assert_eq!(lines[PREVIEW_MAX_LINES], "… +1 more line");
}

#[test]
fn preview_lines_zero_width_does_not_panic() {
    let preview = PastePreview::new("content\n".to_string());
    let lines = preview.preview_lines(0);
    // Non-empty line collapses to the ellipsis marker; no panic.
    assert_eq!(lines, vec!["…".to_string()]);
}

#[test]
fn clip_line_never_splits_multibyte_chars() {
    // Each char is two bytes; clipping by char must stay on char boundaries.
    let clipped = clip_line("ααααα", 3);
    // Width 3: keep 2 chars + ellipsis.
    assert_eq!(clipped, "αα…");
    // Round-trips as valid UTF-8 (it is a String already, but assert length is
    // by-char as intended).
    assert_eq!(clipped.chars().count(), 3);
}

#[test]
fn clip_line_empty_input_with_zero_width_is_empty() {
    assert_eq!(clip_line("", 0), "");
    assert_eq!(clip_line("", 5), "");
}

#[test]
fn group_thousands_inserts_separators() {
    assert_eq!(group_thousands(0), "0");
    assert_eq!(group_thousands(42), "42");
    assert_eq!(group_thousands(1_000), "1,000");
    assert_eq!(group_thousands(12_345_678), "12,345,678");
}

#[test]
fn paste_decision_round_trips() {
    assert_eq!(PasteDecision::Confirm, PasteDecision::Confirm);
    assert_ne!(PasteDecision::Confirm, PasteDecision::Cancel);
}
