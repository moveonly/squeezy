//! Unit tests for the incremental search model.
//!
//! Included into [`crate::search`] via `#[path]` per the repo test layout.

use super::*;
use crate::selection::SelectionSurface;
use ratatui::text::{Line, Span};

// ---- builders -------------------------------------------------------------

fn line(s: &str) -> Line<'static> {
    Line::from(s.to_string())
}

/// A line built from several styled spans so the find pass is exercised against
/// multi-span lines (it projects to plain text, so spans must not matter).
fn styled_line(parts: &[&str]) -> Line<'static> {
    Line::from(
        parts
            .iter()
            .map(|t| Span::raw(t.to_string()))
            .collect::<Vec<_>>(),
    )
}

fn rows(strs: &[&str]) -> Vec<Line<'static>> {
    strs.iter().map(|s| line(s)).collect()
}

fn state(query: &str, matches: Vec<Match>, current: Option<usize>) -> SearchState {
    SearchState {
        query: query.to_string(),
        surface: SelectionSurface::Main,
        width: 80,
        matches,
        current,
        include_tool_output: true,
        include_reasoning: true,
    }
}

fn m(row: usize, col: Range<usize>) -> Match {
    Match { row, col }
}

// ---- find: basic ----------------------------------------------------------

#[test]
fn empty_query_finds_nothing() {
    let r = rows(&["hello world", "another line"]);
    assert!(find(&r, &[], "", true, true).is_empty());
}

#[test]
fn single_match_reports_char_range() {
    let r = rows(&["hello world"]);
    let found = find(&r, &[], "world", true, true);
    assert_eq!(found, vec![m(0, 6..11)]);
}

#[test]
fn find_across_multiple_rows_in_reading_order() {
    let r = rows(&["foo bar", "baz foo", "no match here", "foo again"]);
    let found = find(&r, &[], "foo", true, true);
    assert_eq!(found, vec![m(0, 0..3), m(1, 4..7), m(3, 0..3)]);
}

#[test]
fn multiple_matches_on_one_row_are_non_overlapping_and_ordered() {
    let r = rows(&["abababab"]);
    let found = find(&r, &[], "ab", true, true);
    assert_eq!(found, vec![m(0, 0..2), m(0, 2..4), m(0, 4..6), m(0, 6..8)]);
}

#[test]
fn overlapping_pattern_does_not_double_count() {
    // "aa" in "aaaa": non-overlapping scan yields 2, not 3.
    let r = rows(&["aaaa"]);
    let found = find(&r, &[], "aa", true, true);
    assert_eq!(found, vec![m(0, 0..2), m(0, 2..4)]);
}

#[test]
fn no_match_yields_empty() {
    let r = rows(&["alpha", "beta"]);
    assert!(find(&r, &[], "gamma", true, true).is_empty());
}

// ---- find: case behavior --------------------------------------------------

#[test]
fn search_is_case_insensitive() {
    let r = rows(&["Hello WORLD", "hELLo"]);
    let found = find(&r, &[], "hello", true, true);
    assert_eq!(found, vec![m(0, 0..5), m(1, 0..5)]);
}

#[test]
fn uppercase_query_matches_lowercase_content() {
    let r = rows(&["the quick brown fox"]);
    let found = find(&r, &[], "QUICK", true, true);
    assert_eq!(found, vec![m(0, 4..9)]);
}

// ---- find: non-1:1 Unicode lowercasing fallback ---------------------------
//
// `char::to_lowercase('İ')` (U+0130) expands to two chars ("i\u{307}"), which
// trips `find`'s offset-stability guard and routes the line through the
// `simple_lower` fallback instead of the flattened-lowercase fast path. These
// pin that the fallback (a) keeps char offsets 1:1 so the reported column is
// correct, and (b) STILL folds non-ASCII case — the previous ASCII-only
// downgrade would have silently failed (b).

#[test]
fn non_1to1_lowercasing_char_keeps_offsets_stable() {
    // "İstanbul café": the leading U+0130 forces the fallback path. "café"
    // begins at char index 9 (İ s t a n b u l ␠ c …), so the reported column
    // must be 9..13 — an offset desync would shift or drop the range.
    let r = rows(&["İstanbul café"]);
    let found = find(&r, &[], "café", true, true);
    assert_eq!(found, vec![m(0, 9..13)]);
}

