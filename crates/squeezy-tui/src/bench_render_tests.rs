use std::sync::{Arc, Mutex};

use ratatui::layout::Rect;
use ratatui::{Terminal, TerminalOptions, Viewport, backend::CrosstermBackend};

use super::*;
use crate::terminal_writer::TerminalWriter;

/// A representative spread of terminal sizes used across these tests: the
/// classic 80x24, a wide layout, and a deliberately tiny one that still has to
/// paint a composer. Every benchmark dimension a scenario can land on.
const SIZES: &[BenchSize] = &[
    BenchSize::new(80, 24),
    BenchSize::new(160, 48),
    BenchSize::new(40, 10),
];

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

#[test]
fn percentile_nearest_rank_picks_expected_values() {
    let sorted = [10u128, 20, 30, 40, 50];
    // p95 of 5 samples: rank = ceil(0.95*5) = 5 -> last element.
    assert_eq!(percentile(&sorted, 95), 50);
    // p50 -> rank = ceil(2.5) = 3 -> the median.
    assert_eq!(percentile(&sorted, 50), 30);
    // p100 -> the max; p1 -> the min.
    assert_eq!(percentile(&sorted, 100), 50);
    assert_eq!(percentile(&sorted, 1), 10);
}

#[test]
fn percentile_of_empty_is_zero() {
    // Edge case: no frames means no percentile to report. Must not panic.
    assert_eq!(percentile(&[], 95), 0);
}

#[test]
fn scenario_slugs_are_unique_and_stable() {
    let mut slugs: Vec<&str> = BenchScenario::ALL.iter().map(|s| s.slug()).collect();
    let count = slugs.len();
    slugs.sort_unstable();
    slugs.dedup();
    assert_eq!(slugs.len(), count, "scenario slugs must be unique");
    // ALL must list exactly the variants the suite sweeps.
    assert_eq!(count, 5);
}

#[test]
fn count_glyph_counts_only_the_target_symbol() {
    // A 4x2 buffer with two target glyphs placed at known cells.
    let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 2));
    buffer[(0, 0)].set_symbol("┃");
    buffer[(3, 1)].set_symbol("┃");
    buffer[(1, 0)].set_symbol("x");
    assert_eq!(count_glyph(&buffer, '┃'), 2);
    assert_eq!(count_glyph(&buffer, 'x'), 1);
    assert_eq!(count_glyph(&buffer, 'z'), 0);
}

// ---------------------------------------------------------------------------
// Scenario builders
// ---------------------------------------------------------------------------

#[test]
fn empty_scenario_builds_an_empty_transcript() {
    let app = BenchScenario::Empty.build_app(BenchSize::new(80, 24));
    assert!(app.transcript.is_empty(), "empty scenario has no entries");
    assert!(
        app.input.is_empty(),
        "empty scenario starts with empty composer"
    );
}

#[test]
fn long_session_overflows_the_viewport() {
    let size = BenchSize::new(80, 24);
    let app = BenchScenario::LongSession.build_app(size);
    let lines = transcript_lines_for_render(&app, Some(size.width), true).len();
    assert!(
        lines > size.height as usize,
        "long session must overflow viewport ({} lines vs {} rows)",
        lines,
        size.height
    );
    // A fresh long session still follows the tail.
    assert!(active_transcript_scroll(&app).is_following());
}

#[test]
fn scrolled_scenario_leaves_the_tail() {
    let size = BenchSize::new(80, 24);
    let app = BenchScenario::ScrolledLongSession.build_app(size);
    assert!(
        !active_transcript_scroll(&app).is_following(),
        "scrolled scenario must not follow the tail"
    );
}

// ---------------------------------------------------------------------------
// run_scenario — drives the real render() through the capture-sink seam
// ---------------------------------------------------------------------------

