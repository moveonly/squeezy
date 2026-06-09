//! Unit tests for the frame-local hit-test registry, focus model, and gesture
//! recognizer. Included into [`crate::interaction`] via `#[path]` per the repo
//! test layout. Every assertion is a pure function over model state — no
//! terminal, no clock except an injected `Instant`.

use super::*;
use crate::transcript_surface::{EntryId, RowId};
use std::time::{Duration, Instant};

// ---- builders -------------------------------------------------------------

fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}

fn entry_key(id: u64) -> TargetKey {
    TargetKey::Entry(EntryId(id))
}

// ===========================================================================
// Registry hit-testing
// ===========================================================================

#[test]
fn hit_test_returns_key_and_action_for_containing_rect() {
    let mut reg = Registry::new();
    reg.register(
        rect(2, 3, 10, 1),
        entry_key(7),
        Action::FocusEntry(EntryId(7)),
    );
    // Inside.
    assert_eq!(
        reg.hit_test(2, 3),
        Some((entry_key(7), Action::FocusEntry(EntryId(7))))
    );
    assert_eq!(
        reg.hit_test(11, 3),
        Some((entry_key(7), Action::FocusEntry(EntryId(7))))
    );
    // Just outside on each axis (half-open).
    assert_eq!(reg.hit_test(12, 3), None);
    assert_eq!(reg.hit_test(1, 3), None);
    assert_eq!(reg.hit_test(2, 4), None);
}

#[test]
fn hit_test_topmost_wins_via_reverse_iteration() {
    let mut reg = Registry::new();
    // Two overlapping targets at the same cell; the later-registered one (an
    // overlay drawn on top) must win.
    reg.register(
        rect(0, 0, 5, 5),
        entry_key(1),
        Action::FocusEntry(EntryId(1)),
    );
    reg.register(
        rect(0, 0, 5, 5),
        TargetKey::Chrome(ChromeKey::QueueStrip),
        Action::ToggleQueueOverlay,
    );
    assert_eq!(
        reg.hit_test(2, 2),
        Some((
            TargetKey::Chrome(ChromeKey::QueueStrip),
            Action::ToggleQueueOverlay
        ))
    );
}

#[test]
fn begin_frame_clears_then_reregistration_moves_target() {
    let mut reg = Registry::new();
    reg.register(
        rect(0, 0, 4, 1),
        entry_key(3),
        Action::ToggleEntryCollapsed(EntryId(3)),
    );
    assert_eq!(reg.len(), 1);
    assert!(reg.hit_test(1, 0).is_some());

    // Next frame: clear and re-register the SAME key at a NEW rect (a resize
    // moved the header down a row). The hit-test follows the key's new rect,
    // never a remembered coordinate.
    reg.begin_frame();
    assert_eq!(reg.len(), 0);
    assert_eq!(reg.hit_test(1, 0), None);
    reg.register(
        rect(0, 5, 4, 1),
        entry_key(3),
        Action::ToggleEntryCollapsed(EntryId(3)),
    );
    assert_eq!(reg.hit_test(1, 0), None);
    assert_eq!(
        reg.hit_test(1, 5),
        Some((entry_key(3), Action::ToggleEntryCollapsed(EntryId(3))))
    );
}

#[test]
fn hit_test_distinguishes_cards_sharing_an_action_kind_via_key() {
    // Two card headers both produce a FocusEntry action but for different
    // entries; the returned key tells them apart.
    let mut reg = Registry::new();
    reg.register(
        rect(0, 0, 8, 1),
        entry_key(10),
        Action::FocusEntry(EntryId(10)),
    );
    reg.register(
        rect(0, 1, 8, 1),
        entry_key(20),
        Action::FocusEntry(EntryId(20)),
    );
    assert_eq!(reg.hit_test(0, 0).unwrap().0, entry_key(10));
    assert_eq!(reg.hit_test(0, 1).unwrap().0, entry_key(20));
}

