//! Shared "card" background helpers.
//!
//! Originally these lived inside `plan_card.rs`. Pulled into a dedicated
//! module so tool-call cards can apply the same subtle surface tint, with
//! a single gate against terminals that can't render bg blends cleanly.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::render::palette::{self, ColorLevel};

/// Tone- and color-level-aware background for a card surface. Returns
/// `None` on Ansi16 / NoColor terminals where blended bg cells look
/// muddy — callers then fall back to the flat (unpatched) layout.
pub(crate) fn card_background_style() -> Option<Style> {
    if !matches!(palette::color_level(), ColorLevel::TrueColor) {
        return None;
    }
    let (r, g, b) = match palette::palette_tone() {
        palette::PaletteTone::Dark => (28, 28, 34),
        palette::PaletteTone::Light => (244, 244, 248),
    };
    Some(Style::default().bg(palette::best_color((r, g, b))))
}

/// Patch `bg` onto every span in `line`. No-op when `bg` is `None`.
pub(crate) fn apply_background(line: Line<'static>, bg: Option<Style>) -> Line<'static> {
    let Some(bg) = bg else {
        return line;
    };
    let spans: Vec<Span<'static>> = line
        .spans
        .into_iter()
        .map(|span| {
            let style = span.style.patch(bg);
            Span::styled(span.content, style)
        })
        .collect();
    Line::from(spans)
}

/// Blank line styled with the card background, used as top/bottom padding.
pub(crate) fn blank_card_line(bg: Option<Style>) -> Option<Line<'static>> {
    bg.map(|bg| Line::from(vec![Span::styled(String::new(), bg)]))
}
