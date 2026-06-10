//! Unit tests for the pure Scratchpad Pane model (§12.3.3). These exercise the
//! editing primitives (insert/delete/cursor/newline), the append-quote /
//! source-link verbs, the dirty flag, the clear verb, and the line/cursor math
//! directly, with no terminal — the pane's keyboard/mouse/render integration
//! through the real `render()` is covered by the capture-sink suite in
//! `lib_tests.rs`.

use super::*;

#[test]
fn a_fresh_scratchpad_is_empty_clean_and_one_line() {
    let pad = Scratchpad::new();
    assert!(pad.is_empty());
    assert!(!pad.is_dirty());
    assert_eq!(pad.text(), "");
    assert_eq!(pad.char_count(), 0);
    assert_eq!(pad.line_count(), 1, "an empty buffer is one line");
    assert!(pad.links().is_empty());
    // `lines()` always yields at least one (empty) line so the renderer never
    // special-cases an empty buffer.
    assert_eq!(pad.lines(), vec![""]);
    assert_eq!(pad.cursor_line_col(), (0, 0));
}

#[test]
fn typing_chars_appends_and_flags_dirty() {
    let mut pad = Scratchpad::new();
    for ch in "note".chars() {
        pad.insert_char(ch);
    }
    assert_eq!(pad.text(), "note");
    assert!(pad.is_dirty());
    assert_eq!(pad.char_count(), 4);
    assert_eq!(pad.cursor_line_col(), (0, 4));
}

#[test]
fn insert_text_in_the_middle_splices_at_the_caret() {
    let mut pad = Scratchpad::new();
    pad.insert_text("ad");
    pad.move_left(); // caret between a and d
    pad.insert_text("bc");
    assert_eq!(pad.text(), "abcd");
    // Caret advanced past the inserted run.
    assert_eq!(pad.cursor_line_col(), (0, 3));
}

#[test]
fn insert_empty_text_is_a_no_op_and_does_not_dirty() {
    let mut pad = Scratchpad::new();
    pad.insert_text("");
    assert!(pad.is_empty());
    assert!(!pad.is_dirty(), "inserting nothing never flags dirty");
}

#[test]
fn enter_inserts_a_newline_and_grows_the_line_count() {
    let mut pad = Scratchpad::new();
    pad.insert_text("first");
    pad.insert_char('\n');
    pad.insert_text("second");
    assert_eq!(pad.text(), "first\nsecond");
    assert_eq!(pad.line_count(), 2);
    assert_eq!(pad.lines(), vec!["first", "second"]);
    assert_eq!(pad.cursor_line_col(), (1, 6), "caret is on the second line");
}

#[test]
fn a_trailing_newline_keeps_an_empty_final_line() {
    let mut pad = Scratchpad::new();
    pad.insert_text("x\n");
    assert_eq!(pad.line_count(), 2);
    assert_eq!(pad.lines(), vec!["x", ""], "the empty final line is kept");
}

#[test]
fn backspace_deletes_the_char_before_the_caret() {
    let mut pad = Scratchpad::new();
    pad.insert_text("abc");
    pad.delete_back();
    assert_eq!(pad.text(), "ab");
    assert_eq!(pad.cursor_line_col(), (0, 2));
    // Backspace at the start of the buffer is a no-op.
    pad.move_home();
    pad.delete_back();
    assert_eq!(pad.text(), "ab");
}

#[test]
fn delete_forward_removes_the_char_at_the_caret() {
    let mut pad = Scratchpad::new();
    pad.insert_text("abc");
    pad.move_home();
    pad.delete_forward();
    assert_eq!(pad.text(), "bc");
    assert_eq!(pad.cursor_line_col(), (0, 0), "the caret stays put");
    // Delete at the end of the buffer is a no-op.
    pad.move_end();
    pad.delete_forward();
    assert_eq!(pad.text(), "bc");
}

#[test]
fn cursor_moves_saturate_at_both_ends() {
    let mut pad = Scratchpad::new();
    pad.insert_text("ab");
    pad.move_end();
    pad.move_right(); // already at the end
    assert_eq!(pad.cursor_line_col(), (0, 2));
    pad.move_home();
    pad.move_left(); // already at the start
    assert_eq!(pad.cursor_line_col(), (0, 0));
}

#[test]
fn editing_primitives_never_split_a_multibyte_char() {
    // A buffer of multi-byte chars: every edit must keep the caret on a char
    // boundary, so no primitive ever panics on a non-boundary slice.
    let mut pad = Scratchpad::new();
    pad.insert_text("café\u{2014}déjà"); // accented + em dash
    pad.move_home();
    for _ in 0..3 {
        pad.move_right();
    }
    pad.delete_forward(); // delete the 'é'
    assert_eq!(pad.text(), "caf\u{2014}déjà");
    // Walk to the end deleting back; never panics on a boundary.
    pad.move_end();
    for _ in 0..20 {
        pad.delete_back();
    }
    assert!(pad.is_empty());
}

