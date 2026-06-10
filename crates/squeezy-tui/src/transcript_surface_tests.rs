use super::*;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

fn styled_line(text: &str) -> Line<'static> {
    // Split into two spans to prove the plain-text projection re-joins
    // them losslessly regardless of span boundaries.
    let (a, b) = text.split_at(text.len() / 2);
    Line::from(vec![
        Span::styled(a.to_string(), Style::default()),
        Span::styled(b.to_string(), Style::default()),
    ])
}

fn row(id: usize, text: &str) -> TranscriptRow {
    let line = styled_line(text);
    let copy_text = plain_text_of_line(&line);
    let char_len = copy_text.chars().count();
    let style_spans = style_spans_of_line(&line);
    TranscriptRow {
        row_id: RowId(id),
        entry_id: Some(EntryId(id as u64)),
        entry_kind: Some(RowKind::Message),
        visual_line_index: 0,
        line,
        copy_text,
        text_range: 0..char_len,
        style_spans,
        fold_state: FoldState::Expanded,
        search_match_ranges: Vec::new(),
    }
}

#[test]
fn plain_text_rejoins_spans_losslessly() {
    let line = styled_line("hello world from spans");
    assert_eq!(plain_text_of_line(&line), "hello world from spans");
}

#[test]
fn strip_focus_caret_only_eats_a_true_column_zero_caret() {
    // A focused header opens on the selection caret `"> "`; that chrome IS
    // stripped so copied text is focus-invariant.
    assert_eq!(strip_focus_caret("> answer text"), "answer text");
    assert_eq!(strip_focus_caret(">  answer text"), "answer text");

    // Load-bearing invariant guard (finding #18): content rows in this surface
    // never begin with `"> "` at column 0 — body text and Markdown blockquotes
    // hang under a whitespace indent. Pin that a blockquote rendered on an
    // indented continuation row is NOT mutated, so a future renderer change that
    // emitted `"> "` at column 0 would fail this loudly instead of silently
    // eating the blockquote marker from copied text.
    assert_eq!(
        strip_focus_caret("    > quoted body"),
        "    > quoted body",
        "an indented blockquote must survive the caret strip untouched"
    );
    // A bare `>` with no following space (real content) is never eaten.
    assert_eq!(strip_focus_caret(">no space"), ">no space");
}

#[test]
fn strip_message_marker_handles_no_space_and_eol_markers() {
    // No space after the marker: drop just the marker glyph, keep the content
    // verbatim (slices on a `char_indices` boundary, so it stays UTF-8-safe).
    assert_eq!(strip_message_marker("☽answer"), "answer");
    // The marker is the only/last char: nothing remains after it.
    assert_eq!(strip_message_marker("●"), "");
}

#[test]
fn plain_text_of_spans_matches_line_projection() {
    let line = styled_line("alpha beta gamma");
    assert_eq!(plain_text_of_spans(&line.spans), plain_text_of_line(&line));
}

#[test]
fn plain_text_handles_empty_line() {
    assert_eq!(plain_text_of_line(&Line::from("")), "");
    assert_eq!(plain_text_of_spans(&[]), "");
}

#[test]
fn copy_range_joins_one_row_per_line() {
    let rows = vec![row(0, "first line"), row(1, "second line"), row(2, "third")];
    assert_eq!(copy_range(&rows), "first line\nsecond line\nthird");
}

#[test]
fn copy_range_empty_is_empty_string() {
    assert_eq!(copy_range(&[]), "");
}

#[test]
fn copy_row_span_is_inclusive_and_order_independent() {
    let rows = vec![row(0, "aa"), row(1, "bb"), row(2, "cc"), row(3, "dd")];
    assert_eq!(copy_row_span(&rows, RowId(1), RowId(2)), "bb\ncc");
    // Reversed anchors yield the same span.
    assert_eq!(copy_row_span(&rows, RowId(2), RowId(1)), "bb\ncc");
    // Single row.
    assert_eq!(copy_row_span(&rows, RowId(0), RowId(0)), "aa");
}

#[test]
fn copy_row_span_clamps_out_of_range_end() {
    let rows = vec![row(0, "aa"), row(1, "bb")];
    assert_eq!(copy_row_span(&rows, RowId(0), RowId(99)), "aa\nbb");
}

#[test]
fn copy_row_span_out_of_range_start_is_empty() {
    let rows = vec![row(0, "aa")];
    assert_eq!(copy_row_span(&rows, RowId(5), RowId(9)), "");
    assert_eq!(copy_row_span(&[], RowId(0), RowId(0)), "");
}

#[test]
fn detail_policy_expand_all_bit() {
    assert!(DetailPolicy::Expanded.expand_all());
    assert!(!DetailPolicy::Collapsed.expand_all());
}

#[test]
fn row_id_and_entry_id_are_ordered() {
    assert!(RowId(1) < RowId(2));
    assert!(EntryId(1) < EntryId(2));
}

// --- Drift guards: keep the owned mirror enums exhaustive vs. lib.rs ---

#[test]
fn detail_policy_mirrors_every_overlay_detail_variant() {
    // Round-trips every `crate::OverlayDetail` value through the owned
    // `DetailPolicy` mapping. If a variant is added upstream the EXHAUSTIVE
    // match below stops compiling, which is the drift guard.
    for detail in [
        crate::OverlayDetail::Collapsed,
        crate::OverlayDetail::Expanded,
    ] {
        let policy = DetailPolicy::from(detail);
        let expected = match detail {
            crate::OverlayDetail::Collapsed => DetailPolicy::Collapsed,
            crate::OverlayDetail::Expanded => DetailPolicy::Expanded,
        };
        assert_eq!(policy, expected);
        // The owned `expand_all` bit must agree with the upstream one for
        // every variant, so the two enums can't drift on the only bit the
        // pipeline actually reads.
        assert_eq!(policy.expand_all(), detail.expand_all());
        // `&OverlayDetail` and owned `OverlayDetail` conversions agree.
        assert_eq!(DetailPolicy::from(&detail), policy);
    }
}

/// Compile-time exhaustiveness guard for the `RowKind` mapping. Listing every
/// `crate::TranscriptEntryKind` variant here means adding a new upstream
/// variant breaks this test's compilation as well as
/// [`RowKind::from_entry_kind`], so the classification is never silently
/// skipped. Never called with real data — its job is to compile.
#[allow(dead_code)]
fn row_kind_for_every_entry_kind(kind: &crate::TranscriptEntryKind) -> RowKind {
    let mapped = match kind {
        crate::TranscriptEntryKind::Message(_) => RowKind::Message,
        crate::TranscriptEntryKind::ToolResult(_) => RowKind::ToolResult,
        crate::TranscriptEntryKind::Log(_) => RowKind::Log,
        crate::TranscriptEntryKind::PlanCard(_) => RowKind::PlanCard,
        crate::TranscriptEntryKind::Diff(_) => RowKind::Diff,
        crate::TranscriptEntryKind::Reasoning(_) => RowKind::Reasoning,
        crate::TranscriptEntryKind::SlashEcho(_) => RowKind::SlashEcho,
    };
    // Must agree with the production mapping for the same input.
    debug_assert_eq!(mapped, RowKind::from(kind));
    debug_assert_eq!(mapped, RowKind::from_entry_kind(kind));
    mapped
}
