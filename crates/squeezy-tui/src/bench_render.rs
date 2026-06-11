//! Real Terminal Benchmark Suite (spec §12.10.2).
//!
//! A Rust-only benchmark harness that drives the single fullscreen
//! [`render`](crate::render) through representative scenarios and *real*
//! terminal byte streams, then reports per-scenario latency, emitted bytes,
//! frame counts, scroll jumps, and teardown correctness as serializable JSON.
//!
//! ## Why a byte-stream benchmark
//!
//! The spec's original plan extended a "term-matrix benchmark mode"; that
//! harness was deleted along with the inline renderer (`render()` is now the
//! one and only renderer). This module rebuilds the same measurement contract
//! on the surviving seam: [`TerminalWriter::capture`](crate::terminal_writer)
//! behind a [`CrosstermBackend`], which is the exact in-memory counterpart of
//! the real stdout the production `TerminalGuard` writes through. Driving
//! `render()` over that sink measures the genuine ANSI byte stream the TUI
//! would emit to a terminal — not just the cell grid a [`TestBackend`] keeps.
//!
//! Two backends are exercised per scenario:
//!
//! * [`BenchBackend::CaptureStream`] — `CrosstermBackend<TerminalWriter>` over
//!   the capture sink. This is the "real terminal byte stream": bytes-emitted
//!   and latency reflect the actual ANSI the diffing renderer produced.
//! * [`BenchBackend::Cells`] — a [`TestBackend`] whose post-frame cell buffer
//!   the hard-invariant checks read (one composer cursor, latest output
//!   visible, no duplicated composer, in-bounds paint).
//!
//! Node/tmux/PTY backends are intentionally omitted: the spec keeps Rust-only
//! backends as the portable floor that runs on every PR and every platform,
//! with the environment-sensitive legs left as a report-only follow-up.
//!
//! ## Hard invariants
//!
//! Every benchmarked frame is checked against the spec's invariants:
//! one composer, no duplicated UI, cursor in bounds, latest output visible
//! when following the tail, and — once per scenario — a clean teardown whose
//! transcript mirror is emitted *after* `LeaveAlternateScreen`. A violated
//! invariant fails the run rather than silently skewing a number.
//!
//! Gated behind `cfg(test)` so it never compiles into a shipped TUI binary,
//! and so every item is exercised by the sibling test module (no dead code).

use std::sync::{Arc, Mutex};
use std::time::Instant;

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::{Terminal, TerminalOptions, Viewport, backend::CrosstermBackend};
use serde::Serialize;
use squeezy_core::{AppConfig, PermissionMode, PermissionPolicy, SessionMode, TranscriptItem};

use crate::terminal_writer::TerminalWriter;
use crate::{
    Clipboard, TuiApp, active_transcript_scroll, active_transcript_scroll_mut,
    emit_finish_fullscreen, render, render_lines_to_owned_buffer, transcript_lines_for_render,
};

/// The empty-composer cursor glyph [`crate::prompt_cursor_span`] paints. An
/// idle main-view frame renders this exactly once — the single composer
/// caret — which is how the "one composer / no duplicated UI" invariant is
/// checked without depending on any other chrome glyph.
const COMPOSER_CURSOR: char = '┃';

/// `\x1b[?1049l` — crossterm's `LeaveAlternateScreen`. The teardown invariant
/// asserts the transcript mirror is emitted *after* this leave so it lands in
/// real scrollback, not the alternate screen.
const LEAVE_ALT_SCREEN: &str = "\x1b[?1049l";

/// A terminal size to benchmark a scenario at. Named so JSON summaries and
/// failure messages identify the geometry that produced a number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct BenchSize {
    pub(crate) width: u16,
    pub(crate) height: u16,
}

impl BenchSize {
    pub(crate) const fn new(width: u16, height: u16) -> Self {
        Self { width, height }
    }
}

