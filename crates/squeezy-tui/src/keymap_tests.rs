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
fn queue_undo_action_round_trips_and_defaults_to_u() {
    // Slug round-trips and is registered in `ALL` (so `/keymap` lists it and
    // an override can target it).
    assert_eq!(Action::from_slug("queue_undo"), Some(Action::QueueUndo));
    assert!(Action::ALL.contains(&Action::QueueUndo));
    // Default binding is bare `u`.
    let resolver = KeymapResolver::from_overrides(&BTreeMap::new());
    assert_eq!(
        resolver.binding(Action::QueueUndo),
        KeyBinding::new(KeyCode::Char('u'), KeyModifiers::NONE),
    );
    assert_eq!(
        resolver.lookup(KeyCode::Char('u'), KeyModifiers::NONE),
        Some(Action::QueueUndo),
    );
}

#[test]
fn copy_all_code_action_round_trips_and_defaults_to_alt_j() {
    // §12.5.5 Code-Aware Copy/Export: slug round-trips, is registered in `ALL`
    // (so `/keymap` lists it and an override can target it), and defaults to
    // `Alt+j` — next to the single-block `Alt+k`.
    assert_eq!(
        Action::from_slug("copy_all_code"),
        Some(Action::CopyAllCode)
    );
    assert!(Action::ALL.contains(&Action::CopyAllCode));
    let resolver = KeymapResolver::from_overrides(&BTreeMap::new());
    assert_eq!(
        resolver.binding(Action::CopyAllCode),
        KeyBinding::new(KeyCode::Char('j'), KeyModifiers::ALT),
    );
    assert_eq!(
        resolver.lookup(KeyCode::Char('j'), KeyModifiers::ALT),
        Some(Action::CopyAllCode),
    );
    // `Alt+j` is a Meta/Alt chord — honestly terminal-dependent, like the rest
    // of the semantic-copy family.
    assert_eq!(
        Action::CopyAllCode.terminal_compat_note(),
        Some("terminal-dependent"),
    );
}

