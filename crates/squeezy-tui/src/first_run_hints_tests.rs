//! Unit tests for the Gentle First-Run Interaction Hints engine (§12.1.8). Pure,
//! terminal-free coverage of the spec's checklist: triggers, the settle delay,
//! display-once latching, dismissal, modal suppression, the disabled-config path,
//! priority ordering, the live-chord message substitution, and — critically — the
//! no-idle-redraw-loop / zero-idle-cost invariant.

use super::*;

/// A fresh engine plus a base instant, so each test stamps deterministic times off
/// a single anchor without touching the wall clock.
fn engine() -> (HintEngine, Instant) {
    (HintEngine::default(), Instant::now())
}

/// `now` advanced past the settle window so a settled candidate actually paints.
fn settled(base: Instant) -> Instant {
    base + HINT_SETTLE + Duration::from_millis(1)
}

#[test]
fn fresh_engine_is_enabled_and_not_quiet() {
    let (engine, _) = engine();
    assert!(engine.is_enabled());
    // Nothing seen yet and the feature is on, so the engine is NOT quiet: it has
    // work to do (a hint will settle and paint).
    assert!(!engine.is_quiet());
    for id in HintId::ALL {
        assert!(!engine.is_seen(id), "no hint is seen on a fresh engine");
    }
}

#[test]
fn hint_does_not_flash_before_the_settle_delay() {
    let (engine, base) = engine();
    // The very first frame a hint becomes eligible stamps the clock but paints
    // nothing — no instant flash.
    assert_eq!(engine.active_hint(base, false), None);
    assert_eq!(engine.showing(), None);
    // Just shy of the settle window: still nothing.
    let almost = base + HINT_SETTLE - Duration::from_millis(1);
    assert_eq!(engine.active_hint(almost, false), None);
}

#[test]
fn highest_priority_hint_paints_after_settle() {
    let (engine, base) = engine();
    // Arm the settle clock, then advance past it.
    assert_eq!(engine.active_hint(base, false), None);
    let shown = engine.active_hint(settled(base), false);
    assert_eq!(
        shown,
        Some(HintId::PaletteChord),
        "the first priority hint paints once settled",
    );
    assert_eq!(engine.showing(), Some(HintId::PaletteChord));
}

#[test]
fn settle_clock_anchors_on_first_eligible_frame_not_process_start() {
    let (engine, base) = engine();
    // First eligibility is at `base + 10s` (e.g. the user only now reached a
    // main-surface frame). The settle window must be measured from THAT, not from
    // the engine's construction.
    let first = base + Duration::from_secs(10);
    assert_eq!(engine.active_hint(first, false), None, "stamps, no flash");
    // `base`-relative "settled" would be in the past for `first`; prove it still
    // waits the full window from `first`.
    assert_eq!(
        engine.active_hint(first + HINT_SETTLE - Duration::from_millis(1), false),
        None,
    );
    assert_eq!(
        engine.active_hint(first + HINT_SETTLE + Duration::from_millis(1), false),
        Some(HintId::PaletteChord),
    );
}

#[test]
fn note_used_fades_the_hint_once_and_latches_seen() {
    let (mut engine, base) = engine();
    // Paint it.
    engine.active_hint(base, false);
    assert_eq!(
        engine.active_hint(settled(base), false),
        Some(HintId::PaletteChord)
    );
    // The user does the thing it teaches: it fades, and the call reports that a
    // painted line must be erased.
    assert!(engine.note_used(HintId::PaletteChord));
    assert!(engine.is_seen(HintId::PaletteChord));
    assert_eq!(engine.showing(), None);
    // A second note_used is a no-op (already seen, nothing painted to erase).
    assert!(!engine.note_used(HintId::PaletteChord));
}

#[test]
fn note_used_before_paint_does_not_request_a_redraw() {
    let (mut engine, _) = engine();
    // The user opens the palette before the hint ever settled/painted: nothing was
    // on screen, so no erase repaint is needed — but the hint is still retired.
    assert!(
        !engine.note_used(HintId::PaletteChord),
        "no line was painted, so no redraw is requested",
    );
    assert!(engine.is_seen(HintId::PaletteChord));
}

#[test]
fn used_hint_yields_to_the_next_priority_hint() {
    let (mut engine, base) = engine();
    engine.active_hint(base, false);
    assert_eq!(
        engine.active_hint(settled(base), false),
        Some(HintId::PaletteChord)
    );
    engine.note_used(HintId::PaletteChord);
    // The next-priority hint now becomes the candidate and starts its OWN settle
    // window fresh (no instant flash on the hand-off frame).
    let t2 = settled(base);
    assert_eq!(
        engine.active_hint(t2, false),
        None,
        "next hint starts settling"
    );
    assert_eq!(
        engine.active_hint(settled(t2), false),
        Some(HintId::Hover),
        "the second priority hint paints after its own settle",
    );
}

#[test]
fn dismiss_retires_the_shown_hint_and_reports_it() {
    let (mut engine, base) = engine();
    engine.active_hint(base, false);
    assert_eq!(
        engine.active_hint(settled(base), false),
        Some(HintId::PaletteChord)
    );
    assert_eq!(engine.dismiss(), Some(HintId::PaletteChord));
    assert!(engine.is_seen(HintId::PaletteChord));
    assert_eq!(engine.showing(), None);
    // Nothing showing now ⇒ dismiss is a no-op that reports None (so the verb falls
    // through to the surface beneath).
    assert_eq!(engine.dismiss(), None);
}

