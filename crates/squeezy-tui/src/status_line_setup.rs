//! Interactive `/statusline` picker overlay.
//!
//! A checkbox list of every [`StatusLineItem`], a "Use theme colors"
//! toggle, an order-preserving reorder via Shift+↑/↓, a free-form
//! search filter, and a live preview built with the same renderer the
//! real status bar uses. Save persists the picker state to
//! `[tui].status_line` + `[tui].status_line_use_colors` and applies the
//! change in-memory immediately.
//!
//! The view owns no app state of its own beyond what's needed to draw and
//! handle keys; on save it asks the caller to update `TuiApp` so the next
//! frame already reflects the new layout.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
};

use crate::TuiApp;
use crate::status::{self, StatusLineAccent, StatusLineItem};

/// Result of a key event on the picker — propagated up so the caller can
/// close the overlay and (on Save) persist the configured values.
#[derive(Debug, Clone)]
pub(crate) enum KeyOutcome {
    /// Continue handling keys; redraw on the next frame.
    Continue,
    /// User pressed Esc; close without saving.
    Cancel,
    /// User pressed Enter; close after applying these values.
    Save {
        items: Vec<StatusLineItem>,
        use_colors: bool,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct StatusLineSetupState {
    /// Items in the order they will render in the status bar.
    items: Vec<(StatusLineItem, bool)>,
    /// Whether the accent palette is applied to enabled items.
    use_colors: bool,
    /// Cursor row in the *visible* (filter-matched) list. Row 0 is the
    /// "Use theme colors" toggle; rows 1.. are item rows after the
    /// separator. Stored as an index into [`Self::visible_indices`].
    cursor: usize,
    /// Free-form search filter typed by the user. Empty = show all.
    search: String,
}

impl StatusLineSetupState {
    pub(crate) fn new(configured: Option<&[StatusLineItem]>, use_colors: bool) -> Self {
        // Start from the user's configured order; append any items they
        // haven't enabled in the canonical "all available" order so
        // they remain reachable in the picker.
        let mut items: Vec<(StatusLineItem, bool)> = Vec::new();
        if let Some(configured) = configured {
            for item in configured {
                if !items.iter().any(|(existing, _)| existing == item) {
                    items.push((*item, true));
                }
            }
        } else {
            for item in status::DEFAULT_STATUS_LINE_ITEMS {
                items.push((*item, true));
            }
        }
        for item in StatusLineItem::ALL {
            if !items.iter().any(|(existing, _)| existing == item) {
                items.push((*item, false));
            }
        }
        Self {
            items,
            use_colors,
            cursor: 0,
            search: String::new(),
        }
    }

    fn visible_item_indices(&self) -> Vec<usize> {
        let needle = self.search.trim().to_ascii_lowercase();
        self.items
            .iter()
            .enumerate()
            .filter_map(|(i, (item, _))| {
                if needle.is_empty()
                    || item.slug().contains(&needle)
                    || item.description().to_ascii_lowercase().contains(&needle)
                {
                    Some(i)
                } else {
                    None
                }
            })
            .collect()
    }

    /// Total visible rows = "Use theme colors" + matching item rows.
    fn visible_row_count(&self) -> usize {
        1 + self.visible_item_indices().len()
    }

    fn clamp_cursor(&mut self) {
        let max = self.visible_row_count().saturating_sub(1);
        if self.cursor > max {
            self.cursor = max;
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        let count = self.visible_row_count();
        if count == 0 {
            return;
        }
        let next = (self.cursor as isize + delta).rem_euclid(count as isize);
        self.cursor = next as usize;
    }

    /// Move the item under the cursor up/down within `items`. No-op when
    /// the cursor is on the "Use theme colors" toggle row or the filter
    /// would make the move invisible.
    fn move_item(&mut self, delta: isize) {
        if self.cursor == 0 {
            return;
        }
        let visible = self.visible_item_indices();
        let pos_in_visible = self.cursor - 1;
        if pos_in_visible >= visible.len() {
            return;
        }
        let src = visible[pos_in_visible];
        let dst_pos_in_visible = pos_in_visible as isize + delta;
        if dst_pos_in_visible < 0 || dst_pos_in_visible as usize >= visible.len() {
            return;
        }
        let dst = visible[dst_pos_in_visible as usize];
        self.items.swap(src, dst);
        self.cursor = (dst_pos_in_visible as usize) + 1;
    }

    fn toggle_under_cursor(&mut self) {
        if self.cursor == 0 {
            self.use_colors = !self.use_colors;
            return;
        }
        let visible = self.visible_item_indices();
        let pos = self.cursor - 1;
        if let Some(&idx) = visible.get(pos) {
            self.items[idx].1 = !self.items[idx].1;
        }
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> KeyOutcome {
        match key.code {
            KeyCode::Esc => KeyOutcome::Cancel,
            KeyCode::Enter => {
                let items = self
                    .items
                    .iter()
                    .filter_map(|(item, on)| if *on { Some(*item) } else { None })
                    .collect();
                KeyOutcome::Save {
                    items,
                    use_colors: self.use_colors,
                }
            }
            KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.move_item(-1);
                KeyOutcome::Continue
            }
            KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.move_item(1);
                KeyOutcome::Continue
            }
            KeyCode::Up => {
                self.move_cursor(-1);
                KeyOutcome::Continue
            }
            KeyCode::Down => {
                self.move_cursor(1);
                KeyOutcome::Continue
            }
            KeyCode::Char(' ') => {
                self.toggle_under_cursor();
                KeyOutcome::Continue
            }
            KeyCode::Backspace => {
                self.search.pop();
                self.clamp_cursor();
                KeyOutcome::Continue
            }
            KeyCode::Char(c)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                self.search.push(c);
                self.clamp_cursor();
                KeyOutcome::Continue
            }
            _ => KeyOutcome::Continue,
        }
    }

    /// Build a preview line using the same renderer as the live status bar.
    fn preview_line(&self, app: &TuiApp) -> Line<'static> {
        let enabled: Vec<StatusLineItem> = self
            .items
            .iter()
            .filter_map(|(item, on)| if *on { Some(*item) } else { None })
            .collect();
        status::render_status_detail_line(app, &enabled, self.use_colors)
            .unwrap_or_else(|| Line::from(Span::styled("(nothing to preview)", dim())))
    }
}

