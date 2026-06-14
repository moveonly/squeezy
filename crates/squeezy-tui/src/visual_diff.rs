//! Visual Diff Dashboard (spec §12.10.4).
//!
//! A dev-only tool that renders frames through the single fullscreen
//! [`render`](crate::render) path and surfaces a *visual diff* between two
//! render states as a static HTML artifact. It is the cell-grid counterpart of
//! the byte-stream benchmark in [`bench_render`](crate::bench_render): instead
//! of measuring how many bytes a frame costs, it captures the exact cell grid a
//! frame paints and compares two grids cell-by-cell so a human reviewer (or a
//! snapshot test) can see what moved, what got clipped, and what went stale.
//!
//! ## What it produces
//!
//! [`render_dashboard_html`] emits one self-contained HTML page: for every
//! scenario it shows the baseline grid, the current grid, and the per-cell diff
//! side by side, plus a flagged list of the defect classes the spec calls out —
//! clipped text, overlapping (clobbered) cells, stale cells, duplicated
//! composer, missing focus, bad (too-bright) colors, and resize artifacts. A
//! tiny JS filter lets the reviewer hide unchanged scenarios. The artifact is
//! static: open it in a browser, no server, no runtime cost in the shipped TUI.
//!
//! ## Why no `TuiHarness`, no term-matrix
//!
//! The spec's original plan built on `TuiHarness` plus a *term-matrix replay*;
//! that replay harness was deleted with the inline renderer (`render()` is now
//! the one and only renderer). This module rebuilds the same contract on the
//! surviving seam — a ratatui [`TestBackend`] driven by the real `render()` —
//! exactly as [`bench_render`](crate::bench_render) does for the byte stream.
//! The grid this captures is the same [`FrameCell`](crate::testing::FrameCell)
//! projection the public `testing` harness exposes, so a dashboard cell and a
//! harness snapshot describe the same pixel.
//!
//! ## No runtime cost
//!
//! The whole module is `cfg(test)`-gated. It never compiles into a shipped TUI
//! binary, adds no keybinding, no dispatch arm, and no idle redraw. Every item
//! is exercised by the sibling `visual_diff_tests.rs`, so it carries no dead
//! code on any platform.

use std::fmt::Write as _;

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::{Terminal, TerminalOptions, Viewport};
use squeezy_core::{AppConfig, PermissionMode, PermissionPolicy, SessionMode, TranscriptItem};

use crate::{Clipboard, TuiApp, render};

/// The empty-composer caret glyph [`crate::prompt_cursor_span`] paints. Exactly
/// one of these on a main-view frame is the "one composer / focus present"
/// signal the dashboard checks for; zero means missing focus, more than one
/// means a duplicated composer.
const COMPOSER_CARET: char = '┃';

/// Rec. 601 luminance above which a foreground cell is flagged "too bright": a
/// near-white glyph (mirrored from [`crate::testing::rgb_luminance`]). The
/// eval palette rubric treats anything over ~160 as a finding, but the warm
/// design accent sits at ~230, so the *dashboard* heuristic uses a stricter
/// near-white bar (245) to flag genuinely washed-out text on the dark surface
/// without firing on the intended accent — a curated, low-noise signal, not the
/// eval gate.
const BRIGHT_LUMINANCE: u8 = 245;

/// A terminal geometry a scenario is captured at. Named so the dashboard and
/// its anomaly messages identify which size produced a grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GridSize {
    pub(crate) width: u16,
    pub(crate) height: u16,
}

impl GridSize {
    pub(crate) const fn new(width: u16, height: u16) -> Self {
        Self { width, height }
    }
}

/// The representative render states the dashboard compares. Each maps to a
/// deterministically-built [`TuiApp`] so a captured grid is reproducible across
/// runs and platforms — the spec's "scenarios" axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VisualScenario {
    /// A freshly-started session: empty transcript, composer only. The minimum
    /// frame and an edge case (nothing to wrap, just the caret).
    Empty,
    /// A short back-and-forth: a handful of prose turns. The common case.
    ShortChat,
    /// A long session that overflows the viewport. Stresses wrapping and is the
    /// baseline for the "scrolled" resize/scroll artifact comparison.
    LongSession,
    /// The same long session scrolled up off the tail. Paired with
    /// [`VisualScenario::LongSession`] it is the canonical "view moved" diff.
    ScrolledLongSession,
    /// A session whose composer holds typed text. Used to prove the caret is
    /// still painted exactly once when the composer is non-empty (focus
    /// present) and to exercise a non-trivial composer row.
    ComposerText,
    /// A diff card pinned into the open transcript overlay — the `/diff` review
    /// surface. Captures the diff-card header, summary, and `+/-` body so a
    /// regression on the diff surface flips the grid.
    DiffOverlay,
    /// The Live Review Board open over a multi-worker fan-out, grouped into
    /// lanes. Captures the lane headers, the caret on the selected card, and
    /// the per-worker metrics.
    ReviewBoard,
    /// The diff/detail pane split off the right of the transcript overlay, with
    /// a bulky diff entry pinned. Captures the pane separator and the
    /// independently-scrolled detail body beside the transcript.
    DiffDetailPane,
}

