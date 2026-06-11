//! Unit tests for the Adaptive Density (§12.4.1) model: the size→tier resolver,
//! the explicit-override short-circuit, the slug/cycle round-trips, and the
//! per-tier spacing/chrome the renderer consumes.

use super::*;

// ---------------------------------------------------------------------------
// Auto resolution from terminal size.
// ---------------------------------------------------------------------------

#[test]
fn auto_resolves_compact_on_a_small_terminal() {
    // A genuinely narrow OR short terminal drops to compact — a single scarce
    // dimension is enough, because spacing must yield to content when cells are
    // tight. The thresholds are conservative: a standard 80-column / 24-row
    // terminal is NOT compact (see `auto_resolves_default_in_the_middle_band`).
    let narrow = DensityMode::Auto.resolve(50, 50);
    assert_eq!(narrow.tier(), DensityTier::Compact, "narrow forces compact");

    let short = DensityMode::Auto.resolve(200, 12);
    assert_eq!(short.tier(), DensityTier::Compact, "short forces compact");

    let both = DensityMode::Auto.resolve(50, 12);
    assert_eq!(both.tier(), DensityTier::Compact);
}

#[test]
fn auto_resolves_default_in_the_middle_band() {
    // Comfortable on both axes but not large enough on both for expanded.
    let mid = DensityMode::Auto.resolve(100, 30);
    assert_eq!(mid.tier(), DensityTier::Default);

    // A standard 80x24 terminal stays at Default — the layout an existing
    // session already gets, so this feature does not move it.
    assert_eq!(
        DensityMode::Auto.resolve(80, 24).tier(),
        DensityTier::Default,
        "the common 80x24 terminal is unchanged Default density",
    );

    // Wide but only mid-height stays default (expanded needs slack on BOTH axes).
    let wide_short = DensityMode::Auto.resolve(160, 30);
    assert_eq!(
        wide_short.tier(),
        DensityTier::Default,
        "a single large dimension is not enough for expanded",
    );

    // Tall but only mid-width stays default for the same reason.
    let tall_narrow = DensityMode::Auto.resolve(100, 60);
    assert_eq!(tall_narrow.tier(), DensityTier::Default);
}

#[test]
fn auto_resolves_expanded_only_when_both_dimensions_are_large() {
    let big = DensityMode::Auto.resolve(120, 40);
    assert_eq!(
        big.tier(),
        DensityTier::Expanded,
        "at the expanded thresholds on both axes",
    );

    let huge = DensityMode::Auto.resolve(220, 80);
    assert_eq!(huge.tier(), DensityTier::Expanded);
}

#[test]
fn auto_tier_boundaries_are_exact() {
    // Just below the compact ceiling on width.
    assert_eq!(
        DensityTier::for_size(59, 30),
        DensityTier::Compact,
        "59 cols is compact",
    );
    // Exactly at the compact-width floor, comfortable height → default.
    assert_eq!(
        DensityTier::for_size(60, 16),
        DensityTier::Default,
        "60x16 is the default floor, not compact",
    );
    // One row short of the compact-height floor (the renderer's historical
    // startup-card floor) → compact.
    assert_eq!(DensityTier::for_size(120, 15), DensityTier::Compact);
    // Exactly at the compact-height floor → default.
    assert_eq!(DensityTier::for_size(120, 16), DensityTier::Default);
    // Just below the expanded thresholds → default, not expanded.
    assert_eq!(DensityTier::for_size(119, 40), DensityTier::Default);
    assert_eq!(DensityTier::for_size(120, 39), DensityTier::Default);
    // Exactly at both expanded thresholds → expanded.
    assert_eq!(DensityTier::for_size(120, 40), DensityTier::Expanded);
}

#[test]
fn auto_degrades_gracefully_at_degenerate_sizes() {
    // A zero / one-cell terminal must not panic and must read as compact (the
    // safest, leanest tier).
    assert_eq!(DensityMode::Auto.resolve(0, 0).tier(), DensityTier::Compact);
    assert_eq!(DensityMode::Auto.resolve(1, 1).tier(), DensityTier::Compact);
    // The u16 ceiling resolves to expanded without overflow.
    assert_eq!(
        DensityMode::Auto.resolve(u16::MAX, u16::MAX).tier(),
        DensityTier::Expanded,
    );
}

// ---------------------------------------------------------------------------
// Explicit override short-circuits the size entirely.
// ---------------------------------------------------------------------------

#[test]
fn pinned_modes_ignore_the_terminal_size() {
    // Each pinned mode forces its tier regardless of how big the terminal is.
    for (cols, rows) in [(40u16, 12u16), (100, 30), (240, 90)] {
        assert_eq!(
            DensityMode::Compact.resolve(cols, rows).tier(),
            DensityTier::Compact,
        );
        assert_eq!(
            DensityMode::Default.resolve(cols, rows).tier(),
            DensityTier::Default,
        );
        assert_eq!(
            DensityMode::Expanded.resolve(cols, rows).tier(),
            DensityTier::Expanded,
        );
    }
}

#[test]
fn resolved_density_remembers_its_mode() {
    let resolved = DensityMode::Auto.resolve(50, 12);
    assert_eq!(resolved.mode(), DensityMode::Auto);
    assert_eq!(resolved.tier(), DensityTier::Compact);

    let pinned = DensityMode::Expanded.resolve(50, 12);
    assert_eq!(pinned.mode(), DensityMode::Expanded);
    assert_eq!(pinned.tier(), DensityTier::Expanded);
}

// ---------------------------------------------------------------------------
// Per-tier spacing / chrome the renderer consumes.
// ---------------------------------------------------------------------------

