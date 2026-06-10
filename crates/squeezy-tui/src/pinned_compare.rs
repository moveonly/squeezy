//! Pinned Compare View (§12.2.3): pin one transcript entry/region and view it
//! side-by-side (or stacked, on a narrow terminal) against the live transcript —
//! or against a second pinned entry — for comparison, with each pane keeping its
//! own scroll and one pane "active" so the keyboard/wheel targets it.
//!
//! **Reuses the §11G.10 detail-pane split machinery.** The diff/detail pane
//! ([`crate::diff_detail_pane`]) already carves a fixed, independently-scrolled
//! column off the Ctrl+T overlay and pins one entry into it by stable
//! `TranscriptEntry::id`. This module is the spec's natural extension of that:
//! instead of *one* pane beside the scrolling transcript, it pins *two* surfaces
//! and shows them as equal columns (a TRUE compare) with a focus model so
//! scroll/copy target the active pane. The geometry primitives mirror the detail
//! pane's — a `MIN_SPLIT_WIDTH` threshold, a one-cell separator gutter,
//! `pane_inner` one-cell insets, the same `clamp`/`max_scroll` semantics, and the
//! same half-open `rect_contains` hit-test — so the two features behave
//! identically where they overlap and a reader of one understands the other.
//!
//! **Layout thresholds for split vs. stacked.** The spec asks for "layout
//! thresholds for split vs overlay/tab". On a wide terminal the two surfaces sit
//! as equal side-by-side columns ([`CompareLayout::Split`]); below
//! [`MIN_SPLIT_WIDTH`] there is no room for two readable columns, so the view
//! stacks them — the active pane on top, the other below a divider
//! ([`CompareLayout::Stacked`]) — keeping both visible without a corrupted
//! squeeze. Either way both panes paint; neither is hidden.
//!
//! **Addressed by stable id, healed when it disappears.** Both the pinned entry
//! and the optional compare target are addressed by `TranscriptEntry::id` (never
//! a `Vec` index), so a streamed/coalesced transcript mutation never repoints a
//! pane at the wrong entry. When an id falls out of the transcript the crate root
//! closes the view (heals to `None`) rather than painting an empty pane.
//!
//! **Pure geometry/scroll/diff math, no `TuiApp`.** Like its peer leaf modules
//! (`diff_detail_pane`, `interaction`, `minimap`, `jump_marks`) this file holds
//! only the small pinned-state struct plus pure layout/scroll/diff helpers. The
//! crate root owns the field, the keybinding, the per-frame render call, and the
//! content extraction. Keeping the math here lets it be unit-tested without a
//! terminal.

use ratatui::layout::Rect;

/// The minimum overlay CONTENT width (inside the rounded border) at which an
/// equal side-by-side split is worth doing. Below this the two columns would each
/// be uselessly narrow, so the view stacks the panes vertically instead (the
/// spec's "split vs overlay/tab" threshold). Matches the detail pane's threshold
/// (`diff_detail_pane::MIN_SPLIT_WIDTH`) so the two features flip to their
/// fallback at the same width.
pub(crate) const MIN_SPLIT_WIDTH: u16 = 64;

/// The minimum overlay CONTENT height at which the stacked fallback can show two
/// readable rows of panes (each needs a border + at least one content row, plus a
/// one-row divider between them). Below this even stacking is hopeless and the
/// caller paints nothing.
pub(crate) const MIN_STACK_HEIGHT: u16 = 7;

/// One row/column of separator drawn between the two panes.
const SEPARATOR: u16 = 1;

/// Which pane the keyboard / wheel currently drives. The *active* pane scrolls
/// with the scroll keys and is the copy target; the other pane holds its own
/// independent offset. `Tab` (and a click on a pane) flips this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ComparePane {
    /// The pinned ("left"/"top") entry's pane.
    Pinned,
    /// The compare-target ("right"/"bottom") pane. When the view compares against
    /// the live transcript rather than a second pinned entry, this is the live
    /// transcript surface.
    Compare,
}

