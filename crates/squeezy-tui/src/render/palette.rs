use std::{
    env,
    sync::{
        OnceLock,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
};

use ratatui::style::Color;

// Squeezy palette rule (default `AccentVariant`):
//
// - The agent's brand colour is **dark amber `Rgb(215, 147, 52)`**. It
//   carries banner, working-card balls/spinner, transcript chevrons,
//   and every other "this is squeezy talking to you" cue.
// - Questions and other neutral surfaces (plan-mode chrome, config
//   environment hints, request_user_input modals) use **warm taupe
//   `Rgb(145, 132, 113)`** — distinct from the amber brand but still
//   calm; carries no value judgement.
// - Success and failure use **other dark identifying colours** (deep
//   green, deep crimson). Never bright — rule of thumb: luminance
//   `0.299*R + 0.587*G + 0.114*B` ≤ ~160 on TrueColor terminals so a
//   dark-mode user reading the screen at a glance can pick the brand
//   amber out from the cooler semantic markers without being shouted
//   at.
// - **Exception:** the working-shimmer crest (`WORKING_SHIMMER_HIGHLIGHT`)
//   is an animated brand cue that needs perceptible contrast against the
//   AMBER base (lum ~156.5). Because AMBER already sits within ~3.5 points
//   of the cap, any highlight that meaningfully shimmers must peak above
//   it; the crest is permitted to exceed 160 for animation contrast only,
//   and only at the band centre of a moving sweep (per-character peak is
//   transient, not a steady-state surface colour).
// - Catppuccin / HighContrast variants override these defaults via the
//   `accent_*` accessors; the constants below are the baseline every
//   un-themed install renders against.
pub(crate) const AMBER: Color = Color::Rgb(215, 147, 52);
pub(crate) const GOLD: Color = Color::Rgb(184, 124, 38);
pub(crate) const MODE_PURPLE: Color = Color::Rgb(145, 132, 113);
pub(crate) const SUCCESS_GREEN: Color = Color::Rgb(22, 101, 52);
pub(crate) const MODE_BUILD_GREEN: Color = Color::Rgb(34, 117, 64);
pub(crate) const ERROR_RED: Color = Color::Rgb(180, 60, 60);
pub(crate) const BANG_RED: Color = Color::Rgb(153, 27, 27);
pub(crate) const QUIET: Color = Color::DarkGray;
pub(crate) const PROMPT_BG: Color = Color::Rgb(31, 31, 35);
pub(crate) const WORKING_SHIMMER_HIGHLIGHT: Color = Color::Rgb(220, 190, 130);
pub(crate) const DIFF_ADD_FG: Color = Color::Rgb(21, 128, 61);
pub(crate) const DIFF_DEL_FG: Color = Color::Rgb(185, 88, 88);
pub(crate) const DIFF_HUNK_FG: Color = Color::Rgb(184, 124, 38);
pub(crate) const SEPARATOR_BLUE: Color = Color::Rgb(96, 165, 250);

/// Status-line accent fallbacks. Mid-tone so they stay readable on both
/// dark and light terminals without screaming for attention next to the
/// surrounding QUIET-styled separators and hints.
pub(crate) const ACCENT_CYAN: Color = Color::Rgb(64, 158, 158);
pub(crate) const ACCENT_GREEN: Color = Color::Rgb(64, 158, 64);
pub(crate) const ACCENT_MAGENTA: Color = Color::Rgb(158, 64, 158);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PaletteTone {
    Dark,
    Light,
}

/// Active accent family. `Default` keeps the amber/gold identity (matches
/// the previous single-palette behaviour). `Catppuccin` swaps the primary
/// accent for mauve and the secondary for soft lavender, intended to ride
/// the `Dark` tone. `HighContrast` swaps to a white/bright-yellow pair on
/// the `Light` tone so accessibility-strict configs keep WCAG-grade
/// foreground contrast against the prompt and status surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AccentVariant {
    Default,
    Catppuccin,
    HighContrast,
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

/// Runtime accent family override. `0 = Default (amber/gold)`,
/// `1 = Catppuccin`, `2 = HighContrast`. The default value matches the
/// pre-themed behaviour so unconfigured installs render exactly as before.
static ACCENT_OVERRIDE: AtomicU8 = AtomicU8::new(0);

const ACCENT_DEFAULT: u8 = 0;
const ACCENT_CATPPUCCIN: u8 = 1;
const ACCENT_HIGH_CONTRAST: u8 = 2;

/// Monotonic generation counter bumped whenever a runtime palette knob
/// changes (tone override, accent variant). Downstream render caches
/// (notably the per-entry transcript cache in `render::cache`) capture
/// this value in their cache key so a `/theme` switch invalidates every
/// memoised line that was rendered against the prior palette.
///
/// `Ordering::Relaxed` is sufficient: the counter has no causal
/// relationship with other state; readers only need *some* fresh value
/// after each write, not strict cross-thread ordering against unrelated
/// data. The render loop is single-threaded, but theme overrides may be
/// triggered from background tasks during config reloads — the relaxed
/// monotonic semantics handle that case correctly.
static PALETTE_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Read the current palette generation. The value is stable across reads
/// as long as no theme override fires between them; render caches store
/// the value observed at insertion time and recompute when the live
/// value moves past it.
pub(crate) fn palette_generation() -> u64 {
    PALETTE_GENERATION.load(Ordering::Relaxed)
}

fn bump_palette_generation() {
    PALETTE_GENERATION.fetch_add(1, Ordering::Relaxed);
}

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
    let prior = TONE_OVERRIDE.swap(encoded, Ordering::Relaxed);
    // Only bump when the override actually changed, so a noop set (same
    // tone reapplied during config reload) doesn't churn cached lines.
    if prior != encoded {
        bump_palette_generation();
    }
}

