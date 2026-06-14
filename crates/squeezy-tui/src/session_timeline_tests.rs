//! Unit tests for the Session Timeline (§12.2.6) event model: classification,
//! status priority, clock/label rendering, the staleness fingerprint fast path,
//! per-kind filtering, turn grouping, navigation, and the summary readout. Pure
//! over `TimelineSource` slices — no terminal, no `TuiApp`. The end-to-end
//! keyboard/mouse/render coverage lives in `lib_tests.rs`.

use super::*;

/// A `TimelineSource` builder with sensible defaults so each test states only
/// the fields it cares about.
fn source(id: u64, kind: TimelineKind) -> TimelineSource {
    TimelineSource {
        id,
        revision: 0,
        kind,
        is_error: false,
        is_pending: false,
        turn: 1,
        timestamp: None,
        raw_label: String::new(),
    }
}

#[test]
fn all_kinds_have_distinct_nonempty_labels() {
    let mut seen = std::collections::HashSet::new();
    for kind in TimelineKind::ALL.iter().copied() {
        let label = kind.label();
        assert!(!label.is_empty(), "{kind:?} has an empty label");
        assert!(seen.insert(label), "duplicate label {label:?}");
    }
    // Every kind appears in ALL (exhaustiveness guard against a missed variant).
    assert_eq!(TimelineKind::ALL.len(), 10);
}

#[test]
fn status_labels_are_distinct() {
    assert_eq!(TimelineStatus::Ok.label(), "ok");
    assert_eq!(TimelineStatus::Failed.label(), "failed");
    assert_eq!(TimelineStatus::Pending.label(), "pending");
}

#[test]
fn clean_label_takes_first_line_collapses_and_caps() {
    // First non-blank line wins; interior whitespace collapses.
    assert_eq!(
        clean_label(
            "  refactor\tthe   parser \n second line",
            TimelineKind::Prompt
        ),
        "refactor the parser",
    );
    // A blank source falls back to the kind label, never invented text.
    assert_eq!(clean_label("   \n  ", TimelineKind::Tool), "(tool)");
    assert_eq!(clean_label("", TimelineKind::Error), "(error)");
    // Over-long labels are capped on a char boundary with an ellipsis.
    let long = "x".repeat(200);
    let cleaned = clean_label(&long, TimelineKind::Note);
    assert!(cleaned.ends_with('\u{2026}'));
    assert!(cleaned.chars().count() <= LABEL_CAP + 1);
}

#[test]
fn event_status_folds_error_then_pending_then_ok() {
    // Error wins over pending: a failed in-flight turn reads as failed.
    let mut src = source(1, TimelineKind::Turn);
    src.is_error = true;
    src.is_pending = true;
    assert_eq!(event_for_source(&src, 0).status, TimelineStatus::Failed);

    src.is_error = false;
    assert_eq!(event_for_source(&src, 0).status, TimelineStatus::Pending);

    src.is_pending = false;
    assert_eq!(event_for_source(&src, 0).status, TimelineStatus::Ok);
}

#[test]
fn event_carries_sequence_turn_and_label() {
    let mut src = source(7, TimelineKind::Tool);
    src.turn = 3;
    src.raw_label = "shell".to_string();
    let event = event_for_source(&src, 42);
    assert_eq!(event.entry_id, 7);
    assert_eq!(event.sequence, 42);
    assert_eq!(event.turn, 3);
    assert_eq!(event.kind, TimelineKind::Tool);
    assert_eq!(event.label, "shell");
}

#[test]
fn clock_renders_mmss_or_dashes_for_missing_timestamp() {
    let mut src = source(1, TimelineKind::Prompt);
    src.timestamp = Some(0);
    assert_eq!(event_for_source(&src, 0).clock(), "0:00");
    src.timestamp = Some(5);
    assert_eq!(event_for_source(&src, 0).clock(), "0:05");
    src.timestamp = Some(75);
    assert_eq!(event_for_source(&src, 0).clock(), "1:15");
    src.timestamp = Some(605);
    assert_eq!(event_for_source(&src, 0).clock(), "10:05");
    // Past an hour the minutes field rolls into hours rather than printing
    // a runaway "90:05" that would break the fixed-width column.
    src.timestamp = Some(3600);
    assert_eq!(event_for_source(&src, 0).clock(), "1:00:00");
    src.timestamp = Some(5405);
    assert_eq!(event_for_source(&src, 0).clock(), "1:30:05");
    // The spec's "missing timestamps" case: honest dashes, never a fake 0:00.
    src.timestamp = None;
    assert_eq!(event_for_source(&src, 0).clock(), "--:--");
}

