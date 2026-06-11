//! Unit tests for the multi-cursor disjoint selection set (§12.1.6).
//!
//! Included into [`crate::multi_selection`] via `#[path]` per the repo test
//! layout. Covers add/remove/merge/order/clear, the combined flatten over the
//! live range, and the combined clean-text join over painted rows.

use super::*;
use crate::selection::{Selection, SelectionMode};
use ratatui::text::Line;

// ---- builders -------------------------------------------------------------

fn line(s: &str) -> Line<'static> {
    Line::from(s.to_string())
}

/// A single-row cell selection on the main surface over `row`, chars `[lo, hi)`.
fn cell(row: usize, lo: usize, hi: usize) -> Selection {
    let mut sel = Selection::at(
        SelectionSurface::Main,
        Pos::new(row, lo),
        SelectionMode::Cell,
        80,
    );
    sel.cursor = Pos::new(row, hi);
    sel
}

/// A whole-row selection on the main surface over `row`, chars `[0, len)`.
fn whole_row(row: usize, len: usize) -> Selection {
    let mut sel = Selection::at(
        SelectionSurface::Main,
        Pos::new(row, 0),
        SelectionMode::Row,
        80,
    );
    sel.cursor = Pos::new(row, len);
    sel
}

/// A multi-row whole-row (`Row`-mode) selection spanning rows `[top, bottom]`
/// edge to edge, on the main surface.
fn multi_row(top: usize, bottom: usize) -> Selection {
    let mut sel = Selection::at(
        SelectionSurface::Main,
        Pos::new(top, 0),
        SelectionMode::Row,
        80,
    );
    sel.cursor = Pos::new(bottom, 0);
    sel
}

// ---- add / order ----------------------------------------------------------

#[test]
fn empty_set_holds_nothing() {
    let set = SelectionSet::new();
    assert!(set.is_empty());
    assert_eq!(set.len(), 0);
    assert!(set.members().is_empty());
}

#[test]
fn add_rejects_an_empty_collapsed_selection() {
    let mut set = SelectionSet::new();
    // A collapsed cell selection (anchor == cursor) is empty: not a real range.
    let collapsed = cell(0, 3, 3);
    assert!(!set.add(collapsed), "an empty selection must not be added");
    assert!(set.is_empty());
}

#[test]
fn add_keeps_members_in_reading_order() {
    let mut set = SelectionSet::new();
    // Insert out of order; the set sorts top-to-bottom.
    assert!(set.add(cell(5, 0, 4)));
    assert!(set.add(cell(1, 0, 4)));
    assert!(set.add(cell(3, 0, 4)));
    let rows: Vec<usize> = set.members().iter().map(|m| m.normalized().0.row).collect();
    assert_eq!(rows, vec![1, 3, 5], "members order by start row");
}

#[test]
fn add_ignores_an_exact_duplicate() {
    let mut set = SelectionSet::new();
    assert!(set.add(cell(2, 1, 5)));
    assert!(
        !set.add(cell(2, 1, 5)),
        "re-adding the same range is a no-op"
    );
    assert_eq!(set.len(), 1);
}

// ---- merge ----------------------------------------------------------------

#[test]
fn add_merges_overlapping_single_row_ranges() {
    let mut set = SelectionSet::new();
    assert!(set.add(cell(0, 2, 6)));
    // Overlaps [2,6): merges into one [2,9).
    assert!(set.add(cell(0, 4, 9)));
    assert_eq!(set.len(), 1, "overlapping ranges on one row merge");
    let (start, end) = set.members()[0].normalized();
    assert_eq!((start.col, end.col), (2, 9));
}

#[test]
fn add_merges_touching_single_row_ranges() {
    let mut set = SelectionSet::new();
    assert!(set.add(cell(0, 0, 4)));
    // Abuts [0,4) exactly at col 4: touching ranges collapse to [0,8).
    assert!(set.add(cell(0, 4, 8)));
    assert_eq!(set.len(), 1, "touching ranges on one row merge");
    let (start, end) = set.members()[0].normalized();
    assert_eq!((start.col, end.col), (0, 8));
}

#[test]
fn add_does_not_merge_disjoint_single_row_ranges() {
    let mut set = SelectionSet::new();
    assert!(set.add(cell(0, 0, 3)));
    // Gap between col 3 and col 5: stays as two members.
    assert!(set.add(cell(0, 5, 8)));
    assert_eq!(set.len(), 2, "a gap keeps the ranges disjoint");
}

#[test]
fn add_does_not_merge_across_rows() {
    let mut set = SelectionSet::new();
    assert!(set.add(cell(0, 0, 4)));
    assert!(set.add(cell(1, 0, 4)));
    assert_eq!(set.len(), 2, "ranges on different rows never merge");
}

#[test]
fn multi_row_members_order_but_never_merge() {
    // A multi-row (Row-mode spanning two rows) member preserves its block
    // boundary even if a single-row member abuts it.
    let mut multi = whole_row(0, 6);
    multi.cursor = Pos::new(1, 6);
    let mut set = SelectionSet::new();
    assert!(set.add(multi));
    assert!(set.add(cell(1, 0, 3)));
    assert_eq!(set.len(), 2, "multi-row members order but do not merge");
}

// ---- remove ---------------------------------------------------------------

#[test]
fn remove_at_drops_the_range_under_a_point() {
    let mut set = SelectionSet::new();
    assert!(set.add(cell(0, 2, 6)));
    assert!(set.add(cell(2, 0, 4)));
    // A point inside the first range removes it.
    assert!(set.remove_at(SelectionSurface::Main, Pos::new(0, 4)));
    assert_eq!(set.len(), 1);
    assert_eq!(set.members()[0].normalized().0.row, 2);
}

