//! Unit tests for the Subagent Timeline Panel (§12.8.1) model: status labels and
//! attention/running classification, elapsed-clock and cost-label rendering, the
//! latest-activity label cleaning/fallback/cap, the staleness fingerprint fast
//! path, per-status filtering, navigation, and the summary readout. Pure over
//! `SubagentTimelineSource` slices — no terminal, no `TuiApp`. The end-to-end
//! keyboard/mouse/render coverage lives in `lib_tests.rs`.

use super::*;

/// A `SubagentTimelineSource` builder with sensible defaults so each test states
/// only the fields it cares about.
fn source(id: u64, status: SubagentTimelineStatus) -> SubagentTimelineSource {
    SubagentTimelineSource {
        id,
        agent: "delegate".to_string(),
        status,
        latest: String::new(),
        elapsed_secs: Some(0),
        tool_count: 0,
        cost_micros: None,
    }
}

#[test]
fn all_statuses_have_distinct_nonempty_labels() {
    let mut seen = std::collections::HashSet::new();
    for status in SubagentTimelineStatus::ALL.iter().copied() {
        let label = status.label();
        assert!(!label.is_empty(), "{status:?} has an empty label");
        assert!(seen.insert(label), "duplicate label {label:?}");
    }
    // Every status appears in ALL (exhaustiveness guard against a missed variant).
    assert_eq!(SubagentTimelineStatus::ALL.len(), 4);
}

#[test]
fn attention_and_running_classification_is_honest() {
    assert!(!SubagentTimelineStatus::Running.is_attention());
    assert!(!SubagentTimelineStatus::Completed.is_attention());
    assert!(SubagentTimelineStatus::Failed.is_attention());
    assert!(SubagentTimelineStatus::Rejected.is_attention());

    assert!(SubagentTimelineStatus::Running.is_running());
    assert!(!SubagentTimelineStatus::Completed.is_running());
    assert!(!SubagentTimelineStatus::Failed.is_running());
    assert!(!SubagentTimelineStatus::Rejected.is_running());
}

#[test]
fn clean_label_takes_first_line_collapses_and_caps() {
    // First non-blank line wins; interior whitespace collapses.
    assert_eq!(
        clean_label(
            "  ran\tthe   tests \n second line",
            SubagentTimelineStatus::Running
        ),
        "ran the tests",
    );
    // A blank source falls back to the status label, never invented text.
    assert_eq!(
        clean_label("   \n  ", SubagentTimelineStatus::Completed),
        "(done)"
    );
    assert_eq!(clean_label("", SubagentTimelineStatus::Failed), "(failed)");
    // Over-long labels are capped on a char boundary with an ellipsis.
    let long = "x".repeat(200);
    let cleaned = clean_label(&long, SubagentTimelineStatus::Running);
    assert!(cleaned.ends_with('\u{2026}'));
    assert!(cleaned.chars().count() <= LABEL_CAP + 1);
}

#[test]
fn elapsed_clock_renders_mmss_or_dash_for_missing_time() {
    let mut src = source(1, SubagentTimelineStatus::Running);
    src.elapsed_secs = Some(0);
    assert_eq!(entry_for_source(&src, 1).elapsed_clock(), "0:00");
    src.elapsed_secs = Some(5);
    assert_eq!(entry_for_source(&src, 1).elapsed_clock(), "0:05");
    src.elapsed_secs = Some(75);
    assert_eq!(entry_for_source(&src, 1).elapsed_clock(), "1:15");
    src.elapsed_secs = Some(605);
    assert_eq!(entry_for_source(&src, 1).elapsed_clock(), "10:05");
    // A record that never ran (cap rejection): an honest dash, never a fake 0:00.
    src.elapsed_secs = None;
    assert_eq!(entry_for_source(&src, 1).elapsed_clock(), "-");
}

