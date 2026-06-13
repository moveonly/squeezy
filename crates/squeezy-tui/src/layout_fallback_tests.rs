//! Unit tests for the Last-Known-Good Layout Fallback (§12.9.3) policy: the
//! degenerate-frame validity check, the record-good / fall-back state machine,
//! the size-keyed substitution (no stale geometry across a resize), and the
//! diagnostics line. Pure logic, no TTY.

use super::*;

/// A valid, roomy main-view geometry for an `area` of `width x height`: a
/// composer plus a transcript that together fit the area. The convenience
/// builder keeps the table tests focused on the field under test.
fn good_geometry(width: u16, height: u16) -> LayoutGeometry {
    LayoutGeometry {
        width,
        height,
        task_height: None,
        approval_height: 0,
        plan_indicator_height: 0,
        subagent_height: 0,
        attachment_height: 0,
        transcript_prompt_gap_height: 0,
        // Reserve a 1-row composer + the fixed status block + a transcript that
        // fits the rest, so the geometry exactly fills the area and is valid.
        transcript_height: height.saturating_sub(1 + STATUS_BLOCK_HEIGHT),
        show_completed_turn_divider: false,
        input_height: 1,
    }
}

// ---------------------------------------------------------------------------
// Validity check.
// ---------------------------------------------------------------------------

#[test]
fn a_roomy_composer_plus_transcript_frame_is_not_degenerate() {
    let geom = good_geometry(80, 24);
    assert!(
        !geom.is_degenerate(),
        "a frame with a composer that fits its area is usable",
    );
}

#[test]
fn a_frame_with_no_composer_row_is_degenerate() {
    let mut geom = good_geometry(80, 24);
    geom.input_height = 0;
    assert!(
        geom.is_degenerate(),
        "losing the composer leaves no way to type — a degenerate frame",
    );
}

#[test]
fn a_frame_whose_reserved_rows_overflow_the_area_is_degenerate() {
    let mut geom = good_geometry(80, 10);
    // Ask for more transcript rows than the whole area, with the composer on
    // top: the reserved height now exceeds the area and ratatui would clip.
    geom.transcript_height = 20;
    geom.input_height = 1;
    assert!(
        geom.reserved_height() > geom.height,
        "test fixture really does overflow the area",
    );
    assert!(
        geom.is_degenerate(),
        "an over-constrained layout that clips the bottom is degenerate",
    );
}

#[test]
fn a_sub_usable_tiny_area_is_never_degenerate() {
    // A 2-row terminal genuinely cannot host the split. With no composer row it
    // would be degenerate on a usable area, but here there is no better frame to
    // restore, so the check is disabled.
    let mut geom = good_geometry(80, 2);
    geom.input_height = 0;
    geom.transcript_height = 0;
    assert!(
        !LayoutGeometry::area_is_usable(geom.width, geom.height),
        "a 2-row area is below the usable threshold",
    );
    assert!(
        !geom.is_degenerate(),
        "nothing is degenerate on a sub-usable area — paint what we have",
    );
}

#[test]
fn the_usable_thresholds_are_the_documented_minimums() {
    // Pin the constants so a future tweak is a conscious decision, not a silent
    // change to what counts as a usable area. Wrapped in a `const` block to
    // satisfy clippy's `assertions_on_constants`.
    const _: () = {
        assert!(MIN_USABLE_HEIGHT == 4);
        assert!(MIN_USABLE_WIDTH == 8);
    };
    assert!(!LayoutGeometry::area_is_usable(MIN_USABLE_WIDTH - 1, 24));
    assert!(!LayoutGeometry::area_is_usable(80, MIN_USABLE_HEIGHT - 1));
    assert!(LayoutGeometry::area_is_usable(
        MIN_USABLE_WIDTH,
        MIN_USABLE_HEIGHT
    ));
}

// ---------------------------------------------------------------------------
// Record-good / fall-back state machine.
// ---------------------------------------------------------------------------

#[test]
fn a_fresh_store_holds_no_good_snapshot() {
    let store = LastGoodLayout::default();
    assert_eq!(store.good(), None);
    assert_eq!(store.fallback_count(), 0);
}

#[test]
fn a_valid_frame_is_recorded_and_used_verbatim() {
    let store = LastGoodLayout::default();
    let geom = good_geometry(80, 24);
    let resolution = store.resolve(geom);
    assert_eq!(resolution, LayoutResolution::Use(geom));
    assert!(!resolution.is_fallback());
    assert_eq!(resolution.geometry(), geom);
    assert_eq!(
        store.good(),
        Some(geom),
        "a valid frame becomes the good snapshot"
    );
    assert_eq!(store.fallback_count(), 0);
}

#[test]
fn a_degenerate_frame_with_a_same_size_good_snapshot_falls_back() {
    let store = LastGoodLayout::default();
    let good = good_geometry(80, 24);
    // First a good frame establishes the snapshot.
    assert_eq!(store.resolve(good), LayoutResolution::Use(good));

    // Then a degenerate frame at the SAME size: it should restore the good one.
    let mut broken = good_geometry(80, 24);
    broken.input_height = 0;
    let resolution = store.resolve(broken);
    assert!(resolution.is_fallback());
    assert_eq!(
        resolution,
        LayoutResolution::Fallback(good),
        "the degenerate frame is replaced by the last good geometry",
    );
    assert_eq!(resolution.geometry(), good);
    assert_eq!(store.fallback_count(), 1);
    assert_eq!(
        store.good(),
        Some(good),
        "a degenerate frame never overwrites the good snapshot",
    );
}

