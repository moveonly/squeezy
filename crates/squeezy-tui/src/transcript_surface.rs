//! Shared transcript ROW MODEL.
//!
//! This is a faithfully-attributed row model (plan Phase 3, "MOVE 2") that
//! selection, search, and copy will build on. It does NOT re-implement any
//! layout — it REUSES the crate-root transcript pipeline through
//! [`crate::wrap_entries`], which runs the SAME per-entry formatting /
//! coalescing / rail / divider loop as the **overlay** draw and wraps it with
//! the SAME wrapper, additionally tagging each wrapped visual row with the
//! `TranscriptEntry.id` it came from. The row model then decorates each row
//! with stable identity (`RowId` / `EntryId`), a plain-text `copy_text`
//! projection, and per-row style/click metadata.
//!
//! SURFACE SCOPE (important): [`crate::wrap_entries`] today mirrors only
//! [`crate::transcript_lines_for_overlay`] — the Ctrl+T overlay surface. It is
//! NOT yet a faithful model of the MAIN view, which draws through
//! [`crate::transcript_lines_for_render`] and *diverges*: the main path adds
//! the startup card, the settle-fold animation, `Tinted` tool cards, and a
//! different turn divider. Wiring the main view onto this row model (Phase 4)
//! requires parameterizing `wrap_entries` by surface so it can reproduce the
//! render-path differences; until then, treat this module as the overlay row
//! model only.
//!
//! Why a separate module: the per-feature surfaces (selection rectangle,
//! incremental search, yank-to-clipboard) all need the SAME row list with the
//! SAME identity, indexed the SAME way. Centralising the mapping here means
//! those features parallelize against one model instead of each re-deriving
//! rows from `Line`s. See the parallelization plan, Phase 3.
//!
//! Attribution is FAITHFUL: [`crate::wrap_entries`] threads per-line provenance
//! through the combined pass, so every visual row carries the id of the entry
//! that produced it and chrome rows (title banner, blank spacers, rail
//! connectors, turn dividers, the live pending tail) carry no entry id. This
//! replaces the earlier prefix-diffing stub that attributed every row to the
//! first entry.
//!
//! Visibility note: this is a child module of the crate root, so it can read
//! crate-root *private* items (`crate::wrap_entries`, `crate::AttributedRow`,
//! `crate::TuiApp`, `crate::TranscriptEntry`, `crate::TranscriptEntryKind`,
//! `crate::active_transcript_entries`). Nothing in lib.rs needs a visibility
//! bump for this file to compile.
//!
//! Wiring status (parallelization-plan Phase 3): `build_transcript_rows` is now
//! the real, faithfully-attributed builder (it replaced the prefix-diffing
//! stub) and is fully exercised by the row-model test surface in `lib_tests`.
//! The production `render()` path still draws through
//! [`crate::transcript_lines_for_overlay`] (overlay) and
//! [`crate::transcript_lines_for_render`] (main); routing them — plus
//! selection, search, and copy — through this module, and parameterizing
//! `wrap_entries` by surface so it also mirrors the main view, is the Phase 4+
//! integration step. Until a NON-TEST caller exists the whole surface is dead
//! in a plain `cargo build`, so the module-level `allow(dead_code)` below is
//! what keeps `-D warnings` green; narrow it to per-item allows once the
//! renderer consumes the row model.
#![allow(dead_code)]

use std::num::NonZeroUsize;
use std::ops::Range;
use std::sync::{Mutex, OnceLock};

use lru::LruCache;
use ratatui::style::Style;
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

/// Mirror `crate::OverlayDetail` onto the owned [`DetailPolicy`]. The match is
/// deliberately EXHAUSTIVE (no `_` arm): if a new `OverlayDetail` variant is
/// added upstream this stops compiling here, forcing a conscious decision about
/// how the row model should fold/expand for it instead of silently defaulting.
impl From<&crate::OverlayDetail> for DetailPolicy {
    fn from(detail: &crate::OverlayDetail) -> Self {
        match detail {
            crate::OverlayDetail::Collapsed => DetailPolicy::Collapsed,
            crate::OverlayDetail::Expanded => DetailPolicy::Expanded,
        }
    }
}