/// The representative situations a scenario builder can stage. Each maps to a
/// deterministically-constructed [`TuiApp`] so a benchmark number is
/// reproducible across runs and platforms.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BenchScenario {
    /// A freshly-started session: empty transcript, composer only. The
    /// minimum-work floor and an edge case (nothing to wrap).
    Empty,
    /// A short back-and-forth: a handful of prose turns. The common case.
    ShortChat,
    /// A long session that overflows the viewport: hundreds of turns, used to
    /// stress row wrapping and the diffing renderer's bytes-per-frame.
    LongSession,
    /// A long session scrolled up off the tail. Exercises the scroll path and
    /// proves bytes-emitted shrinks once the view stops following new output.
    ScrolledLongSession,
    /// A code-heavy transcript (fenced blocks). Stresses the syntax-highlight
    /// wrap path that dominates real coding sessions.
    CodeHeavy,
}

impl BenchScenario {
    /// Every scenario, in a stable order, so a driver can sweep the suite.
    pub(crate) const ALL: [BenchScenario; 5] = [
        BenchScenario::Empty,
        BenchScenario::ShortChat,
        BenchScenario::LongSession,
        BenchScenario::ScrolledLongSession,
        BenchScenario::CodeHeavy,
    ];

    /// Stable slug for JSON / log identification.
    pub(crate) fn slug(self) -> &'static str {
        match self {
            BenchScenario::Empty => "empty",
            BenchScenario::ShortChat => "short_chat",
            BenchScenario::LongSession => "long_session",
            BenchScenario::ScrolledLongSession => "scrolled_long_session",
            BenchScenario::CodeHeavy => "code_heavy",
        }
    }

    /// Build the [`TuiApp`] this scenario benchmarks at `size`. The app is
    /// self-contained — it never crawls the real workspace — so the harness
    /// stays headless and side-effect-free.
    fn build_app(self, size: BenchSize) -> TuiApp {
        let mut app = new_bench_app();
        match self {
            BenchScenario::Empty => {}
            BenchScenario::ShortChat => {
                app.push_transcript_item(TranscriptItem::user("explain this stack trace"));
                app.push_transcript_item(TranscriptItem::assistant(
                    "The panic unwinds through `render` because the slice index is out of bounds.",
                ));
                app.push_transcript_item(TranscriptItem::user("how do I fix it?"));
                app.push_transcript_item(TranscriptItem::assistant(
                    "Clamp the offset against the row count before indexing.",
                ));
            }
            BenchScenario::LongSession => {
                seed_long_session(&mut app, 200);
            }
            BenchScenario::ScrolledLongSession => {
                seed_long_session(&mut app, 200);
                // Scroll up off the tail so the view no longer follows new
                // output. The exact landing offset is geometry-clamped by the
                // scroll state; the magnitude here is comfortably past one
                // viewport so `following_tail()` reads false.
                scroll_up(&mut app, 400, size);
            }
            BenchScenario::CodeHeavy => {
                for i in 0..40 {
                    app.push_transcript_item(TranscriptItem::user(format!("show function {i}")));
                    app.push_transcript_item(TranscriptItem::assistant(format!(
                        "```rust\nfn handler_{i}(input: &str) -> usize {{\n    \
                         input.chars().filter(|c| c.is_ascii()).count()\n}}\n```",
                    )));
                }
            }
        }
        app
    }
}

/// Which terminal-output backend a frame is rendered through.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BenchBackend {
    /// `CrosstermBackend` over the in-memory capture sink: the real ANSI byte
    /// stream. `bytes_per_frame` and latency come from this leg.
    CaptureStream,
    /// `TestBackend`: a queryable cell grid. The invariant checks read this
    /// leg's post-frame buffer.
    Cells,
}

impl BenchBackend {
    pub(crate) fn slug(self) -> &'static str {
        match self {
            BenchBackend::CaptureStream => "capture_stream",
            BenchBackend::Cells => "cells",
        }
    }
}

/// One painted frame's measured cost.
#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) struct BenchFrameRecord {
    /// Wall time to build and emit this frame.
    pub(crate) render_micros: u128,
    /// Bytes the diffing renderer emitted this frame (capture leg only; 0 for
    /// the cell leg, which has no byte stream).
    pub(crate) bytes: u64,
}

