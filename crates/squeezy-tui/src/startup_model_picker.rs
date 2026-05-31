//! Startup model/provider picker.
//!
//! This is the first-run setup sibling of the resume picker. It deliberately
//! uses ratatui instead of stdio prompts so interactive startup stays inside
//! one visual surface.

use std::{io, path::Path};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
};
use squeezy_core::ReasoningEffort;

use crate::render::theme;

const REASONING_EFFORTS: [ReasoningEffort; 4] = [
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupModelPickerProvider {
    pub label: String,
    pub models: Vec<StartupModelPickerModel>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupModelPickerModel {
    pub label: String,
    pub reasoning_effort: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupModelPickerSelection {
    pub provider_index: usize,
    pub model_index: usize,
    pub reasoning_effort: Option<ReasoningEffort>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerStep {
    Provider,
    Model,
    Reasoning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StartupModelPickerState {
    choices: Vec<StartupModelPickerProvider>,
    step: PickerStep,
    provider_cursor: usize,
    model_cursor: usize,
    effort_cursor: usize,
}

impl StartupModelPickerState {
    fn new(choices: Vec<StartupModelPickerProvider>) -> Self {
        Self {
            choices,
            step: PickerStep::Provider,
            provider_cursor: 0,
            model_cursor: 0,
            effort_cursor: 1,
        }
    }

    fn provider(&self) -> Option<&StartupModelPickerProvider> {
        self.choices.get(self.provider_cursor)
    }

    fn selected_model(&self) -> Option<&StartupModelPickerModel> {
        self.provider()
            .and_then(|provider| provider.models.get(self.model_cursor))
    }

    fn selected_model_requires_reasoning(&self) -> bool {
        self.selected_model()
            .is_some_and(|model| model.reasoning_effort)
    }

    fn move_cursor(&mut self, delta: isize) {
        let len = match self.step {
            PickerStep::Provider => self.choices.len(),
            PickerStep::Model => self
                .provider()
                .map(|provider| provider.models.len())
                .unwrap_or(0),
            PickerStep::Reasoning => REASONING_EFFORTS.len(),
        };
        if len == 0 {
            return;
        }
        let cursor = match self.step {
            PickerStep::Provider => &mut self.provider_cursor,
            PickerStep::Model => &mut self.model_cursor,
            PickerStep::Reasoning => &mut self.effort_cursor,
        };
        let len = len as isize;
        *cursor = ((*cursor as isize + delta).rem_euclid(len)) as usize;
        if self.step == PickerStep::Provider {
            self.model_cursor = 0;
        }
    }

    fn go_left(&mut self) {
        self.step = match self.step {
            PickerStep::Provider => PickerStep::Provider,
            PickerStep::Model => PickerStep::Provider,
            PickerStep::Reasoning => PickerStep::Model,
        };
    }

    fn go_right_or_finish(&mut self) -> Option<StartupModelPickerSelection> {
        match self.step {
            PickerStep::Provider => {
                self.step = PickerStep::Model;
                None
            }
            PickerStep::Model if self.selected_model_requires_reasoning() => {
                self.step = PickerStep::Reasoning;
                None
            }
            PickerStep::Model | PickerStep::Reasoning => Some(self.selection()),
        }
    }

    fn selection(&self) -> StartupModelPickerSelection {
        StartupModelPickerSelection {
            provider_index: self.provider_cursor,
            model_index: self.model_cursor,
            reasoning_effort: self
                .selected_model_requires_reasoning()
                .then(|| REASONING_EFFORTS[self.effort_cursor]),
        }
    }

    fn dispatch(&mut self, key: KeyEvent) -> Option<PickerOutcome> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Up, _) => {
                self.move_cursor(-1);
                None
            }
            (KeyCode::Down, _) => {
                self.move_cursor(1);
                None
            }
            (KeyCode::Left, _) => {
                self.go_left();
                None
            }
            (KeyCode::Right, _) | (KeyCode::Enter, _) => {
                self.go_right_or_finish().map(PickerOutcome::Selected)
            }
            (KeyCode::Esc, _) | (KeyCode::Char('q'), _) | (KeyCode::Char('Q'), _) => {
                Some(PickerOutcome::Quit)
            }
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => Some(PickerOutcome::Quit),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PickerOutcome {
    Selected(StartupModelPickerSelection),
    Quit,
}

pub(crate) fn run_picker<W: io::Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    settings_path: &Path,
    choices: Vec<StartupModelPickerProvider>,
) -> io::Result<Option<StartupModelPickerSelection>> {
    let mut state = StartupModelPickerState::new(choices);
    if state.choices.is_empty() {
        return Ok(None);
    }
    loop {
        terminal.draw(|frame| render_picker(frame, &state, settings_path))?;
        match event::read()? {
            Event::Key(key) => match state.dispatch(key) {
                Some(PickerOutcome::Selected(selection)) => return Ok(Some(selection)),
                Some(PickerOutcome::Quit) => return Ok(None),
                None => {}
            },
            Event::Resize(_, _) => continue,
            _ => continue,
        }
    }
}

fn render_picker(frame: &mut ratatui::Frame<'_>, state: &StartupModelPickerState, path: &Path) {
    let full = frame.area();
    let area = centered_area(full);
    frame.render_widget(Clear, full);

    let title = Line::from(vec![
        Span::styled(" ◆ ", Style::default().fg(theme::accent())),
        Span::styled(
            "squeezy",
            Style::default()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" · ", Style::default().fg(theme::quiet())),
        Span::styled("setup defaults", Style::default().fg(Color::White)),
        Span::raw(" "),
    ]);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::accent()))
        .title(title)
        .title_alignment(Alignment::Left);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .split(inner);

    frame.render_widget(Paragraph::new(render_step_line(state)), layout[0]);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("save ", Style::default().fg(theme::quiet())),
            Span::styled(
                path.display().to_string(),
                Style::default().fg(theme::path_hint()),
            ),
            Span::styled(" · env var names only", Style::default().fg(theme::quiet())),
        ])),
        layout[1],
    );
    frame.render_widget(Paragraph::new(render_selection_summary(state)), layout[2]);
    frame.render_widget(
        Paragraph::new(render_choice_rows(state, usize::from(layout[3].height))),
        layout[3],
    );
    frame.render_widget(Paragraph::new(render_footer(state)), layout[4]);
}