#[test]
fn append_block_lands_a_quote_on_its_own_line() {
    let mut pad = Scratchpad::new();
    pad.insert_text("intro");
    pad.append_block("a quoted block");
    assert_eq!(
        pad.text(),
        "intro\na quoted block",
        "the quote is separated by a newline from existing content",
    );
    assert!(pad.is_dirty());
    // Caret parks at the end after an append.
    assert_eq!(pad.cursor_line_col(), (1, 14));
}

#[test]
fn append_block_into_an_empty_buffer_does_not_lead_with_a_newline() {
    let mut pad = Scratchpad::new();
    pad.append_block("solo");
    assert_eq!(pad.text(), "solo");
}

#[test]
fn append_block_after_a_trailing_newline_does_not_double_it() {
    let mut pad = Scratchpad::new();
    pad.insert_text("line\n");
    pad.append_block("next");
    assert_eq!(pad.text(), "line\nnext", "no doubled blank line");
}

#[test]
fn append_empty_block_is_a_no_op() {
    let mut pad = Scratchpad::new();
    pad.append_block("");
    assert!(pad.is_empty());
    assert!(!pad.is_dirty());
}

#[test]
fn append_source_link_records_provenance_and_splices_a_reference_line() {
    let mut pad = Scratchpad::new();
    pad.append_source_link(42, "shell — cargo test");
    assert_eq!(pad.links().len(), 1);
    let link = &pad.links()[0];
    assert_eq!(link.entry_id, 42, "the STABLE entry id is retained");
    assert!(link.label.contains("shell"));
    // The visible buffer carries a concise, copy-safe reference line.
    assert!(pad.text().contains("entry #42"));
    assert!(pad.text().contains("[source:"));
    assert!(pad.is_dirty());
}

#[test]
fn append_source_link_with_a_blank_label_still_carries_the_entry_id() {
    let mut pad = Scratchpad::new();
    pad.append_source_link(7, "   ");
    assert_eq!(pad.links().len(), 1);
    assert_eq!(pad.links()[0].entry_id, 7);
    assert!(pad.text().contains("entry #7"));
}

#[test]
fn source_links_are_bounded() {
    let mut pad = Scratchpad::new();
    for id in 0..(MAX_LINKS as u64 + 10) {
        pad.append_source_link(id, "label");
    }
    assert_eq!(
        pad.links().len(),
        MAX_LINKS,
        "the breadcrumb list is bounded",
    );
    // The OLDEST were dropped: the newest id must still be present.
    let newest = MAX_LINKS as u64 + 9;
    assert!(pad.links().iter().any(|l| l.entry_id == newest));
    assert!(
        !pad.links().iter().any(|l| l.entry_id == 0),
        "the oldest breadcrumb was evicted",
    );
}

#[test]
fn a_long_source_link_label_is_flattened_and_clipped() {
    let mut pad = Scratchpad::new();
    let long = "x".repeat(LABEL_CHARS * 3);
    pad.append_source_link(1, &format!("multi\nline\t{long}"));
    let label = &pad.links()[0].label;
    assert!(
        label.chars().count() <= LABEL_CHARS,
        "label clipped to LABEL_CHARS: {} chars",
        label.chars().count(),
    );
    assert!(!label.contains('\n'), "newlines flattened to spaces");
    assert!(
        label.ends_with('\u{2026}'),
        "an over-long label is ellipsized"
    );
}

#[test]
fn clear_empties_the_buffer_and_links_and_resets_the_caret() {
    let mut pad = Scratchpad::new();
    pad.insert_text("notes");
    pad.append_source_link(1, "src");
    pad.clear();
    assert!(pad.is_empty());
    assert!(pad.links().is_empty());
    assert_eq!(pad.cursor_line_col(), (0, 0));
    assert!(pad.is_dirty(), "clearing real content flags dirty");
}

#[test]
fn clearing_an_already_empty_pad_does_not_dirty_it() {
    let mut pad = Scratchpad::new();
    pad.clear();
    assert!(!pad.is_dirty(), "no content cleared, no dirty flag");
}

#[test]
fn mark_clean_clears_the_dirty_flag_without_touching_text() {
    let mut pad = Scratchpad::new();
    pad.insert_text("kept");
    assert!(pad.is_dirty());
    pad.mark_clean();
    assert!(!pad.is_dirty());
    assert_eq!(pad.text(), "kept", "the buffer survives a mark_clean");
    // A later edit re-dirties it.
    pad.insert_char('!');
    assert!(pad.is_dirty());
}

#[test]
fn cursor_line_col_tracks_position_across_lines() {
    let mut pad = Scratchpad::new();
    pad.insert_text("ab\ncde\nf");
    // Caret at the very end.
    assert_eq!(pad.cursor_line_col(), (2, 1));
    pad.move_home();
    assert_eq!(pad.cursor_line_col(), (0, 0));
}