#[test]
fn rebuild_is_skipped_when_fingerprint_is_unchanged() {
    let sources = vec![
        source(1, TimelineKind::Prompt),
        source(2, TimelineKind::Turn),
    ];
    let fp = SessionTimeline::fingerprint_of(sources.iter());

    let mut timeline = SessionTimeline::new();
    assert!(timeline.rebuild_if_stale(fp, &sources), "first build runs");
    assert_eq!(timeline.len(), 2);
    assert_eq!(timeline.fingerprint(), fp);

    // Same fingerprint: the zero-idle-cost fast path returns false and rebuilds
    // nothing.
    assert!(
        !timeline.rebuild_if_stale(fp, &sources),
        "unchanged fingerprint skips the rebuild",
    );
}

#[test]
fn fingerprint_moves_on_any_meaningful_change() {
    let base = vec![source(1, TimelineKind::Prompt)];
    let fp_base = SessionTimeline::fingerprint_of(base.iter());

    // Revision bump.
    let mut rev = base.clone();
    rev[0].revision = 1;
    assert_ne!(SessionTimeline::fingerprint_of(rev.iter()), fp_base);

    // Status flip.
    let mut err = base.clone();
    err[0].is_error = true;
    assert_ne!(SessionTimeline::fingerprint_of(err.iter()), fp_base);

    // Turn change.
    let mut turn = base.clone();
    turn[0].turn = 2;
    assert_ne!(SessionTimeline::fingerprint_of(turn.iter()), fp_base);

    // Timestamp appearing.
    let mut ts = base.clone();
    ts[0].timestamp = Some(10);
    assert_ne!(SessionTimeline::fingerprint_of(ts.iter()), fp_base);

    // Append.
    let mut more = base.clone();
    more.push(source(2, TimelineKind::Tool));
    assert_ne!(SessionTimeline::fingerprint_of(more.iter()), fp_base);
}

#[test]
fn empty_timeline_builds_once_and_reports_empty() {
    let mut timeline = SessionTimeline::new();
    let empty: Vec<TimelineSource> = Vec::new();
    let fp = SessionTimeline::fingerprint_of(empty.iter());
    assert!(
        timeline.rebuild_if_stale(fp, &empty),
        "first empty build runs"
    );
    assert!(timeline.is_empty());
    assert_eq!(timeline.len(), 0);
    assert_eq!(timeline.summary(), "");
    assert_eq!(timeline.turn_count(), 0);
    assert!(timeline.visible().is_empty());
    assert_eq!(timeline.next_index(None), None);
    // A second empty build is the fast path (built + unchanged fingerprint).
    assert!(!timeline.rebuild_if_stale(fp, &empty));
}

/// Build a small timeline spanning two turns with a tool, a failed tool, a
/// pending turn, and a couple of notes — the shape the filter/navigation tests
/// reuse.
fn sample_timeline() -> SessionTimeline {
    let mut sources = vec![
        source(1, TimelineKind::Prompt),
        source(2, TimelineKind::Tool),
        source(3, TimelineKind::Tool),
        source(4, TimelineKind::Turn),
    ];
    sources[2].is_error = true; // a failed tool
    sources[3].is_pending = true; // an in-flight turn
    // Second turn.
    sources.push({
        let mut s = source(5, TimelineKind::Prompt);
        s.turn = 2;
        s
    });
    sources.push({
        let mut s = source(6, TimelineKind::Note);
        s.turn = 2;
        s
    });
    let mut timeline = SessionTimeline::new();
    let fp = SessionTimeline::fingerprint_of(sources.iter());
    timeline.rebuild_if_stale(fp, &sources);
    timeline
}

#[test]
fn events_are_in_chronological_order_with_sequence_numbers() {
    let timeline = sample_timeline();
    let seqs: Vec<u32> = timeline.events().iter().map(|e| e.sequence).collect();
    assert_eq!(seqs, vec![0, 1, 2, 3, 4, 5]);
    let ids: Vec<u64> = timeline.events().iter().map(|e| e.entry_id).collect();
    assert_eq!(ids, vec![1, 2, 3, 4, 5, 6]);
}

#[test]
fn counts_and_turn_grouping_are_reported() {
    let timeline = sample_timeline();
    assert_eq!(timeline.len(), 6);
    assert_eq!(timeline.count_of(TimelineKind::Tool), 2);
    assert_eq!(timeline.count_of(TimelineKind::Prompt), 2);
    assert_eq!(timeline.failed_count(), 1);
    assert_eq!(timeline.pending_count(), 1);
    assert_eq!(timeline.turn_count(), 2, "events span two turns");
}