fn render_step_line(state: &StartupModelPickerState) -> Line<'static> {
    let provider_active = state.step == PickerStep::Provider;
    let model_active = state.step == PickerStep::Model;
    let reasoning_active = state.step == PickerStep::Reasoning;
    Line::from(vec![
        step_span("1 provider", provider_active),
        Span::styled("  ", Style::default()),
        step_span("2 model", model_active),
        Span::styled("  ", Style::default()),
        step_span("3 reasoning", reasoning_active),
        Span::styled("  ", Style::default()),
        Span::styled("then resume", Style::default().fg(theme::quiet())),
    ])
}

fn step_span(label: &'static str, active: bool) -> Span<'static> {
    if active {
        Span::styled(
            format!("▸ {label}"),
            Style::default()
                .fg(theme::secondary())
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(format!("  {label}"), Style::default().fg(theme::quiet()))
    }
}

fn render_selection_summary(state: &StartupModelPickerState) -> Line<'static> {
    let provider = state
        .provider()
        .map(|provider| provider.label.as_str())
        .unwrap_or("not selected");
    let model = state
        .selected_model()
        .map(|model| model.label.as_str())
        .unwrap_or("not selected");
    Line::from(vec![
        Span::styled("provider ", Style::default().fg(theme::quiet())),
        Span::styled(truncate(provider, 34), Style::default().fg(Color::White)),
        Span::styled("  model ", Style::default().fg(theme::quiet())),
        Span::styled(truncate(model, 44), Style::default().fg(Color::White)),
    ])
}

fn render_choice_rows(state: &StartupModelPickerState, height: usize) -> Vec<Line<'static>> {
    let labels = match state.step {
        PickerStep::Provider => state
            .choices
            .iter()
            .map(|choice| choice.label.clone())
            .collect::<Vec<_>>(),
        PickerStep::Model => state
            .provider()
            .map(|provider| {
                provider
                    .models
                    .iter()
                    .map(|model| model.label.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
        PickerStep::Reasoning => REASONING_EFFORTS
            .iter()
            .map(|effort| effort.as_str().to_string())
            .collect::<Vec<_>>(),
    };
    let cursor = match state.step {
        PickerStep::Provider => state.provider_cursor,
        PickerStep::Model => state.model_cursor,
        PickerStep::Reasoning => state.effort_cursor,
    };
    let (start, end) = visible_window(labels.len(), cursor, height.max(1));
    let mut rows = Vec::with_capacity(height);
    if start > 0 {
        rows.push(Line::from(Span::styled(
            "  ...",
            Style::default().fg(theme::quiet()),
        )));
    }
    for (index, label) in labels.iter().enumerate().take(end).skip(start) {
        rows.push(render_choice_row(label, index == cursor));
    }
    if end < labels.len() {
        rows.push(Line::from(Span::styled(
            "  ...",
            Style::default().fg(theme::quiet()),
        )));
    }
    rows
}

fn render_choice_row(label: &str, active: bool) -> Line<'static> {
    let (prefix_color, style) = if active {
        (
            theme::accent(),
            Style::default()
                .fg(theme::secondary())
                .add_modifier(Modifier::BOLD),
        )
    } else {
        (theme::quiet(), Style::default().fg(Color::White))
    };
    let prefix = if active { "▸ " } else { "  " };
    Line::from(vec![
        Span::styled(prefix, Style::default().fg(prefix_color)),
        Span::styled(truncate(label, 96), style),
    ])
}

fn render_footer(state: &StartupModelPickerState) -> Line<'static> {
    let enter_label = match state.step {
        PickerStep::Provider => "model",
        PickerStep::Model if state.selected_model_requires_reasoning() => "reasoning",
        PickerStep::Model | PickerStep::Reasoning => "confirm",
    };
    Line::from(vec![
        Span::styled("↑/↓ ", Style::default().fg(theme::secondary())),
        Span::styled("move  ", Style::default().fg(theme::quiet())),
        Span::styled("←/→ ", Style::default().fg(theme::secondary())),
        Span::styled("question  ", Style::default().fg(theme::quiet())),
        Span::styled("Enter ", Style::default().fg(theme::secondary())),
        Span::styled(
            format!("{enter_label}  "),
            Style::default().fg(theme::quiet()),
        ),
        Span::styled("Esc/Q ", Style::default().fg(theme::secondary())),
        Span::styled("quit", Style::default().fg(theme::quiet())),
    ])
}

fn visible_window(total: usize, cursor: usize, height: usize) -> (usize, usize) {
    if total <= height {
        return (0, total);
    }
    let half = height / 2;
    let start = cursor.saturating_sub(half).min(total - height);
    (start, start + height)
}

fn centered_area(full: Rect) -> Rect {
    let max_width = 96u16;
    let max_height = 20u16;
    let width = full.width.min(max_width);
    let height = full.height.min(max_height);
    let x = full.x + full.width.saturating_sub(width) / 2;
    let y = full.y + full.height.saturating_sub(height) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
}

fn truncate(input: &str, limit: usize) -> String {
    if input.chars().count() <= limit {
        return input.to_string();
    }
    let mut out: String = input.chars().take(limit.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
#[path = "startup_model_picker_tests.rs"]
mod tests;
