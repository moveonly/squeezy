//! Unit tests for the Live Review Board model (§12.8.5): lane classification,
//! lane-major card layout, attention counting, stable-id + flattened navigation,
//! the incremental-rebuild fast path, and the summary line. Pure model tests — no
//! terminal. The render path (keyboard/mouse/resize through the real `render()`)
//! is covered by the integration tests in `lib_tests.rs`.

use super::*;
use crate::subagent_timeline::{SubagentTimelineSource, SubagentTimelineStatus};

/// A source row with the given id/status; metrics default to a small spread so the
/// `elapsed`/`cost`/`tools` formatting can be asserted where it matters.
fn source(id: u64, agent: &str, status: SubagentTimelineStatus) -> SubagentTimelineSource {
    SubagentTimelineSource {
        id,
        agent: agent.to_string(),
        status,
        latest: format!("activity for {agent}"),
        elapsed_secs: Some(75),
        tool_count: 3,
        cost_micros: Some(1_500_000),
    }
}

/// A representative fan-out: two running, one completed, one failed, one capped —
/// the spread the board groups into its lanes.
fn fanout() -> Vec<SubagentTimelineSource> {
    vec![
        source(1, "explore", SubagentTimelineStatus::Completed),
        source(2, "delegate", SubagentTimelineStatus::Running),
        source(3, "reviewer", SubagentTimelineStatus::Failed),
        source(4, "builder", SubagentTimelineStatus::Running),
        source(5, "extra", SubagentTimelineStatus::Rejected),
    ]
}

#[test]
fn classify_maps_each_real_status_to_an_honest_lane() {
    assert_eq!(
        ReviewLane::classify(SubagentTimelineStatus::Running),
        ReviewLane::Running,
    );
    assert_eq!(
        ReviewLane::classify(SubagentTimelineStatus::Completed),
        ReviewLane::Completed,
    );
    assert_eq!(
        ReviewLane::classify(SubagentTimelineStatus::Failed),
        ReviewLane::Blocked,
    );
    // The spec's load-bearing rule: a cap rejection is *Capped*, NEVER inferred as
    // a runtime "queued" worker.
    assert_eq!(
        ReviewLane::classify(SubagentTimelineStatus::Rejected),
        ReviewLane::Capped,
    );
}

#[test]
fn capped_and_blocked_are_the_attention_lanes() {
    assert!(ReviewLane::Blocked.is_attention());
    assert!(ReviewLane::Capped.is_attention());
    assert!(!ReviewLane::Running.is_attention());
    assert!(!ReviewLane::Completed.is_attention());
}

#[test]
fn lane_gloss_distinguishes_blocked_from_capped() {
    // The two attention lanes look like "failure" but imply different
    // remediations; the gloss must spell out the distinction.
    assert_eq!(ReviewLane::Blocked.gloss(), "Blocked — ran and failed");
    assert_eq!(
        ReviewLane::Capped.gloss(),
        "Capped — refused before start (concurrency cap)"
    );
    assert!(ReviewLane::Running.gloss().starts_with("Running"));
    assert!(ReviewLane::Completed.gloss().starts_with("Completed"));
}

#[test]
fn rebuild_groups_cards_lane_major_in_board_order() {
    let sources = fanout();
    let mut board = ReviewBoard::new();
    let fp = ReviewBoard::fingerprint_of(sources.iter());
    assert!(board.rebuild_if_stale(fp, &sources), "first build runs");

    // Lane-major order: Running (ids 2,4 in record order), Blocked (3), Capped (5),
    // Completed (1).
    let lanes: Vec<ReviewLane> = board.cards().iter().map(|c| c.lane).collect();
    assert_eq!(
        lanes,
        vec![
            ReviewLane::Running,
            ReviewLane::Running,
            ReviewLane::Blocked,
            ReviewLane::Capped,
            ReviewLane::Completed,
        ],
    );
    let ids: Vec<u64> = board.cards().iter().map(|c| c.id).collect();
    assert_eq!(
        ids,
        vec![2, 4, 3, 5, 1],
        "lane-major, record order within lane"
    );
}

#[test]
fn ordinal_tracks_original_record_position_not_board_position() {
    let sources = fanout();
    let mut board = ReviewBoard::new();
    let fp = ReviewBoard::fingerprint_of(sources.iter());
    board.rebuild_if_stale(fp, &sources);

    // Card for id 5 (the capped row) is at board index 3 but is the 5th record, so
    // its ordinal is 5 (matching the Subagent Timeline Panel's `agent #ordinal`).
    let capped = board
        .cards()
        .iter()
        .find(|c| c.id == 5)
        .expect("capped card");
    assert_eq!(capped.ordinal, 5, "ordinal is the original record position");
    // The first running card (id 2) is the 2nd record.
    let running = board
        .cards()
        .iter()
        .find(|c| c.id == 2)
        .expect("running card");
    assert_eq!(running.ordinal, 2);
}

