use std::env;
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::style::Color;

use super::*;

/// Sibling counter so concurrent tests in this file don't race on the
/// same TOML path inside `std::env::temp_dir()`.
static SUFFIX: AtomicU64 = AtomicU64::new(0);

fn temp_toml(body: &str) -> std::path::PathBuf {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let suffix = SUFFIX.fetch_add(1, Ordering::Relaxed);
    let path = env::temp_dir().join(format!(
        "squeezy-theme-test-{}-{}-{}.toml",
        std::process::id(),
        ts,
        suffix,
    ));
    fs::write(&path, body).expect("write temp theme");
    path
}

#[test]
fn default_resolves_known_concrete_token() {
    let theme = Theme::default();
    assert_eq!(theme.resolve("palette.amber"), Some(palette::AMBER));
    assert_eq!(theme.resolve("diff.added"), Some(palette::DIFF_ADD_FG));
    assert!(theme.len() >= 40, "default theme should ship ~40 tokens");
    assert!(!theme.is_empty());
}

#[test]
fn ref_indirection_walks_to_concrete_color() {
    let theme = Theme::default();
    // palette.accent -> palette.amber (Ref), palette.amber -> Color
    assert_eq!(theme.resolve("palette.accent"), Some(palette::AMBER));
    // syntax.comment -> ui.muted -> Color
    assert_eq!(theme.resolve("syntax.comment"), Some(palette::QUIET));
    // transcript.user -> palette.accent -> palette.amber -> Color (two hops)
    assert_eq!(theme.resolve("transcript.user"), Some(palette::AMBER));
}

#[test]
fn ref_cycle_returns_none() {
    let mut theme = Theme::default();
    theme.insert("cycle.a", TokenValue::Ref("cycle.b".into()));
    theme.insert("cycle.b", TokenValue::Ref("cycle.a".into()));
    assert_eq!(theme.resolve("cycle.a"), None);

    // Missing target also returns None (no panic, no infinite loop).
    theme.insert("dangling", TokenValue::Ref("does.not.exist".into()));
    assert_eq!(theme.resolve("dangling"), None);
}

#[test]
fn toml_overlay_applies_on_top_of_defaults() {
    // Dotted keys at the document root must come before any `[header]`
    // because TOML walks the active table per-line — once `[ui]` is in
    // scope, `palette.red = ...` writes to `ui.palette.red`, not to the
    // top-level `palette.red`. The loader handles either grouping the user
    // chooses; this layout exercises both shapes side-by-side.
    let path = temp_toml(
        r##"
palette.red = "#ff00aa"
palette.accent = "@palette.cyan"

[ui]
background = "#101014"
"##,
    );

    let theme = Theme::load_from_toml(&path).expect("load");
    let _ = fs::remove_file(&path);

    assert_eq!(theme.resolve("ui.background"), Some(Color::Rgb(16, 16, 20)));
    assert_eq!(theme.resolve("palette.red"), Some(Color::Rgb(255, 0, 170)));
    // Overlay rebinds accent to cyan; transcript.user (-> palette.accent)
    // should now resolve through to the new cyan target.
    assert_eq!(theme.resolve("palette.accent"), Some(palette::ACCENT_CYAN));
    assert_eq!(theme.resolve("transcript.user"), Some(palette::ACCENT_CYAN));
    // Untouched defaults survive the overlay.
    assert_eq!(theme.resolve("palette.amber"), Some(palette::AMBER));
}
