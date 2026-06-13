//! Screen-buffer-level selection: a linear span over painted terminal cells,
//! used to select CHROME text (status line, footer, breadcrumbs, banners) — the
//! surfaces the per-surface transcript/composer selections don't cover. A drag
//! that begins outside both the transcript text area and the composer arms one
//! of these; copy reads the glyphs straight out of a snapshot of the rendered
//! buffer. Coordinates are absolute screen cells, so any scroll/resize reflow
//! invalidates it (the caller drops it on those events).

/// A linear (text-style, not block) selection over screen cells `(col, row)`.
/// Direction-agnostic: `(anchor, cursor)` is ordered via [`Self::normalized`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScreenSelection {
    /// Fixed end (where the drag started).
    pub(crate) anchor_col: u16,
    pub(crate) anchor_row: u16,
    /// Moving end (follows the drag).
    pub(crate) cursor_col: u16,
    pub(crate) cursor_row: u16,
}

impl ScreenSelection {
    pub(crate) fn at(col: u16, row: u16) -> Self {
        Self {
            anchor_col: col,
            anchor_row: row,
            cursor_col: col,
            cursor_row: row,
        }
    }

    /// `((start_col, start_row), (end_col, end_row))` in reading order
    /// (top-to-bottom, left-to-right within a row).
    pub(crate) fn normalized(&self) -> ((u16, u16), (u16, u16)) {
        if (self.anchor_row, self.anchor_col) <= (self.cursor_row, self.cursor_col) {
            (
                (self.anchor_col, self.anchor_row),
                (self.cursor_col, self.cursor_row),
            )
        } else {
            (
                (self.cursor_col, self.cursor_row),
                (self.anchor_col, self.anchor_row),
            )
        }
    }

    /// Collapsed (a bare click, nothing dragged).
    pub(crate) fn is_empty(&self) -> bool {
        self.anchor_col == self.cursor_col && self.anchor_row == self.cursor_row
    }

    /// Inclusive row span.
    pub(crate) fn row_span(&self) -> std::ops::RangeInclusive<u16> {
        let ((_, start_row), (_, end_row)) = self.normalized();
        start_row..=end_row
    }

    /// Half-open `[lo, hi)` column span on `row`, INCLUSIVE of the cursor cell
    /// (so a drag from col 5→10 covers 5..=10). First row runs from its start
    /// column to `width`; the last row from 0 to the cursor; middle rows span
    /// the full width. `None` when `row` is outside the selection or the span
    /// would be empty.
    pub(crate) fn col_span_for_row(&self, row: u16, width: u16) -> Option<std::ops::Range<u16>> {
        let ((start_col, start_row), (end_col, end_row)) = self.normalized();
        if row < start_row || row > end_row {
            return None;
        }
        let lo = if row == start_row { start_col } else { 0 };
        let hi = if row == end_row {
            end_col.saturating_add(1)
        } else {
            width
        };
        let lo = lo.min(width);
        let hi = hi.min(width);
        if lo >= hi { None } else { Some(lo..hi) }
    }
}
