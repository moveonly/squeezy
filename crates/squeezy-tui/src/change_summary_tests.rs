//! Unit tests for the pure "What Changed Since Here?" model (§12.2.7). These
//! exercise the anchor / grouping / caching math directly, with no terminal — the
//! overlay's keyboard/mouse/render integration is covered by the capture-sink
//! suite in `lib_tests.rs`.

use super::*;
use crate::session_timeline::{TimelineKind, TimelineSource};

/// Build one classified source the way `build_timeline_sources` does, so the
/// grouping math sees the same shape production feeds it.
fn source(id: u64, kind: TimelineKind, is_error: bool, label: &str) -> TimelineSource {
    TimelineSource {
        id,
        revision: 0,
        kind,
        is_error,
        is_pending: false,
        turn: 1,
        timestamp: None,
        raw_label: label.to_string(),
    }
}

/// A representative session: prompt, edit, tool, failed tool, plan, approval,
/// subagent, turn — eight entries in transcript (sequence) order.
fn sample_sources() -> Vec<TimelineSource> {
    vec![
        source(1, TimelineKind::Prompt, false, "refactor the parser"),
        source(2, TimelineKind::Edit, false, "edit src/parser.rs"),
        source(3, TimelineKind::Tool, false, "cargo test"),
        source(4, TimelineKind::Tool, true, "cargo build failed"),
        source(5, TimelineKind::Plan, false, "checkpoint: parser done"),
        source(6, TimelineKind::Approval, false, "approved: write file"),
        source(7, TimelineKind::Subagent, false, "subagent research"),
        source(8, TimelineKind::Turn, false, "Here is the refactor."),
    ]
}

fn rebuild(summary: &mut ChangeSummary, sources: &[TimelineSource]) -> bool {
    let fp = ChangeSummary::fingerprint_of(summary.anchor(), sources.iter());
    summary.rebuild_if_stale(fp, sources)
}

#[test]
fn no_anchor_yields_empty_delta() {
    let mut summary = ChangeSummary::new();
    let sources = sample_sources();
    assert!(!summary.has_anchor());
    assert!(rebuild(&mut summary, &sources), "first build runs");
    assert!(summary.is_empty(), "no anchor means no delta");
    assert_eq!(summary.len(), 0);
    assert_eq!(summary.summary(), "");
    assert!(summary.present_groups().is_empty());
}

#[test]
fn anchor_at_head_surfaces_every_later_change_grouped() {
    let mut summary = ChangeSummary::new();
    let sources = sample_sources();
    // Anchor on the prompt (sequence 0): everything after it is the delta.
    summary.set_anchor(Anchor {
        entry_id: 1,
        sequence: 0,
    });
    assert!(summary.has_anchor());
    assert!(rebuild(&mut summary, &sources));

    // Seven later entries, but the failed tool moves into Errors, so:
    //   Edits 1 (entry 2), Commands 1 (entry 3, the passing test),
    //   Errors 1 (entry 4, the failed build), Checkpoints 1 (entry 5),
    //   Decisions 1 (entry 6), Results 2 (subagent 7 + turn 8).
    assert_eq!(summary.len(), 7, "every later entry is surfaced once");
    assert_eq!(summary.count_of(ChangeGroupKind::Edits), 1);
    assert_eq!(summary.count_of(ChangeGroupKind::Commands), 1);
    assert_eq!(summary.count_of(ChangeGroupKind::Errors), 1);
    assert_eq!(summary.count_of(ChangeGroupKind::Checkpoints), 1);
    assert_eq!(summary.count_of(ChangeGroupKind::Decisions), 1);
    assert_eq!(summary.count_of(ChangeGroupKind::Results), 2);
    assert_eq!(summary.failed_count(), 1, "only the failed build is failed");

    // The failed tool is grouped under Errors, not Commands, and is flagged.
    let err = summary
        .items()
        .iter()
        .find(|i| i.group == ChangeGroupKind::Errors)
        .expect("an errors item");
    assert_eq!(err.entry_id, 4);
    assert!(err.failed);

    // Items are ordered by ChangeGroupKind::ALL (Edits first, Results last).
    assert_eq!(
        summary.items().first().unwrap().group,
        ChangeGroupKind::Edits
    );
    assert_eq!(
        summary.items().last().unwrap().group,
        ChangeGroupKind::Results
    );

    // present_groups is the ALL-ordered set of non-empty groups.
    assert_eq!(
        summary.present_groups(),
        vec![
            ChangeGroupKind::Edits,
            ChangeGroupKind::Commands,
            ChangeGroupKind::Errors,
            ChangeGroupKind::Checkpoints,
            ChangeGroupKind::Decisions,
            ChangeGroupKind::Results,
        ],
    );
}