#[test]
fn toggle_latency_overlay_round_trips_and_defaults_to_ctrl_alt_l() {
    // §12.10.1 latency overlay toggle: slug round-trips, is registered in `ALL`
    // (so `/keymap` lists it and overrides can target it), and defaults to the
    // obscure debug chord Ctrl+Alt+L.
    assert_eq!(
        Action::from_slug("toggle_latency_overlay"),
        Some(Action::ToggleLatencyOverlay)
    );
    assert!(Action::ALL.contains(&Action::ToggleLatencyOverlay));
    let resolver = KeymapResolver::from_overrides(&BTreeMap::new());
    assert_eq!(
        resolver.binding(Action::ToggleLatencyOverlay),
        KeyBinding::new(
            KeyCode::Char('l'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        ),
    );
    assert_eq!(
        resolver.lookup(
            KeyCode::Char('l'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        ),
        Some(Action::ToggleLatencyOverlay),
    );
    // A Ctrl+Alt (Meta) chord is honestly classified terminal-dependent.
    assert_eq!(
        Action::ToggleLatencyOverlay.terminal_compat_note(),
        Some("terminal-dependent")
    );
}

#[test]
fn toggle_dogfood_metrics_round_trips_and_defaults_to_ctrl_alt_m() {
    // §12.10.3 dogfood `/metrics` overlay toggle: slug round-trips, is
    // registered in `ALL`, and defaults to the obscure debug chord Ctrl+Alt+M.
    assert_eq!(
        Action::from_slug("toggle_dogfood_metrics"),
        Some(Action::ToggleDogfoodMetrics)
    );
    assert!(Action::ALL.contains(&Action::ToggleDogfoodMetrics));
    let resolver = KeymapResolver::from_overrides(&BTreeMap::new());
    assert_eq!(
        resolver.binding(Action::ToggleDogfoodMetrics),
        KeyBinding::new(
            KeyCode::Char('m'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        ),
    );
    assert_eq!(
        resolver.lookup(
            KeyCode::Char('m'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        ),
        Some(Action::ToggleDogfoodMetrics),
    );
    // A Ctrl+Alt (Meta) chord is honestly classified terminal-dependent.
    assert_eq!(
        Action::ToggleDogfoodMetrics.terminal_compat_note(),
        Some("terminal-dependent")
    );
}

#[test]
fn quote_selection_to_compose_round_trips_and_defaults_to_greater_than() {
    // §11.1 quote-to-compose: slug round-trips, is registered in `ALL` (so
    // `/keymap` lists it and overrides can target it), and defaults to bare `>`.
    assert_eq!(
        Action::from_slug("quote_selection_to_compose"),
        Some(Action::QuoteSelectionToCompose)
    );
    assert!(Action::ALL.contains(&Action::QuoteSelectionToCompose));
    let resolver = KeymapResolver::from_overrides(&BTreeMap::new());
    assert_eq!(
        resolver.binding(Action::QuoteSelectionToCompose),
        KeyBinding::new(KeyCode::Char('>'), KeyModifiers::NONE),
    );
    assert_eq!(
        resolver.lookup(KeyCode::Char('>'), KeyModifiers::NONE),
        Some(Action::QuoteSelectionToCompose),
    );
    // A terminal that reports the shift bit alongside the already-shifted `>`
    // glyph must still resolve to the same action (the SHIFT is folded away).
    assert_eq!(
        resolver.lookup(KeyCode::Char('>'), KeyModifiers::SHIFT),
        Some(Action::QuoteSelectionToCompose),
    );
    // A bare printable key is broadly portable — no terminal-dependent note.
    assert_eq!(Action::QuoteSelectionToCompose.terminal_compat_note(), None);
}

#[test]
fn shifted_symbol_binding_folds_away_an_incidental_shift_modifier() {
    // The shift bit on an already-shifted printable symbol is redundant and
    // terminal-dependent, so `KeyBinding::new` normalises it away — `>` with or
    // without SHIFT is the same binding. Alphanumerics keep SHIFT (a Shift+'a'
    // is distinct from 'a').
    assert_eq!(
        KeyBinding::new(KeyCode::Char('>'), KeyModifiers::SHIFT),
        KeyBinding::new(KeyCode::Char('>'), KeyModifiers::NONE),
    );
    assert_eq!(
        KeyBinding::new(KeyCode::Char('?'), KeyModifiers::SHIFT),
        KeyBinding::new(KeyCode::Char('?'), KeyModifiers::NONE),
    );
    // Alphanumerics are NOT folded: a digit keeps the SHIFT the caller gave it
    // (only graphic non-alphanumerics and uppercase letters are folded away).
    assert!(
        KeyBinding::new(KeyCode::Char('5'), KeyModifiers::SHIFT)
            .modifiers
            .contains(KeyModifiers::SHIFT),
        "a digit's SHIFT is preserved, not folded",
    );
}

#[test]
fn jump_mark_actions_round_trip_and_default_to_alt_chords() {
    // §11.2 / 11G.2 jump marks: both slugs round-trip, are registered in `ALL`
    // (so `/keymap` lists them and overrides can target them), and default to
    // the `Alt+m` / `Alt+'` chords.
    assert_eq!(
        Action::from_slug("set_jump_mark"),
        Some(Action::SetJumpMark)
    );
    assert_eq!(Action::from_slug("jump_to_mark"), Some(Action::JumpToMark));
    assert!(Action::ALL.contains(&Action::SetJumpMark));
    assert!(Action::ALL.contains(&Action::JumpToMark));

    let resolver = KeymapResolver::from_overrides(&BTreeMap::new());
    assert_eq!(
        resolver.binding(Action::SetJumpMark),
        KeyBinding::new(KeyCode::Char('m'), KeyModifiers::ALT),
    );
    assert_eq!(
        resolver.binding(Action::JumpToMark),
        KeyBinding::new(KeyCode::Char('\''), KeyModifiers::ALT),
    );
    assert_eq!(
        resolver.lookup(KeyCode::Char('m'), KeyModifiers::ALT),
        Some(Action::SetJumpMark),
    );
    assert_eq!(
        resolver.lookup(KeyCode::Char('\''), KeyModifiers::ALT),
        Some(Action::JumpToMark),
    );
    // Alt (Meta) chords are honestly classified terminal-dependent.
    assert_eq!(
        Action::SetJumpMark.terminal_compat_note(),
        Some("terminal-dependent")
    );
    assert_eq!(
        Action::JumpToMark.terminal_compat_note(),
        Some("terminal-dependent")
    );
}

#[test]
fn jump_mark_defaults_do_not_collide_with_other_actions() {
    // The new `Alt+m` / `Alt+'` defaults must not shadow (or be shadowed by)
    // any existing default binding.
    let resolver = KeymapResolver::from_overrides(&BTreeMap::new());
    for collision in resolver.collisions() {
        assert!(
            !collision.1.contains(&Action::SetJumpMark)
                && !collision.1.contains(&Action::JumpToMark),
            "jump-mark default collides: {:?}",
            collision
        );
    }
}

#[test]
fn wide_block_actions_round_trip_and_default_to_alt_chords() {
    // §11.2 / 11G.4 horizontal navigation: all three slugs round-trip, are
    // registered in `ALL`, and default to `Alt+w` / `Alt+h` / `Alt+l`.
    assert_eq!(
        Action::from_slug("toggle_soft_wrap"),
        Some(Action::ToggleSoftWrap)
    );
    assert_eq!(
        Action::from_slug("scroll_block_left"),
        Some(Action::ScrollBlockLeft)
    );
    assert_eq!(
        Action::from_slug("scroll_block_right"),
        Some(Action::ScrollBlockRight)
    );
    assert!(Action::ALL.contains(&Action::ToggleSoftWrap));
    assert!(Action::ALL.contains(&Action::ScrollBlockLeft));
    assert!(Action::ALL.contains(&Action::ScrollBlockRight));

    let resolver = KeymapResolver::from_overrides(&BTreeMap::new());
    assert_eq!(
        resolver.binding(Action::ToggleSoftWrap),
        KeyBinding::new(KeyCode::Char('w'), KeyModifiers::ALT),
    );
    assert_eq!(
        resolver.lookup(KeyCode::Char('w'), KeyModifiers::ALT),
        Some(Action::ToggleSoftWrap),
    );
    assert_eq!(
        resolver.lookup(KeyCode::Char('h'), KeyModifiers::ALT),
        Some(Action::ScrollBlockLeft),
    );
    assert_eq!(
        resolver.lookup(KeyCode::Char('l'), KeyModifiers::ALT),
        Some(Action::ScrollBlockRight),
    );
    // Alt (Meta) chords are honestly classified terminal-dependent.
    for action in [
        Action::ToggleSoftWrap,
        Action::ScrollBlockLeft,
        Action::ScrollBlockRight,
    ] {
        assert_eq!(action.terminal_compat_note(), Some("terminal-dependent"));
    }
}

#[test]
fn wide_block_defaults_do_not_collide_with_other_actions() {
    let resolver = KeymapResolver::from_overrides(&BTreeMap::new());
    for collision in resolver.collisions() {
        assert!(
            !collision.1.contains(&Action::ToggleSoftWrap)
                && !collision.1.contains(&Action::ScrollBlockLeft)
                && !collision.1.contains(&Action::ScrollBlockRight),
            "wide-block default collides: {:?}",
            collision
        );
    }
}

#[test]
fn hyperlink_toggle_action_is_registered_and_defaults_to_alt_8() {
    // §11G.5: the OSC 8 hyperlink-mode cycle resolves from its slug, is in
    // `ALL`, defaults to `Alt+8`, and is honestly terminal-dependent (Meta).
    assert_eq!(
        Action::from_slug("toggle_hyperlinks"),
        Some(Action::ToggleHyperlinks)
    );
    assert!(Action::ALL.contains(&Action::ToggleHyperlinks));

    let resolver = KeymapResolver::from_overrides(&BTreeMap::new());
    assert_eq!(
        resolver.binding(Action::ToggleHyperlinks),
        KeyBinding::new(KeyCode::Char('8'), KeyModifiers::ALT),
    );
    assert_eq!(
        resolver.lookup(KeyCode::Char('8'), KeyModifiers::ALT),
        Some(Action::ToggleHyperlinks),
    );
    assert_eq!(
        Action::ToggleHyperlinks.terminal_compat_note(),
        Some("terminal-dependent"),
    );
    // The default must not collide with any other action's default.
    for collision in resolver.collisions() {
        assert!(
            !collision.1.contains(&Action::ToggleHyperlinks),
            "hyperlink-toggle default collides: {collision:?}",
        );
    }
}

#[test]
fn pinned_compare_toggle_action_is_registered_and_defaults_to_alt_t() {
    // §12.2.3: the Pinned Compare View toggle resolves from its slug, is in
    // `ALL`, defaults to `Alt+t`, and is honestly terminal-dependent (Meta).
    assert_eq!(
        Action::from_slug("toggle_pinned_compare"),
        Some(Action::TogglePinnedCompare)
    );
    assert!(Action::ALL.contains(&Action::TogglePinnedCompare));

    let resolver = KeymapResolver::from_overrides(&BTreeMap::new());
    assert_eq!(
        resolver.binding(Action::TogglePinnedCompare),
        KeyBinding::new(KeyCode::Char('t'), KeyModifiers::ALT),
    );
    assert_eq!(
        resolver.lookup(KeyCode::Char('t'), KeyModifiers::ALT),
        Some(Action::TogglePinnedCompare),
    );
    assert_eq!(
        Action::TogglePinnedCompare.terminal_compat_note(),
        Some("terminal-dependent"),
    );
    // Distinct from the `Ctrl+T` transcript-overlay toggle — the modifier
    // disambiguates the two.
    assert_ne!(
        resolver.lookup(KeyCode::Char('t'), KeyModifiers::CONTROL),
        Some(Action::TogglePinnedCompare),
    );
    // The default must not collide with any other action's default.
    for collision in resolver.collisions() {
        assert!(
            !collision.1.contains(&Action::TogglePinnedCompare),
            "pinned-compare default collides: {collision:?}",
        );
    }
}

#[test]
fn all_actions_have_unique_slugs() {
    // A duplicate slug would let one action silently shadow another in the
    // `[tui.keymap]` table; guard against it as new verbs land.
    let mut slugs: Vec<&str> = Action::ALL.iter().map(|a| a.slug()).collect();
    slugs.sort_unstable();
    let before = slugs.len();
    slugs.dedup();
    assert_eq!(before, slugs.len(), "duplicate action slug detected");
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