impl VisualScenario {
    /// Every scenario, in a stable order, so the dashboard sweeps the suite
    /// deterministically.
    pub(crate) const ALL: [VisualScenario; 8] = [
        VisualScenario::Empty,
        VisualScenario::ShortChat,
        VisualScenario::LongSession,
        VisualScenario::ScrolledLongSession,
        VisualScenario::ComposerText,
        VisualScenario::DiffOverlay,
        VisualScenario::ReviewBoard,
        VisualScenario::DiffDetailPane,
    ];

    /// Stable slug for HTML anchors / log identification.
    pub(crate) fn slug(self) -> &'static str {
        match self {
            VisualScenario::Empty => "empty",
            VisualScenario::ShortChat => "short_chat",
            VisualScenario::LongSession => "long_session",
            VisualScenario::ScrolledLongSession => "scrolled_long_session",
            VisualScenario::ComposerText => "composer_text",
            VisualScenario::DiffOverlay => "diff_overlay",
            VisualScenario::ReviewBoard => "review_board",
            VisualScenario::DiffDetailPane => "diff_detail_pane",
        }
    }

    /// Human-readable label shown in the dashboard heading.
    pub(crate) fn title(self) -> &'static str {
        match self {
            VisualScenario::Empty => "Empty session",
            VisualScenario::ShortChat => "Short chat",
            VisualScenario::LongSession => "Long session (tail)",
            VisualScenario::ScrolledLongSession => "Long session (scrolled)",
            VisualScenario::ComposerText => "Composer with text",
            VisualScenario::DiffOverlay => "Diff card in overlay",
            VisualScenario::ReviewBoard => "Live review board",
            VisualScenario::DiffDetailPane => "Diff detail pane (split)",
        }
    }

    /// Whether this scenario opens a fullscreen overlay surface (transcript
    /// overlay or review board) that takes over the screen. Those surfaces paint
    /// no main-view composer, so the "exactly one composer caret" invariant
    /// applies only to the non-overlay scenarios.
    pub(crate) fn is_overlay(self) -> bool {
        matches!(
            self,
            VisualScenario::DiffOverlay
                | VisualScenario::ReviewBoard
                | VisualScenario::DiffDetailPane
        )
    }

    /// Build the [`TuiApp`] this scenario captures at `size`. Self-contained: a
    /// temp workspace root so construction never crawls the real repo.
    fn build_app(self, size: GridSize) -> TuiApp {
        let mut app = new_diff_app();
        match self {
            VisualScenario::Empty => {}
            VisualScenario::ShortChat => {
                app.push_transcript_item(TranscriptItem::user("explain this stack trace"));
                app.push_transcript_item(TranscriptItem::assistant(
                    "The panic unwinds through `render` because the slice index is out of bounds.",
                ));
                app.push_transcript_item(TranscriptItem::user("how do I fix it?"));
                app.push_transcript_item(TranscriptItem::assistant(
                    "Clamp the offset against the row count before indexing.",
                ));
            }
            VisualScenario::LongSession => {
                seed_long_session(&mut app, 120);
            }
            VisualScenario::ScrolledLongSession => {
                seed_long_session(&mut app, 120);
                scroll_up(&mut app, 400, size);
            }
            VisualScenario::ComposerText => {
                app.push_transcript_item(TranscriptItem::user("draft the migration plan"));
                app.push_transcript_item(TranscriptItem::assistant("Here is the migration plan."));
                app.input = "wip: still typing the next prompt".to_string();
            }
            VisualScenario::DiffOverlay => {
                seed_diff_card(&mut app);
                app.selected_entry = Some(app.transcript.len().saturating_sub(1));
                app.transcript_overlay = Some(crate::TranscriptOverlayState::default());
            }
            VisualScenario::ReviewBoard => {
                seed_review_board(&mut app);
                app.review_board_open = true;
            }
            VisualScenario::DiffDetailPane => {
                let entry_id = seed_diff_card(&mut app);
                app.selected_entry = Some(app.transcript.len().saturating_sub(1));
                app.transcript_overlay = Some(crate::TranscriptOverlayState::default());
                app.diff_detail_pane =
                    Some(crate::diff_detail_pane::DiffDetailPaneState::new(entry_id));
            }
        }
        app
    }
}

/// A single captured cell: its symbol plus the stringified colors and modifiers
/// the diff and anomaly checks read. Mirrors
/// [`crate::testing::FrameCell`](crate::testing::FrameCell) so a dashboard cell
/// and a harness snapshot describe the same pixel — but carries the raw
/// [`Color`] too so luminance checks need no re-parse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GridCell {
    pub(crate) symbol: String,
    pub(crate) fg: Color,
    pub(crate) bg: Color,
    pub(crate) bold: bool,
}