#[test]
fn gap_scale_grows_with_density() {
    assert_eq!(
        DensityMode::Compact.resolve(80, 24).transcript_prompt_gap(),
        0,
        "compact spends no gap",
    );
    assert_eq!(
        DensityMode::Default.resolve(80, 24).transcript_prompt_gap(),
        1,
        "default keeps the historical single-row gap",
    );
    assert_eq!(
        DensityMode::Expanded
            .resolve(80, 24)
            .transcript_prompt_gap(),
        2,
        "expanded doubles the breather",
    );
}

#[test]
fn startup_card_threshold_loosens_with_density() {
    // A compact (short) terminal hides the card sooner; expanded keeps it on a
    // slightly shorter window. Default preserves the renderer's historical 16.
    let compact = DensityMode::Compact
        .resolve(80, 24)
        .startup_card_min_height();
    let default = DensityMode::Default
        .resolve(80, 24)
        .startup_card_min_height();
    let expanded = DensityMode::Expanded
        .resolve(80, 24)
        .startup_card_min_height();
    assert_eq!(default, 16, "default keeps the renderer's prior threshold");
    assert!(
        compact > default,
        "compact requires MORE height before showing the card ({compact} vs {default})",
    );
    assert!(
        expanded < default,
        "expanded shows the card on a shorter window ({expanded} vs {default})",
    );
}

#[test]
fn status_detail_is_shown_except_on_compact() {
    assert!(!DensityMode::Compact.resolve(80, 24).shows_status_detail());
    assert!(DensityMode::Default.resolve(80, 24).shows_status_detail());
    assert!(DensityMode::Expanded.resolve(80, 24).shows_status_detail());
}

#[test]
fn describe_names_the_resolved_tier_for_auto_only() {
    // Auto exposes what it chose so the user can see it.
    assert_eq!(
        DensityMode::Auto.resolve(50, 12).describe(),
        "auto (compact)",
    );
    assert_eq!(
        DensityMode::Auto.resolve(100, 30).describe(),
        "auto (default)",
    );
    assert_eq!(
        DensityMode::Auto.resolve(160, 50).describe(),
        "auto (expanded)",
    );
    // A pinned mode reads as just its label (the tier is implied).
    assert_eq!(DensityMode::Compact.resolve(160, 50).describe(), "compact");
    assert_eq!(DensityMode::Expanded.resolve(40, 10).describe(), "expanded");
}

// ---------------------------------------------------------------------------
// Slug + cycle round-trips (persistence + keybinding plumbing).
// ---------------------------------------------------------------------------

#[test]
fn slug_round_trips_for_every_mode() {
    for &mode in DensityMode::ALL {
        assert_eq!(
            DensityMode::from_slug(mode.as_str()),
            Some(mode),
            "{} round-trips through its slug",
            mode.label(),
        );
    }
}

#[test]
fn from_slug_is_forgiving_and_bounded() {
    // Whitespace + case are tolerated on a hand-edited config.
    assert_eq!(
        DensityMode::from_slug("  Compact "),
        Some(DensityMode::Compact)
    );
    assert_eq!(
        DensityMode::from_slug("EXPANDED"),
        Some(DensityMode::Expanded)
    );
    // Unknown slugs collapse to None so the caller keeps the built-in default.
    assert_eq!(DensityMode::from_slug("roomy"), None);
    assert_eq!(DensityMode::from_slug(""), None);
}

#[test]
fn next_walks_every_mode_and_wraps() {
    // The cycle visits all four modes and returns to the start.
    let mut seen = Vec::new();
    let mut mode = DensityMode::Auto;
    for _ in 0..DensityMode::ALL.len() {
        seen.push(mode);
        mode = mode.next();
    }
    assert_eq!(mode, DensityMode::Auto, "the cycle wraps to the start");
    // Every mode appeared exactly once over one full lap.
    for &m in DensityMode::ALL {
        assert_eq!(
            seen.iter().filter(|s| **s == m).count(),
            1,
            "{} appears exactly once per lap",
            m.label(),
        );
    }
}

#[test]
fn all_array_lists_each_mode_once() {
    // Sanity-check the ALL table feeds the persistence/cycle plumbing with no
    // duplicates or gaps.
    assert_eq!(DensityMode::ALL.len(), 4);
    for &mode in DensityMode::ALL {
        assert_eq!(DensityMode::ALL.iter().filter(|m| **m == mode).count(), 1);
    }
}

#[test]
fn at_least_expanded_lifts_lower_tiers_and_keeps_the_mode() {
    // Presentation Mode (§12.4.6) reuses this to force the spacious layout. A
    // compact / default density is lifted to expanded; an already-expanded one is
    // unchanged. The originating mode is always preserved so a status readout
    // still names what the user picked.
    let compact = DensityMode::Compact.resolve(80, 24);
    assert_eq!(compact.tier(), DensityTier::Compact);
    let lifted = compact.at_least_expanded();
    assert_eq!(lifted.tier(), DensityTier::Expanded);
    assert_eq!(lifted.mode(), DensityMode::Compact, "mode is preserved");

    let default = DensityMode::Default.resolve(80, 24);
    assert_eq!(default.at_least_expanded().tier(), DensityTier::Expanded);

    // Already expanded → unchanged (idempotent).
    let expanded = DensityMode::Expanded.resolve(80, 24);
    let twice = expanded.at_least_expanded();
    assert_eq!(twice.tier(), DensityTier::Expanded);
    assert_eq!(twice.mode(), DensityMode::Expanded);
}

#[test]
fn tiers_order_compact_to_expanded() {
    // The derived ordering reads "more roomy = greater", which the renderer's
    // `>=` comparisons rely on.
    assert!(DensityTier::Compact < DensityTier::Default);
    assert!(DensityTier::Default < DensityTier::Expanded);
}