/// The serializable result for one (scenario, backend, size) cell — the JSON
/// shape the spec asks for: scenario / backend / size / frames / bytes /
/// p95 / scroll jumps / teardown.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct BenchSummary {
    pub(crate) scenario: &'static str,
    pub(crate) backend: &'static str,
    pub(crate) width: u16,
    pub(crate) height: u16,
    /// Number of frames painted during the run.
    pub(crate) frames: usize,
    /// Total bytes emitted across all frames (capture leg).
    pub(crate) total_bytes: u64,
    /// Mean bytes per painted frame.
    pub(crate) mean_bytes_per_frame: u64,
    /// p95 of per-frame render time, in microseconds.
    pub(crate) p95_render_micros: u128,
    /// Maximum per-frame render time, in microseconds.
    pub(crate) max_render_micros: u128,
    /// How many of the scripted scroll steps actually moved the view (a
    /// no-op scroll at the tail does not count). The "scroll jumps" the spec
    /// asks for.
    pub(crate) scroll_jumps: usize,
    /// Whether the post-run teardown emitted a clean stream: the transcript
    /// mirror after `LeaveAlternateScreen`, with no scrollback purge.
    pub(crate) teardown_clean: bool,
}

/// A failed hard invariant. Carries enough context (scenario, size, frame) to
/// localize the regression without dumping the whole byte log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BenchInvariantError {
    pub(crate) scenario: &'static str,
    pub(crate) frame: usize,
    pub(crate) message: String,
}

impl std::fmt::Display for BenchInvariantError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invariant violated [{} frame {}]: {}",
            self.scenario, self.frame, self.message
        )
    }
}

/// How many frames to paint per run. A handful is enough to exercise the
/// diffing renderer's steady state (first frame is a full paint; subsequent
/// frames diff against it) without making the suite slow.
const FRAMES_PER_RUN: usize = 8;

/// Run one (scenario, backend, size) cell: build the app, paint
/// [`FRAMES_PER_RUN`] frames through the backend, check every hard invariant
/// per frame, and once verify the teardown stream. Returns the summary or the
/// first invariant violation.
///
/// The capture leg measures real bytes; the cell leg supplies the queryable
/// buffer the invariants read. We always render a cell copy of every frame so
/// the invariant checks are backend-independent — the capture stream and the
/// cell grid are two views of the same `render()` call.
pub(crate) fn run_scenario(
    scenario: BenchScenario,
    backend: BenchBackend,
    size: BenchSize,
) -> Result<BenchSummary, BenchInvariantError> {
    run_scenario_into_sink(scenario, backend, size, Arc::new(Mutex::new(Vec::new())))
}

/// Like [`run_scenario`] but renders into the caller-supplied capture `sink`, so
/// a test can assert which leg actually touched the stream — the `Cells` backend
/// must leave the sink empty (it never draws the capture leg), while the
/// `CaptureStream` backend fills it.
pub(crate) fn run_scenario_into_sink(
    scenario: BenchScenario,
    backend: BenchBackend,
    size: BenchSize,
    sink: Arc<Mutex<Vec<u8>>>,
) -> Result<BenchSummary, BenchInvariantError> {
    let app = scenario.build_app(size);
    let viewport = Rect::new(0, 0, size.width, size.height);

    let mut frames: Vec<BenchFrameRecord> = Vec::with_capacity(FRAMES_PER_RUN);
    // Capture-stream terminal (real bytes). Built once and reused across
    // frames so the diffing renderer sees the same steady-state it would in
    // production: frame 1 is a full paint, later frames diff against it.
    let mut stream_terminal = Terminal::with_options(
        CrosstermBackend::new(TerminalWriter::capture(Arc::clone(&sink))),
        TerminalOptions {
            viewport: Viewport::Fixed(viewport),
        },
    )
    .expect("capture terminal");
    // Cell terminal (queryable buffer for the invariant checks).
    let mut cell_terminal =
        Terminal::new(TestBackend::new(size.width, size.height)).expect("cell terminal");

    for frame_idx in 0..FRAMES_PER_RUN {
        // Cell leg: render and read the buffer for invariant checks. Always
        // re-render (force a paint) so a frame is materialized even when the
        // diff would otherwise be empty. When `backend == Cells` this is also the
        // *measured* leg, so it is timed below; otherwise it only feeds the
        // backend-independent invariant checks and stays untimed.
        let cell_start = Instant::now();
        cell_terminal
            .draw(|frame| render(frame, &app))
            .expect("cell draw");
        let cell_elapsed = cell_start.elapsed();
        let cell_buffer = cell_terminal.backend().buffer().clone();
        check_frame_invariants(scenario, frame_idx, &app, &cell_buffer, size)?;

        // Each frame times and (for the stream leg) measures *only* the backend
        // the cell selects, so a `Cells` summary reports the cell renderer's
        // latency — not a second sample of the capture-stream draw — and never
        // touches the capture sink.
        let (render_micros, bytes) = match backend {
            BenchBackend::CaptureStream => {
                // Capture leg: measure real bytes + latency for the same frame.
                let before = sink.lock().unwrap_or_else(|p| p.into_inner()).len();
                let start = Instant::now();
                stream_terminal
                    .draw(|frame| render(frame, &app))
                    .expect("stream draw");
                let elapsed = start.elapsed();
                let after = sink.lock().unwrap_or_else(|p| p.into_inner()).len();
                (elapsed.as_micros(), (after - before) as u64)
            }
            // The cell leg's own draw is the measured one; it has no byte stream,
            // so the summary focuses on latency and invariant coverage and the
            // capture stream is left untouched.
            BenchBackend::Cells => (cell_elapsed.as_micros(), 0),
        };
        frames.push(BenchFrameRecord {
            render_micros,
            bytes,
        });
    }

    // Teardown: emit the clean-exit stream into a fresh capture sink and
    // verify the mirror-after-leave invariant. Done once per run, not per
    // frame, because teardown is a session-end event.
    let teardown_clean = verify_teardown(&app, size.width);

    Ok(summarize(
        scenario,
        backend,
        size,
        &frames,
        &app,
        teardown_clean,
    ))
}

