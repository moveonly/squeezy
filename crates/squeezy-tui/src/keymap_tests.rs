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
fn toggle_hover_intent_round_trips_and_defaults_to_ctrl_alt_h() {
    // §12.1.3 Mouse Hover Intent toggle: slug round-trips, is registered in `ALL`
    // (so `/keymap` and the command palette list it and overrides can target it),
    // and defaults to the `Ctrl+Alt+H` debug-style chord.
    assert_eq!(
        Action::from_slug("toggle_hover_intent"),
        Some(Action::ToggleHoverIntent)
    );
    assert!(Action::ALL.contains(&Action::ToggleHoverIntent));
    let resolver = KeymapResolver::from_overrides(&BTreeMap::new());
    assert_eq!(
        resolver.binding(Action::ToggleHoverIntent),
        KeyBinding::new(
            KeyCode::Char('h'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        ),
    );
    assert_eq!(
        resolver.lookup(
            KeyCode::Char('h'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        ),
        Some(Action::ToggleHoverIntent),
    );
    // A Ctrl+Alt (Meta) chord is honestly classified terminal-dependent.
    assert_eq!(
        Action::ToggleHoverIntent.terminal_compat_note(),
        Some("terminal-dependent")
    );
}

#[test]
fn toggle_tool_actions_round_trips_and_defaults_to_ctrl_alt_a() {
    // §12.3.1 Actionable Tool Outputs toggle: slug round-trips, is registered in
    // `ALL` (so `/keymap` and the command palette list it and overrides can target
    // it), and defaults to the `Ctrl+Alt+A` debug-style chord.
    assert_eq!(
        Action::from_slug("toggle_tool_actions"),
        Some(Action::ToggleToolActions)
    );
    assert!(Action::ALL.contains(&Action::ToggleToolActions));
    let resolver = KeymapResolver::from_overrides(&BTreeMap::new());
    assert_eq!(
        resolver.binding(Action::ToggleToolActions),
        KeyBinding::new(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        ),
    );
    assert_eq!(
        resolver.lookup(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        ),
        Some(Action::ToggleToolActions),
    );
    // A Ctrl+Alt (Meta) chord is honestly classified terminal-dependent.
    assert_eq!(
        Action::ToggleToolActions.terminal_compat_note(),
        Some("terminal-dependent")
    );
    // Distinct from the bare `Alt+a` full-transcript copy: the Ctrl modifier
    // disambiguates the two.
    assert_eq!(
        resolver.lookup(KeyCode::Char('a'), KeyModifiers::ALT),
        Some(Action::CopyFullTranscript),
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
fn multi_selection_actions_round_trip_and_default_to_their_chords() {
    // §12.1.6 Multi-Cursor-Like Transcript Selection: both new actions' slugs
    // round-trip, are registered in `ALL` (so `/keymap` lists them and overrides
    // can target them), and default to their respective chords.
    assert_eq!(
        Action::from_slug("add_selection_to_set"),
        Some(Action::AddSelectionToSet)
    );
    assert_eq!(
        Action::from_slug("copy_multi_selection"),
        Some(Action::CopyMultiSelection)
    );
    assert!(Action::ALL.contains(&Action::AddSelectionToSet));
    assert!(Action::ALL.contains(&Action::CopyMultiSelection));

    let resolver = KeymapResolver::from_overrides(&BTreeMap::new());
    // Add-to-set defaults to `Alt+d`.
    assert_eq!(
        resolver.binding(Action::AddSelectionToSet),
        KeyBinding::new(KeyCode::Char('d'), KeyModifiers::ALT),
    );
    assert_eq!(
        resolver.lookup(KeyCode::Char('d'), KeyModifiers::ALT),
        Some(Action::AddSelectionToSet),
    );
    // Combined copy defaults to `Ctrl+Alt+Y`.
    assert_eq!(
        resolver.binding(Action::CopyMultiSelection),
        KeyBinding::new(
            KeyCode::Char('y'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        ),
    );
    assert_eq!(
        resolver.lookup(
            KeyCode::Char('y'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        ),
        Some(Action::CopyMultiSelection),
    );

    // Both are Meta/Alt chords, so both carry the terminal-dependent note.
    assert_eq!(
        Action::AddSelectionToSet.terminal_compat_note(),
        Some("terminal-dependent"),
    );
    assert_eq!(
        Action::CopyMultiSelection.terminal_compat_note(),
        Some("terminal-dependent"),
    );
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
fn bookmark_actions_round_trip_and_default_to_alt_chords() {
    // §12.2.4 Reading Position Bookmarks: both slugs round-trip, are registered in
    // `ALL`, and default to `Alt+;` (drop) / `Alt+q` (list).
    assert_eq!(
        Action::from_slug("drop_bookmark"),
        Some(Action::DropBookmark)
    );
    assert_eq!(
        Action::from_slug("toggle_bookmarks"),
        Some(Action::ToggleBookmarks)
    );
    assert!(Action::ALL.contains(&Action::DropBookmark));
    assert!(Action::ALL.contains(&Action::ToggleBookmarks));

    let resolver = KeymapResolver::from_overrides(&BTreeMap::new());
    assert_eq!(
        resolver.binding(Action::DropBookmark),
        KeyBinding::new(KeyCode::Char(';'), KeyModifiers::ALT),
    );
    assert_eq!(
        resolver.binding(Action::ToggleBookmarks),
        KeyBinding::new(KeyCode::Char('q'), KeyModifiers::ALT),
    );
    assert_eq!(
        resolver.lookup(KeyCode::Char(';'), KeyModifiers::ALT),
        Some(Action::DropBookmark),
    );
    assert_eq!(
        resolver.lookup(KeyCode::Char('q'), KeyModifiers::ALT),
        Some(Action::ToggleBookmarks),
    );
    // Alt (Meta) chords are honestly classified terminal-dependent.
    assert_eq!(
        Action::DropBookmark.terminal_compat_note(),
        Some("terminal-dependent"),
    );
    assert_eq!(
        Action::ToggleBookmarks.terminal_compat_note(),
        Some("terminal-dependent"),
    );
}

#[test]
fn bookmark_defaults_do_not_collide_with_other_actions() {
    // The new `Alt+;` / `Alt+q` defaults must not shadow (or be shadowed by) any
    // existing default binding.
    let resolver = KeymapResolver::from_overrides(&BTreeMap::new());
    for collision in resolver.collisions() {
        assert!(
            !collision.1.contains(&Action::DropBookmark)
                && !collision.1.contains(&Action::ToggleBookmarks),
            "bookmark default collides: {collision:?}",
        );
    }
}

#[test]
fn annotation_actions_round_trip_and_default_to_alt_chords() {
    // §12.2.5 Entry Annotations: both slugs round-trip, are registered in `ALL`,
    // and default to `Alt+/` (annotate) / `Alt+\` (list).
    assert_eq!(
        Action::from_slug("annotate_entry"),
        Some(Action::AnnotateEntry)
    );
    assert_eq!(
        Action::from_slug("toggle_annotations"),
        Some(Action::ToggleAnnotations)
    );
    assert!(Action::ALL.contains(&Action::AnnotateEntry));
    assert!(Action::ALL.contains(&Action::ToggleAnnotations));

    let resolver = KeymapResolver::from_overrides(&BTreeMap::new());
    assert_eq!(
        resolver.binding(Action::AnnotateEntry),
        KeyBinding::new(KeyCode::Char('/'), KeyModifiers::ALT),
    );
    assert_eq!(
        resolver.binding(Action::ToggleAnnotations),
        KeyBinding::new(KeyCode::Char('\\'), KeyModifiers::ALT),
    );
    assert_eq!(
        resolver.lookup(KeyCode::Char('/'), KeyModifiers::ALT),
        Some(Action::AnnotateEntry),
    );
    assert_eq!(
        resolver.lookup(KeyCode::Char('\\'), KeyModifiers::ALT),
        Some(Action::ToggleAnnotations),
    );
    // Alt (Meta) chords are honestly classified terminal-dependent.
    assert_eq!(
        Action::AnnotateEntry.terminal_compat_note(),
        Some("terminal-dependent"),
    );
    assert_eq!(
        Action::ToggleAnnotations.terminal_compat_note(),
        Some("terminal-dependent"),
    );
}

#[test]
fn annotation_defaults_do_not_collide_with_other_actions() {
    // The new `Alt+/` / `Alt+\` defaults must not shadow (or be shadowed by) any
    // existing default binding.
    let resolver = KeymapResolver::from_overrides(&BTreeMap::new());
    for collision in resolver.collisions() {
        assert!(
            !collision.1.contains(&Action::AnnotateEntry)
                && !collision.1.contains(&Action::ToggleAnnotations),
            "annotation default collides: {collision:?}",
        );
    }
}

#[test]
fn changes_since_default_binds_alt_0_and_is_terminal_dependent() {
    // §12.2.7 What Changed Since Here?: slug round-trips, is registered in `ALL`,
    // binds `Alt+0`, and is honestly classified terminal-dependent (an Alt+digit
    // Meta chord).
    assert_eq!(
        Action::from_slug("toggle_changes_since"),
        Some(Action::ToggleChangesSince),
    );
    assert!(Action::ALL.contains(&Action::ToggleChangesSince));
    let resolver = KeymapResolver::from_overrides(&BTreeMap::new());
    assert_eq!(
        resolver.lookup(KeyCode::Char('0'), KeyModifiers::ALT),
        Some(Action::ToggleChangesSince),
    );
    assert_eq!(
        Action::ToggleChangesSince.terminal_compat_note(),
        Some("terminal-dependent"),
    );
}

#[test]
fn changes_since_default_does_not_collide_with_other_actions() {
    // The new `Alt+0` default must not shadow (or be shadowed by) any existing
    // default binding.
    let resolver = KeymapResolver::from_overrides(&BTreeMap::new());
    for collision in resolver.collisions() {
        assert!(
            !collision.1.contains(&Action::ToggleChangesSince),
            "changes-since default collides: {collision:?}",
        );
    }
}

#[test]
fn action_palette_default_binds_alt_enter_and_is_terminal_dependent() {
    // §12.1.2 Contextual Action Palette: slug round-trips, is registered in `ALL`,
    // binds `Alt+Enter`, and is honestly classified terminal-dependent (an
    // Alt+Enter Meta chord).
    assert_eq!(
        Action::from_slug("open_action_palette"),
        Some(Action::OpenActionPalette),
    );
    assert!(Action::ALL.contains(&Action::OpenActionPalette));
    let resolver = KeymapResolver::from_overrides(&BTreeMap::new());
    assert_eq!(
        resolver.lookup(KeyCode::Enter, KeyModifiers::ALT),
        Some(Action::OpenActionPalette),
    );
    assert_eq!(
        Action::OpenActionPalette.terminal_compat_note(),
        Some("terminal-dependent"),
    );
}

#[test]
fn action_palette_default_does_not_collide_with_other_actions() {
    // The new `Alt+Enter` default must not shadow (or be shadowed by) any existing
    // default binding — notably `Ctrl+Enter` (open in detail) and plain `Enter`.
    let resolver = KeymapResolver::from_overrides(&BTreeMap::new());
    for collision in resolver.collisions() {
        assert!(
            !collision.1.contains(&Action::OpenActionPalette),
            "action-palette default collides: {collision:?}",
        );
    }
}

#[test]
fn open_theme_editor_round_trips_and_defaults_to_ctrl_alt_e() {
    // §12.7.2 Theme Editor UI: slug round-trips, is registered in `ALL` (so
    // `/keymap` and the command palette list it and overrides can target it), and
    // defaults to the obscure `Ctrl+Alt+E` chord.
    assert_eq!(
        Action::from_slug("open_theme_editor"),
        Some(Action::OpenThemeEditor),
    );
    assert!(Action::ALL.contains(&Action::OpenThemeEditor));
    let resolver = KeymapResolver::from_overrides(&BTreeMap::new());
    assert_eq!(
        resolver.lookup(
            KeyCode::Char('e'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        ),
        Some(Action::OpenThemeEditor),
    );
    // A Ctrl+Alt (Meta) chord is honestly classified terminal-dependent.
    assert_eq!(
        Action::OpenThemeEditor.terminal_compat_note(),
        Some("terminal-dependent"),
    );
}

#[test]
fn open_theme_editor_default_does_not_collide_with_other_actions() {
    // The `Ctrl+Alt+E` default must not shadow (or be shadowed by) any existing
    // default binding — notably the bare `Alt+e` external-editor handoff verb.
    let resolver = KeymapResolver::from_overrides(&BTreeMap::new());
    for collision in resolver.collisions() {
        assert!(
            !collision.1.contains(&Action::OpenThemeEditor),
            "theme-editor default collides: {collision:?}",
        );
    }
}

#[test]
fn open_workspace_profile_round_trips_and_defaults_to_ctrl_alt_w() {
    // §12.7.4 Per-Workspace UI Profile: slug round-trips, is registered in `ALL`
    // (so `/keymap` and the command palette list it and overrides can target it),
    // and defaults to the obscure `Ctrl+Alt+W` chord.
    assert_eq!(
        Action::from_slug("open_workspace_profile"),
        Some(Action::OpenWorkspaceProfile),
    );
    assert!(Action::ALL.contains(&Action::OpenWorkspaceProfile));
    let resolver = KeymapResolver::from_overrides(&BTreeMap::new());
    assert_eq!(
        resolver.lookup(
            KeyCode::Char('w'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        ),
        Some(Action::OpenWorkspaceProfile),
    );
    // A Ctrl+Alt (Meta) chord is honestly classified terminal-dependent.
    assert_eq!(
        Action::OpenWorkspaceProfile.terminal_compat_note(),
        Some("terminal-dependent"),
    );
}

#[test]
fn open_workspace_profile_default_does_not_collide_with_other_actions() {
    // The `Ctrl+Alt+W` default must not shadow (or be shadowed by) any existing
    // default binding — notably the bare `Alt+w` soft-wrap toggle verb.
    let resolver = KeymapResolver::from_overrides(&BTreeMap::new());
    for collision in resolver.collisions() {
        assert!(
            !collision.1.contains(&Action::OpenWorkspaceProfile),
            "workspace-profile default collides: {collision:?}",
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
