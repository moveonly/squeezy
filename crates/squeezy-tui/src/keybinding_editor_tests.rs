//! Unit tests for the Keybinding Editor UI state machine (§12.7.1). Pure,
//! terminal-free coverage of the spec's checklist: registry-sourced rows, cursor
//! movement, capture/commit/cancel, conflict detection, reserved-key guardrails,
//! reset-to-default, and the in-overlay row update that keeps a later conflict
//! scan honest. The integration tests in `lib_tests.rs` drive these through the
//! real `render()` + key/mouse dispatch.

use super::*;

/// Build editor rows from the registry at their compiled-in defaults — the same
/// shape `keybinding_editor_rows` builds from a fresh resolver.
fn default_rows() -> Vec<EditorRow> {
    Action::ALL
        .iter()
        .copied()
        .map(|action| EditorRow {
            action,
            binding: action.default_binding(),
            is_override: false,
        })
        .collect()
}

fn editor() -> KeybindingEditorState {
    KeybindingEditorState::new(default_rows())
}

#[test]
fn rows_cover_every_registered_action_in_order() {
    let state = editor();
    assert_eq!(state.rows().len(), Action::ALL.len());
    assert!(!state.is_empty());
    for (row, action) in state.rows().iter().zip(Action::ALL.iter().copied()) {
        assert_eq!(row.action, action);
        assert_eq!(row.binding, action.default_binding());
        assert!(!row.is_override);
    }
}

#[test]
fn opens_in_browse_mode_at_top() {
    let state = editor();
    assert!(!state.is_capturing());
    assert_eq!(state.selected_index(), 0);
    assert!(state.pending().is_none());
}

#[test]
fn cursor_moves_and_clamps_at_bounds() {
    let mut state = editor();
    let last = state.rows().len() - 1;

    // Up at the top is a no-op (no wrap).
    state.select_up();
    assert_eq!(state.selected_index(), 0);

    state.select_down();
    assert_eq!(state.selected_index(), 1);

    state.select_last();
    assert_eq!(state.selected_index(), last);

    // Down at the bottom is a no-op (no wrap).
    state.select_down();
    assert_eq!(state.selected_index(), last);

    state.select_first();
    assert_eq!(state.selected_index(), 0);

    // Page movement is clamped at both ends.
    state.page_down(10_000);
    assert_eq!(state.selected_index(), last);
    state.page_up(10_000);
    assert_eq!(state.selected_index(), 0);

    // A page is at least one row even when asked for zero.
    state.page_down(0);
    assert_eq!(state.selected_index(), 1);
}

#[test]
fn select_index_clamps_and_reports_change() {
    let mut state = editor();
    let last = state.rows().len() - 1;

    assert!(state.select_index(3));
    assert_eq!(state.selected_index(), 3);

    // Re-selecting the same row reports no change.
    assert!(!state.select_index(3));

    // Out-of-range clamps to the last row.
    assert!(state.select_index(usize::MAX));
    assert_eq!(state.selected_index(), last);
}

#[test]
fn capture_commit_rebinds_the_selected_row_and_returns_the_binding() {
    let mut state = editor();
    // Pick a row whose default is NOT the chord we will capture.
    let index = Action::ALL
        .iter()
        .position(|a| *a == Action::ToggleMinimap)
        .expect("minimap action present");
    state.select_index(index);

    assert!(state.begin_capture());
    assert!(state.is_capturing());

    // A free chord (unbound function key) captures cleanly.
    let chord = KeyBinding::new(KeyCode::F(9), KeyModifiers::NONE);
    let outcome = state.capture(chord).expect("capture in capture mode");
    assert_eq!(outcome, CaptureOutcome::Free);
    assert_eq!(state.pending().map(|p| p.binding), Some(chord));

    let (action, binding) = state.commit().expect("committable free chord");
    assert_eq!(action, Action::ToggleMinimap);
    assert_eq!(binding, chord);

    // The in-overlay row reflects the change immediately, flagged as an override.
    let row = state.selected_row().expect("row");
    assert_eq!(row.binding, chord);
    assert!(row.is_override);
    // And the editor is back in browse mode with no pending chord.
    assert!(!state.is_capturing());
    assert!(state.pending().is_none());
}

