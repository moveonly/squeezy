//! Shared rendering primitive for clickable in-TUI buttons.
//!
//! Every clickable region — the queue indicator strip today, overlay
//! items / transcript-card actions tomorrow — paints with the same
//! `>` / `v` disclosure glyph and the theme's `palette.blue` accent so
//! users learn the pattern once and the color tracks the active theme.

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

/// Visual state of a disclosure button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ButtonState {
    /// `>` glyph — the thing this button controls is currently hidden.
    Collapsed,
    /// `v` glyph — the thing this button controls is currently shown.
    Expanded,
}

impl ButtonState {
    pub(crate) fn glyph(self) -> &'static str {
        match self {
            ButtonState::Collapsed => ">",
            ButtonState::Expanded => "v",
        }
    }
}

/// Standard button spans: ` > Label ` in the theme's blue accent, bold.
/// Callers append additional `Span`s (hint text, counters) after this prefix.
pub(crate) fn button_spans(label: &str, state: ButtonState) -> Vec<Span<'static>> {
    let style = Style::default()
        .fg(crate::render::theme::blue())
        .add_modifier(Modifier::BOLD);
    vec![
        Span::styled(format!(" {} ", state.glyph()), style),
        Span::styled(label.to_string(), style),
    ]
}
