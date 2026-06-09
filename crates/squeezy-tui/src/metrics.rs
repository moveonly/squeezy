//! Per-frame render-budget instrumentation (Phase 8).
//!
//! The render path had no timing, byte, row, or wrap-cost telemetry: the only
//! `Instant` work in the loop was the frame-rate gate and a one-shot startup
//! trace. This module adds a tiny [`RenderMetrics`] struct, stamped once per
//! *painted* frame in `TerminalGuard::draw_app`, capturing:
//!
//! - **render time** — wall time to build AND emit the frame (the full
//!   `draw`/`paint_main` call, measured at the single `draw_app` chokepoint).
//! - **bytes emitted** — every byte handed to the terminal writer this frame,
//!   counted by an `Arc<AtomicU64>` the writer bumps on each `write` (so it
//!   covers both the fullscreen ratatui path and the inline escape hatch).
//! - **rows built** — the wrapped transcript rows materialized for the main
//!   view this frame (stamped by `render_transcript`).
//! - **cache hit / miss** — a delta of the process-wide cache counters
//!   (`main_render_cache::cache_stats`) across the frame, so the HUD shows how
//!   many entry/main-render lookups this frame hit vs. recomputed.
//! - **longest single-entry wrap** — the slowest per-entry wrap `compute`
//!   closure this frame, tracked as a running max in `main_render_cache` and
//!   snapshotted per frame.
//!
//! ## Idle-redraw contract
//!
//! Metrics are stamped ONLY inside `draw_app`, which the main loop calls only
//! when `wants_draw` (state changed / resize / animation). An idle frame paints
//! nothing and therefore churns no metrics — the struct keeps its previous
//! values until the next real paint. This preserves the "zero idle work"
//! invariant the redraw gate guarantees.
//!
//! ## Hot-path cost
//!
//! The instrumentation is two `Instant::now()` calls bracketing the draw, two
//! relaxed atomic loads (byte counter, cache stats) and a couple of field
//! writes. None of it scales with transcript size, so timing never dominates
//! the render it measures. The byte counter is a single relaxed `fetch_add` per
//! `write`, already the granularity crossterm batches at.
//!
//! ## Exposure
//!
//! Two low-friction surfaces, both off by default:
//! 1. A hidden HUD ([`RenderMetrics::hud_lines`]) painted in the top-right of
//!    the main view when `TuiApp::show_render_metrics` is set. The flag is
//!    seeded from `SQUEEZY_RENDER_METRICS` at startup and flipped at runtime by
//!    [`TuiApp::toggle_render_metrics`], so it never shows unless explicitly
//!    asked for.
//! 2. A `tracing::debug!` line emitted at most once per painted frame (see
//!    `draw_app`), gated on the same flag so a normal session logs nothing.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Shared, monotonic byte counter the terminal writer bumps on every write.
/// `draw_app` clones the handle, samples it at frame begin/end, and the
/// difference is the bytes that frame emitted. An `Arc<AtomicU64>` (not a plain
/// field) because the counter lives inside the `TerminalWriter` owned by the
/// crossterm backend, while the reader is the guard around it.
pub(crate) type ByteCounter = Arc<AtomicU64>;

/// A snapshot of one painted frame's render budget. Stamped per frame in
/// `draw_app`; read by the HUD and the trace line. `Copy` + small so storing it
/// on the app and cloning it for the HUD is free.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RenderMetrics {
    /// Wall time to build and emit the most recent frame.
    pub(crate) render_time: Duration,
    /// Bytes written to the terminal for the most recent frame.
    pub(crate) bytes_emitted: u64,
    /// Wrapped transcript rows materialized for the main view this frame.
    pub(crate) rows_built: usize,
    /// Cache lookups that hit this frame (main-render + per-entry-wrap).
    pub(crate) cache_hits: u64,
    /// Cache lookups that missed (recomputed) this frame.
    pub(crate) cache_misses: u64,
    /// Slowest single-entry wrap `compute` this frame (zero when every entry
    /// was a cache hit and nothing was wrapped).
    pub(crate) longest_entry_wrap: Duration,
    /// Total painted frames since process start. Lets the HUD/trace show a
    /// frame ordinal so a static screenshot is unambiguous about which frame it
    /// captured.
    pub(crate) frame: u64,
}