#[test]
fn anchor_excludes_itself_and_earlier_entries() {
    let mut summary = ChangeSummary::new();
    let sources = sample_sources();
    // Anchor on the plan (sequence 4): only entries 6/7/8 are later.
    summary.set_anchor(Anchor {
        entry_id: 5,
        sequence: 4,
    });
    assert!(rebuild(&mut summary, &sources));

    assert_eq!(summary.len(), 3, "only the three entries after the plan");
    // The plan itself (the anchor) and everything before it are excluded.
    assert!(
        summary.items().iter().all(|i| i.entry_id > 5),
        "no item at or before the anchor sequence",
    );
    assert_eq!(
        summary.count_of(ChangeGroupKind::Decisions),
        1,
        "approval 6"
    );
    assert_eq!(
        summary.count_of(ChangeGroupKind::Results),
        2,
        "subagent+turn"
    );
    assert_eq!(summary.count_of(ChangeGroupKind::Edits), 0);
}

#[test]
fn anchor_at_session_end_is_an_honest_empty_delta() {
    let mut summary = ChangeSummary::new();
    let sources = sample_sources();
    // Anchor on the last entry (sequence 7): nothing came after it.
    summary.set_anchor(Anchor {
        entry_id: 8,
        sequence: 7,
    });
    assert!(rebuild(&mut summary, &sources));
    assert!(summary.has_anchor(), "the mark stands");
    assert!(
        summary.is_empty(),
        "an anchor at the end honestly reports no later changes",
    );
    assert_eq!(summary.summary(), "");
}

#[test]
fn empty_sources_with_anchor_is_empty() {
    let mut summary = ChangeSummary::new();
    summary.set_anchor(Anchor {
        entry_id: 1,
        sequence: 0,
    });
    let sources: Vec<TimelineSource> = Vec::new();
    assert!(rebuild(&mut summary, &sources));
    assert!(summary.is_empty());
    assert_eq!(summary.next_index(None), None, "nothing to navigate");
    assert_eq!(summary.prev_index(None), None);
    assert!(summary.get(0).is_none());
}

#[test]
fn rebuild_is_skipped_when_fingerprint_is_unchanged() {
    let mut summary = ChangeSummary::new();
    let sources = sample_sources();
    summary.set_anchor(Anchor {
        entry_id: 1,
        sequence: 0,
    });
    assert!(rebuild(&mut summary, &sources), "first build runs");
    let before = summary.fingerprint();
    // Same anchor, same sources: the second refresh is a no-op.
    assert!(
        !rebuild(&mut summary, &sources),
        "unchanged refresh is skipped"
    );
    assert_eq!(summary.fingerprint(), before, "fingerprint is stable");
}

#[test]
fn appending_a_later_event_invalidates_and_rebuilds() {
    let mut summary = ChangeSummary::new();
    let mut sources = sample_sources();
    summary.set_anchor(Anchor {
        entry_id: 1,
        sequence: 0,
    });
    assert!(rebuild(&mut summary, &sources));
    let before = summary.len();

    // A new failed tool appends after the anchor: the fingerprint moves and the
    // delta grows by one (and the failed count by one).
    sources.push(source(9, TimelineKind::Tool, true, "lint failed"));
    assert!(
        rebuild(&mut summary, &sources),
        "append invalidates the cache"
    );
    assert_eq!(summary.len(), before + 1);
    assert_eq!(summary.count_of(ChangeGroupKind::Errors), 2);
    assert_eq!(summary.failed_count(), 2);
}

#[test]
fn re_marking_the_anchor_invalidates_and_renarrows() {
    let mut summary = ChangeSummary::new();
    let sources = sample_sources();
    summary.set_anchor(Anchor {
        entry_id: 1,
        sequence: 0,
    });
    assert!(rebuild(&mut summary, &sources));
    assert_eq!(summary.len(), 7);

    // Re-mark later in the session: the delta renarrows to just the tail.
    summary.set_anchor(Anchor {
        entry_id: 6,
        sequence: 5,
    });
    assert!(
        rebuild(&mut summary, &sources),
        "re-mark invalidates the cache"
    );
    assert_eq!(summary.len(), 2, "only subagent 7 + turn 8 remain");
    assert!(summary.items().iter().all(|i| i.entry_id > 6));
}

#[test]
fn clear_drops_the_anchor_and_the_delta() {
    let mut summary = ChangeSummary::new();
    let sources = sample_sources();
    summary.set_anchor(Anchor {
        entry_id: 1,
        sequence: 0,
    });
    assert!(rebuild(&mut summary, &sources));
    assert!(!summary.is_empty());

    summary.clear();
    assert!(!summary.has_anchor());
    assert!(summary.is_empty());
    assert_eq!(summary.anchor(), None);
}

