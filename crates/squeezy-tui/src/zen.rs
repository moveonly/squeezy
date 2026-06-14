//! Zen Mode (§12.4.5): a low-noise layout policy that focuses the surface on the
//! transcript and composer. Secondary chrome — the minimap turn rail, the
//! Clickable Breadcrumbs strip, and the detailed multi-line status block — hides,
//! while the composer, blocking approvals/errors, and every reachable command
//! (search / copy / queue / help) stay live.
//!
//! **Layout policy, not a renderer.** This module owns *no* drawing. It is a tiny
//! piece of durable state (is zen on?) plus the pure predicates the single
//! fullscreen `render()` consults to decide whether each chrome element paints.
//! The transcript and composer paint exactly as they always do; zen only
//! *suppresses* the secondary chrome and condenses the status block to one terse
//! line. That keeps the spec's contract — "Zen is layout policy, not a renderer" —
//! enforceable: every gate is a `bool` read here, never a second paint path.
//!
//! **Togglable + persisted.** [`ZenMode::toggle`] flips the flag in-session; the
//! caller persists it to the user-scope config (`[tui].zen`) so a session the user
//! left in zen reopens in zen. Restoration is a best-effort pure read at startup
//! ([`ZenMode::from_persisted`]); an absent / malformed value collapses to the
//! built-in `off` default, so a session that never toggled zen behaves exactly as
//! before.
//!
//! **Every mouse affordance has a keyboard twin.** Zen is entered/left by the
//! `ToggleZenMode` keymap verb (`Ctrl+Alt+.` default) *and* by a click on the
//! minimal status line that zen paints; both drive the same [`ZenMode::toggle`],
//! so keyboard/mouse parity holds by construction.
//!
//! **Zero idle cost.** The state is one `bool`. Toggling requests a single redraw;
//! an idle session in (or out of) zen pays one `bool` check per painted frame and
//! schedules no background tick, no clock, and no allocation. Suppressing chrome
//! can only ever paint *less*, so zen never adds idle redraw.

/// Durable Zen Mode state (§12.4.5): a single latch held by `TuiApp`.
///
/// `active == false` is the resting default — every chrome element paints as it
/// always has. `active == true` is the distraction-free layout: the secondary
/// chrome predicates below return "suppressed" and the status block condenses to
/// one line. The flag lives here (never recomputed from terminal cells) so the
/// mode survives every redraw, resize, scroll, and stream tick until the user
/// toggles it back.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ZenMode {
    active: bool,
}

/// The user-scope config slug Zen Mode persists under (`[tui].zen`). A single
/// place so the writer ([`ZenMode::as_persist_bool`]) and the reader
/// ([`ZenMode::from_persisted`]) cannot drift.
pub(crate) const PERSIST_KEY: &str = "zen";

impl ZenMode {
    /// Whether zen is currently on. The single predicate the renderer and the
    /// chrome gates below build on.
    #[must_use]
    pub(crate) fn is_active(self) -> bool {
        self.active
    }

    /// Flip the mode and return the new state. The keyboard verb and the
    /// minimal-status-line click both call this, so the two entry points can
    /// never diverge.
    pub(crate) fn toggle(&mut self) -> bool {
        self.active = !self.active;
        self.active
    }

    /// Whether the minimap turn rail / breadcrumbs / dock panel / non-essential
    /// status detail should be suppressed this frame. Today this is exactly
    /// [`is_active`], but routing every chrome gate through one named predicate
    /// keeps "what zen hides" in one place — another distraction-reducing mode can
    /// layer on top without re-deciding the policy at each call site.
    ///
    /// [`is_active`]: ZenMode::is_active
    #[must_use]
    pub(crate) fn chrome_suppressed(self) -> bool {
        self.is_active()
    }

    /// The value to persist for the current state. `Some(true)` is written so a
    /// zen session reopens in zen; `None` means "clear the key" so an off session
    /// leaves no stale `true` behind. Pairing the clear with the default keeps the
    /// persisted file minimal — a never-toggled session writes nothing here.
    #[must_use]
    pub(crate) fn as_persist_bool(self) -> Option<bool> {
        if self.active { Some(true) } else { None }
    }

    /// Rebuild the mode from a persisted bool (or its absence). `Some(true)`
    /// restores zen; `Some(false)` / `None` is the off default. Pure and
    /// total — a malformed value never reaches here (the caller parses the typed
    /// bool), and an absent key maps to the default, so restoration can never
    /// block startup.
    #[must_use]
    pub(crate) fn from_persisted(value: Option<bool>) -> Self {
        Self {
            active: value.unwrap_or(false),
        }
    }

    /// The terse one-line state zen paints where the detailed status block used to
    /// sit. Names the mode and the keyboard way out so the way back is always on
    /// screen — the spec's "minimal one-line state". `label` is the short session
    /// descriptor the caller threads in (e.g. `provider:model`); `exit_hint` is the
    /// resolved keyboard shortcut for the zen toggle (e.g. `Ctrl+Alt+.`), so the
    /// line stays correct under rebinds. When `label` is empty the line degrades to
    /// just the mode + exit hint. Zen is keyboard-driven (and toggleable from
    /// `/config`); the line is deliberately not a click affordance.
    #[must_use]
    pub(crate) fn minimal_status(self, label: &str, exit_hint: &str) -> String {
        let label = label.trim();
        if label.is_empty() {
            format!("zen — {exit_hint} to exit")
        } else {
            format!("zen · {label} — {exit_hint} to exit")
        }
    }
}

#[cfg(test)]
#[path = "zen_tests.rs"]
mod tests;
