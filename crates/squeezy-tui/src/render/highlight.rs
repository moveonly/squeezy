use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use tree_sitter::Language;
use tree_sitter_highlight::{Highlight, HighlightConfiguration, HighlightEvent, Highlighter};

use crate::render::palette::{self, PaletteTone, best_color};

const MAX_HIGHLIGHT_BYTES: usize = 512 * 1024;
const MAX_HIGHLIGHT_LINES: usize = 10_000;

/// Foreground colors for each highlight category. Built per render from the
/// runtime [`PaletteTone`] so a `/theme` flip is reflected on the next draw
/// without rebuilding any highlighter state. RGB triples flow through
/// [`best_color`] for ANSI-256 / ANSI-16 / `NO_COLOR` adaptation.
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
    /// Picks readable foregrounds for the supplied tone. Dark values match
    /// the original ANSI-256 palette (light purple / cyan / amber on black);
    /// light values are darker, more saturated hues (deep purple / teal /
    /// brown / dark slate) chosen to retain WCAG-AA contrast on white.
    pub(crate) fn for_tone(tone: PaletteTone) -> Self {
        match tone {
            PaletteTone::Dark => Self {
                keyword: best_color(KEYWORD_DARK),
                r#type: best_color(TYPE_DARK),
                string: best_color(STRING_DARK),
                number: best_color(NUMBER_DARK),
                comment: best_color(COMMENT_DARK),
                function: best_color(FUNCTION_DARK),
                variable: best_color(VARIABLE_DARK),
                constant: best_color(CONSTANT_DARK),
                operator: best_color(OPERATOR_DARK),
                punctuation: best_color(PUNCTUATION_DARK),
            },
            PaletteTone::Light => Self {
                keyword: best_color(KEYWORD_LIGHT),
                r#type: best_color(TYPE_LIGHT),
                string: best_color(STRING_LIGHT),
                number: best_color(NUMBER_LIGHT),
                comment: best_color(COMMENT_LIGHT),
                function: best_color(FUNCTION_LIGHT),
                variable: best_color(VARIABLE_LIGHT),
                constant: best_color(CONSTANT_LIGHT),
                operator: best_color(OPERATOR_LIGHT),
                punctuation: best_color(PUNCTUATION_LIGHT),
            },
        }
    }

    /// Snapshot of the palette to use for the *current* draw — reads
    /// [`palette::palette_tone`] at call time.
    pub(crate) fn current() -> Self {
        Self::for_tone(palette::palette_tone())
    }
}

// Dark-tone RGB approximations of the original `Color::Indexed` palette
// (xterm-256 indices 141 / 110 / 143 / 173 / 244 / 81 / 252 / 179 / 250 / 245).
// Listed in absolute terms so the values remain auditable even if the
// ANSI-256 mapping is refreshed.
const KEYWORD_DARK: (u8, u8, u8) = (175, 135, 215);
const TYPE_DARK: (u8, u8, u8) = (135, 175, 215);
const STRING_DARK: (u8, u8, u8) = (175, 175, 135);
const NUMBER_DARK: (u8, u8, u8) = (215, 135, 95);
const COMMENT_DARK: (u8, u8, u8) = (128, 128, 128);
const FUNCTION_DARK: (u8, u8, u8) = (95, 215, 255);
const VARIABLE_DARK: (u8, u8, u8) = (208, 208, 208);
const CONSTANT_DARK: (u8, u8, u8) = (215, 175, 95);
const OPERATOR_DARK: (u8, u8, u8) = (188, 188, 188);
const PUNCTUATION_DARK: (u8, u8, u8) = (138, 138, 138);

