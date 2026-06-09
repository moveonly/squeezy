//! Visual SELECTION state and range math for the transcript surfaces.
//!
//! This module owns the *pure* selection model: an anchor/cursor pair over the
//! painted wrapped rows of one surface, the normalization + per-row column-span
//! math that turns that pair into "which cells are selected on row N", and the
//! two bridges built on top of it — clean clipboard-text extraction and a
//! highlight restyle of the drawn rows. It is deliberately free of any rendering
//! or event side effects; the crate root (`lib.rs`) owns the field on `TuiApp`,
//! the gesture/key dispatch, and the per-frame text-area caches that turn a
//! mouse click into a [`Pos`].
//!
//! ## Why select on the painted `Vec<Line>`, not the row model
//!
//! The main view and the Ctrl+T overlay draw from **different** wrapped
//! `Vec<Line>` sources (`transcript_lines_for_render` vs.
//! `with_transcript_overlay_rows`), and the shared
//! [`crate::transcript_surface`] row model mirrors only the overlay. So a
//! selection that must align byte-for-byte with the painted glyphs cannot route
//! through that model. Instead a [`Pos`] is *surface-local*: `row` indexes the
//! surface's own `Vec<Line>` and `col` is a **char offset** into that line's
//! joined plain text — the same char basis `split_spans_at_column` and
//! `style_spans_of_line` use. Both row sources are pure functions of
//! `(app, width, detail)`, so the indices are stable across redraws and a resize
//! simply re-anchors (clamp `row`/`col` to the new row count and width).
//!
//! ## Char offsets vs. display width
//!
//! The highlight + slice math is expressed in **char offsets**, because that is
//! the basis the existing span splitter and the clean-text helpers
//! ([`crate::transcript_surface::strip_gutter`] /
//! [`crate::transcript_surface::plain_text_of_line`]) already use. Mapping a
//! mouse **display column** (where a CJK/wide glyph occupies two cells) onto a
//! char offset is a hit-testing concern handled at the call site; this module
//! exposes [`char_offset_for_display_col`] so the mouse handler and tests share
//! one width-aware mapping rather than each re-deriving it. The inverse helper
//! [`display_width_of_chars`] is used only by this module's tests (asserting the
//! cell width of a char-offset prefix); the mouse handler does not consume it.
//!
//! Until the renderers and gesture/key handlers consume this module (the Phase 5
//! integration step), the whole surface is dead in a plain `cargo build`, so the
//! module-level `allow(dead_code)` below is what keeps `-D warnings` green —
//! mirroring [`crate::transcript_surface`]. Narrow it to per-item allows once
//! `render_transcript` and the mouse/key dispatch wire it up.
#![allow(dead_code)]

use std::ops::{Range, RangeInclusive};

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

use crate::transcript_surface::{plain_text_of_line, strip_gutter};

/// Which drawn surface the selection lives on. The two surfaces have
/// independent row `Vec`s, so a selection is only meaningful against one of
/// them at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelectionSurface {
    /// The always-on main transcript pane.
    Main,
    /// The Ctrl+T full-transcript overlay.
    Overlay,
}

/// A point in the active surface's PAINTED wrapped rows.
///
/// `row` indexes the surface's `Vec<Line>`; `col` is a **char offset** into
/// that line's joined plain text (`0..=char_count`, inclusive end so a caret can
/// sit past the last char). Positions compare in `(row, col)` reading order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Pos {
    /// Visual row index into the surface's wrapped `Vec<Line>`.
    pub(crate) row: usize,
    /// Char offset into the row's joined plain text.
    pub(crate) col: usize,
}

impl Pos {
    pub(crate) fn new(row: usize, col: usize) -> Self {
        Self { row, col }
    }
}

/// Granularity of a selection — chosen by gesture at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelectionMode {
    /// Character-granular: spans partial columns on the first/last rows
    /// (single mouse drag).
    Cell,
    /// Whole-visual-row: every covered row is selected edge to edge
    /// (Shift+Up/Down, triple-click).
    Row,
    /// Word: anchor/cursor snapped to word boundaries (double-click).
    Word,
}

