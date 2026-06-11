//! Multi-Cursor-Like Transcript SELECTION (§12.1.6).
//!
//! The base [`crate::selection`] module owns a *single* anchor/cursor range over
//! the painted transcript rows. This module layers a [`SelectionSet`] on top: a
//! list of additional, **disjoint** committed ranges so the user can build up
//! several non-contiguous selections (different rows, different words) and
//! copy/act on all of them at once — the "multi-cursor" affordance the spec asks
//! for ("select multiple non-contiguous entries, blocks, or ranges for
//! copy/export/quote").
//!
//! ## Division of labour with `app.selection`
//!
//! The crate root still keeps the LIVE, in-flight range in `app.selection`
//! (the one a drag/extend is currently editing). The set here holds only the
//! *committed* extra ranges. A keyboard chord (add-selection) or a modifier-click
//! "commits" the live range into the set with [`SelectionSet::add`] and clears the
//! live one so the next gesture starts a fresh disjoint range. Copy/quote then
//! flatten the set **plus** the live range together via
//! [`combined_ranges`].
//!
//! ## Why reuse `selection::Selection`
//!
//! Each member is a full [`selection::Selection`], so all of the existing
//! per-row column-span math, highlight restyle
//! ([`selection::rows_with_selection_highlight`]), and clean-text extraction
//! ([`selection::selection_clean_text`]) apply unchanged. The set never invents a
//! new geometry; it is purely a container with add/remove/normalize/order/clear
//! semantics over the established model. That keeps the highlight the user sees
//! byte-for-byte aligned with the text a combined copy produces.
//!
//! ## Normalization
//!
//! Members are kept in reading order (top row first, then column) and **merged**
//! when their normalized spans touch or overlap on the same single row, so adding
//! a range that abuts an existing one collapses to one — exactly the
//! "normalize overlapping selections while preserving block boundaries" the spec
//! requires. Multi-row (`Row`-mode) members are ordered but not merged across
//! rows, so block boundaries survive. Two multi-row members can therefore still
//! overlap on the rows they share; [`combined_clean_text`] de-duplicates at the
//! `(row, cleaned-col)` cell level so a shared row's text is emitted exactly once
//! in a combined copy even though both members keep their own boundary.
//!
//! Selection is MAIN-view only (mirroring [`crate::selection`]), and the whole
//! module is dead in a plain non-Unix build path until the dispatch wires it up,
//! so a module-level dead-code allowance keeps the Windows `-D warnings` clippy
//! gate green without sprinkling per-item allows.
#![cfg_attr(not(unix), allow(dead_code))]

use crate::selection::{self, Pos, Selection, SelectionSurface};

/// An ordered list of committed, disjoint transcript selections (§12.1.6).
///
/// Empty by default, so a session that never uses multi-select pays nothing and
/// renders/copies exactly as the single-selection build did. All members share
/// one [`SelectionSurface`] (the main view); a member on a different surface is
/// rejected by [`add`](Self::add) to keep the combined copy coherent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SelectionSet {
    members: Vec<Selection>,
}

impl SelectionSet {
    /// A fresh empty set.
    pub(crate) fn new() -> Self {
        Self {
            members: Vec::new(),
        }
    }

    /// True when no range has been committed yet.
    pub(crate) fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// How many committed ranges the set holds.
    pub(crate) fn len(&self) -> usize {
        self.members.len()
    }

    /// The committed ranges in reading order (top-to-bottom, left-to-right).
    pub(crate) fn members(&self) -> &[Selection] {
        &self.members
    }

    /// Drop every committed range.
    pub(crate) fn clear(&mut self) {
        self.members.clear();
    }

    /// Commit `sel` into the set, ignoring an empty (collapsed) selection so a
    /// bare click never adds a zero-width member. Returns `true` when a range was
    /// actually added.
    ///
    /// The inserted member is kept in reading order and **merged** with any
    /// existing single-row member it touches/overlaps, so overlapping adds
    /// collapse to one normalized span (block boundaries on multi-row members are
    /// preserved — they order but never merge across rows).
    pub(crate) fn add(&mut self, sel: Selection) -> bool {
        if sel.is_empty() {
            return false;
        }
        // Drop an exact duplicate (re-committing the identical range is a no-op
        // rather than a second highlight of the same cells).
        if self
            .members
            .iter()
            .any(|m| m.normalized() == sel.normalized() && m.surface == sel.surface)
        {
            return false;
        }
        self.members.push(sel);
        self.normalize();
        true
    }

    /// Remove any committed range that CONTAINS `pos` (a modifier-click on an
    /// already-selected cell toggles it off). Returns `true` when something was
    /// removed.
    pub(crate) fn remove_at(&mut self, surface: SelectionSurface, pos: Pos) -> bool {
        let before = self.members.len();
        self.members
            .retain(|m| !(m.surface == surface && range_contains(m, pos)));
        self.members.len() != before
    }

    /// Sort members into reading order, then merge adjacent/overlapping
    /// single-row members. Multi-row members order but never merge (block
    /// boundaries are preserved).
    fn normalize(&mut self) {
        self.members.sort_by_key(|m| {
            let (start, _) = m.normalized();
            (start.row, start.col)
        });
        let mut merged: Vec<Selection> = Vec::with_capacity(self.members.len());
        for sel in self.members.drain(..) {
            if let Some(last) = merged.last_mut()
                && let Some(combined) = try_merge_single_row(last, &sel)
            {
                *last = combined;
                continue;
            }
            merged.push(sel);
        }
        self.members = merged;
    }
}

