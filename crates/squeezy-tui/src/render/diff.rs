use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use squeezy_vcs::DiffFile;

use crate::render::palette;

#[derive(Debug, Clone)]
struct DiffLine {
    kind: DiffLineKind,
    old: Option<u32>,
    new: Option<u32>,
    content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffLineKind {
    Add,
    Delete,
    Context,
    Hunk,
}

pub(crate) fn render_diff_file(file: &DiffFile) -> Vec<Line<'static>> {
    if file.binary {
        return vec![Line::from(Span::styled(
            "binary file",
            Style::default().fg(palette::QUIET),
        ))];
    }
    let Some(patch) = file
        .patch
        .as_deref()
        .filter(|patch| !patch.trim().is_empty())
    else {
        return vec![Line::from(Span::styled(
            "no patch available",
            Style::default().fg(palette::QUIET),
        ))];
    };
    render_patch(patch)
}

pub(crate) fn render_patch_preview_lines(patch: &str, limit: usize) -> Vec<Line<'static>> {
    let lines = parse_patch(patch);
    let lines = head_tail(lines, limit);
    render_parsed_lines(&lines)
}

pub(crate) fn render_patch_full_lines(patch: &str) -> Vec<Line<'static>> {
    render_patch(patch)
}

fn render_patch(patch: &str) -> Vec<Line<'static>> {
    let lines = parse_patch(patch);
    render_parsed_lines(&lines)
}

fn parse_patch(patch: &str) -> Vec<DiffLine> {
    let mut rendered = Vec::new();
    let mut old_line = 0;
    let mut new_line = 0;
    let mut saw_hunk = false;

    for raw in patch.lines().filter(|line| !is_diff_metadata_line(line)) {
        if let Some((old_start, new_start)) = parse_hunk_header(raw) {
            old_line = old_start;
            new_line = new_start;
            if saw_hunk {
                rendered.push(DiffLine {
                    kind: DiffLineKind::Hunk,
                    old: None,
                    new: None,
                    content: "⋮".to_string(),
                });
            }
            saw_hunk = true;
            continue;
        }

        let mut chars = raw.chars();
        let sign = chars.next().unwrap_or(' ');
        let content = chars.as_str().to_string();
        match sign {
            '+' => {
                rendered.push(DiffLine {
                    kind: DiffLineKind::Add,
                    old: None,
                    new: (new_line > 0).then_some(new_line),
                    content,
                });
                new_line = new_line.saturating_add(1);
            }
            '-' => {
                rendered.push(DiffLine {
                    kind: DiffLineKind::Delete,
                    old: (old_line > 0).then_some(old_line),
                    new: None,
                    content,
                });
                old_line = old_line.saturating_add(1);
            }
            ' ' => {
                rendered.push(DiffLine {
                    kind: DiffLineKind::Context,
                    old: (old_line > 0).then_some(old_line),
                    new: (new_line > 0).then_some(new_line),
                    content,
                });
                old_line = old_line.saturating_add(1);
                new_line = new_line.saturating_add(1);
            }
            _ => rendered.push(DiffLine {
                kind: DiffLineKind::Context,
                old: None,
                new: None,
                content: raw.to_string(),
            }),
        }
    }

    rendered
}

fn render_parsed_lines(lines: &[DiffLine]) -> Vec<Line<'static>> {
    let gutter_width = lines
        .iter()
        .flat_map(|line| [line.old, line.new])
        .flatten()
        .map(decimal_width)
        .max()
        .unwrap_or(1);

    lines
        .iter()
        .map(|line| render_line(line, gutter_width))
        .collect()
}

fn render_line(line: &DiffLine, gutter_width: usize) -> Line<'static> {
    if line.kind == DiffLineKind::Hunk {
        return Line::from(vec![
            Span::styled(
                format!("{:>width$} ", "", width = gutter_width),
                Style::default().fg(palette::QUIET),
            ),
            Span::styled(
                line.content.clone(),
                Style::default()
                    .fg(palette::best_color(palette::rgb_components(
                        palette::DIFF_HUNK_FG,
                    )))
                    .add_modifier(Modifier::BOLD),
            ),
        ]);
    }

    let number = match line.kind {
        DiffLineKind::Add => line.new,
        DiffLineKind::Delete => line.old,
        DiffLineKind::Context => line.new.or(line.old),
        DiffLineKind::Hunk => None,
    };
    let (sign, style) = match line.kind {
        DiffLineKind::Add => ('+', add_style()),
        DiffLineKind::Delete => ('-', delete_style()),
        DiffLineKind::Context => (' ', Style::default().fg(palette::QUIET)),
        DiffLineKind::Hunk => (' ', Style::default()),
    };
    let gutter = number
        .map(|number| format!("{number:>width$} ", width = gutter_width))
        .unwrap_or_else(|| format!("{:>width$} ", "", width = gutter_width));
    Line::from(vec![
        Span::styled(gutter, Style::default().fg(palette::QUIET)),
        Span::styled(format!("{sign}{}", line.content), style),
    ])
}

fn add_style() -> Style {
    Style::default()
        .fg(palette::best_color(palette::rgb_components(
            palette::DIFF_ADD_FG,
        )))
        .add_modifier(Modifier::BOLD)
}

fn delete_style() -> Style {
    Style::default()
        .fg(palette::best_color(palette::rgb_components(
            palette::DIFF_DEL_FG,
        )))
        .add_modifier(Modifier::BOLD)
}

fn parse_hunk_header(line: &str) -> Option<(u32, u32)> {
    if !line.starts_with("@@ ") {
        return None;
    }
    let mut parts = line.split_whitespace();
    parts.next()?;
    let old = parse_hunk_range(parts.next()?)?;
    let new = parse_hunk_range(parts.next()?)?;
    Some((old, new))
}

fn parse_hunk_range(range: &str) -> Option<u32> {
    range
        .trim_start_matches(['-', '+'])
        .split(',')
        .next()?
        .parse()
        .ok()
}

fn is_diff_metadata_line(line: &str) -> bool {
    line.starts_with("diff --git ")
        || line.starts_with("index ")
        || line.starts_with("--- ")
        || line.starts_with("+++ ")
}

fn head_tail(mut lines: Vec<DiffLine>, limit: usize) -> Vec<DiffLine> {
    if lines.len() <= limit {
        return lines;
    }
    let head = limit / 2;
    let tail = limit.saturating_sub(head).saturating_sub(1);
    let omitted = lines.len().saturating_sub(head + tail);
    let mut preview = lines.drain(..head).collect::<Vec<_>>();
    preview.push(DiffLine {
        kind: DiffLineKind::Hunk,
        old: None,
        new: None,
        content: format!("... +{omitted} lines (Ctrl-E to expand)"),
    });
    preview.extend(
        lines
            .into_iter()
            .rev()
            .take(tail)
            .collect::<Vec<_>>()
            .into_iter()
            .rev(),
    );
    preview
}

fn decimal_width(value: u32) -> usize {
    value.max(1).ilog10() as usize + 1
}
