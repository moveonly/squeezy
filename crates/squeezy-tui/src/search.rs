//! Incremental SEARCH model for the always-on transcript renderer.
//!
//! This is the *third* consumer of the per-surface painted-row pattern, beside
//! [`crate::selection`] and [`crate::transcript_surface`]. Like
//! [`crate::selection`] it is a **pure model**: it owns the live search session
//! (the query, the ordered match positions, the current index, the
//! include/exclude toggles) and the find pass that scans painted rows, but it
//! performs no rendering and touches no `app` state. `lib.rs` owns the
//! `Option<SearchState>` field on `TuiApp`, the key dispatch, the per-frame
//! painted-row plumbing, and the scroll-into-view.
//!
//! ## Why search on the painted `Vec<Line>`, not the row model
//!
//! The main view and the Ctrl+T overlay draw from **different** wrapped
//! `Vec<Line>` sources, and the shared [`crate::transcript_surface`] row model
//! mirrors only the overlay. So a match that must align byte-for-byte with the
//! painted glyphs cannot route through that model — exactly the trap
//! [`crate::selection`] documents avoiding. Instead a [`Match`] is
//! *surface-local*: `row` indexes the active surface's own `Vec<Line>` and `col`
//! is a **char-offset range** into that line's joined plain text, the same basis
//! [`crate::selection::Selection::col_span_for_row`] yields, so a match feeds
//! [`crate::selection::rows_with_selection_highlight`]'s machinery directly.
//!
//! ## Column offsets are in the FULL painted line's char space
//!
//! The find pass strips the rail/gutter (and any role marker) with
//! [`crate::transcript_surface::strip_gutter`] so it only matches real content,
//! then re-adds the gutter char shift so the reported column range indexes the
//! **full painted line** — the same inversion
//! [`crate::selection::selection_clean_text`] performs. This keeps a search
//! highlight landing on exactly the drawn cells.
//!
//! Until `lib.rs` wires it up the whole surface is dead in a plain
//! `cargo build`, so the module-level `allow(dead_code)` keeps `-D warnings`
//! green, mirroring [`crate::selection`].
#![allow(dead_code)]

use std::ops::Range;

use ratatui::text::Line;

use crate::selection::SelectionSurface;
use crate::transcript_surface::{plain_text_of_line, strip_gutter};

/// Coarse classification of a painted row, supplied by the caller per row so the
/// find pass can honour the include/exclude toggles without `search.rs` knowing
/// transcript semantics. `lib.rs` builds a `Vec<RowKind>` parallel to the
/// painted `Vec<Line>` of the active surface. When both toggles are `true` (the
/// default) classification is irrelevant and every row is scanned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowKind {
    /// Ordinary message / chrome content — always searched.
    Normal,
    /// A row that came from tool output. Skipped when `include_tool_output` is
    /// false.
    ToolOutput,
    /// A row that came from reasoning. Skipped when `include_reasoning` is false.
    Reasoning,
}

/// A single search hit: the surface-local wrapped-row index plus the
/// **char-offset column range** on that row, the same `(row, col-range)` basis
/// [`crate::selection::Selection::col_span_for_row`] yields so it feeds the
/// selection highlight machinery directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Match {
    pub(crate) row: usize,
    pub(crate) col: Range<usize>,
}

/// The live incremental-search session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SearchState {
    /// The incremental query as typed.
    pub(crate) query: String,
    /// The surface the current match set was computed against. Captured at open;
    /// if the active surface changes while search is live, `lib.rs` updates this
    /// and re-runs the find pass.
    pub(crate) surface: SelectionSurface,
    /// The painted width the live `matches` were computed at. Recorded on every
    /// [`rebuild`] (not just at open), so it always reflects the width the
    /// current positions index into. The resize handler re-runs the pass
    /// unconditionally (`lib.rs` `Event::Resize`), so this is the source of
    /// truth for "what width are the matches anchored to" rather than a stored
    /// short-circuit trigger.
    pub(crate) width: u16,
    /// Ordered match positions in reading order (row-major, then col).
    pub(crate) matches: Vec<Match>,
    /// Index into `matches` of the active match; `None` when `matches` is empty.
    pub(crate) current: Option<usize>,
    /// Include rows classified as tool output. Default `true`.
    pub(crate) include_tool_output: bool,
    /// Include rows classified as reasoning. Default `true`.
    pub(crate) include_reasoning: bool,
}