#[test]
fn cost_label_renders_dollars_or_dash_for_unknown_cost() {
    let mut src = source(1, SubagentTimelineStatus::Completed);
    src.cost_micros = Some(1_234_560);
    assert_eq!(entry_for_source(&src, 1).cost_label(), "$1.234560");
    src.cost_micros = Some(0);
    assert_eq!(entry_for_source(&src, 1).cost_label(), "$0.000000");
    // The spec's "accurate cost depends on child metrics" case: an unknown cost
    // reads as a dash, never an invented number.
    src.cost_micros = None;
    assert_eq!(entry_for_source(&src, 1).cost_label(), "-");
}

#[test]
fn entry_carries_ordinal_metrics_and_cleaned_label() {
    let mut src = source(7, SubagentTimelineStatus::Failed);
    src.agent = "reviewer".to_string();
    src.latest = "  boom  ".to_string();
    src.elapsed_secs = Some(42);
    src.tool_count = 9;
    src.cost_micros = Some(500_000);
    let entry = entry_for_source(&src, 3);
    assert_eq!(entry.id, 7);
    assert_eq!(entry.ordinal, 3);
    assert_eq!(entry.agent, "reviewer");
    assert_eq!(entry.status, SubagentTimelineStatus::Failed);
    assert_eq!(entry.latest, "boom");
    assert_eq!(entry.elapsed_secs, Some(42));
    assert_eq!(entry.tool_count, 9);
    assert_eq!(entry.cost_micros, Some(500_000));
    assert!(entry.is_attention());
}

#[test]
fn rebuild_is_skipped_when_fingerprint_is_unchanged() {
    let sources = vec![
        source(1, SubagentTimelineStatus::Running),
        source(2, SubagentTimelineStatus::Completed),
    ];
    let fp = SubagentTimeline::fingerprint_of(sources.iter());

    let mut timeline = SubagentTimeline::new();
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
    let base = vec![source(1, SubagentTimelineStatus::Running)];
    let fp_base = SubagentTimeline::fingerprint_of(base.iter());

    // Status flip (running -> done).
    let mut status = base.clone();
    status[0].status = SubagentTimelineStatus::Completed;
    assert_ne!(SubagentTimeline::fingerprint_of(status.iter()), fp_base);

    // Elapsed tick.
    let mut elapsed = base.clone();
    elapsed[0].elapsed_secs = Some(1);
    assert_ne!(SubagentTimeline::fingerprint_of(elapsed.iter()), fp_base);

    // Tool count tick.
    let mut tools = base.clone();
    tools[0].tool_count = 1;
    assert_ne!(SubagentTimeline::fingerprint_of(tools.iter()), fp_base);

    // Cost appearing.
    let mut cost = base.clone();
    cost[0].cost_micros = Some(10);
    assert_ne!(SubagentTimeline::fingerprint_of(cost.iter()), fp_base);

    // Latest-activity line changing.
    let mut latest = base.clone();
    latest[0].latest = "did a thing".to_string();
    assert_ne!(SubagentTimeline::fingerprint_of(latest.iter()), fp_base);

    // Append.
    let mut more = base.clone();
    more.push(source(2, SubagentTimelineStatus::Failed));
    assert_ne!(SubagentTimeline::fingerprint_of(more.iter()), fp_base);
}

#[test]
fn empty_timeline_builds_once_and_reports_empty() {
    let mut timeline = SubagentTimeline::new();
    let empty: Vec<SubagentTimelineSource> = Vec::new();
    let fp = SubagentTimeline::fingerprint_of(empty.iter());
    assert!(
        timeline.rebuild_if_stale(fp, &empty),
        "first empty build runs"
    );
    assert!(timeline.is_empty());
    assert_eq!(timeline.len(), 0);
    assert_eq!(timeline.summary(), "");
    assert_eq!(timeline.running_count(), 0);
    assert_eq!(timeline.attention_count(), 0);
    assert!(timeline.visible().is_empty());
    assert_eq!(timeline.next_index(None), None);
    assert_eq!(timeline.prev_index(None), None);
    // Cycling a filter on an empty timeline is a no-op (stays "show all").
    timeline.cycle_filter();
    assert_eq!(timeline.filter(), None);
    // A second empty build is the fast path (built + unchanged fingerprint).
    assert!(!timeline.rebuild_if_stale(fp, &empty));
}

