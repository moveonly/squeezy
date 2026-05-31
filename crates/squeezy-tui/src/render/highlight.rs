use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use tree_sitter::Language;
use tree_sitter_highlight::{Highlight, HighlightConfiguration, HighlightEvent, Highlighter};

use crate::render::cache;
use crate::render::theme::{self, token};

const MAX_HIGHLIGHT_BYTES: usize = 512 * 1024;
const MAX_HIGHLIGHT_LINES: usize = 10_000;

/// Foreground colors for each highlight category. Built per render from the
/// active theme so a `/theme` flip is reflected on the next draw without
/// rebuilding any highlighter state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HighlightPalette {
    pub keyword: Color,
    pub r#type: Color,
    pub string: Color,
    pub number: Color,
    pub comment: Color,
    pub function: Color,
    pub variable: Color,
    pub constant: Color,
    pub operator: Color,
    pub punctuation: Color,
}

impl HighlightPalette {
    /// Snapshot of the palette to use for the current draw.
    pub(crate) fn current() -> Self {
        Self {
            keyword: theme::color(token::SYNTAX_KEYWORD),
            r#type: theme::color(token::SYNTAX_TYPE),
            string: theme::color(token::SYNTAX_STRING),
            number: theme::color(token::SYNTAX_LITERAL),
            comment: theme::color(token::SYNTAX_COMMENT),
            function: theme::color(token::SYNTAX_FUNCTION),
            variable: theme::color(token::SYNTAX_VARIABLE),
            constant: theme::color(token::SYNTAX_LITERAL),
            operator: theme::color(token::SYNTAX_OPERATOR),
            punctuation: theme::color(token::SYNTAX_OPERATOR),
        }
    }
}

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
    let Some(lang) = language_hint.and_then(HighlightLanguage::from_hint) else {
        return plain_lines(source);
    };
    // `LanguageSpec::name` is `&'static str`, so it can key the cache directly.
    let name = language_spec(lang).name;
    cache::get_or_compute_highlight(source, name, || highlight_uncached(source, lang))
}

fn highlight_uncached(source: &str, lang: HighlightLanguage) -> Vec<Line<'static>> {
    let Some(config) = highlight_config(lang) else {
        return plain_lines(source);
    };
    let mut highlighter = Highlighter::new();
    let Ok(events) = highlighter.highlight(&config, source.as_bytes(), None, |_| None) else {
        return plain_lines(source);
    };
    render_events(source, events, &HighlightPalette::current())
}

/// Process-lifetime cache of [`HighlightConfiguration`] keyed by language.
///
/// `HighlightConfiguration::new` parses the language's highlight + injection
/// queries and `configure(&HIGHLIGHT_NAMES)` resolves capture indices against
/// the shared name table — multi-millisecond work per call for nontrivial
/// grammars. Hoisting the result behind an `Arc` turns every render past the
/// first into an `Arc::clone`, which is the entire point of this finding.
///
/// Construction is per-entry lazy: the `LazyLock` only allocates an empty
/// map, and each language is built on its first lookup. Our supported
/// language set is finite (~21 entries) and bounded by the
/// [`HighlightLanguage`] enum, so we deliberately do not impose an LRU
/// eviction policy — once a language is built we keep it for the life of
/// the process. Failed builds are not cached so a transient bug in a
/// grammar still falls back to [`plain_lines`] on every call rather than
/// silently sticking.
fn highlight_config(lang: HighlightLanguage) -> Option<Arc<HighlightConfiguration>> {
    static CACHE: LazyLock<Mutex<HashMap<HighlightLanguage, Arc<HighlightConfiguration>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    if let Ok(cache) = CACHE.lock()
        && let Some(existing) = cache.get(&lang)
    {
        return Some(Arc::clone(existing));
    }
    let spec = language_spec(lang);
    let mut config = HighlightConfiguration::new(
        spec.language,
        spec.name,
        spec.highlights_query,
        spec.injections_query,
        "",
    )
    .ok()?;
    config.configure(&HIGHLIGHT_NAMES);
    let arc = Arc::new(config);
    if let Ok(mut cache) = CACHE.lock() {
        // Two threads racing the first lookup would each build a fresh
        // config; prefer whichever entry won the insert race so subsequent
        // callers see a stable `Arc::ptr_eq` view.
        return Some(Arc::clone(cache.entry(lang).or_insert(arc)));
    }
    Some(arc)
}

