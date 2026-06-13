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
use unicode_width::UnicodeWidthStr;

use crate::render::button::{ButtonState, button_spans};
use crate::render::palette;

/// Width (in cells) of the trailing delete affordance painted on each open
/// item row and registered as the `QueueDelete` hit zone. Shared between the
/// painter ([`render_lines`]) and the hit-target registrar in `lib.rs` so the
/// glyph and the click rect can never drift apart.
pub(crate) const DELETE_AFFORDANCE_WIDTH: u16 = 3;

/// The glyph painted in the trailing [`DELETE_AFFORDANCE_WIDTH`] cells of each
/// item row. Exactly three cells wide so it fills the registered delete zone.
const DELETE_GLYPH: &str = "[x]";

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
///
/// `conditions` is a per-item Conditional-Queue-Items marker (§12.3.5), one entry
/// per queue slot in queue order: each is the item's [`QueueCondition`] (defaulting
/// to `Always`, painted as aligning blanks) tinted by its evaluation against the
/// latest `outcome` so a skip-bound / blocked row reads at a glance. A shorter /
/// absent slice paints every row unconditional. Like the group marker it is an
/// inline prefix, so it never changes the row count the height calc relies on.
///
/// `content_width` is the render area width: when `Some`, each item row is padded
/// out and a `[x]` delete glyph is painted right-aligned in its trailing
/// [`DELETE_AFFORDANCE_WIDTH`] cells, landing at exactly the column the `QueueDelete`
/// hit rect occupies (`content_width - DELETE_AFFORDANCE_WIDTH`) so the painted
/// affordance and the click zone coincide. The preview body is truncated to fit
/// left of the glyph so it never overwrites it. `None` (tests that only inspect
/// the prefix markers) paints rows without the trailing glyph.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_lines(
    state: &PromptQueueState,
    queue: &VecDeque<String>,
    tagged: Option<&[bool]>,
    groups: Option<&[Option<&crate::queue_groups::QueueGroup>]>,
    conditions: Option<&[crate::queue_conditions::QueueCondition]>,
    outcome: Option<crate::queue_conditions::TurnOutcome>,
    content_width: Option<u16>,
) -> Vec<Line<'static>> {
    let group_active = tagged.is_some_and(|t| t.iter().any(|&b| b));
    let any_group = groups.is_some_and(|g| g.iter().any(|m| m.is_some()));
    let any_condition = conditions.is_some_and(|c| c.iter().any(|cond| !cond.is_always()));
    // `g form group` (not the noun `g group`) so the create semantics read like
    // its siblings `z fold` / `p pause` / `G dissolve` — the key *forms* a group
    // from the tagged rows.
    let hint: String = if queue.is_empty() {
        // No rows to select / tag / reorder yet: explain how the queue fills
        // instead of teaching keys that act on a list that does not exist.
        "  queue fills as you Enter prompts while a turn runs · Esc close".to_string()
    } else if group_active {
        // Multi-select and conditions are orthogonal — a queue can have tagged
        // rows *and* gated rows at once — so keep the `v cond` pointer here when
        // any row carries a condition rather than hiding the correctness-relevant
        // feature behind the transient one.
        let cond_note = if any_condition { " · v cond" } else { "" };
        format!(
            "  Space tag · g form group · Del delete group · Shift+↑↓ move group · m merge · c clear{cond_note}"
        )
    } else if any_group {
        "  ↑↓ select · g form group · z fold · p pause · G dissolve · v cond · r run next · Del remove · Esc".to_string()
    } else if any_condition {
        "  ↑↓ select · v condition · g form group · Shift+↑↓ reorder · Enter/e edit · r run next · Del remove · Esc".to_string()
    } else {
        "  ↑↓ select · Space tag · g form group · v cond · Shift+↑↓ reorder · Enter/e edit · r run next · Del · Esc".to_string()
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
        let condition = conditions
            .and_then(|c| c.get(index))
            .copied()
            .unwrap_or_default();
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
        // The header leads with the member count (`×N` ahead of the label) so the
        // most useful fact survives truncation; only the group's first member shows
        // it, later members fold to a quiet continuation rule so the header never
        // repeats down the stack.
        let collapsed_g = group.filter(|g| g.collapsed);
        let is_first_collapsed_member = collapsed_g.is_some()
            && groups
                .and_then(|g| index.checked_sub(1).and_then(|p| g.get(p)))
                .copied()
                .flatten()
                .map(|prev| prev.id)
                != collapsed_g.map(|g| g.id);
        let (body, body_style) = match collapsed_g {
            Some(g) if is_first_collapsed_member => (
                format!("{:>2}. ⊟ ×{} {}", index + 1, g.members.len(), g.name),
                style,
            ),
            Some(_) => (
                format!("{:>2}.  ┊", index + 1),
                Style::default().fg(crate::render::theme::quiet()),
            ),
            None => (format!("{:>2}. {}", index + 1, preview(item)), style),
        };
        let mut spans = vec![
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
            crate::queue_conditions::condition_marker_span(condition, outcome),
            Span::raw(" "),
        ];
        // Reserve the trailing cells for a visible `[x]` delete affordance that
        // lines up with the registered `QueueDelete` hit rect. Truncate the body
        // so it never overwrites the glyph, pad the gap so the glyph lands flush
        // right at `content_width - DELETE_AFFORDANCE_WIDTH`.
        if let Some(width) = content_width.filter(|w| *w > DELETE_AFFORDANCE_WIDTH) {
            let prefix_w: usize = spans
                .iter()
                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            let body_budget = (width as usize)
                .saturating_sub(DELETE_AFFORDANCE_WIDTH as usize)
                .saturating_sub(prefix_w);
            let body = truncate_to_width(&body, body_budget);
            let body_w = UnicodeWidthStr::width(body.as_str());
            let pad = body_budget.saturating_sub(body_w);
            spans.push(Span::styled(body, body_style));
            if pad > 0 {
                spans.push(Span::raw(" ".repeat(pad)));
            }
            spans.push(Span::styled(
                DELETE_GLYPH,
                Style::default().fg(crate::render::theme::quiet()),
            ));
        } else {
            spans.push(Span::styled(body, body_style));
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// Truncate `text` so its display width is at most `budget` cells, appending a
/// `…` (1 cell) when it had to cut so the user sees the row continues. Width-aware
/// (a CJK preview never overruns the delete glyph) and never splits a codepoint.
fn truncate_to_width(text: &str, budget: usize) -> String {
    if UnicodeWidthStr::width(text) <= budget {
        return text.to_string();
    }
    if budget == 0 {
        return String::new();
    }
    // Leave one cell for the ellipsis.
    let target = budget.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let w = UnicodeWidthStr::width(ch.to_string().as_str());
        if used + w > target {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
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
    // The hint no longer varies with the turn state (it stays stable across turn
    // boundaries), but the caller still threads this in; kept for the signature.
    _turn_running: bool,
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
    // Keep the keybinding hint stable across turn boundaries: the strip is meant
    // to read as settled, so it no longer grows an `Esc cancels current` clause
    // when a turn starts (Esc already carries its own affordance elsewhere).
    let hint = if overlay_open {
        "Ctrl+X Q to close"
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
