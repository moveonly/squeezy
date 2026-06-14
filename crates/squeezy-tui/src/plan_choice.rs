use std::path::PathBuf;

use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

use crate::render::palette;

/// Plan-mode UI/runtime state that crosses modal choice, Build handoff, and
/// pause/resume flows. Grouped so the plan feature owns its app-state cluster
/// instead of leaving independent fields scattered on `TuiApp`.
#[derive(Debug, Default)]
pub(crate) struct PlanUiState {
    pub(crate) current_id: Option<String>,
    pub(crate) pending_handoff: Option<PathBuf>,
    pub(crate) handoff_turns_seen: u32,
    pub(crate) pending_choice: Option<PendingPlanChoice>,
    pub(crate) pause: Option<PlanPauseState>,
    pub(crate) resume_marker: Option<String>,
}

/// Interactive prompt that appears after a `<proposed_plan>` block lands
/// and persists. Lets the user execute, refine, discard, or view the
/// plan file without typing a slash command.
#[derive(Debug, Clone)]
pub(crate) struct PendingPlanChoice {
    pub(crate) plan_id: String,
    pub(crate) plan_path: PathBuf,
    pub(crate) selection_index: usize,
}

/// Captured plan-execution state at the moment of a Shift+Tab pause
/// (PR-G item 6). `plan_id` is compared against the current plan id on
/// the next Plan->Build crossing so the resume marker can tell the
/// model whether the plan body was refined while paused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanPauseState {
    pub(crate) plan_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlanChoiceAction {
    Execute,
    ExecuteClean,
    Refine,
    Discard,
}

pub(crate) struct PlanChoiceOption {
    pub(crate) action: PlanChoiceAction,
    pub(crate) label: &'static str,
    pub(crate) hint: &'static str,
    pub(crate) shortcut: char,
}

pub(crate) const PLAN_CHOICES: &[PlanChoiceOption] = &[
    PlanChoiceOption {
        action: PlanChoiceAction::Execute,
        label: "Execute",
        hint: "switch to Build; keep history; run the plan",
        shortcut: 'e',
    },
    PlanChoiceOption {
        action: PlanChoiceAction::ExecuteClean,
        label: "Execute (clean)",
        hint: "compact prior chat to a summary, then run the plan",
        shortcut: 'c',
    },
    PlanChoiceOption {
        action: PlanChoiceAction::Refine,
        label: "Refine",
        hint: "stay in Plan; describe what to change",
        shortcut: 'r',
    },
    PlanChoiceOption {
        action: PlanChoiceAction::Discard,
        label: "Discard",
        hint: "delete the plan file and dismiss this prompt",
        shortcut: 'd',
    },
];

pub(crate) fn menu_lines(
    pending: &PendingPlanChoice,
    compact_plan_path: String,
) -> Vec<Line<'static>> {
    let selected = pending.selection_index.min(PLAN_CHOICES.len() - 1);
    let mut lines = vec![Line::from(vec![
        Span::styled(
            "Plan ready",
            Style::default()
                .fg(crate::render::theme::secondary())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" · {}", pending.plan_id),
            Style::default().fg(crate::render::theme::quiet()),
        ),
    ])];
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(compact_plan_path, Style::default().fg(palette::muted_fg())),
    ]));
    for (idx, option) in PLAN_CHOICES.iter().enumerate() {
        let is_selected = idx == selected;
        let marker = if is_selected { "› " } else { "  " };
        let label_style = if is_selected {
            Style::default().fg(crate::render::theme::secondary())
        } else {
            Style::default().fg(palette::muted_fg())
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
            Span::styled(
                format!("[{}] {}", option.shortcut, option.label),
                label_style,
            ),
            Span::styled(
                format!(" · {}", option.hint),
                Style::default().fg(crate::render::theme::quiet()),
            ),
        ]));
    }
    lines
}
