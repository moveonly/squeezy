//! Corner toast notifications with auto-dismiss.
//!
//! Distinct from [`crate::notification::NotificationQueue`], which rotates a
//! single inline pane above the status bar. Toasts overlay the top-right of
//! the frame, stack up to three at a time (newest at top), and disappear on
//! their own after a few seconds. They are intended for fire-and-forget
//! status events — telemetry flushed, MCP server connected, index updated —
//! that should be visible at a glance without stealing space from the
//! transcript or the prompt.
//!
//! The queue itself is layout-agnostic: callers push toasts, call [`tick`]
//! from the same animation loop that drives the inline notification queue,
//! and consult [`ToastQueue::visible`] when rendering.
//!
//! [`tick`]: ToastQueue::tick

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use ratatui::style::Color;

use crate::render::palette::{AMBER, ERROR_RED, GOLD, SUCCESS_GREEN};

/// How long a freshly pushed toast stays on screen before auto-dismiss.
/// Mirrors OpenCode's `ui/toast.tsx` default.
pub(crate) const DEFAULT_TOAST_TTL: Duration = Duration::from_secs(5);

/// Cap on simultaneously visible toasts. Three is enough to absorb a small
/// burst (e.g. MCP connect + index ready + telemetry flush) without the
/// overlay drifting down the screen.
pub(crate) const MAX_VISIBLE_TOASTS: usize = 3;

/// Visual variant. Maps directly to a palette colour for the leading glyph
/// and border, matching the [`crate::notification::Severity`] vocabulary so
/// call sites can use whichever surface fits the event.
//
// `allow(dead_code)`: variants are constructed by the test suite and by
// future consumers (telemetry flush, MCP connect, index ready) wired in
// follow-up tickets — see audits/opencode-comparison-2026-05-25/06-ui.md
// recommendations for the call sites the audit identified.
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
            Self::Info => AMBER,
            Self::Success => SUCCESS_GREEN,
            Self::Warning => GOLD,
            Self::Error => ERROR_RED,
        }
    }

    pub(crate) const fn glyph(self) -> &'static str {
        match self {
            Self::Info => "ℹ",
            Self::Success => "✓",
            Self::Warning => "!",
            Self::Error => "✗",
        }
    }
}

/// A single toast entry. `dismissed_at` is the absolute deadline at which
/// the toast should be pruned. Storing the deadline instead of `(created,
/// ttl)` keeps `expired()` to a single comparison and lets callers tune the
/// lifetime per push without complicating the queue.
#[derive(Debug, Clone)]
pub(crate) struct Toast {
    pub id: u64,
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
    #[allow(dead_code)] // assigned on push; read by future dismissal/coalesce paths
    next_id: u64,
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
            next_id: 1,
        }
    }

    /// Push a toast with [`DEFAULT_TOAST_TTL`] and return its id.
    #[allow(dead_code)] // primary entry point for consumers wired in follow-up tickets
    pub(crate) fn push(&mut self, message: impl Into<String>, variant: ToastVariant) -> u64 {
        self.push_with_ttl(message, variant, DEFAULT_TOAST_TTL)
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
    ) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let toast = Toast {
            id,
            message: message.into(),
            variant,
            dismissed_at: Instant::now() + ttl,
        };
        while self.items.len() >= MAX_VISIBLE_TOASTS {
            self.items.pop_front();
        }
        self.items.push_back(toast);
        id
    }

    /// Drop expired toasts. Idempotent; safe to call on every animation
    /// tick. Returns `true` when the visible set changed so the caller can
    /// schedule a redraw.
    pub(crate) fn tick(&mut self) -> bool {
        let before = self.items.len();
        self.items.retain(|t| !t.expired());
        before != self.items.len()
    }

    /// Drop a single toast by id. Silent no-op when the id is unknown.
    #[allow(dead_code)] // wired for future user-driven dismissal keys
    pub(crate) fn dismiss(&mut self, id: u64) -> bool {
        if let Some(pos) = self.items.iter().position(|t| t.id == id) {
            self.items.remove(pos);
            true
        } else {
            false
        }
    }

    /// Drop every toast. Returns how many were removed.
    #[allow(dead_code)] // exposed for /clear and shutdown paths
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
