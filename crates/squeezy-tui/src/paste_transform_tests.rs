use super::*;

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

#[test]
fn classify_single_line_plain_text() {
    let payload = PastePayload::new("just one line".to_string());
    assert_eq!(payload.kind(), PasteKind::PlainSingle);
    assert_eq!(payload.line_count(), 1);
}

#[test]
fn classify_multiline_plain_text() {
    let payload = PastePayload::new("first line\nsecond line\nthird".to_string());
    assert_eq!(payload.kind(), PasteKind::PlainMultiline);
    assert_eq!(payload.line_count(), 3);
}

#[test]
fn classify_unified_diff() {
    let diff = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new";
    let payload = PastePayload::new(diff.to_string());
    assert_eq!(payload.kind(), PasteKind::Diff);
}

#[test]
fn classify_hunk_only_diff() {
    let diff = "@@ -10,3 +10,4 @@ fn main()\n context\n+added";
    assert_eq!(PastePayload::new(diff.to_string()).kind(), PasteKind::Diff);
}

#[test]
fn classify_json_object_and_array() {
    let object = "{\n  \"a\": 1,\n  \"b\": [2, 3]\n}";
    assert_eq!(
        PastePayload::new(object.to_string()).kind(),
        PasteKind::Json
    );
    let array = "[\n  1,\n  2\n]";
    assert_eq!(PastePayload::new(array.to_string()).kind(), PasteKind::Json);
}

#[test]
fn classify_json_requires_matching_envelope() {
    // Opens with `{` but does not close with `}` — not JSON-shaped.
    let not_json = "{ unbalanced\nmore text";
    assert_ne!(
        PastePayload::new(not_json.to_string()).kind(),
        PasteKind::Json
    );
}

#[test]
fn classify_log_block() {
    let log = "2026-06-09 ERROR boom\n2026-06-09 WARN careful\n2026-06-09 INFO ok";
    assert_eq!(PastePayload::new(log.to_string()).kind(), PasteKind::Log);
}

#[test]
fn classify_bracketed_timestamp_log() {
    let log = "[12:00:01] starting\n[12:00:02] connected\n[12:00:03] done";
    assert_eq!(PastePayload::new(log.to_string()).kind(), PasteKind::Log);
}

#[test]
fn classify_code_block_by_punctuation() {
    let code = "fn main() {\n    let x = 1;\n    println!(\"{x}\");\n}";
    assert_eq!(PastePayload::new(code.to_string()).kind(), PasteKind::Code);
}

#[test]
fn classify_prefers_diff_over_code() {
    // A diff that also has braces must classify as a diff, not code.
    let diff = "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-fn f() {}\n+fn f() { return; }";
    assert_eq!(PastePayload::new(diff.to_string()).kind(), PasteKind::Diff);
}

#[test]
fn classify_does_not_treat_windows_path_as_code() {
    // Single-line Windows path: structured-looking but a single line → plain.
    let path = r"C:\Users\me\file.txt";
    assert_eq!(
        PastePayload::new(path.to_string()).kind(),
        PasteKind::PlainSingle
    );
}

// ---------------------------------------------------------------------------
// should_open_transform_menu gate
// ---------------------------------------------------------------------------

#[test]
fn menu_does_not_open_for_plain_single_line() {
    let payload = PastePayload::new("/usr/local/bin/thing".to_string());
    assert!(!should_open_transform_menu(&payload));
}

#[test]
fn menu_opens_for_multiline() {
    let payload = PastePayload::new("line one\nline two".to_string());
    assert!(should_open_transform_menu(&payload));
}

#[test]
fn menu_opens_for_ansi_even_single_line() {
    let payload = PastePayload::new("\x1b[31mred\x1b[0m".to_string());
    assert!(payload.has_ansi());
    assert!(should_open_transform_menu(&payload));
}

