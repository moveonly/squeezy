//! Unit tests for the pure Gesture Settings model (§12.7.5).
//!
//! These cover the defaults (single-sourced from the interaction timing
//! constants), the bounded clamp, the per-field adjust/cycle rules, the editor
//! cursor, and the config (de)serialization round-trip — all in isolation, with
//! no terminal and no `TuiApp`. The overlay's behaviour through the real
//! `render()` + key/mouse dispatch is covered by the capture-sink suite in
//! `lib_tests.rs`.

use std::time::{Duration, Instant};

use super::*;
use crate::interaction::{HOVER_INTENT_MS, MULTI_CLICK_MS, Phase, Recognizer, TargetKey};
use crate::transcript_surface::EntryId;

#[test]
fn default_dwell_is_single_sourced_from_interaction_constant() {
    // The default hover dwell must equal the recognizer's HOVER_INTENT_MS so the
    // settings surface can never drift from the live hover-intent delay.
    assert_eq!(
        u128::from(GestureSettings::DEFAULT.hover_dwell_ms),
        HOVER_INTENT_MS,
        "default dwell mirrors interaction::HOVER_INTENT_MS"
    );
    // And the read-only double-click window the editor surfaces is the same
    // constant the recognizer keys multi-click off of.
    assert_eq!(GestureSettings::multi_click_window_ms(), MULTI_CLICK_MS);
}

#[test]
fn default_is_within_every_bound() {
    let d = GestureSettings::DEFAULT;
    assert!(d.scroll_lines >= GestureSettings::SCROLL_MIN);
    assert!(d.scroll_lines <= GestureSettings::SCROLL_MAX);
    assert!(d.hover_dwell_ms <= GestureSettings::DWELL_MAX);
    assert_eq!(d, d.clamped(), "the default is already clamped");
}

#[test]
fn clamp_pulls_out_of_range_values_into_bounds() {
    let wild = GestureSettings {
        scroll_lines: 250,
        hover_dwell_ms: 9999,
        ..GestureSettings::DEFAULT
    }
    .clamped();
    assert_eq!(wild.scroll_lines, GestureSettings::SCROLL_MAX);
    assert_eq!(wild.hover_dwell_ms, GestureSettings::DWELL_MAX);

    let tiny = GestureSettings {
        scroll_lines: 0,
        ..GestureSettings::DEFAULT
    }
    .clamped();
    assert_eq!(tiny.scroll_lines, GestureSettings::SCROLL_MIN);
}

#[test]
fn scroll_lines_nudge_clamps_at_both_ends() {
    let mut s = GestureSettings {
        scroll_lines: GestureSettings::SCROLL_MAX,
        ..GestureSettings::DEFAULT
    };
    s = GestureField::ScrollLines.adjust(s, 1);
    assert_eq!(
        s.scroll_lines,
        GestureSettings::SCROLL_MAX,
        "forward nudge saturates at the ceiling"
    );

    let mut low = GestureSettings {
        scroll_lines: GestureSettings::SCROLL_MIN,
        ..GestureSettings::DEFAULT
    };
    low = GestureField::ScrollLines.adjust(low, -1);
    assert_eq!(
        low.scroll_lines,
        GestureSettings::SCROLL_MIN,
        "back nudge saturates at the floor"
    );
}

#[test]
fn dwell_nudge_steps_by_dwell_step_and_clamps() {
    let base = GestureSettings {
        hover_dwell_ms: 100,
        ..GestureSettings::DEFAULT
    };
    let up = GestureField::HoverDwellMs.adjust(base, 1);
    assert_eq!(up.hover_dwell_ms, 100 + GestureSettings::DWELL_STEP);
    let down = GestureField::HoverDwellMs.adjust(base, -1);
    assert_eq!(down.hover_dwell_ms, 100 - GestureSettings::DWELL_STEP);

    // Saturates at the ceiling.
    let high = GestureSettings {
        hover_dwell_ms: GestureSettings::DWELL_MAX,
        ..GestureSettings::DEFAULT
    };
    assert_eq!(
        GestureField::HoverDwellMs.adjust(high, 1).hover_dwell_ms,
        GestureSettings::DWELL_MAX
    );
}

#[test]
fn toggles_flip_regardless_of_direction() {
    let s = GestureSettings::DEFAULT;
    // Forward and back both flip a boolean (it is a two-state toggle).
    assert_ne!(
        GestureField::ShiftWheelPan.adjust(s, 1).shift_wheel_pan,
        s.shift_wheel_pan
    );
    assert_ne!(
        GestureField::ShiftWheelPan.adjust(s, -1).shift_wheel_pan,
        s.shift_wheel_pan
    );
    assert_ne!(
        GestureField::DragSelect.adjust(s, 1).drag_select,
        s.drag_select
    );
}