/// A visual selection over one surface's painted wrapped rows.
///
/// `anchor` is the fixed end (where the drag/extend began) and `cursor` is the
/// moving end; the pair is direction-agnostic — [`Selection::normalized`] sorts
/// them into reading order. `width` records the `text_area.width` the positions
/// were taken at so a resize can detect drift and re-anchor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Selection {
    pub(crate) surface: SelectionSurface,
    /// Where the drag/extend started (fixed).
    pub(crate) anchor: Pos,
    /// The moving end.
    pub(crate) cursor: Pos,
    pub(crate) mode: SelectionMode,
    /// The surface width the positions were captured at. A different live width
    /// means the wrapped rows differ and the indices must be re-anchored.
    pub(crate) width: u16,
}

impl Selection {
    /// A fresh selection collapsed onto a single point.
    pub(crate) fn at(surface: SelectionSurface, pos: Pos, mode: SelectionMode, width: u16) -> Self {
        Self {
            surface,
            anchor: pos,
            cursor: pos,
            mode,
            width,
        }
    }

    /// Normalized `(start, end)` with `start <= end` in `(row, col)` reading
    /// order, independent of drag direction.
    pub(crate) fn normalized(&self) -> (Pos, Pos) {
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }

    /// Inclusive row span touched by the selection.
    pub(crate) fn row_span(&self) -> RangeInclusive<usize> {
        let (start, end) = self.normalized();
        start.row..=end.row
    }

    /// The half-open COLUMN span `[start..end)` selected on visual row `row`,
    /// given that row's total char width `row_len`.
    ///
    /// Encodes the first/last/middle rule:
    /// - single-row selection: `start.col .. end.col`
    /// - first row of a multi-row range: `start.col .. row_len`
    /// - last  row of a multi-row range: `0 .. end.col`
    /// - interior rows: `0 .. row_len` (full width)
    ///
    /// In [`SelectionMode::Row`] the endpoints are forced to the row edges so a
    /// whole-row selection always spans `0..row_len`. In [`SelectionMode::Word`]
    /// the endpoints are expected to be pre-snapped by the caller, so this still
    /// yields the right edges. Returns `None` when `row` is outside
    /// [`row_span`](Self::row_span). Column endpoints are clamped to `row_len`.
    pub(crate) fn col_span_for_row(&self, row: usize, row_len: usize) -> Option<Range<usize>> {
        let (start, end) = self.normalized();
        if row < start.row || row > end.row {
            return None;
        }

        // Row mode selects each covered row edge to edge regardless of the raw
        // anchor/cursor columns.
        if self.mode == SelectionMode::Row {
            return Some(0..row_len);
        }

        let single = start.row == end.row;
        let (lo, hi) = if single {
            (start.col, end.col)
        } else if row == start.row {
            (start.col, row_len)
        } else if row == end.row {
            (0, end.col)
        } else {
            (0, row_len)
        };

        let lo = lo.min(row_len);
        let hi = hi.min(row_len);
        // Keep the range well-formed even if a stale col overshoots after a
        // resize re-anchor.
        Some(lo..hi.max(lo))
    }

    /// True when nothing is selected: a collapsed `Cell` selection (anchor ==
    /// cursor). `Row`/`Word` selections are never empty — they always cover at
    /// least the snapped span of one row.
    pub(crate) fn is_empty(&self) -> bool {
        matches!(self.mode, SelectionMode::Cell) && self.anchor == self.cursor
    }
}

/// Char count of a line's joined plain text — the unit `col` is measured in.
fn line_char_len(line: &Line<'static>) -> usize {
    plain_text_of_line(line).chars().count()
}

/// Display width (terminal cells) of the first `char_count` chars of `text`.
/// Wide glyphs (CJK, some emoji) count as two cells; control chars as zero.
pub(crate) fn display_width_of_chars(text: &str, char_count: usize) -> usize {
    text.chars()
        .take(char_count)
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
        .sum()
}

