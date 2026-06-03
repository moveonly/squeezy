use std::collections::BTreeMap;

use squeezy_core::{AppConfig, TUI_THEME_COLOR_TOKENS, TuiThemeSettings};

use super::*;

#[test]
fn default_theme_resolves_every_token() {
    let cfg = AppConfig::default();
    let theme = resolve_theme(&cfg, "default");

    assert_eq!(theme.name, "default");
    assert_eq!(
        theme.colors().len(),
        TUI_THEME_COLOR_TOKENS.len(),
        "builtin default should define every public color token"
    );
    assert_eq!(theme.resolve(token::PALETTE_ACCENT), Some([242, 199, 92]));
    assert_eq!(theme.resolve(token::DIFF_ADDED), Some([143, 217, 176]));
}

#[test]
fn custom_theme_overlays_default_tokens() {
    let mut cfg = AppConfig::default();
    cfg.tui.themes.insert(
        "solarized".to_string(),
        TuiThemeSettings {
            colors: BTreeMap::from([(token::PALETTE_ACCENT.to_string(), [1, 2, 3])]),
        },
    );

    let theme = resolve_theme(&cfg, "solarized");
    let default = resolve_theme(&cfg, "default");
    assert_eq!(theme.name, "solarized");
    assert_eq!(theme.resolve(token::PALETTE_ACCENT), Some([1, 2, 3]));
    assert_eq!(
        theme.resolve(token::PALETTE_SECONDARY),
        default.resolve(token::PALETTE_SECONDARY)
    );
}

#[test]
fn builtin_theme_overrides_can_be_modified_by_settings() {
    let mut cfg = AppConfig::default();
    cfg.tui.themes.insert(
        "fun".to_string(),
        TuiThemeSettings {
            colors: BTreeMap::from([(token::UI_FOREGROUND.to_string(), [9, 8, 7])]),
        },
    );

    let theme = resolve_theme(&cfg, "fun");
    assert_eq!(theme.resolve(token::UI_FOREGROUND), Some([9, 8, 7]));
    assert_ne!(
        theme.resolve(token::PALETTE_ACCENT),
        resolve_theme(&cfg, "default").resolve(token::PALETTE_ACCENT),
        "fun still keeps its builtin palette for tokens that settings do not override"
    );
}

#[test]
fn setting_active_theme_swaps_snapshot_and_bumps_generation() {
    let mut cfg = AppConfig::default();
    cfg.tui.theme = "bright".to_string();
    let before_name = current_theme_name();
    let before = theme_generation();

    set_active_theme(&cfg);

    assert_eq!(current_theme_name(), "bright");
    if before_name != "bright" {
        assert!(theme_generation() > before);
    }
    assert_eq!(
        current_theme().resolve(token::PALETTE_ACCENT),
        Some([255, 214, 85])
    );
}
