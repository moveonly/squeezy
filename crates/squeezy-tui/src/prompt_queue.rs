//! Cursor-style prompt queue.
//!
//! While a turn is running, Enter (and paste) push the composer text
//! onto `TuiApp::prompt_queue` instead of being rejected. Each time a
//! turn finishes, the next queued prompt drains and runs automatically.
//!
//! This module is the *pure-state* surface used by the reorder overlay:
//! it owns nothing but a selection cursor. The list itself lives on
//! `TuiApp::prompt_queue` so renderers and the drain path share one
//! source of truth.

use std::collections::VecDeque;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::render::button::{ButtonState, button_spans};
use crate::render::palette;

#[derive(Debug, Clone, Default)]
pub(crate) struct PromptQueueState {
    pub(crate) selected: usize,
}

/// Outcome of a key press while the overlay is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueueDispatch {
    /// Selection / order changed; keep the overlay open.
    Handled,
    /// The user asked to close the overlay (Esc or Enter).
    Close,
    /// Key was not consumed.
    Ignored,
}

impl PromptQueueState {
    pub(crate) fn new() -> Self {
        Self { selected: 0 }
    }

    /// Apply a key event against the overlay state and the live queue.
    pub(crate) fn dispatch(
        &mut self,
        queue: &mut VecDeque<String>,
        key: KeyEvent,
    ) -> QueueDispatch {
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Esc | KeyCode::Enter => QueueDispatch::Close,
            KeyCode::Up if shift => self.swap_up(queue),
            KeyCode::Down if shift => self.swap_down(queue),
            KeyCode::Up => self.move_up(queue),
            KeyCode::Down => self.move_down(queue),
            KeyCode::Delete | KeyCode::Backspace => self.delete(queue),
            _ => QueueDispatch::Ignored,
        }
    }

    fn clamp(&mut self, queue: &VecDeque<String>) {
        if queue.is_empty() {
            self.selected = 0;
        } else if self.selected >= queue.len() {
            self.selected = queue.len() - 1;
        }
    }

    fn move_up(&mut self, queue: &VecDeque<String>) -> QueueDispatch {
        if queue.is_empty() {
            return QueueDispatch::Handled;
        }
        if self.selected > 0 {
            self.selected -= 1;
        }
        QueueDispatch::Handled
    }

    fn move_down(&mut self, queue: &VecDeque<String>) -> QueueDispatch {
        if queue.is_empty() {
            return QueueDispatch::Handled;
        }
        if self.selected + 1 < queue.len() {
            self.selected += 1;
        }
        QueueDispatch::Handled
    }

    fn swap_up(&mut self, queue: &mut VecDeque<String>) -> QueueDispatch {
        if self.selected > 0 && self.selected < queue.len() {
            queue.swap(self.selected, self.selected - 1);
            self.selected -= 1;
        }
        QueueDispatch::Handled
    }

    fn swap_down(&mut self, queue: &mut VecDeque<String>) -> QueueDispatch {
        if self.selected + 1 < queue.len() {
            queue.swap(self.selected, self.selected + 1);
            self.selected += 1;
        }
        QueueDispatch::Handled
    }

    fn delete(&mut self, queue: &mut VecDeque<String>) -> QueueDispatch {
        if self.selected < queue.len() {
            queue.remove(self.selected);
            self.clamp(queue);
        }
        QueueDispatch::Handled
    }
}

/// Number of overlay item rows shown at once. The single source of truth for
/// the visible window: both `render_lines` (painting) and the lib.rs hit-target
/// registration derive their slice from `visible_window`, so the painted rows
/// and the registered click rects can never drift apart.
pub(crate) const WINDOW: usize = 5;

/// The `(start, count)` slice of queued items the overlay shows, centred on the
/// focus cursor. Shared by `render_lines` and `register_queue_item_targets` so
/// the hit rects line up with the painted rows one-for-one.
pub(crate) fn visible_window(selected: usize, total: usize) -> (usize, usize) {
    if total == 0 {
        return (0, 0);
    }
    let count = WINDOW.min(total);
    let half = WINDOW / 2;
    let start = selected
        .saturating_sub(half)
        .min(total.saturating_sub(count));
    (start, count)
}

/// One-line preview of a queued prompt for the overlay / indicator.
fn preview(text: &str) -> String {
    let first = text.lines().next().unwrap_or("").trim();
    const LIMIT: usize = 80;
    if first.chars().nth(LIMIT).is_none() {
        first.to_string()
    } else {
        let mut end = LIMIT;
        while !first.is_char_boundary(end) {
            end -= 1;
        }
        let mut out = String::with_capacity(end + '…'.len_utf8());
        out.push_str(&first[..end]);
        out.push('…');
        out
    }
}