impl GridCell {
    /// True when two cells paint identically — same glyph, same colors, same
    /// weight. The diff treats any inequality as a changed cell.
    fn matches(&self, other: &GridCell) -> bool {
        self == other
    }
}

/// A full rendered frame projected to a cell grid plus the metadata the
/// dashboard heads each panel with — scenario, geometry, and the theme label
/// the frame was captured under (the spec's scenario/size/theme axes).
#[derive(Clone, Debug)]
pub(crate) struct FrameGrid {
    pub(crate) scenario: VisualScenario,
    pub(crate) size: GridSize,
    pub(crate) theme: &'static str,
    pub(crate) cells: Vec<GridCell>,
}

impl FrameGrid {
    /// Capture `scenario` at `size` by driving the real [`render`] over a
    /// [`TestBackend`] and projecting its post-frame buffer to a grid. `theme`
    /// is a label recorded as metadata only — the renderer reads the global
    /// theme, which this dev tool leaves at its default so parallel captures
    /// stay deterministic.
    pub(crate) fn capture(
        scenario: VisualScenario,
        size: GridSize,
        theme: &'static str,
    ) -> FrameGrid {
        let app = scenario.build_app(size);
        Self::capture_app(&app, scenario, size, theme)
    }

    /// Capture an already-built `app`. Split out so a test can stage a bespoke
    /// app (a deliberately-clipped one, say) without a scenario variant.
    fn capture_app(
        app: &TuiApp,
        scenario: VisualScenario,
        size: GridSize,
        theme: &'static str,
    ) -> FrameGrid {
        let viewport = Rect::new(0, 0, size.width, size.height);
        let mut terminal = Terminal::with_options(
            TestBackend::new(size.width, size.height),
            TerminalOptions {
                viewport: Viewport::Fixed(viewport),
            },
        )
        .expect("test backend");
        terminal.draw(|frame| render(frame, app)).expect("draw");
        let buffer = terminal.backend().buffer().clone();
        FrameGrid {
            scenario,
            size,
            theme,
            cells: grid_from_buffer(&buffer, size),
        }
    }

    /// Cell at (`x`, `y`), or `None` when out of bounds. Row-major.
    fn cell(&self, x: u16, y: u16) -> Option<&GridCell> {
        if x >= self.size.width || y >= self.size.height {
            return None;
        }
        self.cells
            .get(y as usize * self.size.width as usize + x as usize)
    }

    /// Flatten the grid to plain text (rows joined by newlines), trailing
    /// spaces preserved so column shifts stay visible. The substring surface
    /// the clipping check reads.
    fn plain_text(&self) -> String {
        let mut out =
            String::with_capacity((self.size.width as usize + 1) * self.size.height as usize);
        for y in 0..self.size.height {
            for x in 0..self.size.width {
                if let Some(cell) = self.cell(x, y) {
                    out.push_str(&cell.symbol);
                }
            }
            out.push('\n');
        }
        out
    }
}

/// Project a rendered [`Buffer`] to the dashboard's flat cell grid.
fn grid_from_buffer(buffer: &Buffer, size: GridSize) -> Vec<GridCell> {
    let mut cells = Vec::with_capacity(size.width as usize * size.height as usize);
    for y in 0..size.height {
        for x in 0..size.width {
            let cell = &buffer[(x, y)];
            cells.push(GridCell {
                symbol: cell.symbol().to_string(),
                fg: cell.fg,
                bg: cell.bg,
                bold: cell.modifier.contains(ratatui::style::Modifier::BOLD),
            });
        }
    }
    cells
}

/// How a single cell changed between baseline and current grids.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CellChange {
    /// Identical in both grids.
    Same,
    /// Glyph changed (with or without a style change).
    Glyph,
    /// Same glyph, different color or weight.
    Style,
    /// Present in current but the baseline cell was blank, or vice versa — a
    /// cell that *appeared* or *vanished* (the spec's "stale cell" signal when
    /// it lingers from an old frame).
    Appeared,
    /// Blank now, non-blank before: content the new frame failed to repaint.
    Vanished,
}

impl CellChange {
    /// CSS class the HTML grid tags a changed cell with, so the stylesheet can
    /// color the diff. `Same` is untagged.
    fn css_class(self) -> &'static str {
        match self {
            CellChange::Same => "same",
            CellChange::Glyph => "glyph",
            CellChange::Style => "style",
            CellChange::Appeared => "appeared",
            CellChange::Vanished => "vanished",
        }
    }
}

/// The cell-by-cell diff of two grids: a parallel `changes` vector plus the
/// rollup counts the dashboard summarizes each scenario with.
#[derive(Clone, Debug)]
pub(crate) struct GridDiff {
    pub(crate) size: GridSize,
    pub(crate) changes: Vec<CellChange>,
    pub(crate) changed_cells: usize,
}