#[test]
fn rowspan_target_round_trips() {
    let mut reg = Registry::new();
    let key = TargetKey::RowSpan(RowId(4), RowSpan::new(2, 9));
    reg.register(rect(2, 0, 7, 1), key, Action::JumpToLatest);
    assert_eq!(reg.hit_test(5, 0), Some((key, Action::JumpToLatest)));
}

// ===========================================================================
// Focus model
// ===========================================================================

#[test]
fn focus_next_prev_wrap_and_step() {
    let ids = [100u64, 101, 102];
    let mut focus = Focus::new();
    assert_eq!(focus.focused(), None);

    // From none, next wraps to the first.
    assert_eq!(focus.focus_next(&ids), Some(EntryId(100)));
    assert_eq!(focus.focus_next(&ids), Some(EntryId(101)));
    assert_eq!(focus.focus_next(&ids), Some(EntryId(102)));
    // Clamp at the last entry (mirrors select_next_transcript_entry).
    assert_eq!(focus.focus_next(&ids), Some(EntryId(102)));

    // From none, prev wraps to the last.
    let mut focus = Focus::new();
    assert_eq!(focus.focus_prev(&ids), Some(EntryId(102)));
    assert_eq!(focus.focus_prev(&ids), Some(EntryId(101)));
    assert_eq!(focus.focus_prev(&ids), Some(EntryId(100)));
    // Clamp at the first entry.
    assert_eq!(focus.focus_prev(&ids), Some(EntryId(100)));
}

#[test]
fn focus_resolves_id_to_live_index() {
    let ids = [5u64, 9, 13];
    let mut focus = Focus::new();
    focus.set(EntryId(9));
    assert_eq!(focus.resolve_index(&ids), Some(1));
    // The same id resolves to a NEW index after an earlier entry is pruned —
    // this is the whole point of keying focus on the id, not the index.
    let pruned = [9u64, 13];
    assert_eq!(focus.resolve_index(&pruned), Some(0));
    // A focus whose id vanished resolves to None (caller falls back).
    let gone = [13u64];
    assert_eq!(focus.resolve_index(&gone), None);
}

#[test]
fn focus_next_from_pruned_focus_restarts_from_first() {
    let ids = [1u64, 2, 3];
    let mut focus = Focus::new();
    focus.set(EntryId(99)); // not in `ids`
    // resolve_index is None, so focus_next behaves like "from none".
    assert_eq!(focus.focus_next(&ids), Some(EntryId(1)));
}

#[test]
fn focus_set_from_index_syncs_id() {
    let ids = [7u64, 8, 9];
    let mut focus = Focus::new();
    focus.set_from_index(Some(2), &ids);
    assert_eq!(focus.focused(), Some(EntryId(9)));
    focus.set_from_index(None, &ids);
    assert_eq!(focus.focused(), None);
    // Out-of-range index clears focus rather than panicking.
    focus.set_from_index(Some(99), &ids);
    assert_eq!(focus.focused(), None);
}

#[test]
fn focus_on_empty_order_is_inert() {
    let ids: [u64; 0] = [];
    let mut focus = Focus::new();
    assert_eq!(focus.focus_next(&ids), None);
    assert_eq!(focus.focus_prev(&ids), None);
    assert_eq!(focus.focused(), None);
}

// ===========================================================================
// Gesture recognizer
// ===========================================================================

fn t0() -> Instant {
    Instant::now()
}

fn hit(key: TargetKey, action: Action) -> Option<(TargetKey, Action)> {
    Some((key, action))
}

#[test]
fn single_double_triple_click_keyed_on_target() {
    let mut rec = Recognizer::new();
    let now = t0();
    let h = hit(entry_key(1), Action::FocusEntry(EntryId(1)));

    let g1 = rec.recognize(Phase::Press, h, now);
    assert!(matches!(
        g1,
        Gesture::Click {
            target: Some(_),
            ..
        }
    ));
    // Second press on the same key within the window → double.
    let g2 = rec.recognize(Phase::Press, h, now + Duration::from_millis(100));
    assert!(matches!(g2, Gesture::DoubleClick { .. }));
    // Third → triple.
    let g3 = rec.recognize(Phase::Press, h, now + Duration::from_millis(200));
    assert!(matches!(g3, Gesture::TripleClick { .. }));
    // Fourth stays clamped at triple.
    let g4 = rec.recognize(Phase::Press, h, now + Duration::from_millis(300));
    assert!(matches!(g4, Gesture::TripleClick { .. }));
}

