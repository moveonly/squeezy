use super::*;

#[test]
fn record_inserts_newest_first_and_returns_distinct_ids() {
    let mut store = ClipboardHistoryStore::new();
    let a = store.record("first", "entry");
    let b = store.record("second", "viewport");
    assert!(a != b, "ids must be distinct");
    assert_eq!(store.len(), 2);
    // Newest first.
    assert_eq!(store.entries()[0].text, "second");
    assert_eq!(store.entries()[1].text, "first");
    // The cursor follows the freshest entry to the front.
    assert_eq!(store.selected_index(), 0);
    assert_eq!(
        store.selected_entry().map(|e| e.text.as_str()),
        Some("second")
    );
}

#[test]
fn record_collapses_back_to_back_duplicate_of_newest() {
    let mut store = ClipboardHistoryStore::new();
    let first = store.record("dup", "entry");
    let again = store.record("dup", "entry");
    assert_eq!(first, again, "an exact repeat returns the existing id");
    assert_eq!(store.len(), 1, "no duplicate row is inserted");
}

#[test]
fn record_does_not_collapse_when_label_differs() {
    let mut store = ClipboardHistoryStore::new();
    store.record("same", "entry");
    store.record("same", "viewport");
    // Same text but different scope is a distinct, recoverable copy.
    assert_eq!(store.len(), 2);
}

#[test]
fn entry_cap_evicts_oldest_unpinned() {
    let mut store = ClipboardHistoryStore::new();
    for i in 0..(MAX_ENTRIES + 5) {
        store.record(&format!("payload-{i}"), "entry");
    }
    assert_eq!(store.len(), MAX_ENTRIES, "count is capped");
    // The newest is at the front; the oldest survivors were the most recent ones.
    assert_eq!(
        store.entries()[0].text,
        format!("payload-{}", MAX_ENTRIES + 4)
    );
}

#[test]
fn pinned_entry_survives_entry_cap_eviction() {
    let mut store = ClipboardHistoryStore::new();
    let pinned_id = store.record("keep-me", "entry");
    assert_eq!(store.toggle_pin(pinned_id), Some(true));
    // Flood the store well past the cap.
    for i in 0..(MAX_ENTRIES + 10) {
        store.record(&format!("noise-{i}"), "entry");
    }
    assert!(
        store.text_of(pinned_id).is_some(),
        "a pinned entry is never evicted by the entry cap"
    );
    assert_eq!(store.text_of(pinned_id), Some("keep-me"));
}

#[test]
fn byte_cap_evicts_oldest_unpinned_until_total_fits() {
    let mut store = ClipboardHistoryStore::new();
    // Each payload is ~1/4 of the byte cap, so a handful overflows it.
    let chunk = "x".repeat(MAX_TOTAL_BYTES / 4);
    for i in 0..8 {
        store.record(&format!("{chunk}{i}"), "viewport");
    }
    assert!(
        store.total_bytes() <= MAX_TOTAL_BYTES,
        "byte cap holds: {} <= {}",
        store.total_bytes(),
        MAX_TOTAL_BYTES
    );
    assert!(!store.is_empty(), "the newest entries are retained");
}

#[test]
fn single_oversized_payload_is_still_recorded() {
    let mut store = ClipboardHistoryStore::new();
    store.record("small", "entry");
    let huge = "y".repeat(MAX_TOTAL_BYTES * 2);
    let huge_id = store.record(&huge, "transcript");
    // The oversized payload forces every unpinned entry out but is itself kept —
    // truncating it would corrupt a re-copy.
    assert_eq!(store.len(), 1);
    assert_eq!(store.text_of(huge_id).map(str::len), Some(huge.len()));
}

#[test]
fn select_up_and_down_saturate_and_track_rows() {
    let mut store = ClipboardHistoryStore::new();
    store.record("oldest", "entry");
    store.record("middle", "entry");
    store.record("newest", "entry");
    assert_eq!(store.selected_index(), 0);
    store.select_up(); // already at top → no-op
    assert_eq!(store.selected_index(), 0);
    store.select_down();
    assert_eq!(store.selected_index(), 1);
    store.select_down();
    assert_eq!(store.selected_index(), 2);
    store.select_down(); // at bottom → no-op
    assert_eq!(store.selected_index(), 2);
    assert_eq!(
        store.selected_entry().map(|e| e.text.as_str()),
        Some("oldest")
    );
}

#[test]
fn select_id_points_cursor_at_stable_entry() {
    let mut store = ClipboardHistoryStore::new();
    let target = store.record("target", "entry");
    store.record("newer", "entry");
    assert!(store.select_id(target));
    assert_eq!(store.selected_entry().map(|e| e.id), Some(target));
    assert!(!store.select_id(9_999), "unknown id selects nothing");
}

#[test]
fn delete_removes_entry_and_keeps_selection_valid() {
    let mut store = ClipboardHistoryStore::new();
    store.record("a", "entry");
    let mid = store.record("b", "entry");
    store.record("c", "entry");
    // Move selection to the last row, then delete a middle row.
    store.select_down();
    store.select_down();
    assert_eq!(store.selected_index(), 2);
    assert!(store.delete(mid));
    assert_eq!(store.len(), 2);
    // Selection clamps back into range.
    assert!(store.selected_index() < store.len());
    assert!(!store.delete(mid), "deleting an already-gone id is a no-op");
}

#[test]
fn clear_drops_everything_including_pinned() {
    let mut store = ClipboardHistoryStore::new();
    let pinned = store.record("pinned", "entry");
    store.toggle_pin(pinned);
    store.record("loose", "entry");
    store.clear();
    assert!(store.is_empty(), "clear is unconditional, pinned included");
    assert_eq!(store.selected_index(), 0);
    assert!(store.selected_entry().is_none());
}

#[test]
fn toggle_pin_flips_and_reports_state() {
    let mut store = ClipboardHistoryStore::new();
    let id = store.record("p", "entry");
    assert_eq!(store.toggle_pin(id), Some(true));
    assert_eq!(store.toggle_pin(id), Some(false));
    assert_eq!(store.toggle_pin(42), None);
}

#[test]
fn preview_flattens_newlines_and_clips_to_bound() {
    let mut store = ClipboardHistoryStore::new();
    store.record("line one\nline two\tindented", "entry");
    let preview = store.entries()[0].preview();
    assert!(!preview.contains('\n'), "newlines flattened");
    assert!(!preview.contains('\t'), "tabs flattened");
    assert!(preview.contains("line one line two"));

    // A payload longer than the bound is clipped with a trailing ellipsis.
    let long = "z".repeat(PREVIEW_CHARS + 50);
    store.record(&long, "transcript");
    let clipped = store.entries()[0].preview();
    assert_eq!(clipped.chars().count(), PREVIEW_CHARS);
    assert!(clipped.ends_with('…'));
}

#[test]
fn empty_store_has_no_selection() {
    let store = ClipboardHistoryStore::new();
    assert!(store.is_empty());
    assert_eq!(store.len(), 0);
    assert!(store.selected_entry().is_none());
    assert_eq!(store.total_bytes(), 0);
}

#[test]
fn text_of_returns_full_payload_for_re_copy() {
    let mut store = ClipboardHistoryStore::new();
    let id = store.record("the full payload\nwith two lines", "entry");
    assert_eq!(store.text_of(id), Some("the full payload\nwith two lines"));
    assert_eq!(store.text_of(id + 100), None);
}
