use std::{
    env,
    sync::{
        OnceLock,
        atomic::{AtomicU8, Ordering},
    },
};

use ratatui::style::Color;

pub(crate) const AMBER: Color = Color::Rgb(252, 211, 77);
pub(crate) const GOLD: Color = Color::Rgb(254, 240, 138);
pub(crate) const MODE_PURPLE: Color = Color::Rgb(216, 180, 254);
pub(crate) const SUCCESS_GREEN: Color = Color::Rgb(22, 101, 52);
pub(crate) const MODE_BUILD_GREEN: Color = Color::Rgb(34, 117, 64);
pub(crate) const ERROR_RED: Color = Color::Rgb(248, 113, 113);
pub(crate) const QUIET: Color = Color::DarkGray;
pub(crate) const PROMPT_BG: Color = Color::Rgb(31, 31, 35);
pub(crate) const WORKING_SHIMMER_HIGHLIGHT: Color = Color::Rgb(255, 251, 235);
pub(crate) const DIFF_ADD_FG: Color = Color::Rgb(21, 128, 61);
pub(crate) const DIFF_DEL_FG: Color = Color::Rgb(252, 165, 165);
pub(crate) const DIFF_HUNK_FG: Color = Color::Rgb(254, 240, 138);
pub(crate) const SEPARATOR_BLUE: Color = Color::Rgb(96, 165, 250);

/// Status-line accent fallbacks. Softened toward 85% saturation / full
/// brightness so they sit well on both light and dark terminals, mirroring
/// codex's `soften_status_line_style` choice.
pub(crate) const ACCENT_CYAN: Color = Color::Rgb(95, 217, 217);
pub(crate) const ACCENT_GREEN: Color = Color::Rgb(95, 217, 95);
pub(crate) const ACCENT_MAGENTA: Color = Color::Rgb(217, 95, 217);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaletteTone {
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColorLevel {
    NoColor,
    Ansi16,
    Ansi256,
    TrueColor,
}

/// Runtime palette tone override. Encoded as `0 = unset (follow detection)`,
/// `1 = Dark`, `2 = Light`. Updates from `/theme` flip this atomically and
/// every read of [`palette_tone`] sees the new value on the next draw.
static TONE_OVERRIDE: AtomicU8 = AtomicU8::new(0);

const TONE_OVERRIDE_UNSET: u8 = 0;
const TONE_OVERRIDE_DARK: u8 = 1;
const TONE_OVERRIDE_LIGHT: u8 = 2;

pub(crate) fn palette_tone() -> PaletteTone {
    match TONE_OVERRIDE.load(Ordering::Relaxed) {
        TONE_OVERRIDE_DARK => PaletteTone::Dark,
        TONE_OVERRIDE_LIGHT => PaletteTone::Light,
        _ => detected_palette_tone(),
    }
}

/// Cached terminal-detected tone. Separate from the override so the `system`
/// preference falls back to detection without a redundant env read.
pub(crate) fn detected_palette_tone() -> PaletteTone {
    static TONE: OnceLock<PaletteTone> = OnceLock::new();
    *TONE.get_or_init(detect_palette_tone)
}

/// Pin the runtime palette tone. `Some(tone)` overrides terminal detection;
/// `None` clears the override and falls back to detection. Subsequent renders
/// see the change immediately.
pub(crate) fn set_palette_tone_override(tone: Option<PaletteTone>) {
    let encoded = match tone {
        None => TONE_OVERRIDE_UNSET,
        Some(PaletteTone::Dark) => TONE_OVERRIDE_DARK,
        Some(PaletteTone::Light) => TONE_OVERRIDE_LIGHT,
    };
    TONE_OVERRIDE.store(encoded, Ordering::Relaxed);
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
    if env::var_os("NO_COLOR").is_some() {
        return ColorLevel::NoColor;
    }
    match supports_color::on_cached(supports_color::Stream::Stdout) {
        Some(level) if level.has_16m => ColorLevel::TrueColor,
        Some(level) if level.has_256 => ColorLevel::Ansi256,
        Some(_) => ColorLevel::Ansi16,
        None => ColorLevel::Ansi16,
    }
}

fn detect_palette_tone() -> PaletteTone {
    env::var("COLORFGBG")
        .ok()
        .and_then(|value| value.rsplit(';').next()?.parse::<u8>().ok())
        .map(|bg| {
            if matches!(bg, 0..=6 | 8) {
                PaletteTone::Dark
            } else {
                PaletteTone::Light
            }
        })
        .unwrap_or(PaletteTone::Dark)
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

const ANSI16_COLORS: [(Color, (u8, u8, u8)); 8] = [
    (Color::Black, (0, 0, 0)),
    (Color::Red, (255, 0, 0)),
    (Color::Green, (0, 128, 0)),
    (Color::Yellow, (255, 255, 0)),
    (Color::Blue, (0, 0, 255)),
    (Color::Magenta, (255, 0, 255)),
    (Color::Cyan, (0, 255, 255)),
    (Color::White, (255, 255, 255)),
];

fn perceptual_distance(a: (u8, u8, u8), b: (u8, u8, u8)) -> f32 {
    let dr = a.0 as f32 - b.0 as f32;
    let dg = a.1 as f32 - b.1 as f32;
    let db = a.2 as f32 - b.2 as f32;
    (0.30 * dr * dr) + (0.59 * dg * dg) + (0.11 * db * db)
}
