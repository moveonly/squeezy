//! Token-based theme schema with `@token.name` var-indirection.
//!
//! A `Theme` is a flat `BTreeMap` keyed by dotted token name (`ui.background`,
//! `syntax.keyword`, `palette.accent`, …). Each value is either a concrete
//! [`ratatui::style::Color`] or a string `Ref` pointing at another token,
//! which lets palettes compose without duplicating RGB values.
//!
//! The default theme ships ~40 named tokens whose colors come straight from
//! the existing 3-tone palette in `super::palette`, so themed renderers
//! migrated onto this schema produce byte-identical output until a
//! follow-up commit actually wires it into the render path.
//!
//! `load_from_toml` overlays a user file (typically `~/.squeezy/theme.toml`)
//! on top of the default tokens. The expected document shape is flat
//! dotted-key TOML — either grouped via headers or written as compound keys.
//! Compound keys must appear before any section header so they bind to the
//! document root rather than the active table:
//!
//! ```toml
//! palette.accent = "@palette.cyan"
//!
//! [ui]
//! background = "#101014"
//! ```
//!
//! Values that begin with `@` are stored as `Ref`s and resolve through
//! [`Theme::resolve`] with cycle detection. Values starting with `#` are
//! parsed as `#rrggbb` hex. Other strings are ignored so user files can
//! carry comments or future metadata fields without parse errors.

// Scaffolding-only module: nothing in the crate consumes these items yet.
// The follow-up commit that wires the schema into the render path removes
// this allow. Keep this allow narrow to this module so the rest of the
// `render` tree still benefits from dead-code linting.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::Path;

use ratatui::style::Color;
use toml_edit::{DocumentMut, Item, Value};

use super::palette;

/// One entry in a [`Theme`]: either a concrete color or a textual reference
/// to another token name (`palette.accent` -> `Ref("palette.amber")`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenValue {
    Color(Color),
    Ref(String),
}

/// A flat, name-keyed token table. Built from [`Theme::default`] plus an
/// optional user overlay loaded by [`Theme::load_from_toml`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    tokens: BTreeMap<String, TokenValue>,
}

impl Theme {
    /// Resolve a token name to a concrete color, walking `Ref` chains until
    /// either a `Color` is reached or a cycle / missing target is detected
    /// (in which case the function returns `None`).
    pub fn resolve(&self, key: &str) -> Option<Color> {
        let mut visited = BTreeSet::new();
        let mut current = key;
        loop {
            if !visited.insert(current.to_string()) {
                return None;
            }
            match self.tokens.get(current)? {
                TokenValue::Color(color) => return Some(*color),
                TokenValue::Ref(next) => current = next.as_str(),
            }
        }
    }

    /// Load a theme by overlaying `path` on top of [`Theme::default`].
    /// File I/O errors propagate as-is; TOML parse failures are wrapped as
    /// [`io::ErrorKind::InvalidData`] so callers only have to handle one
    /// error type.
    pub fn load_from_toml(path: &Path) -> io::Result<Self> {
        let src = fs::read_to_string(path)?;
        let doc: DocumentMut = src
            .parse()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;

        let mut theme = Self::default();
        collect_tokens("", doc.as_table(), &mut theme.tokens);
        Ok(theme)
    }

    /// Direct token insertion. Intended for tests and for tools that want
    /// to build a theme programmatically without going through TOML.
    #[cfg(test)]
    pub fn insert(&mut self, key: impl Into<String>, value: TokenValue) {
        self.tokens.insert(key.into(), value);
    }

    /// Token count — useful for sanity checks in tests and tooling.
    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    /// `true` when the theme has no tokens. Provided alongside [`Self::len`]
    /// to satisfy clippy's `len_without_is_empty` lint.
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            tokens: default_tokens(),
        }
    }
}