#[test]
fn non_1to1_fallback_still_folds_non_ascii_case() {
    // Same fallback-triggering line, but an UPPERCASE non-ASCII query. The
    // fallback must fold "CAFÉ" -> "café" (Unicode case-insensitive), not
    // degrade to ASCII-only — which would have left "É" unmatched.
    let r = rows(&["İstanbul CAFÉ here"]);
    let found = find(&r, &[], "café", true, true);
    assert_eq!(found, vec![m(0, 9..13)]);
}

#[test]
fn non_1to1_lowercasing_char_is_itself_matchable() {
    // The expanding char folds to the first char of its lowercase expansion
    // ('İ' -> 'i'), so a plain "i" query finds it at its own column.
    let r = rows(&["İ and i"]);
    let found = find(&r, &[], "i", true, true);
    assert_eq!(found, vec![m(0, 0..1), m(0, 6..7)]);
}

#[test]
fn expanding_query_is_offset_stable_regardless_of_row_expansion() {
    // The fold decision is symmetric in needle and haystack. The query "İ"
    // (U+0130) expands 1:N under `to_lowercase`, so it must route through the
    // offset-stable `simple_lower` fold whether or not the row's own chars
    // expand. Row A's chars are all 1:1 lowercasing (only the query expands);
    // row B has a leading "İ" that also expands the haystack — the case that
    // previously flipped the path and rebuilt the needle differently. In both,
    // "İ" folds to a single "i" and matches at every "i"-folding char with an
    // offset-stable, single-char span.
    let only_query_expands = rows(&["abci\u{307}xyz"]);
    let row_also_expands = rows(&["İabci\u{307}xyz"]);
    let a = find(&only_query_expands, &[], "İ", true, true);
    let b = find(&row_also_expands, &[], "İ", true, true);
    // Row A: a b c i ̇ x y z — the bare "i" at char index 3 folds-matches.
    assert_eq!(a, vec![m(0, 3..4)]);
    // Row B: İ a b c i ̇ x y z — the folded "İ" at 0 and the bare "i" at 4.
    assert_eq!(b, vec![m(0, 0..1), m(0, 4..5)]);
}

#[test]
fn expanding_query_span_is_single_char() {
    // "İ" folds (via `simple_lower`) to exactly one "i", so its needle is one
    // char. It matches the "i" char only — never silently widening or dropping
    // part of an expansion into a stray multi-char span.
    let r = rows(&["xi\u{307}y"]);
    let found = find(&r, &[], "İ", true, true);
    assert_eq!(found, vec![m(0, 1..2)]);
}

// ---- find: multi-span projection ------------------------------------------

#[test]
fn match_spans_across_span_boundary() {
    // "hel" + "lo wor" + "ld" joins to "hello world"; "world" straddles spans.
    let r = vec![styled_line(&["hel", "lo wor", "ld"])];
    let found = find(&r, &[], "world", true, true);
    assert_eq!(found, vec![m(0, 6..11)]);
}

// ---- find: include/exclude toggles ----------------------------------------

#[test]
fn excluding_tool_output_skips_those_rows() {
    let r = rows(&["normal match", "tool match", "normal match"]);
    let kinds = [RowKind::Normal, RowKind::ToolOutput, RowKind::Normal];
    let found = find(&r, &kinds, "match", true, true);
    assert_eq!(found.len(), 3, "all rows searched when toggle on");

    let found = find(&r, &kinds, "match", false, true);
    assert_eq!(
        found,
        vec![m(0, 7..12), m(2, 7..12)],
        "tool-output row skipped when toggle off"
    );
}

#[test]
fn excluding_reasoning_skips_those_rows() {
    let r = rows(&["a think", "b think", "c think"]);
    let kinds = [RowKind::Normal, RowKind::Reasoning, RowKind::Normal];
    let found = find(&r, &kinds, "think", true, false);
    assert_eq!(found, vec![m(0, 2..7), m(2, 2..7)]);
}

#[test]
fn excluding_both_kinds_searches_only_normal() {
    let r = rows(&["x hit", "y hit", "z hit"]);
    let kinds = [RowKind::ToolOutput, RowKind::Reasoning, RowKind::Normal];
    let found = find(&r, &kinds, "hit", false, false);
    assert_eq!(found, vec![m(2, 2..5)]);
}

#[test]
fn short_kinds_slice_treats_missing_rows_as_normal() {
    let r = rows(&["one hit", "two hit"]);
    // Only one kind provided; row 1 falls back to Normal and is searched.
    let kinds = [RowKind::Normal];
    let found = find(&r, &kinds, "hit", false, false);
    assert_eq!(found, vec![m(0, 4..7), m(1, 4..7)]);
}