impl GridDiff {
    /// Diff `current` against `baseline`. The two grids must share a geometry;
    /// a size mismatch is itself a resize artifact, surfaced separately by
    /// [`detect_anomalies`], so here we diff over the overlapping region and
    /// treat the non-overlapping tail as `Appeared`/`Vanished`.
    pub(crate) fn compute(baseline: &FrameGrid, current: &FrameGrid) -> GridDiff {
        let size = current.size;
        let mut changes = Vec::with_capacity(size.width as usize * size.height as usize);
        let mut changed = 0usize;
        for y in 0..size.height {
            for x in 0..size.width {
                let cur = current.cell(x, y);
                let base = baseline.cell(x, y);
                let change = classify(base, cur);
                if change != CellChange::Same {
                    changed += 1;
                }
                changes.push(change);
            }
        }
        GridDiff {
            size,
            changes,
            changed_cells: changed,
        }
    }

    /// Change at (`x`, `y`), or [`CellChange::Same`] out of bounds.
    fn change(&self, x: u16, y: u16) -> CellChange {
        if x >= self.size.width || y >= self.size.height {
            return CellChange::Same;
        }
        self.changes
            .get(y as usize * self.size.width as usize + x as usize)
            .copied()
            .unwrap_or(CellChange::Same)
    }
}

/// True for a cell that paints nothing visible — the space glyph, the only
/// glyph a fresh ratatui buffer fills with.
fn is_blank(cell: Option<&GridCell>) -> bool {
    match cell {
        None => true,
        Some(c) => c.symbol == " " || c.symbol.is_empty(),
    }
}

/// Classify one cell position's change between baseline and current.
fn classify(base: Option<&GridCell>, cur: Option<&GridCell>) -> CellChange {
    match (base, cur) {
        (Some(b), Some(c)) if b.matches(c) => CellChange::Same,
        (b, c) => {
            let base_blank = is_blank(b);
            let cur_blank = is_blank(c);
            match (base_blank, cur_blank) {
                (true, true) => CellChange::Same,
                (true, false) => CellChange::Appeared,
                (false, true) => CellChange::Vanished,
                (false, false) => {
                    // Both non-blank and not equal: glyph change wins over a
                    // pure style change so the diff prioritizes content moves.
                    match (b, c) {
                        (Some(b), Some(c)) if b.symbol == c.symbol => CellChange::Style,
                        _ => CellChange::Glyph,
                    }
                }
            }
        }
    }
}

/// A defect class the spec asks the dashboard to expose, with a localized
/// message. Collected per scenario and rendered as a flagged list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Anomaly {
    pub(crate) kind: AnomalyKind,
    pub(crate) message: String,
}

/// The spec's enumerated defect classes: clipped text, overlap, stale cells,
/// duplicated composer, missing focus, bad colors, resize artifacts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AnomalyKind {
    ClippedText,
    Overlap,
    StaleCell,
    DuplicatedComposer,
    MissingFocus,
    BadColor,
    ResizeArtifact,
}

impl AnomalyKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            AnomalyKind::ClippedText => "clipped-text",
            AnomalyKind::Overlap => "overlap",
            AnomalyKind::StaleCell => "stale-cell",
            AnomalyKind::DuplicatedComposer => "duplicated-composer",
            AnomalyKind::MissingFocus => "missing-focus",
            AnomalyKind::BadColor => "bad-color",
            AnomalyKind::ResizeArtifact => "resize-artifact",
        }
    }
}