pub(crate) fn exceeds_highlight_limits(source: &str) -> bool {
    source.len() > MAX_HIGHLIGHT_BYTES || source.lines().count() > MAX_HIGHLIGHT_LINES
}

fn render_events(
    source: &str,
    events: impl Iterator<Item = Result<HighlightEvent, tree_sitter_highlight::Error>>,
    palette: &HighlightPalette,
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
                current_style = highlight_style(highlight, palette);
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

fn highlight_style(highlight: Highlight, palette: &HighlightPalette) -> Style {
    let name = HIGHLIGHT_NAMES.get(highlight.0).copied().unwrap_or("");
    let color = match name {
        "attribute" | "property" => palette.variable,
        "comment" => palette.comment,
        "constant" => palette.constant,
        "constant.builtin" => palette.number,
        "constant.numeric" => palette.number,
        "function" => palette.function,
        "keyword" => palette.keyword,
        "number" => palette.number,
        "operator" => palette.operator,
        "punctuation.bracket" | "punctuation.delimiter" => palette.punctuation,
        "string" => palette.string,
        "type" => palette.r#type,
        "variable" => palette.variable,
        _ => Color::Reset,
    };
    let mut style = Style::default().fg(color);
    if matches!(name, "keyword" | "type" | "function") {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum HighlightLanguage {
    Bash,
    C,
    CSharp,
    Cpp,
    Css,
    Go,
    Html,
    Java,
    JavaScript,
    Json,
    Jsx,
    Lua,
    Php,
    Python,
    Ruby,
    Rust,
    Sql,
    Toml,
    TypeScript,
    Tsx,
    Yaml,
}

fn language_spec(lang: HighlightLanguage) -> LanguageSpec {
    match lang {
        HighlightLanguage::Bash => LanguageSpec {
            name: "bash",
            language: tree_sitter_bash::LANGUAGE.into(),
            highlights_query: tree_sitter_bash::HIGHLIGHT_QUERY,
            injections_query: "",
        },
        HighlightLanguage::C => LanguageSpec {
            name: "c",
            language: tree_sitter_c::LANGUAGE.into(),
            highlights_query: tree_sitter_c::HIGHLIGHT_QUERY,
            injections_query: "",
        },
        HighlightLanguage::CSharp => LanguageSpec {
            name: "csharp",
            language: tree_sitter_c_sharp::LANGUAGE.into(),
            highlights_query: tree_sitter_c_sharp::HIGHLIGHTS_QUERY,
            injections_query: "",
        },
        HighlightLanguage::Cpp => LanguageSpec {
            name: "cpp",
            language: tree_sitter_cpp::LANGUAGE.into(),
            highlights_query: tree_sitter_cpp::HIGHLIGHT_QUERY,
            injections_query: "",
        },
        HighlightLanguage::Css => LanguageSpec {
            name: "css",
            language: tree_sitter_css::LANGUAGE.into(),
            highlights_query: tree_sitter_css::HIGHLIGHTS_QUERY,
            injections_query: "",
        },
        HighlightLanguage::Go => LanguageSpec {
            name: "go",
            language: tree_sitter_go::LANGUAGE.into(),
            highlights_query: tree_sitter_go::HIGHLIGHTS_QUERY,
            injections_query: "",
        },
        HighlightLanguage::Html => LanguageSpec {
            name: "html",
            language: tree_sitter_html::LANGUAGE.into(),
            highlights_query: tree_sitter_html::HIGHLIGHTS_QUERY,
            injections_query: tree_sitter_html::INJECTIONS_QUERY,
        },
        HighlightLanguage::Java => LanguageSpec {
            name: "java",
            language: tree_sitter_java::LANGUAGE.into(),
            highlights_query: tree_sitter_java::HIGHLIGHTS_QUERY,
            injections_query: "",
        },
        HighlightLanguage::JavaScript => LanguageSpec {
            name: "javascript",
            language: tree_sitter_javascript::LANGUAGE.into(),
            highlights_query: tree_sitter_javascript::HIGHLIGHT_QUERY,
            injections_query: tree_sitter_javascript::INJECTIONS_QUERY,
        },
        HighlightLanguage::Json => LanguageSpec {
            name: "json",
            language: tree_sitter_json::LANGUAGE.into(),
            highlights_query: tree_sitter_json::HIGHLIGHTS_QUERY,
            injections_query: "",
        },
        HighlightLanguage::Jsx => LanguageSpec {
            name: "jsx",
            language: tree_sitter_javascript::LANGUAGE.into(),
            highlights_query: tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
            injections_query: tree_sitter_javascript::INJECTIONS_QUERY,
        },
        HighlightLanguage::Lua => LanguageSpec {
            name: "lua",
            language: tree_sitter_lua::LANGUAGE.into(),
            highlights_query: tree_sitter_lua::HIGHLIGHTS_QUERY,
            injections_query: "",
        },
        HighlightLanguage::Php => LanguageSpec {
            name: "php",
            language: tree_sitter_php::LANGUAGE_PHP.into(),
            highlights_query: tree_sitter_php::HIGHLIGHTS_QUERY,
            injections_query: tree_sitter_php::INJECTIONS_QUERY,
        },
        HighlightLanguage::Python => LanguageSpec {
            name: "python",
            language: tree_sitter_python::LANGUAGE.into(),
            highlights_query: tree_sitter_python::HIGHLIGHTS_QUERY,
            injections_query: "",
        },
        HighlightLanguage::Ruby => LanguageSpec {
            name: "ruby",
            language: tree_sitter_ruby::LANGUAGE.into(),
            highlights_query: tree_sitter_ruby::HIGHLIGHTS_QUERY,
            injections_query: "",
        },
        HighlightLanguage::Rust => LanguageSpec {
            name: "rust",
            language: tree_sitter_rust::LANGUAGE.into(),
            highlights_query: tree_sitter_rust::HIGHLIGHTS_QUERY,
            injections_query: tree_sitter_rust::INJECTIONS_QUERY,
        },
        HighlightLanguage::Sql => LanguageSpec {
            name: "sql",
            language: tree_sitter_sequel::LANGUAGE.into(),
            highlights_query: tree_sitter_sequel::HIGHLIGHTS_QUERY,
            injections_query: "",
        },
        HighlightLanguage::Toml => LanguageSpec {
            name: "toml",
            language: tree_sitter_toml_updated::language(),
            highlights_query: tree_sitter_toml_updated::HIGHLIGHT_QUERY,
            injections_query: "",
        },
        HighlightLanguage::TypeScript => LanguageSpec {
            name: "typescript",
            language: tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            highlights_query: tree_sitter_typescript::HIGHLIGHTS_QUERY,
            injections_query: "",
        },
        HighlightLanguage::Tsx => LanguageSpec {
            name: "tsx",
            language: tree_sitter_typescript::LANGUAGE_TSX.into(),
            highlights_query: tree_sitter_typescript::HIGHLIGHTS_QUERY,
            injections_query: "",
        },
        HighlightLanguage::Yaml => LanguageSpec {
            name: "yaml",
            language: tree_sitter_yaml::LANGUAGE.into(),
            highlights_query: tree_sitter_yaml::HIGHLIGHTS_QUERY,
            injections_query: "",
        },
    }
}

impl HighlightLanguage {
    fn from_hint(hint: &str) -> Option<Self> {
        let hint = hint
            .trim()
            .trim_start_matches('.')
            .split(|ch: char| ch.is_whitespace() || ch == ',' || ch == ';')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        Some(match hint.as_str() {
            "bash" | "sh" | "shell" | "zsh" | "ksh" => Self::Bash,
            "c" | "h" => Self::C,
            "c#" | "csharp" | "cs" | "csx" => Self::CSharp,
            "c++" | "cpp" | "cc" | "cxx" | "hpp" | "hxx" | "hh" => Self::Cpp,
            "css" | "scss" => Self::Css,
            "go" | "golang" => Self::Go,
            "html" | "htm" | "xhtml" => Self::Html,
            "java" => Self::Java,
            "javascript" | "js" | "mjs" | "cjs" => Self::JavaScript,
            "json" | "jsonc" | "json5" => Self::Json,
            "jsx" => Self::Jsx,
            "lua" => Self::Lua,
            "php" | "phtml" => Self::Php,
            "python" | "py" => Self::Python,
            "ruby" | "rb" => Self::Ruby,
            "rust" | "rs" => Self::Rust,
            "sql" | "psql" | "mysql" => Self::Sql,
            "toml" => Self::Toml,
            "typescript" | "ts" | "mts" | "cts" => Self::TypeScript,
            "tsx" => Self::Tsx,
            "yaml" | "yml" => Self::Yaml,
            _ => return None,
        })
    }
}

#[cfg(test)]
#[path = "highlight_tests.rs"]
mod tests;