// Light-tone foregrounds — darker, higher-saturation analogues that read
// against a white background. Modeled on VS Code's "Light+" theme so the
// pairing keyword/type/string is familiar (deep purple / teal / brown).
const KEYWORD_LIGHT: (u8, u8, u8) = (175, 0, 219);
const TYPE_LIGHT: (u8, u8, u8) = (38, 127, 153);
const STRING_LIGHT: (u8, u8, u8) = (163, 21, 21);
const NUMBER_LIGHT: (u8, u8, u8) = (9, 134, 88);
const COMMENT_LIGHT: (u8, u8, u8) = (106, 117, 122);
const FUNCTION_LIGHT: (u8, u8, u8) = (121, 94, 38);
const VARIABLE_LIGHT: (u8, u8, u8) = (0, 16, 128);
const CONSTANT_LIGHT: (u8, u8, u8) = (170, 55, 49);
const OPERATOR_LIGHT: (u8, u8, u8) = (53, 53, 53);
const PUNCTUATION_LIGHT: (u8, u8, u8) = (82, 82, 82);

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
    render_events(source, events, &HighlightPalette::current())
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

fn language_spec(language_hint: Option<&str>) -> Option<LanguageSpec> {
    let lang = HighlightLanguage::from_hint(language_hint?)?;
    Some(match lang {
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
    })
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
mod tests {
    use super::*;
    use crate::render::palette::ColorLevel;

    /// `for_tone(Dark)` and `for_tone(Light)` must emit visibly different
    /// foregrounds — otherwise a `/theme` flip is a no-op for code blocks.
    /// Asserted on more than two kinds so a single shared constant across
    /// tones is caught.
    #[test]
    fn dark_and_light_palettes_differ() {
        let dark = HighlightPalette::for_tone(PaletteTone::Dark);
        let light = HighlightPalette::for_tone(PaletteTone::Light);
        assert_ne!(dark.keyword, light.keyword, "keyword color must change");
        assert_ne!(dark.string, light.string, "string color must change");
        assert_ne!(dark.function, light.function, "function color must change");
        assert_ne!(dark.comment, light.comment, "comment color must change");
    }

    /// `for_tone` must surface the same `best_color`-quantised value the
    /// renderer uses, so a /theme flip and any test reading the struct see
    /// identical Colors.
    #[test]
    fn palette_constants_round_trip_through_best_color() {
        let dark = HighlightPalette::for_tone(PaletteTone::Dark);
        assert_eq!(dark.keyword, best_color(KEYWORD_DARK));
        assert_eq!(dark.number, best_color(NUMBER_DARK));

        let light = HighlightPalette::for_tone(PaletteTone::Light);
        assert_eq!(light.keyword, best_color(KEYWORD_LIGHT));
        assert_eq!(light.number, best_color(NUMBER_LIGHT));
    }

    /// On truecolor terminals each channel of the configured RGB must
    /// survive intact, so dark/light keyword/string/function values render
    /// as distinct hues regardless of how `color_level()` was probed.
    #[test]
    fn truecolor_pre_quantisation_differs_per_tone() {
        let to_rgb =
            |triple: (u8, u8, u8)| palette::best_color_for_level(triple, ColorLevel::TrueColor);
        assert_ne!(to_rgb(KEYWORD_DARK), to_rgb(KEYWORD_LIGHT));
        assert_ne!(to_rgb(STRING_DARK), to_rgb(STRING_LIGHT));
        assert_ne!(to_rgb(FUNCTION_DARK), to_rgb(FUNCTION_LIGHT));
    }

    /// The light-tone keyword foreground must have lower luminance than
    /// the dark-tone one so it stays readable on white backgrounds.
    #[test]
    fn light_tone_keyword_is_darker_than_dark_tone() {
        let dark_avg = avg(KEYWORD_DARK);
        let light_avg = avg(KEYWORD_LIGHT);
        assert!(
            light_avg < dark_avg,
            "light keyword must have lower luminance \
             (light={light_avg}, dark={dark_avg}) so it reads on white"
        );
    }

    fn avg((r, g, b): (u8, u8, u8)) -> u32 {
        (r as u32 + g as u32 + b as u32) / 3
    }
}