impl SearchState {
    /// Open a fresh search session on `surface` at painted `width`, with an empty
    /// query and both include toggles on. `lib.rs` runs the first
    /// [`rebuild`] against the live painted rows immediately after.
    pub(crate) fn open(surface: SelectionSurface, width: u16) -> Self {
        Self {
            query: String::new(),
            surface,
            width,
            matches: Vec::new(),
            current: None,
            include_tool_output: true,
            include_reasoning: true,
        }
    }
}

/// Whether a row of the given [`RowKind`] is searched under the include toggles.
fn row_included(kind: RowKind, include_tool_output: bool, include_reasoning: bool) -> bool {
    match kind {
        RowKind::Normal => true,
        RowKind::ToolOutput => include_tool_output,
        RowKind::Reasoning => include_reasoning,
    }
}

/// The find pass.
///
/// For each row in `rows`: skip rows whose [`RowKind`] is excluded by the
/// toggles; project to plain text; strip the gutter so only real content is
/// scanned; case-insensitively find every non-overlapping occurrence of `query`;
/// re-add the gutter char shift so the reported `col` indexes the **full painted
/// line**. Matches come back in reading order (row-major, then col). An empty
/// `query` yields `[]`.
///
/// `kinds` is indexed in lock-step with `rows`; a row past the end of `kinds` is
/// treated as [`RowKind::Normal`] (always searched), so a short/empty `kinds`
/// degrades to "search everything" rather than dropping rows.
/// Offset-stable case fold: one source char in, exactly one lowercase char out.
///
/// `char::to_lowercase` can expand a char to several chars (e.g. 'İ' U+0130 ->
/// "i\u{307}"), which would desync the char offsets the search column math
/// relies on. This takes the FIRST char of the lowercase expansion, which is
/// always defined and keeps the 1:1 char-to-column mapping while still folding
/// non-ASCII case (unlike a bare `to_ascii_lowercase`). For the overwhelming
/// 1:1 majority of chars this is identical to `to_lowercase`.
fn simple_lower(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

pub(crate) fn find(
    rows: &[Line<'static>],
    kinds: &[RowKind],
    query: &str,
    include_tool_output: bool,
    include_reasoning: bool,
) -> Vec<Match> {
    let mut out = Vec::new();
    if query.is_empty() {
        return out;
    }
    let needle: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    if needle.is_empty() {
        return out;
    }

    for (row_idx, line) in rows.iter().enumerate() {
        let kind = kinds.get(row_idx).copied().unwrap_or(RowKind::Normal);
        if !row_included(kind, include_tool_output, include_reasoning) {
            continue;
        }

        let plain = plain_text_of_line(line);
        let full_len = plain.chars().count();
        let cleaned = strip_gutter(&plain);
        // Number of leading chars the gutter/marker strip dropped, used to shift
        // a match found in the cleaned text back into the full painted line's
        // char space (the same inversion `selection_clean_text` performs).
        let gutter_chars = full_len.saturating_sub(cleaned.chars().count());

        // Lowercase the haystack into a `Vec<char>` so column offsets are char
        // offsets (not byte offsets) and the case-insensitive scan compares
        // char-for-char with `needle`.
        let hay: Vec<char> = cleaned.chars().flat_map(char::to_lowercase).collect();
        // `char::to_lowercase` can expand one char into several (e.g. 'İ'
        // U+0130 -> "i\u{307}"), which would desync char offsets from the
        // original cleaned text and corrupt the reported column ranges. Guard
        // against that: the offset-stable common case (1:1 lowering for every
        // char in the line) is used directly; otherwise fall back to an
        // offset-preserving simple fold — `simple_lower`, which maps each
        // source char to exactly one lowercase char (the first char of its
        // expansion). That keeps the char-for-char 1:1 mapping the column math
        // needs WHILE preserving Unicode case-insensitivity (e.g. "É" still
        // matches "é"), rather than the old ASCII-only downgrade that silently
        // dropped non-ASCII folding for the whole line. The decision is symmetric
        // in haystack and needle: an expanding char on EITHER side desyncs the
        // pair, so both are routed through `simple_lower` together to stay
        // comparable and offset-stable regardless of which side expands.
        let needle_offset_stable = needle.len() == query.chars().count();
        let (hay, needle) = if hay.len() == cleaned.chars().count() && needle_offset_stable {
            (hay, needle.clone())
        } else {
            (
                cleaned.chars().map(simple_lower).collect::<Vec<char>>(),
                query.chars().map(simple_lower).collect::<Vec<char>>(),
            )
        };

        let mut start = 0usize;
        while start + needle.len() <= hay.len() {
            if hay[start..start + needle.len()] == needle[..] {
                let lo = start + gutter_chars;
                let hi = lo + needle.len();
                out.push(Match {
                    row: row_idx,
                    col: lo..hi,
                });
                // Non-overlapping: advance past this whole match.
                start += needle.len();
            } else {
                start += 1;
            }
        }
    }
    out
}

/// Re-run [`find`] for the current query/toggles, then **preserve the current
/// match as well as possible**: keep `current` pointing at the match nearest (by
/// `(row, col.start)`) to the previously-current one, so an incremental
/// keystroke / resize / surface switch does not jolt the selected match.
pub(crate) fn rebuild(
    state: &mut SearchState,
    rows: &[Line<'static>],
    kinds: &[RowKind],
    width: u16,
) {
    // Record the width the new positions are computed at so `state.width`
    // always reflects the geometry the live `matches` index into.
    state.width = width;
    let previous = state.current.and_then(|i| state.matches.get(i)).cloned();
    state.matches = find(
        rows,
        kinds,
        &state.query,
        state.include_tool_output,
        state.include_reasoning,
    );
    state.current = if state.matches.is_empty() {
        None
    } else if let Some(prev) = previous {
        // Nearest match to where we were, in (row, col.start) reading order.
        let target = (prev.row, prev.col.start);
        let idx = state
            .matches
            .iter()
            .enumerate()
            .min_by_key(|(_, m)| {
                let here = (m.row, m.col.start);
                if here >= target {
                    (0u8, distance(here, target))
                } else {
                    (1u8, distance(here, target))
                }
            })
            .map(|(i, _)| i)
            .unwrap_or(0);
        Some(idx)
    } else {
        Some(0)
    };
}

/// Reading-order distance between two `(row, col)` points, used to pick the
/// nearest surviving match on rebuild. Row distance dominates (scaled) so a hit
/// on the same row is always preferred to one on an adjacent row.
fn distance(a: (usize, usize), b: (usize, usize)) -> usize {
    let row_delta = a.0.abs_diff(b.0);
    let col_delta = a.1.abs_diff(b.1);
    row_delta
        .saturating_mul(1_000_000)
        .saturating_add(col_delta)
}

/// Append `c` to the query. Leaves `matches`/`current` stale for `lib.rs` to
/// refresh via [`rebuild`] against fresh painted rows the same frame.
pub(crate) fn push_char(state: &mut SearchState, c: char) {
    state.query.push(c);
}

/// Drop the last char of the query. Leaves `matches`/`current` stale for
/// `lib.rs` to refresh via [`rebuild`]. An empty query after the pop keeps the
/// session open with no matches (the caller does not auto-close).
pub(crate) fn pop_char(state: &mut SearchState) {
    state.query.pop();
}

/// Advance `current` to the next match, wrapping past the end. No-op when there
/// are no matches.
pub(crate) fn next(state: &mut SearchState) {
    if state.matches.is_empty() {
        state.current = None;
        return;
    }
    let len = state.matches.len();
    state.current = Some(match state.current {
        Some(i) => (i + 1) % len,
        None => 0,
    });
}

/// Retreat `current` to the previous match, wrapping past the start. No-op when
/// there are no matches.
pub(crate) fn prev(state: &mut SearchState) {
    if state.matches.is_empty() {
        state.current = None;
        return;
    }
    let len = state.matches.len();
    state.current = Some(match state.current {
        Some(0) | None => len - 1,
        Some(i) => i - 1,
    });
}

/// The active match, for the scroll-into-view + status (`"3/17"`) computation in
/// `lib.rs`.
pub(crate) fn current_match(state: &SearchState) -> Option<&Match> {
    state.current.and_then(|i| state.matches.get(i))
}

/// Yield `(row, col_range, is_current)` for every match, so the renderer can
/// paint all matches in one style and the current match in an accent style. The
/// bridge into the selection highlight machinery.
pub(crate) fn match_ranges_by_row(
    state: &SearchState,
) -> impl Iterator<Item = (usize, Range<usize>, bool)> + '_ {
    state
        .matches
        .iter()
        .enumerate()
        .map(move |(i, m)| (m.row, m.col.clone(), state.current == Some(i)))
}

#[cfg(test)]
#[path = "search_tests.rs"]
mod tests;