#[test]
fn missing_or_blank_labels_fall_back_to_the_group_heading() {
    let mut summary = ChangeSummary::new();
    let sources = vec![
        source(1, TimelineKind::Prompt, false, "anchor"),
        source(2, TimelineKind::Edit, false, "   "),
        source(3, TimelineKind::Tool, false, ""),
    ];
    summary.set_anchor(Anchor {
        entry_id: 1,
        sequence: 0,
    });
    assert!(rebuild(&mut summary, &sources));
    let edit = summary
        .items()
        .iter()
        .find(|i| i.group == ChangeGroupKind::Edits)
        .expect("an edit item");
    assert_eq!(edit.label, "(files changed)", "blank edit falls back");
    let cmd = summary
        .items()
        .iter()
        .find(|i| i.group == ChangeGroupKind::Commands)
        .expect("a command item");
    assert_eq!(cmd.label, "(commands & tests)", "empty command falls back");
}

#[test]
fn long_labels_are_collapsed_and_capped() {
    let mut summary = ChangeSummary::new();
    let long = "a ".repeat(120);
    let sources = vec![
        source(1, TimelineKind::Prompt, false, "anchor"),
        source(2, TimelineKind::Tool, false, &format!("run\n{long}")),
    ];
    summary.set_anchor(Anchor {
        entry_id: 1,
        sequence: 0,
    });
    assert!(rebuild(&mut summary, &sources));
    let item = summary.get(0).expect("the command item");
    // First non-empty line wins; "run" has no whitespace runs to collapse and is
    // well within the cap, so it survives verbatim.
    assert_eq!(item.label, "run");

    // A single very long first line is capped with an ellipsis.
    let sources2 = vec![
        source(1, TimelineKind::Prompt, false, "anchor"),
        source(2, TimelineKind::Tool, false, &"x".repeat(200)),
    ];
    summary.set_anchor(Anchor {
        entry_id: 1,
        sequence: 0,
    });
    assert!(rebuild(&mut summary, &sources2));
    let capped = summary.get(0).expect("the capped item");
    assert!(capped.label.ends_with('\u{2026}'), "capped with ellipsis");
    assert!(
        capped.label.chars().count() <= 57,
        "bounded to the cap + ellipsis: {}",
        capped.label.chars().count(),
    );
}

#[test]
fn navigation_wraps_over_the_flattened_list() {
    let mut summary = ChangeSummary::new();
    let sources = sample_sources();
    summary.set_anchor(Anchor {
        entry_id: 1,
        sequence: 0,
    });
    assert!(rebuild(&mut summary, &sources));
    let last = summary.len() - 1;

    // Forward from None lands on the first; forward from the last wraps to 0.
    assert_eq!(summary.next_index(None), Some(0));
    assert_eq!(summary.next_index(Some(0)), Some(1));
    assert_eq!(summary.next_index(Some(last)), Some(0), "wraps at the end");

    // Backward from None lands on the last; backward from 0 wraps to the last.
    assert_eq!(summary.prev_index(None), Some(last));
    assert_eq!(
        summary.prev_index(Some(0)),
        Some(last),
        "wraps at the start"
    );
    assert_eq!(summary.prev_index(Some(1)), Some(0));
}

#[test]
fn summary_line_counts_groups_and_failures() {
    let mut summary = ChangeSummary::new();
    let sources = sample_sources();
    summary.set_anchor(Anchor {
        entry_id: 1,
        sequence: 0,
    });
    assert!(rebuild(&mut summary, &sources));
    let line = summary.summary();
    assert!(line.starts_with("7 changes"), "total first: {line}");
    assert!(line.contains("1 files changed"), "{line}");
    assert!(line.contains("1 commands & tests"), "{line}");
    assert!(line.contains("1 errors"), "{line}");
    assert!(line.contains("1 checkpoints"), "{line}");
    assert!(line.contains("1 decisions"), "{line}");
    assert!(line.contains("2 tool results"), "{line}");
    assert!(line.ends_with("1 failed"), "failures last: {line}");
}

#[test]
fn stale_ids_drop_out_on_rebuild() {
    let mut summary = ChangeSummary::new();
    let mut sources = sample_sources();
    summary.set_anchor(Anchor {
        entry_id: 1,
        sequence: 0,
    });
    assert!(rebuild(&mut summary, &sources));
    assert!(summary.items().iter().any(|i| i.entry_id == 4));

    // Drop the failed-build entry (compaction / clear). After a rebuild the item
    // keyed by its id is gone, and so is the failure it carried.
    sources.retain(|s| s.id != 4);
    assert!(rebuild(&mut summary, &sources));
    assert!(
        summary.items().iter().all(|i| i.entry_id != 4),
        "the dropped id falls out",
    );
    assert_eq!(summary.failed_count(), 0, "its failure flag drops with it");
}