#[test]
fn capture_outside_capture_mode_is_ignored() {
    let mut state = editor();
    let chord = KeyBinding::new(KeyCode::F(9), KeyModifiers::NONE);
    assert!(state.capture(chord).is_none());
    assert!(state.commit().is_none());
}

#[test]
fn cancel_capture_drops_the_pending_chord_without_rebinding() {
    let mut state = editor();
    let before = state.selected_row().expect("row").binding;
    assert!(state.begin_capture());
    state.capture(KeyBinding::new(KeyCode::F(9), KeyModifiers::NONE));
    assert!(state.pending().is_some());

    assert!(state.cancel_capture());
    assert!(!state.is_capturing());
    assert!(state.pending().is_none());
    // The binding is unchanged.
    assert_eq!(state.selected_row().expect("row").binding, before);

    // Cancelling again (already in browse) reports no-op.
    assert!(!state.cancel_capture());
}

#[test]
fn conflicting_chord_warns_but_still_commits_shadowing_the_other_action() {
    let mut state = editor();
    // Capture, for the minimap row, the chord that ToggleTranscriptOverlay uses
    // by default (Ctrl+T) — a genuine conflict.
    let other = Action::ToggleTranscriptOverlay.default_binding();
    let index = Action::ALL
        .iter()
        .position(|a| *a == Action::ToggleMinimap)
        .expect("minimap present");
    state.select_index(index);
    state.begin_capture();

    let outcome = state.capture(other).expect("capture");
    assert_eq!(
        outcome,
        CaptureOutcome::Conflict {
            with: Action::ToggleTranscriptOverlay.slug(),
        }
    );
    assert!(outcome.is_committable());

    // A conflict is still committable (the user accepts the shadow).
    let (action, binding) = state.commit().expect("conflict commits");
    assert_eq!(action, Action::ToggleMinimap);
    assert_eq!(binding, other);
}

#[test]
fn reserved_recovery_keys_cannot_be_captured() {
    let mut state = editor();
    state.begin_capture();

    for (binding, label) in [
        (
            KeyBinding::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            "Ctrl+C",
        ),
        (KeyBinding::new(KeyCode::Esc, KeyModifiers::NONE), "Esc"),
        (
            KeyBinding::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
            "Ctrl+D",
        ),
    ] {
        let outcome = state.capture(binding).expect("capture");
        assert_eq!(outcome, CaptureOutcome::Reserved { label });
        assert!(!outcome.is_committable());
        // Commit refuses a reserved chord and stays in capture mode awaiting a
        // different key (the pending reserved chord is retained for the warning).
        assert!(state.commit().is_none());
        assert!(state.is_capturing());
        assert!(state.pending().is_some());
    }
}

#[test]
fn reserved_label_is_case_insensitive_on_letters() {
    // A lowercase Ctrl+c capture is caught the same as Ctrl+C.
    assert_eq!(
        reserved_label(&KeyBinding::new(KeyCode::Char('C'), KeyModifiers::CONTROL)),
        Some("Ctrl+C"),
    );
    // A free chord is not reserved.
    assert_eq!(
        reserved_label(&KeyBinding::new(KeyCode::F(7), KeyModifiers::NONE)),
        None,
    );
}

#[test]
fn capture_outcome_treats_the_editing_rows_own_binding_as_free() {
    let rows = default_rows();
    // Re-capturing an action's own current binding is not a self-conflict.
    let own = Action::ToggleMinimap.default_binding();
    assert_eq!(
        capture_outcome(&rows, Action::ToggleMinimap, own),
        CaptureOutcome::Free,
    );
}

#[test]
fn conflict_scan_sees_an_already_committed_rebind() {
    let mut state = editor();
    // First, rebind minimap onto a free chord (F9).
    let minimap = Action::ALL
        .iter()
        .position(|a| *a == Action::ToggleMinimap)
        .expect("minimap present");
    state.select_index(minimap);
    state.begin_capture();
    let chord = KeyBinding::new(KeyCode::F(9), KeyModifiers::NONE);
    state.capture(chord);
    state.commit().expect("first rebind");

    // Now selecting a different row and capturing the SAME F9 chord must report a
    // conflict with the just-rebound minimap — proving the scan reads the live,
    // in-overlay binding, not the stale default.
    let other = Action::ALL
        .iter()
        .position(|a| *a == Action::ToggleErrorLens)
        .expect("error lens present");
    state.select_index(other);
    state.begin_capture();
    let outcome = state.capture(chord).expect("capture");
    assert_eq!(
        outcome,
        CaptureOutcome::Conflict {
            with: Action::ToggleMinimap.slug(),
        }
    );
}