/// Build a small timeline with one running, one completed, one failed, and one
/// cap-rejected subagent — the shape the filter/navigation tests reuse. Ordinals
/// land in record order (1..=4).
fn sample_timeline() -> SubagentTimeline {
    let sources = vec![
        source(1, SubagentTimelineStatus::Running),
        source(2, SubagentTimelineStatus::Completed),
        source(3, SubagentTimelineStatus::Failed),
        {
            // A cap-rejected record: a synthetic high id, no timing, no cost.
            let mut s = source(u64::MAX, SubagentTimelineStatus::Rejected);
            s.elapsed_secs = None;
            s
        },
    ];
    let mut timeline = SubagentTimeline::new();
    let fp = SubagentTimeline::fingerprint_of(sources.iter());
    timeline.rebuild_if_stale(fp, &sources);
    timeline
}

#[test]
fn rows_are_in_record_order_with_one_based_ordinals() {
    let timeline = sample_timeline();
    let ordinals: Vec<u32> = timeline.entries().iter().map(|e| e.ordinal).collect();
    assert_eq!(ordinals, vec![1, 2, 3, 4]);
    let ids: Vec<u64> = timeline.entries().iter().map(|e| e.id).collect();
    assert_eq!(ids, vec![1, 2, 3, u64::MAX]);
}

#[test]
fn counts_and_attention_are_reported() {
    let timeline = sample_timeline();
    assert_eq!(timeline.len(), 4);
    assert_eq!(timeline.count_of(SubagentTimelineStatus::Running), 1);
    assert_eq!(timeline.count_of(SubagentTimelineStatus::Completed), 1);
    assert_eq!(timeline.count_of(SubagentTimelineStatus::Failed), 1);
    assert_eq!(timeline.count_of(SubagentTimelineStatus::Rejected), 1);
    assert_eq!(timeline.running_count(), 1);
    // Failed + rejected both want attention.
    assert_eq!(timeline.attention_count(), 2);
}

#[test]
fn summary_lists_subagents_statuses_and_attention() {
    let summary = sample_timeline().summary();
    assert!(summary.contains("4 subagents"), "{summary}");
    assert!(summary.contains("1 running"), "{summary}");
    assert!(summary.contains("1 done"), "{summary}");
    assert!(summary.contains("1 failed"), "{summary}");
    assert!(summary.contains("1 rejected"), "{summary}");
    assert!(summary.contains("2 attention"), "{summary}");
}

#[test]
fn single_subagent_summary_uses_singular_noun() {
    let sources = vec![source(1, SubagentTimelineStatus::Running)];
    let mut timeline = SubagentTimeline::new();
    let fp = SubagentTimeline::fingerprint_of(sources.iter());
    timeline.rebuild_if_stale(fp, &sources);
    let summary = timeline.summary();
    assert!(summary.contains("1 subagent "), "{summary}");
    assert!(!summary.contains("subagents"), "{summary}");
}

#[test]
fn filter_cycles_through_present_statuses_then_back_to_all() {
    let mut timeline = sample_timeline();
    assert_eq!(timeline.filter(), None, "starts unfiltered");
    assert_eq!(timeline.visible_len(), 4);

    // present statuses, in ALL order: Running, Completed, Failed, Rejected.
    assert_eq!(
        timeline.present_statuses(),
        vec![
            SubagentTimelineStatus::Running,
            SubagentTimelineStatus::Completed,
            SubagentTimelineStatus::Failed,
            SubagentTimelineStatus::Rejected,
        ],
    );

    timeline.cycle_filter();
    assert_eq!(timeline.filter(), Some(SubagentTimelineStatus::Running));
    assert_eq!(timeline.visible_len(), 1);

    timeline.cycle_filter();
    assert_eq!(timeline.filter(), Some(SubagentTimelineStatus::Completed));

    timeline.cycle_filter();
    assert_eq!(timeline.filter(), Some(SubagentTimelineStatus::Failed));

    timeline.cycle_filter();
    assert_eq!(timeline.filter(), Some(SubagentTimelineStatus::Rejected));

    // Last present status wraps back to "show all".
    timeline.cycle_filter();
    assert_eq!(timeline.filter(), None);
    assert_eq!(timeline.visible_len(), 4);
}

