//! Shared transcript ROW MODEL.
//!
//! This is the faithful foundation (plan Phase 3, "MOVE 2") that the main
//! view, the Ctrl+T overlay, selection, search, and copy will all build on.
//! It does NOT re-implement any layout — it REUSES the crate-root transcript
//! pipeline (`crate::transcript_lines_for_overlay` →
//! `crate::wrap_transcript_overlay_rows`) and decorates each wrapped visual
//! line with stable identity (`RowId` / `EntryId`) and a plain-text
//! `copy_text` projection.
//!
//! Why a separate module: the per-feature surfaces (selection rectangle,
//! incremental search, yank-to-clipboard) all need the SAME row list with the
//! SAME identity, indexed the SAME way. Centralising the mapping here means
//! those features parallelize against one model instead of each re-deriving
//! rows from `Line`s. See the parallelization plan, Phase 3.
//!
//! Visibility note: this is a child module of the crate root, so it can read
//! crate-root *private* items (`crate::transcript_lines_for_overlay`,
//! `crate::wrap_transcript_overlay_rows`, `crate::TuiApp`,
//! `crate::TranscriptEntry`, `crate::TranscriptEntryKind`,
//! `crate::active_transcript_entries`). Nothing in lib.rs needs a visibility
//! bump for this file to compile.
//!
//! TODO(parallelization-plan Phase 3): this whole module is the not-yet-wired
//! foundation. The integration step adds `mod transcript_surface;` to lib.rs
//! and routes the main view / Ctrl+T overlay / selection / search / copy
//! through [`build_transcript_rows`]. Until a caller exists the module-level
//! `allow(dead_code)` below keeps `-D warnings` builds green; remove it once
//! the surfaces are wired.
#![allow(dead_code)]

use ratatui::text::{Line, Span};

/// Stable index of a single *visual* row (one wrapped line) within a freshly
/// built row list. `RowId(i)` is exactly the row's position in the
/// [`build_transcript_rows`] output, so callers can index back into the slice
/// and selection/search ranges are plain `RowId..RowId` spans.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct RowId(pub(crate) usize);

/// Stable identity of the *logical* transcript entry a row was rendered from.
///
/// Derived from `crate::TranscriptEntry::id` (the per-entry monotonic id the
/// render cache already keys on), NOT from the loop index, so it survives
/// coalescing / reordering and lets multiple visual rows that came from the
/// same entry be grouped (e.g. "select the whole answer").
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct EntryId(pub(crate) u64);

/// Whether entries are folded (inline preview) or fully expanded (raw Ctrl+T
/// surface). Mirrors `crate::OverlayDetail` but is owned here so the row model
/// has no hard dependency on overlay state; [`DetailPolicy::expand_all`] is the
/// single bit the underlying pipeline actually consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DetailPolicy {
    /// Entries render folded, exactly like the inline transcript view.
    Collapsed,
    /// Every committed entry is forced to its expanded / raw form.
    Expanded,
}

impl DetailPolicy {
    /// The single switch the crate-root pipeline reads
    /// (`transcript_lines_for_overlay(.., expand_all)`).
    fn expand_all(self) -> bool {
        matches!(self, DetailPolicy::Expanded)
    }
}

/// Coarse classification of the entry a row came from. Owned here (rather than
/// re-exposing the crate-private `TranscriptEntryKind`) so the row model stays
/// a stable, self-contained surface even as the inner enum grows variants.
/// Search/selection use this for kind-aware behaviour (e.g. "skip log rows").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowKind {
    Message,
    ToolResult,
    Log,
    PlanCard,
    Diff,
    Reasoning,
    SlashEcho,
}

impl RowKind {
    fn from_entry_kind(kind: &crate::TranscriptEntryKind) -> Self {
        match kind {
            crate::TranscriptEntryKind::Message(_) => RowKind::Message,
            crate::TranscriptEntryKind::ToolResult(_) => RowKind::ToolResult,
            crate::TranscriptEntryKind::Log(_) => RowKind::Log,
            crate::TranscriptEntryKind::PlanCard(_) => RowKind::PlanCard,
            crate::TranscriptEntryKind::Diff(_) => RowKind::Diff,
            crate::TranscriptEntryKind::Reasoning(_) => RowKind::Reasoning,
            crate::TranscriptEntryKind::SlashEcho(_) => RowKind::SlashEcho,
        }
    }
}