#[test]
fn summary_lists_events_turns_kinds_and_failures() {
    let summary = sample_timeline().summary();
    assert!(summary.contains("6 events"), "{summary}");
    assert!(summary.contains("2 turns"), "{summary}");
    assert!(summary.contains("2 tool"), "{summary}");
    assert!(summary.contains("1 failed"), "{summary}");
    assert!(summary.contains("1 pending"), "{summary}");
}

#[test]
fn filter_cycles_through_present_kinds_then_back_to_all() {
    let mut timeline = sample_timeline();
    assert_eq!(timeline.filter(), None, "starts unfiltered");
    assert_eq!(timeline.visible_len(), 6);

    // present kinds, in ALL order: Prompt, Turn, Tool, Note.
    let present = timeline.present_kinds();
    assert_eq!(
        present,
        vec![
            TimelineKind::Prompt,
            TimelineKind::Turn,
            TimelineKind::Tool,
            TimelineKind::Note,
        ],
    );

    timeline.cycle_filter();
    assert_eq!(timeline.filter(), Some(TimelineKind::Prompt));
    assert_eq!(timeline.visible_len(), 2);

    // Walk to the last present kind.
    timeline.cycle_filter(); // Turn
    timeline.cycle_filter(); // Tool
    assert_eq!(timeline.filter(), Some(TimelineKind::Tool));
    assert_eq!(timeline.visible_len(), 2);
    timeline.cycle_filter(); // Note
    assert_eq!(timeline.filter(), Some(TimelineKind::Note));

    // Past the last present kind wraps back to "show all".
    timeline.cycle_filter();
    assert_eq!(timeline.filter(), None);
    assert_eq!(timeline.visible_len(), 6);
}

#[test]
fn filter_is_a_noop_on_an_empty_timeline() {
    let mut timeline = SessionTimeline::new();
    let empty: Vec<TimelineSource> = Vec::new();
    timeline.rebuild_if_stale(SessionTimeline::fingerprint_of(empty.iter()), &empty);
    timeline.cycle_filter();
    assert_eq!(timeline.filter(), None, "no kinds to filter to");
}

#[test]
fn visible_get_indexes_into_the_filtered_list() {
    let mut timeline = sample_timeline();
    timeline.cycle_filter(); // Prompt only.
    assert_eq!(timeline.filter(), Some(TimelineKind::Prompt));
    assert_eq!(
        timeline.visible_get(0).map(|e| e.entry_id),
        Some(1),
        "first prompt",
    );
    assert_eq!(
        timeline.visible_get(1).map(|e| e.entry_id),
        Some(5),
        "second prompt",
    );
    assert_eq!(timeline.visible_get(2), None, "only two prompts");
}

#[test]
fn navigation_wraps_over_the_visible_list() {
    let timeline = sample_timeline();
    assert_eq!(timeline.next_index(None), Some(0));
    assert_eq!(timeline.next_index(Some(0)), Some(1));
    // Last visible wraps to the first.
    assert_eq!(timeline.next_index(Some(5)), Some(0));

    assert_eq!(timeline.prev_index(None), Some(5));
    assert_eq!(timeline.prev_index(Some(0)), Some(5));
    assert_eq!(timeline.prev_index(Some(3)), Some(2));
}

#[test]
fn navigation_follows_the_active_filter_bounds() {
    let mut timeline = sample_timeline();
    timeline.cycle_filter(); // Prompt only: two visible.
    assert_eq!(timeline.visible_len(), 2);
    // Wrapping respects the filtered count, not the full count.
    assert_eq!(timeline.next_index(Some(1)), Some(0));
    assert_eq!(timeline.prev_index(Some(0)), Some(1));
}

#[test]
fn filter_survives_a_rebuild_as_a_view_setting() {
    let mut timeline = sample_timeline();
    timeline.cycle_filter(); // Prompt.
    assert_eq!(timeline.filter(), Some(TimelineKind::Prompt));

    // A content change (revision bump) rebuilds the events but keeps the filter.
    let mut sources: Vec<TimelineSource> = vec![
        source(1, TimelineKind::Prompt),
        source(2, TimelineKind::Tool),
    ];
    sources[0].revision = 9;
    let fp = SessionTimeline::fingerprint_of(sources.iter());
    assert!(timeline.rebuild_if_stale(fp, &sources), "content changed");
    assert_eq!(
        timeline.filter(),
        Some(TimelineKind::Prompt),
        "filter is a view setting, preserved across rebuild",
    );
    assert_eq!(timeline.visible_len(), 1);
}
