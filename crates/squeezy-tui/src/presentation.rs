//! Presentation Mode (§12.4.6): a screen-share / demo display mode with spacious
//! cards, simplified chrome, and default hiding of cost / account / provider /
//! full-path metadata so a live screen-share does not leak it. Togglable and
//! persisted, with a one-shot reveal that un-hides the suppressed metadata
//! without leaving the mode.
//!
//! **Display policy, not redaction.** Presentation Mode is a *display* layer on
//! top of the same transcript model — it never mutates the transcript, the
//! cost/account snapshot, or any persisted state. Turning it off (or revealing)
//! brings every hidden value straight back, byte-identical, because nothing was
//! ever removed. The spec's "display hiding is not redaction" holds by
//! construction: this module owns no data, only a pair of booleans and the
//! pure policy methods the renderer reads.
//!
//! **Reuses the density machinery.** "Spacious cards" is not a second spacing
//! engine — Presentation Mode elevates the resolved Adaptive Density
//! ([`crate::density`]) tier to [`crate::density::DensityTier::Expanded`] so the
//! renderer's existing per-tier spacing (the transcript-to-prompt gap, the
//! roomier card threshold) produces the spacious layout. One density table, one
//! spacing policy.
//!
//! **Zero idle cost.** The state is two `bool`s; every method is a pure,
//! allocation-free predicate with no clock, no I/O, and no caching to
//! invalidate. The renderer reads it only while laying out a frame (and the
//! toggle/reveal runs only on a keypress or click), so an idle session that
//! paints nothing calls into this module zero times — it adds no idle redraw and
//! no background work.

use crate::density::ResolvedDensity;

/// The Presentation Mode (§12.4.6) display state: whether the mode is on, and
/// whether the one-shot reveal is currently lifting metadata suppression. Both
/// default to `false`, so a fresh session is byte-identical to one without the
/// feature. `enabled` is persisted at `[tui].presentation`; `revealed` is
/// deliberately session-local (a reveal never survives a restart — the mode
/// re-hides on the next launch so a screen-share starts clean).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct PresentationState {
    /// Whether Presentation Mode is active. When `true` the renderer elevates the
    /// density to expanded and (unless `revealed`) suppresses the metadata detail
    /// line. The single persisted knob.
    enabled: bool,
    /// Whether the one-shot reveal is lifting metadata suppression. Only
    /// meaningful while `enabled`; cleared automatically when the mode turns off
    /// (so re-entering the mode starts hidden again). Never persisted.
    revealed: bool,
}

impl PresentationState {
    /// The bounded value persisted at `[tui].presentation`. Only the on-state is
    /// written (the off-state removes the key), so this is the single wire form;
    /// keep in sync with [`PresentationState::from_persisted`].
    pub(crate) const ENABLED_SLUG: &'static str = "on";

    /// Whether Presentation Mode is active.
    pub(crate) fn is_enabled(self) -> bool {
        self.enabled
    }

    /// Whether the one-shot reveal is currently lifting metadata suppression.
    /// Always `false` while the mode is off (a reveal outside the mode is
    /// meaningless), so callers can read it without first checking `is_enabled`.
    pub(crate) fn is_revealed(self) -> bool {
        self.enabled && self.revealed
    }

    /// Toggle Presentation Mode on/off. Turning it *off* also clears any pending
    /// reveal, so the next time the mode is entered it starts with metadata
    /// hidden again (a screen-share always starts clean). Returns the new
    /// enabled-state so the caller can phrase its status acknowledgement.
    pub(crate) fn toggle(&mut self) -> bool {
        self.enabled = !self.enabled;
        if !self.enabled {
            self.revealed = false;
        }
        self.enabled
    }

    /// Apply the one-shot reveal: lift metadata suppression while staying in the
    /// mode. A no-op when the mode is off (there is nothing to reveal). Returns
    /// `true` when it actually flipped the reveal on (so the caller can phrase a
    /// "revealed" vs. "nothing to reveal" status), `false` otherwise (mode off,
    /// or already revealed).
    pub(crate) fn reveal(&mut self) -> bool {
        if self.enabled && !self.revealed {
            self.revealed = true;
            true
        } else {
            false
        }
    }

    /// Whether the renderer should suppress the metadata detail line
    /// (cost / account / provider / paths) this frame: on while the mode is
    /// active and the reveal is not lifting it. The suppression is display-only —
    /// the underlying snapshot is untouched, so clearing the mode (or revealing)
    /// brings it straight back.
    pub(crate) fn suppresses_metadata(self) -> bool {
        self.enabled && !self.revealed
    }

    /// Elevate a resolved Adaptive Density to the spacious presentation layout
    /// when the mode is active: presentation forces at least the expanded tier so
    /// the renderer's existing per-tier spacing produces the roomy cards the spec
    /// asks for. A no-op when the mode is off, so a normal session keeps exactly
    /// the density it resolved. Reuses the density table rather than introducing a
    /// second spacing engine.
    pub(crate) fn present_density(self, resolved: ResolvedDensity) -> ResolvedDensity {
        if self.enabled {
            resolved.at_least_expanded()
        } else {
            resolved
        }
    }

    /// The active-mode indicator painted on the status line while the mode is on,
    /// or `None` when it is off (nothing is painted, so a normal session's status
    /// line is byte-identical). ASCII-only and short; names whether metadata is
    /// currently hidden or revealed so the user always knows the live policy.
    /// Drives both the painted badge and its click-target width.
    pub(crate) fn indicator(self) -> Option<&'static str> {
        if !self.enabled {
            return None;
        }
        if self.is_revealed() {
            Some("[present: revealed]")
        } else {
            Some("[present]")
        }
    }

    /// Short human readout of the live state for the status acknowledgement line.
    pub(crate) fn describe(self) -> &'static str {
        match (self.enabled, self.revealed) {
            (false, _) => "off",
            (true, false) => "on (metadata hidden)",
            (true, true) => "on (metadata revealed)",
        }
    }

    /// Build a state from the persisted `[tui].presentation` value. Only the
    /// on-slug (`"on"`, case-insensitive) restores an active mode; anything else
    /// (absent, empty, or unrecognised) collapses to the off-default, so a
    /// hand-edited config is forgiving and a missing key keeps the built-in
    /// behaviour. The reveal is never persisted — a restored mode always starts
    /// hidden.
    pub(crate) fn from_persisted(slug: &str) -> PresentationState {
        if slug.trim().eq_ignore_ascii_case(Self::ENABLED_SLUG) {
            PresentationState {
                enabled: true,
                revealed: false,
            }
        } else {
            PresentationState::default()
        }
    }
}

#[cfg(test)]
#[path = "presentation_tests.rs"]
mod tests;