/// Flatten the committed set PLUS the live `active` range (when present and
/// non-empty) into one reading-ordered list of ranges to act on. The live range
/// is folded through the same touch/overlap merge as a committed member, so a
/// single-row live range that overlaps a single-row member collapses into it.
/// Overlapping MULTI-row members are ordered but not merged (their block
/// boundaries are preserved), so the returned list can still contain ranges that
/// share rows — [`combined_clean_text`] is what de-duplicates those shared cells
/// when the combined text is taken, so no cell is copied twice.
pub(crate) fn combined_ranges(set: &SelectionSet, active: Option<&Selection>) -> Vec<Selection> {
    let mut all = SelectionSet::new();
    for m in &set.members {
        all.members.push(m.clone());
    }
    if let Some(sel) = active
        && !sel.is_empty()
    {
        // Skip an exact duplicate of an existing member.
        if !all
            .members
            .iter()
            .any(|m| m.normalized() == sel.normalized() && m.surface == sel.surface)
        {
            all.members.push(sel.clone());
        }
    }
    all.normalize();
    all.members
}

/// Join the clean text of every range in `ranges` over the painted `rows`,
/// separating disjoint ranges with a blank line so the combined payload reads as
/// distinct blocks (the spec's combined copy/export).
///
/// Ranges are emitted in the order given (callers pass them in reading order),
/// and any `(row, cleaned-col)` cell already emitted by an earlier range is
/// SUBTRACTED from a later one before its text is taken, so two overlapping
/// multi-row members never duplicate the rows they share (deep-review #48). A
/// range whose every cell was already claimed contributes nothing and its block
/// is dropped, exactly as an empty slice was before. For non-overlapping ranges
/// nothing is subtracted, so each block is byte-for-byte what a single-range
/// copy of it would yield.
pub(crate) fn combined_clean_text(
    rows: &[ratatui::text::Line<'static>],
    ranges: &[Selection],
) -> String {
    // Per visual row, the cleaned-char spans already emitted by earlier ranges.
    let mut claimed: std::collections::BTreeMap<usize, Vec<std::ops::Range<usize>>> =
        std::collections::BTreeMap::new();
    let mut blocks: Vec<String> = Vec::new();
    for sel in ranges {
        let mut row_slices: Vec<String> = Vec::new();
        for row in sel.row_span() {
            let Some((cleaned, span)) = selection::cleaned_row_span(rows, sel, row) else {
                continue;
            };
            let row_claimed = claimed.entry(row).or_default();
            // Emit only the part of this row's span not already taken by an
            // earlier range, then record the whole span as claimed so a later
            // overlapping range subtracts it in turn.
            for sub in subtract_claimed(span.clone(), row_claimed) {
                let slice: String = cleaned.chars().skip(sub.start).take(sub.len()).collect();
                row_slices.push(slice);
            }
            row_claimed.push(span);
        }
        let block = row_slices.join("\n").trim_end().to_string();
        if !block.is_empty() {
            blocks.push(block);
        }
    }
    blocks.join("\n\n")
}

/// Subtract every already-`claimed` cleaned-char range from `span`, returning the
/// uncovered sub-ranges in ascending order. Empty when `span` is wholly claimed.
fn subtract_claimed(
    span: std::ops::Range<usize>,
    claimed: &[std::ops::Range<usize>],
) -> Vec<std::ops::Range<usize>> {
    let mut pieces = vec![span];
    for taken in claimed {
        if taken.start >= taken.end {
            continue;
        }
        let mut next = Vec::with_capacity(pieces.len());
        for piece in pieces {
            // The part of `piece` strictly before `taken`.
            let left = piece.start..piece.end.min(taken.start);
            if left.start < left.end {
                next.push(left);
            }
            // The part of `piece` strictly after `taken`.
            let right = piece.start.max(taken.end)..piece.end;
            if right.start < right.end {
                next.push(right);
            }
        }
        pieces = next;
    }
    pieces
}

/// True when `pos` falls inside `sel`'s normalized span (row+column aware on the
/// endpoints; full rows in between for a multi-row range).
fn range_contains(sel: &Selection, pos: Pos) -> bool {
    let (start, end) = sel.normalized();
    if pos.row < start.row || pos.row > end.row {
        return false;
    }
    // Row mode covers each row edge to edge (mirrors `col_span_for_row`'s
    // `0..row_len`), and `extend_by_row` flips to Row without resetting the raw
    // anchor/cursor columns. Match any cell on a covered row, ignoring columns,
    // so a toggle-off click left of the anchor still hits the visible highlight.
    if sel.mode == selection::SelectionMode::Row {
        return true;
    }
    if start.row == end.row {
        return pos.col >= start.col && pos.col < end.col.max(start.col + 1);
    }
    if pos.row == start.row {
        return pos.col >= start.col;
    }
    if pos.row == end.row {
        return pos.col < end.col;
    }
    true
}

/// Merge two SINGLE-ROW selections whose normalized spans touch or overlap into
/// one covering selection; returns `None` when they cannot merge (different rows,
/// either is multi-row, different surface, or a gap between them). `a` is assumed
/// to start no later than `b` (the caller sorts first).
fn try_merge_single_row(a: &Selection, b: &Selection) -> Option<Selection> {
    if a.surface != b.surface {
        return None;
    }
    let (a_start, a_end) = a.normalized();
    let (b_start, b_end) = b.normalized();
    // Both must sit on the same single visual row.
    if a_start.row != a_end.row || b_start.row != b_end.row || a_start.row != b_start.row {
        return None;
    }
    // Touch/overlap test: b starts at or before a's end.
    if b_start.col > a_end.col {
        return None;
    }
    let merged_end = if a_end.col >= b_end.col { a_end } else { b_end };
    let mut out = a.clone();
    out.anchor = a_start;
    out.cursor = merged_end;
    Some(out)
}

#[cfg(test)]
#[path = "multi_selection_tests.rs"]
mod tests;