// ---- next / prev wraparound -----------------------------------------------

#[test]
fn next_advances_and_wraps() {
    let mut s = state("x", vec![m(0, 0..1), m(1, 0..1), m(2, 0..1)], Some(0));
    next(&mut s);
    assert_eq!(s.current, Some(1));
    next(&mut s);
    assert_eq!(s.current, Some(2));
    next(&mut s);
    assert_eq!(s.current, Some(0), "wraps past the end");
}

#[test]
fn prev_retreats_and_wraps() {
    let mut s = state("x", vec![m(0, 0..1), m(1, 0..1), m(2, 0..1)], Some(0));
    prev(&mut s);
    assert_eq!(s.current, Some(2), "wraps past the start");
    prev(&mut s);
    assert_eq!(s.current, Some(1));
}

#[test]
fn next_prev_are_noop_when_empty() {
    let mut s = state("x", vec![], None);
    next(&mut s);
    assert_eq!(s.current, None);
    prev(&mut s);
    assert_eq!(s.current, None);
}

#[test]
fn next_from_none_starts_at_zero() {
    let mut s = state("x", vec![m(0, 0..1), m(1, 0..1)], None);
    next(&mut s);
    assert_eq!(s.current, Some(0));
}

// ---- current_match / match_ranges_by_row ----------------------------------

#[test]
fn current_match_returns_active() {
    let s = state("x", vec![m(0, 0..1), m(5, 2..3)], Some(1));
    assert_eq!(current_match(&s), Some(&m(5, 2..3)));
}

#[test]
fn current_match_none_when_empty() {
    let s = state("x", vec![], None);
    assert_eq!(current_match(&s), None);
}

#[test]
fn match_ranges_flags_current() {
    let s = state("x", vec![m(0, 0..1), m(1, 0..1), m(2, 0..1)], Some(1));
    let ranges: Vec<(usize, Range<usize>, bool)> = match_ranges_by_row(&s).collect();
    assert_eq!(
        ranges,
        vec![(0, 0..1, false), (1, 0..1, true), (2, 0..1, false)]
    );
}

// ---- rebuild --------------------------------------------------------------

#[test]
fn rebuild_sets_current_to_zero_from_empty() {
    let mut s = SearchState::open(SelectionSurface::Main, 80);
    s.query = "foo".to_string();
    let r = rows(&["foo", "bar foo"]);
    rebuild(&mut s, &r, &[], 80);
    assert_eq!(s.matches, vec![m(0, 0..3), m(1, 4..7)]);
    assert_eq!(s.current, Some(0));
}

#[test]
fn rebuild_preserves_nearest_match() {
    // Start with current on the second match (row 1), then rebuild after the
    // query grows; current should snap to the nearest surviving match.
    let r = rows(&["foo a", "foo b", "foo c"]);
    let mut s = state("foo", find(&r, &[], "foo", true, true), Some(1));
    assert_eq!(s.current, Some(1));
    // Query unchanged but rebuilt: stays anchored at the same (row, col).
    rebuild(&mut s, &r, &[], 80);
    assert_eq!(
        current_match(&s),
        Some(&m(1, 0..3)),
        "current stays on the same row after rebuild"
    );
}

#[test]
fn rebuild_clears_current_when_no_matches() {
    let r = rows(&["alpha", "beta"]);
    let mut s = state("alpha", find(&r, &[], "alpha", true, true), Some(0));
    s.query = "zzz".to_string();
    rebuild(&mut s, &r, &[], 80);
    assert!(s.matches.is_empty());
    assert_eq!(s.current, None);
}

#[test]
fn rebuild_anchors_forward_when_previous_match_disappears() {
    // current was on row 2; after a rebuild where row 2 no longer matches, the
    // nearest surviving match at or after the old position is chosen.
    let r = rows(&["hit here", "hit there", "hit last"]);
    let mut s = state("hit", find(&r, &[], "hit", true, true), Some(2));
    // Narrow the query so only rows 0 and 1 still match.
    s.query = "hit t".to_string();
    rebuild(&mut s, &r, &[], 80);
    assert_eq!(s.matches, vec![m(1, 0..5)]);
    assert_eq!(s.current, Some(0));
}