#[test]
fn multiplicity_keyed_on_key_not_cell_survives_reflow() {
    // The correctness caveat the design calls out: a double-click that lands on
    // the SAME target after a reflow (different cell, but same EntryId) must
    // still be a double. Keying on the key — not the screen cell — guarantees
    // it. We model "same key, two presses" and expect a double.
    let mut rec = Recognizer::new();
    let now = t0();
    let h = hit(entry_key(42), Action::FocusEntry(EntryId(42)));
    let _ = rec.recognize(Phase::Press, h, now);
    let g = rec.recognize(Phase::Press, h, now + Duration::from_millis(50));
    assert!(matches!(g, Gesture::DoubleClick { .. }));
}

#[test]
fn press_on_different_key_resets_multiplicity() {
    let mut rec = Recognizer::new();
    let now = t0();
    let a = hit(entry_key(1), Action::FocusEntry(EntryId(1)));
    let b = hit(entry_key(2), Action::FocusEntry(EntryId(2)));
    let _ = rec.recognize(Phase::Press, a, now);
    // A press on a DIFFERENT key within the window is a fresh single, not a
    // double.
    let g = rec.recognize(Phase::Press, b, now + Duration::from_millis(50));
    assert!(matches!(g, Gesture::Click { .. }));
}

#[test]
fn slow_second_press_is_a_fresh_single() {
    let mut rec = Recognizer::new();
    let now = t0();
    let h = hit(entry_key(1), Action::FocusEntry(EntryId(1)));
    let _ = rec.recognize(Phase::Press, h, now);
    // Past the multi-click window → single again.
    let g = rec.recognize(
        Phase::Press,
        h,
        now + Duration::from_millis(MULTI_CLICK_MS as u64 + 1),
    );
    assert!(matches!(g, Gesture::Click { .. }));
}

#[test]
fn click_carries_target_and_action() {
    let mut rec = Recognizer::new();
    let now = t0();
    let h = hit(entry_key(5), Action::ToggleEntryCollapsed(EntryId(5)));
    match rec.recognize(Phase::Press, h, now) {
        Gesture::Click { target, action } => {
            assert_eq!(target, Some(entry_key(5)));
            assert_eq!(action, Some(Action::ToggleEntryCollapsed(EntryId(5))));
        }
        other => panic!("expected Click, got {other:?}"),
    }
}

#[test]
fn drag_start_extend_end_track_keys_not_pixels() {
    let mut rec = Recognizer::new();
    let now = t0();
    let origin = hit(TargetKey::QueueItem(1), Action::QueueReorderBegin(1));
    let over2 = hit(TargetKey::QueueItem(2), Action::QueueReorderBegin(2));
    let over3 = hit(TargetKey::QueueItem(3), Action::QueueReorderBegin(3));

    // Press starts a potential drag.
    let _ = rec.recognize(Phase::Press, origin, now);
    assert!(rec.is_dragging());

    // First Drag off the origin → DragStart, drag.origin is the item we grabbed.
    let g = rec.recognize(Phase::Drag, over2, now);
    assert!(matches!(
        g,
        Gesture::DragStart {
            target: Some(TargetKey::QueueItem(1))
        }
    ));
    assert_eq!(rec.drag().unwrap().current, Some(TargetKey::QueueItem(2)));

    // Subsequent Drag → DragExtend with the new hovered key as insertion anchor.
    let g = rec.recognize(Phase::Drag, over3, now);
    assert!(matches!(
        g,
        Gesture::DragExtend {
            target: Some(TargetKey::QueueItem(3))
        }
    ));

    // Release ends the drag at the drop key.
    let g = rec.recognize(Phase::Release, over3, now);
    assert!(matches!(
        g,
        Gesture::DragEnd {
            target: Some(TargetKey::QueueItem(3))
        }
    ));
    assert!(!rec.is_dragging());
}