impl ComparePane {
    /// The other pane — the target of a focus toggle.
    pub(crate) fn toggled(self) -> Self {
        match self {
            ComparePane::Pinned => ComparePane::Compare,
            ComparePane::Compare => ComparePane::Pinned,
        }
    }

    /// Short ASCII label for the pane (used in pane titles / status). ASCII-only
    /// so it carries meaning without color or a private-use glyph.
    pub(crate) fn label(self) -> &'static str {
        match self {
            ComparePane::Pinned => "pinned",
            ComparePane::Compare => "compare",
        }
    }
}

/// How the two pinned surfaces are presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompareMode {
    /// Show each surface's own content verbatim (side-by-side or stacked).
    Content,
    /// Show a line-based clean-text diff of the two surfaces in a single column.
    Diff,
}

impl CompareMode {
    /// Toggle Content <-> Diff.
    pub(crate) fn toggled(self) -> Self {
        match self {
            CompareMode::Content => CompareMode::Diff,
            CompareMode::Diff => CompareMode::Content,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            CompareMode::Content => "content",
            CompareMode::Diff => "diff",
        }
    }
}

/// Pinned state for the compare view (§12.2.3). `pinned_id` is the entry pinned
/// into the first pane; `compare_id` is the optional second pinned entry — `None`
/// means "compare against the live transcript", which is the spec's default
/// "pin an old response beside the live transcript". `focus` routes the keyboard
/// / wheel; `pinned_scroll` / `compare_scroll` are the two INDEPENDENT logical
/// row offsets (clamped at render time). Both ids are stable
/// `TranscriptEntry::id`s, never `Vec` indices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PinnedCompareState {
    /// `TranscriptEntry::id` of the pinned entry (first pane).
    pub(crate) pinned_id: u64,
    /// `TranscriptEntry::id` of the compare target (second pane), or `None` to
    /// compare against the live transcript surface.
    pub(crate) compare_id: Option<u64>,
    /// Which pane the keyboard / wheel drives.
    pub(crate) focus: ComparePane,
    /// Content vs. line-based clean-text diff.
    pub(crate) mode: CompareMode,
    /// Independent scroll offset for the pinned pane.
    pub(crate) pinned_scroll: usize,
    /// Independent scroll offset for the compare pane.
    pub(crate) compare_scroll: usize,
}

impl PinnedCompareState {
    /// Open the compare view pinned to `pinned_id`, comparing against the live
    /// transcript, focus on the pinned pane, content mode, both panes at the top.
    pub(crate) fn new(pinned_id: u64) -> Self {
        Self {
            pinned_id,
            compare_id: None,
            focus: ComparePane::Pinned,
            mode: CompareMode::Content,
            pinned_scroll: 0,
            compare_scroll: 0,
        }
    }

    /// The scroll offset of the currently-focused pane.
    pub(crate) fn focused_scroll(&self) -> usize {
        match self.focus {
            ComparePane::Pinned => self.pinned_scroll,
            ComparePane::Compare => self.compare_scroll,
        }
    }

    /// Set the scroll offset of the currently-focused pane.
    pub(crate) fn set_focused_scroll(&mut self, scroll: usize) {
        match self.focus {
            ComparePane::Pinned => self.pinned_scroll = scroll,
            ComparePane::Compare => self.compare_scroll = scroll,
        }
    }
}

/// The two equal columns (or stacked rows) a compare view produces, plus the
/// separator between them. In [`CompareLayout::Split`] `first`/`second` are
/// left/right; in [`CompareLayout::Stacked`] they are top/bottom. `first` always
/// holds the *active* pane so the focused surface is the prominent one (left, or
/// top), matching the focus model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompareLayout {
    /// Wide terminal: two equal side-by-side columns with a vertical separator.
    Split {
        first: Rect,
        separator: Rect,
        second: Rect,
    },
    /// Narrow terminal: the active pane on top, the other below a horizontal
    /// divider. The spec's "overlay/tab" fallback, realised as a stack so both
    /// surfaces stay visible.
    Stacked {
        first: Rect,
        separator: Rect,
        second: Rect,
    },
}

