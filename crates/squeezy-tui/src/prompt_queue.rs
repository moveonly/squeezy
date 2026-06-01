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
pub(crate) fn render_lines(
    state: &PromptQueueState,
    queue: &VecDeque<String>,
) -> Vec<Line<'static>> {
    let header = Line::from(vec![
        Span::styled(
            "Queued prompts",
            Style::default()
                .fg(crate::render::theme::secondary())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "  ↑↓ select · Shift+↑↓ reorder · Del remove · Enter close",
            Style::default().fg(crate::render::theme::quiet()),
        ),
    ]);
    let mut lines = vec![header];
    if queue.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (queue is empty)",
            Style::default().fg(crate::render::theme::quiet()),
        )));
        return lines;
    }
    const WINDOW: usize = 5;
    let total = queue.len();
    let half = WINDOW / 2;
    let start = state
        .selected
        .saturating_sub(half)
        .min(total.saturating_sub(WINDOW.min(total)));
    let end = (start + WINDOW).min(total);
    for (rel, item) in queue.iter().skip(start).take(end - start).enumerate() {
        let index = start + rel;
        let is_selected = index == state.selected;
        let marker = if is_selected { "› " } else { "  " };
        let style = if is_selected {
            Style::default()
                .fg(crate::render::theme::secondary())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(palette::muted_fg())
        };
        let body = format!("{:>2}. {}", index + 1, preview(item));
        lines.push(Line::from(vec![
            Span::styled(
                marker,
                Style::default().fg(if is_selected {
                    crate::render::theme::secondary()
                } else {
                    crate::render::theme::quiet()
                }),
            ),
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
pub(crate) fn indicator_line(
    queue: &VecDeque<String>,
    turn_running: bool,
    overlay_open: bool,
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
    spans.push(Span::raw("  "));
    spans.push(Span::styled(
        hint,
        Style::default().fg(crate::render::theme::quiet()),
    ));
    Some(Line::from(spans))
}