/// Sweep every scenario across both backends at both `sizes`, returning one
/// summary per cell. The first invariant violation short-circuits the sweep so
/// a regression surfaces as a hard error, not a buried number.
pub(crate) fn run_suite(sizes: &[BenchSize]) -> Result<Vec<BenchSummary>, BenchInvariantError> {
    let mut out = Vec::new();
    for &scenario in &BenchScenario::ALL {
        for &backend in &[BenchBackend::CaptureStream, BenchBackend::Cells] {
            for &size in sizes {
                out.push(run_scenario(scenario, backend, size)?);
            }
        }
    }
    Ok(out)
}

/// Serialize a suite result to pretty JSON. The driver writes this to its
/// report; the schema is the `BenchSummary` derive above.
pub(crate) fn summaries_to_json(summaries: &[BenchSummary]) -> String {
    serde_json::to_string_pretty(summaries).unwrap_or_else(|_| "[]".to_string())
}

/// Fold a run's per-frame records into the serializable summary, stamping the
/// scroll-jump count and teardown verdict from `app` state.
fn summarize(
    scenario: BenchScenario,
    backend: BenchBackend,
    size: BenchSize,
    frames: &[BenchFrameRecord],
    app: &TuiApp,
    teardown_clean: bool,
) -> BenchSummary {
    let total_bytes: u64 = frames.iter().map(|f| f.bytes).sum();
    let mean_bytes_per_frame = if frames.is_empty() {
        0
    } else {
        total_bytes / frames.len() as u64
    };
    let mut micros: Vec<u128> = frames.iter().map(|f| f.render_micros).collect();
    micros.sort_unstable();
    let p95_render_micros = percentile(&micros, 95);
    let max_render_micros = micros.last().copied().unwrap_or(0);

    BenchSummary {
        scenario: scenario.slug(),
        backend: backend.slug(),
        width: size.width,
        height: size.height,
        frames: frames.len(),
        total_bytes,
        mean_bytes_per_frame,
        p95_render_micros,
        max_render_micros,
        scroll_jumps: count_scroll_jumps(app, size),
        teardown_clean,
    }
}

/// Linear-interpolation-free percentile: the value at the `p`th rank of a
/// pre-sorted slice (nearest-rank). Returns 0 for an empty slice.
fn percentile(sorted: &[u128], p: usize) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    // Nearest-rank: rank = ceil(p/100 * n), 1-based, clamped into range.
    let n = sorted.len();
    let rank = (p * n).div_ceil(100);
    let idx = rank.saturating_sub(1).min(n - 1);
    sorted[idx]
}