/// Read the active accent variant. The default keeps the amber/gold
/// identity matching the existing palette.
pub(crate) fn accent_variant() -> AccentVariant {
    match ACCENT_OVERRIDE.load(Ordering::Relaxed) {
        ACCENT_CATPPUCCIN => AccentVariant::Catppuccin,
        ACCENT_HIGH_CONTRAST => AccentVariant::HighContrast,
        _ => AccentVariant::Default,
    }
}

/// Pin the runtime accent variant. Subsequent renders see the change.
pub(crate) fn set_accent_variant(variant: AccentVariant) {
    let encoded = match variant {
        AccentVariant::Default => ACCENT_DEFAULT,
        AccentVariant::Catppuccin => ACCENT_CATPPUCCIN,
        AccentVariant::HighContrast => ACCENT_HIGH_CONTRAST,
    };
    let prior = ACCENT_OVERRIDE.swap(encoded, Ordering::Relaxed);
    if prior != encoded {
        bump_palette_generation();
    }
}

/// Primary accent — the AMBER-equivalent. Surfaces that opt into theming
/// (working-shimmer base, prompt activity ring) read this instead of
/// `AMBER` so a `/theme` switch is visible on the most-used cues. The
/// default keeps the AMBER value unchanged.
pub(crate) fn accent_primary() -> Color {
    match accent_variant() {
        AccentVariant::Default => AMBER,
        // Darker mauve in the catppuccin violet family. Luminance
        // 0.299*140 + 0.587*100 + 0.114*200 = 41.86 + 58.70 + 22.80 =
        // 123.36, comfortably under the 160 dark-only cap while still
        // reading as the canonical catppuccin mauve hue.
        AccentVariant::Catppuccin => Color::Rgb(140, 100, 200),
        // Strong-yellow identity preserved for accessibility (R==G with
        // zero blue keeps the WCAG-grade yellow signal) but darkened
        // under the cap. Luminance 0.299*160 + 0.587*130 + 0.114*0 =
        // 47.84 + 76.31 = 124.15.
        AccentVariant::HighContrast => Color::Rgb(160, 130, 0),
    }
}

/// Working-shimmer highlight. Default keeps the warm beige cue; themed
/// variants shift to a hue that pops against their own primary accent.
pub(crate) fn accent_working_highlight() -> Color {
    match accent_variant() {
        AccentVariant::Default => WORKING_SHIMMER_HIGHLIGHT,
        // Darker catppuccin rosewater. Luminance 0.299*150 + 0.587*130
        // + 0.114*125 = 44.85 + 76.31 + 14.25 = 135.41, under the cap
        // and still warm enough to pop against the mauve primary.
        AccentVariant::Catppuccin => Color::Rgb(150, 130, 125),
        // Neutral gray substituting the white peak. Luminance 155
        // (0.299*155 + 0.587*155 + 0.114*155), just under the cap and
        // still bright relative to the dark-gold HighContrast primary.
        AccentVariant::HighContrast => Color::Rgb(155, 155, 155),
    }
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

/// Approximate foreground color for the current terminal tone. Used as the
/// base for [`muted_fg`] / [`footer_fg`] blending — terminals don't expose
/// their actual fg/bg, so we use a representative value per tone.
fn tone_fg_base() -> (u8, u8, u8) {
    match palette_tone() {
        PaletteTone::Dark => (220, 220, 220),
        PaletteTone::Light => (40, 40, 40),
    }
}

/// Approximate background color for the current terminal tone.
fn tone_bg_base() -> (u8, u8, u8) {
    match palette_tone() {
        PaletteTone::Dark => (24, 24, 28),
        PaletteTone::Light => (250, 250, 250),
    }
}

/// Tone-aware muted foreground for tool-call body lines and other secondary
/// text. Drops below `QUIET` (which is a flat `DarkGray`) on TrueColor
/// terminals; falls back to `QUIET` when the terminal can't render arbitrary
/// RGB cleanly.
pub(crate) fn muted_fg() -> Color {
    match color_level() {
        ColorLevel::TrueColor | ColorLevel::Ansi256 => {
            let fg = tone_fg_base();
            let bg = tone_bg_base();
            blend_color(
                Color::Rgb(fg.0, fg.1, fg.2),
                Color::Rgb(bg.0, bg.1, bg.2),
                0.45,
            )
        }
        _ => QUIET,
    }
}

/// Even dimmer than [`muted_fg`] — for operational chrome (compaction
/// notices, turn-complete markers) that should fade into the periphery.
pub(crate) fn footer_fg() -> Color {
    match color_level() {
        ColorLevel::TrueColor | ColorLevel::Ansi256 => {
            let fg = tone_fg_base();
            let bg = tone_bg_base();
            blend_color(
                Color::Rgb(fg.0, fg.1, fg.2),
                Color::Rgb(bg.0, bg.1, bg.2),
                0.65,
            )
        }
        _ => QUIET,
    }
}

/// Subtle card-surface background — `Some(color)` only when the terminal can
/// render a faint tint without banding. Returns `None` on Ansi16 / NoColor
/// where blended bg cells look uniformly muddy.
#[allow(dead_code)]
pub(crate) fn surface_bg(accent: Color) -> Option<Color> {
    if !matches!(color_level(), ColorLevel::TrueColor) {
        return None;
    }
    let bg = tone_bg_base();
    Some(blend_color(Color::Rgb(bg.0, bg.1, bg.2), accent, 0.08))
}