fn dim() -> Style {
    Style::default()
        .add_modifier(Modifier::DIM)
        .fg(crate::render::theme::quiet())
}

pub(crate) fn render(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &StatusLineSetupState,
    app: &TuiApp,
) {
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(crate::render::theme::secondary()))
        .title(" Configure Status Line ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // intro + search
            Constraint::Min(3),    // list
            Constraint::Length(3), // preview
            Constraint::Length(1), // help
        ])
        .split(inner);

    render_header(frame, chunks[0], state);
    render_list(frame, chunks[1], state);
    render_preview(frame, chunks[2], state, app);
    render_help(frame, chunks[3]);
}

fn render_header(frame: &mut Frame<'_>, area: Rect, state: &StatusLineSetupState) {
    let lines = vec![
        Line::from(Span::styled(
            "Select which items to display in the status line.",
            dim(),
        )),
        Line::from(vec![
            Span::styled("Search: ", dim()),
            Span::raw(state.search.clone()),
            Span::styled("_", Style::default().add_modifier(Modifier::SLOW_BLINK)),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), area);
}

fn render_list(frame: &mut Frame<'_>, area: Rect, state: &StatusLineSetupState) {
    let visible = state.visible_item_indices();
    let mut lines: Vec<Line<'static>> = Vec::new();
    // Row 0: "Use theme colors" toggle.
    lines.push(row_line(
        state.cursor == 0,
        state.use_colors,
        "Use theme colors",
        "Color status items with their accent palette",
        Style::default().fg(crate::render::theme::quiet()),
    ));
    lines.push(Line::from(Span::styled(
        "─".repeat(area.width as usize),
        dim(),
    )));
    // The list pane height limits how many rows we can draw. Compute a
    // scroll offset so the cursor row stays visible.
    let visible_rows = area.height.saturating_sub(2) as usize;
    let cursor_in_items = state.cursor.saturating_sub(1);
    let offset = cursor_in_items.saturating_sub(visible_rows.saturating_sub(1));
    for (vi, &idx) in visible.iter().enumerate().skip(offset).take(visible_rows) {
        let (item, enabled) = state.items[idx];
        let accent = StatusLineAccent::for_item(item);
        let item_style = Style::default().fg(accent.fallback_color());
        lines.push(row_line(
            state.cursor == vi + 1,
            enabled,
            item.slug(),
            item.description(),
            item_style,
        ));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn row_line(
    cursor: bool,
    enabled: bool,
    label: &str,
    description: &str,
    label_style: Style,
) -> Line<'static> {
    let pointer = if cursor { "› " } else { "  " };
    let checkbox = if enabled { "[x] " } else { "[ ] " };
    let pad = 24usize.saturating_sub(label.chars().count() + 1);
    let padding = " ".repeat(pad);
    Line::from(vec![
        Span::styled(
            pointer.to_string(),
            Style::default().fg(if cursor {
                Color::Yellow
            } else {
                crate::render::theme::quiet()
            }),
        ),
        Span::styled(checkbox.to_string(), label_style),
        Span::styled(label.to_string(), label_style),
        Span::raw(padding),
        Span::styled(description.to_string(), dim()),
    ])
}

fn render_preview(frame: &mut Frame<'_>, area: Rect, state: &StatusLineSetupState, app: &TuiApp) {
    let lines = vec![
        Line::from(Span::styled("Preview:", dim())),
        state.preview_line(app),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn render_help(frame: &mut Frame<'_>, area: Rect) {
    let help = Line::from(Span::styled(
        "↑/↓ move · Space toggle · Shift+↑/↓ reorder · type to filter · Enter save · Esc cancel",
        dim(),
    ));
    frame.render_widget(Paragraph::new(help), area);
}
