//! Unit tests for the Presentation Mode (§12.4.6) display state: the
//! toggle/reveal state machine, the metadata-suppression predicate, the density
//! elevation (reusing the §12.4.1 table), the status indicator/describe
//! readouts, and the persistence round-trip.

use super::*;
use crate::density::{DensityMode, DensityTier};

// ---------------------------------------------------------------------------
// Default + toggle state machine.
// ---------------------------------------------------------------------------

#[test]
fn default_is_off_and_inert() {
    let state = PresentationState::default();
    assert!(!state.is_enabled(), "mode is off by default");
    assert!(!state.is_revealed());
    assert!(
        !state.suppresses_metadata(),
        "an off mode never suppresses metadata",
    );
    assert_eq!(
        state.indicator(),
        None,
        "an off mode paints no status badge",
    );
}

#[test]
fn toggle_flips_enabled_and_returns_new_state() {
    let mut state = PresentationState::default();
    assert!(state.toggle(), "first toggle turns the mode on");
    assert!(state.is_enabled());
    assert!(!state.toggle(), "second toggle turns the mode off");
    assert!(!state.is_enabled());
}

#[test]
fn toggling_off_clears_a_pending_reveal() {
    let mut state = PresentationState::default();
    state.toggle();
    assert!(state.reveal(), "reveal lifts suppression while on");
    assert!(state.is_revealed());
    // Turning the mode off must drop the reveal so the next entry starts hidden.
    state.toggle();
    assert!(!state.is_revealed());
    // Re-entering the mode starts with metadata hidden again (a screen-share
    // always opens clean).
    state.toggle();
    assert!(state.is_enabled());
    assert!(!state.is_revealed(), "re-entering the mode starts hidden");
    assert!(state.suppresses_metadata());
}

// ---------------------------------------------------------------------------
// One-shot reveal.
// ---------------------------------------------------------------------------

#[test]
fn reveal_is_a_noop_when_the_mode_is_off() {
    let mut state = PresentationState::default();
    assert!(!state.reveal(), "nothing to reveal while the mode is off");
    assert!(!state.is_revealed());
}

#[test]
fn reveal_lifts_suppression_then_is_idempotent() {
    let mut state = PresentationState::default();
    state.toggle();
    assert!(state.suppresses_metadata(), "hidden by default in the mode");
    assert!(state.reveal(), "first reveal flips suppression off");
    assert!(state.is_revealed());
    assert!(
        !state.suppresses_metadata(),
        "a revealed mode shows metadata",
    );
    assert!(
        !state.reveal(),
        "a second reveal is a no-op (already revealed)",
    );
    assert!(state.is_revealed());
}

#[test]
fn is_revealed_is_false_outside_the_mode_even_if_the_flag_lingers() {
    // `is_revealed` gates on `enabled` so a caller can read it without first
    // checking the mode is on.
    let mut state = PresentationState::default();
    state.toggle();
    state.reveal();
    state.toggle(); // off, which also clears the flag
    assert!(!state.is_revealed());
}

// ---------------------------------------------------------------------------
// Density elevation reuses the §12.4.1 table.
// ---------------------------------------------------------------------------

#[test]
fn present_density_is_identity_when_the_mode_is_off() {
    let state = PresentationState::default();
    // A compact terminal resolves compact; with the mode off it stays compact.
    let resolved = DensityMode::Auto.resolve(50, 12);
    assert_eq!(resolved.tier(), DensityTier::Compact);
    assert_eq!(
        state.present_density(resolved).tier(),
        DensityTier::Compact,
        "an off mode leaves the resolved density untouched",
    );
}

#[test]
fn present_density_forces_expanded_when_the_mode_is_on() {
    let mut state = PresentationState::default();
    state.toggle();
    // Every resolved tier is lifted to expanded for the spacious screen-share
    // layout, regardless of the terminal size or pinned density.
    for (cols, rows) in [(50u16, 12u16), (90, 24), (200, 80)] {
        let resolved = DensityMode::Auto.resolve(cols, rows);
        assert_eq!(
            state.present_density(resolved).tier(),
            DensityTier::Expanded,
            "presentation forces the spacious tier at {cols}x{rows}",
        );
    }
    // A pinned-compact density is still lifted — presentation wins.
    let pinned = DensityMode::Compact.resolve(200, 80);
    let presented = state.present_density(pinned);
    assert_eq!(presented.tier(), DensityTier::Expanded);
    // ...but the originating mode is preserved so a status readout still names
    // what the user picked.
    assert_eq!(presented.mode(), DensityMode::Compact);
}

// ---------------------------------------------------------------------------
// Metadata suppression + status readouts.
// ---------------------------------------------------------------------------

#[test]
fn suppresses_metadata_tracks_enabled_and_reveal() {
    let mut state = PresentationState::default();
    assert!(!state.suppresses_metadata(), "off: no suppression");
    state.toggle();
    assert!(state.suppresses_metadata(), "on + hidden: suppressed");
    state.reveal();
    assert!(
        !state.suppresses_metadata(),
        "on + revealed: not suppressed",
    );
}

#[test]
fn indicator_names_the_live_policy() {
    let mut state = PresentationState::default();
    assert_eq!(state.indicator(), None);
    state.toggle();
    assert_eq!(state.indicator(), Some("[present]"));
    state.reveal();
    assert_eq!(state.indicator(), Some("[present: revealed]"));
}

#[test]
fn describe_reads_every_state() {
    let mut state = PresentationState::default();
    assert_eq!(state.describe(), "off");
    state.toggle();
    assert_eq!(state.describe(), "on (metadata hidden)");
    state.reveal();
    assert_eq!(state.describe(), "on (metadata revealed)");
}

// ---------------------------------------------------------------------------
// Persistence round-trip.
// ---------------------------------------------------------------------------

#[test]
fn from_persisted_restores_only_the_on_slug() {
    // The on-slug restores an active, hidden mode.
    let restored = PresentationState::from_persisted(PresentationState::ENABLED_SLUG);
    assert!(restored.is_enabled());
    assert!(
        !restored.is_revealed(),
        "a restored mode always starts hidden (the reveal is never persisted)",
    );
}

#[test]
fn from_persisted_is_forgiving_and_bounded() {
    // Whitespace + case are tolerated on a hand-edited config.
    assert!(PresentationState::from_persisted("  ON ").is_enabled());
    assert!(PresentationState::from_persisted("On").is_enabled());
    // Anything else collapses to the off-default so a stale / garbage value keeps
    // the built-in behaviour.
    assert!(!PresentationState::from_persisted("off").is_enabled());
    assert!(!PresentationState::from_persisted("true").is_enabled());
    assert!(!PresentationState::from_persisted("").is_enabled());
    assert!(!PresentationState::from_persisted("present").is_enabled());
}