impl CompareLayout {
    /// The active pane's rect (`first`) and the other pane's rect (`second`),
    /// regardless of split/stacked. Convenience for the renderer/hit-test.
    pub(crate) fn panes(self) -> (Rect, Rect) {
        match self {
            CompareLayout::Split { first, second, .. }
            | CompareLayout::Stacked { first, second, .. } => (first, second),
        }
    }

    /// The separator rect (vertical rule when split, horizontal rule when
    /// stacked).
    pub(crate) fn separator(self) -> Rect {
        match self {
            CompareLayout::Split { separator, .. } | CompareLayout::Stacked { separator, .. } => {
                separator
            }
        }
    }

    /// Whether this layout is the stacked (narrow) fallback.
    pub(crate) fn is_stacked(self) -> bool {
        matches!(self, CompareLayout::Stacked { .. })
    }
}

/// Split an overlay CONTENT rect (inside the overlay's rounded border) into two
/// equal panes for the compare view. Wide enough → an even left/right split with
/// a one-cell vertical separator. Too narrow but tall enough → a top/bottom stack
/// with a one-row horizontal divider (the spec's "split vs overlay/tab"
/// threshold). Too small for either → `None`, and the caller falls back to the
/// plain transcript.
///
/// In both modes the *first* rect is the column/row that will hold the ACTIVE
/// pane, so the focused surface gets the prominent slot (left, or top).
pub(crate) fn split_overlay_content(content: Rect) -> Option<CompareLayout> {
    if content.width == 0 || content.height == 0 {
        return None;
    }
    if content.width >= MIN_SPLIT_WIDTH {
        // Equal side-by-side columns. Carve the separator out of the middle, then
        // split the remainder evenly; the right column absorbs an odd cell so the
        // rects always tile the content exactly.
        let usable = content.width - SEPARATOR;
        let first_width = usable / 2;
        let second_width = usable - first_width;
        if first_width == 0 || second_width == 0 {
            return None;
        }
        let first = Rect {
            x: content.x,
            y: content.y,
            width: first_width,
            height: content.height,
        };
        let separator = Rect {
            x: content.x + first_width,
            y: content.y,
            width: SEPARATOR,
            height: content.height,
        };
        let second = Rect {
            x: content.x + first_width + SEPARATOR,
            y: content.y,
            width: second_width,
            height: content.height,
        };
        return Some(CompareLayout::Split {
            first,
            separator,
            second,
        });
    }
    // Narrow: stack the panes if there's vertical room for two readable rows.
    if content.height < MIN_STACK_HEIGHT {
        return None;
    }
    let usable = content.height - SEPARATOR;
    let first_height = usable / 2;
    let second_height = usable - first_height;
    if first_height == 0 || second_height == 0 {
        return None;
    }
    let first = Rect {
        x: content.x,
        y: content.y,
        width: content.width,
        height: first_height,
    };
    let separator = Rect {
        x: content.x,
        y: content.y + first_height,
        width: content.width,
        height: SEPARATOR,
    };
    let second = Rect {
        x: content.x,
        y: content.y + first_height + SEPARATOR,
        width: content.width,
        height: second_height,
    };
    Some(CompareLayout::Stacked {
        first,
        separator,
        second,
    })
}

/// The text rect INSIDE a pane's rounded border (one cell of inset on every
/// side). Returns a zero-area rect when the pane is too small to hold content, so
/// callers can short-circuit without painting into the border. Mirrors
/// `diff_detail_pane::pane_inner`.
pub(crate) fn pane_inner(pane: Rect) -> Rect {
    Rect {
        x: pane.x.saturating_add(1),
        y: pane.y.saturating_add(1),
        width: pane.width.saturating_sub(2),
        height: pane.height.saturating_sub(2),
    }
}

/// The largest scroll offset that still shows content: `total_rows -
/// viewport_h`, saturating to `0` when the body fits. Mirrors the detail pane and
/// the transcript so a short body never scrolls past its last row.
pub(crate) fn pane_max_scroll(total_rows: usize, viewport_h: usize) -> usize {
    total_rows.saturating_sub(viewport_h)
}

