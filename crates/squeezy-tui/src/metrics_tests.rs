//! Unit tests for the per-frame render-budget metrics primitives: the
//! per-frame accumulators (rows built, longest entry wrap) reset/record/read
//! cleanly, and the snapshot formatting (HUD lines, trace summary) is stable.
//!
//! These exercise the module-local helpers directly. The end-to-end stamping
//! (that `draw_app` populates a snapshot per painted frame and leaves idle
//! frames untouched) and the byte-counter wiring are covered by the integration
//! tests in `lib_tests.rs` against the capture-sink guard.

use super::*;
use std::time::Duration;

#[test]
fn fmt_dur_uses_micros_under_a_millisecond() {
    assert_eq!(fmt_dur(Duration::from_micros(0)), "0µs");
    assert_eq!(fmt_dur(Duration::from_micros(999)), "999µs");
}

#[test]
fn fmt_dur_uses_millis_at_and_above_one_millisecond() {
    assert_eq!(fmt_dur(Duration::from_micros(1000)), "1.0ms");
    assert_eq!(
        fmt_dur(Duration::from_millis(12) + Duration::from_micros(345)),
        "12.3ms"
    );
}

#[test]
fn rows_built_accumulator_resets_records_and_reads() {
    reset_rows_built();
    assert_eq!(rows_built(), 0, "reset zeroes the accumulator");
    record_rows_built(42);
    assert_eq!(rows_built(), 42, "record overwrites with the latest count");
    record_rows_built(7);
    assert_eq!(
        rows_built(),
        7,
        "the latest record wins (per-frame, not summed)"
    );
    reset_rows_built();
    assert_eq!(rows_built(), 0);
}

#[test]
fn longest_entry_wrap_tracks_running_max_until_reset() {
    reset_longest_entry_wrap();
    assert_eq!(longest_entry_wrap(), Duration::ZERO);
    record_entry_wrap(Duration::from_micros(100));
    record_entry_wrap(Duration::from_micros(500));
    record_entry_wrap(Duration::from_micros(300));
    assert_eq!(
        longest_entry_wrap(),
        Duration::from_micros(500),
        "the accumulator keeps the SLOWEST wrap, not the last"
    );
    reset_longest_entry_wrap();
    assert_eq!(
        longest_entry_wrap(),
        Duration::ZERO,
        "reset clears the running max for the next frame"
    );
}

#[test]
fn hud_lines_cover_every_metric_and_compute_hit_rate() {
    let m = RenderMetrics {
        render_time: Duration::from_micros(1500),
        bytes_emitted: 2048,
        rows_built: 130,
        cache_hits: 9,
        cache_misses: 1,
        longest_entry_wrap: Duration::from_micros(250),
        frame: 5,
    };
    let lines = m.hud_lines();
    let joined = lines.join("\n");
    assert!(joined.contains("frame   5"), "{joined}");
    assert!(joined.contains("render  1.5ms"), "{joined}");
    assert!(joined.contains("bytes   2048"), "{joined}");
    assert!(joined.contains("rows    130"), "{joined}");
    // 9 hits / 10 lookups = 90%.
    assert!(joined.contains("cache   9/10 (90%)"), "{joined}");
    assert!(joined.contains("wrap    250µs"), "{joined}");
}

#[test]
fn hud_lines_report_full_hit_rate_with_no_lookups() {
    // A frame that serves entirely from the assembled-render cache makes zero
    // sub-lookups; the HUD must not divide by zero.
    let m = RenderMetrics::default();
    let joined = m.hud_lines().join("\n");
    assert!(
        joined.contains("cache   0/0 (100%)"),
        "no lookups reads as a full hit rate, not NaN: {joined}"
    );
}

#[test]
fn trace_summary_is_a_single_structured_line() {
    let m = RenderMetrics {
        render_time: Duration::from_micros(800),
        bytes_emitted: 512,
        rows_built: 12,
        cache_hits: 3,
        cache_misses: 2,
        longest_entry_wrap: Duration::from_micros(40),
        frame: 99,
    };
    let s = m.trace_summary();
    assert!(
        !s.contains('\n'),
        "the trace summary is a single line: {s:?}"
    );
    for needle in [
        "frame=99",
        "render=800µs",
        "bytes=512",
        "rows=12",
        "cache_hits=3",
        "cache_misses=2",
        "longest_wrap=40µs",
    ] {
        assert!(s.contains(needle), "trace summary missing {needle:?}: {s}");
    }
}
