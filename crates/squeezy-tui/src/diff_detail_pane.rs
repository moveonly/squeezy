//! Diff / detail pane for the Ctrl+T overlay (§11.7 "Diff/detail panes",
//! backlog 11G.10).
//!
//! When a transcript entry carries a large diff, a file excerpt, or bulky tool
//! output, reading it inside the scrolling overlay forces the rest of the
//! transcript off-screen. This module splits the open overlay into two columns:
//! the transcript stays on the LEFT (context preserved) while a fixed,
//! independently-scrolled DETAIL pane on the RIGHT shows the focused entry's
//! fully-expanded body. The user pins an entry into the pane with `d`, scrolls
//! it with `Shift`+the scroll keys (or the wheel over the pane), and closes it
//! with `d` again or `Esc`.
//!
//! Like its peer leaf modules (`interaction`, `minimap`, `jump_marks`) this file
//! holds only PURE geometry/scroll math plus the small pinned-state struct. It
//! has no dependency on `TuiApp`; the crate root owns the field, the keybinding,
//! the per-frame render call, and the content extraction (which needs
//! `TranscriptEntryKind`). Keeping the math here lets it be unit-tested without a
//! terminal, exactly like the rest of the direct-manipulation substrate.

use ratatui::layout::Rect;

/// The minimum overlay CONTENT width (inside the rounded border) at which the
/// split is worth doing. Below this the two columns would each be uselessly
/// narrow, so the pane request is honoured but paints nothing this frame and
/// the transcript keeps the full width — a graceful no-op rather than a
/// corrupted two-column squeeze. Chosen so each side keeps at least ~28 cells.
pub(crate) const MIN_SPLIT_WIDTH: u16 = 64;

/// The detail pane's share of the overlay content width, as a fraction. The
/// transcript keeps the rest. Two-fifths gives the pane enough room for a
/// wrapped diff while leaving the transcript the majority for context.
const PANE_NUMERATOR: u16 = 2;
const PANE_DENOMINATOR: u16 = 5;

/// One row of separator drawn between the transcript and the pane.
const SEPARATOR_WIDTH: u16 = 1;

/// Pinned state for the detail pane. Addresses the pinned entry by its STABLE
/// `TranscriptEntry::id` (never a Vec index), so a streamed/coalesced transcript
/// mutation never repoints the pane at the wrong entry — if the id disappears
/// the crate root closes the pane. `scroll` is a logical row offset from the top
/// of the pinned entry's expanded body, clamped at render time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DiffDetailPaneState {
    /// `TranscriptEntry::id` of the pinned entry.
    pub(crate) entry_id: u64,
    /// Logical scroll offset (rows from the top of the pinned body).
    pub(crate) scroll: usize,
}

impl DiffDetailPaneState {
    pub(crate) fn new(entry_id: u64) -> Self {
        Self {
            entry_id,
            scroll: 0,
        }
    }
}

/// The two columns the split produces from an overlay content rect: the
/// (narrowed) transcript on the left and the detail pane on the right, with a
/// one-cell separator gutter between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DiffDetailLayout {
    /// The transcript's narrowed rect (left column).
    pub(crate) transcript: Rect,
    /// The one-cell vertical separator between the columns.
    pub(crate) separator: Rect,
    /// The detail pane's rect (right column), border included.
    pub(crate) pane: Rect,
}

/// Split an overlay CONTENT rect (the area inside the overlay's rounded border,
/// before the inner text/scrollbar split) into a transcript column and a detail
/// pane column. Returns `None` when the area is too narrow to usefully split
/// (`width < MIN_SPLIT_WIDTH` or zero height) — the caller then paints the
/// transcript full-width as if no pane were open.
pub(crate) fn split_overlay_content(content: Rect) -> Option<DiffDetailLayout> {
    if content.width < MIN_SPLIT_WIDTH || content.height == 0 {
        return None;
    }
    // Reserve the pane's share off the right edge, then a one-cell separator.
    let pane_width = (content.width * PANE_NUMERATOR / PANE_DENOMINATOR).max(1);
    // Guard: the transcript must keep at least one cell after the pane +
    // separator are carved off. `MIN_SPLIT_WIDTH` already guarantees this for
    // the chosen fraction, but compute defensively so the rects never overlap.
    let reserved = pane_width.saturating_add(SEPARATOR_WIDTH);
    if reserved >= content.width {
        return None;
    }
    let transcript_width = content.width - reserved;
    let transcript = Rect {
        x: content.x,
        y: content.y,
        width: transcript_width,
        height: content.height,
    };
    let separator = Rect {
        x: content.x + transcript_width,
        y: content.y,
        width: SEPARATOR_WIDTH,
        height: content.height,
    };
    let pane = Rect {
        x: content.x + transcript_width + SEPARATOR_WIDTH,
        y: content.y,
        width: pane_width,
        height: content.height,
    };
    Some(DiffDetailLayout {
        transcript,
        separator,
        pane,
    })
}

/// The text rect INSIDE the detail pane's rounded border (one cell of inset on
/// every side). Returns a zero-area rect when the pane is too small to hold any
/// content, so callers can short-circuit without painting into the border.
pub(crate) fn pane_inner(pane: Rect) -> Rect {
    Rect {
        x: pane.x.saturating_add(1),
        y: pane.y.saturating_add(1),
        width: pane.width.saturating_sub(2),
        height: pane.height.saturating_sub(2),
    }
}

/// The largest scroll offset that still shows content: `total_rows -
/// viewport_h`, saturating to `0` when the body fits the pane. Mirrors the
/// transcript's own clamp so a short body never scrolls past its last row.
pub(crate) fn pane_max_scroll(total_rows: usize, viewport_h: usize) -> usize {
    total_rows.saturating_sub(viewport_h)
}

/// Clamp a requested scroll into `[0, pane_max_scroll]`. Used by both the
/// keyboard (`Shift`+scroll) and wheel paths, and re-applied at render time so a
/// transcript mutation that shrinks the pinned body never strands the offset
/// past the end.
pub(crate) fn clamp_pane_scroll(scroll: usize, total_rows: usize, viewport_h: usize) -> usize {
    scroll.min(pane_max_scroll(total_rows, viewport_h))
}

/// Whether a `(column, row)` cell falls inside `rect`. Half-open on both axes,
/// matching `ratatui::layout::Rect`'s geometry, so the wheel hit-test routes a
/// scroll event to the pane only when the pointer is actually over it.
pub(crate) fn rect_contains(rect: Rect, column: u16, row: u16) -> bool {
    rect.width > 0
        && rect.height > 0
        && column >= rect.x
        && column < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

#[cfg(test)]
#[path = "diff_detail_pane_tests.rs"]
mod tests;
