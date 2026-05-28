use std::sync::Arc;

use super::*;
use crate::render::palette::ColorLevel;

/// `for_tone(Dark)` and `for_tone(Light)` must emit visibly different
/// foregrounds — otherwise a `/theme` flip is a no-op for code blocks.
/// Compared on raw RGB triples so the assertion is independent of the
/// terminal color level the host runner reports.
#[test]
fn dark_and_light_palettes_differ() {
    assert_ne!(KEYWORD_DARK, KEYWORD_LIGHT, "keyword RGB must change");
    assert_ne!(STRING_DARK, STRING_LIGHT, "string RGB must change");
    assert_ne!(FUNCTION_DARK, FUNCTION_LIGHT, "function RGB must change");
    assert_ne!(COMMENT_DARK, COMMENT_LIGHT, "comment RGB must change");
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

/// Two consecutive lookups for the same language must return the same
/// `Arc<HighlightConfiguration>`. Without the per-language cache the TUI
/// pays a multi-millisecond `HighlightConfiguration::new` +
/// `configure(&HIGHLIGHT_NAMES)` rebuild on every render — this is the
/// regression guard for F09.
#[test]
fn highlight_config_is_arc_ptr_eq_on_repeat() {
    let first = highlight_config(HighlightLanguage::Rust)
        .expect("rust highlight config must build from the bundled grammar");
    let second = highlight_config(HighlightLanguage::Rust)
        .expect("rust highlight config must build from the bundled grammar");
    assert!(
        Arc::ptr_eq(&first, &second),
        "consecutive lookups for HighlightLanguage::Rust must return the cached Arc, \
         not a freshly rebuilt HighlightConfiguration",
    );
}

/// Different languages must produce distinct cached configs. Without
/// this isolation a fence tagged ```python` would reuse the Rust grammar
/// (or vice versa) and emit nonsense highlights.
#[test]
fn highlight_config_isolates_languages() {
    let rust = highlight_config(HighlightLanguage::Rust).expect("rust highlight config must build");
    let python =
        highlight_config(HighlightLanguage::Python).expect("python highlight config must build");
    assert!(
        !Arc::ptr_eq(&rust, &python),
        "Rust and Python must have distinct cached HighlightConfiguration entries",
    );
}