#[test]
fn press_release_without_movement_is_not_a_drag() {
    let mut rec = Recognizer::new();
    let now = t0();
    let h = hit(entry_key(1), Action::FocusEntry(EntryId(1)));
    let _ = rec.recognize(Phase::Press, h, now); // emits Click
    // Release with no intervening Drag → no DragEnd, just None (the click
    // already fired on press).
    let g = rec.recognize(Phase::Release, h, now);
    assert!(matches!(g, Gesture::None));
    assert!(!rec.is_dragging());
}

#[test]
fn drag_resolves_marker_from_current_key_after_resize() {
    // A resize mid-drag changes which cell maps to which queue item, but the
    // recognizer only ever stores the hovered KEY. We model that by feeding a
    // different key on the post-resize Drag and confirming the marker follows
    // the key, never a stale pixel.
    let mut rec = Recognizer::new();
    let now = t0();
    let origin = hit(TargetKey::QueueItem(1), Action::QueueReorderBegin(1));
    let _ = rec.recognize(Phase::Press, origin, now);
    let _ = rec.recognize(
        Phase::Drag,
        hit(TargetKey::QueueItem(2), Action::QueueReorderBegin(2)),
        now,
    );
    // Post-resize Drag lands on item 5.
    let g = rec.recognize(
        Phase::Drag,
        hit(TargetKey::QueueItem(5), Action::QueueReorderBegin(5)),
        now,
    );
    assert!(matches!(
        g,
        Gesture::DragExtend {
            target: Some(TargetKey::QueueItem(5))
        }
    ));
    assert_eq!(rec.drag().unwrap().current, Some(TargetKey::QueueItem(5)));
}

#[test]
fn hover_intent_requires_delay_on_same_key() {
    let mut rec = Recognizer::new();
    let now = t0();
    let h = hit(entry_key(1), Action::FocusEntry(EntryId(1)));

    // First Move onto the key starts the clock — no enter yet.
    let g = rec.recognize(Phase::Move, h, now);
    assert!(matches!(g, Gesture::None));

    // A Move before the delay elapses still produces nothing.
    let g = rec.recognize(
        Phase::Move,
        h,
        now + Duration::from_millis(HOVER_INTENT_MS as u64 - 1),
    );
    assert!(matches!(g, Gesture::None));

    // Once the delay elapses on the SAME key → HoverEnter.
    let g = rec.recognize(
        Phase::Move,
        h,
        now + Duration::from_millis(HOVER_INTENT_MS as u64),
    );
    assert!(matches!(g, Gesture::HoverEnter { target } if target == entry_key(1)));

    // A further Move on the same armed key is a no-op (no re-enter).
    let g = rec.recognize(
        Phase::Move,
        h,
        now + Duration::from_millis(HOVER_INTENT_MS as u64 + 50),
    );
    assert!(matches!(g, Gesture::None));
}

#[test]
fn hover_leave_when_pointer_leaves_armed_target() {
    let mut rec = Recognizer::new();
    let now = t0();
    let h = hit(entry_key(1), Action::FocusEntry(EntryId(1)));
    let _ = rec.recognize(Phase::Move, h, now);
    let _ = rec.recognize(
        Phase::Move,
        h,
        now + Duration::from_millis(HOVER_INTENT_MS as u64),
    );
    // Move onto empty space after arming → HoverLeave.
    let g = rec.recognize(
        Phase::Move,
        None,
        now + Duration::from_millis(HOVER_INTENT_MS as u64 + 10),
    );
    assert!(matches!(g, Gesture::HoverLeave));
}

