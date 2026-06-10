//! Unit tests for the Local Transcript Index (§12.5.1) pure model.

use super::*;

fn entry(id: u64, primary: EntryCategory) -> IndexedEntry {
    IndexedEntry {
        id,
        revision: 0,
        primary,
        is_error: false,
        tool_name: None,
    }
}

fn tool(id: u64, name: &str, is_error: bool) -> IndexedEntry {
    IndexedEntry {
        id,
        revision: 0,
        primary: EntryCategory::ToolCall,
        is_error,
        tool_name: Some(name.to_string()),
    }
}

fn build(entries: &[IndexedEntry]) -> TranscriptIndex {
    let mut index = TranscriptIndex::new();
    let fp = TranscriptIndex::fingerprint_of(entries.iter());
    index.rebuild_if_stale(fp, entries);
    index
}

#[test]
fn empty_index_has_no_entries_and_empty_summary() {
    let index = build(&[]);
    assert_eq!(index.total(), 0);
    assert_eq!(index.count(EntryCategory::ToolCall), 0);
    assert!(index.ids(EntryCategory::UserTurn).is_empty());
    assert_eq!(index.summary(), "");
    assert!(index.non_empty_categories().is_empty());
    assert_eq!(index.next_in(EntryCategory::ToolCall, None), None);
    assert_eq!(index.prev_in(EntryCategory::Error, Some(7)), None);
}

#[test]
fn buckets_group_by_primary_category_in_transcript_order() {
    let entries = [
        entry(1, EntryCategory::UserTurn),
        entry(2, EntryCategory::Assistant),
        tool(3, "shell", false),
        tool(4, "edit", false),
        entry(5, EntryCategory::UserTurn),
    ];
    let index = build(&entries);
    assert_eq!(index.total(), 5);
    assert_eq!(index.ids(EntryCategory::UserTurn), &[1, 5]);
    assert_eq!(index.ids(EntryCategory::Assistant), &[2]);
    assert_eq!(index.ids(EntryCategory::ToolCall), &[3, 4]);
    assert_eq!(index.count(EntryCategory::UserTurn), 2);
    assert_eq!(index.category_of(3), Some(EntryCategory::ToolCall));
    assert_eq!(index.category_of(999), None);
}

#[test]
fn failed_tool_appears_in_both_tool_and_error_buckets() {
    let entries = [
        tool(1, "shell", false),
        tool(2, "shell", true),
        entry(3, EntryCategory::UserTurn),
    ];
    let index = build(&entries);
    // Both tool calls counted under ToolCall; only the failed one cross-cuts to Error.
    assert_eq!(index.ids(EntryCategory::ToolCall), &[1, 2]);
    assert_eq!(index.ids(EntryCategory::Error), &[2]);
    // Primary identity is preserved: a failed tool is still a ToolCall.
    assert_eq!(index.category_of(2), Some(EntryCategory::ToolCall));
    // total counts each entry once under its primary, not the cross-cut.
    assert_eq!(index.total(), 3);
}

#[test]
fn primary_error_entry_is_not_double_counted_in_error_bucket() {
    // A Log classified primarily as Error with is_error also set must not appear
    // twice in the error bucket.
    let entries = [IndexedEntry {
        id: 1,
        revision: 0,
        primary: EntryCategory::Error,
        is_error: true,
        tool_name: None,
    }];
    let index = build(&entries);
    assert_eq!(index.ids(EntryCategory::Error), &[1]);
}

#[test]
fn tool_name_lookup_groups_by_name() {
    let entries = [
        tool(1, "shell", false),
        tool(2, "edit", false),
        tool(3, "shell", true),
    ];
    let index = build(&entries);
    assert_eq!(index.ids_for_tool("shell"), &[1, 3]);
    assert_eq!(index.ids_for_tool("edit"), &[2]);
    assert!(index.ids_for_tool("missing").is_empty());
}

#[test]
fn next_in_wraps_and_handles_unknown_anchor() {
    let entries = [
        tool(10, "a", false),
        tool(20, "b", false),
        tool(30, "c", false),
    ];
    let index = build(&entries);
    assert_eq!(index.next_in(EntryCategory::ToolCall, None), Some(10));
    assert_eq!(index.next_in(EntryCategory::ToolCall, Some(10)), Some(20));
    assert_eq!(index.next_in(EntryCategory::ToolCall, Some(30)), Some(10)); // wrap
    // Unknown anchor falls back to the first.
    assert_eq!(index.next_in(EntryCategory::ToolCall, Some(999)), Some(10));
}

#[test]
fn prev_in_wraps_and_handles_unknown_anchor() {
    let entries = [
        tool(10, "a", false),
        tool(20, "b", false),
        tool(30, "c", false),
    ];
    let index = build(&entries);
    assert_eq!(index.prev_in(EntryCategory::ToolCall, Some(20)), Some(10));
    assert_eq!(index.prev_in(EntryCategory::ToolCall, Some(10)), Some(30)); // wrap
    assert_eq!(index.prev_in(EntryCategory::ToolCall, None), Some(30));
    assert_eq!(index.prev_in(EntryCategory::ToolCall, Some(999)), Some(30));
}