/// Render lines for the reorder overlay. Mirrors `SelectOverlay::render`
/// but reads the live queue from `TuiApp::prompt_queue`.
///
/// `tagged` is a per-item multi-select flag (§11G.7), one bool per queue slot
/// in queue order; `None` (or a shorter slice) renders the base overlay with no
/// group markers. A tagged row gets a `[x]` checkbox in the accent colour; the
/// header hint switches to the multi-select cheatsheet while a group is active.
///
/// `groups` is a per-item Queue-Group marker (§12.3.4), one entry per queue slot
/// in queue order: `Some(group)` for a grouped prompt (painted with a `[G]`/`[P]`
/// tag reflecting running/paused state) or `None` for a loose prompt (painted as
/// aligning blanks). A shorter / absent slice paints every row loose. Markers are
/// inline prefixes, so they never change the row count the height calc relies on.
pub(crate) fn render_lines(
    state: &PromptQueueState,
    queue: &VecDeque<String>,
    tagged: Option<&[bool]>,
    groups: Option<&[Option<&crate::queue_groups::QueueGroup>]>,
) -> Vec<Line<'static>> {
    let group_active = tagged.is_some_and(|t| t.iter().any(|&b| b));
    let any_group = groups.is_some_and(|g| g.iter().any(|m| m.is_some()));
    let hint = if group_active {
        "  Space tag · g group · Del delete group · Shift+↑↓ move group · m merge · c clear"
    } else if any_group {
        "  ↑↓ select · g group · z fold · p pause · G dissolve · r run next · Del remove · Esc close"
    } else {
        "  ↑↓ select · Space tag · g group · Shift+↑↓ reorder · Enter/e edit · r run next · Del remove · Esc close"
    };
    let header = Line::from(vec![
        Span::styled(
            "Queued prompts",
            Style::default()
                .fg(crate::render::theme::secondary())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(hint, Style::default().fg(crate::render::theme::quiet())),
    ]);
    let mut lines = vec![header];
    if queue.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (queue is empty)",
            Style::default().fg(crate::render::theme::quiet()),
        )));
        return lines;
    }
    let total = queue.len();
    let (start, count) = visible_window(state.selected, total);
    for (rel, item) in queue.iter().skip(start).take(count).enumerate() {
        let index = start + rel;
        let is_selected = index == state.selected;
        let is_tagged = tagged.and_then(|t| t.get(index)).copied().unwrap_or(false);
        let group = groups.and_then(|g| g.get(index)).copied().flatten();
        let marker = if is_selected { "› " } else { "  " };
        let style = if is_selected {
            Style::default()
                .fg(crate::render::theme::secondary())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette::muted_fg())
        };
        // A collapsed group's member rows stay in place (so the windowing and the
        // hit-target rects never drift) but read as folded — the preview is
        // replaced by the group name on the first member and dimmed elsewhere.
        let body = if let Some(g) = group.filter(|g| g.collapsed) {
            format!("{:>2}. ⊟ {} ({})", index + 1, g.name, g.members.len())
        } else {
            format!("{:>2}. {}", index + 1, preview(item))
        };
        lines.push(Line::from(vec![
            Span::styled(
                marker,
                Style::default().fg(if is_selected {
                    crate::render::theme::secondary()
                } else {
                    crate::render::theme::quiet()
                }),
            ),
            crate::prompt_queue_multiselect::marker_span(is_tagged),
            crate::queue_groups::group_marker_span(group),
            Span::raw(" "),
            Span::styled(body, style),
        ]));
    }
    lines
}

#[cfg(test)]
#[path = "prompt_queue_tests.rs"]
mod tests;

/// One-line clickable strip shown above the composer whenever the queue
/// is non-empty. The leading `>` / `v` glyph is rendered in a strong
/// dark blue so the row reads as a disclosure button — `>` when the
/// reorder overlay is closed (click to expand), `v` when it's open
/// (click to collapse).
///
/// `groups` is the Queue-Groups (§12.3.4) one-line summary
/// ([`crate::queue_groups::groups_summary`]) — e.g. `Group 1 (2, paused)` — or
/// `None`/empty when no groups exist. When present it is appended in the warn
/// colour so the strip "shows next/paused/blocked groups and item counts".
pub(crate) fn indicator_line(
    queue: &VecDeque<String>,
    turn_running: bool,
    overlay_open: bool,
    groups: Option<&str>,
) -> Option<Line<'static>> {
    if queue.is_empty() {
        return None;
    }
    let n = queue.len();
    let state = if overlay_open {
        ButtonState::Expanded
    } else {
        ButtonState::Collapsed
    };
    let hint = if overlay_open {
        "Ctrl+X Q to close"
    } else if turn_running {
        "Ctrl+X Q to reorder · Esc cancels current (queue keeps draining)"
    } else {
        "Ctrl+X Q to reorder"
    };
    let mut spans = button_spans(&format!("queued: {n}"), state);
    // Surface the group summary (counts + paused/collapsed flags) right after the
    // count so a held-back batch is visible without opening the overlay.
    if let Some(summary) = groups.filter(|s| !s.is_empty()) {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("· {summary}"),
            Style::default().fg(crate::render::theme::warn()),
        ));
    }
    spans.push(Span::raw("  "));
    spans.push(Span::styled(
        hint,
        Style::default().fg(crate::render::theme::quiet()),
    ));
    Some(Line::from(spans))
}