#[test]
fn remove_at_outside_any_range_is_a_noop() {
    let mut set = SelectionSet::new();
    assert!(set.add(cell(0, 2, 6)));
    assert!(
        !set.remove_at(SelectionSurface::Main, Pos::new(0, 9)),
        "a point outside every range removes nothing"
    );
    assert_eq!(set.len(), 1);
}

#[test]
fn remove_at_hits_row_mode_left_of_anchor_col() {
    // A `Row`-mode member painted/copied edge to edge whose raw anchor keeps a
    // mid-row column (as `extend_by_row` leaves it). A toggle-off click left of
    // that anchor column, but inside the visible edge-to-edge highlight, must
    // still match the member and remove it.
    let mut sel = Selection::at(
        SelectionSurface::Main,
        Pos::new(0, 4),
        SelectionMode::Row,
        80,
    );
    sel.cursor = Pos::new(2, 0);

    let mut set = SelectionSet::new();
    assert!(set.add(sel));
    assert!(
        set.remove_at(SelectionSurface::Main, Pos::new(0, 1)),
        "a click left of the anchor column is still inside the edge-to-edge Row highlight"
    );
    assert!(set.is_empty());
}

#[test]
fn clear_drops_every_member() {
    let mut set = SelectionSet::new();
    assert!(set.add(cell(0, 0, 4)));
    assert!(set.add(cell(2, 0, 4)));
    set.clear();
    assert!(set.is_empty());
}

// ---- combined ranges ------------------------------------------------------

#[test]
fn combined_ranges_folds_in_the_live_selection() {
    let mut set = SelectionSet::new();
    assert!(set.add(cell(0, 0, 4)));
    let live = cell(2, 0, 4);
    let combined = combined_ranges(&set, Some(&live));
    assert_eq!(combined.len(), 2, "the live range joins the committed set");
    let rows: Vec<usize> = combined.iter().map(|m| m.normalized().0.row).collect();
    assert_eq!(rows, vec![0, 2]);
}

#[test]
fn combined_ranges_skips_an_empty_live_selection() {
    let mut set = SelectionSet::new();
    assert!(set.add(cell(0, 0, 4)));
    let empty_live = cell(2, 3, 3); // collapsed: empty
    let combined = combined_ranges(&set, Some(&empty_live));
    assert_eq!(combined.len(), 1, "an empty live range is ignored");
}

#[test]
fn combined_ranges_with_no_set_and_no_live_is_empty() {
    let set = SelectionSet::new();
    assert!(combined_ranges(&set, None).is_empty());
}

#[test]
fn combined_ranges_merges_a_live_range_that_overlaps_a_member() {
    let mut set = SelectionSet::new();
    assert!(set.add(cell(0, 0, 4)));
    // The live range overlaps the committed one on the same row.
    let live = cell(0, 2, 8);
    let combined = combined_ranges(&set, Some(&live));
    assert_eq!(combined.len(), 1, "an overlapping live range merges in");
    let (start, end) = combined[0].normalized();
    assert_eq!((start.col, end.col), (0, 8));
}

// ---- combined clean text --------------------------------------------------

#[test]
fn combined_clean_text_joins_disjoint_ranges_with_a_blank_line() {
    let rows = vec![line("alpha line"), line("beta line"), line("gamma line")];
    // Two disjoint whole-row selections (rows 0 and 2).
    let ranges = vec![whole_row(0, 10), whole_row(2, 10)];
    let text = combined_clean_text(&rows, &ranges);
    assert_eq!(
        text, "alpha line\n\ngamma line",
        "disjoint blocks join with a blank line, skipping the middle row"
    );
}

#[test]
fn combined_clean_text_drops_empty_range_slices() {
    let rows = vec![line("alpha"), line("")];
    // The second range covers an empty row → contributes nothing.
    let ranges = vec![whole_row(0, 5), whole_row(1, 0)];
    let text = combined_clean_text(&rows, &ranges);
    assert_eq!(
        text, "alpha",
        "an empty range slice is dropped from the join"
    );
}

#[test]
fn combined_clean_text_of_no_ranges_is_empty() {
    let rows = vec![line("alpha")];
    assert!(combined_clean_text(&rows, &[]).is_empty());
}

#[test]
fn combined_clean_text_dedupes_rows_shared_by_overlapping_multi_row_ranges() {
    // Regression for deep-review #48: a committed multi-row Row-mode selection
    // over rows 3-7 and a live range over rows 5-9 overlap on rows 5-7. Multi-row
    // members order but never merge, so both survive `combined_ranges`; without
    // cell-level de-duplication `combined_clean_text` would emit rows 5-7 twice.
    let rows = vec![
        line("r0"),
        line("r1"),
        line("r2"),
        line("row3"),
        line("row4"),
        line("row5"),
        line("row6"),
        line("row7"),
        line("row8"),
        line("row9"),
    ];
    let mut set = SelectionSet::new();
    assert!(set.add(multi_row(3, 7)));
    let live = multi_row(5, 9);

    let ranges = combined_ranges(&set, Some(&live));
    assert_eq!(
        ranges.len(),
        2,
        "overlapping multi-row members order but do not merge"
    );

    let text = combined_clean_text(&rows, &ranges);
    // Every shared row appears exactly once.
    for shared in ["row5", "row6", "row7"] {
        assert_eq!(
            text.matches(shared).count(),
            1,
            "a row shared by two overlapping multi-row ranges must appear once, not twice: {text:?}"
        );
    }
    // The first block carries rows 3-7; the second carries ONLY the rows the
    // first did not (8-9), joined as a distinct block.
    assert_eq!(
        text, "row3\nrow4\nrow5\nrow6\nrow7\n\nrow8\nrow9",
        "shared rows stay with the first block; the later range keeps only its unclaimed tail"
    );
}