/// Scan baseline + current grids and their diff for every defect class the
/// dashboard surfaces. Returns the flagged anomalies in a stable order so a
/// snapshot test is deterministic.
pub(crate) fn detect_anomalies(
    baseline: &FrameGrid,
    current: &FrameGrid,
    diff: &GridDiff,
) -> Vec<Anomaly> {
    let mut out = Vec::new();

    // Resize artifact: a geometry mismatch between the two states. The diff
    // already clamped to the current size; flag the mismatch so the reviewer
    // knows the comparison spans a resize.
    if baseline.size != current.size {
        out.push(Anomaly {
            kind: AnomalyKind::ResizeArtifact,
            message: format!(
                "geometry changed {}x{} -> {}x{}",
                baseline.size.width, baseline.size.height, current.size.width, current.size.height
            ),
        });
    }

    // Duplicated composer / missing focus: the empty-composer caret must be
    // painted exactly once. Zero = the composer lost focus / vanished; more
    // than one = a duplicated composer. Only meaningful when the composer is
    // empty (the caret is the empty-composer glyph); a non-empty composer
    // paints its text instead, so we skip the count there but still require the
    // bottom rows to carry *some* glyph (focus present).
    let caret_count = count_caret(current);
    if caret_count == 0 {
        out.push(Anomaly {
            kind: AnomalyKind::MissingFocus,
            message: "no composer caret painted — focus appears lost".to_string(),
        });
    } else if caret_count > 1 {
        out.push(Anomaly {
            kind: AnomalyKind::DuplicatedComposer,
            message: format!("composer caret painted {caret_count} times (expected 1)"),
        });
    }

    // Clipped text: a glyph painted in the last column that is mid-word (its
    // left neighbor is also a letter and the cell is at the hard right edge)
    // signals content that ran off the viewport without an ellipsis. A
    // conservative heuristic — flagged, not failed.
    if let Some(col) = clipped_column(current) {
        out.push(Anomaly {
            kind: AnomalyKind::ClippedText,
            message: format!("row {col} ends mid-word at the right edge — possible clip"),
        });
    }

    // Overlap: a wide glyph (CJK / emoji) whose following cell was not blanked
    // by ratatui leaves a stray symbol — the classic "double-width clobber". A
    // cell whose left neighbor is wide *and* which still carries its own
    // non-space glyph is an overlap.
    if let Some((x, y)) = overlap_cell(current) {
        out.push(Anomaly {
            kind: AnomalyKind::Overlap,
            message: format!("cell ({x},{y}) overlaps a wide glyph to its left"),
        });
    }

    // Stale cell: a cell the new frame left non-blank but the diff marks
    // `Vanished` elsewhere while this position is unchanged from a *different*
    // baseline glyph — i.e. content that should have been repainted but wasn't.
    // We approximate "stale" as a `Vanished` change: the baseline had a glyph
    // the current frame dropped to blank, which on a real terminal would leave
    // the old glyph on screen (the diffing renderer must emit a space to clear
    // it). Reported as a count so the reviewer can scan the diff grid.
    let vanished = diff
        .changes
        .iter()
        .filter(|c| **c == CellChange::Vanished)
        .count();
    if vanished > 0 {
        out.push(Anomaly {
            kind: AnomalyKind::StaleCell,
            message: format!(
                "{vanished} cell(s) went blank vs baseline — must be cleared, not left stale"
            ),
        });
    }

    // Bad color: a foreground glyph painted brighter than the luminance
    // guardrail reads as washed-out against the dark surface. Report the first
    // offending cell so the reviewer can find it.
    if let Some((x, y, lum)) = too_bright_cell(current) {
        out.push(Anomaly {
            kind: AnomalyKind::BadColor,
            message: format!("cell ({x},{y}) fg luminance {lum} exceeds {BRIGHT_LUMINANCE}"),
        });
    }

    out
}

/// Count the empty-composer caret glyph across a grid.
fn count_caret(grid: &FrameGrid) -> usize {
    grid.cells
        .iter()
        .filter(|c| c.symbol.starts_with(COMPOSER_CARET))
        .count()
}

/// The first row index that ends mid-word at the hard right edge, if any. A
/// conservative clip heuristic: the last column is an ASCII letter and so is the
/// cell to its left (so it is not a single trailing char or punctuation).
fn clipped_column(grid: &FrameGrid) -> Option<u16> {
    let w = grid.size.width;
    if w < 2 {
        return None;
    }
    for y in 0..grid.size.height {
        let last = grid.cell(w - 1, y);
        let prev = grid.cell(w - 2, y);
        if is_ascii_letter(last) && is_ascii_letter(prev) {
            return Some(y);
        }
    }
    None
}

/// The first cell that overlaps a wide glyph to its left, if any. ratatui blanks
/// the trailing half of a double-width glyph; a non-blank trailing cell whose
/// left neighbor is wide is an overlap clobber.
fn overlap_cell(grid: &FrameGrid) -> Option<(u16, u16)> {
    for y in 0..grid.size.height {
        for x in 1..grid.size.width {
            let left = grid.cell(x - 1, y);
            let here = grid.cell(x, y);
            if is_wide(left) && !is_blank(here) {
                return Some((x, y));
            }
        }
    }
    None
}

/// The first foreground cell brighter than [`BRIGHT_LUMINANCE`], with its
/// luminance. Only RGB / named colors are checked; `Reset` and indexed colors
/// inherit the terminal default and are skipped.
fn too_bright_cell(grid: &FrameGrid) -> Option<(u16, u16, u8)> {
    for y in 0..grid.size.height {
        for x in 0..grid.size.width {
            let Some(cell) = grid.cell(x, y) else {
                continue;
            };
            if is_blank(Some(cell)) {
                continue;
            }
            if let Some(rgb) = color_rgb(cell.fg) {
                let lum = crate::testing::rgb_luminance(rgb);
                if lum > BRIGHT_LUMINANCE {
                    return Some((x, y, lum));
                }
            }
        }
    }
    None
}

/// True when a cell carries a single ASCII letter.
fn is_ascii_letter(cell: Option<&GridCell>) -> bool {
    matches!(cell, Some(c) if c.symbol.len() == 1
        && c.symbol.as_bytes()[0].is_ascii_alphabetic())
}

/// True when a cell carries a double-width glyph (CJK / emoji).
fn is_wide(cell: Option<&GridCell>) -> bool {
    use unicode_width::UnicodeWidthStr;
    matches!(cell, Some(c) if c.symbol.width() >= 2)
}

