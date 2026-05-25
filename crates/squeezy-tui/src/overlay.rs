//! Interactive overlays for `/model`, `/verbosity`, `/tool-verbosity`,
//! and `/permissions`.
//!
//! The audit calls out that today these commands print snapshots into
//! the transcript with no way to pick visually. The overlay replaces
//! that with an inline list-of-options picker reusing the slash/mention
//! popup pattern: Up/Down to move, Enter to apply, Esc to cancel.
//!
//! Permissions overlay is read-only here (it lists current rules); the
//! editing flow stays with the existing `/permissions allow ...` slash
//! command. That keeps the persistence path inside `squeezy-agent`
//! unchanged for this PR.

#![allow(dead_code)]

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use squeezy_core::{ResponseVerbosity, ToolOutputVerbosity};
use squeezy_llm::{MODEL_REGISTRY, ModelInfo};

use crate::{AMBER, GOLD, QUIET};

#[derive(Debug, Clone)]
pub(crate) enum Overlay {
    Model(SelectOverlay<ModelEntry>),
    Verbosity(SelectOverlay<VerbosityEntry>),
    ToolVerbosity(SelectOverlay<ToolVerbosityEntry>),
    Permissions(ReadOnlyOverlay),
}

impl Overlay {
    pub(crate) fn title(&self) -> &'static str {
        match self {
            Overlay::Model(_) => "Select model",
            Overlay::Verbosity(_) => "Select response verbosity",
            Overlay::ToolVerbosity(_) => "Select tool output verbosity",
            Overlay::Permissions(_) => "Permission rules",
        }
    }

    pub(crate) fn move_up(&mut self) {
        match self {
            Overlay::Model(o) => o.move_up(),
            Overlay::Verbosity(o) => o.move_up(),
            Overlay::ToolVerbosity(o) => o.move_up(),
            Overlay::Permissions(_) => {}
        }
    }

    pub(crate) fn move_down(&mut self) {
        match self {
            Overlay::Model(o) => o.move_down(),
            Overlay::Verbosity(o) => o.move_down(),
            Overlay::ToolVerbosity(o) => o.move_down(),
            Overlay::Permissions(_) => {}
        }
    }

    pub(crate) fn render_lines(&self) -> Vec<Line<'static>> {
        let mut lines = vec![header_line(self.title())];
        match self {
            Overlay::Model(o) => lines.extend(o.render(|e| e.label())),
            Overlay::Verbosity(o) => lines.extend(o.render(|e| e.label())),
            Overlay::ToolVerbosity(o) => lines.extend(o.render(|e| e.label())),
            Overlay::Permissions(o) => lines.extend(o.render()),
        }
        lines.push(footer_line());
        lines
    }
}

fn header_line(title: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            title,
            Style::default().fg(GOLD).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "  ↑↓ choose · Enter apply · Esc cancel",
            Style::default().fg(QUIET),
        ),
    ])
}

fn footer_line() -> Line<'static> {
    Line::from(Span::styled("", Style::default().fg(QUIET)))
}

#[derive(Debug, Clone)]
pub(crate) struct SelectOverlay<T> {
    pub entries: Vec<T>,
    pub selected: usize,
}

impl<T> SelectOverlay<T> {
    pub(crate) fn new(entries: Vec<T>, default_index: usize) -> Self {
        let selected = default_index.min(entries.len().saturating_sub(1));
        Self { entries, selected }
    }

    pub(crate) fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    pub(crate) fn move_down(&mut self) {
        if self.selected + 1 < self.entries.len() {
            self.selected += 1;
        }
    }

    pub(crate) fn selected(&self) -> Option<&T> {
        self.entries.get(self.selected)
    }