#[test]
fn capture_stream_emits_real_bytes_for_a_painted_scenario() {
    // The integration test of record: a non-empty scenario rendered through
    // `CrosstermBackend<TerminalWriter::capture>` must emit a non-empty ANSI
    // byte stream and pass every invariant.
    let summary = run_scenario(
        BenchScenario::ShortChat,
        BenchBackend::CaptureStream,
        BenchSize::new(80, 24),
    )
    .expect("short chat must pass invariants");

    assert_eq!(summary.scenario, "short_chat");
    assert_eq!(summary.backend, "capture_stream");
    assert_eq!(summary.frames, FRAMES_PER_RUN);
    assert!(
        summary.total_bytes > 0,
        "the capture leg must emit real bytes, got {}",
        summary.total_bytes
    );
    assert!(
        summary.mean_bytes_per_frame > 0,
        "mean bytes per frame must be positive"
    );
    assert!(
        summary.teardown_clean,
        "a settled session must tear down cleanly"
    );
}

#[test]
fn cells_backend_reports_no_bytes_but_full_frames() {
    // The cell leg has no byte stream, so it reports zero bytes — but it still
    // paints every frame and runs the invariant checks.
    let summary = run_scenario(
        BenchScenario::ShortChat,
        BenchBackend::Cells,
        BenchSize::new(80, 24),
    )
    .expect("cells leg must pass invariants");
    assert_eq!(summary.backend, "cells");
    assert_eq!(summary.total_bytes, 0);
    assert_eq!(summary.frames, FRAMES_PER_RUN);
}

#[test]
fn cells_backend_never_touches_the_capture_stream() {
    // The `cells` backend must time and read *its own* (TestBackend) draw, so it
    // must never render the capture-stream leg. If it did, the `cells` summary's
    // latency would be a second sample of the capture-stream backend rather than
    // the cell renderer's. We observe this from the empty capture sink: a `Cells`
    // run leaves it untouched.
    let sink: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let summary = run_scenario_into_sink(
        BenchScenario::ShortChat,
        BenchBackend::Cells,
        BenchSize::new(80, 24),
        Arc::clone(&sink),
    )
    .expect("cells leg must pass invariants");
    assert_eq!(summary.backend, "cells");
    assert_eq!(
        sink.lock().unwrap().len(),
        0,
        "the cells backend must not draw the capture-stream leg"
    );

    // Sanity contrast: the capture-stream backend *does* fill the same sink, so
    // the assertion above is meaningful (the scenario genuinely emits bytes).
    let stream_sink: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    run_scenario_into_sink(
        BenchScenario::ShortChat,
        BenchBackend::CaptureStream,
        BenchSize::new(80, 24),
        Arc::clone(&stream_sink),
    )
    .expect("capture leg must pass invariants");
    assert!(
        !stream_sink.lock().unwrap().is_empty(),
        "the capture-stream backend must fill the sink"
    );
}

#[test]
fn empty_scenario_passes_invariants_at_every_size() {
    // Edge case: an empty transcript still paints exactly one composer at
    // every size, including the tiny 40x10.
    for &size in SIZES {
        let summary = run_scenario(BenchScenario::Empty, BenchBackend::CaptureStream, size)
            .unwrap_or_else(|e| panic!("empty scenario at {}x{}: {e}", size.width, size.height));
        assert_eq!(summary.scenario, "empty");
        assert_eq!(summary.scroll_jumps, 0, "empty transcript cannot scroll");
        assert!(summary.teardown_clean);
    }
}

#[test]
fn steady_state_frames_emit_fewer_bytes_than_the_first_full_paint() {
    // The diffing renderer's whole point: after the first full paint, an
    // unchanged frame diffs to almost nothing. Drive the capture seam by hand
    // so we can compare the first frame against a later one.
    let app = BenchScenario::LongSession.build_app(BenchSize::new(80, 24));
    let sink: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(TerminalWriter::capture(Arc::clone(&sink))),
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, 80, 24)),
        },
    )
    .expect("terminal");

    let first = {
        let before = sink.lock().unwrap().len();
        terminal.draw(|frame| render(frame, &app)).expect("draw 1");
        sink.lock().unwrap().len() - before
    };
    let second = {
        let before = sink.lock().unwrap().len();
        terminal.draw(|frame| render(frame, &app)).expect("draw 2");
        sink.lock().unwrap().len() - before
    };
    assert!(first > 0, "first frame is a full paint");
    assert!(
        second < first,
        "an unchanged second frame must diff to fewer bytes than the first full paint \
         (first={first}, second={second})"
    );
}