/// Map a mouse **display column** within a line's plain text to a **char
/// offset**, accounting for wide glyphs. A click that lands on the *second*
/// cell of a wide glyph resolves to the offset *after* that glyph, and a click
/// past the end of the content clamps to the char count (a caret past the last
/// char). This is the bridge the mouse hit-tester uses so a click on column
/// math respects display width even when the row holds CJK/wide glyphs.
pub(crate) fn char_offset_for_display_col(line_plain: &str, display_col: usize) -> usize {
    let mut consumed = 0usize;
    for (idx, ch) in line_plain.chars().enumerate() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        // Zero-width chars (combining marks, ZWJ joiners) attach to the
        // preceding grapheme and occupy no display cell, so a click never
        // resolves *to* one and they must not consume a column. Skipping them
        // (rather than treating them as one cell via `w.max(1)`) keeps the
        // mapping aligned: a click past a base glyph lands on the next *cell*
        // glyph, not on a phantom cell for the zero-width char.
        if w == 0 {
            continue;
        }
        // A click on any cell the glyph occupies selects up to that glyph; the
        // boundary that "contains" the click is the one whose cell span covers
        // display_col.
        if display_col < consumed + w {
            return idx;
        }
        consumed += w;
    }
    line_plain.chars().count()
}

/// Snap a `col` to the start/end of the WORD under it within a line's plain
/// text. A word is a maximal run of non-space, non-rail-gutter chars; a click in
/// whitespace snaps to that whitespace run so a double-click there does not jump
/// into the next word. Offsets are char offsets, matching [`Pos::col`].
pub(crate) fn word_bounds(line_plain: &str, col: usize) -> Range<usize> {
    let chars: Vec<char> = line_plain.chars().collect();
    let n = chars.len();
    if n == 0 {
        return 0..0;
    }
    // Clamp a caret that sits past the last char back onto the last char so the
    // word under "the end" is the trailing word.
    let probe = col.min(n - 1);
    let is_word = |c: char| !c.is_whitespace();
    let target = is_word(chars[probe]);

    let mut start = probe;
    while start > 0 && is_word(chars[start - 1]) == target {
        start -= 1;
    }
    let mut end = probe + 1;
    while end < n && is_word(chars[end]) == target {
        end += 1;
    }
    start..end
}

/// Move the cursor end of `sel` by `delta` whole visual rows (positive = down),
/// switching to [`SelectionMode::Row`] so each covered row selects edge to edge.
/// Clamps the cursor row to `0..row_count`. A no-op `row_count` of 0 leaves the
/// selection untouched.
pub(crate) fn extend_by_row(sel: &mut Selection, delta: i64, row_count: usize) {
    if row_count == 0 {
        return;
    }
    let max_row = row_count - 1;
    let next = (sel.cursor.row as i64 + delta).clamp(0, max_row as i64) as usize;
    sel.cursor.row = next;
    sel.mode = SelectionMode::Row;
}

/// Move the cursor end of `sel` by one `page` of rows (Shift+PgUp/PgDn), in
/// [`SelectionMode::Row`]. `up` extends toward row 0. Clamps to `0..row_count`.
pub(crate) fn extend_by_page(sel: &mut Selection, page: usize, up: bool, row_count: usize) {
    let delta = page as i64;
    extend_by_row(sel, if up { -delta } else { delta }, row_count);
}

/// Restyle the highlight `Style` onto a clone of `rows`, splitting each selected
/// line at its column-span boundaries (the SAME char-offset basis the wrapper's
/// `split_spans_at_column` uses) and patching the middle slice. Rows outside
/// [`Selection::row_span`] are cloned unchanged. The full pre-scroll `Vec` is
/// returned so callers bake the highlight *before* applying their scroll offset
/// — off-screen rows are then clipped by the surface's own paragraph/skip.
pub(crate) fn rows_with_selection_highlight(
    rows: &[Line<'static>],
    sel: &Selection,
    highlight: Style,
) -> Vec<Line<'static>> {
    rows.iter()
        .enumerate()
        .map(|(row_idx, line)| {
            let row_len = line_char_len(line);
            match sel.col_span_for_row(row_idx, row_len) {
                Some(span) if !span.is_empty() => highlight_line(line, span, highlight),
                _ => line.clone(),
            }
        })
        .collect()
}

