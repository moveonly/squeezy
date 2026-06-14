//! UX latency budgets (§12.10.1).
//!
//! Phase 8 added [`crate::metrics::RenderMetrics`]: a per-*frame* render-budget
//! snapshot (time, bytes, rows, cache hit/miss, longest wrap). That answers "how
//! expensive was the last paint?" but says nothing about *which interaction*
//! drove the paint or whether it met a latency target the user can feel.
//!
//! This module adds the missing half: a per-*interaction* latency budget. The
//! event loop tags each painted frame with the [`InteractionKind`] that woke it
//! (a keypress, a scroll, a page jump, a queue drag, a paste preview, a copy
//! ack, a search jump, or a resize redraw). Each tagged paint feeds the frame's
//! render time into a [`LatencyTracker`], which keeps a tiny bounded ring of
//! recent samples per interaction, computes p95/p99, and compares them against a
//! compiled-in [`TuiLatencyBudget`]. A violation (a percentile over budget) is
//! remembered as the [`LastViolation`] and surfaced in the hidden render-metrics
//! overlay and the per-frame trace line.
//!
//! ## What is measured
//!
//! The render time the tracker records is the *app-side* `draw_app` wall time
//! already measured by Phase 8 — the render-and-emit window only: ratatui render
//! → crossterm write+flush, bracketed by the timer that starts at the top of
//! `draw_app` and is read at frame end. Event dispatch and state mutation run
//! earlier, in the event loop (`handle_input_event` → `handle_key`), *before*
//! `draw_app` is called, so they are NOT included in this sample. PTY / terminal
//! flush jitter past the process boundary is also deliberately excluded: the
//! spec calls that out as noisy and report-only, so the budgets here gate the
//! render-and-emit part Squeezy actually controls inside `draw_app`.
//!
//! ## Idle-redraw contract
//!
//! The tracker only records when the event loop tags a frame with an interaction
//! AND that frame actually paints. An idle frame carries no tag (the loop clears
//! it after every paint and never sets it on a timer/animation tick), so the
//! tracker churns nothing while idle — preserving the zero-idle-work invariant.
//! No background timer, no allocation on the idle path.
//!
//! ## Cost
//!
//! Recording a sample is one push into a fixed 64-slot ring (no allocation after
//! construction) plus an O(n) percentile over ≤64 elements when the overlay asks
//! for it — never on the hot path. Tagging is a single enum write. None of it
//! scales with transcript size.

#![cfg_attr(not(unix), allow(dead_code))]

use std::time::Duration;

/// How many recent samples each interaction keeps for its percentile window.
/// Small on purpose: latency is interactive, so a short window tracks the
/// *current* feel rather than smearing a slow burst across a whole session, and
/// it keeps the ring allocation-free and the percentile scan trivially cheap.
const WINDOW: usize = 64;

/// The set of interactions that carry a latency budget. Mirrors the spec's
/// enumeration (keypress echo, scroll, page jumps, queue drag, paste preview,
/// copy ack, search jump, resize redraw). Each variant maps to a compiled-in
/// [`TuiLatencyBudget`] via [`InteractionKind::budget`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum InteractionKind {
    /// A composer keystroke echoing to the screen — the tightest budget, since
    /// it is the most felt latency in the whole app.
    KeypressEcho,
    /// A line/wheel scroll of the transcript.
    Scroll,
    /// A PageUp / PageDown / Home / End viewport jump.
    PageJump,
    /// A prompt-queue drag-reorder frame.
    QueueDrag,
    /// Rendering the paste-transform preview after a bracketed paste.
    PastePreview,
    /// Painting the "copied" acknowledgement after a copy command.
    CopyAck,
    /// Scrolling a search match into view (find / next / prev).
    SearchJump,
    /// Repainting the whole surface after a terminal resize — the loosest
    /// budget, since a full reflow legitimately costs more.
    ResizeRedraw,
}