#[test]
fn filter_skips_absent_statuses() {
    // Only running + failed are present; the cycle never offers done/rejected.
    let sources = vec![
        source(1, SubagentTimelineStatus::Running),
        source(2, SubagentTimelineStatus::Failed),
    ];
    let mut timeline = SubagentTimeline::new();
    let fp = SubagentTimeline::fingerprint_of(sources.iter());
    timeline.rebuild_if_stale(fp, &sources);
    assert_eq!(
        timeline.present_statuses(),
        vec![
            SubagentTimelineStatus::Running,
            SubagentTimelineStatus::Failed,
        ],
    );
    timeline.cycle_filter();
    assert_eq!(timeline.filter(), Some(SubagentTimelineStatus::Running));
    timeline.cycle_filter();
    assert_eq!(timeline.filter(), Some(SubagentTimelineStatus::Failed));
    timeline.cycle_filter();
    assert_eq!(
        timeline.filter(),
        None,
        "wraps back to all, skipping absentees"
    );
}

#[test]
fn visible_get_and_navigation_walk_the_filtered_list() {
    let timeline = sample_timeline();
    // Unfiltered: visible_get reaches every row in order.
    assert_eq!(timeline.visible_get(0).map(|e| e.id), Some(1));
    assert_eq!(timeline.visible_get(3).map(|e| e.id), Some(u64::MAX));
    assert_eq!(timeline.visible_get(4), None, "out of range");

    // next_index wraps at the end; prev_index wraps at the front.
    assert_eq!(timeline.next_index(None), Some(0));
    assert_eq!(timeline.next_index(Some(0)), Some(1));
    assert_eq!(timeline.next_index(Some(3)), Some(0), "wraps at the tail");
    assert_eq!(timeline.prev_index(Some(0)), Some(3), "wraps at the head");
    assert_eq!(timeline.prev_index(Some(2)), Some(1));
    assert_eq!(timeline.prev_index(None), Some(3));
}

#[test]
fn filter_preserved_across_rebuild_and_stale_filter_falls_back_to_all() {
    let mut timeline = sample_timeline();
    timeline.cycle_filter();
    timeline.cycle_filter();
    timeline.cycle_filter();
    assert_eq!(timeline.filter(), Some(SubagentTimelineStatus::Failed));

    // Rebuild with a different set that still has a failed row: the filter
    // survives (it is a view setting, not data).
    let sources = vec![
        source(1, SubagentTimelineStatus::Running),
        source(3, SubagentTimelineStatus::Failed),
    ];
    let fp = SubagentTimeline::fingerprint_of(sources.iter());
    timeline.rebuild_if_stale(fp, &sources);
    assert_eq!(timeline.filter(), Some(SubagentTimelineStatus::Failed));
    assert_eq!(timeline.visible_len(), 1);

    // Now cycle from a filter whose status has dropped: rebuild to a set with no
    // failed rows, and a cycle falls back to "show all" rather than a stale
    // status.
    let no_failed = vec![source(1, SubagentTimelineStatus::Running)];
    let fp2 = SubagentTimeline::fingerprint_of(no_failed.iter());
    timeline.rebuild_if_stale(fp2, &no_failed);
    // Filter still says Failed, but no failed rows remain.
    assert_eq!(timeline.visible_len(), 0);
    timeline.cycle_filter();
    assert_eq!(
        timeline.filter(),
        None,
        "a filter whose status vanished falls back to show-all",
    );
}