/// Split `line`'s spans at the char-offset boundaries of `span` and patch the
/// selected middle slice with `highlight`, preserving the surrounding styles.
fn highlight_line(line: &Line<'static>, span: Range<usize>, highlight: Style) -> Line<'static> {
    let (before, rest) = split_spans_at_char(&line.spans, span.start);
    let (mid, after) = split_spans_at_char(&rest, span.end - span.start);
    let mut out: Vec<Span<'static>> = Vec::with_capacity(before.len() + mid.len() + after.len());
    out.extend(before);
    out.extend(mid.into_iter().map(|s| {
        let style = s.style.patch(highlight);
        Span::styled(s.content, style)
    }));
    out.extend(after);
    let mut new_line = Line::from(out);
    new_line.alignment = line.alignment;
    new_line.style = line.style;
    new_line
}

/// Char-offset analogue of the renderer's `split_spans_at_column`: split a span
/// slice at char offset `col`, cloning whole spans and splitting the one the
/// boundary falls inside. Kept private and identical in basis so the highlight
/// lands on exactly the cells the wrapper drew.
fn split_spans_at_char(
    spans: &[Span<'static>],
    col: usize,
) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
    let mut left = Vec::new();
    let mut right = Vec::new();
    let mut consumed = 0usize;
    for span in spans {
        let len = span.content.chars().count();
        if consumed >= col {
            right.push(span.clone());
        } else if consumed + len <= col {
            left.push(span.clone());
            consumed += len;
        } else {
            let at = col - consumed;
            let head: String = span.content.chars().take(at).collect();
            let tail: String = span.content.chars().skip(at).collect();
            left.push(Span::styled(head, span.style));
            right.push(Span::styled(tail, span.style));
            consumed = col;
        }
    }
    (left, right)
}

/// Join the selected cells of `rows` (the live painted `Vec<Line>` of the active
/// surface) into clipboard text, reusing the Phase 5a copy-text cleaning.
///
/// For each row in [`Selection::row_span`]:
/// 1. project the line to plain text ([`plain_text_of_line`]),
/// 2. clean it with [`strip_gutter`] (which also drops a leading message-prompt
///    marker), and compute how many leading chars that dropped,
/// 3. intersect the selected column span with the post-strip content so a
///    partial selection that *begins inside the gutter* still pastes from the
///    first content char,
/// 4. slice the CLEANED text by char offset and push it; rows join with `\n`.
///
/// Trailing blank lines are trimmed exactly like `copy::format_plain`, so a
/// full-row selection produces the byte-for-byte payload the bulk-copy path
/// would.
pub(crate) fn selection_clean_text(rows: &[Line<'static>], sel: &Selection) -> String {
    let mut out: Vec<String> = Vec::new();
    for row in sel.row_span() {
        let Some(line) = rows.get(row) else {
            continue;
        };
        let plain = plain_text_of_line(line);
        let full_len = plain.chars().count();
        let Some(span) = sel.col_span_for_row(row, full_len) else {
            continue;
        };

        // Clean the WHOLE line (strip_gutter measures the rail against the full
        // line), then re-intersect the selected column span with the surviving
        // content: the gutter occupies `gutter_chars` leading chars.
        let cleaned = strip_gutter(&plain);
        let cleaned_len = cleaned.chars().count();
        let gutter_chars = full_len.saturating_sub(cleaned_len);

        // Shift the selection into the cleaned text's char space, clamping the
        // start up past the stripped gutter so a selection that begins inside
        // the chrome still starts at the first content char.
        // (`x.max(g).saturating_sub(g)` == `x.saturating_sub(g)` for all `x`, so
        // the saturating subtraction alone does the clamp.)
        let lo = span.start.saturating_sub(gutter_chars);
        let hi = span.end.saturating_sub(gutter_chars);
        let lo = lo.min(cleaned_len);
        let hi = hi.min(cleaned_len).max(lo);

        let slice: String = cleaned.chars().skip(lo).take(hi - lo).collect();
        out.push(slice);
    }
    out.join("\n").trim_end().to_string()
}

#[cfg(test)]
#[path = "selection_tests.rs"]
mod tests;