impl InteractionKind {
    /// Every variant, in display order. Drives the overlay table and the
    /// exhaustiveness tests.
    pub(crate) const ALL: &'static [InteractionKind] = &[
        InteractionKind::KeypressEcho,
        InteractionKind::Scroll,
        InteractionKind::PageJump,
        InteractionKind::QueueDrag,
        InteractionKind::PastePreview,
        InteractionKind::CopyAck,
        InteractionKind::SearchJump,
        InteractionKind::ResizeRedraw,
    ];

    /// A short fixed-width label for the overlay / trace line. Padded to a
    /// common width so the overlay columns line up.
    pub(crate) fn label(self) -> &'static str {
        match self {
            InteractionKind::KeypressEcho => "keypress",
            InteractionKind::Scroll => "scroll  ",
            InteractionKind::PageJump => "pagejump",
            InteractionKind::QueueDrag => "qdrag   ",
            InteractionKind::PastePreview => "paste   ",
            InteractionKind::CopyAck => "copyack ",
            InteractionKind::SearchJump => "search  ",
            InteractionKind::ResizeRedraw => "resize  ",
        }
    }

    /// The compiled-in p95/p99 budget for this interaction. These are
    /// app-side targets (see the module doc): the time `draw_app` spends
    /// building and emitting the frame, not wall-clock terminal latency. Values
    /// are deliberately generous relative to a warm in-memory render so that a
    /// violation means a real regression, not normal scheduler jitter.
    pub(crate) fn budget(self) -> TuiLatencyBudget {
        match self {
            // Keypress echo is the most felt; hold it tight.
            InteractionKind::KeypressEcho => {
                TuiLatencyBudget::new(Duration::from_millis(8), Duration::from_millis(16))
            }
            InteractionKind::Scroll => {
                TuiLatencyBudget::new(Duration::from_millis(12), Duration::from_millis(24))
            }
            InteractionKind::PageJump => {
                TuiLatencyBudget::new(Duration::from_millis(16), Duration::from_millis(32))
            }
            InteractionKind::QueueDrag => {
                TuiLatencyBudget::new(Duration::from_millis(12), Duration::from_millis(24))
            }
            InteractionKind::PastePreview => {
                TuiLatencyBudget::new(Duration::from_millis(24), Duration::from_millis(48))
            }
            InteractionKind::CopyAck => {
                TuiLatencyBudget::new(Duration::from_millis(16), Duration::from_millis(32))
            }
            InteractionKind::SearchJump => {
                TuiLatencyBudget::new(Duration::from_millis(16), Duration::from_millis(32))
            }
            // A full reflow legitimately costs more than a keystroke.
            InteractionKind::ResizeRedraw => {
                TuiLatencyBudget::new(Duration::from_millis(33), Duration::from_millis(66))
            }
        }
    }
}

/// A measurable p95/p99 latency target for one interaction. `p99 >= p95` by
/// construction (the constructor clamps a mis-ordered pair so a budget can never
/// flag p99 as "better" than p95).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TuiLatencyBudget {
    pub(crate) p95: Duration,
    pub(crate) p99: Duration,
}

impl TuiLatencyBudget {
    /// Build a budget, clamping so `p99 >= p95`.
    pub(crate) fn new(p95: Duration, p99: Duration) -> Self {
        Self {
            p95,
            p99: p99.max(p95),
        }
    }
}

/// A bounded ring of the most recent render durations for a single interaction.
/// Fixed capacity ([`WINDOW`]); once full it overwrites the oldest sample. No
/// allocation after construction.
#[derive(Clone, Debug)]
struct Samples {
    buf: [Duration; WINDOW],
    /// Number of slots written so far, capped at `WINDOW`.
    len: usize,
    /// Next write position (wraps at `WINDOW`).
    next: usize,
}

impl Default for Samples {
    fn default() -> Self {
        Self {
            buf: [Duration::ZERO; WINDOW],
            len: 0,
            next: 0,
        }
    }
}

impl Samples {
    fn record(&mut self, d: Duration) {
        self.buf[self.next] = d;
        self.next = (self.next + 1) % WINDOW;
        if self.len < WINDOW {
            self.len += 1;
        }
    }

    /// The `pct`-th percentile (0.0..=1.0) of the recorded samples using the
    /// nearest-rank method, or `None` when nothing has been recorded yet.
    fn percentile(&self, pct: f64) -> Option<Duration> {
        if self.len == 0 {
            return None;
        }
        let mut sorted: Vec<Duration> = self.buf[..self.len].to_vec();
        sorted.sort_unstable();
        // Nearest-rank: rank = ceil(pct * n), 1-based, clamped to [1, n].
        let n = sorted.len();
        let rank = (pct * n as f64).ceil() as usize;
        let idx = rank.clamp(1, n) - 1;
        Some(sorted[idx])
    }
}

/// The most recent budget violation, kept so the overlay can show "what last
/// blew the budget" even after the offending samples age out of the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LastViolation {
    pub(crate) kind: InteractionKind,
    /// Which percentile broke (95 or 99).
    pub(crate) percentile: u8,
    /// The observed percentile value at the moment of the violation.
    pub(crate) observed: Duration,
    /// The budget it exceeded.
    pub(crate) budget: Duration,
    /// The frame ordinal (from [`crate::metrics::RenderMetrics::frame`]) on
    /// which it was detected, so a static overlay screenshot is unambiguous.
    pub(crate) frame: u64,
}

