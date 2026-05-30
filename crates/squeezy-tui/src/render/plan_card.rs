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
use crate::render::{markdown, palette};

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

/// Background tint applied to every line of the card. Picked to read
/// well against both light and dark backgrounds via the existing
/// palette adapter; falls back to no background on `NO_COLOR` to stay
/// accessible.
fn card_background() -> Style {
    let tone = palette::palette_tone();
    let (r, g, b) = match tone {
        palette::PaletteTone::Dark => (28, 25, 38),
        palette::PaletteTone::Light => (245, 240, 255),
    };
    Style::default().bg(palette::best_color((r, g, b)))
}

/// Top-of-card render entry point. Pulls the body from disk and
/// composes the styled lines. Returns a single-line fallback ("plan
/// file missing") when the file has been deleted out from under us so
/// the transcript never silently empties.
pub(crate) fn render_plan_card(data: &PlanCardData) -> Vec<Line<'static>> {
    let body = match proposed_plan::read_plan_body(&data.path) {
        Ok(body) => body,
        Err(_) => return missing_file_card(data),
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
    boxed_card_lines(plan_title(&data.plan_id, step_count), lines)
}

/// Card shown when the backing file is missing. Stays in palette so
/// the transcript layout doesn't jump.
fn missing_file_card(data: &PlanCardData) -> Vec<Line<'static>> {
    boxed_card_lines(
        format!("Plan {} · file missing", data.plan_id),
        vec![Line::from(vec![Span::styled(
            data.path.display().to_string(),
            Style::default().fg(palette::ERROR_RED),
        )])],
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
        Style::default().fg(palette::QUIET),
    )])
}

fn blank_card_line() -> Line<'static> {
    Line::from("")
}

fn apply_card_background(line: Line<'static>) -> Line<'static> {
    let bg = card_background();
    let spans: Vec<Span<'static>> = line
        .spans
        .into_iter()
        .map(|span| {
            let style = span.style.patch(bg);
            Span::styled(span.content, style)
        })
        .collect();
    Line::from(spans)
}

fn boxed_card_lines(title: String, inner: Vec<Line<'static>>) -> Vec<Line<'static>> {
    let title_width = text_width(&title);
    let content_width = inner.iter().map(line_width).max().unwrap_or(0);
    let inner_width = content_width
        .saturating_add(2)
        .max(title_width.saturating_add(3))
        .max(24);
    let bg = card_background();
    let border = Style::default()
        .fg(palette::AMBER)
        .add_modifier(Modifier::BOLD)
        .patch(bg);
    let mut lines = Vec::with_capacity(inner.len() + 2);
    let title_fill = inner_width.saturating_sub(title_width.saturating_add(3));
    lines.push(Line::from(vec![
        Span::styled("╭─ ", border),
        Span::styled(title, border),
        Span::styled(format!(" {}╮", "─".repeat(title_fill)), border),
    ]));
    for line in inner {
        lines.push(boxed_content_line(line, inner_width));
    }
    lines.push(Line::from(vec![Span::styled(
        format!("╰{}╯", "─".repeat(inner_width)),
        border,
    )]));
    lines
}

fn boxed_content_line(line: Line<'static>, inner_width: usize) -> Line<'static> {
    let bg = card_background();
    let border = Style::default()
        .fg(palette::AMBER)
        .add_modifier(Modifier::BOLD)
        .patch(bg);
    let content_width = line_width(&line);
    let padding = inner_width.saturating_sub(content_width.saturating_add(2));
    let mut spans = vec![Span::styled("│ ", border)];
    spans.extend(apply_card_background(line).spans);
    if padding > 0 {
        spans.push(Span::styled(" ".repeat(padding), bg));
    }
    spans.push(Span::styled(" │", border));
    Line::from(spans)
}

fn line_width(line: &Line<'_>) -> usize {
    line.spans
        .iter()
        .map(|span| text_width(span.content.as_ref()))
        .sum()
}

fn text_width(text: &str) -> usize {
    text.chars().count()
}

fn diff_header_line(parent_id: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled("diff vs ", Style::default().fg(palette::QUIET)),
        Span::styled(
            parent_id.to_string(),
            Style::default()
                .fg(palette::MODE_PURPLE)
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
    let bg = card_background();
    let diff = TextDiff::from_lines(parent, current);
    let mut out = Vec::new();
    for change in diff.iter_all_changes() {
        let (sigil, fg) = match change.tag() {
            similar::ChangeTag::Equal => (' ', palette::QUIET),
            similar::ChangeTag::Insert => ('+', palette::DIFF_ADD_FG),
            similar::ChangeTag::Delete => ('-', palette::DIFF_DEL_FG),
        };
        let text = change.to_string();
        let text = text.trim_end_matches('\n').to_string();
        let span_text = format!("  {sigil} {text}");
        out.push(Line::from(vec![Span::styled(
            span_text,
            Style::default().fg(fg).patch(bg),
        )]));
    }
    out
}

#[cfg(test)]
#[path = "plan_card_tests.rs"]
mod tests;