#[test]
fn double_click_action_cycles_through_all_three() {
    let mut a = DoubleClickAction::Expand;
    let mut seen = vec![a];
    for _ in 0..DoubleClickAction::ALL.len() {
        a = a.next();
        seen.push(a);
    }
    // After ALL.len() steps it wraps back to the start.
    assert_eq!(seen.first(), seen.last());
    for variant in DoubleClickAction::ALL {
        assert!(
            seen.contains(&variant),
            "cycle visits every action: missing {variant:?}"
        );
    }
}

#[test]
fn every_double_click_label_round_trips_through_config_str() {
    for variant in DoubleClickAction::ALL {
        assert_eq!(DoubleClickAction::from_str(variant.as_str()), Some(variant));
    }
    assert_eq!(DoubleClickAction::from_str("nonsense"), None);
}

#[test]
fn config_round_trip_preserves_every_field() {
    let original = GestureSettings {
        scroll_lines: 7,
        shift_wheel_pan: true,
        hover_dwell_ms: 300,
        double_click: DoubleClickAction::OpenDetail,
        drag_select: false,
    };
    let pairs = original.as_config_pairs();
    let restored = GestureSettings::from_config_lookup(GestureSettings::DEFAULT, |key| {
        pairs
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.clone())
    })
    .expect("a full table is recognised");
    assert_eq!(restored, original, "every field round-trips through config");
}

#[test]
fn partial_config_only_overrides_named_fields() {
    // Only the scroll-lines key is present; every other field keeps the default.
    let restored = GestureSettings::from_config_lookup(GestureSettings::DEFAULT, |key| {
        (key == KEY_SCROLL_LINES).then(|| "5".to_string())
    })
    .expect("a partial table is still recognised");
    assert_eq!(restored.scroll_lines, 5);
    assert_eq!(
        restored.shift_wheel_pan,
        GestureSettings::DEFAULT.shift_wheel_pan
    );
    assert_eq!(restored.double_click, GestureSettings::DEFAULT.double_click);
}

#[test]
fn empty_or_unparsable_config_yields_none() {
    // No recognised key at all -> None (distinct from "default override").
    assert_eq!(
        GestureSettings::from_config_lookup(GestureSettings::DEFAULT, |_| None),
        None
    );
    // A present-but-garbage value for the only key is ignored, leaving no
    // recognised field, so the whole lookup collapses to None.
    assert_eq!(
        GestureSettings::from_config_lookup(GestureSettings::DEFAULT, |key| {
            (key == KEY_SCROLL_LINES).then(|| "not-a-number".to_string())
        }),
        None
    );
}

#[test]
fn config_load_clamps_out_of_range_file_value() {
    // A hand-edited file with an absurd scroll value is clamped, never honoured
    // verbatim.
    let restored = GestureSettings::from_config_lookup(GestureSettings::DEFAULT, |key| {
        (key == KEY_SCROLL_LINES).then(|| "200".to_string())
    })
    .expect("recognised");
    assert_eq!(restored.scroll_lines, GestureSettings::SCROLL_MAX);
}

#[test]
fn permissive_bool_parsing_accepts_common_spellings() {
    for (key_present, expect) in [("true", true), ("on", true), ("1", true), ("yes", true)] {
        let restored = GestureSettings::from_config_lookup(
            GestureSettings {
                shift_wheel_pan: false,
                ..GestureSettings::DEFAULT
            },
            |key| (key == KEY_SHIFT_WHEEL_PAN).then(|| key_present.to_string()),
        )
        .expect("recognised");
        assert_eq!(restored.shift_wheel_pan, expect, "parsed {key_present}");
    }
    for falsey in ["false", "off", "0", "no"] {
        let restored = GestureSettings::from_config_lookup(
            GestureSettings {
                drag_select: true,
                ..GestureSettings::DEFAULT
            },
            |key| (key == KEY_DRAG_SELECT).then(|| falsey.to_string()),
        )
        .expect("recognised");
        assert!(!restored.drag_select, "parsed {falsey} as false");
    }
}