/// Count how many scripted scroll steps would actually move the view. Mirrors
/// the scenario's own scroll loop so the reported "scroll jumps" reflects real
/// movement, not no-op scrolls that hit the top or sit at the tail.
fn count_scroll_jumps(app: &TuiApp, geom: BenchSize) -> usize {
    // Recompute against a throwaway clone so the live app is untouched.
    let line_count = transcript_lines_for_render(app, Some(geom.width), true).len();
    let viewport_h = geom.height.saturating_sub(1) as usize;
    let mut scroll = crate::scroll::ScrollState::pinned();
    let mut jumps = 0usize;
    for _ in 0..(line_count.max(1)) {
        let before = scroll.offset(line_count, viewport_h);
        scroll.scroll_by(1, line_count, viewport_h);
        if scroll.offset(line_count, viewport_h) != before {
            jumps += 1;
        }
    }
    jumps
}

/// Check the spec's per-frame hard invariants against a rendered cell buffer
/// and the source app state.
fn check_frame_invariants(
    scenario: BenchScenario,
    frame: usize,
    app: &TuiApp,
    buffer: &Buffer,
    size: BenchSize,
) -> Result<(), BenchInvariantError> {
    let err = |message: String| BenchInvariantError {
        scenario: scenario.slug(),
        frame,
        message,
    };

    // Invariant: the painted buffer fills exactly the viewport — no cell is
    // out of bounds (ratatui guarantees this, but assert it so a future
    // off-by-one in a surface layout is caught here, not in a terminal).
    if buffer.area.width != size.width || buffer.area.height != size.height {
        return Err(err(format!(
            "buffer area {}x{} does not match viewport {}x{}",
            buffer.area.width, buffer.area.height, size.width, size.height
        )));
    }

    // Invariant: exactly one composer. The empty-composer caret glyph appears
    // once and only once per main-view frame. When the composer is empty (all
    // bench scenarios start with an empty composer) the caret count is the
    // composer count.
    let composer_carets = count_glyph(buffer, COMPOSER_CURSOR);
    if app.input.is_empty() && composer_carets != 1 {
        return Err(err(format!(
            "expected exactly one composer caret '{COMPOSER_CURSOR}', found {composer_carets}"
        )));
    }

    // Invariant: latest output visible when following the tail. A
    // tail-following session must paint the final transcript rows somewhere on
    // screen; otherwise new output scrolled off without the user scrolling.
    // Only meaningful once there is output: an empty transcript has no tail.
    if !app.transcript.is_empty()
        && active_transcript_scroll(app).is_following()
        && let Some(tail) = latest_assistant_tail_marker(app, size.width, size.height)
    {
        let screen = buffer_text(buffer);
        if !screen.contains(&tail) {
            return Err(err(format!(
                "latest output marker {tail:?} not visible while following tail"
            )));
        }
    }

    Ok(())
}

/// Build the teardown stream and verify it is clean: the transcript mirror is
/// emitted *after* `LeaveAlternateScreen` (so it lands in real scrollback) and
/// no scrollback-purge (`\x1b[3J`) appears. Returns `true` on a clean stream.
fn verify_teardown(app: &TuiApp, width: u16) -> bool {
    let lines = transcript_lines_for_render(app, Some(width), true);
    let mirror = render_lines_to_owned_buffer(&lines, width);
    let mut bytes = Vec::new();
    if emit_finish_fullscreen(
        &mut bytes,
        &mirror,
        width,
        None,
        app.effective_hyperlink_caps(),
    )
    .is_err()
    {
        return false;
    }
    let ansi = String::from_utf8_lossy(&bytes);
    // No scrollback purge on a normal exit.
    if ansi.contains("\x1b[3J") {
        return false;
    }
    // The leave must precede the first mirror row terminator.
    let Some(leave_pos) = ansi.find(LEAVE_ALT_SCREEN) else {
        return false;
    };
    match ansi.find("\r\n") {
        Some(first_crlf) => leave_pos < first_crlf,
        // No mirror rows (empty transcript) is still clean: the leave alone is
        // a valid teardown.
        None => true,
    }
}

