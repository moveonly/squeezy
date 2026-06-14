use std::{
    env,
    sync::{
        OnceLock,
        atomic::{AtomicU8, Ordering},
    },
};

use ratatui::style::Color;

use super::theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColorLevel {
    NoColor,
    Ansi16,
    Ansi256,
    TrueColor,
}

/// A persisted terminal-profile colour override, encoded for the atomic: `0`
/// means "no override, autodetect", otherwise the stored byte is the
/// [`ColorLevel`] discriminant + 1. The override is honoured by
/// [`color_level`] *unless* `$NO_COLOR` is set, which always wins. Set once
/// before the first paint via [`set_color_override`].
static COLOR_OVERRIDE: AtomicU8 = AtomicU8::new(0);

impl ColorLevel {
    /// Decode an override byte back into a level, or `None` when unset.
    fn from_override_byte(byte: u8) -> Option<ColorLevel> {
        match byte {
            1 => Some(ColorLevel::NoColor),
            2 => Some(ColorLevel::Ansi16),
            3 => Some(ColorLevel::Ansi256),
            4 => Some(ColorLevel::TrueColor),
            _ => None,
        }
    }

    /// Encode a level into the non-zero override byte.
    fn to_override_byte(self) -> u8 {
        match self {
            ColorLevel::NoColor => 1,
            ColorLevel::Ansi16 => 2,
            ColorLevel::Ansi256 => 3,
            ColorLevel::TrueColor => 4,
        }
    }
}

/// Pin a colour-depth override resolved from the persisted terminal profile
/// (§12.7.3). Takes effect from the next [`color_level`] resolution; because
/// `color_level` caches its first result, callers set this before the first
/// paint. `$NO_COLOR` still wins over any override (env is the highest-precedence
/// signal). Idempotent and cheap — one relaxed atomic store.
pub(crate) fn set_color_override(level: ColorLevel) {
    COLOR_OVERRIDE.store(level.to_override_byte(), Ordering::Relaxed);
}

pub(crate) fn palette_generation() -> u64 {
    theme::theme_generation()
}

/// Primary accent resolved from the active named theme.
pub(crate) fn accent_primary() -> Color {
    theme::accent()
}

/// Working-shimmer highlight. Default keeps the warm beige cue; themed
/// variants shift to a hue that pops against their own primary accent.
pub(crate) fn accent_working_highlight() -> Color {
    theme::shimmer()
}

pub(crate) fn color_level() -> ColorLevel {
    static LEVEL: OnceLock<ColorLevel> = OnceLock::new();
    *LEVEL.get_or_init(detect_color_level)
}

pub(crate) fn best_color(target: (u8, u8, u8)) -> Color {
    best_color_for_detected_level(target, color_level())
}

#[cfg(test)]
pub(crate) fn best_color_for_level(target: (u8, u8, u8), level: ColorLevel) -> Color {
    best_color_for_detected_level(target, level)
}

fn best_color_for_detected_level(target: (u8, u8, u8), level: ColorLevel) -> Color {
    match level {
        ColorLevel::NoColor => Color::Reset,
        ColorLevel::TrueColor => Color::Rgb(target.0, target.1, target.2),
        ColorLevel::Ansi256 => quantize_rgb_to_ansi256(target),
        ColorLevel::Ansi16 => quantize_rgb_to_ansi16(target),
    }
}

pub(crate) fn blend_color(base: Color, highlight: Color, intensity: f32) -> Color {
    let (base_r, base_g, base_b) = rgb_components(base);
    let (hi_r, hi_g, hi_b) = rgb_components(highlight);
    let t = intensity.clamp(0.0, 1.0);
    Color::Rgb(
        blend_channel(base_r, hi_r, t),
        blend_channel(base_g, hi_g, t),
        blend_channel(base_b, hi_b, t),
    )
}

fn blend_channel(base: u8, highlight: u8, intensity: f32) -> u8 {
    (base as f32 + (highlight as f32 - base as f32) * intensity).round() as u8
}

pub(crate) fn rgb_components(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Black => (0, 0, 0),
        Color::Red => (255, 0, 0),
        Color::Green => (0, 128, 0),
        Color::Yellow => (255, 255, 0),
        Color::Blue => (0, 0, 255),
        Color::Magenta => (255, 0, 255),
        Color::Cyan => (0, 255, 255),
        Color::Gray => (128, 128, 128),
        Color::DarkGray => (80, 80, 80),
        Color::LightRed => (255, 128, 128),
        Color::LightGreen => (128, 255, 128),
        Color::LightYellow => (255, 255, 128),
        Color::LightBlue => (128, 128, 255),
        Color::LightMagenta => (255, 128, 255),
        Color::LightCyan => (128, 255, 255),
        Color::White => (255, 255, 255),
        _ => (255, 255, 255),
    }
}