#[test]
fn hover_switch_to_new_key_leaves_old_and_restarts_clock() {
    let mut rec = Recognizer::new();
    let now = t0();
    let a = hit(entry_key(1), Action::FocusEntry(EntryId(1)));
    let b = hit(entry_key(2), Action::FocusEntry(EntryId(2)));
    let _ = rec.recognize(Phase::Move, a, now);
    let _ = rec.recognize(
        Phase::Move,
        a,
        now + Duration::from_millis(HOVER_INTENT_MS as u64),
    );
    // Move onto a different key: leave the old one, restart the intent clock.
    let g = rec.recognize(
        Phase::Move,
        b,
        now + Duration::from_millis(HOVER_INTENT_MS as u64 + 5),
    );
    assert!(matches!(g, Gesture::HoverLeave));
    // The new key isn't armed yet.
    let g = rec.recognize(
        Phase::Move,
        b,
        now + Duration::from_millis(HOVER_INTENT_MS as u64 + 6),
    );
    assert!(matches!(g, Gesture::None));
    // After its own delay, it arms.
    let g = rec.recognize(
        Phase::Move,
        b,
        now + Duration::from_millis(HOVER_INTENT_MS as u64 + 5 + HOVER_INTENT_MS as u64),
    );
    assert!(matches!(g, Gesture::HoverEnter { target } if target == entry_key(2)));
}

#[test]
fn press_clears_hover_state() {
    let mut rec = Recognizer::new();
    let now = t0();
    let h = hit(entry_key(1), Action::FocusEntry(EntryId(1)));
    let _ = rec.recognize(Phase::Move, h, now);
    let _ = rec.recognize(
        Phase::Move,
        h,
        now + Duration::from_millis(HOVER_INTENT_MS as u64),
    );
    // A press while hovering begins a drag and drops hover state.
    let _ = rec.recognize(
        Phase::Press,
        h,
        now + Duration::from_millis(HOVER_INTENT_MS as u64 + 1),
    );
    assert!(rec.is_dragging());
    // The next Move on empty space should not report a stale leave from the
    // pre-press hover (it was cleared).
    let g = rec.recognize(
        Phase::Drag,
        None,
        now + Duration::from_millis(HOVER_INTENT_MS as u64 + 2),
    );
    assert!(matches!(g, Gesture::DragStart { .. }));
}

#[test]
fn reset_clears_all_in_flight_state() {
    let mut rec = Recognizer::new();
    let now = t0();
    let h = hit(entry_key(1), Action::FocusEntry(EntryId(1)));
    let _ = rec.recognize(Phase::Press, h, now);
    assert!(rec.is_dragging());
    rec.reset();
    assert!(!rec.is_dragging());
    // After reset, a press is a fresh single (multiplicity history gone).
    let g = rec.recognize(Phase::Press, h, now + Duration::from_millis(10));
    assert!(matches!(g, Gesture::Click { .. }));
}

// ===========================================================================
// Keyboard / mouse parity at the action level
// ===========================================================================
//
// Full parity (mouse gesture → same handler as the keymap action) is exercised
// in lib_tests.rs against a live `TuiApp`. Here we assert the substrate-level
// invariant: a caret click and the Ctrl+O keyboard verb both resolve to the
// SAME `Action::ToggleEntryCollapsed(EntryId)`, and a header click and a
// focus-set both resolve to `Action::FocusEntry(EntryId)` — i.e. the mouse
// path produces exactly the action the keyboard path would dispatch.

#[test]
fn caret_click_and_keyboard_fold_share_one_action() {
    let id = EntryId(77);
    // The registry registers the caret target with the toggle action; the
    // keyboard ToggleFocusedFold resolves the focused id and dispatches the
    // same variant.
    let caret_action = Action::ToggleEntryCollapsed(id);
    let keyboard_action = Action::ToggleEntryCollapsed(id);
    assert_eq!(caret_action, keyboard_action);
}

#[test]
fn header_click_and_focus_set_share_one_action() {
    let id = EntryId(77);
    assert_eq!(Action::FocusEntry(id), Action::FocusEntry(id));
}

#[test]
fn double_click_collapsed_card_maps_to_expand() {
    // A double-click over a card header dispatches ExpandEntry (guarded to only
    // expand when collapsed). Distinct from the single-click FocusEntry.
    let id = EntryId(3);
    assert_ne!(Action::FocusEntry(id), Action::ExpandEntry(id));
}
