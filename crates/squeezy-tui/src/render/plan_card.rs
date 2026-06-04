//! Styled "plan card" renderer.
//!
//! Plan-mode v3 (PR-F) replaces the original log-line representation of
//! newly proposed plans with a structured cell:
//!
//! ```text
//! ╭─ Plan plan-abc12 · 4 steps ─╮
//! │ .squeezy/plans/<session>/... │
//! │                              │
//! │ <markdown-rendered body>     │
//! │                              │
//! │ <diff vs parent, if any>     │
//! ╰──────────────────────────────╯
//! ```
//!
//! Body text and the diff are read from disk at render time so the card
//! survives transcript compaction (the persisted plan file is the
//! source of truth, not the cell's captured snapshot).

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use similar::TextDiff;
use std::path::{Path, PathBuf};

use crate::proposed_plan;
use crate::render::markdown;

/// Static metadata captured at the moment a plan was persisted. The
/// actual body is *not* cached here — readers go through
/// [`proposed_plan::read_plan_body`] so PR-G's auto-checkmarks and
/// PR-C's in-place refinements are reflected automatically.
#[derive(Debug, Clone)]
pub(crate) struct PlanCardData {
    pub plan_id: String,
    pub path: PathBuf,
    pub parent_plan_id: Option<String>,
}

/// Top-of-card render entry point. Pulls the body from disk and
/// composes the styled lines. Returns a single-line fallback ("plan
/// file missing") when the file has been deleted out from under us so
/// the transcript never silently empties.
///
/// `width` is the terminal column count the card will be painted into.
/// The box is clamped so it never exceeds it; without a clamp a single
/// long prose line makes the card wider than the viewport and the
/// `Wrap { trim: false }` paint shatters the border rows.
pub(crate) fn render_plan_card(data: &PlanCardData, width: Option<u16>) -> Vec<Line<'static>> {
    let body = match proposed_plan::read_plan_body(&data.path) {
        Ok(body) => body,
        Err(_) => return missing_file_card(data, width),
    };
    let step_count = crate::count_plan_steps(&body);
    let mut lines = Vec::new();
    lines.push(card_path_line(&data.path));
    lines.push(blank_card_line());
    for line in markdown::render_markdown(&body) {
        lines.push(line);
    }
    lines.push(blank_card_line());

    if let Some(parent_id) = data.parent_plan_id.as_deref() {
        let parent_path = sibling_plan_path(&data.path, parent_id);
        if let Ok(parent_body) = proposed_plan::read_plan_body(&parent_path) {
            lines.push(diff_header_line(parent_id));
            lines.extend(render_plan_diff(&parent_body, &body));
            lines.push(blank_card_line());
        }
    }
    plain_card_lines(plan_title(&data.plan_id, step_count), lines, width)
}

/// Card shown when the backing file is missing. Stays in palette so
/// the transcript layout doesn't jump.
fn missing_file_card(data: &PlanCardData, width: Option<u16>) -> Vec<Line<'static>> {
    plain_card_lines(
        format!("Plan {} · file missing", data.plan_id),
        vec![Line::from(vec![Span::styled(
            data.path.display().to_string(),
            Style::default().fg(crate::render::theme::red()),
        )])],
        width,
    )
}

fn plan_title(plan_id: &str, step_count: usize) -> String {
    match step_count {
        0 => format!("Plan {plan_id}"),
        1 => format!("Plan {plan_id} · 1 step"),
        _ => format!("Plan {plan_id} · {step_count} steps"),
    }
}

fn card_path_line(path: &Path) -> Line<'static> {
    Line::from(vec![Span::styled(
        path.display().to_string(),
        Style::default().fg(crate::render::theme::quiet()),
    )])
}

fn blank_card_line() -> Line<'static> {
    Line::from("")
}

/// Calm, borderless plan rendering: an amber heading marker, then the plan
/// body flush — no box and no filled background, so a plan reads as a quiet
/// section of the transcript instead of a loud full-width card.
fn plain_card_lines(
    title: String,
    inner: Vec<Line<'static>>,
    _width: Option<u16>,
) -> Vec<Line<'static>> {
    let heading = Line::from(vec![Span::styled(
        format!("◇ {title}"),
        Style::default()
            .fg(crate::render::theme::accent())
            .add_modifier(Modifier::BOLD),
    )]);
    let mut lines = Vec::with_capacity(inner.len() + 1);
    lines.push(heading);
    lines.extend(inner);
    lines
}

fn diff_header_line(parent_id: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            "diff vs ",
            Style::default().fg(crate::render::theme::quiet()),
        ),
        Span::styled(
            parent_id.to_string(),
            Style::default()
                .fg(crate::render::theme::magenta())
                .add_modifier(Modifier::ITALIC),
        ),
    ])
}

/// Construct a sibling plan file's absolute path. Plan files live next
/// to each other inside the session's plan dir, so we just rewrite the
/// last path component.
fn sibling_plan_path(path: &Path, sibling_id: &str) -> PathBuf {
    let mut parent = path.to_path_buf();
    parent.pop();
    parent.join(format!("{sibling_id}.md"))
}

/// Unified diff between the parent plan body and the new plan body,
/// rendered with the same color scheme as the patch viewer. Lines are
/// indented two spaces so they read as a sub-section of the card.
pub(crate) fn render_plan_diff(parent: &str, current: &str) -> Vec<Line<'static>> {
    let diff = TextDiff::from_lines(parent, current);
    let mut out = Vec::new();
    for change in diff.iter_all_changes() {
        let (sigil, fg) = match change.tag() {
            similar::ChangeTag::Equal => (' ', crate::render::theme::quiet()),
            similar::ChangeTag::Insert => (
                '+',
                crate::render::theme::color(crate::render::theme::token::DIFF_ADDED),
            ),
            similar::ChangeTag::Delete => (
                '-',
                crate::render::theme::color(crate::render::theme::token::DIFF_REMOVED),
            ),
        };
        let text = change.to_string();
        let text = text.trim_end_matches('\n').to_string();
        let span_text = format!("  {sigil} {text}");
        out.push(Line::from(vec![Span::styled(
            span_text,
            Style::default().fg(fg),
        )]));
    }
    out
}

#[cfg(test)]
#[path = "plan_card_tests.rs"]
mod tests;