#[test]
fn editor_navigation_clamps_at_both_ends() {
    let mut ed = GestureSettingsEditor::new(None);
    assert_eq!(ed.field_index(), 0);
    assert!(!ed.focus_prev_field(), "already at the top, no move");
    // Walk to the bottom.
    for _ in 0..GestureField::ALL.len() {
        ed.focus_next_field();
    }
    assert_eq!(ed.field_index(), GestureField::ALL.len() - 1);
    assert!(!ed.focus_next_field(), "already at the bottom, no move");
}

#[test]
fn editor_focus_field_ignores_out_of_range_and_same_index() {
    let mut ed = GestureSettingsEditor::new(None);
    assert!(!ed.focus_field(0), "focusing the current index is a no-op");
    assert!(ed.focus_field(2), "focusing a new index moves");
    assert_eq!(ed.field_index(), 2);
    assert!(!ed.focus_field(99), "an out-of-range index is ignored");
    assert_eq!(ed.field_index(), 2, "focus unchanged after a bad index");
}

#[test]
fn editor_adjust_and_reset_round_trip() {
    let mut ed = GestureSettingsEditor::new(None);
    assert!(!ed.is_overridden(), "a fresh editor matches the default");
    // Cycle the focused (scroll) field forward, creating an override.
    ed.adjust_focused(1);
    assert!(ed.is_overridden(), "adjusting marks the editor overridden");
    // Reset restores the default exactly.
    let restored = ed.reset_to_default();
    assert_eq!(restored, ed.default_settings());
    assert!(!ed.is_overridden(), "reset clears the override");
}

#[test]
fn editor_seeds_from_override_and_clamps_it() {
    // A persisted override with an out-of-range scroll value is clamped on open.
    let pinned = GestureSettings {
        scroll_lines: 250,
        double_click: DoubleClickAction::None,
        ..GestureSettings::DEFAULT
    };
    let ed = GestureSettingsEditor::new(Some(pinned));
    assert_eq!(ed.working().scroll_lines, GestureSettings::SCROLL_MAX);
    assert_eq!(ed.working().double_click, DoubleClickAction::None);
    assert!(ed.is_overridden());
}

/// Build a synthetic `Instant` `ahead` of `base`. Uses [`Instant::checked_add`]
/// (forward, never a back-dated subtraction) so it is immune to the Windows clock
/// trap — a fresh Windows CI runner whose monotonic clock is younger than an
/// offset only panics on *subtraction*, never on addition. Falls back to `base`
/// in the (practically impossible) overflow case so the helper can never panic.
fn ahead(base: Instant, by: Duration) -> Instant {
    base.checked_add(by).unwrap_or(base)
}

#[test]
fn configured_dwell_drives_the_recognizer_hover_intent() {
    // The dwell the settings surface tunes is the same delay the recognizer uses
    // before it arms a hover. Drive the recognizer with synthetic instants derived
    // by adding forward offsets to a single `base` (Windows-safe — addition, not a
    // back-dated subtraction): a Move before the dwell does not arm, one at/after
    // the dwell does.
    let dwell = Duration::from_millis(u64::from(GestureSettings::DEFAULT.hover_dwell_ms));
    // The default dwell must be strictly positive for the "before" instant to land
    // between the first Move and the dwell deadline.
    assert!(dwell > Duration::ZERO, "the default dwell is positive");
    let mut rec = Recognizer::new();
    let target = TargetKey::Entry(EntryId(1));

    // First Move at `base` establishes the hover-intent clock.
    let base = Instant::now();
    let first = rec.recognize(Phase::Move, Some((target, dummy_action())), base);
    assert!(
        matches!(first, crate::interaction::Gesture::None),
        "the first hover does not arm immediately"
    );

    // A Move strictly before the dwell deadline still does not arm.
    let before = ahead(base, dwell / 2);
    let mid = rec.recognize(Phase::Move, Some((target, dummy_action())), before);
    assert!(
        matches!(mid, crate::interaction::Gesture::None),
        "a Move before the dwell elapses does not arm hover"
    );

    // A Move at/after the configured dwell arms the hover-enter.
    let after = ahead(base, dwell + Duration::from_millis(1));
    let armed = rec.recognize(Phase::Move, Some((target, dummy_action())), after);
    assert!(
        matches!(
            armed,
            crate::interaction::Gesture::HoverEnter { target: t } if t == target
        ),
        "a Move at/after the configured dwell arms HoverEnter: {armed:?}"
    );
}

/// A throwaway action for feeding the recognizer; its identity is irrelevant to
/// the hover-timing assertions.
fn dummy_action() -> crate::interaction::Action {
    crate::interaction::Action::FocusEntry(EntryId(1))
}
