use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use squeezy_core::LanguageKind;
use tree_sitter::Language;
use tree_sitter_highlight::{Highlight, HighlightConfiguration, HighlightEvent, Highlighter};

const MAX_HIGHLIGHT_BYTES: usize = 512 * 1024;
const MAX_HIGHLIGHT_LINES: usize = 10_000;

pub(crate) const KEYWORD_COLOR: Color = Color::Indexed(141);
pub(crate) const TYPE_COLOR: Color = Color::Indexed(110);
pub(crate) const STRING_COLOR: Color = Color::Indexed(143);
pub(crate) const NUMBER_COLOR: Color = Color::Indexed(173);
pub(crate) const COMMENT_COLOR: Color = Color::Indexed(244);
pub(crate) const FUNCTION_COLOR: Color = Color::Indexed(81);
pub(crate) const VARIABLE_COLOR: Color = Color::Indexed(252);
pub(crate) const CONSTANT_COLOR: Color = Color::Indexed(179);
pub(crate) const OPERATOR_COLOR: Color = Color::Indexed(250);
pub(crate) const PUNCTUATION_COLOR: Color = Color::Indexed(245);

const HIGHLIGHT_NAMES: [&str; 15] = [
    "attribute",
    "comment",
    "constant",
    "constant.builtin",
    "constant.numeric",
    "function",
    "keyword",
    "number",
    "operator",
    "property",
    "punctuation.bracket",
    "punctuation.delimiter",
    "string",
    "type",
    "variable",
];

pub(crate) fn highlight_code(language_hint: Option<&str>, source: &str) -> Vec<Line<'static>> {
    if exceeds_highlight_limits(source) {
        return plain_lines(source);
    }
    let Some(spec) = language_spec(language_hint) else {
        return plain_lines(source);
    };
    let Ok(mut config) = HighlightConfiguration::new(
        spec.language,
        spec.name,
        spec.highlights_query,
        spec.injections_query,
        "",
    ) else {
        return plain_lines(source);
    };
    config.configure(&HIGHLIGHT_NAMES);

    let mut highlighter = Highlighter::new();
    let Ok(events) = highlighter.highlight(&config, source.as_bytes(), None, |_| None) else {
        return plain_lines(source);
    };
    render_events(source, events)
}

pub(crate) fn exceeds_highlight_limits(source: &str) -> bool {
    source.len() > MAX_HIGHLIGHT_BYTES || source.lines().count() > MAX_HIGHLIGHT_LINES
}

fn render_events(
    source: &str,
    events: impl Iterator<Item = Result<HighlightEvent, tree_sitter_highlight::Error>>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut spans = Vec::new();
    let mut style_stack = Vec::new();
    let mut current_style = Style::default();

    for event in events.flatten() {
        match event {
            HighlightEvent::Source { start, end } => {
                if let Some(text) = source.get(start..end) {
                    push_segment(&mut lines, &mut spans, text, current_style);
                }
            }
            HighlightEvent::HighlightStart(highlight) => {
                style_stack.push(current_style);
                current_style = highlight_style(highlight);
            }
            HighlightEvent::HighlightEnd => {
                current_style = style_stack.pop().unwrap_or_default();
            }
        }
    }

    if !spans.is_empty() || lines.is_empty() {
        lines.push(Line::from(spans));
    }
    lines
}

fn push_segment(
    lines: &mut Vec<Line<'static>>,
    spans: &mut Vec<Span<'static>>,
    text: &str,
    style: Style,
) {
    for segment in text.split_inclusive('\n') {
        if let Some(line) = segment.strip_suffix('\n') {
            if !line.is_empty() {
                spans.push(Span::styled(line.to_string(), style));
            }
            lines.push(Line::from(std::mem::take(spans)));
        } else if !segment.is_empty() {
            spans.push(Span::styled(segment.to_string(), style));
        }
    }
}

fn highlight_style(highlight: Highlight) -> Style {
    let color = match HIGHLIGHT_NAMES.get(highlight.0).copied().unwrap_or("") {
        "attribute" | "property" => VARIABLE_COLOR,
        "comment" => COMMENT_COLOR,
        "constant" => CONSTANT_COLOR,
        "constant.builtin" => NUMBER_COLOR,
        "constant.numeric" => NUMBER_COLOR,
        "function" => FUNCTION_COLOR,
        "keyword" => KEYWORD_COLOR,
        "number" => NUMBER_COLOR,
        "operator" => OPERATOR_COLOR,
        "punctuation.bracket" | "punctuation.delimiter" => PUNCTUATION_COLOR,
        "string" => STRING_COLOR,
        "type" => TYPE_COLOR,
        "variable" => VARIABLE_COLOR,
        _ => Color::White,
    };
    let mut style = Style::default().fg(color);
    if matches!(
        HIGHLIGHT_NAMES.get(highlight.0).copied(),
        Some("keyword" | "type" | "function")
    ) {
        style = style.add_modifier(Modifier::BOLD);
    }
    style
}

