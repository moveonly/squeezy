//! Startup setup picker.
//!
//! First-run setup deliberately stays inside the TUI surface: theme is
//! selected first and applied before provider/model pages render.

use std::{io, path::Path};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
};
use squeezy_core::{
    AppConfig, ReasoningEffort,
    settings_writer::{EditOp, SettingsEdit, SettingsScope, apply_edits},
};

use crate::render::theme;

const REASONING_EFFORTS: [ReasoningEffort; 4] = [
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupThemeChoice {
    pub name: String,
    pub label: String,
    pub action: StartupThemeAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupThemeAction {
    Select,
    ConfigureInConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupModelPickerProvider {
    pub label: String,
    pub credential: StartupProviderCredential,
    pub models: Vec<StartupModelPickerModel>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupProviderCredential {
    Configured,
    NeedsConfig { env_var: String },
    NotRequired,
}

impl StartupProviderCredential {
    fn needs_config(&self) -> bool {
        matches!(self, Self::NeedsConfig { .. })
    }

    fn env_var(&self) -> Option<&str> {
        match self {
            Self::NeedsConfig { env_var } => Some(env_var),
            Self::Configured | Self::NotRequired => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupModelPickerModel {
    pub label: String,
    pub reasoning_effort: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupModelPickerSelection {
    pub theme: String,
    pub provider_index: usize,
    pub model_index: usize,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub open_theme_config: bool,
    pub open_model_config: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupModelPickerResult {
    Selected(StartupModelPickerSelection),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerStep {
    Theme,
    Provider,
    Key,
    Model,
    Reasoning,
}

impl PickerStep {
    const fn prompt(self) -> &'static str {
        match self {
            Self::Theme => "Choose a theme",
            Self::Provider => "Choose a provider",
            Self::Key => "Add provider key",
            Self::Model => "Choose a model",
            Self::Reasoning => "Choose reasoning effort",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StartupModelPickerState {
    themes: Vec<StartupThemeChoice>,
    choices: Vec<StartupModelPickerProvider>,
    initial_theme: String,
    trailing_question_count: usize,
    step: PickerStep,
    theme_cursor: usize,
    provider_cursor: usize,
    key_cursor: usize,
    model_cursor: usize,
    effort_cursor: usize,
}

impl StartupModelPickerState {
    fn new(
        themes: Vec<StartupThemeChoice>,
        choices: Vec<StartupModelPickerProvider>,
        initial_theme: &str,
        trailing_question_count: usize,
    ) -> Self {
        let theme_cursor = themes
            .iter()
            .position(|choice| choice.name == initial_theme)
            .unwrap_or(0);
        Self {
            themes,
            choices,
            initial_theme: initial_theme.to_string(),
            trailing_question_count,
            step: PickerStep::Theme,
            theme_cursor,
            provider_cursor: 0,
            key_cursor: 0,
            model_cursor: 0,
            effort_cursor: 1,
        }
    }

    fn theme(&self) -> Option<&StartupThemeChoice> {
        self.themes.get(self.theme_cursor)
    }

    fn theme_action(&self) -> StartupThemeAction {
        self.theme()
            .map(|theme| theme.action)
            .unwrap_or(StartupThemeAction::Select)
    }

    fn selected_theme_name(&self) -> Option<&str> {
        self.theme().and_then(|theme| match theme.action {
            StartupThemeAction::Select => Some(theme.name.as_str()),
            StartupThemeAction::ConfigureInConfig => None,
        })
    }

    fn provider(&self) -> Option<&StartupModelPickerProvider> {
        self.choices.get(self.provider_cursor)
    }

    fn provider_needs_key_config(&self) -> bool {
        self.provider()
            .is_some_and(|provider| provider.credential.needs_config())
    }

    fn selected_model(&self) -> Option<&StartupModelPickerModel> {
        self.provider()
            .and_then(|provider| provider.models.get(self.model_cursor))
    }

    fn selected_model_requires_reasoning(&self) -> bool {
        self.selected_model()
            .is_some_and(|model| model.reasoning_effort)
    }

    fn visible_steps(&self) -> Vec<PickerStep> {
        let mut steps = vec![PickerStep::Theme, PickerStep::Provider];
        if self.provider_needs_key_config() {
            steps.push(PickerStep::Key);
        }
        steps.push(PickerStep::Model);
        if self.selected_model_requires_reasoning() {
            steps.push(PickerStep::Reasoning);
        }
        steps
    }

    fn progress(&self) -> (usize, usize) {
        let steps = self.visible_steps();
        let index = steps
            .iter()
            .position(|step| *step == self.step)
            .unwrap_or(0);
        (index + 1, steps.len() + self.trailing_question_count)
    }

    fn move_cursor(&mut self, delta: isize) {
        let len = match self.step {
            PickerStep::Theme => self.themes.len(),
            PickerStep::Provider => self.choices.len(),
            PickerStep::Key => key_options(self).len(),
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
            PickerStep::Theme => &mut self.theme_cursor,
            PickerStep::Provider => &mut self.provider_cursor,
            PickerStep::Key => &mut self.key_cursor,
            PickerStep::Model => &mut self.model_cursor,
            PickerStep::Reasoning => &mut self.effort_cursor,
        };
        let len = len as isize;
        *cursor = ((*cursor as isize + delta).rem_euclid(len)) as usize;
        if self.step == PickerStep::Provider {
            self.model_cursor = 0;
            self.key_cursor = 0;
        }
    }

    fn go_left(&mut self) {
        self.step = match self.step {
            PickerStep::Theme => PickerStep::Theme,
            PickerStep::Provider => PickerStep::Theme,
            PickerStep::Key => PickerStep::Provider,
            PickerStep::Model if self.provider_needs_key_config() => PickerStep::Key,
            PickerStep::Model => PickerStep::Provider,
            PickerStep::Reasoning => PickerStep::Model,
        };
    }

    fn go_right_or_finish(&mut self) -> Option<PickerOutcome> {
        match self.step {
            PickerStep::Theme => {
                if self.theme_action() == StartupThemeAction::ConfigureInConfig {
                    self.step = PickerStep::Provider;
                    return None;
                }
                let theme = self.selected_theme_name()?.to_string();
                self.step = PickerStep::Provider;
                Some(PickerOutcome::ApplyTheme(theme))
            }
            PickerStep::Provider => {
                if self.provider_needs_key_config() {
                    self.step = PickerStep::Key;
                    return None;
                }
                self.step = PickerStep::Model;
                None
            }
            PickerStep::Key => {
                self.step = PickerStep::Model;
                None
            }
            PickerStep::Model if self.selected_model_requires_reasoning() => {
                self.step = PickerStep::Reasoning;
                None
            }
            PickerStep::Model | PickerStep::Reasoning => {
                Some(PickerOutcome::Selected(self.selection()))
            }
        }
    }

    fn selection(&self) -> StartupModelPickerSelection {
        StartupModelPickerSelection {
            theme: self
                .selected_theme_name()
                .map(str::to_string)
                .unwrap_or_else(|| self.initial_theme.clone()),
            provider_index: self.provider_cursor,
            model_index: self.model_cursor,
            reasoning_effort: self
                .selected_model_requires_reasoning()
                .then(|| REASONING_EFFORTS[self.effort_cursor]),
            open_theme_config: self.theme_action() == StartupThemeAction::ConfigureInConfig,
            open_model_config: self.provider_needs_key_config(),
        }
    }

    fn dispatch(&mut self, key: KeyEvent) -> Option<PickerOutcome> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Up, _) => {
                let preview = self.step == PickerStep::Theme;
                self.move_cursor(-1);
                preview
                    .then(|| self.selected_theme_name().map(str::to_string))
                    .flatten()
                    .map(PickerOutcome::PreviewTheme)
            }
            (KeyCode::Down, _) => {
                let preview = self.step == PickerStep::Theme;
                self.move_cursor(1);
                preview
                    .then(|| self.selected_theme_name().map(str::to_string))
                    .flatten()
                    .map(PickerOutcome::PreviewTheme)
            }
            (KeyCode::Left, _) => {
                self.go_left();
                None
            }
            (KeyCode::Right, _) | (KeyCode::Enter, _) => self.go_right_or_finish(),
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
    PreviewTheme(String),
    ApplyTheme(String),
    Selected(StartupModelPickerSelection),
    Quit,
}

pub(crate) fn run_picker<W: io::Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    config: &AppConfig,
    settings_path: &Path,
    themes: Vec<StartupThemeChoice>,
    choices: Vec<StartupModelPickerProvider>,
    trailing_question_count: usize,
) -> io::Result<Option<StartupModelPickerResult>> {
    let mut state =
        StartupModelPickerState::new(themes, choices, &config.tui.theme, trailing_question_count);
    if state.themes.is_empty() || state.choices.is_empty() {
        return Ok(None);
    }
    loop {
        terminal.draw(|frame| render_picker(frame, &state, settings_path))?;
        match event::read()? {
            Event::Key(key) => match state.dispatch(key) {
                Some(PickerOutcome::PreviewTheme(theme)) => {
                    let mut next = config.clone();
                    next.tui.theme = theme;
                    crate::apply_theme_overrides(&next);
                }
                Some(PickerOutcome::ApplyTheme(theme)) => {
                    persist_theme(settings_path, &theme)?;
                    let mut next = config.clone();
                    next.tui.theme = theme;
                    crate::apply_theme_overrides(&next);
                }
                Some(PickerOutcome::Selected(selection)) => {
                    return Ok(Some(StartupModelPickerResult::Selected(selection)));
                }
                Some(PickerOutcome::Quit) => return Ok(None),
                None => {}
            },
            Event::Resize(_, _) => continue,
            _ => continue,
        }
    }
}

fn persist_theme(settings_path: &Path, theme: &str) -> io::Result<()> {
    apply_edits(
        &SettingsScope::user(settings_path),
        &[SettingsEdit {
            path: &["tui", "theme"],
            op: EditOp::SetString(theme.to_string()),
        }],
    )
    .map(|_| ())
    .map_err(|err| io::Error::other(err.to_string()))
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
        Span::styled("first run setup", Style::default().fg(theme::foreground())),
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
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Min(5),
            Constraint::Length(2),
        ])
        .split(inner);

    frame.render_widget(Paragraph::new(render_question_line(state)), layout[0]);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("save ", Style::default().fg(theme::quiet())),
            Span::styled(
                path.display().to_string(),
                Style::default().fg(theme::path_hint()),
            ),
        ]))
        .wrap(Wrap { trim: false }),
        layout[1],
    );
    frame.render_widget(
        Paragraph::new(render_selection_summary(state)).wrap(Wrap { trim: false }),
        layout[2],
    );
    frame.render_widget(
        Paragraph::new(render_choice_rows(state, usize::from(layout[3].height)))
            .wrap(Wrap { trim: false }),
        layout[3],
    );
    frame.render_widget(
        Paragraph::new(render_footer(state)).wrap(Wrap { trim: false }),
        layout[4],
    );
}

fn render_question_line(state: &StartupModelPickerState) -> Line<'static> {
    let (current, total) = state.progress();
    Line::from(vec![
        Span::styled(
            format!("Question {current}/{total} "),
            Style::default()
                .fg(theme::secondary())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            state.step.prompt(),
            Style::default().fg(theme::foreground()),
        ),
    ])
}

fn render_selection_summary(state: &StartupModelPickerState) -> Line<'static> {
    let theme_name = state
        .selected_theme_name()
        .or_else(|| state.theme().map(|theme| theme.label.as_str()))
        .unwrap_or("default");
    let provider = state
        .provider()
        .map(|provider| provider.label.as_str())
        .unwrap_or("not selected");
    let model = state
        .selected_model()
        .map(|model| model.label.as_str())
        .unwrap_or("not selected");
    let mut spans = vec![
        Span::styled("theme ", Style::default().fg(theme::quiet())),
        Span::styled(
            theme_name.to_string(),
            Style::default().fg(theme::foreground()),
        ),
    ];
    if !matches!(state.step, PickerStep::Theme) {
        spans.push(Span::styled(
            "  provider ",
            Style::default().fg(theme::quiet()),
        ));
        spans.push(Span::styled(
            provider.to_string(),
            Style::default().fg(theme::foreground()),
        ));
    }
    if matches!(state.step, PickerStep::Model | PickerStep::Reasoning) {
        spans.push(Span::styled(
            "  model ",
            Style::default().fg(theme::quiet()),
        ));
        spans.push(Span::styled(
            model.to_string(),
            Style::default().fg(theme::foreground()),
        ));
    }
    Line::from(spans)
}

fn render_choice_rows(state: &StartupModelPickerState, height: usize) -> Vec<Line<'static>> {
    let labels = match state.step {
        PickerStep::Theme => state
            .themes
            .iter()
            .map(|choice| choice.label.clone())
            .collect::<Vec<_>>(),
        PickerStep::Provider => state
            .choices
            .iter()
            .map(|choice| choice.label.clone())
            .collect::<Vec<_>>(),
        PickerStep::Key => key_options(state),
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
        PickerStep::Theme => state.theme_cursor,
        PickerStep::Provider => state.provider_cursor,
        PickerStep::Key => state.key_cursor,
        PickerStep::Model => state.model_cursor,
        PickerStep::Reasoning => state.effort_cursor,
    };
    let (start, end, show_above, show_below) = visible_window(labels.len(), cursor, height.max(1));
    let mut rows = Vec::with_capacity(height);
    if show_above {
        rows.push(Line::from(Span::styled(
            "  ↑ more",
            Style::default().fg(theme::quiet()),
        )));
    }
    for (index, label) in labels.iter().enumerate().take(end).skip(start) {
        rows.push(render_choice_row(label, index == cursor));
    }
    if show_below {
        rows.push(Line::from(Span::styled(
            "  ↓ more",
            Style::default().fg(theme::quiet()),
        )));
    }
    rows
}

fn key_options(state: &StartupModelPickerState) -> Vec<String> {
    let env = state
        .provider()
        .and_then(|provider| provider.credential.env_var())
        .unwrap_or("provider API key");
    vec![format!("Configure {env} later in /config")]
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
        (theme::quiet(), Style::default().fg(theme::foreground()))
    };
    let prefix = if active { "▸ " } else { "  " };
    Line::from(vec![
        Span::styled(prefix, Style::default().fg(prefix_color)),
        Span::styled(label.to_string(), style),
    ])
}