impl From<crate::OverlayDetail> for DetailPolicy {
    fn from(detail: crate::OverlayDetail) -> Self {
        DetailPolicy::from(&detail)
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

/// Project the crate-private `crate::TranscriptEntryKind` onto the owned
/// [`RowKind`]. The match is deliberately EXHAUSTIVE (no `_` arm): adding a new
/// upstream `TranscriptEntryKind` variant breaks compilation here, forcing the
/// row model to classify it instead of silently mislabelling it. This is the
/// drift guard that keeps [`RowKind`] honest as the inner enum grows.
impl From<&crate::TranscriptEntryKind> for RowKind {
    fn from(kind: &crate::TranscriptEntryKind) -> Self {
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

impl RowKind {
    /// Thin wrapper over the [`From<&crate::TranscriptEntryKind>`] drift guard,
    /// kept for call sites that read more clearly as a named constructor.
    fn from_entry_kind(kind: &crate::TranscriptEntryKind) -> Self {
        RowKind::from(kind)
    }

    /// Stable, machine-readable identifier for this kind. Mirrors the
    /// `keymap::Action::slug` convention so the `JsonSlice` copy format can
    /// emit a fixed `"kind"` tag per entry that scripts can match on. The
    /// strings are part of the copy/export wire shape — do not rename without
    /// a migration note.
    pub(crate) fn slug(self) -> &'static str {
        match self {
            RowKind::Message => "message",
            RowKind::ToolResult => "tool_result",
            RowKind::Log => "log",
            RowKind::PlanCard => "plan_card",
            RowKind::Diff => "diff",
            RowKind::Reasoning => "reasoning",
            RowKind::SlashEcho => "slash_echo",
        }
    }
}

/// Message-prompt marker glyphs a message header may carry before its content:
/// the assistant/user "coin" moon-phase family (`☽ ☾ ◐ ◑ ◔ ◕ ● ○`) and the
/// composer cursor bar (`▌`). A copy of an answer should begin at the first
/// content character, not on the marker — so [`strip_gutter`] drops a leading
/// `<marker> ` (marker plus its single trailing space) once the rail run is
/// removed. Kept narrow on purpose: only these known role bullets are stripped,
/// so ordinary content that happens to start with punctuation is untouched.
const MESSAGE_MARKER_GLYPHS: [char; 9] = ['☽', '☾', '◐', '◑', '◔', '◕', '●', '○', '▌'];

/// Strip a row's leading rail/gutter run (`│ ├ ╰─` and the node marker glyph)
/// AND any leading message-prompt marker (`☽`/`▌`/role bullet) so a copied line
/// begins at the first content character, not the rail chrome or role coin.
/// Reuses the crate-root [`crate::rail_prefix_width`] — the single canonical
/// gutter definition the renderer measures with — so a copy strips exactly what
/// the rail painted. This is the strip the historical [`copy_range`] TODO
/// deferred; the copy formatters in [`crate::copy`] apply it per line.
pub(crate) fn strip_gutter(line: &str) -> &str {
    // A focused entry's header carries a leading selection caret (`">  "` for
    // messages, `"> "` for reasoning/slash echoes — see `assistant_line` /
    // `reasoning_block_lines`). That caret is selection chrome, not content, so
    // a copy/selection must yield the same text whether or not the entry happens
    // to be focused when it is grabbed. Drop it first, before the rail/marker
    // strip. Safe because content rows in this surface never begin with `"> "`
    // at column 0 — body text hangs under a whitespace indent and Markdown
    // blockquotes are rendered on indented continuation rows, not at column 0.
    let line = strip_focus_caret(line);
    let prefix = crate::rail_prefix_width(line);
    // `rail_prefix_width` counts *chars*; advance by that many char boundaries.
    let after_gutter = match line.char_indices().nth(prefix) {
        Some((byte_idx, _)) => &line[byte_idx..],
        None if prefix == 0 => line,
        None => "",
    };
    strip_message_marker(after_gutter)
}

/// Drop a leading focus/selection caret (`›` plus its trailing space run) when a
/// line opens on one. The renderer paints `"›  "` (message header) or `"› "`
/// (reasoning / slash echo) at column 0 of the *focused* entry's header; this
/// removes exactly that so the cleaned text is focus-invariant. A no-op when the
/// line does not open on `›` immediately followed by a space — and, because the
/// caret is a distinct glyph from a Markdown blockquote's ASCII `>`, a blockquote
/// is never mistaken for it.
fn strip_focus_caret(line: &str) -> &str {
    let mut chars = line.char_indices();
    let Some((_, '\u{203a}')) = chars.next() else {
        return line;
    };
    // Require at least one space after the caret so a stray `›` that begins real
    // content (no following space) is never eaten. `space_idx` is the byte offset
    // of that space — a char boundary, so the slice stays UTF-8-safe past the
    // multi-byte caret.
    match chars.next() {
        Some((space_idx, ' ')) => line[space_idx..].trim_start_matches(' '),
        _ => line,
    }
}

/// Drop a leading message-prompt marker glyph and its single following space
/// (`"☽ answer"` → `"answer"`). A no-op when the line does not open on a known
/// marker, so non-message content is preserved verbatim.
fn strip_message_marker(line: &str) -> &str {
    let mut chars = line.char_indices();
    let Some((_, first)) = chars.next() else {
        return line;
    };
    if !MESSAGE_MARKER_GLYPHS.contains(&first) {
        return line;
    }
    match chars.next() {
        // Marker followed by a space: drop both.
        Some((idx, ' ')) => &line[idx + 1..],
        // Marker immediately followed by content (no space): drop just the
        // marker, keeping the content from the second char on.
        Some((idx, _)) => &line[idx..],
        // Marker at end of line (it was the only/last char): nothing remains.
        None => "",
    }
}

/// One visual row of the transcript: a single wrapped line plus the identity,
/// plain-text projection, style metadata, and interaction state the
/// higher-level features need.
///
/// `line` is the exact `ratatui` line the renderer draws, so a consumer can
/// build rows once and both render them and operate on them (search hit
/// highlighting, selection) without a second pass. `copy_text`, `text_range`,
/// `style_spans`, and `search_match_ranges` all index by the SAME char offsets,
/// so a search hit found in `copy_text` maps straight onto a re-style of the
/// matching cells.
#[derive(Debug, Clone)]
pub(crate) struct TranscriptRow {
    /// Position of this row in the built list (see [`RowId`]).
    pub(crate) row_id: RowId,
    /// Logical entry this row belongs to (see [`EntryId`]).
    ///
    /// `None` for chrome rows owned by no single entry: the title banner and
    /// its blank spacer, inter-node rail connectors, turn dividers, and the
    /// live pending-assistant tail. This is FAITHFUL provenance threaded by
    /// [`crate::wrap_entries`], not an approximation.
    pub(crate) entry_id: Option<EntryId>,
    /// Coarse kind of the owning entry, or `None` for chrome rows.
    pub(crate) entry_kind: Option<RowKind>,
    /// 0-based index of this row *within its owning entry's* wrapped block —
    /// i.e. how many rows of the same `entry_id` preceded it. Lets a feature
    /// address "the 3rd visual line of this answer". For chrome rows this is
    /// the run-length position within the current chrome run.
    pub(crate) visual_line_index: usize,
    /// The styled line as drawn.
    pub(crate) line: Line<'static>,
    /// Plain-text projection of `line` (spans joined, styling dropped). The
    /// clipboard / search substrate works off this.
    pub(crate) copy_text: String,
    /// Half-open char-offset span of this row within `copy_text`. Always
    /// `0..copy_text.chars().count()` (one row == one line of text); carried
    /// explicitly so selection/search can address sub-row ranges uniformly
    /// with the same offset basis the other fields use.
    pub(crate) text_range: Range<usize>,
    /// Per-cell style runs derived from `line.spans`: `(char_range, style)`
    /// over `copy_text` char offsets. Lets search highlighting re-style a hit
    /// without re-walking the spans, and lets selection invert a sub-row range.
    pub(crate) style_spans: Vec<(Range<usize>, Style)>,
    /// Whether the owning entry is folded (collapsed preview) or expanded.
    /// Chrome rows inherit the build's [`DetailPolicy`].
    pub(crate) fold_state: FoldState,
    /// Char-offset ranges (within `copy_text`) of incremental-search matches on
    /// this row. Default empty.
    ///
    /// TODO(parallelization-plan Phase 7): populated by the incremental-search
    /// step, which scans `copy_text` for the active query and records hit
    /// ranges here so the renderer highlights them in place.
    pub(crate) search_match_ranges: Vec<Range<usize>>,
}

/// Whether a row's owning entry renders folded or expanded. Mirrors the build's
/// [`DetailPolicy`] one-to-one; carried per row so a future per-entry fold
/// (where some entries expand while others stay folded) can vary it without a
/// struct change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FoldState {
    Collapsed,
    Expanded,
}

impl From<DetailPolicy> for FoldState {
    fn from(detail: DetailPolicy) -> Self {
        match detail {
            DetailPolicy::Collapsed => FoldState::Collapsed,
            DetailPolicy::Expanded => FoldState::Expanded,
        }
    }
}

/// Build the shared row model for `app` at the given render `width`.
///
/// REUSES the crate pipeline verbatim through [`crate::wrap_entries`], which
/// runs the SAME per-entry formatting / coalescing / rail / divider loop as the
/// overlay draw and wraps it with the SAME wrapper, additionally tagging every
/// wrapped row with the `TranscriptEntry.id` it came from (or `None` for
/// chrome). This function decorates each tagged row with stable identity, the
/// plain-text / style projections, and interaction state.
///
/// Attribution is FAITHFUL: each row's `entry_id` is the id `wrap_entries`
/// threaded through provenance, so a long answer's wrapped continuation rows
/// share one id while chrome rows carry none. There is no prefix-diffing and no
/// "attribute everything to the first entry" fallback.
///
/// Memoised: the built row list is cached under a composite of every input that
/// affects it (see [`row_cache`]); an unchanged transcript redraws from cache
/// without re-running `wrap_entries`.
pub(crate) fn build_transcript_rows(
    app: &crate::TuiApp,
    width: u16,
    detail: DetailPolicy,
) -> Vec<TranscriptRow> {
    build_transcript_rows_filtered(app, width, detail, crate::OverlayFilter::All)
}

/// As [`build_transcript_rows`], but with an explicit overlay entry `filter`.
/// The main surface always passes `All`; the Ctrl+T overlay passes its active
/// filter so the row model selection/search/copy index into matches the painted
/// (filtered) overlay rows exactly.
pub(crate) fn build_transcript_rows_filtered(
    app: &crate::TuiApp,
    width: u16,
    detail: DetailPolicy,
    filter: crate::OverlayFilter,
) -> Vec<TranscriptRow> {
    let width = width.max(1);
    let key = row_cache_key(app, width, detail, filter);
    if let Some(cached) = row_cache_get(&key) {
        return cached;
    }
    let rows = build_transcript_rows_uncached(app, width, detail, filter);
    row_cache_put(key, rows.clone());
    rows
}

/// Build the row model without consulting the cache. Split out so the cache
/// wrapper stays tiny and tests can exercise the build directly.
fn build_transcript_rows_uncached(
    app: &crate::TuiApp,
    width: u16,
    detail: DetailPolicy,
    filter: crate::OverlayFilter,
) -> Vec<TranscriptRow> {
    let width = width.max(1);
    let expand_all = detail.expand_all();
    let fold_state = FoldState::from(detail);

    // Faithfully attributed, width-wrapped rows — byte-identical lines to the
    // overlay draw, each tagged with its owning entry id (or `None`).
    let attributed = crate::wrap_entries(app, width, expand_all, filter);

    // Map entry ids to their `RowKind` once so attribution is O(1) per row.
    let entries = crate::active_transcript_entries(app);

    let mut rows = Vec::with_capacity(attributed.len());
    // Run-length counter over consecutive equal owners (entry id or chrome),
    // giving each row its index within the current run.
    let mut run: Option<(Option<u64>, usize)> = None;
    for (i, attr) in attributed.into_iter().enumerate() {
        let entry_id = attr.entry_id.map(EntryId);
        let entry_kind = attr
            .entry_id
            .and_then(|id| entries.iter().find(|e| e.id == id))
            .map(|e| RowKind::from_entry_kind(&e.kind));
        let visual_line_index = match run.as_mut() {
            Some((owner, n)) if *owner == attr.entry_id => {
                *n += 1;
                *n
            }
            _ => {
                run = Some((attr.entry_id, 0));
                0
            }
        };
        let copy_text = plain_text_of_line(&attr.line);
        let char_len = copy_text.chars().count();
        let style_spans = style_spans_of_line(&attr.line);
        rows.push(TranscriptRow {
            row_id: RowId(i),
            entry_id,
            entry_kind,
            visual_line_index,
            line: attr.line,
            copy_text,
            text_range: 0..char_len,
            style_spans,
            fold_state,
            // TODO(parallelization-plan Phase 7): incremental search fills this.
            search_match_ranges: Vec::new(),
        });
    }
    rows
}

/// The slice of `rows` visible in a `viewport_height`-tall viewport.
///
/// When `from_bottom` is `true` the viewport is anchored to the END of the
/// transcript (the live-tail / "scrolled to bottom" case): the last
/// `viewport_height` rows are returned. When `false` it is anchored to the
/// TOP (the title-banner / "scrolled to top" case): the first `viewport_height`
/// rows are returned. A `viewport_height` of 0 yields an empty slice, and a
/// viewport taller than the row list yields the whole list. This is the single
/// projection selection/search/render share so the visible window is computed
/// the same way everywhere.
pub(crate) fn visible_transcript_rows(
    rows: &[TranscriptRow],
    viewport_height: usize,
    from_bottom: bool,
) -> &[TranscriptRow] {
    if viewport_height == 0 || rows.is_empty() {
        return &[];
    }
    if viewport_height >= rows.len() {
        return rows;
    }
    if from_bottom {
        &rows[rows.len() - viewport_height..]
    } else {
        &rows[..viewport_height]
    }
}

/// Per-cell style runs of a line over `copy_text` char offsets:
/// `(char_range, style)` for each span, in order. Re-uses the line's own spans
/// (no re-styling), so `style_spans` and `copy_text` always agree on offsets.
fn style_spans_of_line(line: &Line<'static>) -> Vec<(Range<usize>, Style)> {
    let mut out = Vec::with_capacity(line.spans.len());
    let mut offset = 0usize;
    for span in &line.spans {
        let len = span.content.chars().count();
        if len > 0 {
            out.push((offset..offset + len, span.style));
            offset += len;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Revision-keyed row cache
// ---------------------------------------------------------------------------
//
// `build_transcript_rows` runs on every redraw (resize, scroll, key, async
// event), but `wrap_entries` + per-row decoration only change when an input to
// the layout changes. This cache mirrors the per-entry render cache's
// invalidation model (`render::cache`): a key that pins the structural inputs,
// validity tags that capture the volatile ones, and an LRU bound.
//
// Granularity note: the design's ideal is a per-entry block cache, but
// `wrap_entries` necessarily takes the whole app because coalescing, the rail
// connector injection, and the turn divider all depend on cross-entry state, so
// no single entry's rows can be produced in isolation today. The faithful unit
// we *can* memoise is therefore the whole attributed row list, keyed on the
// composite the `wrap_entries` design lists: the session, width, expand_all,
// palette generation, selected entry, the three formatting toggles
// (`tool_output_verbosity`, `show_reasoning_usage`, `coalesce_tool_runs`), the
// transcript shortcut, the turn-divider snapshot, plus a fold over each visible
// `(entry.id, entry.revision)` and the pending stream. Every one of these is an
// input to `wrap_entries`; a change to any flips the key and rebuilds. Because
// the per-entry render LRU is hit *inside* `wrap_entries`, a rebuild on a single
// entry's revision bump only re-runs the cheap cross-entry assembly + wrap, not
// the markdown/tree-sitter formatting.

/// Bound on distinct cached row lists. One slot per (width, detail, toggle)
/// combination of the live transcript; a handful of resizes / mode flips fit
/// comfortably while the LRU stops an animating transcript from accumulating
/// stale lists without bound.
const ROW_CACHE_CAPACITY: usize = 32;

/// Structural + validity composite for one cached row list. Derives `Hash`/`Eq`
/// so the whole key participates in the LRU lookup — there are no separate
/// "validity tags" because, unlike the per-entry cache, there is no stable
/// sub-key to preserve a slot across an input change (the row list is rebuilt
/// wholesale anyway).
#[derive(Clone, PartialEq, Eq, Hash)]
struct RowCacheKey {
    render_cache_session: u64,
    width: u16,
    expand_all: bool,
    /// Active overlay entry filter (`All` for the main surface). Part of the key
    /// so a filter cycle rebuilds the row list instead of serving the prior set.
    filter: crate::OverlayFilter,
    palette_generation: u64,
    selected_entry: Option<usize>,
    tool_output_verbosity: u8,
    show_reasoning_usage: bool,
    coalesce_tool_runs: bool,
    /// Hash of the active conversation source (main vs. a specific subagent),
    /// from `subagent_discriminator` — a discriminator, not a boolean flag.
    subagent_source_hash: u64,
    /// FNV/Default fold over every visible `(entry.id, entry.revision)`.
    transcript_revision_hash: u64,
    /// Fold over the live pending reasoning + assistant stream (no entry id /
    /// revision exists for uncommitted text, so it is content-hashed).
    pending_hash: u64,
    /// Turn-divider animation snapshot, folded into a `u64` (it is `Hash`).
    turn_divider_hash: u64,
    /// `shortcut_hash` of the transcript shortcut so a keymap rebind busts the
    /// cache, matching the per-entry render cache's behaviour.
    shortcut_hash: u64,
}

fn row_cache() -> &'static Mutex<LruCache<RowCacheKey, std::sync::Arc<Vec<TranscriptRow>>>> {
    static CACHE: OnceLock<Mutex<LruCache<RowCacheKey, std::sync::Arc<Vec<TranscriptRow>>>>> =
        OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(LruCache::new(
            NonZeroUsize::new(ROW_CACHE_CAPACITY).expect("non-zero capacity"),
        ))
    })
}

/// Compute the cache composite for the current app/width/detail. Reuses the
/// crate-private accessors the overlay render key uses, so the row cache
/// invalidates on exactly the same events the overlay does.
fn row_cache_key(
    app: &crate::TuiApp,
    width: u16,
    detail: DetailPolicy,
    filter: crate::OverlayFilter,
) -> RowCacheKey {
    use std::hash::{Hash, Hasher};

    let entries = crate::active_transcript_entries(app);
    let mut transcript_hasher = std::collections::hash_map::DefaultHasher::new();
    for entry in entries {
        entry.id.hash(&mut transcript_hasher);
        entry.revision.hash(&mut transcript_hasher);
    }

    let mut pending_hasher = std::collections::hash_map::DefaultHasher::new();
    crate::active_pending_reasoning(app).hash(&mut pending_hasher);
    if crate::active_subagent_record(app).is_none() && !app.pending_assistant.trim_is_empty() {
        app.pending_assistant.text().hash(&mut pending_hasher);
    }

    let mut divider_hasher = std::collections::hash_map::DefaultHasher::new();
    crate::overlay_turn_divider_snapshot(app).hash(&mut divider_hasher);

    RowCacheKey {
        render_cache_session: app.render_cache_session,
        width,
        expand_all: detail.expand_all(),
        filter,
        palette_generation: crate::render::palette::palette_generation(),
        selected_entry: crate::active_selected_entry(app),
        tool_output_verbosity: app.tool_output_verbosity as u8,
        show_reasoning_usage: app.show_reasoning_usage,
        coalesce_tool_runs: app.coalesce_tool_runs,
        subagent_source_hash: subagent_discriminator(app),
        transcript_revision_hash: transcript_hasher.finish(),
        pending_hash: pending_hasher.finish(),
        turn_divider_hash: divider_hasher.finish(),
        shortcut_hash: crate::shortcut_hash(
            crate::key_hint(app, crate::keymap::Action::ToggleTranscriptOverlay).as_str(),
        ),
    }
}

/// Discriminate which conversation `wrap_entries` is rendering (main vs. a
/// specific subagent), since the active source changes the entire row list.
fn subagent_discriminator(app: &crate::TuiApp) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    app.subagent_pane.active.hash(&mut h);
    h.finish()
}

fn row_cache_get(key: &RowCacheKey) -> Option<Vec<TranscriptRow>> {
    let mut cache = row_cache().lock().ok()?;
    cache.get(key).map(|rows| (**rows).clone())
}

fn row_cache_put(key: RowCacheKey, rows: Vec<TranscriptRow>) {
    if let Ok(mut cache) = row_cache().lock() {
        cache.put(key, std::sync::Arc::new(rows));
    }
}

#[cfg(test)]
fn row_cache_clear() {
    if let Ok(mut cache) = row_cache().lock() {
        cache.clear();
    }
}

/// Plain text of a single line: its spans' contents concatenated, styling
/// dropped. This is the unit the clipboard and search substrate consume.
pub(crate) fn plain_text_of_line(line: &Line<'static>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Plain text of a sequence of spans (same projection as
/// [`plain_text_of_line`], exposed for callers that hold raw spans).
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
pub(crate) fn copy_range(rows: &[TranscriptRow]) -> String {
    rows.iter()
        .map(|r| r.copy_text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Copy primitive addressed by [`RowId`] range (`start..=end`, inclusive),
/// clamped to the available rows. Convenience over [`copy_range`] for
/// selection code that tracks anchors as `RowId`s.
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
#[path = "transcript_surface_tests.rs"]
mod tests;
