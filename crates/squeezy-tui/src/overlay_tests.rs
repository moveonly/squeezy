use super::*;

#[test]
fn model_overlay_lists_registry_entries() {
    let overlay = build_model_overlay("openai", "nonexistent");
    let Overlay::Model(inner) = overlay else {
        panic!("expected model overlay")
    };
    assert_eq!(inner.entries.len(), MODEL_REGISTRY.len());
}

#[test]
fn model_overlay_selects_current() {
    let entry = &MODEL_REGISTRY[0];
    let overlay = build_model_overlay(entry.provider, entry.id);
    let Overlay::Model(inner) = overlay else {
        panic!("expected model overlay")
    };
    assert_eq!(inner.selected().unwrap().id, entry.id);
}

#[test]
fn verbosity_overlay_includes_three_levels() {
    let overlay = build_verbosity_overlay(ResponseVerbosity::Normal);
    let Overlay::Verbosity(inner) = overlay else {
        panic!("expected verbosity overlay")
    };
    assert_eq!(inner.entries.len(), 3);
    assert_eq!(inner.selected().unwrap().0, ResponseVerbosity::Normal);
}

#[test]
fn select_overlay_navigation_clamps_at_bounds() {
    let mut overlay = build_verbosity_overlay(ResponseVerbosity::Normal);
    overlay.move_up();
    overlay.move_up();
    overlay.move_up(); // should not panic
    overlay.move_down();
    overlay.move_down();
    overlay.move_down();
    overlay.move_down(); // should not panic
    // unchanged in shape
    if let Overlay::Verbosity(inner) = overlay {
        assert_eq!(inner.entries.len(), 3);
    }
}

#[test]
fn overlay_render_lines_include_title_and_options() {
    let overlay = build_verbosity_overlay(ResponseVerbosity::Concise);
    let rendered: String = overlay
        .render_lines()
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("Select response verbosity"), "{rendered}");
    assert!(rendered.contains("concise"), "{rendered}");
    assert!(rendered.contains("normal"), "{rendered}");
    assert!(rendered.contains("verbose"), "{rendered}");
}

#[test]
fn dialog_handle_open_assigns_fresh_id_and_fills_slot() {
    let mut slot: Option<Overlay> = None;
    let mut next_id: u64 = 0;
    let mut active_id: Option<u64> = None;
    let handle = DialogHandle::open(
        &mut slot,
        &mut next_id,
        &mut active_id,
        build_verbosity_overlay(ResponseVerbosity::Normal),
        PriorFocus::Composer,
    );
    assert_eq!(next_id, 1);
    assert_eq!(active_id, Some(handle.id()));
    assert!(matches!(slot, Some(Overlay::Verbosity(_))));
    assert_eq!(handle.prior_focus(), &PriorFocus::Composer);
}

#[test]
fn dialog_handle_hide_removes_overlay() {
    let mut slot: Option<Overlay> = None;
    let mut next_id: u64 = 0;
    let mut active_id: Option<u64> = None;
    let handle = DialogHandle::open(
        &mut slot,
        &mut next_id,
        &mut active_id,
        build_verbosity_overlay(ResponseVerbosity::Normal),
        PriorFocus::Composer,
    );
    assert!(slot.is_some());

    let closed = handle.hide(&mut slot, &mut active_id);
    assert!(closed);
    assert!(slot.is_none());
    assert!(active_id.is_none());
}

#[test]
fn dialog_handle_hide_on_stale_handle_is_noop() {
    let mut slot: Option<Overlay> = None;
    let mut next_id: u64 = 0;
    let mut active_id: Option<u64> = None;

    let stale = DialogHandle::open(
        &mut slot,
        &mut next_id,
        &mut active_id,
        build_verbosity_overlay(ResponseVerbosity::Normal),
        PriorFocus::Composer,
    );
    // A second open replaces the dialog; `stale` now refers to a closed
    // dialog generation while a fresh one owns the slot.
    let fresh = DialogHandle::open(
        &mut slot,
        &mut next_id,
        &mut active_id,
        build_model_overlay("openai", "nonexistent"),
        PriorFocus::Composer,
    );
    assert_ne!(stale.id(), fresh.id());

    let closed = stale.hide(&mut slot, &mut active_id);
    assert!(!closed, "stale handle must not close the live dialog");
    assert!(matches!(slot, Some(Overlay::Model(_))));
    assert_eq!(active_id, Some(fresh.id()));
}

#[test]
fn dialog_handle_set_replaces_content_and_keeps_handle_valid() {
    let mut slot: Option<Overlay> = None;
    let mut next_id: u64 = 0;
    let mut active_id: Option<u64> = None;
    let handle = DialogHandle::open(
        &mut slot,
        &mut next_id,
        &mut active_id,
        build_verbosity_overlay(ResponseVerbosity::Normal),
        PriorFocus::Composer,
    );

    let replaced = handle.set(
        &mut slot,
        &active_id,
        build_model_overlay("openai", "nonexistent"),
    );
    assert!(replaced);
    assert!(matches!(slot, Some(Overlay::Model(_))));
    // Same id — the handle is still live.
    assert_eq!(active_id, Some(handle.id()));

    // And the same handle still hides the slot.
    let closed = handle.hide(&mut slot, &mut active_id);
    assert!(closed);
    assert!(slot.is_none());
}

#[test]
fn dialog_handle_restore_focus_returns_prior_holder() {
    let mut slot: Option<Overlay> = None;
    let mut next_id: u64 = 0;
    let mut active_id: Option<u64> = None;
    let handle = DialogHandle::open(
        &mut slot,
        &mut next_id,
        &mut active_id,
        build_verbosity_overlay(ResponseVerbosity::Normal),
        PriorFocus::Composer,
    );

    let restored = handle.restore_focus(&mut slot, &mut active_id);
    assert_eq!(restored, PriorFocus::Composer);
    // restore_focus also closes the dialog.
    assert!(slot.is_none());
    assert!(active_id.is_none());
}

#[test]
fn dialog_handle_restore_focus_on_stale_handle_still_reports_prior() {
    let mut slot: Option<Overlay> = None;
    let mut next_id: u64 = 0;
    let mut active_id: Option<u64> = None;
    let stale = DialogHandle::open(
        &mut slot,
        &mut next_id,
        &mut active_id,
        build_verbosity_overlay(ResponseVerbosity::Normal),
        PriorFocus::None,
    );
    let _fresh = DialogHandle::open(
        &mut slot,
        &mut next_id,
        &mut active_id,
        build_model_overlay("openai", "nonexistent"),
        PriorFocus::Composer,
    );

    // Stale handle does not close the live dialog…
    let restored = stale.restore_focus(&mut slot, &mut active_id);
    assert!(matches!(slot, Some(Overlay::Model(_))));
    // …but still reports its own captured focus hint.
    assert_eq!(restored, PriorFocus::None);
}
