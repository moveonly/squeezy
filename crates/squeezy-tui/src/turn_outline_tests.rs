//! Unit tests for the Semantic Turn Outline model (§12.2.1). Pure: no terminal,
//! no rendering — they exercise deterministic title cleaning, the title-less
//! fallback, status folding, ordering, the empty case, the staleness fast path,
//! forward/backward navigation, kind/failed counts, and the summary.

use super::*;

/// An ok user entry the tests mutate one field at a time.
fn entry(id: u64, kind: OutlineKind, raw_title: &str) -> OutlineEntry {
    OutlineEntry {
        id,
        revision: 0,
        kind,
        is_error: false,
        raw_title: raw_title.to_string(),
    }
}

#[test]
fn node_for_entry_carries_id_kind_and_clean_title() {
    let node = node_for_entry(&entry(7, OutlineKind::UserTurn, "run the build please"));
    assert_eq!(node.entry_id, 7);
    assert_eq!(node.kind, OutlineKind::UserTurn);
    assert_eq!(node.status, OutlineStatus::Ok);
    assert_eq!(node.title, "run the build please");
}

#[test]
fn clean_title_takes_first_nonblank_line_and_collapses_whitespace() {
    // Leading blank lines are skipped; interior whitespace collapses to single
    // spaces; the result is trimmed.
    let title = clean_title(
        "\n\n  hello    world\t\ttab  \nsecond",
        OutlineKind::Assistant,
    );
    assert_eq!(title, "hello world tab");
}

#[test]
fn clean_title_falls_back_to_kind_label_when_blank() {
    // Empty source -> "(kind)" rather than inventing text.
    assert_eq!(clean_title("", OutlineKind::Assistant), "(assistant)");
    assert_eq!(clean_title("   \n\t ", OutlineKind::ToolRun), "(tool)");
}

#[test]
fn clean_title_caps_long_titles_with_ellipsis() {
    let long = "x".repeat(200);
    let title = clean_title(&long, OutlineKind::UserTurn);
    // Capped to TITLE_CAP chars + a one-char ellipsis.
    assert_eq!(title.chars().count(), TITLE_CAP + 1);
    assert!(title.ends_with('\u{2026}'), "ellipsis appended: {title}");
}

#[test]
fn node_retains_full_title_when_truncated() {
    // A long source caps the displayed `title` but retains the uncapped
    // `full_title` so the overlay can reveal the cut tail in place.
    let long = "word ".repeat(40);
    let node = node_for_entry(&entry(9, OutlineKind::Assistant, &long));
    assert!(node.is_truncated(), "long title should report truncation");
    assert_eq!(node.title.chars().count(), TITLE_CAP + 1);
    assert!(node.title.ends_with('\u{2026}'));
    assert!(
        node.full_title.chars().count() > TITLE_CAP,
        "full title must keep the uncapped text: {}",
        node.full_title
    );
    assert!(node.full_title.starts_with("word word"));
}

#[test]
fn node_full_title_matches_title_when_short() {
    // A short title is not truncated: `full_title` equals `title` and the node
    // reports no truncation, so the overlay paints no reveal row.
    let node = node_for_entry(&entry(2, OutlineKind::UserTurn, "short label"));
    assert_eq!(node.title, "short label");
    assert_eq!(node.full_title, "short label");
    assert!(!node.is_truncated());
}

#[test]
fn clean_title_caps_on_char_boundary_for_multibyte() {
    // A run of multibyte chars must cap without panicking on a byte boundary.
    let long = "é".repeat(200);
    let title = clean_title(&long, OutlineKind::Note);
    assert_eq!(title.chars().count(), TITLE_CAP + 1);
}

#[test]
fn status_folds_error_flag() {
    let mut e = entry(3, OutlineKind::ToolRun, "shell");
    e.is_error = true;
    let node = node_for_entry(&e);
    assert_eq!(node.status, OutlineStatus::Failed);
}

#[test]
fn rebuild_preserves_transcript_order() {
    let entries = vec![
        entry(1, OutlineKind::UserTurn, "prompt"),
        entry(2, OutlineKind::Reasoning, "thinking"),
        entry(3, OutlineKind::ToolRun, "shell"),
        entry(4, OutlineKind::Assistant, "answer"),
    ];
    let mut index = OutlineIndex::new();
    let fp = OutlineIndex::fingerprint_of(entries.iter());
    assert!(index.rebuild_if_stale(fp, &entries), "first build runs");
    let ids: Vec<u64> = index.nodes().iter().map(|n| n.entry_id).collect();
    assert_eq!(ids, vec![1, 2, 3, 4], "outline keeps transcript order");
    assert_eq!(index.len(), 4);
    assert!(!index.is_empty());
}

#[test]
fn empty_transcript_outlines_to_nothing() {
    let mut index = OutlineIndex::new();
    let fp = OutlineIndex::fingerprint_of(std::iter::empty());
    assert!(
        index.rebuild_if_stale(fp, &[]),
        "first build runs even on empty"
    );
    assert!(index.is_empty());
    assert_eq!(index.len(), 0);
    assert_eq!(index.summary(), "");
    assert_eq!(index.next_index(None), None, "no node to jump to");
    assert_eq!(index.prev_index(None), None);
    assert_eq!(index.get(0), None);
}

