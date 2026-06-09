//! Runtime TUI theme registry.
//!
//! All user-facing colors should flow through this module. Builtin themes
//! provide a full token table; user settings overlay RGB triples onto those
//! tokens and the active table is swapped atomically when `/theme` or the
//! config screen changes a theme.

use std::collections::BTreeMap;
use std::sync::{
    OnceLock, RwLock,
    atomic::{AtomicU64, Ordering},
};

use ratatui::style::Color;
use squeezy_core::{
    AppConfig, BUILTIN_TUI_THEME_NAMES, DEFAULT_TUI_THEME_NAME, TUI_THEME_COLOR_TOKENS, TuiRgb,
    is_builtin_tui_theme_name,
};

pub(crate) mod token {
    pub(crate) const PALETTE_ACCENT: &str = "palette.accent";
    pub(crate) const PALETTE_SECONDARY: &str = "palette.secondary";
    pub(crate) const PALETTE_RED: &str = "palette.red";
    pub(crate) const PALETTE_GREEN: &str = "palette.green";
    pub(crate) const PALETTE_YELLOW: &str = "palette.yellow";
    pub(crate) const PALETTE_BLUE: &str = "palette.blue";
    pub(crate) const PALETTE_MAGENTA: &str = "palette.magenta";
    pub(crate) const PALETTE_CYAN: &str = "palette.cyan";
    pub(crate) const UI_BACKGROUND: &str = "ui.background";
    pub(crate) const UI_FOREGROUND: &str = "ui.foreground";
    pub(crate) const UI_BORDER: &str = "ui.border";
    pub(crate) const UI_MUTED: &str = "ui.muted";
    pub(crate) const UI_QUIET: &str = "ui.quiet";
    pub(crate) const UI_FOOTER: &str = "ui.footer";
    pub(crate) const UI_SURFACE: &str = "ui.surface";
    pub(crate) const UI_PROMPT_BG: &str = "ui.prompt_bg";
    pub(crate) const SYNTAX_KEYWORD: &str = "syntax.keyword";
    pub(crate) const SYNTAX_STRING: &str = "syntax.string";
    pub(crate) const SYNTAX_COMMENT: &str = "syntax.comment";
    pub(crate) const SYNTAX_LITERAL: &str = "syntax.literal";
    pub(crate) const SYNTAX_FUNCTION: &str = "syntax.function";
    pub(crate) const SYNTAX_TYPE: &str = "syntax.type";
    pub(crate) const SYNTAX_OPERATOR: &str = "syntax.operator";
    pub(crate) const SYNTAX_VARIABLE: &str = "syntax.variable";
    pub(crate) const STATUS_OK: &str = "status.ok";
    pub(crate) const STATUS_WARN: &str = "status.warn";
    pub(crate) const STATUS_ERR: &str = "status.err";
    pub(crate) const STATUS_INFO: &str = "status.info";
    pub(crate) const TRANSCRIPT_USER: &str = "transcript.user";
    pub(crate) const TRANSCRIPT_ASSISTANT: &str = "transcript.assistant";
    pub(crate) const TRANSCRIPT_TOOL: &str = "transcript.tool";
    pub(crate) const TRANSCRIPT_SYSTEM: &str = "transcript.system";
    pub(crate) const DIFF_ADDED: &str = "diff.added";
    pub(crate) const DIFF_REMOVED: &str = "diff.removed";
    pub(crate) const DIFF_ADDED_BG: &str = "diff.added_bg";
    pub(crate) const DIFF_REMOVED_BG: &str = "diff.removed_bg";
    pub(crate) const DIFF_CONTEXT: &str = "diff.context";
    pub(crate) const DIFF_HUNK: &str = "diff.hunk";
    pub(crate) const EFFECTS_SHIMMER: &str = "effects.shimmer";
    pub(crate) const SEPARATOR_PRIMARY: &str = "separator.primary";
    pub(crate) const INLINE_CODE: &str = "inline.code";
    pub(crate) const INLINE_MODEL: &str = "inline.model";
    pub(crate) const PATH_HINT: &str = "path.hint";
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Theme {
    pub(crate) name: String,
    colors: BTreeMap<String, TuiRgb>,
}

impl Theme {
    pub(crate) fn resolve(&self, token: &str) -> Option<TuiRgb> {
        self.colors.get(token).copied()
    }

