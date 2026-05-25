//! Reusable rotating notification queue for fire-and-forget status messages.
//!
//! Renders one line above the status bar. Multiple items rotate every ~10 s.
//! Each item auto-dismisses when its ttl expires. When empty, the pane
//! reserves zero rows so the rest of the layout stays compact.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use ratatui::style::Color;

use crate::render::palette::{AMBER, ERROR_RED, GOLD, QUIET, SUCCESS_GREEN};

/// Visual severity. Drives the fg color used to render the line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Severity {
    Info,
    Success,
    Warn,
    Error,
}

impl Severity {
    pub(crate) fn color(self) -> Color {
        match self {
            Self::Info => AMBER,
            Self::Success => SUCCESS_GREEN,
            Self::Warn => GOLD,
            Self::Error => ERROR_RED,
        }
    }

    pub(crate) const fn glyph(self) -> &'static str {
        match self {
            Self::Info => "ℹ",
            Self::Success => "✓",
            Self::Warn => "!",
            Self::Error => "✗",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Notification {
    pub id: u64,
    pub message: String,
    pub severity: Severity,
    pub created_at: Instant,
    pub ttl: Duration,
    pub action_hint: Option<&'static str>,
}

impl Notification {
    pub(crate) fn elapsed(&self) -> Duration {
        self.created_at.elapsed()
    }

    pub(crate) fn expired(&self) -> bool {
        self.elapsed() >= self.ttl
    }

    pub(crate) fn remaining(&self) -> Duration {
        self.ttl.saturating_sub(self.elapsed())
    }
}

pub(crate) const DEFAULT_TTL: Duration = Duration::from_secs(10);
pub(crate) const DEFAULT_ROTATE_EVERY: Duration = Duration::from_secs(10);

pub(crate) struct NotificationQueue {
    items: VecDeque<Notification>,
    current_index: usize,
    last_rotate: Instant,
    rotate_every: Duration,
    next_id: u64,
}

impl Default for NotificationQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl NotificationQueue {
    pub(crate) fn new() -> Self {
        Self {
            items: VecDeque::new(),
            current_index: 0,
            last_rotate: Instant::now(),
            rotate_every: DEFAULT_ROTATE_EVERY,
            next_id: 1,
        }
    }

    /// Append a notification with the default ttl.
    pub(crate) fn push(&mut self, message: impl Into<String>, severity: Severity) -> u64 {
        self.push_with_ttl(message, severity, DEFAULT_TTL, None)
    }

    pub(crate) fn push_with_ttl(
        &mut self,
        message: impl Into<String>,
        severity: Severity,
        ttl: Duration,
        action_hint: Option<&'static str>,
    ) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.items.push_back(Notification {
            id,
            message: message.into(),
            severity,
            created_at: Instant::now(),
            ttl,
            action_hint,
        });
        if self.items.len() == 1 {
            self.current_index = 0;
            self.last_rotate = Instant::now();
        }
        id
    }

    #[allow(dead_code)] // dismiss-by-id exposed for future user-driven dismissal keys
    pub(crate) fn dismiss(&mut self, id: u64) {
        if let Some(pos) = self.items.iter().position(|n| n.id == id) {
            self.items.remove(pos);
            if self.current_index >= self.items.len() {
                self.current_index = 0;
            }
        }
    }

    /// Drop expired items, advance rotation if it's time. Returns `true` if
    /// the visible item changed (caller should redraw). Idempotent and cheap
    /// — safe to call on every animation tick.
    pub(crate) fn tick(&mut self) -> bool {
        let mut changed = false;
        let pre = self.current_id();
        self.items.retain(|n| !n.expired());
        if self.current_index >= self.items.len() {
            self.current_index = 0;
        }
        if self.items.len() > 1 && self.last_rotate.elapsed() >= self.rotate_every {
            self.current_index = (self.current_index + 1) % self.items.len();
            self.last_rotate = Instant::now();
        }
        if self.current_id() != pre {
            changed = true;
        }
        changed
    }

    pub(crate) fn current(&self) -> Option<&Notification> {
        self.items.get(self.current_index)
    }

    fn current_id(&self) -> Option<u64> {
        self.current().map(|n| n.id)
    }

    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Whether the layout should reserve one row for the pane.
    pub(crate) fn height(&self) -> u16 {
        if self.is_empty() { 0 } else { 1 }
    }

    #[cfg(test)]
    pub(crate) fn force_rotate_now(&mut self) {
        self.last_rotate = Instant::now().checked_sub(self.rotate_every).unwrap();
    }

    #[cfg(test)]
    #[allow(dead_code)] // reserved for tests that exercise specific rotation periods
    pub(crate) fn override_rotate_every(&mut self, every: Duration) {
        self.rotate_every = every;
    }
}

#[allow(dead_code)]
const _: Color = QUIET;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_queue_has_zero_height_and_no_current() {
        let q = NotificationQueue::new();
        assert_eq!(q.height(), 0);
        assert!(q.current().is_none());
        assert!(q.is_empty());
    }

    #[test]
    fn push_makes_pane_one_row_tall() {
        let mut q = NotificationQueue::new();
        q.push("hello", Severity::Info);
        assert_eq!(q.height(), 1);
        assert_eq!(q.current().unwrap().message, "hello");
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn rotation_cycles_through_items() {
        let mut q = NotificationQueue::new();
        q.push("a", Severity::Info);
        q.push("b", Severity::Success);
        q.push("c", Severity::Warn);
        assert_eq!(q.current().unwrap().message, "a");
        q.force_rotate_now();
        let changed = q.tick();
        assert!(changed);
        assert_eq!(q.current().unwrap().message, "b");
        q.force_rotate_now();
        q.tick();
        assert_eq!(q.current().unwrap().message, "c");
        q.force_rotate_now();
        q.tick();
        assert_eq!(q.current().unwrap().message, "a");
    }

    #[test]
    fn expired_items_get_pruned_on_tick() {
        let mut q = NotificationQueue::new();
        q.push_with_ttl("ephemeral", Severity::Info, Duration::from_millis(1), None);
        q.push("permanent", Severity::Info);
        std::thread::sleep(Duration::from_millis(5));
        q.tick();
        assert_eq!(q.len(), 1);
        assert_eq!(q.current().unwrap().message, "permanent");
    }

    #[test]
    fn dismiss_by_id_removes_item() {
        let mut q = NotificationQueue::new();
        let a = q.push("a", Severity::Info);
        let b = q.push("b", Severity::Info);
        q.dismiss(a);
        assert_eq!(q.len(), 1);
        assert_eq!(q.current().unwrap().id, b);
    }

    #[test]
    fn severity_color_matches_palette() {
        assert_eq!(Severity::Info.color(), AMBER);
        assert_eq!(Severity::Success.color(), SUCCESS_GREEN);
        assert_eq!(Severity::Warn.color(), GOLD);
        assert_eq!(Severity::Error.color(), ERROR_RED);
    }
}
