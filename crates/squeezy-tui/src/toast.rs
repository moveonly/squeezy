//! Corner toast notifications with auto-dismiss.
//!
//! Toasts overlay the top-right of the frame, stack up to three at a time
//! (newest at top), and disappear on their own after a few seconds. They
//! are intended for fire-and-forget status events — telemetry flushed, MCP
//! server connected, index updated — that should be visible at a glance
//! without stealing space from the transcript or the prompt. Durable
//! notices land in the transcript instead.
//!
//! The queue itself is layout-agnostic: callers push toasts, call [`tick`]
//! from the animation loop, and consult [`ToastQueue::visible`] when
//! rendering.
//!
//! [`tick`]: ToastQueue::tick

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use ratatui::style::Color;

/// How long a freshly pushed toast stays on screen before auto-dismiss.
/// 5s is long enough to read a one-line notice without forcing the user
/// to dismiss it manually, short enough that stale toasts don't pile up.
pub(crate) const DEFAULT_TOAST_TTL: Duration = Duration::from_secs(5);

/// Cap on simultaneously visible toasts. Three is enough to absorb a small
/// burst (e.g. MCP connect + index ready + telemetry flush) without the
/// overlay drifting down the screen.
pub(crate) const MAX_VISIBLE_TOASTS: usize = 3;

/// Visual variant. Maps directly to a palette colour for the leading glyph
/// and border, using the standard info / success / warning / error
/// vocabulary so call sites can use whichever surface fits the event.
//
// `allow(dead_code)`: variants are constructed by the test suite and by
// future consumers (telemetry flush, MCP connect, index ready) wired in
// follow-up tickets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum ToastVariant {
    Info,
    Success,
    Warning,
    Error,
}

impl ToastVariant {
    pub(crate) fn color(self) -> Color {
        match self {
            // Mirror the rail's message taxonomy so a toast reads the same as its
            // on-rail equivalent and amber stays rationed: info = blue (cool,
            // not gold), warning = cyan (the warn tier), success = green,
            // error = red.
            Self::Info => crate::render::theme::blue(),
            Self::Success => crate::render::theme::green(),
            Self::Warning => crate::render::theme::cyan(),
            Self::Error => crate::render::theme::red(),
        }
    }

    pub(crate) const fn glyph(self) -> &'static str {
        match self {
            Self::Info => "ℹ",
            Self::Success => "✓",
            Self::Warning => "⚠",
            Self::Error => "✖",
        }
    }
}

/// A single toast entry. `dismissed_at` is the absolute deadline at which
/// the toast should be pruned. Storing the deadline instead of `(created,
/// ttl)` keeps `expired()` to a single comparison and lets callers tune the
/// lifetime per push without complicating the queue.
#[derive(Debug, Clone)]
pub(crate) struct Toast {
    pub message: String,
    pub variant: ToastVariant,
    pub dismissed_at: Instant,
}

impl Toast {
    pub(crate) fn expired(&self) -> bool {
        Instant::now() >= self.dismissed_at
    }
}

/// FIFO queue of toasts with newest-on-top render order.
///
/// Maintained internally as a `VecDeque` in insertion order so existing
/// entries shift naturally when older ones are pruned. [`visible`] reverses
/// the slice it returns so the caller draws newest first — matching the
/// "stack grows downward" expectation.
///
/// [`visible`]: ToastQueue::visible
pub(crate) struct ToastQueue {
    items: VecDeque<Toast>,
}

impl Default for ToastQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl ToastQueue {
    pub(crate) fn new() -> Self {
        Self {
            items: VecDeque::new(),
        }
    }

    /// Push a toast with [`DEFAULT_TOAST_TTL`].
    #[allow(dead_code)] // primary entry point for consumers wired in follow-up tickets
    pub(crate) fn push(&mut self, message: impl Into<String>, variant: ToastVariant) {
        self.push_with_ttl(message, variant, DEFAULT_TOAST_TTL);
    }

    /// Push a toast with an explicit ttl. When the queue is already at
    /// [`MAX_VISIBLE_TOASTS`], the oldest entry is dropped to make room so
    /// the visible stack height stays bounded.
    #[allow(dead_code)] // used by tests today; production sites land in follow-up tickets
    pub(crate) fn push_with_ttl(
        &mut self,
        message: impl Into<String>,
        variant: ToastVariant,
        ttl: Duration,
    ) {
        let toast = Toast {
            message: message.into(),
            variant,
            dismissed_at: Instant::now() + ttl,
        };
        while self.items.len() >= MAX_VISIBLE_TOASTS {
            self.items.pop_front();
        }
        self.items.push_back(toast);
    }

    /// Drop expired toasts. Idempotent; safe to call on every animation
    /// tick. Returns `true` when the visible set changed so the caller can
    /// schedule a redraw.
    pub(crate) fn tick(&mut self) -> bool {
        let before = self.items.len();
        self.items.retain(|t| !t.expired());
        before != self.items.len()
    }

    /// Drop every toast. Returns how many were removed.
    pub(crate) fn clear(&mut self) -> usize {
        let n = self.items.len();
        self.items.clear();
        n
    }

    #[allow(dead_code)] // probed by callers that gate redraws on whether anything is visible
    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    #[allow(dead_code)] // exposed for status-line counters and tests
    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    /// Snapshot of currently visible toasts ordered newest-first. Returned
    /// as an owned `Vec` because the public API hides the underlying deque;
    /// the slice is tiny (≤3 entries) so the allocation is negligible.
    pub(crate) fn visible(&self) -> Vec<&Toast> {
        self.items.iter().rev().collect()
    }
}

#[cfg(test)]
#[path = "toast_tests.rs"]
mod tests;
