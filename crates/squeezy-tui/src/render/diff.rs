use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use squeezy_vcs::DiffFile;

use crate::render::{highlight, palette};

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
    render_patch(patch, language_hint_from_path(&file.path))
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
    let sign = match line.kind {
        DiffLineKind::Add => '+',
        DiffLineKind::Delete => '-',
        DiffLineKind::Context => ' ',
        DiffLineKind::Hunk => ' ',
    };
    let fg_style = match line.kind {
        DiffLineKind::Add => add_fg_style(),
        DiffLineKind::Delete => delete_fg_style(),
        DiffLineKind::Context => Style::default().fg(palette::QUIET),
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
    let mut gutter_style = Style::default().fg(palette::QUIET);
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

    // Content spans: try syntax highlighting; fall back to a single span
    // with the diff foreground color. Either way, layer the bg tint.
    let content_spans = content_spans(&line.content, language_hint, fg_style);
    for mut span in content_spans {
        if let Some(bg) = bg {
            span.style = span.style.bg(bg);
        }
        spans.push(span);
    }

    Line::from(spans)
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

fn add_fg_style() -> Style {
    Style::default()
        .fg(palette::best_color(palette::rgb_components(
            palette::DIFF_ADD_FG,
        )))
        .add_modifier(Modifier::BOLD)
}

fn delete_fg_style() -> Style {
    Style::default()
        .fg(palette::best_color(palette::rgb_components(
            palette::DIFF_DEL_FG,
        )))
        .add_modifier(Modifier::BOLD)
}

/// Soft green tint behind added lines. Values mirror codex's diff
/// backgrounds (`#213A2B` dark / `#dafbe1` light) so the look matches
/// existing patch-review tools and reads on both themes via
/// `palette::best_color` quantisation.
pub(crate) fn diff_add_bg() -> Color {
    let rgb = match palette::palette_tone() {
        palette::PaletteTone::Dark => (33, 58, 43),
        palette::PaletteTone::Light => (218, 251, 225),
    };
    palette::best_color(rgb)
}

/// Soft red tint behind removed lines. Values mirror codex's
/// `#4A221D` dark / `#ffebe9` light diff backgrounds.
pub(crate) fn diff_del_bg() -> Color {
    let rgb = match palette::palette_tone() {
        palette::PaletteTone::Dark => (74, 34, 29),
        palette::PaletteTone::Light => (255, 235, 233),
    };
    palette::best_color(rgb)
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
mod tests {
    use super::*;
    use squeezy_vcs::{DiffFile, DiffFileStatus, DiffHunk};

    fn sample_file(path: &str, patch: &str) -> DiffFile {
        DiffFile {
            path: path.to_string(),
            status: DiffFileStatus::Modified,
            code: "M".to_string(),
            additions: 1,
            deletions: 1,
            binary: false,
            hunks: vec![DiffHunk {
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 1,
                start_line: 1,
                end_line: 2,
            }],
            patch: Some(patch.to_string()),
            patch_truncated: false,
        }
    }

    fn find_span<'a>(lines: &'a [Line<'static>], starts_with: &str) -> &'a Span<'static> {
        lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.as_ref().starts_with(starts_with))
            .unwrap_or_else(|| panic!("no span starts with {starts_with:?}"))
    }

    #[test]
    fn added_lines_carry_green_background_tint() {
        let file = sample_file("src/lib.rs", "@@ -1 +1 @@\n-old\n+new\n");
        let lines = render_diff_file(&file);

        let add_sign = find_span(&lines, "+");
        assert_eq!(
            add_sign.style.bg,
            Some(diff_add_bg()),
            "+ sign should carry add bg tint",
        );
    }

    #[test]
    fn removed_lines_carry_red_background_tint() {
        let file = sample_file("src/lib.rs", "@@ -1 +1 @@\n-old\n+new\n");
        let lines = render_diff_file(&file);

        let del_sign = find_span(&lines, "-");
        assert_eq!(
            del_sign.style.bg,
            Some(diff_del_bg()),
            "- sign should carry delete bg tint",
        );
    }

    #[test]
    fn context_lines_have_no_background_tint() {
        let file = sample_file("src/lib.rs", "@@ -1,3 +1,3 @@\n context\n-old\n+new\n");
        let lines = render_diff_file(&file);

        // context line content begins with a literal space, then the body.
        let context_line = lines
            .iter()
            .find(|line| {
                line.spans
                    .iter()
                    .any(|span| span.content.as_ref() == "context")
            })
            .expect("context line");
        for span in &context_line.spans {
            assert_eq!(
                span.style.bg, None,
                "context spans should not have a bg tint",
            );
        }
    }

    #[test]
    fn gutter_on_changed_lines_shares_the_tint() {
        let file = sample_file("src/lib.rs", "@@ -1 +1 @@\n-old\n+new\n");
        let lines = render_diff_file(&file);

        let add_line = lines
            .iter()
            .find(|line| {
                line.spans
                    .iter()
                    .any(|span| span.content.as_ref().starts_with('+'))
            })
            .expect("add line");
        let del_line = lines
            .iter()
            .find(|line| {
                line.spans
                    .iter()
                    .any(|span| span.content.as_ref().starts_with('-'))
            })
            .expect("del line");

        // Every span on a +/- line (gutter, sign, content) carries the tint.
        for span in &add_line.spans {
            assert_eq!(span.style.bg, Some(diff_add_bg()));
        }
        for span in &del_line.spans {
            assert_eq!(span.style.bg, Some(diff_del_bg()));
        }
    }

    #[test]
    fn rust_content_picks_up_syntax_highlight_in_diff() {
        // A `fn` keyword inside an added line should land as a `keyword`
        // capture, picking up the highlighter's keyword color rather than
        // the bare diff-add foreground.
        let file = sample_file(
            "src/lib.rs",
            "@@ -1 +1 @@\n-fn old() {}\n+fn brand_new() {}\n",
        );
        let lines = render_diff_file(&file);

        let add_fn_span = lines
            .iter()
            .find(|line| {
                line.spans
                    .iter()
                    .any(|span| span.content.as_ref().starts_with('+'))
            })
            .and_then(|line| line.spans.iter().find(|span| span.content.as_ref() == "fn"))
            .expect("`fn` keyword span on the added line");
        assert_eq!(
            add_fn_span.style.fg,
            Some(highlight::HighlightPalette::current().keyword),
            "`fn` should be coloured by the highlighter, not the diff fg",
        );
        // Highlighted spans on added lines inherit the add-line bg tint.
        assert_eq!(add_fn_span.style.bg, Some(diff_add_bg()));
    }

    #[test]
    fn unknown_extension_falls_back_to_diff_foreground() {
        let file = sample_file("notes.unknownext", "@@ -1 +1 @@\n-old line\n+new line\n");
        let lines = render_diff_file(&file);

        let add_content = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.as_ref() == "new line")
            .expect("add content span");
        assert_eq!(
            add_content.style.fg,
            Some(palette::best_color(palette::rgb_components(
                palette::DIFF_ADD_FG,
            ))),
            "without a known language hint we keep the diff fg color",
        );
        assert_eq!(add_content.style.bg, Some(diff_add_bg()));
    }

    #[test]
    fn language_hint_from_path_extracts_extension() {
        assert_eq!(language_hint_from_path("src/lib.rs"), Some("rs"));
        assert_eq!(language_hint_from_path("README"), None);
        assert_eq!(language_hint_from_path("a/b/c.tsx"), Some("tsx"));
        assert_eq!(language_hint_from_path(".gitignore"), None);
    }
}