#[test]
fn rebuild_is_a_no_op_when_fingerprint_is_unchanged() {
    let entries = vec![entry(1, OutlineKind::UserTurn, "prompt")];
    let mut index = OutlineIndex::new();
    let fp = OutlineIndex::fingerprint_of(entries.iter());
    assert!(index.rebuild_if_stale(fp, &entries), "first build runs");
    let stored = index.fingerprint();
    // Same fingerprint: the fast path returns false and rebuilds nothing.
    assert!(
        !index.rebuild_if_stale(fp, &entries),
        "unchanged fingerprint is the zero-idle-cost fast path",
    );
    assert_eq!(index.fingerprint(), stored, "fingerprint unchanged");
}

#[test]
fn revision_bump_moves_the_fingerprint_and_rebuilds() {
    let mut entries = vec![entry(1, OutlineKind::Assistant, "draft")];
    let mut index = OutlineIndex::new();
    let fp1 = OutlineIndex::fingerprint_of(entries.iter());
    index.rebuild_if_stale(fp1, &entries);

    // A revision bump + new title (a streamed edit) moves the fingerprint.
    entries[0].revision = 1;
    entries[0].raw_title = "final answer".to_string();
    let fp2 = OutlineIndex::fingerprint_of(entries.iter());
    assert_ne!(fp1, fp2, "a revision/title change moves the fingerprint");
    assert!(
        index.rebuild_if_stale(fp2, &entries),
        "stale -> rebuild runs"
    );
    assert_eq!(index.get(0).unwrap().title, "final answer");
}

#[test]
fn dropped_ids_fall_out_on_rebuild() {
    let entries = vec![
        entry(1, OutlineKind::UserTurn, "a"),
        entry(2, OutlineKind::Assistant, "b"),
    ];
    let mut index = OutlineIndex::new();
    let fp = OutlineIndex::fingerprint_of(entries.iter());
    index.rebuild_if_stale(fp, &entries);
    assert_eq!(index.len(), 2);

    // The first entry was dropped (compaction): only the survivor remains.
    let after = vec![entry(2, OutlineKind::Assistant, "b")];
    let fp2 = OutlineIndex::fingerprint_of(after.iter());
    index.rebuild_if_stale(fp2, &after);
    assert_eq!(index.len(), 1);
    assert_eq!(index.get(0).unwrap().entry_id, 2);
}

#[test]
fn next_index_walks_forward_and_wraps() {
    let entries = vec![
        entry(1, OutlineKind::UserTurn, "a"),
        entry(2, OutlineKind::ToolRun, "b"),
        entry(3, OutlineKind::Assistant, "c"),
    ];
    let mut index = OutlineIndex::new();
    index.rebuild_if_stale(OutlineIndex::fingerprint_of(entries.iter()), &entries);
    assert_eq!(index.next_index(None), Some(0));
    assert_eq!(index.next_index(Some(0)), Some(1));
    assert_eq!(index.next_index(Some(2)), Some(0), "wraps at the end");
    // Out-of-range cursor wraps to the first.
    assert_eq!(index.next_index(Some(99)), Some(0));
}

#[test]
fn prev_index_walks_backward_and_wraps() {
    let entries = vec![
        entry(1, OutlineKind::UserTurn, "a"),
        entry(2, OutlineKind::ToolRun, "b"),
        entry(3, OutlineKind::Assistant, "c"),
    ];
    let mut index = OutlineIndex::new();
    index.rebuild_if_stale(OutlineIndex::fingerprint_of(entries.iter()), &entries);
    assert_eq!(index.prev_index(None), Some(2));
    assert_eq!(index.prev_index(Some(2)), Some(1));
    assert_eq!(index.prev_index(Some(0)), Some(2), "wraps at the start");
}

#[test]
fn counts_and_summary_report_kinds_and_failures() {
    let mut tool = entry(3, OutlineKind::ToolRun, "shell");
    tool.is_error = true; // a failed tool
    let entries = vec![
        entry(1, OutlineKind::UserTurn, "ask"),
        entry(2, OutlineKind::UserTurn, "ask again"),
        tool,
        entry(4, OutlineKind::Error, "boom"),
        entry(5, OutlineKind::Assistant, "done"),
    ];
    // The bare error line is itself a failure.
    let mut entries = entries;
    entries[3].is_error = true;

    let mut index = OutlineIndex::new();
    index.rebuild_if_stale(OutlineIndex::fingerprint_of(entries.iter()), &entries);

    assert_eq!(index.count_of(OutlineKind::UserTurn), 2);
    assert_eq!(index.count_of(OutlineKind::ToolRun), 1);
    assert_eq!(index.count_of(OutlineKind::Error), 1);
    assert_eq!(index.count_of(OutlineKind::Reasoning), 0);
    assert_eq!(index.failed_count(), 2, "failed tool + error line");

    let summary = index.summary();
    assert!(summary.starts_with("5 sections"), "{summary}");
    assert!(summary.contains("2 user"), "{summary}");
    assert!(summary.contains("1 tool"), "{summary}");
    assert!(summary.contains("2 failed"), "{summary}");
}

#[test]
fn singular_section_word_when_only_one_node() {
    let entries = vec![entry(1, OutlineKind::UserTurn, "hi")];
    let mut index = OutlineIndex::new();
    index.rebuild_if_stale(OutlineIndex::fingerprint_of(entries.iter()), &entries);
    assert!(
        index.summary().starts_with("1 section "),
        "{}",
        index.summary()
    );
}

#[test]
fn all_kinds_have_distinct_nonempty_labels() {
    let mut seen = std::collections::HashSet::new();
    for kind in OutlineKind::ALL.iter().copied() {
        let label = kind.label();
        assert!(!label.is_empty(), "{kind:?} has a label");
        assert!(label.is_ascii(), "{kind:?} label is ASCII");
        assert!(seen.insert(label), "{kind:?} label {label} is unique");
    }
}
