//! Shared "card" background helpers.
//!
//! Originally these lived inside `plan_card.rs`. Pulled into a dedicated
//! module so tool-call cards can apply the same subtle surface tint, with
//! a single gate against terminals that can't render bg blends cleanly.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

/// Tool cards render flat — the redesign drops the filled surface tint so the
/// transcript reads as one calm background. Kept as a `None`-returning seam so
/// the card helpers and their callers stay unchanged.
pub(crate) fn card_background_style() -> Option<Style> {
    None
}

/// Patch `bg` onto every span in `line`. No-op when `bg` is `None`.
pub(crate) fn apply_background(line: Line<'static>, bg: Option<Style>) -> Line<'static> {
    let Some(bg) = bg else {
        return line;
    };
    let mut line = line;
    let line_has_bg = line.style.bg.is_some();
    if !line_has_bg {
        line.style = line.style.patch(bg);
    }
    if line_has_bg {
        return line;
    }
    for span in &mut line.spans {
        if span.style.bg.is_none() {
            span.style = span.style.patch(bg);
        }
    }
    line
}

/// Blank line styled with the card background, used as top/bottom padding.
pub(crate) fn blank_card_line(bg: Option<Style>) -> Option<Line<'static>> {
    bg.map(|bg| Line::from(vec![Span::styled(String::new(), bg)]))
}

#[cfg(test)]
#[path = "card_tests.rs"]
mod tests;