fn plain_lines(source: &str) -> Vec<Line<'static>> {
    if source.is_empty() {
        return vec![Line::from("")];
    }
    source
        .split('\n')
        .map(|line| Line::from(line.to_string()))
        .collect()
}

struct LanguageSpec {
    name: &'static str,
    language: Language,
    highlights_query: &'static str,
    injections_query: &'static str,
}

fn language_spec(language_hint: Option<&str>) -> Option<LanguageSpec> {
    match language_kind(language_hint?) {
        LanguageKind::C => Some(LanguageSpec {
            name: "c",
            language: tree_sitter_c::LANGUAGE.into(),
            highlights_query: tree_sitter_c::HIGHLIGHT_QUERY,
            injections_query: "",
        }),
        LanguageKind::CSharp => Some(LanguageSpec {
            name: "csharp",
            language: tree_sitter_c_sharp::LANGUAGE.into(),
            highlights_query: tree_sitter_c_sharp::HIGHLIGHTS_QUERY,
            injections_query: "",
        }),
        LanguageKind::Cpp => Some(LanguageSpec {
            name: "cpp",
            language: tree_sitter_cpp::LANGUAGE.into(),
            highlights_query: tree_sitter_cpp::HIGHLIGHT_QUERY,
            injections_query: "",
        }),
        LanguageKind::Go => Some(LanguageSpec {
            name: "go",
            language: tree_sitter_go::LANGUAGE.into(),
            highlights_query: tree_sitter_go::HIGHLIGHTS_QUERY,
            injections_query: "",
        }),
        LanguageKind::Java => Some(LanguageSpec {
            name: "java",
            language: tree_sitter_java::LANGUAGE.into(),
            highlights_query: tree_sitter_java::HIGHLIGHTS_QUERY,
            injections_query: "",
        }),
        LanguageKind::JavaScript => Some(LanguageSpec {
            name: "javascript",
            language: tree_sitter_javascript::LANGUAGE.into(),
            highlights_query: tree_sitter_javascript::HIGHLIGHT_QUERY,
            injections_query: tree_sitter_javascript::INJECTIONS_QUERY,
        }),
        LanguageKind::Jsx => Some(LanguageSpec {
            name: "jsx",
            language: tree_sitter_javascript::LANGUAGE.into(),
            highlights_query: tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
            injections_query: tree_sitter_javascript::INJECTIONS_QUERY,
        }),
        LanguageKind::Python => Some(LanguageSpec {
            name: "python",
            language: tree_sitter_python::LANGUAGE.into(),
            highlights_query: tree_sitter_python::HIGHLIGHTS_QUERY,
            injections_query: "",
        }),
        LanguageKind::Rust => Some(LanguageSpec {
            name: "rust",
            language: tree_sitter_rust::LANGUAGE.into(),
            highlights_query: tree_sitter_rust::HIGHLIGHTS_QUERY,
            injections_query: tree_sitter_rust::INJECTIONS_QUERY,
        }),
        LanguageKind::TypeScript => Some(LanguageSpec {
            name: "typescript",
            language: tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            highlights_query: tree_sitter_typescript::HIGHLIGHTS_QUERY,
            injections_query: "",
        }),
        LanguageKind::Tsx => Some(LanguageSpec {
            name: "tsx",
            language: tree_sitter_typescript::LANGUAGE_TSX.into(),
            highlights_query: tree_sitter_typescript::HIGHLIGHTS_QUERY,
            injections_query: "",
        }),
        LanguageKind::Unsupported | LanguageKind::Unknown => None,
    }
}

fn language_kind(hint: &str) -> LanguageKind {
    let hint = hint
        .trim()
        .trim_start_matches('.')
        .split(|ch: char| ch.is_whitespace() || ch == ',' || ch == ';')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match hint.as_str() {
        "c#" | "csharp" | "cs" | "csx" => LanguageKind::CSharp,
        "c++" | "cpp" | "cc" | "cxx" | "hpp" | "hxx" | "hh" => LanguageKind::Cpp,
        "javascript" | "js" | "mjs" | "cjs" => LanguageKind::JavaScript,
        "jsx" => LanguageKind::Jsx,
        "typescript" | "ts" | "mts" | "cts" => LanguageKind::TypeScript,
        "tsx" => LanguageKind::Tsx,
        "python" | "py" => LanguageKind::Python,
        "rust" | "rs" => LanguageKind::Rust,
        "golang" | "go" => LanguageKind::Go,
        "java" => LanguageKind::Java,
        "c" | "h" => LanguageKind::C,
        other => LanguageKind::from_extension(other),
    }
}