#[test]
fn dismissing_all_three_makes_the_engine_quiet_forever() {
    let (mut engine, mut t) = engine();
    for expected in HintId::ALL {
        engine.active_hint(t, false);
        let painted = engine.active_hint(settled(t), false);
        assert_eq!(painted, Some(expected));
        assert_eq!(engine.dismiss(), Some(expected));
        t = settled(t);
    }
    // Every hint seen: the engine is quiet, paints nothing, and schedules no tick.
    assert!(engine.is_quiet());
    assert_eq!(engine.active_hint(settled(t), false), None);
    assert!(
        !engine.reveal_pending(settled(t), false),
        "an all-seen engine schedules no redraw — zero idle cost",
    );
}

#[test]
fn suppression_hides_the_hint_without_burning_its_showing() {
    let (engine, base) = engine();
    // Settle the first hint and paint it.
    engine.active_hint(base, false);
    assert_eq!(
        engine.active_hint(settled(base), false),
        Some(HintId::PaletteChord)
    );
    // A modal opens: suppressed ⇒ nothing paints, the settle clock does not advance
    // the candidate past, and the hint is NOT marked seen.
    assert_eq!(engine.active_hint(settled(base), true), None);
    assert!(
        !engine.is_seen(HintId::PaletteChord),
        "suppression never retires a hint"
    );
    // Modal closes: the same un-seen hint reappears.
    assert_eq!(
        engine.active_hint(settled(base), false),
        Some(HintId::PaletteChord)
    );
}

#[test]
fn reveal_pending_tracks_the_settle_window_then_goes_quiet() {
    let (engine, base) = engine();
    // Before the FIRST stamp: NO tick is scheduled from cold — the first render
    // (driven by focus, not by the hint engine) does the stamping. This is the
    // zero-idle-cost guarantee: the engine never spins up the loop itself.
    assert!(
        !engine.reveal_pending(base, false),
        "an un-stamped hint never schedules a tick from cold",
    );
    // Stamp the clock (as the first render does); still inside the window ⇒ pending,
    // so the loop keeps the already-running settle alive until it paints.
    engine.active_hint(base, false);
    assert!(engine.reveal_pending(base, false));
    // Past the window and showing ⇒ still considered pending (so the final settling
    // paint lands), then the engine quiesces once the hint is resolved.
    let after = settled(base);
    engine.active_hint(after, false); // paints + marks showing
    assert!(engine.reveal_pending(after, false));
    // Suppressed ⇒ never pending (no tick scheduled behind a modal).
    assert!(!engine.reveal_pending(after, true));
}

#[test]
fn disabled_engine_is_quiet_and_shows_nothing() {
    let (mut engine, base) = engine();
    // Toggle off returns the new (disabled) state.
    assert!(!engine.toggle());
    assert!(!engine.is_enabled());
    assert!(
        engine.is_quiet(),
        "a disabled engine is quiet — zero idle cost"
    );
    assert_eq!(engine.active_hint(settled(base), false), None);
    assert!(!engine.reveal_pending(settled(base), false));
    // Re-enabling does not resurrect any learned hints (none here), and the first
    // hint can settle again.
    assert!(engine.toggle());
    assert!(engine.is_enabled());
    assert!(!engine.is_quiet());
}

#[test]
fn disabling_clears_a_shown_hint_but_preserves_the_seen_set() {
    let (mut engine, base) = engine();
    engine.active_hint(base, false);
    assert_eq!(
        engine.active_hint(settled(base), false),
        Some(HintId::PaletteChord)
    );
    // Learn the second-priority hint's interaction directly.
    engine.note_used(HintId::Hover);
    assert!(engine.is_seen(HintId::Hover));
    // Disable: the shown line is cleared, but the Hover seen-latch survives.
    engine.toggle();
    assert_eq!(engine.showing(), None);
    assert!(
        engine.is_seen(HintId::Hover),
        "the seen-set survives a disable"
    );
}

#[test]
fn priority_order_is_palette_then_hover_then_jump() {
    // The declaration order in ALL is the priority order, and the engine always
    // surfaces the first not-yet-seen hint in it.
    assert_eq!(
        HintId::ALL,
        [HintId::PaletteChord, HintId::Hover, HintId::Jump]
    );
    let (mut engine, mut t) = engine();
    for expected in HintId::ALL {
        engine.active_hint(t, false);
        assert_eq!(engine.active_hint(settled(t), false), Some(expected));
        engine.note_used(expected);
        t = settled(t);
    }
}

#[test]
fn message_substitutes_the_live_chord_for_every_hint() {
    let palette = HintId::PaletteChord.message("Ctrl+Alt+P");
    assert!(palette.contains("Ctrl+Alt+P"), "{palette}");
    assert!(palette.contains("command palette"), "{palette}");
    let jump = HintId::Jump.message("Alt+Down");
    assert!(jump.contains("Alt+Down"), "{jump}");
    assert!(jump.contains("jump"), "{jump}");
    // The Hover hint substitutes the live focus chord too.
    let hover = HintId::Hover.message("Alt+\u{2191}/Alt+\u{2193}");
    assert!(hover.contains("Alt+\u{2191}/Alt+\u{2193}"), "{hover}");
    assert!(hover.to_lowercase().contains("peek"), "{hover}");
}

#[test]
fn slugs_are_stable_and_distinct() {
    let slugs: Vec<&str> = HintId::ALL.iter().map(|id| id.slug()).collect();
    assert_eq!(slugs, vec!["palette_chord", "hover", "jump"]);
    // ASCII-only and non-empty so a future persisted seen-set has a stable key.
    for slug in slugs {
        assert!(!slug.is_empty());
        assert!(slug.is_ascii());
    }
}
