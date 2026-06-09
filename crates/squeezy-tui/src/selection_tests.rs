//! Unit tests for the visual selection state and range math.
//!
//! Included into [`crate::selection`] via `#[path]` per the repo test layout.

use super::*;
use ratatui::style::{Color, Modifier};
use ratatui::text::Span;

// ---- builders -------------------------------------------------------------

fn line(s: &str) -> Line<'static> {
    Line::from(s.to_string())
}

/// A line built from several styled spans so span-splitting is exercised.
fn styled_line(parts: &[(&str, Style)]) -> Line<'static> {
    Line::from(
        parts
            .iter()
            .map(|(t, st)| Span::styled(t.to_string(), *st))
            .collect::<Vec<_>>(),
    )
}

fn sel(
    surface: SelectionSurface,
    anchor: (usize, usize),
    cursor: (usize, usize),
    mode: SelectionMode,
) -> Selection {
    Selection {
        surface,
        anchor: Pos::new(anchor.0, anchor.1),
        cursor: Pos::new(cursor.0, cursor.1),
        mode,
        width: 80,
    }
}

fn hl() -> Style {
    Style::default().add_modifier(Modifier::REVERSED)
}

/// Concatenate every span's content of a line back into plain text.
fn plain(l: &Line<'static>) -> String {
    l.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// The plain text covered by the highlighted (REVERSED) spans of a line.
fn highlighted_text(l: &Line<'static>) -> String {
    l.spans
        .iter()
        .filter(|s| s.style.add_modifier.contains(Modifier::REVERSED))
        .map(|s| s.content.as_ref())
        .collect()
}

// ---- normalization --------------------------------------------------------

#[test]
fn normalized_orders_anchor_before_cursor_forward_drag() {
    let s = sel(SelectionSurface::Main, (1, 2), (3, 4), SelectionMode::Cell);
    let (start, end) = s.normalized();
    assert_eq!(start, Pos::new(1, 2));
    assert_eq!(end, Pos::new(3, 4));
}

#[test]
fn normalized_orders_anchor_before_cursor_backward_drag() {
    // Cursor dragged UP/LEFT of the anchor: normalized must still read in order.
    let s = sel(SelectionSurface::Main, (3, 4), (1, 2), SelectionMode::Cell);
    let (start, end) = s.normalized();
    assert_eq!(start, Pos::new(1, 2));
    assert_eq!(end, Pos::new(3, 4));
}

#[test]
fn normalized_orders_within_same_row_by_column() {
    let s = sel(SelectionSurface::Main, (2, 9), (2, 3), SelectionMode::Cell);
    let (start, end) = s.normalized();
    assert_eq!(start, Pos::new(2, 3));
    assert_eq!(end, Pos::new(2, 9));
}

#[test]
fn row_span_is_inclusive_and_direction_agnostic() {
    let down = sel(SelectionSurface::Main, (2, 0), (5, 0), SelectionMode::Cell);
    let up = sel(SelectionSurface::Main, (5, 0), (2, 0), SelectionMode::Cell);
    assert_eq!(down.row_span(), 2..=5);
    assert_eq!(up.row_span(), 2..=5);
}

// ---- empty selection ------------------------------------------------------

#[test]
fn is_empty_when_cell_anchor_equals_cursor() {
    let s = sel(SelectionSurface::Main, (4, 7), (4, 7), SelectionMode::Cell);
    assert!(s.is_empty());
}

#[test]
fn is_not_empty_when_cell_columns_differ() {
    let s = sel(SelectionSurface::Main, (4, 7), (4, 8), SelectionMode::Cell);
    assert!(!s.is_empty());
}

#[test]
fn row_and_word_modes_are_never_empty_even_collapsed() {
    let row = sel(SelectionSurface::Main, (1, 3), (1, 3), SelectionMode::Row);
    let word = sel(SelectionSurface::Main, (1, 3), (1, 3), SelectionMode::Word);
    assert!(!row.is_empty());
    assert!(!word.is_empty());
}

#[test]
fn empty_cell_selection_yields_empty_col_span() {
    let s = sel(SelectionSurface::Main, (0, 5), (0, 5), SelectionMode::Cell);
    let span = s.col_span_for_row(0, 20).unwrap();
    assert!(span.is_empty(), "collapsed caret selects nothing: {span:?}");
}

// ---- single-row col spans -------------------------------------------------

#[test]
fn single_row_col_span_is_start_to_end() {
    let s = sel(SelectionSurface::Main, (0, 3), (0, 9), SelectionMode::Cell);
    assert_eq!(s.col_span_for_row(0, 40), Some(3..9));
}

#[test]
fn col_span_none_outside_row_span() {
    let s = sel(SelectionSurface::Main, (2, 0), (4, 6), SelectionMode::Cell);
    assert_eq!(s.col_span_for_row(1, 40), None);
    assert_eq!(s.col_span_for_row(5, 40), None);
}

#[test]
fn single_row_col_span_clamps_to_row_len() {
    let s = sel(
        SelectionSurface::Main,
        (0, 3),
        (0, 100),
        SelectionMode::Cell,
    );
    // row only 10 chars wide.
    assert_eq!(s.col_span_for_row(0, 10), Some(3..10));
}

// ---- multi-row col spans (first / middle / last) --------------------------

#[test]
fn multi_row_first_row_runs_anchor_to_row_end() {
    let s = sel(SelectionSurface::Main, (1, 4), (3, 2), SelectionMode::Cell);
    // first row: from col 4 to its own width (12).
    assert_eq!(s.col_span_for_row(1, 12), Some(4..12));
}

#[test]
fn multi_row_interior_rows_are_full_width() {
    let s = sel(SelectionSurface::Main, (1, 4), (4, 2), SelectionMode::Cell);
    // interior rows 2 and 3: full width (their own widths differ).
    assert_eq!(s.col_span_for_row(2, 7), Some(0..7));
    assert_eq!(s.col_span_for_row(3, 15), Some(0..15));
}

#[test]
fn multi_row_last_row_runs_zero_to_cursor() {
    let s = sel(SelectionSurface::Main, (1, 4), (3, 5), SelectionMode::Cell);
    assert_eq!(s.col_span_for_row(3, 20), Some(0..5));
}

#[test]
fn multi_row_backward_drag_normalizes_edges() {
    // Cursor is the EARLIER point; first/last must still derive from normalized.
    let s = sel(SelectionSurface::Main, (3, 5), (1, 4), SelectionMode::Cell);
    assert_eq!(s.col_span_for_row(1, 12), Some(4..12)); // first row
    assert_eq!(s.col_span_for_row(2, 8), Some(0..8)); // interior
    assert_eq!(s.col_span_for_row(3, 20), Some(0..5)); // last row
}

// ---- row mode -------------------------------------------------------------

#[test]
fn row_mode_selects_full_width_on_every_covered_row() {
    let s = sel(SelectionSurface::Main, (2, 5), (4, 1), SelectionMode::Row);
    assert_eq!(s.col_span_for_row(2, 9), Some(0..9));
    assert_eq!(s.col_span_for_row(3, 13), Some(0..13));
    assert_eq!(s.col_span_for_row(4, 4), Some(0..4));
}

// ---- word_bounds ----------------------------------------------------------

#[test]
fn word_bounds_snaps_to_surrounding_word() {
    let text = "hello bright world";
    // col 8 is inside "bright" (chars 6..12).
    assert_eq!(word_bounds(text, 8), 6..12);
}

#[test]
fn word_bounds_at_word_start_and_end() {
    let text = "alpha beta";
    assert_eq!(word_bounds(text, 0), 0..5);
    assert_eq!(word_bounds(text, 4), 0..5);
    assert_eq!(word_bounds(text, 6), 6..10);
}

#[test]
fn word_bounds_on_whitespace_snaps_to_whitespace_run() {
    let text = "a   b";
    // cols 1..4 are the space run.
    assert_eq!(word_bounds(text, 2), 1..4);
}

#[test]
fn word_bounds_past_end_clamps_to_last_word() {
    let text = "one two";
    assert_eq!(word_bounds(text, 99), 4..7);
}

#[test]
fn word_bounds_empty_line() {
    assert_eq!(word_bounds("", 0), 0..0);
}

// ---- extend helpers -------------------------------------------------------

#[test]
fn extend_by_row_moves_cursor_and_switches_to_row_mode() {
    let mut s = sel(SelectionSurface::Main, (2, 3), (2, 3), SelectionMode::Cell);
    extend_by_row(&mut s, 2, 10);
    assert_eq!(s.cursor.row, 4);
    assert_eq!(s.anchor.row, 2, "anchor stays fixed");
    assert_eq!(s.mode, SelectionMode::Row);
}

#[test]
fn extend_by_row_clamps_to_bounds() {
    let mut s = sel(SelectionSurface::Main, (1, 0), (1, 0), SelectionMode::Cell);
    extend_by_row(&mut s, -5, 10);
    assert_eq!(s.cursor.row, 0);
    extend_by_row(&mut s, 100, 10);
    assert_eq!(s.cursor.row, 9);
}

#[test]
fn extend_by_row_noop_on_empty_surface() {
    let mut s = sel(SelectionSurface::Main, (0, 0), (0, 0), SelectionMode::Cell);
    extend_by_row(&mut s, 3, 0);
    assert_eq!(s.cursor.row, 0);
    // unchanged surface => mode untouched.
    assert_eq!(s.mode, SelectionMode::Cell);
}

#[test]
fn extend_by_page_up_and_down() {
    let mut s = sel(
        SelectionSurface::Main,
        (10, 0),
        (10, 0),
        SelectionMode::Cell,
    );
    extend_by_page(&mut s, 4, true, 20);
    assert_eq!(s.cursor.row, 6);
    extend_by_page(&mut s, 4, false, 20);
    assert_eq!(s.cursor.row, 10);
}

// ---- display-width mapping (wide / CJK) -----------------------------------

#[test]
fn display_width_counts_wide_glyphs_as_two() {
    // "你好" = two double-width glyphs.
    assert_eq!(display_width_of_chars("你好", 2), 4);
    assert_eq!(display_width_of_chars("你好", 1), 2);
    assert_eq!(display_width_of_chars("ab你", 3), 4);
}

#[test]
fn char_offset_for_display_col_respects_wide_glyphs() {
    // "你好" — display cols 0,1 -> char 0; cols 2,3 -> char 1; >=4 -> end.
    assert_eq!(char_offset_for_display_col("你好", 0), 0);
    assert_eq!(char_offset_for_display_col("你好", 1), 0);
    assert_eq!(char_offset_for_display_col("你好", 2), 1);
    assert_eq!(char_offset_for_display_col("你好", 3), 1);
    assert_eq!(char_offset_for_display_col("你好", 4), 2);
    assert_eq!(char_offset_for_display_col("你好", 99), 2);
}

#[test]
fn char_offset_for_display_col_mixed_ascii_and_wide() {
    // "a你b": a(0) 你(1,2) b(3) in display cells.
    let text = "a你b";
    assert_eq!(char_offset_for_display_col(text, 0), 0); // a
    assert_eq!(char_offset_for_display_col(text, 1), 1); // first cell of 你
    assert_eq!(char_offset_for_display_col(text, 2), 1); // second cell of 你
    assert_eq!(char_offset_for_display_col(text, 3), 2); // b
}

#[test]
fn char_offset_for_display_col_skips_combining_marks() {
    // "e" + combining acute + "x": the combining mark (U+0301, zero-width) sits
    // ON the `e`'s cell, occupying NO cell of its own. Display cell 0 is `e`
    // (char 0), cell 1 is `x` (char 2). A naive `w.max(1)` gave the mark a
    // phantom cell, resolving col 1 to the mark (char 1) — the off-by-one.
    let text = "e\u{0301}x";
    assert_eq!(char_offset_for_display_col(text, 0), 0); // base `e`
    assert_eq!(char_offset_for_display_col(text, 1), 2); // `x`, NOT the mark
    assert_eq!(char_offset_for_display_col(text, 2), 3); // past end → char count
}

#[test]
fn char_offset_for_display_col_skips_zwj_joiners() {
    // A ZWJ family emoji "👨\u{200d}👩": two wide scalars (2 cells each) joined
    // by a zero-width U+200D. Chars: 👨(0) ZWJ(1) 👩(2), trailing "!"(3).
    // Display cells: 👨 -> 0,1 ; 👩 -> 2,3 ; ! -> 4. The ZWJ must consume no
    // cell, so cells 2,3 resolve to the second emoji (char 2), never the joiner.
    let text = "👨\u{200d}👩!";
    assert_eq!(char_offset_for_display_col(text, 0), 0); // first cell of 👨
    assert_eq!(char_offset_for_display_col(text, 1), 0); // second cell of 👨
    assert_eq!(char_offset_for_display_col(text, 2), 2); // first cell of 👩 (skips ZWJ at char 1)
    assert_eq!(char_offset_for_display_col(text, 3), 2); // second cell of 👩
    assert_eq!(char_offset_for_display_col(text, 4), 3); // the "!"
}

#[test]
fn char_offset_for_display_col_leading_combining_mark() {
    // A pathological leading zero-width char must not steal cell 0 from the
    // first real glyph.
    let text = "\u{0301}ab";
    assert_eq!(char_offset_for_display_col(text, 0), 1); // `a`, not the mark
    assert_eq!(char_offset_for_display_col(text, 1), 2); // `b`
}

#[test]
fn col_span_on_wide_row_selects_by_char_offset() {
    // Selection columns are char offsets; a wide row of 3 chars selects 1..3.
    let s = sel(
        SelectionSurface::Overlay,
        (0, 1),
        (0, 3),
        SelectionMode::Cell,
    );
    // "你好世" has 3 chars (display width 6) — col math is in CHARS.
    let text = "你好世";
    let span = s.col_span_for_row(0, 3).unwrap();
    assert_eq!(span, 1..3);
    // And the cleaned text slice over those char offsets picks the wide glyphs.
    let rows = vec![line(text)];
    assert_eq!(selection_clean_text(&rows, &s), "好世");

    // Display-cell alignment: the char-offset span maps onto the right terminal
    // CELLS (each wide glyph is two cells), so the highlight the renderer paints
    // lands exactly on cols 2..6 — not just the right chars.
    assert_eq!(display_width_of_chars(text, span.start), 2); // skip 你 (2 cells)
    assert_eq!(display_width_of_chars(text, span.end), 6); // through 世 (6 cells)
    // The selected slice itself is 4 cells wide (好 + 世).
    let selected: String = text.chars().take(span.end).skip(span.start).collect();
    assert_eq!(display_width_of_chars(&selected, span.end - span.start), 4);
    // And a display-col round-trip resolves the span's cell edges back to the
    // same char offsets the highlight/copy use.
    assert_eq!(char_offset_for_display_col(text, 2), span.start);
    assert_eq!(char_offset_for_display_col(text, 6), span.end);
}

// ---- highlight restyle ----------------------------------------------------

#[test]
fn highlight_marks_only_selected_cells_single_row() {
    let rows = vec![line("hello world")];
    let s = sel(SelectionSurface::Main, (0, 6), (0, 11), SelectionMode::Cell);
    let out = rows_with_selection_highlight(&rows, &s, hl());
    assert_eq!(highlighted_text(&out[0]), "world");
    // Plain text round-trips unchanged.
    assert_eq!(plain(&out[0]), "hello world");
}

#[test]
fn highlight_preserves_surrounding_span_styles() {
    let red = Style::default().fg(Color::Red);
    let blue = Style::default().fg(Color::Blue);
    let rows = vec![styled_line(&[("red ", red), ("blue", blue)])];
    // "red blue": r0 e1 d2 ' '3 b4 l5 u6 e7. Select chars 1..6 == "ed bl",
    // which spans the "red "/"blue" boundary.
    let s = sel(SelectionSurface::Main, (0, 1), (0, 6), SelectionMode::Cell);
    let out = rows_with_selection_highlight(&rows, &s, hl());
    assert_eq!(highlighted_text(&out[0]), "ed bl");
    assert_eq!(plain(&out[0]), "red blue");
    // Untouched head keeps its red fg, untouched tail keeps blue.
    let head = &out[0].spans[0];
    assert_eq!(head.content.as_ref(), "r");
    assert_eq!(head.style.fg, Some(Color::Red));
    let tail = out[0].spans.last().unwrap();
    assert_eq!(tail.content.as_ref(), "ue");
    assert_eq!(tail.style.fg, Some(Color::Blue));
    // Highlighted middle keeps its underlying fg AND gains REVERSED.
    let mid_blue = out[0]
        .spans
        .iter()
        .find(|sp| sp.content.as_ref() == "bl")
        .unwrap();
    assert_eq!(mid_blue.style.fg, Some(Color::Blue));
    assert!(mid_blue.style.add_modifier.contains(Modifier::REVERSED));
}

#[test]
fn highlight_spans_multiple_rows_first_middle_last() {
    let rows = vec![line("first line"), line("middle"), line("last line")];
    let s = sel(SelectionSurface::Main, (0, 6), (2, 4), SelectionMode::Cell);
    let out = rows_with_selection_highlight(&rows, &s, hl());
    assert_eq!(highlighted_text(&out[0]), "line"); // first: col 6..end
    assert_eq!(highlighted_text(&out[1]), "middle"); // interior: full
    assert_eq!(highlighted_text(&out[2]), "last"); // last: 0..4
}

#[test]
fn highlight_leaves_rows_outside_span_untouched() {
    let rows = vec![line("zero"), line("one"), line("two")];
    let s = sel(SelectionSurface::Main, (1, 0), (1, 3), SelectionMode::Cell);
    let out = rows_with_selection_highlight(&rows, &s, hl());
    assert_eq!(highlighted_text(&out[0]), "");
    assert_eq!(highlighted_text(&out[2]), "");
    assert_eq!(highlighted_text(&out[1]), "one");
}

#[test]
fn highlight_empty_selection_marks_nothing() {
    let rows = vec![line("untouched")];
    let s = sel(SelectionSurface::Main, (0, 3), (0, 3), SelectionMode::Cell);
    let out = rows_with_selection_highlight(&rows, &s, hl());
    assert_eq!(highlighted_text(&out[0]), "");
    assert_eq!(plain(&out[0]), "untouched");
}

// ---- clean-text extraction ------------------------------------------------

#[test]
fn clean_text_single_row_partial() {
    let rows = vec![line("hello world")];
    let s = sel(SelectionSurface::Main, (0, 6), (0, 11), SelectionMode::Cell);
    assert_eq!(selection_clean_text(&rows, &s), "world");
}

#[test]
fn clean_text_multi_row_joins_with_newline() {
    let rows = vec![line("first line"), line("middle"), line("last line")];
    let s = sel(SelectionSurface::Main, (0, 6), (2, 4), SelectionMode::Cell);
    assert_eq!(selection_clean_text(&rows, &s), "line\nmiddle\nlast");
}

#[test]
fn clean_text_strips_rail_gutter_on_full_row() {
    // A rail header line: "│ ├─● answer text" — the gutter + marker is chrome.
    let rows = vec![line("│ ├─● answer text")];
    let len = plain(&rows[0]).chars().count();
    let s = sel(
        SelectionSurface::Main,
        (0, 0),
        (0, len),
        SelectionMode::Cell,
    );
    // strip_gutter drops the rail run and the role marker+space.
    assert_eq!(selection_clean_text(&rows, &s), "answer text");
}

#[test]
fn clean_text_selection_starting_inside_gutter_pastes_from_first_content_char() {
    // Selection begins at col 1 (inside the rail). After cleaning, the paste
    // must still start at the first content char, not mid-gutter.
    let rows = vec![line("│ ├─● answer text")];
    let s = sel(SelectionSurface::Main, (0, 1), (0, 17), SelectionMode::Cell);
    assert_eq!(selection_clean_text(&rows, &s), "answer text");
}

#[test]
fn clean_text_partial_within_content_after_gutter() {
    // Content "answer text" begins after a 6-char gutter ("│ ├─● ").
    // Select chars 6..12 of the FULL line == "answer".
    let full = "│ ├─● answer text";
    let rows = vec![line(full)];
    // Sanity: assert the gutter width assumption.
    assert_eq!(strip_gutter(full), "answer text");
    let gutter = full.chars().count() - strip_gutter(full).chars().count();
    let s = sel(
        SelectionSurface::Main,
        (0, gutter),
        (0, gutter + 6),
        SelectionMode::Cell,
    );
    assert_eq!(selection_clean_text(&rows, &s), "answer");
}

#[test]
fn clean_text_trims_trailing_blank_rows() {
    let rows = vec![line("content"), line(""), line("")];
    let s = sel(SelectionSurface::Main, (0, 0), (2, 0), SelectionMode::Row);
    // Trailing blanks dropped like copy::format_plain.
    assert_eq!(selection_clean_text(&rows, &s), "content");
}

#[test]
fn clean_text_empty_selection_is_empty() {
    let rows = vec![line("anything")];
    let s = sel(SelectionSurface::Main, (0, 4), (0, 4), SelectionMode::Cell);
    assert_eq!(selection_clean_text(&rows, &s), "");
}

#[test]
fn clean_text_row_mode_takes_whole_rows() {
    let rows = vec![line("│ ├─● alpha"), line("│ beta")];
    let s = sel(SelectionSurface::Main, (0, 3), (1, 1), SelectionMode::Row);
    assert_eq!(selection_clean_text(&rows, &s), "alpha\nbeta");
}