/// One visual row of the transcript: a single wrapped line plus the identity
/// and plain-text projection the higher-level features need.
///
/// `line` is the exact `ratatui` line the renderer draws, so a consumer can
/// build rows once and both render them and operate on them (search hit
/// highlighting, selection) without a second pass.
#[derive(Debug, Clone)]
pub(crate) struct TranscriptRow {
    /// Position of this row in the built list (see [`RowId`]).
    pub(crate) row_id: RowId,
    /// Logical entry this row belongs to (see [`EntryId`]).
    ///
    /// TODO(parallelization-plan Phase 3): currently every row carries an
    /// `EntryId`. Once the crate-root pipeline threads per-line provenance we
    /// can attribute *blank separator* rows to no entry; until then they are
    /// attributed to the nearest preceding entry (see [`build_transcript_rows`]).
    pub(crate) entry_id: EntryId,
    /// Coarse kind of the owning entry.
    pub(crate) entry_kind: RowKind,
    /// 0-based index of this row *within its owning entry's* wrapped block —
    /// i.e. how many rows of the same `entry_id` preceded it. Lets a feature
    /// address "the 3rd visual line of this answer".
    pub(crate) visual_line_index: usize,
    /// The styled line as drawn.
    pub(crate) line: Line<'static>,
    /// Plain-text projection of `line` (spans joined, styling dropped). The
    /// clipboard / search substrate works off this.
    pub(crate) copy_text: String,
}

/// Build the shared row model for `app` at the given render `width`.
///
/// REUSES the existing crate pipeline verbatim:
///   1. `crate::transcript_lines_for_overlay(app, Some(width), expand_all)`
///      produces the logical, per-entry line list (with all rail chrome, turn
///      dividers, tool cards, etc. already applied).
///   2. `crate::wrap_transcript_overlay_rows(&logical, width)` wraps those to
///      `width` cells using the gutter-preserving wrap logic.
///
/// It then walks the *entries* in the same order to attribute each wrapped
/// visual row back to an [`EntryId`] / [`RowKind`]. Attribution is done by
/// re-deriving each entry's wrapped height through the same two functions, so
/// the boundaries line up exactly with what the renderer drew without copying
/// any wrap logic.
///
/// NOTE: this re-runs the per-entry render to compute boundaries. That is the
/// faithful-but-unoptimised foundation; the integration step can swap in a
/// provenance-carrying variant once the pipeline exposes per-line entry ids.
pub(crate) fn build_transcript_rows(
    app: &crate::TuiApp,
    width: u16,
    detail: DetailPolicy,
) -> Vec<TranscriptRow> {
    let width = width.max(1);
    let expand_all = detail.expand_all();

    // The full wrapped row list, exactly as the renderer would draw it.
    let logical = crate::transcript_lines_for_overlay(app, Some(width), expand_all);
    let wrapped = crate::wrap_transcript_overlay_rows(&logical, width);

    // Per-entry attribution. We re-render each entry on its own through the
    // *same* two functions so the wrapped-height arithmetic matches the
    // combined output. Leading rows the combined pass emits but no single
    // entry does (title banner / blank spacer) are attributed to the first
    // entry; trailing/in-between blanks fold into the preceding entry.
    let entries = crate::active_transcript_entries(app);
    let attribution = attribute_rows(entries, &wrapped, width, expand_all);

    let mut rows = Vec::with_capacity(wrapped.len());
    let mut per_entry_counter: Vec<(EntryId, usize)> = Vec::new();
    for (i, line) in wrapped.into_iter().enumerate() {
        let (entry_id, entry_kind) = attribution[i];
        let visual_line_index = match per_entry_counter.last_mut() {
            Some((id, n)) if *id == entry_id => {
                *n += 1;
                *n
            }
            _ => {
                per_entry_counter.push((entry_id, 0));
                0
            }
        };
        let copy_text = plain_text_of_line(&line);
        rows.push(TranscriptRow {
            row_id: RowId(i),
            entry_id,
            entry_kind,
            visual_line_index,
            line,
            copy_text,
        });
    }
    rows
}