impl RenderMetrics {
    /// The HUD text: one compact line per metric, newest frame's values. Kept
    /// to short fixed-width labels so the box stays narrow in the top-right
    /// corner and never competes with the transcript for room.
    pub(crate) fn hud_lines(&self) -> Vec<String> {
        let hit_total = self.cache_hits + self.cache_misses;
        let hit_rate = if hit_total == 0 {
            100.0
        } else {
            (self.cache_hits as f64 / hit_total as f64) * 100.0
        };
        vec![
            format!("frame   {}", self.frame),
            format!("render  {}", fmt_dur(self.render_time)),
            format!("bytes   {}", self.bytes_emitted),
            format!("rows    {}", self.rows_built),
            format!(
                "cache   {}/{} ({:.0}%)",
                self.cache_hits, hit_total, hit_rate
            ),
            format!("wrap    {}", fmt_dur(self.longest_entry_wrap)),
        ]
    }

    /// One-line summary for the per-frame `tracing::debug!`. Field-per-key so a
    /// structured-log consumer can pivot, but also readable in a plain console.
    pub(crate) fn trace_summary(&self) -> String {
        format!(
            "frame={} render={} bytes={} rows={} cache_hits={} cache_misses={} longest_wrap={}",
            self.frame,
            fmt_dur(self.render_time),
            self.bytes_emitted,
            self.rows_built,
            self.cache_hits,
            self.cache_misses,
            fmt_dur(self.longest_entry_wrap),
        )
    }
}

/// Format a `Duration` compactly: microseconds under 1 ms, else milliseconds
/// with one decimal. Keeps the HUD column narrow and the trace line readable.
fn fmt_dur(d: Duration) -> String {
    let us = d.as_micros();
    if us < 1000 {
        format!("{us}µs")
    } else {
        format!("{:.1}ms", d.as_secs_f64() * 1000.0)
    }
}

/// Running max of the slowest per-entry wrap `compute` observed since the last
/// reset. `main_render_cache::get_or_compute_entry_wrap` records each wrap here;
/// `draw_app` resets it at frame begin and reads it at frame end so the value is
/// per-frame. Stored as nanoseconds in an atomic so the recording site needs no
/// lock on the hot wrap path.
static LONGEST_WRAP_NANOS: AtomicU64 = AtomicU64::new(0);

/// Record one per-entry wrap duration, keeping the per-frame running max.
/// Called by the wrap cache's miss branch. Lock-free `fetch_max`.
pub(crate) fn record_entry_wrap(elapsed: Duration) {
    let nanos = elapsed.as_nanos().min(u128::from(u64::MAX)) as u64;
    LONGEST_WRAP_NANOS.fetch_max(nanos, Ordering::Relaxed);
}

/// Reset the per-frame longest-wrap accumulator. Called at frame begin.
pub(crate) fn reset_longest_entry_wrap() {
    LONGEST_WRAP_NANOS.store(0, Ordering::Relaxed);
}

/// Read the per-frame longest-wrap accumulator. Called at frame end.
pub(crate) fn longest_entry_wrap() -> Duration {
    Duration::from_nanos(LONGEST_WRAP_NANOS.load(Ordering::Relaxed))
}

/// Per-frame rows-built accumulator. `render_transcript` stamps the wrapped-row
/// count it painted; `draw_app` resets it at frame begin (so a frame that does
/// not paint the main view — e.g. the config screen — reads zero) and snapshots
/// it at frame end. Process-wide atomic so it needs no `&mut TuiApp` plumbing
/// through the render closure, which only has `&TuiApp`.
static ROWS_BUILT: AtomicU64 = AtomicU64::new(0);

/// Record the wrapped-row count painted for the main view this frame.
pub(crate) fn record_rows_built(rows: usize) {
    ROWS_BUILT.store(rows as u64, Ordering::Relaxed);
}

/// Reset the per-frame rows-built accumulator. Called at frame begin.
pub(crate) fn reset_rows_built() {
    ROWS_BUILT.store(0, Ordering::Relaxed);
}

/// Read the per-frame rows-built accumulator. Called at frame end.
pub(crate) fn rows_built() -> u64 {
    ROWS_BUILT.load(Ordering::Relaxed)
}

#[cfg(test)]
#[path = "metrics_tests.rs"]
mod tests;