fn detect_color_level() -> ColorLevel {
    // `$NO_COLOR` is the highest-precedence signal and wins over any saved
    // terminal-profile colour override (env beats persisted policy).
    if env::var_os("NO_COLOR").is_some() {
        return ColorLevel::NoColor;
    }
    // A persisted terminal-profile colour depth (§12.7.3) takes precedence over
    // the probabilistic autodetect below.
    if let Some(level) = ColorLevel::from_override_byte(COLOR_OVERRIDE.load(Ordering::Relaxed)) {
        return level;
    }
    match supports_color::on_cached(supports_color::Stream::Stdout) {
        Some(level) if level.has_16m => ColorLevel::TrueColor,
        Some(level) if level.has_256 => ColorLevel::Ansi256,
        Some(_) => ColorLevel::Ansi16,
        None => ColorLevel::Ansi16,
    }
}

fn quantize_rgb_to_ansi256(target: (u8, u8, u8)) -> Color {
    let mut best = (16u8, f32::MAX);
    for index in 16..=255u8 {
        let candidate = ansi256_rgb(index);
        let distance = perceptual_distance(candidate, target);
        if distance < best.1 {
            best = (index, distance);
        }
    }
    Color::Indexed(best.0)
}

fn ansi256_rgb(index: u8) -> (u8, u8, u8) {
    if index >= 232 {
        let level = 8 + (index - 232) * 10;
        return (level, level, level);
    }
    let offset = index - 16;
    let r = offset / 36;
    let g = (offset % 36) / 6;
    let b = offset % 6;
    (
        ansi256_component(r),
        ansi256_component(g),
        ansi256_component(b),
    )
}

fn ansi256_component(value: u8) -> u8 {
    if value == 0 { 0 } else { 55 + value * 40 }
}

fn quantize_rgb_to_ansi16(target: (u8, u8, u8)) -> Color {
    ANSI16_COLORS
        .iter()
        .min_by(|(_, a), (_, b)| {
            perceptual_distance(*a, target)
                .partial_cmp(&perceptual_distance(*b, target))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(color, _)| *color)
        .unwrap_or(Color::Reset)
}

const ANSI16_COLORS: [(Color, (u8, u8, u8)); 16] = [
    (Color::Black, (0, 0, 0)),
    (Color::Red, (255, 0, 0)),
    (Color::Green, (0, 128, 0)),
    (Color::Yellow, (255, 255, 0)),
    (Color::Blue, (0, 0, 255)),
    (Color::Magenta, (255, 0, 255)),
    (Color::Cyan, (0, 255, 255)),
    (Color::White, (255, 255, 255)),
    (Color::DarkGray, (80, 80, 80)),
    (Color::Gray, (128, 128, 128)),
    (Color::LightRed, (255, 128, 128)),
    (Color::LightGreen, (128, 255, 128)),
    (Color::LightYellow, (255, 255, 128)),
    (Color::LightBlue, (128, 128, 255)),
    (Color::LightMagenta, (255, 128, 255)),
    (Color::LightCyan, (128, 255, 255)),
];

fn perceptual_distance(a: (u8, u8, u8), b: (u8, u8, u8)) -> f32 {
    let dr = a.0 as f32 - b.0 as f32;
    let dg = a.1 as f32 - b.1 as f32;
    let db = a.2 as f32 - b.2 as f32;
    (0.30 * dr * dr) + (0.59 * dg * dg) + (0.11 * db * db)
}

/// Tone-aware muted foreground for tool-call body lines and other secondary
/// text. Drops below `QUIET` (which is a flat `DarkGray`) on TrueColor
/// terminals; falls back to `QUIET` when the terminal can't render arbitrary
/// RGB cleanly.
pub(crate) fn muted_fg() -> Color {
    theme::muted()
}

/// Even dimmer than [`muted_fg`] — for operational chrome (compaction
/// notices, turn-complete markers) that should fade into the periphery.
pub(crate) fn footer_fg() -> Color {
    theme::footer()
}

/// Subtle card-surface background — `Some(color)` only when the terminal can
/// render a faint tint without banding. Returns `None` on Ansi16 / NoColor
/// where blended bg cells look uniformly muddy.
#[allow(dead_code)]
pub(crate) fn surface_bg(accent: Color) -> Option<Color> {
    if !matches!(color_level(), ColorLevel::TrueColor) {
        return None;
    }
    let [r, g, b] = theme::rgb(theme::token::UI_SURFACE);
    Some(blend_color(Color::Rgb(r, g, b), accent, 0.08))
}
