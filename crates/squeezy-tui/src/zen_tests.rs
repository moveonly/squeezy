//! Unit tests for the Zen Mode (§12.4.5) layout-policy state. Pure,
//! terminal-free coverage of the spec's checklist: the off-by-default latch, the
//! toggle, the chrome-suppression predicate, the persist round-trip (including the
//! clear-on-off contract), the from-persisted restoration over the typed bool /
//! its absence, and the minimal one-line state (label + empty-label edge case).

use super::*;

#[test]
fn default_is_off_and_paints_all_chrome() {
    let zen = ZenMode::default();
    assert!(!zen.is_active(), "fresh zen mode is off");
    assert!(
        !zen.chrome_suppressed(),
        "off zen suppresses no chrome — every element paints as before"
    );
}

#[test]
fn toggle_flips_and_reports_the_new_state() {
    let mut zen = ZenMode::default();
    assert!(zen.toggle(), "first toggle turns zen on and returns true");
    assert!(zen.is_active());
    assert!(zen.chrome_suppressed(), "an active zen suppresses chrome");
    assert!(
        !zen.toggle(),
        "second toggle turns zen off and returns false"
    );
    assert!(!zen.is_active());
    assert!(!zen.chrome_suppressed());
}

#[test]
fn persist_writes_true_when_on_and_clears_when_off() {
    let mut zen = ZenMode::default();
    // Off persists as `None` so the key is cleared (no stale `true` left behind).
    assert_eq!(
        zen.as_persist_bool(),
        None,
        "off zen clears the persisted key"
    );
    zen.toggle();
    assert_eq!(
        zen.as_persist_bool(),
        Some(true),
        "on zen persists `true` so the next session reopens in zen"
    );
}

#[test]
fn from_persisted_restores_over_bool_and_absence() {
    assert!(
        ZenMode::from_persisted(Some(true)).is_active(),
        "a persisted `true` reopens in zen"
    );
    assert!(
        !ZenMode::from_persisted(Some(false)).is_active(),
        "a persisted `false` reopens with chrome on"
    );
    assert!(
        !ZenMode::from_persisted(None).is_active(),
        "an absent / malformed value collapses to the chrome-on default"
    );
}

#[test]
fn persist_round_trips_through_from_persisted() {
    for start_active in [false, true] {
        let mut zen = ZenMode::default();
        if start_active {
            zen.toggle();
        }
        let restored = ZenMode::from_persisted(zen.as_persist_bool());
        assert_eq!(
            restored.is_active(),
            zen.is_active(),
            "persist/restore preserves the latch (active = {start_active})"
        );
    }
}

#[test]
fn minimal_status_names_the_mode_and_the_way_out() {
    let zen = ZenMode::from_persisted(Some(true));
    let line = zen.minimal_status("openai:gpt-test", "Ctrl+Alt+.");
    assert!(line.contains("zen"), "names the mode: {line}");
    assert!(
        line.contains("openai:gpt-test"),
        "threads the session label: {line}"
    );
    assert!(
        line.contains("Ctrl+Alt+.") && line.contains("exit"),
        "names the keyboard way out so the exit is always on screen: {line}"
    );
    assert!(
        !line.contains("click"),
        "zen is keyboard-driven, not a click affordance: {line}"
    );
}

#[test]
fn minimal_status_degrades_without_a_label() {
    let zen = ZenMode::from_persisted(Some(true));
    // An empty / whitespace label drops the separator rather than printing a
    // dangling `zen ·  — …` with a bare middle dot.
    let line = zen.minimal_status("   ", "Ctrl+Alt+.");
    assert!(line.starts_with("zen"), "still names the mode: {line}");
    assert!(
        !line.contains('\u{00b7}'),
        "no dangling separator when there is no label: {line}"
    );
    assert!(
        line.contains("Ctrl+Alt+.") && line.contains("exit"),
        "the way out stays on the line: {line}"
    );
}