#[test]
fn counts_present_lanes_and_attention() {
    let sources = fanout();
    let mut board = ReviewBoard::new();
    board.rebuild_if_stale(ReviewBoard::fingerprint_of(sources.iter()), &sources);

    assert_eq!(board.len(), 5);
    assert_eq!(board.count_in(ReviewLane::Running), 2);
    assert_eq!(board.count_in(ReviewLane::Blocked), 1);
    assert_eq!(board.count_in(ReviewLane::Capped), 1);
    assert_eq!(board.count_in(ReviewLane::Completed), 1);
    // Every lane has a worker, so all four are present.
    assert_eq!(board.present_lanes(), ReviewLane::ALL.to_vec());
    // Failed + capped want a look.
    assert_eq!(board.attention_count(), 2);

    // cards_in returns the lane's cards in record order.
    let running_ids: Vec<u64> = board
        .cards_in(ReviewLane::Running)
        .iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(running_ids, vec![2, 4]);
}

#[test]
fn empty_lane_is_absent_from_present_lanes_but_board_still_builds() {
    // Only running + completed workers — Blocked and Capped lanes are empty.
    let sources = vec![
        source(1, "a", SubagentTimelineStatus::Running),
        source(2, "b", SubagentTimelineStatus::Completed),
    ];
    let mut board = ReviewBoard::new();
    board.rebuild_if_stale(ReviewBoard::fingerprint_of(sources.iter()), &sources);

    assert_eq!(
        board.present_lanes(),
        vec![ReviewLane::Running, ReviewLane::Completed],
    );
    assert_eq!(board.count_in(ReviewLane::Blocked), 0);
    assert_eq!(board.count_in(ReviewLane::Capped), 0);
    assert_eq!(board.attention_count(), 0);
}

#[test]
fn empty_board_is_empty_and_summary_is_blank() {
    let mut board = ReviewBoard::new();
    assert!(board.rebuild_if_stale(ReviewBoard::fingerprint_of([].iter()), &[]));
    assert!(board.is_empty());
    assert_eq!(board.len(), 0);
    assert!(board.summary().is_empty());
    assert!(board.present_lanes().is_empty());
    // Navigation over an empty board never panics and yields nothing.
    assert_eq!(board.next_index(None), None);
    assert_eq!(board.prev_index(None), None);
    assert_eq!(board.index_of(1), None);
    assert_eq!(board.card_at(0), None);
}

#[test]
fn navigation_walks_flattened_board_and_wraps() {
    let sources = fanout();
    let mut board = ReviewBoard::new();
    board.rebuild_if_stale(ReviewBoard::fingerprint_of(sources.iter()), &sources);

    // Forward from the start wraps at the end.
    assert_eq!(board.next_index(None), Some(0));
    assert_eq!(board.next_index(Some(0)), Some(1));
    assert_eq!(board.next_index(Some(4)), Some(0), "wraps at the end");
    // Backward from the start wraps to the end.
    assert_eq!(board.prev_index(None), Some(4));
    assert_eq!(board.prev_index(Some(0)), Some(4), "wraps at the start");
    assert_eq!(board.prev_index(Some(2)), Some(1));
}

#[test]
fn index_of_finds_workers_by_stable_id_across_lanes() {
    let sources = fanout();
    let mut board = ReviewBoard::new();
    board.rebuild_if_stale(ReviewBoard::fingerprint_of(sources.iter()), &sources);

    // id navigation hits the right flattened position regardless of lane.
    assert_eq!(board.index_of(2), Some(0), "first running");
    assert_eq!(board.index_of(3), Some(2), "blocked");
    assert_eq!(board.index_of(1), Some(4), "completed (last lane)");
    assert_eq!(board.index_of(999), None, "a vanished worker is not found");
}

#[test]
fn id_navigation_heals_when_a_worker_vanishes() {
    let sources = fanout();
    let mut board = ReviewBoard::new();
    board.rebuild_if_stale(ReviewBoard::fingerprint_of(sources.iter()), &sources);
    // The cursor is parked on id 5 (capped). It is pruned; the board rebuilds.
    let cursor_id = 5u64;
    assert!(board.index_of(cursor_id).is_some());

    let pruned: Vec<SubagentTimelineSource> = sources.into_iter().filter(|s| s.id != 5).collect();
    board.rebuild_if_stale(ReviewBoard::fingerprint_of(pruned.iter()), &pruned);

    // id 5 is gone — index_of returns None so the caller heals to a surviving
    // worker rather than pointing at the wrong card.
    assert_eq!(board.index_of(cursor_id), None);
    assert_eq!(board.len(), 4);
    assert_eq!(board.count_in(ReviewLane::Capped), 0);
}