/// Clamp a requested scroll into `[0, pane_max_scroll]`. Re-applied at render time
/// so a transcript mutation that shrinks a pane's body never strands the offset.
pub(crate) fn clamp_pane_scroll(scroll: usize, total_rows: usize, viewport_h: usize) -> usize {
    scroll.min(pane_max_scroll(total_rows, viewport_h))
}

/// Whether a `(column, row)` cell falls inside `rect`. Half-open on both axes,
/// matching `ratatui::layout::Rect`. Used to route a wheel/click to the pane the
/// pointer is over. Mirrors `diff_detail_pane::rect_contains`.
pub(crate) fn rect_contains(rect: Rect, column: u16, row: u16) -> bool {
    rect.width > 0
        && rect.height > 0
        && column >= rect.x
        && column < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

/// The maximum number of source rows either side may contribute to a clean-text
/// diff before the diff is refused. The spec's named risk is "expensive large
/// diffs"; capping the input keeps the line-based diff's `O(n*m)` LCS bounded and
/// predictable. Above the cap the caller shows the two panes' content verbatim
/// instead of a diff.
pub(crate) const DIFF_LINE_LIMIT: usize = 600;

/// One line of a [`clean_text_diff`] result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffTag {
    /// Present in both sides, unchanged.
    Same,
    /// Present only in the NEW (compare/right/bottom) side — an addition.
    Added,
    /// Present only in the OLD (pinned/left/top) side — a removal.
    Removed,
}

impl DiffTag {
    /// The conventional one-char gutter marker (` `, `+`, `-`). ASCII so the diff
    /// reads without color.
    pub(crate) fn marker(self) -> char {
        match self {
            DiffTag::Same => ' ',
            DiffTag::Added => '+',
            DiffTag::Removed => '-',
        }
    }
}

/// One tagged row of a clean-text diff: its change tag and the line text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiffLine {
    pub(crate) tag: DiffTag,
    pub(crate) text: String,
}

/// A bounded line-based clean-text diff of `old` vs `new` (the spec's "Start with
/// line-based clean-text diff"). Computes a longest-common-subsequence alignment
/// and walks it into a `Same`/`Removed`/`Added` row list — `Removed` lines (only
/// in `old`) and `Added` lines (only in `new`) in the order they appear, with the
/// shared lines as `Same`.
///
/// Returns `None` when either side exceeds [`DIFF_LINE_LIMIT`] rows, so the caller
/// can fall back to plain content rather than pay an unbounded `O(n*m)` cost (the
/// spec's "add size limits and lazy diffing" mitigation).
pub(crate) fn clean_text_diff(old: &[String], new: &[String]) -> Option<Vec<DiffLine>> {
    if old.len() > DIFF_LINE_LIMIT || new.len() > DIFF_LINE_LIMIT {
        return None;
    }
    let n = old.len();
    let m = new.len();
    // LCS length table: `lcs[i][j]` = length of the longest common subsequence of
    // `old[i..]` and `new[j..]`. Filled bottom-up so the forward walk below can
    // greedily reconstruct one optimal alignment.
    let mut lcs = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if old[i] == new[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut out = Vec::with_capacity(n + m);
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if old[i] == new[j] {
            out.push(DiffLine {
                tag: DiffTag::Same,
                text: old[i].clone(),
            });
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            out.push(DiffLine {
                tag: DiffTag::Removed,
                text: old[i].clone(),
            });
            i += 1;
        } else {
            out.push(DiffLine {
                tag: DiffTag::Added,
                text: new[j].clone(),
            });
            j += 1;
        }
    }
    while i < n {
        out.push(DiffLine {
            tag: DiffTag::Removed,
            text: old[i].clone(),
        });
        i += 1;
    }
    while j < m {
        out.push(DiffLine {
            tag: DiffTag::Added,
            text: new[j].clone(),
        });
        j += 1;
    }
    Some(out)
}

#[cfg(test)]
#[path = "pinned_compare_tests.rs"]
mod tests;
