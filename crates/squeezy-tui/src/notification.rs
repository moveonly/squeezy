//! Off-tab desktop attention surfaces.
//!
//! Exposes [`DesktopNotifier`] for OSC 9 desktop notifications and BEL
//! audible alerts on turn-complete / approval-pending events. Both are
//! opt-in via the `[tui].desktop_notifications` setting and default off so
//! a fresh install never honks a terminal bell unexpectedly.

use std::{env, io, io::Write};

use squeezy_core::NotificationMethod;

/// Emit OSC 9 / BEL surfaces for off-tab attention events (turn-complete,
/// approval-pending). The on-screen surface is the durable transcript; this
/// fills the gap when the user has tab-switched away from the terminal
/// during a long turn.
///
/// Behaviour is gated on the user-configured [`NotificationMethod`]:
/// - `Off` (default) — no bytes emitted.
/// - `Bel` — emits `\x07` (BEL).
/// - `Osc9` — emits `ESC ] 9 ; <message> BEL` (the iTerm-style desktop
///   notification escape, also honoured by Ghostty / Kitty / WezTerm /
///   Warp).
/// - `Auto` — emits OSC 9 when `$TERM_PROGRAM` matches a known
///   OSC-9-capable terminal; otherwise falls back to BEL.
pub(crate) struct DesktopNotifier {
    method: NotificationMethod,
}

impl DesktopNotifier {
    pub(crate) fn new(method: NotificationMethod) -> Self {
        Self { method }
    }

    /// Effective backend after resolving `Auto` against the running
    /// terminal. Returns `None` when notifications are disabled.
    pub(crate) fn resolved(&self) -> Option<NotificationMethod> {
        match self.method {
            NotificationMethod::Off => None,
            NotificationMethod::Bel => Some(NotificationMethod::Bel),
            NotificationMethod::Osc9 => Some(NotificationMethod::Osc9),
            NotificationMethod::Auto => Some(if terminal_supports_osc9() {
                NotificationMethod::Osc9
            } else {
                NotificationMethod::Bel
            }),
        }
    }

    /// Emit a notification for `message` to stdout. No-op when disabled.
    pub(crate) fn notify(&self, message: &str) -> io::Result<()> {
        let mut stdout = io::stdout().lock();
        let wrote = self.write_to(&mut stdout, message)?;
        if wrote {
            stdout.flush()?;
        }
        Ok(())
    }

    /// Write the configured sequence into `w`. Returns whether any bytes
    /// were emitted (so callers can avoid flushing a no-op).
    pub(crate) fn write_to<W: Write>(&self, w: &mut W, message: &str) -> io::Result<bool> {
        let Some(backend) = self.resolved() else {
            return Ok(false);
        };
        match backend {
            NotificationMethod::Bel => {
                w.write_all(b"\x07")?;
            }
            NotificationMethod::Osc9 => {
                w.write_all(b"\x1b]9;")?;
                w.write_all(sanitized_message(message).as_bytes())?;
                w.write_all(b"\x07")?;
            }
            // `Off` is filtered out by `resolved()`; `Auto` is resolved to
            // a concrete backend above. Both are unreachable here.
            NotificationMethod::Off | NotificationMethod::Auto => return Ok(false),
        }
        Ok(true)
    }
}

/// Strip bytes that would either terminate the OSC string early (BEL,
/// `ESC \\`) or break out into raw escape sequences. Control characters
/// below 0x20 (except newline → space) are dropped so an approval prompt
/// can't accidentally inject color codes into the desktop banner.
fn sanitized_message(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    for ch in message.chars() {
        match ch {
            '\u{07}' | '\u{1b}' => {}
            '\n' | '\t' => out.push(' '),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
    out
}

fn terminal_supports_osc9() -> bool {
    detect_osc9_support_from_env(|key: &str| env::var_os(key))
}

/// Pure capability heuristic for OSC 9 support based on environment-variable
/// signals. Factored out for testability — production calls `env::var_os` in;
/// tests pass a closure backed by a fixture map.
pub(crate) fn detect_osc9_support_from_env<F>(env_get: F) -> bool
where
    F: Fn(&str) -> Option<std::ffi::OsString>,
{
    // TERM_PROGRAM match — macOS/cross-platform emulators that report
    // themselves via this var and are known to honour OSC 9.
    if env_get("TERM_PROGRAM").is_some_and(|prog| {
        matches!(
            prog.to_string_lossy().as_ref(),
            "iTerm.app" | "Ghostty" | "WezTerm" | "kitty" | "WarpTerminal"
        )
    }) {
        return true;
    }
    // Linux-specific environment signals present in capable emulators
    // even when running inside tmux (which overwrites $TERM_PROGRAM).
    if env_get("KITTY_WINDOW_ID").is_some()
        || env_get("WEZTERM_PANE").is_some()
        || env_get("WEZTERM_EXECUTABLE").is_some()
        || env_get("GHOSTTY_RESOURCES_DIR").is_some()
    {
        return true;
    }
    // TERM values that identify OSC-9-capable emulators on Linux.
    if let Some(term) = env_get("TERM") {
        let term = term.to_string_lossy().to_ascii_lowercase();
        if term.contains("kitty")
            || term.contains("ghostty")
            || term.contains("wezterm")
            || term.contains("foot")
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
#[path = "notification_tests.rs"]
mod tests;