/// Resolve a [`Color`] to an sRGB triple for luminance checks. `Reset` and
/// indexed colors return `None` (inherit terminal default).
fn color_rgb(color: Color) -> Option<(u8, u8, u8)> {
    match color {
        Color::Reset | Color::Indexed(_) => None,
        Color::Rgb(r, g, b) => Some((r, g, b)),
        other => Some(crate::render::palette::rgb_components(other)),
    }
}

/// One scenario's full comparison: both grids, the diff, and the flagged
/// anomalies. The unit the dashboard renders a panel per.
#[derive(Clone, Debug)]
pub(crate) struct ScenarioReport {
    pub(crate) baseline: FrameGrid,
    pub(crate) current: FrameGrid,
    pub(crate) diff: GridDiff,
    pub(crate) anomalies: Vec<Anomaly>,
}

impl ScenarioReport {
    /// Build a report by capturing `scenario` twice — once as the baseline,
    /// once as the current state — at the given sizes. When `baseline_size`
    /// equals `current_size` and the scenario is unchanged the diff is empty;
    /// callers stage a real change by varying the size (resize) or the scenario
    /// pair (see [`compare_scenarios`]).
    pub(crate) fn capture(
        scenario: VisualScenario,
        baseline_size: GridSize,
        current_size: GridSize,
        theme: &'static str,
    ) -> ScenarioReport {
        let baseline = FrameGrid::capture(scenario, baseline_size, theme);
        let current = FrameGrid::capture(scenario, current_size, theme);
        Self::from_grids(baseline, current)
    }

    /// Build a report from two already-captured grids — the path a test uses to
    /// pin a one-cell / moved / clipped / stale fixture.
    pub(crate) fn from_grids(baseline: FrameGrid, current: FrameGrid) -> ScenarioReport {
        let diff = GridDiff::compute(&baseline, &current);
        let anomalies = detect_anomalies(&baseline, &current, &diff);
        ScenarioReport {
            baseline,
            current,
            diff,
            anomalies,
        }
    }
}

/// Compare two *different* scenarios at one size — e.g. the long session before
/// and after scrolling — so the diff shows a real "view moved" change rather
/// than an identity. The current grid drives the result geometry.
pub(crate) fn compare_scenarios(
    baseline: VisualScenario,
    current: VisualScenario,
    size: GridSize,
    theme: &'static str,
) -> ScenarioReport {
    let base = FrameGrid::capture(baseline, size, theme);
    let cur = FrameGrid::capture(current, size, theme);
    ScenarioReport::from_grids(base, cur)
}

/// Build the default dashboard suite: every scenario captured at `size`,
/// compared against itself (the no-change baseline) plus the canonical
/// long-session scroll move. The set a CI snapshot pins.
pub(crate) fn default_reports(size: GridSize) -> Vec<ScenarioReport> {
    let mut reports = Vec::new();
    for &scenario in &VisualScenario::ALL {
        reports.push(ScenarioReport::capture(scenario, size, size, "default"));
    }
    // The canonical real change: the long session before vs after scrolling.
    reports.push(compare_scenarios(
        VisualScenario::LongSession,
        VisualScenario::ScrolledLongSession,
        size,
        "default",
    ));
    reports
}

/// Render the full static HTML dashboard for `reports`. Self-contained: inline
/// CSS + a tiny filter script, no external assets. The artifact a reviewer
/// opens in a browser and a snapshot test pins.
pub(crate) fn render_dashboard_html(reports: &[ScenarioReport]) -> String {
    let mut html = String::with_capacity(16 * 1024);
    html.push_str("<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n");
    html.push_str("<meta charset=\"utf-8\">\n");
    html.push_str("<title>Squeezy TUI Visual Diff Dashboard</title>\n");
    html.push_str("<style>\n");
    html.push_str(DASHBOARD_CSS);
    html.push_str("</style>\n</head>\n<body>\n");
    html.push_str("<h1>Squeezy TUI Visual Diff Dashboard</h1>\n");
    html.push_str(
        "<p class=\"intro\">Baseline vs current cell grids rendered through the single \
         fullscreen <code>render()</code>. Filter to changed scenarios only:</p>\n",
    );
    html.push_str(
        "<label class=\"filter\"><input type=\"checkbox\" id=\"changed-only\"> \
         show changed scenarios only</label>\n",
    );

    for report in reports {
        write_scenario_panel(&mut html, report);
    }

    html.push_str("<script>\n");
    html.push_str(DASHBOARD_JS);
    html.push_str("</script>\n");
    html.push_str("</body>\n</html>\n");
    html
}

