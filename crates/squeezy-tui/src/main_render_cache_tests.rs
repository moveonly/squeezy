//! Unit tests for the main-render cache primitives: hit/miss behaviour, LRU
//! bounds under a resize storm, per-entry wrap reuse, and bounded memory under
//! stress. These exercise the cache APIs directly with synthetic keys/lines so
//! they never collide with the integration tests in `lib_tests.rs` (which drive
//! the real `transcript_lines_and_entry_offsets` path through a `TuiApp`).
//!
//! Each test mints a private `render_cache_session` via `next_session_id` so
//! parallel `cargo test` runs sharing the process-wide LRU cannot clobber each
//! other's slots — the same isolation the production session id provides.
//!
//! Hit/miss is asserted via a *local* compute-call counter (a `Cell` the
//! `compute` closure bumps), not the global atomic stat counters: those
//! counters are process-wide and race under parallel test execution, so they
//! are only asserted for monotonicity, never exact deltas.

use super::*;
use crate::render::cache::next_session_id;
use std::cell::Cell;

/// Serialize cache-behaviour tests on the crate-shared lock (see
/// `main_render_cache::test_lock`) so unit and integration tests never flood
/// the small process-wide LRU concurrently. Each test still mints a private
/// session so values never collide; the lock only orders the floods.
fn lock_cache() -> std::sync::MutexGuard<'static, ()> {
    crate::main_render_cache::test_lock()
}

fn key_at(session: u64, width: u16) -> MainRenderKey {
    MainRenderKey {
        render_cache_session: session,
        width,
        selected_entry: None,
        tool_output_verbosity: 0,
        show_reasoning_usage: false,
        coalesce_tool_runs: false,
        palette_generation: 0,
        subagent_source_hash: 0,
        transcript_revision_hash: 0,
        pending_hash: 0,
        turn_divider_hash: 0,
        shortcut_hash: 0,
        tail_anim_phase: 0,
        include_startup_card: false,
        semantic_filter_hash: 0,
    }
}

fn value(tag: &str) -> (Vec<Line<'static>>, Vec<usize>, Vec<crate::search::RowKind>) {
    (
        vec![Line::from(tag.to_string())],
        vec![0],
        vec![crate::search::RowKind::Normal],
    )
}