#[test]
fn reset_selected_reverts_to_default_and_reports_the_change() {
    let mut state = editor();
    let index = Action::ALL
        .iter()
        .position(|a| *a == Action::ToggleMinimap)
        .expect("minimap present");
    state.select_index(index);

    // Rebind, then reset.
    state.begin_capture();
    let chord = KeyBinding::new(KeyCode::F(9), KeyModifiers::NONE);
    state.capture(chord);
    state.commit().expect("rebind");
    assert!(state.selected_row().expect("row").is_override);

    let (action, default) = state.reset_selected().expect("reset reverts override");
    assert_eq!(action, Action::ToggleMinimap);
    assert_eq!(default, Action::ToggleMinimap.default_binding());
    let row = state.selected_row().expect("row");
    assert_eq!(row.binding, default);
    assert!(!row.is_override);

    // Resetting a row already at its default is a no-op.
    assert!(state.reset_selected().is_none());
}

#[test]
fn bare_structural_composer_keys_cannot_be_captured() {
    // Enter / Backspace / arrows / a bare letter are needed unconditionally by the
    // composer (typing + submission). The editor must refuse to bind them, exactly
    // like a reserved recovery key — otherwise three Enters in a row could rebind
    // Enter and brick prompt submission (deep-review #3).
    let rows = default_rows();
    for (binding, label) in [
        (KeyBinding::new(KeyCode::Enter, KeyModifiers::NONE), "Enter"),
        (
            KeyBinding::new(KeyCode::Backspace, KeyModifiers::NONE),
            "Backspace",
        ),
        (KeyBinding::new(KeyCode::Tab, KeyModifiers::NONE), "Tab"),
        (KeyBinding::new(KeyCode::Up, KeyModifiers::NONE), "Up"),
        (KeyBinding::new(KeyCode::Down, KeyModifiers::NONE), "Down"),
        (KeyBinding::new(KeyCode::Left, KeyModifiers::NONE), "Left"),
        (KeyBinding::new(KeyCode::Right, KeyModifiers::NONE), "Right"),
        (
            KeyBinding::new(KeyCode::Char('a'), KeyModifiers::NONE),
            "a bare key",
        ),
    ] {
        let outcome = capture_outcome(&rows, Action::ToggleMinimap, binding);
        assert_eq!(
            outcome,
            CaptureOutcome::Unsafe { label },
            "{binding:?} must classify as Unsafe",
        );
        assert!(
            !outcome.is_committable(),
            "{binding:?} must not be committable",
        );
    }
}

#[test]
fn modified_structural_keys_stay_bindable() {
    // Only BARE structural keys are refused — Alt+Enter, Ctrl+Left, Shift+Tab and a
    // modified letter remain capturable so power chords still work.
    let rows = default_rows();
    for binding in [
        KeyBinding::new(KeyCode::Enter, KeyModifiers::ALT),
        KeyBinding::new(KeyCode::Left, KeyModifiers::CONTROL),
        KeyBinding::new(KeyCode::Tab, KeyModifiers::SHIFT),
        KeyBinding::new(KeyCode::Char('a'), KeyModifiers::ALT),
    ] {
        let outcome = capture_outcome(&rows, Action::ToggleMinimap, binding);
        assert!(
            outcome.is_committable(),
            "{binding:?} must stay committable",
        );
        assert!(
            !matches!(outcome, CaptureOutcome::Unsafe { .. }),
            "{binding:?} must not be classified Unsafe",
        );
    }
}

#[test]
fn begin_capture_is_a_noop_when_already_capturing() {
    let mut state = editor();
    assert!(state.begin_capture());
    // A second begin while capturing returns false (and does not clobber pending).
    state.capture(KeyBinding::new(KeyCode::F(9), KeyModifiers::NONE));
    assert!(!state.begin_capture());
    assert!(state.pending().is_some());
}
