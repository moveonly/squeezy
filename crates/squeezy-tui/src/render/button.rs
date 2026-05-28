//! Shared rendering primitive for clickable in-TUI buttons.
//!
//! Every clickable region — the queue indicator strip today, overlay
//! items / transcript-card actions tomorrow — paints with the same
//! `>` / `v` disclosure glyph and dark-blue accent so users learn the
//! pattern once.

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

/// Dark-blue indexed color. 33 is bright/azure across most ANSI
/// palettes — punchy without being neon, and accessible on both
/// light and dark themes. Matches the prior ad-hoc styling in
/// `prompt_queue::indicator_line`.
pub(crate) const BUTTON_FG: ratatui::style::Color = ratatui::style::Color::Indexed(33);

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

/// Standard button spans: ` > Label ` in dark blue, bold. Callers
/// append additional `Span`s (hint text, counters) after this prefix.
pub(crate) fn button_spans(label: &str, state: ButtonState) -> Vec<Span<'static>> {
    let style = Style::default().fg(BUTTON_FG).add_modifier(Modifier::BOLD);
    vec![
        Span::styled(format!(" {} ", state.glyph()), style),
        Span::styled(label.to_string(), style),
    ]
}