fn line_text(line: &Line<'static>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

#[test]
fn main_cache_hits_on_repeat_key() {
    let _guard = lock_cache();
    let session = next_session_id();
    let key = key_at(session, 80);
    let computes = Cell::new(0u32);

    let first = get_or_compute_main(key.clone(), || {
        computes.set(computes.get() + 1);
        value("first")
    });
    assert_eq!(line_text(&first.0[0]), "first");
    assert_eq!(computes.get(), 1, "first lookup runs compute");
    assert!(contains_main_key(&key), "first lookup populated the slot");

    // The unchanged second frame returns the cached value WITHOUT running compute.
    let second = get_or_compute_main(key, || {
        computes.set(computes.get() + 1);
        value("MISS")
    });
    assert_eq!(
        line_text(&second.0[0]),
        "first",
        "the unchanged second frame reuses the cached rows"
    );
    assert_eq!(computes.get(), 1, "the second frame runs no compute");

    // The global counters at least moved in the right direction.
    let (hits, misses, _, _) = cache_stats();
    assert!(hits >= 1 && misses >= 1);
}

#[test]
fn main_cache_invalidates_on_each_key_dimension() {
    let _guard = lock_cache();
    let session = next_session_id();
    // Each variant flips exactly one key field; every one must run compute
    // (miss) because every field can change the produced rows.
    let base = key_at(session, 80);
    let variants: Vec<MainRenderKey> = vec![
        MainRenderKey {
            width: 81,
            ..base.clone()
        },
        MainRenderKey {
            selected_entry: Some(0),
            ..base.clone()
        },
        MainRenderKey {
            tool_output_verbosity: 1,
            ..base.clone()
        },
        MainRenderKey {
            show_reasoning_usage: true,
            ..base.clone()
        },
        MainRenderKey {
            coalesce_tool_runs: true,
            ..base.clone()
        },
        MainRenderKey {
            palette_generation: 1,
            ..base.clone()
        },
        MainRenderKey {
            subagent_source_hash: 1,
            ..base.clone()
        },
        MainRenderKey {
            transcript_revision_hash: 1,
            ..base.clone()
        },
        MainRenderKey {
            pending_hash: 1,
            ..base.clone()
        },
        MainRenderKey {
            turn_divider_hash: 1,
            ..base.clone()
        },
        MainRenderKey {
            shortcut_hash: 1,
            ..base.clone()
        },
        MainRenderKey {
            include_startup_card: true,
            ..base.clone()
        },
    ];

    // Seed the base key.
    let _ = get_or_compute_main(base.clone(), || value("base"));
    for variant in variants {
        assert_ne!(variant, base, "variant must differ from base");
        let computes = Cell::new(0u32);
        let _ = get_or_compute_main(variant.clone(), || {
            computes.set(computes.get() + 1);
            value("variant")
        });
        assert_eq!(
            computes.get(),
            1,
            "a changed key dimension must miss and recompute: {variant:?}"
        );
    }
}

#[test]
fn main_cache_lru_bounded_under_resize_storm() {
    let _guard = lock_cache();
    let session = next_session_id();
    let cap = main_render_capacity();
    // Walk many more distinct widths than the cap — a resize drag. The LRU must
    // never exceed its bound regardless of how many widths we visit.
    for width in 0..(cap as u16 * 4) {
        let _ = get_or_compute_main(key_at(session, width + 1), || value("w"));
        assert!(
            main_render_len() <= cap,
            "main render cache exceeded its LRU bound during a resize storm"
        );
    }
    assert!(
        main_render_len() <= cap,
        "main render cache stayed within its LRU bound"
    );
}

#[test]
fn main_cache_evicts_oldest_not_most_recent() {
    let _guard = lock_cache();
    // The eviction-ORDER proof: insert two of our keys, keep re-touching the
    // FIRST so it stays most-recently-used while we push many fresh keys past
    // the cap. The continuously-touched key must survive; an untouched early key
    // must not. Serialized via `lock_cache` so no other test floods the shared
    // LRU mid-proof.
    let session = next_session_id();
    let cap = main_render_capacity();
    let kept = key_at(session, 1);
    let doomed = key_at(session, 2);
    let _ = get_or_compute_main(kept.clone(), || value("kept"));
    let _ = get_or_compute_main(doomed.clone(), || value("doomed"));
    // Push well past the cap, re-touching `kept` each round so it never becomes
    // the LRU victim. `doomed` is never touched again.
    for width in 3..(cap as u16 * 3) {
        let _ = get_or_compute_main(key_at(session, width), || value("filler"));
        let _ = get_or_compute_main(kept.clone(), || value("MISS-kept"));
    }
    // Touch `kept` once more immediately before the check so it is the single
    // most-recently-used slot — robust even if a concurrent session inserts a
    // burst between this and the assertion.
    let _ = get_or_compute_main(kept.clone(), || value("MISS-kept"));
    assert!(
        contains_main_key(&kept),
        "a continuously-touched key survives eviction (LRU, not FIFO)"
    );
    assert!(
        !contains_main_key(&doomed),
        "an untouched early key is evicted once the cap is exceeded"
    );
    // Re-inserting the evicted key runs compute again.
    let computes = Cell::new(0u32);
    let _ = get_or_compute_main(doomed, || {
        computes.set(computes.get() + 1);
        value("recompute")
    });
    assert_eq!(computes.get(), 1, "the evicted key recomputes");
}

#[test]
fn entry_wrap_cache_reuses_unchanged_entry() {
    let _guard = lock_cache();
    let session = next_session_id();
    let entry_id = 7;
    let computes = Cell::new(0u32);
    let first = get_or_compute_entry_wrap(session, entry_id, 0, 80, 0xABCD, 0, || {
        computes.set(computes.get() + 1);
        vec![Line::from("wrapped")]
    });
    assert_eq!(line_text(&first[0]), "wrapped");
    assert_eq!(computes.get(), 1, "first wrap of an entry runs compute");

    // Same entry, same width/detail/revision/palette: hit, no recompute.
    let second = get_or_compute_entry_wrap(session, entry_id, 0, 80, 0xABCD, 0, || {
        computes.set(computes.get() + 1);
        vec![Line::from("MISS")]
    });
    assert_eq!(line_text(&second[0]), "wrapped");
    assert_eq!(computes.get(), 1, "unchanged entry does not re-wrap");
}

#[test]
fn entry_wrap_cache_busts_on_revision_width_detail_and_theme() {
    let _guard = lock_cache();
    let session = next_session_id();
    let id = 11;
    let _ = get_or_compute_entry_wrap(session, id, 0, 80, 1, 0, || vec![Line::from("v0")]);

    // Each variant flips exactly one validity dimension and must re-wrap.
    let cases: &[(u64, u16, u64, u64, &str)] = &[
        (1, 80, 1, 0, "revision bump"),
        (0, 100, 1, 0, "width change"),
        (0, 80, 2, 0, "detail change"),
        (0, 80, 1, 1, "theme change"),
    ];
    for &(rev, width, detail, palette, label) in cases {
        let computes = Cell::new(0u32);
        let _ = get_or_compute_entry_wrap(session, id, rev, width, detail, palette, || {
            computes.set(computes.get() + 1);
            vec![Line::from(label.to_string())]
        });
        assert_eq!(computes.get(), 1, "{label} must re-wrap");
    }
}

#[test]
fn entry_wrap_two_widths_mint_separate_slots() {
    // Regression for deep-review #47: `width` and `detail_hash` are part of the
    // EntryWrapKey, NOT validity tags — so the same entry wrapped at two widths
    // occupies TWO slots that coexist, contradicting the old doc's claim that a
    // re-wrap "replaces its one slot". Proven session-locally (race-free under
    // the shared process-wide LRU, which a sibling test could otherwise grow or
    // evict mid-check): after wrapping the same entry at width 80 then width 100,
    // re-requesting BOTH widths must HIT (no recompute), which is only possible
    // if neither slot replaced the other.
    let _guard = lock_cache();
    let session = next_session_id();
    let id = 31;

    let _ = get_or_compute_entry_wrap(session, id, 0, 80, 0, 0, || vec![Line::from("w80")]);
    let _ = get_or_compute_entry_wrap(session, id, 0, 100, 0, 0, || vec![Line::from("w100")]);

    // The width-80 slot survived the width-100 wrap: re-requesting it is a hit
    // that still returns the width-80 content (not "w100"), proving the two
    // widths coexist as separate slots rather than one replacing the other.
    let computes = Cell::new(0u32);
    let at80 = get_or_compute_entry_wrap(session, id, 0, 80, 0, 0, || {
        computes.set(computes.get() + 1);
        vec![Line::from("MISS-80")]
    });
    let at100 = get_or_compute_entry_wrap(session, id, 0, 100, 0, 0, || {
        computes.set(computes.get() + 1);
        vec![Line::from("MISS-100")]
    });
    assert_eq!(line_text(&at80[0]), "w80");
    assert_eq!(line_text(&at100[0]), "w100");
    assert_eq!(
        computes.get(),
        0,
        "neither width's slot was replaced by the other — both stay resident"
    );
}

#[test]
fn entry_wrap_cache_bounded_under_thousands_of_entries_and_resizes() {
    let _guard = lock_cache();
    let session = next_session_id();
    let cap = entry_wrap_capacity();
    // Stress: thousands of distinct entries across several widths (a resize
    // storm over a very long transcript). The per-entry wrap cache must stay
    // bounded — memory never grows past the LRU cap no matter how many
    // (entry, width) pairs we touch.
    let entries = 4000u64;
    for width in [60u16, 80, 100] {
        for id in 0..entries {
            let _ =
                get_or_compute_entry_wrap(session, id, 0, width, 0, 0, || vec![Line::from("x")]);
            assert!(
                entry_wrap_len() <= cap,
                "per-entry wrap cache exceeded its LRU bound under stress"
            );
        }
    }
    assert!(entry_wrap_len() <= cap);
}

#[test]
fn very_long_single_entry_stays_bounded() {
    let _guard = lock_cache();
    let session = next_session_id();
    let cap = entry_wrap_capacity();
    // A single very long entry that re-wraps across many widths: one slot per
    // (entry, width) — still bounded by the LRU cap.
    let huge: Vec<Line<'static>> = (0..20_000)
        .map(|i| Line::from(format!("long line {i} with enough text to wrap somewhere")))
        .collect();
    for width in 1..=(cap as u16 + 50) {
        let block = huge.clone();
        let _ = get_or_compute_entry_wrap(session, 0, 0, width, 0, 0, move || block.to_vec());
        assert!(
            entry_wrap_len() <= cap,
            "a very long single entry across many widths stayed bounded"
        );
    }
}