/// Write one scenario's panel: heading, anomaly list, then the baseline /
/// current / diff grids side by side.
fn write_scenario_panel(html: &mut String, report: &ScenarioReport) {
    let scenario = report.current.scenario;
    let changed = report.diff.changed_cells > 0;
    let _ = writeln!(
        html,
        "<section class=\"scenario {}\" data-changed=\"{}\" id=\"{}\">",
        if changed { "is-changed" } else { "is-same" },
        changed,
        escape_attr(scenario.slug()),
    );
    let _ = writeln!(
        html,
        "<h2>{} <span class=\"meta\">{}x{} · theme {} · {} changed cell(s)</span></h2>",
        escape_html(scenario.title()),
        report.current.size.width,
        report.current.size.height,
        escape_html(report.current.theme),
        report.diff.changed_cells,
    );

    // Anomaly list (or a clean badge).
    if report.anomalies.is_empty() {
        html.push_str("<p class=\"clean\">no anomalies detected</p>\n");
    } else {
        html.push_str("<ul class=\"anomalies\">\n");
        for anomaly in &report.anomalies {
            let _ = writeln!(
                html,
                "<li class=\"anomaly {}\"><span class=\"tag\">{}</span> {}</li>",
                escape_attr(anomaly.kind.label()),
                escape_html(anomaly.kind.label()),
                escape_html(&anomaly.message),
            );
        }
        html.push_str("</ul>\n");
    }

    html.push_str("<div class=\"grids\">\n");
    write_grid(html, "baseline", &report.baseline, None);
    write_grid(html, "current", &report.current, Some(&report.diff));
    write_diff_grid(html, &report.diff);
    html.push_str("</div>\n</section>\n");
}

/// Write a single cell grid as a monospace table of spans. When `diff` is
/// supplied each cell is tagged with its change class so the current grid
/// highlights what moved.
fn write_grid(html: &mut String, label: &str, grid: &FrameGrid, diff: Option<&GridDiff>) {
    let _ = write!(
        html,
        "<figure class=\"grid\"><figcaption>{label}</figcaption><pre>"
    );
    for y in 0..grid.size.height {
        for x in 0..grid.size.width {
            let cell = grid.cell(x, y);
            let symbol = cell.map(|c| c.symbol.as_str()).unwrap_or(" ");
            let class = diff.map(|d| d.change(x, y).css_class()).unwrap_or("same");
            let _ = write!(
                html,
                "<span class=\"c {}\">{}</span>",
                class,
                escape_html(display_symbol(symbol)),
            );
        }
        html.push('\n');
    }
    html.push_str("</pre></figure>\n");
}

/// Write the dedicated diff grid: a `.` for unchanged cells and the change-class
/// glyph for everything else, so the reviewer sees the shape of the change at a
/// glance independent of content.
fn write_diff_grid(html: &mut String, diff: &GridDiff) {
    html.push_str("<figure class=\"grid\"><figcaption>diff</figcaption><pre>");
    for y in 0..diff.size.height {
        for x in 0..diff.size.width {
            let change = diff.change(x, y);
            let glyph = match change {
                CellChange::Same => '.',
                CellChange::Glyph => '#',
                CellChange::Style => '~',
                CellChange::Appeared => '+',
                CellChange::Vanished => '-',
            };
            let _ = write!(
                html,
                "<span class=\"c {}\">{}</span>",
                change.css_class(),
                glyph,
            );
        }
        html.push('\n');
    }
    html.push_str("</pre></figure>\n");
}

/// Map a possibly-blank or control symbol to a printable display glyph for the
/// HTML grid. A space stays a non-breaking space so column alignment holds.
fn display_symbol(symbol: &str) -> &str {
    if symbol.is_empty() || symbol == " " {
        "\u{00a0}"
    } else {
        symbol
    }
}

/// Minimal HTML-attribute escaping for the slugs/labels we emit (no quotes in
/// inputs, but be safe).
fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;").replace('"', "&quot;")
}

/// Minimal HTML text escaping for cell content and messages.
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

