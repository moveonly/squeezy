use std::collections::BTreeMap;

use crossterm::event::{KeyCode, KeyModifiers};

use super::*;

fn binding_for(spec: &str) -> KeyBinding {
    parse_keyspec(spec).unwrap_or_else(|| panic!("expected {spec:?} to parse"))
}

#[test]
fn parses_modifier_combinations() {
    assert_eq!(
        binding_for("Ctrl+T"),
        KeyBinding {
            code: KeyCode::Char('t'),
            modifiers: KeyModifiers::CONTROL,
        },
    );
    assert_eq!(
        binding_for("Alt+K"),
        KeyBinding {
            code: KeyCode::Char('k'),
            modifiers: KeyModifiers::ALT,
        },
    );
    assert_eq!(
        binding_for("Ctrl+Alt+Delete"),
        KeyBinding::new(KeyCode::Delete, KeyModifiers::CONTROL | KeyModifiers::ALT,),
    );
    assert_eq!(
        binding_for("PageUp"),
        KeyBinding::new(KeyCode::PageUp, KeyModifiers::NONE),
    );
    assert_eq!(
        binding_for("F11"),
        KeyBinding::new(KeyCode::F(11), KeyModifiers::NONE),
    );
}

#[test]
fn uppercase_override_specs_match_normalised_key_events() {
    let mut overrides = BTreeMap::new();
    overrides.insert("transcript_overlay".to_string(), "Ctrl+O".to_string());
    let resolver = KeymapResolver::from_overrides(&overrides);

    assert_eq!(
        resolver.lookup(KeyCode::Char('o'), KeyModifiers::CONTROL),
        Some(Action::ToggleTranscriptOverlay),
    );
    assert_eq!(
        resolver.binding(Action::ToggleTranscriptOverlay).display(),
        "Ctrl+O",
    );
}

#[test]
fn rejects_garbage_specs() {
    assert!(parse_keyspec("").is_none());
    assert!(parse_keyspec("Ctrl+").is_none());
    assert!(parse_keyspec("Ctrl+a+b").is_none());
    assert!(parse_keyspec("ab").is_none());
    assert!(parse_keyspec("F99").is_none());
}

#[test]
fn defaults_match_legacy_hardcoded() {
    let resolver = KeymapResolver::from_overrides(&BTreeMap::new());
    assert_eq!(
        resolver.binding(Action::ToggleTranscriptOverlay),
        KeyBinding::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
    );
    assert_eq!(
        resolver.binding(Action::ScrollTranscriptPageUp),
        KeyBinding::new(KeyCode::PageUp, KeyModifiers::NONE),
    );
    assert_eq!(
        resolver.lookup(KeyCode::Char('t'), KeyModifiers::CONTROL),
        Some(Action::ToggleTranscriptOverlay),
    );
    assert_eq!(
        resolver.lookup(KeyCode::PageUp, KeyModifiers::NONE),
        Some(Action::ScrollTranscriptPageUp),
    );
}

#[test]
fn override_replaces_default_lookup() {
    let mut overrides = BTreeMap::new();
    overrides.insert("transcript_overlay".to_string(), "Ctrl+o".to_string());
    let resolver = KeymapResolver::from_overrides(&overrides);
    assert_eq!(
        resolver.lookup(KeyCode::Char('o'), KeyModifiers::CONTROL),
        Some(Action::ToggleTranscriptOverlay),
    );
    // The old default no longer resolves to the same action.
    assert_ne!(
        resolver.lookup(KeyCode::Char('t'), KeyModifiers::CONTROL),
        Some(Action::ToggleTranscriptOverlay),
    );
}

#[test]
fn invalid_entries_surface_in_diagnostics() {
    let mut overrides = BTreeMap::new();
    overrides.insert("not_an_action".to_string(), "Ctrl+x".to_string());
    overrides.insert("page_up".to_string(), "wat".to_string());
    let resolver = KeymapResolver::from_overrides(&overrides);
    assert!(
        resolver
            .unknown_actions
            .iter()
            .any(|(k, _)| k == "not_an_action")
    );
    assert!(
        resolver
            .invalid_bindings
            .iter()
            .any(|(k, _, _)| k == "page_up")
    );
    // The invalid spec keeps the default binding live.
    assert_eq!(
        resolver.binding(Action::ScrollTranscriptPageUp).display(),
        "PageUp",
    );
}

#[test]
fn keymap_report_includes_overrides_and_warnings() {
    let mut overrides = BTreeMap::new();
    overrides.insert("transcript_overlay".to_string(), "Ctrl+o".to_string());
    overrides.insert("nope".to_string(), "Ctrl+q".to_string());
    let resolver = KeymapResolver::from_overrides(&overrides);
    let report = format_keymap_command(&resolver);
    assert!(report.contains("transcript_overlay"));
    assert!(report.contains("Ctrl+O"));
    assert!(report.contains("(override)"));
    assert!(report.contains("Unknown action names"));
    assert!(report.contains("nope"));
}
