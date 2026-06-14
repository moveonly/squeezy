//! Shared rendering primitive for clickable in-TUI buttons.
//!
//! Every clickable region — the queue indicator strip today, overlay
//! items / transcript-card actions tomorrow — paints with the same
//! disclosure glyph (the active [`GlyphTokens`] fold marker, so it tracks
//! glyph mode like the rest of the chrome) and the theme's `palette.blue`
//! accent so users learn the pattern once and the color tracks the theme.

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

use crate::glyph_mode::GlyphTokens;

/// Visual state of a disclosure button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ButtonState {
    /// Fold-collapsed glyph — the thing this button controls is currently hidden.
    Collapsed,
    /// Fold-expanded glyph — the thing this button controls is currently shown.
    Expanded,
}

impl ButtonState {
    pub(crate) fn glyph(self, tokens: GlyphTokens) -> &'static str {
        match self {
            ButtonState::Collapsed => tokens.fold_collapsed,
            ButtonState::Expanded => tokens.fold_expanded,
        }
    }
}

/// Standard button spans: ` ▸ Label ` in the theme's blue accent, bold, with the
/// disclosure glyph sourced from `tokens` so it honors the active glyph mode.
/// Callers append additional `Span`s (hint text, counters) after this prefix.
pub(crate) fn button_spans(
    label: &str,
    state: ButtonState,
    tokens: GlyphTokens,
) -> Vec<Span<'static>> {
    let style = Style::default()
        .fg(crate::render::theme::blue())
        .add_modifier(Modifier::BOLD);
    vec![
        Span::styled(format!(" {} ", state.glyph(tokens)), style),
        Span::styled(label.to_string(), style),
    ]
}