#[test]
fn rebuild_is_skipped_when_the_fingerprint_is_unchanged() {
    let sources = fanout();
    let mut board = ReviewBoard::new();
    let fp = ReviewBoard::fingerprint_of(sources.iter());
    assert!(board.rebuild_if_stale(fp, &sources), "first build runs");
    let stored = board.fingerprint();

    // Same fingerprint → no recompute (the zero-idle-cost fast path).
    assert!(
        !board.rebuild_if_stale(fp, &sources),
        "unchanged fingerprint skips the rebuild",
    );
    assert_eq!(board.fingerprint(), stored, "fingerprint unchanged");

    // A status flip moves the fingerprint → recompute runs.
    let mut moved = sources.clone();
    moved[1].status = SubagentTimelineStatus::Completed;
    let fp2 = ReviewBoard::fingerprint_of(moved.iter());
    assert_ne!(fp2, fp, "a status change moves the fingerprint");
    assert!(
        board.rebuild_if_stale(fp2, &moved),
        "changed fingerprint rebuilds"
    );
}

#[test]
fn card_formats_metrics_at_the_edge_with_honest_dashes() {
    let sources = vec![
        source(1, "running", SubagentTimelineStatus::Running),
        // A capped worker with no timing / no cost — the honest "-" case.
        SubagentTimelineSource {
            id: 2,
            agent: "capped".to_string(),
            status: SubagentTimelineStatus::Rejected,
            latest: String::new(),
            elapsed_secs: None,
            tool_count: 0,
            cost_micros: None,
        },
    ];
    let mut board = ReviewBoard::new();
    board.rebuild_if_stale(ReviewBoard::fingerprint_of(sources.iter()), &sources);

    let running = board.cards_in(ReviewLane::Running)[0];
    assert_eq!(running.elapsed_clock(), "1:15", "75s renders as m:ss");
    assert_eq!(running.cost_label(), "$1.500000");

    // Past an hour the clock rolls minutes into an hours field instead of
    // accumulating misleadingly large minute counts.
    let mut hour_card = (*running).clone();
    hour_card.elapsed_secs = Some(3905);
    assert_eq!(
        hour_card.elapsed_clock(),
        "1:05:05",
        "3905s renders as h:mm:ss, not 65:05"
    );
    hour_card.elapsed_secs = Some(7200);
    assert_eq!(
        hour_card.elapsed_clock(),
        "2:00:00",
        "7200s renders as 2:00:00"
    );
    hour_card.elapsed_secs = Some(3600);
    assert_eq!(
        hour_card.elapsed_clock(),
        "1:00:00",
        "the hour boundary rolls over"
    );
    hour_card.elapsed_secs = Some(3599);
    assert_eq!(
        hour_card.elapsed_clock(),
        "59:59",
        "just under an hour stays m:ss"
    );

    let capped = board.cards_in(ReviewLane::Capped)[0];
    assert_eq!(
        capped.elapsed_clock(),
        "-",
        "no start time → dash, never a fake"
    );
    assert_eq!(
        capped.cost_label(),
        "-",
        "no cost → dash, never an invented number"
    );
    // A blank source latest falls back to the cleaned "(status)" label.
    assert_eq!(capped.latest, "(rejected)");
}

#[test]
fn summary_reads_total_lanes_and_attention() {
    let sources = fanout();
    let mut board = ReviewBoard::new();
    board.rebuild_if_stale(ReviewBoard::fingerprint_of(sources.iter()), &sources);

    let summary = board.summary();
    assert!(summary.starts_with("5 workers"), "total leads: {summary}");
    assert!(summary.contains("2 Running"), "{summary}");
    assert!(summary.contains("1 Blocked"), "{summary}");
    assert!(summary.contains("1 Capped"), "{summary}");
    assert!(summary.contains("1 Completed"), "{summary}");
    assert!(summary.contains("2 attention"), "{summary}");

    // A single worker uses the singular "worker".
    let one = vec![source(1, "solo", SubagentTimelineStatus::Running)];
    let mut board2 = ReviewBoard::new();
    board2.rebuild_if_stale(ReviewBoard::fingerprint_of(one.iter()), &one);
    assert!(
        board2.summary().starts_with("1 worker \u{00b7}"),
        "{}",
        board2.summary()
    );
}