#[test]
fn rebuild_reanchors_matches_after_width_change() {
    // Models a resize while search is open: the same logical text reflows to a
    // different set of painted rows at the new width, and `rebuild` must produce
    // (row,col) positions against the *new* rows — never keep the stale ones —
    // and record the new width on the state so it is no longer write-only.
    //
    // Narrow width: "needle" wraps onto its own row (row 1).
    let narrow = rows(&["alpha beta", "needle here"]);
    let mut s = SearchState::open(SelectionSurface::Main, 12);
    s.query = "needle".to_string();
    rebuild(&mut s, &narrow, &[], 12);
    assert_eq!(
        s.matches,
        vec![m(1, 0..6)],
        "match found on the wrapped row"
    );
    assert_eq!(s.width, 12, "width recorded at narrow rebuild");

    // Wider width: the text reflows so "needle" now sits on row 0 at a new col.
    // A stale match set would still claim (row 1, 0..6); the rebuild must drop
    // it and re-anchor to the reflowed geometry.
    let wide = rows(&["alpha beta needle here"]);
    rebuild(&mut s, &wide, &[], 24);
    assert_eq!(
        s.matches,
        vec![m(0, 11..17)],
        "matches re-anchor to the reflowed rows, no stale (row,col)"
    );
    assert_eq!(s.width, 24, "width updated to the new painted width");
    assert_eq!(s.current, Some(0));
}

// ---- push / pop -----------------------------------------------------------

#[test]
fn push_and_pop_edit_query() {
    let mut s = SearchState::open(SelectionSurface::Main, 80);
    push_char(&mut s, 'a');
    push_char(&mut s, 'b');
    assert_eq!(s.query, "ab");
    pop_char(&mut s);
    assert_eq!(s.query, "a");
    pop_char(&mut s);
    pop_char(&mut s);
    assert_eq!(s.query, "", "pop on empty is a no-op");
}

// ---- gutter shift ---------------------------------------------------------

#[test]
fn match_column_is_in_full_line_space_after_gutter_strip() {
    // A rail-gutter prefix ("│ ") is stripped before scanning, but the reported
    // column must index the FULL painted line so the highlight lands on the
    // drawn cells.
    let r = rows(&["│ hello world"]);
    let found = find(&r, &[], "world", true, true);
    // "world" begins at char offset 8 in "│ hello world".
    assert_eq!(found, vec![m(0, 8..13)]);
    // Sanity: that range slices "world" out of the full line.
    let full: String = "│ hello world".chars().collect();
    let slice: String = full
        .chars()
        .skip(found[0].col.start)
        .take(found[0].col.len())
        .collect();
    assert_eq!(slice, "world");
}

// ---- case-fold expansion + wide glyph offset stability ---------------------

#[test]
fn match_col_is_stable_past_1_to_n_lowercase_expansion() {
    // `İ` (U+0130) lowercases to TWO chars ("i" + combining dot above), so the
    // line's straightforward char-by-char lowering desyncs char offsets and
    // `find` falls back to the offset-preserving `simple_lower` fold. The reported
    // `col` must still index the correct chars in the FULL line — i.e. the İ
    // counts as exactly one column despite the 1:N expansion.
    let r = rows(&["İstanbul guide"]);
    let found = find(&r, &[], "guide", true, true);
    // "guide" begins at char offset 9 in "İstanbul guide" (İ s t a n b u l ' ').
    assert_eq!(found, vec![m(0, 9..14)]);
    // Sanity: that range slices "guide" out of the full line, proving the offset
    // survived the İ→i+combining expansion.
    let full: String = "İstanbul guide".chars().collect();
    let slice: String = full
        .chars()
        .skip(found[0].col.start)
        .take(found[0].col.len())
        .collect();
    assert_eq!(slice, "guide");
}

#[test]
fn match_col_counts_wide_cjk_glyphs_as_single_columns() {
    // The reported `col` is a CHAR offset (consumed by char-slicing in selection),
    // NOT a byte offset and NOT a display-cell offset. With two wide CJK glyphs
    // before the match, a byte-offset regression would report 7..12 and a
    // display-cell (2-cells-per-CJK) regression would report 5..10; the correct
    // char offset is 3..8.
    let r = rows(&["日本 world"]);
    let found = find(&r, &[], "world", true, true);
    assert_eq!(found, vec![m(0, 3..8)]);
    let full: String = "日本 world".chars().collect();
    let slice: String = full
        .chars()
        .skip(found[0].col.start)
        .take(found[0].col.len())
        .collect();
    assert_eq!(slice, "world");
}