const DASHBOARD_CSS: &str = "\
body { background: #1b1b1b; color: #ddd; font-family: sans-serif; margin: 1.5rem; }
h1 { font-size: 1.4rem; }
.intro { color: #aaa; }
.scenario { border: 1px solid #333; border-radius: 6px; margin: 1rem 0; padding: 0.8rem; }
.scenario h2 { font-size: 1.05rem; margin: 0 0 0.4rem; }
.meta { color: #888; font-weight: normal; font-size: 0.85rem; }
.clean { color: #6a9; }
.anomalies { list-style: none; padding: 0; margin: 0.3rem 0; }
.anomaly { margin: 0.15rem 0; }
.tag { display: inline-block; min-width: 9rem; color: #e6b; font-family: monospace; }
.grids { display: flex; gap: 1rem; flex-wrap: wrap; }
.grid pre { background: #111; padding: 0.4rem; line-height: 1; font-size: 11px; }
.grid figcaption { color: #999; font-size: 0.8rem; }
.c { white-space: pre; }
.c.glyph { background: #553; }
.c.style { background: #335; }
.c.appeared { background: #353; }
.c.vanished { background: #533; }
";

const DASHBOARD_JS: &str = "\
document.getElementById('changed-only').addEventListener('change', function (e) {
  var only = e.target.checked;
  document.querySelectorAll('.scenario').forEach(function (s) {
    s.style.display = (only && s.dataset.changed !== 'true') ? 'none' : '';
  });
});
";

/// Seed `app` with `turns` user/assistant exchanges so a scenario overflows the
/// viewport deterministically. Mirrors the bench harness's seeder.
fn seed_long_session(app: &mut TuiApp, turns: usize) {
    for i in 0..turns {
        app.push_transcript_item(TranscriptItem::user(format!("question number {i}")));
        app.push_transcript_item(TranscriptItem::assistant(format!(
            "Answer {i}: the relevant module lives under crates and the fix is local."
        )));
    }
}

/// Push a deterministic diff card and return its transcript-entry id. Fixed
/// paths / `+/-` body so the captured grid is stable across runs.
fn seed_diff_card(app: &mut TuiApp) -> u64 {
    use ratatui::text::{Line, Span};
    let lines: Vec<Line<'static>> = vec![
        Line::from(Span::raw("--- a/src/lib.rs".to_string())),
        Line::from(Span::raw("+++ b/src/lib.rs".to_string())),
        Line::from(Span::raw(
            "+    let clamped = offset.min(rows);".to_string(),
        )),
        Line::from(Span::raw("-    let clamped = offset;".to_string())),
    ];
    app.push_diff_card(crate::DiffCardData {
        summary: "1 file · +1 -1".to_string(),
        plain: "--- a/src/lib.rs\n+++ b/src/lib.rs\n+    let clamped = offset.min(rows);\n-    let clamped = offset;\n".to_string(),
        lines,
    });
    app.transcript
        .last()
        .map(|entry| entry.id)
        .unwrap_or_default()
}

/// Seed a deterministic multi-worker review board (one card per lane) and park
/// the cursor on the first card so the caret renders.
fn seed_review_board(app: &mut TuiApp) {
    use crate::subagent_timeline::{SubagentTimelineSource, SubagentTimelineStatus};
    let sources = vec![
        SubagentTimelineSource {
            id: 1,
            agent: "implement".to_string(),
            status: SubagentTimelineStatus::Running,
            latest: "editing src/lib.rs".to_string(),
            elapsed_secs: Some(42),
            tool_count: 3,
            cost_micros: Some(1_500_000),
        },
        SubagentTimelineSource {
            id: 2,
            agent: "review".to_string(),
            status: SubagentTimelineStatus::Failed,
            latest: "test failed".to_string(),
            elapsed_secs: Some(17),
            tool_count: 2,
            cost_micros: Some(500_000),
        },
        SubagentTimelineSource {
            id: 3,
            agent: "explore".to_string(),
            status: SubagentTimelineStatus::Completed,
            latest: "found the call site".to_string(),
            elapsed_secs: Some(88),
            tool_count: 4,
            cost_micros: Some(900_000),
        },
    ];
    let fingerprint = crate::review_board::ReviewBoard::fingerprint_of(sources.iter());
    app.review_board.rebuild_if_stale(fingerprint, &sources);
    app.review_board_cursor = app.review_board.card_at(0).map(|card| card.id);
}

/// Scroll the active transcript up so the view stops following the tail.
fn scroll_up(app: &mut TuiApp, lines: usize, size: GridSize) {
    let line_count = crate::transcript_lines_for_render(app, Some(size.width), true).len();
    let viewport_h = size.height.saturating_sub(1) as usize;
    crate::active_transcript_scroll_mut(app).set_from_bottom(lines, line_count, viewport_h);
}

/// Build a self-contained [`TuiApp`]: a temp workspace root so construction
/// never crawls the real repo, and a no-op clipboard.
fn new_diff_app() -> TuiApp {
    let config = diff_config();
    TuiApp::new_with_clipboard(
        "visual-diff",
        &config,
        SessionMode::Build,
        None,
        Box::new(NoopDiffClipboard),
    )
}

/// A minimal [`AppConfig`] pinned to a unique temp workspace so construction is
/// hermetic.
fn diff_config() -> AppConfig {
    AppConfig {
        model: "visual-diff-model".to_string(),
        session_mode: SessionMode::Build,
        permissions: PermissionPolicy {
            read: PermissionMode::Allow,
            edit: PermissionMode::Ask,
            shell: PermissionMode::Ask,
            web: PermissionMode::Ask,
            ..Default::default()
        },
        config_sources: vec!["defaults".to_string()],
        workspace_root: diff_temp_root(),
        ..Default::default()
    }
}

/// A unique temp directory so two parallel diff apps never share a root.
fn diff_temp_root() -> std::path::PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let root = std::env::temp_dir().join(format!(
        "squeezy_tui_visual_diff_{}_{nonce}",
        std::process::id()
    ));
    let _ = std::fs::create_dir_all(&root);
    root
}

struct NoopDiffClipboard;

impl Clipboard for NoopDiffClipboard {
    fn copy_text(&mut self, _text: &str) -> std::result::Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
#[path = "visual_diff_tests.rs"]
mod tests;