#[test]
fn scroll_jumps_are_counted_for_an_overflowing_session() {
    // A long session has lines off the top, so the scripted scroll loop
    // registers real jumps; the empty scenario registers none.
    let size = BenchSize::new(80, 24);
    let long = run_scenario(BenchScenario::LongSession, BenchBackend::Cells, size)
        .expect("long session invariants");
    assert!(
        long.scroll_jumps > 0,
        "an overflowing session must register scroll jumps, got {}",
        long.scroll_jumps
    );
}

// ---------------------------------------------------------------------------
// Resize — the feature paints across sizes
// ---------------------------------------------------------------------------

#[test]
fn every_scenario_passes_invariants_across_a_resize_sweep() {
    // Resize coverage: paint every scenario at three very different
    // geometries. A surface that clips, duplicates the composer, or drops the
    // tail at any size fails its invariant here.
    for &scenario in &BenchScenario::ALL {
        for &size in SIZES {
            run_scenario(scenario, BenchBackend::CaptureStream, size).unwrap_or_else(|e| {
                panic!(
                    "{} at {}x{} violated an invariant: {e}",
                    scenario.slug(),
                    size.width,
                    size.height
                )
            });
        }
    }
}

#[test]
fn invariant_error_displays_scenario_and_frame() {
    // The Display impl is what a failing CI run reads; pin its shape.
    let err = BenchInvariantError {
        scenario: "short_chat",
        frame: 3,
        message: "boom".to_string(),
    };
    let text = err.to_string();
    assert!(text.contains("short_chat"), "{text}");
    assert!(text.contains("frame 3"), "{text}");
    assert!(text.contains("boom"), "{text}");
}

// ---------------------------------------------------------------------------
// Suite + JSON
// ---------------------------------------------------------------------------

#[test]
fn run_suite_produces_one_summary_per_cell() {
    // scenarios x backends x sizes.
    let sizes = [BenchSize::new(80, 24), BenchSize::new(120, 30)];
    let summaries = run_suite(&sizes).expect("suite must pass every invariant");
    let expected = BenchScenario::ALL.len() * 2 * sizes.len();
    assert_eq!(summaries.len(), expected);
}

#[test]
fn summaries_serialize_to_json_with_the_documented_fields() {
    let sizes = [BenchSize::new(80, 24)];
    let summaries = run_suite(&sizes).expect("suite");
    let json = summaries_to_json(&summaries);
    // The spec's JSON shape: scenario / backend / size / frames / bytes /
    // p95 / scroll jumps / teardown all present.
    for field in [
        "\"scenario\"",
        "\"backend\"",
        "\"width\"",
        "\"height\"",
        "\"frames\"",
        "\"total_bytes\"",
        "\"p95_render_micros\"",
        "\"scroll_jumps\"",
        "\"teardown_clean\"",
    ] {
        assert!(json.contains(field), "JSON missing {field}:\n{json}");
    }
    // And it round-trips back to a value array of the right length.
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
    assert_eq!(
        parsed.as_array().map(|a| a.len()),
        Some(BenchScenario::ALL.len() * 2)
    );
}

// ---------------------------------------------------------------------------
// Teardown invariant
// ---------------------------------------------------------------------------

#[test]
fn teardown_is_clean_for_a_settled_session() {
    let app = BenchScenario::ShortChat.build_app(BenchSize::new(80, 24));
    assert!(
        verify_teardown(&app, 80),
        "a settled session must emit LeaveAlternateScreen before its mirror, no \\x1b[3J"
    );
}

#[test]
fn teardown_is_clean_for_an_empty_session() {
    // Edge case: an empty transcript has no mirror rows, but the leave alone
    // is still a valid, clean teardown.
    let app = BenchScenario::Empty.build_app(BenchSize::new(80, 24));
    assert!(verify_teardown(&app, 80));
}