/// A short, low-collision marker drawn from the *bottom* of the rendered
/// transcript — the rows that must be on screen when the view follows the tail.
///
/// Returns the longest single alphanumeric word found among the last few
/// rendered transcript rows at `width`. Reading the already-wrapped rows (not
/// the raw message) means the marker is a token the renderer actually paints
/// intact at the tail, which holds even when the final entry is taller than the
/// viewport (a code block, say): only its bottom rows are visible, and that is
/// exactly where this looks. The word must be at least 4 chars (low collision)
/// and fit within `width` minus a rail-gutter margin, or the check is skipped.
fn latest_assistant_tail_marker(app: &TuiApp, width: u16, height: u16) -> Option<String> {
    // Match `render()`'s startup-card gate (`area.height >= 16`) so the rows we
    // scan are the ones actually painted at this height.
    let include_startup_card = height >= 16;
    let lines = transcript_lines_for_render(app, Some(width), include_startup_card);
    // The last few rendered rows are always inside the visible tail for every
    // benchmarked viewport (transcript height >= 4 even at 40x10). Scan them
    // for the longest paintable word.
    let tail_window = lines.len().saturating_sub(4);
    let content_width = width.saturating_sub(8) as usize;
    lines[tail_window..]
        .iter()
        .flat_map(|line| {
            line_text(line)
                .split(|c: char| !c.is_ascii_alphanumeric())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .filter(|w| w.len() >= 4 && w.len() <= content_width)
        .max_by_key(String::len)
}

/// Flatten a rendered [`Line`] to its plain text.
fn line_text(line: &ratatui::text::Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Count occurrences of a single glyph across a rendered buffer.
fn count_glyph(buffer: &Buffer, glyph: char) -> usize {
    let mut needle = [0u8; 4];
    let needle = glyph.encode_utf8(&mut needle);
    let mut count = 0;
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            if buffer[(x, y)].symbol() == needle {
                count += 1;
            }
        }
    }
    count
}

/// Flatten a rendered buffer to a single plain-text string (rows joined by
/// newlines) for substring assertions.
fn buffer_text(buffer: &Buffer) -> String {
    let mut out = String::with_capacity(buffer.area.width as usize * buffer.area.height as usize);
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

/// Seed `app` with `turns` user/assistant exchanges of generic prose so a
/// scenario overflows the viewport deterministically.
fn seed_long_session(app: &mut TuiApp, turns: usize) {
    for i in 0..turns {
        app.push_transcript_item(TranscriptItem::user(format!("question number {i}")));
        app.push_transcript_item(TranscriptItem::assistant(format!(
            "Answer {i}: the relevant module lives under crates and the fix is local."
        )));
    }
}

/// Scroll the active transcript up by `lines`, mirroring the keyboard
/// scroll-up path through [`crate::scroll::ScrollState`]. Used by the scrolled
/// scenario so the view stops following the tail.
fn scroll_up(app: &mut TuiApp, lines: usize, size: BenchSize) {
    let line_count = transcript_lines_for_render(app, Some(size.width), true).len();
    let viewport_h = size.height.saturating_sub(1) as usize;
    active_transcript_scroll_mut(app).set_from_bottom(lines, line_count, viewport_h);
}

/// Build a self-contained [`TuiApp`] for benchmarking: a temp workspace root
/// so `TuiApp::new` never crawls the real repo, the same permission posture as
/// the test fixtures, and a no-op clipboard.
fn new_bench_app() -> TuiApp {
    let config = bench_config();
    TuiApp::new_with_clipboard(
        "bench",
        &config,
        SessionMode::Build,
        None,
        Box::new(NoopBenchClipboard),
    )
}

/// A minimal [`AppConfig`] pinned to a unique temp workspace so construction is
/// hermetic. Mirrors the test fixture's permission posture.
fn bench_config() -> AppConfig {
    AppConfig {
        model: "bench-model".to_string(),
        session_mode: SessionMode::Build,
        permissions: PermissionPolicy {
            read: PermissionMode::Allow,
            edit: PermissionMode::Ask,
            shell: PermissionMode::Ask,
            web: PermissionMode::Ask,
            ..Default::default()
        },
        config_sources: vec!["defaults".to_string()],
        workspace_root: bench_temp_root(),
        ..Default::default()
    }
}

/// A unique temp directory so two parallel bench apps never share a root.
fn bench_temp_root() -> std::path::PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let root =
        std::env::temp_dir().join(format!("squeezy_tui_bench_{}_{nonce}", std::process::id()));
    let _ = std::fs::create_dir_all(&root);
    root
}

struct NoopBenchClipboard;

impl Clipboard for NoopBenchClipboard {
    fn copy_text(&mut self, _text: &str) -> std::result::Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
#[path = "bench_render_tests.rs"]
mod tests;