/// Per-interaction latency tracker: a ring of recent render times for each
/// [`InteractionKind`], plus the last detected budget violation.
///
/// Lives on `TuiApp`; `draw_app` calls [`LatencyTracker::record`] once per
/// painted-and-tagged frame. Cheap to default-construct (no heap until a sample
/// lands, and even then only the percentile scan allocates a tiny scratch vec).
#[derive(Clone, Debug, Default)]
pub(crate) struct LatencyTracker {
    samples: std::collections::BTreeMap<InteractionKind, Samples>,
    last_violation: Option<LastViolation>,
}

impl LatencyTracker {
    /// Record one render duration for `kind`, observed on frame `frame`, and
    /// re-evaluate the budget. If a percentile now exceeds the budget, update
    /// [`Self::last_violation`] and return `Some` describing it. Returns `None`
    /// when the interaction is within budget.
    pub(crate) fn record(
        &mut self,
        kind: InteractionKind,
        render_time: Duration,
        frame: u64,
    ) -> Option<LastViolation> {
        let entry = self.samples.entry(kind).or_default();
        entry.record(render_time);
        let budget = kind.budget();
        // Evaluate p99 first: a p99 breach is the stronger signal, and a single
        // record should produce at most one remembered violation.
        let checks = [
            (99u8, entry.percentile(0.99), budget.p99),
            (95u8, entry.percentile(0.95), budget.p95),
        ];
        for (pct, observed, limit) in checks {
            let Some(observed) = observed else { continue };
            if observed > limit {
                let violation = LastViolation {
                    kind,
                    percentile: pct,
                    observed,
                    budget: limit,
                    frame,
                };
                self.last_violation = Some(violation);
                return Some(violation);
            }
        }
        None
    }

    /// The last detected budget violation, if any.
    pub(crate) fn last_violation(&self) -> Option<LastViolation> {
        self.last_violation
    }

    /// The current p95/p99 for `kind`, or `None` if nothing was recorded yet.
    pub(crate) fn percentiles(&self, kind: InteractionKind) -> Option<(Duration, Duration)> {
        let s = self.samples.get(&kind)?;
        Some((s.percentile(0.95)?, s.percentile(0.99)?))
    }

    /// How many interactions have at least one recorded sample. Used by the
    /// overlay to decide whether to paint the latency panel at all.
    pub(crate) fn observed_kinds(&self) -> usize {
        self.samples.values().filter(|s| s.len > 0).count()
    }

    /// The overlay block: a header, one row per *observed* interaction showing
    /// p95/p99 against budget (with a `!` marker when over), and a final line
    /// for the last violation. Returns an empty vec when nothing has been
    /// recorded, so the overlay reserves no space on a fresh session.
    pub(crate) fn overlay_lines(&self) -> Vec<String> {
        if self.observed_kinds() == 0 {
            return Vec::new();
        }
        let mut lines = vec!["latency p95/p99 (budget)".to_string()];
        for kind in InteractionKind::ALL {
            let Some((p95, p99)) = self.percentiles(*kind) else {
                continue;
            };
            let budget = kind.budget();
            let over = p95 > budget.p95 || p99 > budget.p99;
            let marker = if over { "!" } else { " " };
            lines.push(format!(
                "{}{} {}/{} ({}/{})",
                marker,
                kind.label(),
                fmt_dur(p95),
                fmt_dur(p99),
                fmt_dur(budget.p95),
                fmt_dur(budget.p99),
            ));
        }
        if let Some(v) = self.last_violation() {
            lines.push(format!(
                "last !{} p{} {} > {} @f{}",
                kind_short(v.kind),
                v.percentile,
                fmt_dur(v.observed),
                fmt_dur(v.budget),
                v.frame,
            ));
        }
        lines
    }
}

/// A trimmed (no padding) interaction label for the compact "last violation"
/// line, where fixed-width columns are not needed.
fn kind_short(kind: InteractionKind) -> &'static str {
    kind.label().trim_end()
}

/// Format a `Duration` compactly: microseconds under 1 ms, else milliseconds
/// with one decimal. Mirrors `metrics::fmt_dur` so the two debug surfaces read
/// identically; kept module-local to avoid widening `metrics`' visibility.
fn fmt_dur(d: Duration) -> String {
    let us = d.as_micros();
    if us < 1000 {
        format!("{us}µs")
    } else {
        format!("{:.1}ms", d.as_secs_f64() * 1000.0)
    }
}

#[cfg(test)]
#[path = "latency_tests.rs"]
mod tests;
