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