fn render_footer(state: &StartupModelPickerState) -> Line<'static> {
    let enter_label = match state.step {
        PickerStep::Theme if state.theme_action() == StartupThemeAction::ConfigureInConfig => {
            "continue"
        }
        PickerStep::Theme => "apply",
        PickerStep::Provider if state.provider_needs_key_config() => "key",
        PickerStep::Provider => "model",
        PickerStep::Key => "continue",
        PickerStep::Model if state.selected_model_requires_reasoning() => "effort",
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

fn visible_window(total: usize, cursor: usize, height: usize) -> (usize, usize, bool, bool) {
    if total == 0 || height == 0 {
        return (0, 0, false, false);
    }
    let cursor = cursor.min(total - 1);
    if total <= height {
        return (0, total, false, false);
    }
    if height == 1 {
        return (cursor, cursor + 1, false, false);
    }
    if height == 2 {
        return match (cursor > 0, cursor + 1 < total) {
            (true, _) => (cursor, cursor + 1, true, false),
            (false, true) => (cursor, cursor + 1, false, true),
            (false, false) => (cursor, cursor + 1, false, false),
        };
    }

    let mut show_above = false;
    let mut show_below = false;
    loop {
        let reserved = usize::from(show_above) + usize::from(show_below);
        let item_capacity = height.saturating_sub(reserved).max(1);
        let start = cursor
            .saturating_sub(item_capacity / 2)
            .min(total - item_capacity);
        let end = (start + item_capacity).min(total);
        let next_show_above = start > 0;
        let next_show_below = end < total;
        if next_show_above == show_above && next_show_below == show_below {
            return (start, end, show_above, show_below);
        }
        show_above = next_show_above;
        show_below = next_show_below;
    }
}

fn centered_area(full: Rect) -> Rect {
    let max_width = 98u16;
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

#[cfg(test)]
#[path = "startup_model_picker_tests.rs"]
mod tests;