fn default_tokens() -> BTreeMap<String, TokenValue> {
    use TokenValue::{Color as C, Ref as R};

    // ~40 tokens grouped by surface. Concrete colors come from the existing
    // palette so behavior is preserved when this schema is later wired into
    // the render path. A handful of `Ref`s (e.g. `palette.accent`) seed the
    // var-indirection so downstream surfaces can rebind the accent without
    // re-specifying every token that depends on it.
    let entries: [(&str, TokenValue); 40] = [
        // Palette primitives — the named colors a theme overlay is most
        // likely to retarget.
        ("palette.red", C(palette::ERROR_RED)),
        ("palette.green", C(palette::SUCCESS_GREEN)),
        ("palette.yellow", C(palette::GOLD)),
        ("palette.blue", C(palette::SEPARATOR_BLUE)),
        ("palette.magenta", C(palette::MODE_PURPLE)),
        ("palette.cyan", C(palette::ACCENT_CYAN)),
        ("palette.amber", C(palette::AMBER)),
        ("palette.gold", C(palette::GOLD)),
        ("palette.accent", R("palette.amber".into())),
        ("palette.secondary", R("palette.gold".into())),
        // UI chrome.
        ("ui.background", C(Color::Rgb(24, 24, 28))),
        ("ui.foreground", C(Color::Rgb(220, 220, 220))),
        ("ui.border", C(Color::DarkGray)),
        ("ui.muted", C(palette::QUIET)),
        ("ui.quiet", C(palette::QUIET)),
        ("ui.footer", C(palette::QUIET)),
        ("ui.surface", C(palette::PROMPT_BG)),
        ("ui.prompt_bg", C(palette::PROMPT_BG)),
        // Syntax highlighting.
        ("syntax.keyword", R("palette.magenta".into())),
        ("syntax.string", R("palette.green".into())),
        ("syntax.comment", R("ui.muted".into())),
        ("syntax.literal", R("palette.yellow".into())),
        ("syntax.function", R("palette.blue".into())),
        ("syntax.type", R("palette.cyan".into())),
        ("syntax.operator", R("ui.foreground".into())),
        ("syntax.variable", R("palette.amber".into())),
        // Status callouts.
        ("status.ok", R("palette.green".into())),
        ("status.warn", R("palette.yellow".into())),
        ("status.err", R("palette.red".into())),
        ("status.info", R("palette.blue".into())),
        // Transcript roles.
        ("transcript.user", R("palette.accent".into())),
        ("transcript.assistant", R("ui.foreground".into())),
        ("transcript.tool", R("ui.muted".into())),
        ("transcript.system", R("palette.magenta".into())),
        // Diff surfaces.
        ("diff.added", C(palette::DIFF_ADD_FG)),
        ("diff.removed", C(palette::DIFF_DEL_FG)),
        ("diff.context", R("ui.muted".into())),
        ("diff.hunk", C(palette::DIFF_HUNK_FG)),
        // Misc effects.
        ("shimmer.highlight", C(palette::WORKING_SHIMMER_HIGHLIGHT)),
        ("separator.primary", R("palette.blue".into())),
    ];

    entries
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

/// Walk a `toml_edit::Table`, flattening dotted-key paths back into the
/// token namespace used by [`Theme`] and overwriting matching entries. We
/// only descend into `Item::Table`; inline tables and arrays are ignored so
/// user files can hold scratch keys without confusing the loader.
fn collect_tokens(prefix: &str, table: &toml_edit::Table, out: &mut BTreeMap<String, TokenValue>) {
    for (key, item) in table.iter() {
        let path = if prefix.is_empty() {
            key.to_string()
        } else {
            format!("{prefix}.{key}")
        };
        match item {
            Item::Table(sub) => collect_tokens(&path, sub, out),
            Item::Value(Value::String(s)) => {
                if let Some(value) = parse_token_value(s.value()) {
                    out.insert(path, value);
                }
            }
            _ => {}
        }
    }
}

fn parse_token_value(raw: &str) -> Option<TokenValue> {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix('@') {
        if rest.is_empty() {
            return None;
        }
        return Some(TokenValue::Ref(rest.to_string()));
    }
    parse_hex(trimmed).map(TokenValue::Color)
}

fn parse_hex(raw: &str) -> Option<Color> {
    let body = raw.strip_prefix('#')?;
    if body.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&body[0..2], 16).ok()?;
    let g = u8::from_str_radix(&body[2..4], 16).ok()?;
    let b = u8::from_str_radix(&body[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

#[cfg(test)]
#[path = "theme_tests.rs"]
mod tests;
