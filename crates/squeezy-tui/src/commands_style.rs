//! Tiny ANSI emitter used by the `/context` and `/cost` formatters so
//! their output is bold/colored when it lands in the transcript. Each
//! helper resolves a theme token to RGB *now* (at format time) and emits
//! a SGR-wrapped string; the transcript renderer parses the escapes back
//! into ratatui spans via `crate::render::ansi`. Theme colors are frozen
//! at format time — re-running `/context` after a `/theme` switch picks
//! up the new palette.

use ratatui::style::Color;

use crate::render::theme::{self, token};

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";

fn fg(c: Color) -> Option<String> {
    if let Color::Rgb(r, g, b) = c {
        Some(format!("\x1b[38;2;{r};{g};{b}m"))
    } else {
        None
    }
}

fn wrap(prefix: &str, text: &str) -> String {
    if prefix.is_empty() {
        text.to_string()
    } else {
        format!("{prefix}{text}{RESET}")
    }
}

fn paint(token_name: &str, text: &str, bold: bool) -> String {
    let mut prefix = String::new();
    if bold {
        prefix.push_str(BOLD);
    }
    if let Some(esc) = fg(theme::color(token_name)) {
        prefix.push_str(&esc);
    }
    wrap(&prefix, text)
}

pub(crate) fn header(text: &str) -> String {
    paint(token::PALETTE_ACCENT, text, true)
}

pub(crate) fn accent(text: &str) -> String {
    paint(token::PALETTE_ACCENT, text, false)
}

pub(crate) fn accent_bold(text: &str) -> String {
    paint(token::PALETTE_ACCENT, text, true)
}

pub(crate) fn secondary(text: &str) -> String {
    paint(token::PALETTE_SECONDARY, text, false)
}

pub(crate) fn muted(text: &str) -> String {
    paint(token::UI_QUIET, text, false)
}

pub(crate) fn ok(text: &str) -> String {
    paint(token::STATUS_OK, text, false)
}

pub(crate) fn warn(text: &str) -> String {
    paint(token::STATUS_WARN, text, false)
}

pub(crate) fn err(text: &str) -> String {
    paint(token::STATUS_ERR, text, false)
}

/// Color the headroom percentage by severity. Returns a string that
/// already includes its own SGR reset, ready to inline into a larger
/// formatted line.
pub(crate) fn headroom(percent: f64, text: &str) -> String {
    if percent < 10.0 {
        err(text)
    } else if percent < 30.0 {
        warn(text)
    } else {
        ok(text)
    }
}

/// Format an integer with grouped thousands using `,` — matches the look
/// of the Claude Code reference output the user pointed to.
pub(crate) fn group_thousands(value: u64) -> String {
    let s = value.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_thousands_handles_typical_sizes() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(1_000), "1,000");
        assert_eq!(group_thousands(22_341), "22,341");
        assert_eq!(group_thousands(1_000_000), "1,000,000");
    }

    #[test]
    fn helpers_emit_reset_when_color_resolved() {
        // In test context the active theme is the default, so palette
        // tokens resolve to Color::Rgb and we should see SGR + reset.
        let out = header("X");
        assert!(out.contains("\x1b["), "expected SGR prefix: {out:?}");
        assert!(out.ends_with(RESET), "expected SGR reset: {out:?}");
    }

    #[test]
    fn headroom_picks_status_bands() {
        // Just confirm each band returns a non-empty string with a reset.
        for pct in [5.0, 20.0, 80.0] {
            let s = headroom(pct, "label");
            assert!(s.ends_with(RESET), "pct {pct}: {s:?}");
        }
    }
}