#[test]
fn rebuild_is_skipped_when_fingerprint_unchanged() {
    let entries = [entry(1, EntryCategory::UserTurn), tool(2, "shell", false)];
    let mut index = TranscriptIndex::new();
    let fp = TranscriptIndex::fingerprint_of(entries.iter());

    // First build runs.
    assert!(index.rebuild_if_stale(fp, &entries));
    assert_eq!(index.fingerprint(), fp);
    // Same fingerprint: the zero-idle-cost fast path skips the rebuild.
    assert!(!index.rebuild_if_stale(fp, &entries));
    assert!(!index.rebuild_if_stale(fp, &entries));
}

#[test]
fn empty_transcript_builds_once_then_skips() {
    let mut index = TranscriptIndex::new();
    let fp = TranscriptIndex::fingerprint_of(std::iter::empty());
    // A genuinely empty transcript still records "built" so it is not re-walked.
    assert!(index.rebuild_if_stale(fp, &[]));
    assert!(!index.rebuild_if_stale(fp, &[]));
}

#[test]
fn revision_bump_changes_fingerprint_and_triggers_rebuild() {
    let v0 = [tool(1, "shell", false)];
    let mut bumped = v0.clone();
    bumped[0].revision = 1;

    let fp0 = TranscriptIndex::fingerprint_of(v0.iter());
    let fp1 = TranscriptIndex::fingerprint_of(bumped.iter());
    assert_ne!(fp0, fp1, "a revision bump must move the fingerprint");

    let mut index = TranscriptIndex::new();
    assert!(index.rebuild_if_stale(fp0, &v0));
    assert!(index.rebuild_if_stale(fp1, &bumped));
}

#[test]
fn append_changes_fingerprint() {
    let v0 = [entry(1, EntryCategory::UserTurn)];
    let v1 = [
        entry(1, EntryCategory::UserTurn),
        entry(2, EntryCategory::Assistant),
    ];
    assert_ne!(
        TranscriptIndex::fingerprint_of(v0.iter()),
        TranscriptIndex::fingerprint_of(v1.iter()),
    );
}

#[test]
fn category_change_changes_fingerprint() {
    let mut a = [entry(1, EntryCategory::Note)];
    let b = {
        let mut b = a.clone();
        b[0].primary = EntryCategory::Subagent;
        b
    };
    let fa = TranscriptIndex::fingerprint_of(a.iter());
    let fb = TranscriptIndex::fingerprint_of(b.iter());
    assert_ne!(fa, fb);
    // sanity: mutating back restores it
    a[0].primary = EntryCategory::Subagent;
    assert_eq!(TranscriptIndex::fingerprint_of(a.iter()), fb);
}

#[test]
fn stale_ids_are_dropped_on_rebuild() {
    let v0 = [
        entry(1, EntryCategory::UserTurn),
        tool(2, "shell", false),
        tool(3, "edit", false),
    ];
    // Entry 2 is removed in the next revision.
    let v1 = [entry(1, EntryCategory::UserTurn), tool(3, "edit", false)];

    let mut index = TranscriptIndex::new();
    index.rebuild_if_stale(TranscriptIndex::fingerprint_of(v0.iter()), &v0);
    assert_eq!(index.category_of(2), Some(EntryCategory::ToolCall));

    index.rebuild_if_stale(TranscriptIndex::fingerprint_of(v1.iter()), &v1);
    // The dropped id is gone from both the id map and the tool bucket.
    assert_eq!(index.category_of(2), None);
    assert!(index.ids_for_tool("shell").is_empty());
    assert_eq!(index.ids(EntryCategory::ToolCall), &[3]);
}

#[test]
fn summary_lists_populated_buckets_in_display_order() {
    let entries = [
        entry(1, EntryCategory::UserTurn),
        entry(2, EntryCategory::UserTurn),
        tool(3, "shell", true),
    ];
    let index = build(&entries);
    // UserTurn before ToolCall before Error (display order), error cross-cut counted.
    assert_eq!(
        index.summary(),
        "2 user turns \u{00b7} 1 tool calls \u{00b7} 1 errors"
    );
}

#[test]
fn non_empty_categories_skips_empty_buckets_in_order() {
    let entries = [
        tool(1, "shell", true),
        entry(2, EntryCategory::Note),
        entry(3, EntryCategory::UserTurn),
    ];
    let index = build(&entries);
    // Display order: UserTurn, ToolCall, Error, Note (others empty).
    assert_eq!(
        index.non_empty_categories(),
        vec![
            EntryCategory::UserTurn,
            EntryCategory::ToolCall,
            EntryCategory::Error,
            EntryCategory::Note,
        ],
    );
}

#[test]
fn large_transcript_indexes_in_order() {
    // Perf/scale smoke: 10k entries build and answer lookups deterministically.
    let mut entries = Vec::with_capacity(10_000);
    for i in 0..10_000u64 {
        let category = if i % 100 == 0 {
            EntryCategory::UserTurn
        } else {
            EntryCategory::ToolCall
        };
        entries.push(entry(i, category));
    }
    let index = build(&entries);
    assert_eq!(index.total(), 10_000);
    assert_eq!(index.count(EntryCategory::UserTurn), 100);
    assert_eq!(index.count(EntryCategory::ToolCall), 9_900);
    // Navigation still wraps correctly at the bucket boundary.
    assert_eq!(index.next_in(EntryCategory::UserTurn, Some(9_900)), Some(0));
}