#[test]
fn menu_opens_for_diff_json_code_log() {
    for text in [
        "--- a\n+++ b\n@@ -1 +1 @@",
        "{\n\"a\":1\n}",
        "fn f() {\n    g();\n}",
        "ERROR x\nERROR y",
    ] {
        let payload = PastePayload::new(text.to_string());
        assert!(
            should_open_transform_menu(&payload),
            "should open for {text:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Transforms (pure)
// ---------------------------------------------------------------------------

#[test]
fn transform_as_is_is_verbatim() {
    let text = "keep\tthis\nexactly\n";
    assert_eq!(apply_transform(text, PasteTransform::AsIs), text);
}

#[test]
fn transform_quote_prefixes_each_line() {
    let out = apply_transform("alpha\nbeta", PasteTransform::Quote);
    assert_eq!(out, "> alpha\n> beta");
}

#[test]
fn transform_quote_uses_bare_marker_for_blank_lines() {
    let out = apply_transform("a\n\nb", PasteTransform::Quote);
    assert_eq!(out, "> a\n>\n> b");
}

#[test]
fn transform_quote_preserves_trailing_newline() {
    let out = apply_transform("a\nb\n", PasteTransform::Quote);
    assert_eq!(out, "> a\n> b\n");
}

#[test]
fn transform_code_block_wraps_in_triple_fence() {
    let out = apply_transform("let x = 1;", PasteTransform::CodeBlock);
    assert_eq!(out, "```\nlet x = 1;\n```");
}

#[test]
fn transform_code_block_widens_fence_past_inner_backticks() {
    // Body contains a ``` fence; the wrapper must use a longer fence so the
    // body cannot close the block early.
    let body = "before\n```\ninner\n```\nafter";
    let out = apply_transform(body, PasteTransform::CodeBlock);
    assert!(out.starts_with("````\n"), "fence should widen: {out}");
    assert!(out.ends_with("\n````"), "fence should widen: {out}");
    // The original body is preserved between the fences.
    assert!(out.contains(body), "{out}");
}

#[test]
fn transform_code_block_strips_one_trailing_newline_before_fence() {
    let out = apply_transform("code\n", PasteTransform::CodeBlock);
    assert_eq!(out, "```\ncode\n```");
}

#[test]
fn transform_strip_ansi_removes_csi_sequences() {
    let out = apply_transform("\x1b[31mred\x1b[0m text", PasteTransform::StripAnsi);
    assert_eq!(out, "red text");
}

#[test]
fn transform_strip_ansi_leaves_plain_text_untouched() {
    let out = apply_transform("nothing to strip", PasteTransform::StripAnsi);
    assert_eq!(out, "nothing to strip");
}

#[test]
fn transform_strip_ansi_drops_trailing_bare_escape() {
    let out = apply_transform("text\x1b", PasteTransform::StripAnsi);
    assert_eq!(out, "text");
}

#[test]
fn transform_cancel_yields_empty_and_does_not_insert() {
    assert_eq!(apply_transform("anything", PasteTransform::Cancel), "");
    assert!(!PasteTransform::Cancel.inserts());
    assert!(PasteTransform::AsIs.inserts());
}

#[test]
fn longest_backtick_run_counts_consecutive_only() {
    assert_eq!(longest_backtick_run("no ticks"), 0);
    assert_eq!(longest_backtick_run("a `b` c"), 1);
    assert_eq!(longest_backtick_run("```fence```"), 3);
    assert_eq!(longest_backtick_run("`` `` ````"), 4);
}

// ---------------------------------------------------------------------------
// Payload stats
// ---------------------------------------------------------------------------

#[test]
fn payload_captures_stats_on_construction() {
    let payload = PastePayload::new("héllo\nworld".to_string());
    assert_eq!(payload.char_count(), 11);
    assert_eq!(payload.byte_count(), 12); // é is two bytes
    assert_eq!(payload.line_count(), 2);
    assert_eq!(payload.text(), "héllo\nworld");
}

#[test]
fn payload_summary_is_singular_plural_aware() {
    let single = PastePayload::new("x".to_string());
    assert_eq!(single.summary(), "text · 1 line · 1 char");

    let multi = PastePayload::new("a\nb\nc".to_string());
    assert_eq!(multi.summary(), "multiline text · 3 lines · 5 chars");
}

#[test]
fn payload_into_text_round_trips() {
    let payload = PastePayload::new("exact\ttext\n".to_string());
    assert_eq!(payload.into_text(), "exact\ttext\n");
}

// ---------------------------------------------------------------------------
// Menu model
// ---------------------------------------------------------------------------

#[test]
fn menu_omits_strip_ansi_for_clean_text() {
    let menu = PasteTransformMenu::new(PastePayload::new("a\nb".to_string()));
    assert!(!menu.items().contains(&PasteTransform::StripAnsi));
    // Clean text starts on As-is.
    assert_eq!(menu.selected_transform(), PasteTransform::AsIs);
    // The list always ends with Cancel.
    assert_eq!(menu.items().last(), Some(&PasteTransform::Cancel));
}

#[test]
fn menu_offers_and_preselects_strip_ansi_for_ansi_text() {
    let menu = PasteTransformMenu::new(PastePayload::new("\x1b[31mred\x1b[0m\nmore".to_string()));
    assert!(menu.items().contains(&PasteTransform::StripAnsi));
    assert_eq!(menu.selected_transform(), PasteTransform::StripAnsi);
}

#[test]
fn menu_cursor_wraps_both_directions() {
    let mut menu = PasteTransformMenu::new(PastePayload::new("a\nb".to_string()));
    let len = menu.items().len();
    assert!(len >= 2);
    // Up from the top wraps to the bottom.
    assert_eq!(menu.selected(), 0);
    menu.move_up();
    assert_eq!(menu.selected(), len - 1);
    // Down from the bottom wraps to the top.
    menu.move_down();
    assert_eq!(menu.selected(), 0);
    // Plain down advances by one.
    menu.move_down();
    assert_eq!(menu.selected(), 1);
}

#[test]
fn menu_select_clamps_to_range() {
    let mut menu = PasteTransformMenu::new(PastePayload::new("a\nb".to_string()));
    assert!(menu.select(1));
    assert_eq!(menu.selected(), 1);
    // Out of range is rejected and the cursor stays put.
    assert!(!menu.select(999));
    assert_eq!(menu.selected(), 1);
}

#[test]
fn menu_resolve_applies_selected_transform() {
    let mut menu = PasteTransformMenu::new(PastePayload::new("alpha\nbeta".to_string()));
    // Select Quote (index 1: As-is, Quote, Code block, Cancel).
    assert_eq!(menu.items()[1], PasteTransform::Quote);
    menu.select(1);
    assert_eq!(menu.resolve().as_deref(), Some("> alpha\n> beta"));
}

#[test]
fn menu_resolve_is_none_on_cancel() {
    let mut menu = PasteTransformMenu::new(PastePayload::new("alpha\nbeta".to_string()));
    let cancel_idx = menu
        .items()
        .iter()
        .position(|t| *t == PasteTransform::Cancel)
        .expect("cancel present");
    menu.select(cancel_idx);
    assert!(menu.resolve().is_none());
}

#[test]
fn menu_payload_is_borrowable_for_header() {
    let menu = PasteTransformMenu::new(PastePayload::new("a\nb\nc".to_string()));
    assert_eq!(menu.payload().line_count(), 3);
    assert!(menu.payload().summary().contains("3 lines"));
}

#[test]
fn transform_labels_and_descriptions_are_present() {
    for t in [
        PasteTransform::AsIs,
        PasteTransform::Quote,
        PasteTransform::CodeBlock,
        PasteTransform::StripAnsi,
        PasteTransform::Cancel,
    ] {
        assert!(!t.label().is_empty());
        assert!(!t.description().is_empty());
    }
}

#[test]
fn kind_labels_are_present() {
    for k in [
        PasteKind::Diff,
        PasteKind::Json,
        PasteKind::Code,
        PasteKind::Log,
        PasteKind::PlainMultiline,
        PasteKind::PlainSingle,
    ] {
        assert!(!k.label().is_empty());
    }
}