#[test]
fn a_degenerate_frame_with_no_good_snapshot_is_used_as_is() {
    let store = LastGoodLayout::default();
    let mut broken = good_geometry(80, 24);
    broken.input_height = 0;
    let resolution = store.resolve(broken);
    assert_eq!(
        resolution,
        LayoutResolution::Use(broken),
        "with no prior good frame there is nothing better to paint",
    );
    assert!(!resolution.is_fallback());
    assert_eq!(
        store.good(),
        None,
        "a degenerate frame is not recorded as good"
    );
    assert_eq!(store.fallback_count(), 0);
}

#[test]
fn a_degenerate_frame_at_a_different_size_does_not_use_stale_geometry() {
    let store = LastGoodLayout::default();
    let good = good_geometry(80, 24);
    assert_eq!(store.resolve(good), LayoutResolution::Use(good));

    // A degenerate frame at a DIFFERENT (resized) size must not restore the
    // 80x24 snapshot — that geometry is wrong for the new viewport.
    let mut broken = good_geometry(120, 40);
    broken.input_height = 0;
    let resolution = store.resolve(broken);
    assert_eq!(
        resolution,
        LayoutResolution::Use(broken),
        "a resized degenerate frame paints its own geometry, never stale rows",
    );
    assert!(!resolution.is_fallback());
    assert_eq!(store.fallback_count(), 0);
}

#[test]
fn a_later_valid_frame_replaces_the_good_snapshot() {
    let store = LastGoodLayout::default();
    let first = good_geometry(80, 24);
    let _ = store.resolve(first);

    let mut second = good_geometry(80, 24);
    second.transcript_height = 10;
    second.attachment_height = 3;
    let resolution = store.resolve(second);
    assert_eq!(resolution, LayoutResolution::Use(second));
    assert_eq!(
        store.good(),
        Some(second),
        "the newest valid frame is the last-known-good",
    );
}

#[test]
fn repeated_degenerate_frames_keep_falling_back_and_counting() {
    let store = LastGoodLayout::default();
    let good = good_geometry(80, 24);
    let _ = store.resolve(good);

    let mut broken = good_geometry(80, 24);
    broken.input_height = 0;
    for expected in 1..=3u64 {
        let resolution = store.resolve(broken);
        assert_eq!(resolution, LayoutResolution::Fallback(good));
        assert_eq!(store.fallback_count(), expected);
    }
    // A good frame after the storm clears the slate (counter is monotonic, but
    // the good snapshot refreshes and fallbacks stop).
    assert_eq!(store.resolve(good), LayoutResolution::Use(good));
    assert_eq!(store.fallback_count(), 3, "the counter never resets");
}

#[test]
fn the_first_fallback_notice_fires_exactly_once_per_session() {
    let store = LastGoodLayout::default();
    // No substitution yet: nothing to announce.
    assert!(!store.take_first_fallback_notice());

    let good = good_geometry(80, 24);
    let _ = store.resolve(good);
    let mut broken = good_geometry(80, 24);
    broken.input_height = 0;

    // First substitution: the one-shot cue is owed exactly once.
    let _ = store.resolve(broken);
    assert!(
        store.take_first_fallback_notice(),
        "first fallback is announced"
    );
    assert!(
        !store.take_first_fallback_notice(),
        "the notice is consumed and never re-armed"
    );

    // A later substitution storm never re-arms the cue.
    let _ = store.resolve(broken);
    assert!(
        !store.take_first_fallback_notice(),
        "subsequent fallbacks stay silent"
    );
}

// ---------------------------------------------------------------------------
// Diagnostics line.
// ---------------------------------------------------------------------------

#[test]
fn diagnostics_reports_none_before_the_first_good_frame() {
    let store = LastGoodLayout::default();
    let line = store.diagnostics_line();
    assert!(line.contains("good=none"), "{line}");
    assert!(line.contains("falls=0"), "{line}");
}

#[test]
fn diagnostics_reports_the_good_size_and_fallback_count() {
    let store = LastGoodLayout::default();
    let good = good_geometry(100, 30);
    let _ = store.resolve(good);
    let mut broken = good_geometry(100, 30);
    broken.input_height = 0;
    let _ = store.resolve(broken);

    let line = store.diagnostics_line();
    assert!(line.contains("good=100x30"), "{line}");
    assert!(line.contains("falls=1"), "{line}");
}

#[test]
fn reserved_height_sums_every_block_with_saturation() {
    let geom = LayoutGeometry {
        width: 80,
        height: 24,
        task_height: Some(3),
        approval_height: 2,
        plan_indicator_height: 1,
        subagent_height: 4,
        attachment_height: 2,
        transcript_prompt_gap_height: 1,
        transcript_height: 5,
        show_completed_turn_divider: false,
        input_height: 2,
    };
    assert_eq!(geom.reserved_height(), 3 + 2 + 1 + 4 + 2 + 1 + 5 + 2 + 2);

    // Saturating: an absurd combination can never wrap to a small number.
    let huge = LayoutGeometry {
        transcript_height: u16::MAX,
        input_height: u16::MAX,
        ..geom
    };
    assert_eq!(huge.reserved_height(), u16::MAX);
}
