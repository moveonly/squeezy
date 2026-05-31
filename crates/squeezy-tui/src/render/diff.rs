use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use squeezy_vcs::DiffFile;

use crate::render::{cache, highlight, palette};

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
            Style::default().fg(crate::render::theme::quiet()),
        ))];
    }
    let Some(patch) = file
        .patch
        .as_deref()
        .filter(|patch| !patch.trim().is_empty())
    else {
        return vec![Line::from(Span::styled(
            "no patch available",
            Style::default().fg(crate::render::theme::quiet()),
        ))];
    };
    // Key on `(path, patch_hash)` so identical patches against different
    // paths still produce separate cache entries (the diff renderer
    // applies path-derived syntax highlighting).
    cache::get_or_compute_diff(&file.path, patch, || {
        render_patch(patch, language_hint_from_path(&file.path))
    })
}

pub(crate) fn render_patch_full_lines(
    patch: &str,
    language_hint: Option<&str>,
) -> Vec<Line<'static>> {
    render_patch(patch, language_hint)
}

fn render_patch(patch: &str, language_hint: Option<&str>) -> Vec<Line<'static>> {
    let lines = parse_patch(patch);
    render_parsed_lines(&lines, language_hint)
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

fn render_parsed_lines(lines: &[DiffLine], language_hint: Option<&str>) -> Vec<Line<'static>> {
    let gutter_width = lines
        .iter()
        .flat_map(|line| [line.old, line.new])
        .flatten()
        .map(decimal_width)
        .max()
        .unwrap_or(1);

    lines
        .iter()
        .map(|line| render_line(line, gutter_width, language_hint))
        .collect()
}

fn render_line(line: &DiffLine, gutter_width: usize, language_hint: Option<&str>) -> Line<'static> {
    if line.kind == DiffLineKind::Hunk {
        return Line::from(vec![
            Span::styled(
                format!("{:>width$} ", "", width = gutter_width),
                Style::default().fg(crate::render::theme::quiet()),
            ),
            Span::styled(
                line.content.clone(),
                Style::default()
                    .fg(palette::best_color(palette::rgb_components(
                        crate::render::theme::color(crate::render::theme::token::DIFF_HUNK),
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
    let sign = match line.kind {
        DiffLineKind::Add => '+',
        DiffLineKind::Delete => '-',
        DiffLineKind::Context => ' ',
        DiffLineKind::Hunk => ' ',
    };
    let fg_style = match line.kind {
        DiffLineKind::Add | DiffLineKind::Delete => Style::default(),
        DiffLineKind::Context => Style::default().fg(crate::render::theme::quiet()),
        DiffLineKind::Hunk => Style::default(),
    };
    let bg = match line.kind {
        DiffLineKind::Add => Some(diff_add_bg()),
        DiffLineKind::Delete => Some(diff_del_bg()),
        DiffLineKind::Context | DiffLineKind::Hunk => None,
    };

    let gutter_text = number
        .map(|number| format!("{number:>width$} ", width = gutter_width))
        .unwrap_or_else(|| format!("{:>width$} ", "", width = gutter_width));
    let mut gutter_style = Style::default().fg(crate::render::theme::quiet());
    if let Some(bg) = bg {
        gutter_style = gutter_style.bg(bg);
    }

    let mut spans = vec![Span::styled(gutter_text, gutter_style)];

    // Sign character carries the line's fg color and any bg tint.
    let mut sign_style = fg_style;
    if let Some(bg) = bg {
        sign_style = sign_style.bg(bg);
    }
    spans.push(Span::styled(sign.to_string(), sign_style));

    // Changed rows intentionally skip syntax highlighting: the only add/delete
    // cue should be the row background, not red/green or token-colored text.
    let syntax_hint = match line.kind {
        DiffLineKind::Add | DiffLineKind::Delete => None,
        DiffLineKind::Context | DiffLineKind::Hunk => language_hint,
    };
    let content_spans = content_spans(&line.content, syntax_hint, fg_style);
    for mut span in content_spans {
        if let Some(bg) = bg {
            span.style = span.style.bg(bg);
        }
        spans.push(span);
    }

    let mut rendered = Line::from(spans);
    if let Some(bg) = bg {
        rendered = rendered.style(Style::default().bg(bg));
    }
    rendered
}

fn content_spans(
    content: &str,
    language_hint: Option<&str>,
    fallback_style: Style,
) -> Vec<Span<'static>> {
    if let Some(hint) = language_hint
        && !content.is_empty()
    {
        let highlighted = highlight::highlight_code(Some(hint), content);
        if let Some(line) = highlighted.into_iter().next()
            && line.spans.iter().any(|span| span.style.fg.is_some())
        {
            return line.spans;
        }
    }
    vec![Span::styled(content.to_string(), fallback_style)]
}

/// Soft tint behind added lines.
pub(crate) fn diff_add_bg() -> Color {
    crate::render::theme::color(crate::render::theme::token::DIFF_ADDED_BG)
}

/// Soft tint behind removed lines.
pub(crate) fn diff_del_bg() -> Color {
    crate::render::theme::color(crate::render::theme::token::DIFF_REMOVED_BG)
}

pub(crate) fn language_hint_from_path(path: &str) -> Option<&str> {
    let trimmed = path.rsplit('/').next().unwrap_or(path);
    let (stem, ext) = trimmed.rsplit_once('.')?;
    // Dot-files (`.gitignore`, `.env`) have an empty stem — the leading
    // dot is not an extension separator, so report no hint.
    if stem.is_empty() { None } else { Some(ext) }
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

fn decimal_width(value: u32) -> usize {
    value.max(1).ilog10() as usize + 1
}

#[cfg(test)]
#[path = "diff_tests.rs"]
mod tests;