    fn render(&self, label: impl Fn(&T) -> String) -> Vec<Line<'static>> {
        const WINDOW: usize = 5;
        let total = self.entries.len();
        if total == 0 {
            return Vec::new();
        }
        let half = WINDOW / 2;
        let start = self
            .selected
            .saturating_sub(half)
            .min(total.saturating_sub(WINDOW));
        let end = (start + WINDOW).min(total);
        self.entries[start..end]
            .iter()
            .enumerate()
            .map(|(rel, entry)| {
                let index = start + rel;
                let is_selected = index == self.selected;
                let marker = if is_selected { "› " } else { "  " };
                let style = if is_selected {
                    Style::default().fg(GOLD).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                Line::from(vec![
                    Span::styled(
                        marker,
                        Style::default().fg(if is_selected { GOLD } else { QUIET }),
                    ),
                    Span::styled(label(entry), style),
                ])
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ReadOnlyOverlay {
    pub lines: Vec<String>,
}

impl ReadOnlyOverlay {
    fn render(&self) -> Vec<Line<'static>> {
        self.lines
            .iter()
            .map(|s| Line::from(Span::styled(s.clone(), Style::default().fg(AMBER))))
            .collect()
    }
}

// ---- Model overlay ----

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelEntry {
    pub provider: &'static str,
    pub id: &'static str,
    pub context_window: Option<u64>,
}

impl ModelEntry {
    fn label(&self) -> String {
        let ctx = self
            .context_window
            .map(|c| format!(" · ctx {c}"))
            .unwrap_or_default();
        format!("{}:{}{}", self.provider, self.id, ctx)
    }
}

pub(crate) fn build_model_overlay(current_provider: &str, current_id: &str) -> Overlay {
    let entries: Vec<ModelEntry> = MODEL_REGISTRY
        .iter()
        .map(|model: &ModelInfo| ModelEntry {
            provider: model.provider,
            id: model.id,
            context_window: model.limits.map(|l| l.context_window_tokens),
        })
        .collect();
    let default_index = entries
        .iter()
        .position(|e| e.provider == current_provider && e.id == current_id)
        .unwrap_or(0);
    Overlay::Model(SelectOverlay::new(entries, default_index))
}

// ---- Verbosity overlays ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VerbosityEntry(pub ResponseVerbosity);

impl VerbosityEntry {
    fn label(&self) -> String {
        let descr = match self.0 {
            ResponseVerbosity::Concise => "short answers, minimal preamble",
            ResponseVerbosity::Normal => "balanced; default",
            ResponseVerbosity::Verbose => "detailed walkthroughs",
        };
        format!("{}  ·  {}", self.0.as_str(), descr)
    }
}

pub(crate) fn build_verbosity_overlay(current: ResponseVerbosity) -> Overlay {
    let entries = vec![
        VerbosityEntry(ResponseVerbosity::Concise),
        VerbosityEntry(ResponseVerbosity::Normal),
        VerbosityEntry(ResponseVerbosity::Verbose),
    ];
    let default_index = entries.iter().position(|e| e.0 == current).unwrap_or(1);
    Overlay::Verbosity(SelectOverlay::new(entries, default_index))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ToolVerbosityEntry(pub ToolOutputVerbosity);

impl ToolVerbosityEntry {
    fn label(&self) -> String {
        let descr = match self.0 {
            ToolOutputVerbosity::Compact => "first error, last few lines only",
            ToolOutputVerbosity::Normal => "balanced; default",
            ToolOutputVerbosity::Verbose => "full stdout/stderr",
        };
        format!("{}  ·  {}", self.0.as_str(), descr)
    }
}

pub(crate) fn build_tool_verbosity_overlay(current: ToolOutputVerbosity) -> Overlay {
    let entries = vec![
        ToolVerbosityEntry(ToolOutputVerbosity::Compact),
        ToolVerbosityEntry(ToolOutputVerbosity::Normal),
        ToolVerbosityEntry(ToolOutputVerbosity::Verbose),
    ];
    let default_index = entries.iter().position(|e| e.0 == current).unwrap_or(1);
    Overlay::ToolVerbosity(SelectOverlay::new(entries, default_index))
}

#[cfg(test)]
#[path = "overlay_tests.rs"]
mod tests;