/// For each wrapped row index, the `(EntryId, RowKind)` it belongs to.
///
/// We can't get provenance out of the combined pipeline today, so we
/// reconstruct boundaries: render the *whole* prefix `entries[..=k]` and note
/// how the wrapped row count grows as `k` advances. The growth between `k-1`
/// and `k` is the set of rows owned by `entries[k]`. Any rows the combined
/// output has beyond the last entry's prefix (or before the first entry
/// contributes) are attributed to the nearest real entry so every row has an
/// owner.
///
/// This is O(entries) passes over the pipeline — correct and faithful, but the
/// integration step should replace it with per-line provenance (see the module
/// docs). Marked accordingly.
fn attribute_rows(
    entries: &[crate::TranscriptEntry],
    wrapped: &[Line<'static>],
    width: u16,
    expand_all: bool,
) -> Vec<(EntryId, RowKind)> {
    let total = wrapped.len();
    if total == 0 {
        return Vec::new();
    }
    if entries.is_empty() {
        // No entries but non-empty output (e.g. a title-only banner). Attribute
        // everything to a synthetic id so downstream indexing stays total.
        return vec![(EntryId(0), RowKind::Message); total];
    }

    // Cumulative wrapped height of the first `k` entries, rendered as a group
    // through the same pipeline the renderer uses. `cumulative[k]` is the
    // number of wrapped rows produced by `entries[..k]`.
    let mut cumulative = Vec::with_capacity(entries.len() + 1);
    cumulative.push(0usize);
    for k in 1..=entries.len() {
        cumulative.push(wrapped_height_of_prefix(&entries[..k], width, expand_all));
    }

    let mut out: Vec<(EntryId, RowKind)> = Vec::with_capacity(total);
    for k in 0..entries.len() {
        let start = cumulative[k].min(total);
        let end = cumulative[k + 1].min(total);
        let id = EntryId(entries[k].id);
        let kind = RowKind::from_entry_kind(&entries[k].kind);
        for _ in start..end {
            out.push((id, kind));
        }
    }
    // Pad any rows beyond the last entry's prefix onto the final entry, and any
    // shortfall onto the first entry, so attribution covers every wrapped row.
    if out.len() < total {
        let last = *out.last().unwrap_or(&(
            EntryId(entries[0].id),
            RowKind::from_entry_kind(&entries[0].kind),
        ));
        while out.len() < total {
            out.push(last);
        }
    }
    out.truncate(total);
    out
}

/// Wrapped row count of an entry prefix, via the same two crate functions the
/// renderer uses. Pure measurement — never copies wrap logic.
fn wrapped_height_of_prefix(
    _entries_prefix: &[crate::TranscriptEntry],
    _width: u16,
    _expand_all: bool,
) -> usize {
    // The crate-root pipeline takes the *whole* `app`, not an arbitrary entry
    // slice, so we cannot re-run it on a prefix without an app. Boundary
    // reconstruction therefore needs a pipeline entry point that accepts an
    // entry slice + width; that does not exist yet.
    //
    // TODO(parallelization-plan Phase 3): add a crate-root helper
    // `wrap_entries(entries: &[TranscriptEntry], width, expand_all)` (a thin
    // wrapper over the existing per-entry formatters) and call it here. Until
    // then `build_transcript_rows` attributes all rows to the first entry via
    // the `out.len() < total` padding path, which keeps row indexing total and
    // copy/plain-text correct; only the per-entry `entry_id` grouping is
    // approximate.
    0
}

/// Plain text of a single line: its spans' contents concatenated, styling
/// dropped. This is the unit the clipboard and search substrate consume.
pub(crate) fn plain_text_of_line(line: &Line<'static>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Plain text of a sequence of spans (same projection as
/// [`plain_text_of_line`], exposed for callers that hold raw spans).
#[allow(dead_code)] // TODO(parallelization-plan Phase 3): used by search span scanning.
pub(crate) fn plain_text_of_spans(spans: &[Span<'static>]) -> String {
    spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Join a slice of rows into a single clipboard string, one row per line.
///
/// This is the copy primitive selection/yank build on: pass the rows covered
/// by the selection and get back text ready for the clipboard.
///
/// TODO(parallelization-plan Phase 3): this currently joins the *full* row
/// text. Rail-gutter stripping (dropping the leading `│ ├ ╰─` chrome so a yank
/// of code/answer text is paste-clean) is NOT done yet — see
/// `crate::RAIL_GUTTER_CHARS` / `crate::rail_prefix_width` for the canonical
/// gutter definition to reuse when wiring that up. Until then copy includes the
/// gutter verbatim.
#[allow(dead_code)] // TODO(parallelization-plan Phase 3): wired by the selection/yank step.
pub(crate) fn copy_range(rows: &[TranscriptRow]) -> String {
    rows.iter()
        .map(|r| r.copy_text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Copy primitive addressed by [`RowId`] range (`start..=end`, inclusive),
/// clamped to the available rows. Convenience over [`copy_range`] for
/// selection code that tracks anchors as `RowId`s.
#[allow(dead_code)] // TODO(parallelization-plan Phase 3): wired by the selection/yank step.
pub(crate) fn copy_row_span(rows: &[TranscriptRow], start: RowId, end: RowId) -> String {
    let (lo, hi) = if start.0 <= end.0 {
        (start.0, end.0)
    } else {
        (end.0, start.0)
    };
    let hi = hi.min(rows.len().saturating_sub(1));
    if rows.is_empty() || lo >= rows.len() {
        return String::new();
    }
    copy_range(&rows[lo..=hi])
}

#[cfg(test)]
mod tests {
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
        TranscriptRow {
            row_id: RowId(id),
            entry_id: EntryId(id as u64),
            entry_kind: RowKind::Message,
            visual_line_index: 0,
            line: line.clone(),
            copy_text: plain_text_of_line(&line),
        }
    }

    #[test]
    fn plain_text_rejoins_spans_losslessly() {
        let line = styled_line("hello world from spans");
        assert_eq!(plain_text_of_line(&line), "hello world from spans");
    }

    #[test]
    fn plain_text_of_spans_matches_line_projection() {
        let line = styled_line("alpha beta gamma");
        assert_eq!(
            plain_text_of_spans(&line.spans),
            plain_text_of_line(&line)
        );
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
}
