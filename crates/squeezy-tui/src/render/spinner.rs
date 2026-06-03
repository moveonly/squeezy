//! Sky-themed "working" spinners.
//!
//! The live agent is shown as a small star rather than the moon: the
//! moon motif already carries the header band and the prompt coin, so
//! the working indicator stays a distinct, simple sky shape. Three
//! styles are offered and the active one is chosen by `tui.spinner`
//! (config / `/config`). Rendered in cool starlight, never amber, so
//! gold stays reserved for the brand marks.

use std::sync::{OnceLock, RwLock};

use squeezy_core::AppConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum SpinnerStyle {
    /// A single star that swells and fades — a calm twinkle.
    Twinkle,
    /// A multi-point sparkle that rotates its points.
    Scintillate,
    /// A star drifting left-to-right across a short field.
    #[default]
    Drift,
}

impl SpinnerStyle {
    /// Resolve a config slug to a style, falling back to the calm
    /// default for anything unrecognized.
    pub(crate) fn from_name(name: &str) -> Self {
        match squeezy_core::normalize_tui_spinner_name(name).as_deref() {
            Some("scintillate") => Self::Scintillate,
            Some("twinkle") => Self::Twinkle,
            _ => Self::Drift,
        }
    }

    fn frames(self) -> &'static [&'static str] {
        match self {
            Self::Twinkle => &["·", "⋆", "✦", "✧", "✦", "⋆"],
            Self::Scintillate => &["✶", "✷", "✸", "✹", "✺", "✹", "✸", "✷"],
            Self::Drift => &["✦  ", " ✦ ", "  ✦", " ✦ "],
        }
    }

    fn interval_ms(self) -> u64 {
        match self {
            // Calm cadence: the star reads as gently alive rather than
            // spinning fast. Drift sweeps slowest, like a star adrift.
            Self::Twinkle => 300,
            Self::Scintillate => 220,
            Self::Drift => 560,
        }
    }

    /// The glyph for the current animation phase. `elapsed_ms` is the
    /// turn's elapsed time so the spinner shares the turn clock.
    pub(crate) fn frame(self, elapsed_ms: u64) -> &'static str {
        let frames = self.frames();
        let idx = ((elapsed_ms / self.interval_ms()) as usize) % frames.len();
        frames[idx]
    }
}

static ACTIVE_SPINNER: OnceLock<RwLock<SpinnerStyle>> = OnceLock::new();

fn active() -> &'static RwLock<SpinnerStyle> {
    ACTIVE_SPINNER.get_or_init(|| RwLock::new(SpinnerStyle::default()))
}

/// Adopt the spinner named by `config.tui.spinner` as the active style.
/// Called from the same path that applies theme overrides so a `/config`
/// change takes effect on the next draw.
pub(crate) fn set_active_spinner(config: &AppConfig) {
    let next = SpinnerStyle::from_name(&config.tui.spinner);
    if let Ok(mut active) = active().write() {
        *active = next;
    }
}

pub(crate) fn active_style() -> SpinnerStyle {
    active().read().map(|style| *style).unwrap_or_default()
}

#[cfg(test)]
#[path = "spinner_tests.rs"]
mod tests;