    pub(crate) fn color(&self, token: &str) -> Color {
        self.resolve(token)
            .map(|[r, g, b]| Color::Rgb(r, g, b))
            .unwrap_or(Color::Reset)
    }

    pub(crate) fn colors(&self) -> &BTreeMap<String, TuiRgb> {
        &self.colors
    }
}

static ACTIVE_THEME: OnceLock<RwLock<Theme>> = OnceLock::new();
static THEME_GENERATION: AtomicU64 = AtomicU64::new(0);

fn active_theme() -> &'static RwLock<Theme> {
    ACTIVE_THEME.get_or_init(|| RwLock::new(builtin_theme(DEFAULT_TUI_THEME_NAME)))
}

pub(crate) fn theme_generation() -> u64 {
    THEME_GENERATION.load(Ordering::Relaxed)
}

pub(crate) fn bump_theme_generation() {
    THEME_GENERATION.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn set_active_theme(config: &AppConfig) {
    let next = resolve_theme(config, &config.tui.theme);
    if let Ok(mut active) = active_theme().write()
        && *active != next
    {
        *active = next;
        bump_theme_generation();
    }
}

#[cfg(test)]
pub(crate) fn current_theme_name() -> String {
    active_theme()
        .read()
        .map(|theme| theme.name.clone())
        .unwrap_or_else(|_| DEFAULT_TUI_THEME_NAME.to_string())
}

#[cfg(test)]
pub(crate) fn current_theme() -> Theme {
    active_theme()
        .read()
        .map(|theme| theme.clone())
        .unwrap_or_else(|_| builtin_theme(DEFAULT_TUI_THEME_NAME))
}

pub(crate) fn resolve_theme(config: &AppConfig, name: &str) -> Theme {
    let canonical = squeezy_core::normalize_tui_theme_name(name)
        .unwrap_or_else(|| DEFAULT_TUI_THEME_NAME.to_string());
    let base_name = if is_builtin_tui_theme_name(&canonical) {
        canonical.as_str()
    } else {
        DEFAULT_TUI_THEME_NAME
    };
    let mut theme = builtin_theme(base_name);
    theme.name = canonical.clone();
    if let Some(overrides) = config.tui.themes.get(&canonical) {
        for (token, rgb) in &overrides.colors {
            if squeezy_core::is_tui_theme_color_token(token) {
                theme.colors.insert(token.clone(), *rgb);
            }
        }
    }
    theme
}

pub(crate) fn available_theme_names(config: &AppConfig) -> Vec<String> {
    let mut names: Vec<String> = BUILTIN_TUI_THEME_NAMES
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    for name in config.tui.themes.keys() {
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    names
}

pub(crate) fn theme_exists(config: &AppConfig, name: &str) -> bool {
    let Some(name) = squeezy_core::normalize_tui_theme_name(name) else {
        return false;
    };
    is_builtin_tui_theme_name(&name) || config.tui.themes.contains_key(&name)
}

pub(crate) fn color(token: &str) -> Color {
    active_theme()
        .read()
        .map(|theme| theme.color(token))
        .unwrap_or(Color::Reset)
}

pub(crate) fn rgb(token: &str) -> TuiRgb {
    active_theme()
        .read()
        .ok()
        .and_then(|theme| theme.resolve(token))
        .unwrap_or([255, 255, 255])
}

pub(crate) fn token_rows() -> &'static [&'static str] {
    TUI_THEME_COLOR_TOKENS
}

pub(crate) fn token_category(token: &str) -> &str {
    token
        .split_once('.')
        .map(|(head, _)| head)
        .unwrap_or("other")
}

pub(crate) fn accent() -> Color {
    color(token::PALETTE_ACCENT)
}

pub(crate) fn brand_accent() -> Color {
    builtin_theme(DEFAULT_TUI_THEME_NAME).color(token::PALETTE_ACCENT)
}

pub(crate) fn secondary() -> Color {
    color(token::PALETTE_SECONDARY)
}

pub(crate) fn red() -> Color {
    color(token::PALETTE_RED)
}

pub(crate) fn green() -> Color {
    color(token::PALETTE_GREEN)
}

pub(crate) fn blue() -> Color {
    color(token::PALETTE_BLUE)
}

pub(crate) fn magenta() -> Color {
    color(token::PALETTE_MAGENTA)
}

pub(crate) fn cyan() -> Color {
    color(token::PALETTE_CYAN)
}

pub(crate) fn foreground() -> Color {
    color(token::UI_FOREGROUND)
}

pub(crate) fn background() -> Color {
    color(token::UI_BACKGROUND)
}

pub(crate) fn muted() -> Color {
    color(token::UI_MUTED)
}

pub(crate) fn quiet() -> Color {
    color(token::UI_QUIET)
}

pub(crate) fn footer() -> Color {
    color(token::UI_FOOTER)
}

pub(crate) fn shimmer() -> Color {
    color(token::EFFECTS_SHIMMER)
}

pub(crate) fn path_hint() -> Color {
    color(token::PATH_HINT)
}

pub(crate) fn warn() -> Color {
    color(token::STATUS_WARN)
}

pub(crate) fn inline_code() -> Color {
    color(token::INLINE_CODE)
}

pub(crate) fn inline_model() -> Color {
    color(token::INLINE_MODEL)
}

fn builtin_theme(name: &str) -> Theme {
    let entries = match name {
        "bright" => BRIGHT_COLORS,
        "fun" => FUN_COLORS,
        "catppuccin" => CATPPUCCIN_COLORS,
        "high-contrast" => HIGH_CONTRAST_COLORS,
        _ => DEFAULT_COLORS,
    };
    Theme {
        name: name.to_string(),
        colors: entries
            .iter()
            .map(|(token, rgb)| ((*token).to_string(), *rgb))
            .collect(),
    }
}

// "Starlight" — the default night-sky theme. A deep indigo sky, cool
// silver starlight text tiered across a four-step slate ramp, and a
// single warm undertone (a soft, muted honey-gold) reserved for the
// live agent and the brand glyphs so gold reads as the rare star
// rather than chrome — pitched lower in chroma/luminance than a raw
// amber so it glows instead of glaring on the near-black sky. All
// structural and semantic tones share a slate-blue undertone (hue ~228)
// so the interface reads as one moonlight at several brightnesses.
const DEFAULT_COLORS: &[(&str, TuiRgb)] = &[
    (token::PALETTE_ACCENT, [216, 185, 112]),
    (token::PALETTE_SECONDARY, [224, 202, 156]),
    (token::PALETTE_RED, [236, 140, 156]),
    (token::PALETTE_GREEN, [143, 217, 176]),
    (token::PALETTE_YELLOW, [232, 214, 154]),
    (token::PALETTE_BLUE, [137, 180, 250]),
    (token::PALETTE_MAGENTA, [183, 157, 224]),
    (token::PALETTE_CYAN, [132, 206, 220]),
    (token::UI_BACKGROUND, [15, 19, 32]),
    (token::UI_FOREGROUND, [226, 230, 242]),
    (token::UI_BORDER, [60, 66, 88]),
    (token::UI_MUTED, [150, 156, 184]),
    (token::UI_QUIET, [86, 93, 120]),
    (token::UI_FOOTER, [114, 122, 153]),
    (token::UI_SURFACE, [23, 27, 42]),
    (token::UI_PROMPT_BG, [25, 29, 46]),
    (token::SYNTAX_KEYWORD, [183, 157, 224]),
    (token::SYNTAX_STRING, [166, 216, 160]),
    (token::SYNTAX_COMMENT, [111, 119, 150]),
    (token::SYNTAX_LITERAL, [232, 214, 154]),
    (token::SYNTAX_FUNCTION, [138, 198, 245]),
    (token::SYNTAX_TYPE, [132, 206, 220]),
    (token::SYNTAX_OPERATOR, [167, 174, 203]),
    (token::SYNTAX_VARIABLE, [205, 211, 230]),
    (token::STATUS_OK, [143, 217, 176]),
    (token::STATUS_WARN, [242, 217, 160]),
    (token::STATUS_ERR, [236, 140, 156]),
    (token::STATUS_INFO, [137, 180, 250]),
    (token::TRANSCRIPT_USER, [226, 230, 242]),
    (token::TRANSCRIPT_ASSISTANT, [226, 230, 242]),
    (token::TRANSCRIPT_TOOL, [150, 156, 184]),
    (token::TRANSCRIPT_SYSTEM, [183, 157, 224]),
    (token::DIFF_ADDED, [143, 217, 176]),
    (token::DIFF_REMOVED, [235, 169, 180]),
    (token::DIFF_ADDED_BG, [22, 48, 31]),
    (token::DIFF_REMOVED_BG, [52, 30, 39]),
    (token::DIFF_CONTEXT, [86, 93, 120]),
    (token::DIFF_HUNK, [242, 217, 160]),
    (token::EFFECTS_SHIMMER, [228, 200, 142]),
    (token::SEPARATOR_PRIMARY, [90, 100, 134]),
    (token::INLINE_CODE, [132, 206, 220]),
    (token::INLINE_MODEL, [183, 157, 224]),
    (token::PATH_HINT, [126, 134, 196]),
];

const BRIGHT_COLORS: &[(&str, TuiRgb)] = &[
    (token::PALETTE_ACCENT, [255, 214, 85]),
    (token::PALETTE_SECONDARY, [255, 245, 155]),
    (token::PALETTE_RED, [255, 120, 120]),
    (token::PALETTE_GREEN, [80, 210, 120]),
    (token::PALETTE_YELLOW, [255, 230, 105]),
    (token::PALETTE_BLUE, [105, 190, 255]),
    (token::PALETTE_MAGENTA, [190, 150, 255]),
    (token::PALETTE_CYAN, [85, 220, 220]),
    (token::UI_BACKGROUND, [18, 20, 24]),
    (token::UI_FOREGROUND, [245, 248, 255]),
    (token::UI_BORDER, [120, 130, 145]),
    (token::UI_MUTED, [165, 175, 190]),
    (token::UI_QUIET, [115, 125, 140]),
    (token::UI_FOOTER, [135, 145, 160]),
    (token::UI_SURFACE, [32, 36, 44]),
    (token::UI_PROMPT_BG, [32, 36, 44]),
    (token::SYNTAX_KEYWORD, [205, 155, 255]),
    (token::SYNTAX_STRING, [195, 220, 125]),
    (token::SYNTAX_COMMENT, [145, 155, 165]),
    (token::SYNTAX_LITERAL, [245, 180, 105]),
    (token::SYNTAX_FUNCTION, [110, 220, 255]),
    (token::SYNTAX_TYPE, [120, 205, 255]),
    (token::SYNTAX_OPERATOR, [225, 230, 238]),
    (token::SYNTAX_VARIABLE, [235, 238, 245]),
    (token::STATUS_OK, [80, 210, 120]),
    (token::STATUS_WARN, [255, 230, 105]),
    (token::STATUS_ERR, [255, 120, 120]),
    (token::STATUS_INFO, [105, 190, 255]),
    (token::TRANSCRIPT_USER, [255, 214, 85]),
    (token::TRANSCRIPT_ASSISTANT, [245, 248, 255]),
    (token::TRANSCRIPT_TOOL, [165, 175, 190]),
    (token::TRANSCRIPT_SYSTEM, [190, 150, 255]),
    (token::DIFF_ADDED, [70, 190, 105]),
    (token::DIFF_REMOVED, [255, 145, 145]),
    (token::DIFF_ADDED_BG, [14, 82, 47]),
    (token::DIFF_REMOVED_BG, [118, 35, 35]),
    (token::DIFF_CONTEXT, [115, 125, 140]),
    (token::DIFF_HUNK, [255, 230, 105]),
    (token::EFFECTS_SHIMMER, [255, 245, 155]),
    (token::SEPARATOR_PRIMARY, [105, 190, 255]),
    (token::INLINE_CODE, [85, 220, 220]),
    (token::INLINE_MODEL, [220, 150, 220]),
    (token::PATH_HINT, [145, 145, 220]),
];

const FUN_COLORS: &[(&str, TuiRgb)] = &[
    (token::PALETTE_ACCENT, [255, 118, 182]),
    (token::PALETTE_SECONDARY, [255, 201, 102]),
    (token::PALETTE_RED, [255, 92, 122]),
    (token::PALETTE_GREEN, [70, 220, 150]),
    (token::PALETTE_YELLOW, [255, 214, 90]),
    (token::PALETTE_BLUE, [86, 184, 255]),
    (token::PALETTE_MAGENTA, [192, 132, 252]),
    (token::PALETTE_CYAN, [54, 211, 225]),
    (token::UI_BACKGROUND, [20, 18, 30]),
    (token::UI_FOREGROUND, [238, 241, 255]),
    (token::UI_BORDER, [105, 92, 140]),
    (token::UI_MUTED, [150, 145, 175]),
    (token::UI_QUIET, [95, 88, 126]),
    (token::UI_FOOTER, [120, 112, 150]),
    (token::UI_SURFACE, [31, 26, 45]),
    (token::UI_PROMPT_BG, [31, 26, 45]),
    (token::SYNTAX_KEYWORD, [192, 132, 252]),
    (token::SYNTAX_STRING, [118, 220, 142]),
    (token::SYNTAX_COMMENT, [136, 128, 160]),
    (token::SYNTAX_LITERAL, [255, 180, 100]),
    (token::SYNTAX_FUNCTION, [86, 184, 255]),
    (token::SYNTAX_TYPE, [54, 211, 225]),
    (token::SYNTAX_OPERATOR, [230, 225, 245]),
    (token::SYNTAX_VARIABLE, [255, 206, 235]),
    (token::STATUS_OK, [70, 220, 150]),
    (token::STATUS_WARN, [255, 214, 90]),
    (token::STATUS_ERR, [255, 92, 122]),
    (token::STATUS_INFO, [86, 184, 255]),
    (token::TRANSCRIPT_USER, [255, 118, 182]),
    (token::TRANSCRIPT_ASSISTANT, [238, 241, 255]),
    (token::TRANSCRIPT_TOOL, [150, 145, 175]),
    (token::TRANSCRIPT_SYSTEM, [192, 132, 252]),
    (token::DIFF_ADDED, [70, 220, 150]),
    (token::DIFF_REMOVED, [255, 122, 146]),
    (token::DIFF_ADDED_BG, [21, 75, 62]),
    (token::DIFF_REMOVED_BG, [89, 34, 76]),
    (token::DIFF_CONTEXT, [95, 88, 126]),
    (token::DIFF_HUNK, [255, 214, 90]),
    (token::EFFECTS_SHIMMER, [255, 201, 102]),
    (token::SEPARATOR_PRIMARY, [54, 211, 225]),
    (token::INLINE_CODE, [54, 211, 225]),
    (token::INLINE_MODEL, [255, 118, 182]),
    (token::PATH_HINT, [150, 145, 230]),
];

const CATPPUCCIN_COLORS: &[(&str, TuiRgb)] = &[
    (token::PALETTE_ACCENT, [203, 166, 247]),
    (token::PALETTE_SECONDARY, [245, 194, 231]),
    (token::PALETTE_RED, [243, 139, 168]),
    (token::PALETTE_GREEN, [166, 227, 161]),
    (token::PALETTE_YELLOW, [249, 226, 175]),
    (token::PALETTE_BLUE, [137, 180, 250]),
    // Pink (not Mauve) so the subagent rail stays distinct from the plan
    // accent, which is Mauve in this theme.
    (token::PALETTE_MAGENTA, [245, 194, 231]),
    (token::PALETTE_CYAN, [148, 226, 213]),
    (token::UI_BACKGROUND, [30, 30, 46]),
    (token::UI_FOREGROUND, [205, 214, 244]),
    (token::UI_BORDER, [88, 91, 112]),
    (token::UI_MUTED, [147, 153, 178]),
    (token::UI_QUIET, [108, 112, 134]),
    (token::UI_FOOTER, [127, 132, 156]),
    (token::UI_SURFACE, [49, 50, 68]),
    (token::UI_PROMPT_BG, [49, 50, 68]),
    (token::SYNTAX_KEYWORD, [203, 166, 247]),
    (token::SYNTAX_STRING, [166, 227, 161]),
    (token::SYNTAX_COMMENT, [127, 132, 156]),
    (token::SYNTAX_LITERAL, [249, 226, 175]),
    (token::SYNTAX_FUNCTION, [137, 180, 250]),
    (token::SYNTAX_TYPE, [148, 226, 213]),
    (token::SYNTAX_OPERATOR, [205, 214, 244]),
    (token::SYNTAX_VARIABLE, [245, 224, 220]),
    (token::STATUS_OK, [166, 227, 161]),
    (token::STATUS_WARN, [249, 226, 175]),
    (token::STATUS_ERR, [243, 139, 168]),
    (token::STATUS_INFO, [137, 180, 250]),
    (token::TRANSCRIPT_USER, [203, 166, 247]),
    (token::TRANSCRIPT_ASSISTANT, [205, 214, 244]),
    (token::TRANSCRIPT_TOOL, [147, 153, 178]),
    (token::TRANSCRIPT_SYSTEM, [245, 194, 231]),
    (token::DIFF_ADDED, [166, 227, 161]),
    (token::DIFF_REMOVED, [243, 139, 168]),
    (token::DIFF_ADDED_BG, [40, 74, 59]),
    (token::DIFF_REMOVED_BG, [83, 49, 67]),
    (token::DIFF_CONTEXT, [108, 112, 134]),
    (token::DIFF_HUNK, [249, 226, 175]),
    (token::EFFECTS_SHIMMER, [245, 194, 231]),
    (token::SEPARATOR_PRIMARY, [137, 180, 250]),
    (token::INLINE_CODE, [148, 226, 213]),
    (token::INLINE_MODEL, [245, 194, 231]),
    (token::PATH_HINT, [180, 190, 254]),
];

const HIGH_CONTRAST_COLORS: &[(&str, TuiRgb)] = &[
    (token::PALETTE_ACCENT, [255, 255, 0]),
    (token::PALETTE_SECONDARY, [255, 255, 255]),
    (token::PALETTE_RED, [255, 60, 60]),
    (token::PALETTE_GREEN, [0, 190, 80]),
    (token::PALETTE_YELLOW, [255, 255, 0]),
    (token::PALETTE_BLUE, [0, 140, 255]),
    (token::PALETTE_MAGENTA, [190, 80, 255]),
    (token::PALETTE_CYAN, [0, 210, 255]),
    (token::UI_BACKGROUND, [0, 0, 0]),
    (token::UI_FOREGROUND, [255, 255, 255]),
    (token::UI_BORDER, [180, 180, 180]),
    (token::UI_MUTED, [190, 190, 190]),
    (token::UI_QUIET, [150, 150, 150]),
    (token::UI_FOOTER, [180, 180, 180]),
    (token::UI_SURFACE, [18, 18, 18]),
    (token::UI_PROMPT_BG, [18, 18, 18]),
    (token::SYNTAX_KEYWORD, [190, 80, 255]),
    (token::SYNTAX_STRING, [0, 220, 90]),
    (token::SYNTAX_COMMENT, [170, 170, 170]),
    (token::SYNTAX_LITERAL, [255, 255, 0]),
    (token::SYNTAX_FUNCTION, [0, 180, 255]),
    (token::SYNTAX_TYPE, [0, 210, 255]),
    (token::SYNTAX_OPERATOR, [255, 255, 255]),
    (token::SYNTAX_VARIABLE, [255, 255, 255]),
    (token::STATUS_OK, [0, 220, 90]),
    (token::STATUS_WARN, [255, 255, 0]),
    (token::STATUS_ERR, [255, 60, 60]),
    (token::STATUS_INFO, [0, 180, 255]),
    (token::TRANSCRIPT_USER, [255, 255, 0]),
    (token::TRANSCRIPT_ASSISTANT, [255, 255, 255]),
    (token::TRANSCRIPT_TOOL, [190, 190, 190]),
    (token::TRANSCRIPT_SYSTEM, [190, 80, 255]),
    (token::DIFF_ADDED, [0, 190, 80]),
    (token::DIFF_REMOVED, [255, 60, 60]),
    (token::DIFF_ADDED_BG, [0, 80, 0]),
    (token::DIFF_REMOVED_BG, [110, 0, 0]),
    (token::DIFF_CONTEXT, [150, 150, 150]),
    (token::DIFF_HUNK, [255, 255, 0]),
    (token::EFFECTS_SHIMMER, [255, 255, 255]),
    (token::SEPARATOR_PRIMARY, [0, 180, 255]),
    (token::INLINE_CODE, [0, 210, 255]),
    (token::INLINE_MODEL, [190, 80, 255]),
    (token::PATH_HINT, [180, 180, 255]),
];

#[cfg(test)]
#[path = "theme_tests.rs"]
mod tests;
