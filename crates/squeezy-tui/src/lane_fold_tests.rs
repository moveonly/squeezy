//! Unit tests for the Collapsible Reasoning/Tool Lanes model (§12.2.2). Pure: no
//! terminal, no rendering — they exercise the lane taxonomy, the deterministic
//! preview cleaning, the `(entry_id, lane_id)`-keyed fold store (toggle / collapse
//! / expand / collapse-all / expand-all, generation bumps, persistence), the
//! panel projection (collapse state read-through, staleness fast path, dropped
//! ids, navigation, counts, summary), and the "errored lanes keep visible
//! headers" invariant.

use super::*;

/// A non-error lane source the tests mutate one field at a time.
fn lane(id: LaneId, line_count: usize, preview: &str) -> LaneEntry {
    LaneEntry {
        id,
        line_count,
        is_error: false,
        preview: preview.to_string(),
    }
}

// ---- LaneId taxonomy ----

#[test]
fn all_lane_ids_have_distinct_nonempty_ascii_labels() {
    let mut labels = std::collections::HashSet::new();
    for id in LaneId::ALL.iter().copied() {
        let label = id.label();
        assert!(!label.is_empty(), "{id:?} has a label");
        assert!(label.is_ascii(), "{id:?} label is ASCII");
        assert!(labels.insert(label), "{id:?} label {label} is unique");
    }
}

#[test]
fn rebuild_emits_lanes_in_canonical_taxonomy_order() {
    // Sources pushed out of order must surface in `LaneId::ALL` order (reasoning
    // before assistant text before tool output).
    let store = LaneFoldStore::new();
    let sources = vec![
        lane(LaneId::ToolOutput, 3, "out"),
        lane(LaneId::Reasoning, 2, "think"),
        lane(LaneId::AssistantText, 1, "answer"),
    ];
    let mut panel = LanePanel::new();
    panel.rebuild_if_stale(
        LanePanel::fingerprint_of(Some(1), &sources, store.generation()),
        Some(1),
        &sources,
        &store,
    );
    let ids: Vec<LaneId> = panel.lanes().iter().map(|lane| lane.id()).collect();
    assert_eq!(
        ids,
        vec![LaneId::Reasoning, LaneId::AssistantText, LaneId::ToolOutput],
        "lanes read in canonical taxonomy order regardless of push order",
    );
}

// ---- preview cleaning ----

#[test]
fn clean_preview_takes_first_nonblank_line_and_collapses_whitespace() {
    let preview = clean_preview("\n\n  cargo    build\t\t--release  \nsecond line");
    assert_eq!(preview, "cargo build --release");
}

#[test]
fn clean_preview_is_empty_for_blank_source() {
    assert_eq!(clean_preview(""), "");
    assert_eq!(clean_preview("   \n\t "), "");
}

#[test]
fn clean_preview_caps_long_lines_with_ellipsis_on_char_boundary() {
    let long = "é".repeat(200);
    let preview = clean_preview(&long);
    assert_eq!(preview.chars().count(), PREVIEW_CAP + 1);
    assert!(
        preview.ends_with('\u{2026}'),
        "ellipsis appended: {preview}"
    );
}

// ---- LaneFoldStore: (entry_id, lane_id) keyed fold state ----

#[test]
fn toggle_flips_collapse_state_and_bumps_generation() {
    let mut store = LaneFoldStore::new();
    let key = LaneKey::new(7, LaneId::ToolOutput);
    assert!(!store.is_collapsed(key), "lanes start expanded");
    let g0 = store.generation();

    assert!(store.toggle(key), "first toggle collapses");
    assert!(store.is_collapsed(key));
    assert_ne!(store.generation(), g0, "a toggle bumps the generation");
    assert_eq!(store.collapsed_count(), 1);

    assert!(!store.toggle(key), "second toggle expands");
    assert!(!store.is_collapsed(key));
    assert_eq!(store.collapsed_count(), 0);
}

#[test]
fn fold_state_is_keyed_by_both_entry_and_lane() {
    let mut store = LaneFoldStore::new();
    // Same lane id, two different entries: collapsing one must not collapse the
    // other (the key is the pair, not just the lane id).
    let a = LaneKey::new(1, LaneId::ToolOutput);
    let b = LaneKey::new(2, LaneId::ToolOutput);
    store.collapse(a);
    assert!(store.is_collapsed(a));
    assert!(
        !store.is_collapsed(b),
        "a different entry's same lane stays open"
    );

    // Same entry, two different lanes: independent too.
    let c = LaneKey::new(1, LaneId::Reasoning);
    assert!(
        !store.is_collapsed(c),
        "a different lane of the same entry stays open"
    );
}

#[test]
fn collapse_and_expand_are_idempotent_and_bump_only_on_change() {
    let mut store = LaneFoldStore::new();
    let key = LaneKey::new(3, LaneId::Reasoning);

    store.collapse(key);
    let g1 = store.generation();
    store.collapse(key); // no-op
    assert_eq!(store.generation(), g1, "a redundant collapse does not bump");
    assert!(store.is_collapsed(key));

    store.expand(key);
    let g2 = store.generation();
    store.expand(key); // no-op
    assert_eq!(store.generation(), g2, "a redundant expand does not bump");
    assert!(!store.is_collapsed(key));
}

#[test]
fn collapse_all_and_expand_all_act_in_one_pass() {
    let mut store = LaneFoldStore::new();
    let keys = vec![
        LaneKey::new(5, LaneId::Reasoning),
        LaneKey::new(5, LaneId::AssistantText),
        LaneKey::new(5, LaneId::ToolOutput),
    ];
    store.collapse_all(keys.clone());
    assert_eq!(store.collapsed_count(), 3);
    for key in &keys {
        assert!(store.is_collapsed(*key));
    }

    let g = store.generation();
    store.collapse_all(keys.clone()); // all already collapsed -> no bump
    assert_eq!(
        store.generation(),
        g,
        "redundant collapse-all does not bump"
    );

    store.expand_all(keys.clone());
    assert_eq!(store.collapsed_count(), 0);
}

// ---- LanePanel projection ----

#[test]
fn rebuild_projects_lanes_with_collapse_state_read_through() {
    let mut store = LaneFoldStore::new();
    // Collapse the tool-output lane of entry 9 before building.
    store.collapse(LaneKey::new(9, LaneId::ToolOutput));

    let sources = vec![
        lane(LaneId::ToolInput, 1, "shell"),
        lane(LaneId::ToolOutput, 12, "compiling..."),
    ];
    let mut panel = LanePanel::new();
    let fp = LanePanel::fingerprint_of(Some(9), &sources, store.generation());
    assert!(
        panel.rebuild_if_stale(fp, Some(9), &sources, &store),
        "first build runs"
    );

    assert_eq!(panel.len(), 2);
    assert_eq!(panel.entry_id(), Some(9));
    let input = panel.get(0).unwrap();
    assert_eq!(input.id(), LaneId::ToolInput);
    assert!(!input.collapsed, "tool input was not collapsed");
    assert!(input.body_visible());
    let output = panel.get(1).unwrap();
    assert_eq!(output.id(), LaneId::ToolOutput);
    assert!(
        output.collapsed,
        "tool output's persisted collapse is read through"
    );
    assert!(!output.body_visible(), "a collapsed lane hides its body");
    assert!(output.always_visible(), "but its header still paints");
    assert_eq!(output.line_count, 12);
    assert_eq!(output.preview, "compiling...");
}

#[test]
fn errored_lane_keeps_visible_header_even_when_collapsed() {
    // The spec's named risk mitigation: a collapsed error lane must still show
    // its header so a failure is never silently hidden.
    let mut store = LaneFoldStore::new();
    let key = LaneKey::new(4, LaneId::Error);
    store.collapse(key);

    let sources = vec![LaneEntry {
        id: LaneId::Error,
        line_count: 3,
        is_error: true,
        preview: "error: cannot find value".to_string(),
    }];
    let mut panel = LanePanel::new();
    let fp = LanePanel::fingerprint_of(Some(4), &sources, store.generation());
    panel.rebuild_if_stale(fp, Some(4), &sources, &store);

    let err = panel.get(0).unwrap();
    assert!(err.collapsed, "the error lane's body is collapsed");
    assert!(err.is_error);
    assert!(
        err.always_visible(),
        "an errored lane keeps its visible header even when collapsed",
    );
    assert_eq!(panel.error_count(), 1);
}

#[test]
fn rebuild_is_a_no_op_when_fingerprint_is_unchanged() {
    let store = LaneFoldStore::new();
    let sources = vec![lane(LaneId::AssistantText, 4, "here is the answer")];
    let mut panel = LanePanel::new();
    let fp = LanePanel::fingerprint_of(Some(1), &sources, store.generation());
    assert!(
        panel.rebuild_if_stale(fp, Some(1), &sources, &store),
        "first build runs"
    );
    let stored = panel.fingerprint();
    assert!(
        !panel.rebuild_if_stale(fp, Some(1), &sources, &store),
        "unchanged fingerprint is the zero-idle-cost fast path",
    );
    assert_eq!(panel.fingerprint(), stored, "fingerprint unchanged");
}

#[test]
fn collapse_toggle_moves_the_fingerprint_and_rebuilds() {
    let mut store = LaneFoldStore::new();
    let sources = vec![lane(LaneId::ToolOutput, 8, "ok")];
    let mut panel = LanePanel::new();
    let fp1 = LanePanel::fingerprint_of(Some(2), &sources, store.generation());
    panel.rebuild_if_stale(fp1, Some(2), &sources, &store);
    assert!(!panel.get(0).unwrap().collapsed);

    // A toggle bumps the store generation, so the fingerprint moves and the panel
    // rebuilds with the new collapse state.
    store.toggle(LaneKey::new(2, LaneId::ToolOutput));
    let fp2 = LanePanel::fingerprint_of(Some(2), &sources, store.generation());
    assert_ne!(fp1, fp2, "a collapse toggle moves the fingerprint");
    assert!(
        panel.rebuild_if_stale(fp2, Some(2), &sources, &store),
        "stale -> rebuild"
    );
    assert!(panel.get(0).unwrap().collapsed, "the toggle is reflected");
}

#[test]
fn no_focused_entry_projects_an_empty_panel() {
    let store = LaneFoldStore::new();
    let sources = vec![lane(LaneId::AssistantText, 1, "hi")];
    let mut panel = LanePanel::new();
    // `None` entry id: nothing to fold, so the panel is empty regardless of the
    // lane sources.
    let fp = LanePanel::fingerprint_of(None, &sources, store.generation());
    assert!(
        panel.rebuild_if_stale(fp, None, &sources, &store),
        "first build runs"
    );
    assert!(panel.is_empty());
    assert_eq!(panel.len(), 0);
    assert_eq!(panel.entry_id(), None);
    assert_eq!(panel.summary(), "");
    assert_eq!(panel.next_index(None), None);
    assert_eq!(panel.prev_index(None), None);
    assert_eq!(panel.get(0), None);
}

#[test]
fn empty_lane_sources_project_an_empty_panel() {
    let store = LaneFoldStore::new();
    let mut panel = LanePanel::new();
    let fp = LanePanel::fingerprint_of(Some(1), &[], store.generation());
    assert!(
        panel.rebuild_if_stale(fp, Some(1), &[], &store),
        "first build runs even empty"
    );
    assert!(panel.is_empty());
    assert_eq!(panel.summary(), "");
}

#[test]
fn switching_focused_entry_drops_the_previous_lanes() {
    let store = LaneFoldStore::new();
    let a = vec![
        lane(LaneId::AssistantText, 2, "a"),
        lane(LaneId::Reasoning, 3, "b"),
    ];
    let b = vec![lane(LaneId::ToolOutput, 9, "c")];
    let mut panel = LanePanel::new();
    panel.rebuild_if_stale(
        LanePanel::fingerprint_of(Some(1), &a, store.generation()),
        Some(1),
        &a,
        &store,
    );
    assert_eq!(panel.len(), 2);

    // Focus moves to entry 2: only its lanes survive.
    panel.rebuild_if_stale(
        LanePanel::fingerprint_of(Some(2), &b, store.generation()),
        Some(2),
        &b,
        &store,
    );
    assert_eq!(panel.len(), 1);
    assert_eq!(panel.entry_id(), Some(2));
    assert_eq!(panel.get(0).unwrap().id(), LaneId::ToolOutput);
}

#[test]
fn keys_returns_every_lane_key_for_bulk_verbs() {
    let store = LaneFoldStore::new();
    let sources = vec![
        lane(LaneId::Reasoning, 1, "x"),
        lane(LaneId::AssistantText, 1, "y"),
    ];
    let mut panel = LanePanel::new();
    panel.rebuild_if_stale(
        LanePanel::fingerprint_of(Some(6), &sources, store.generation()),
        Some(6),
        &sources,
        &store,
    );
    let keys = panel.keys();
    assert_eq!(keys.len(), 2);
    assert!(keys.contains(&LaneKey::new(6, LaneId::Reasoning)));
    assert!(keys.contains(&LaneKey::new(6, LaneId::AssistantText)));
}

#[test]
fn next_and_prev_index_walk_and_wrap() {
    let store = LaneFoldStore::new();
    let sources = vec![
        lane(LaneId::Reasoning, 1, "a"),
        lane(LaneId::AssistantText, 1, "b"),
        lane(LaneId::ToolOutput, 1, "c"),
    ];
    let mut panel = LanePanel::new();
    panel.rebuild_if_stale(
        LanePanel::fingerprint_of(Some(1), &sources, store.generation()),
        Some(1),
        &sources,
        &store,
    );
    assert_eq!(panel.next_index(None), Some(0));
    assert_eq!(panel.next_index(Some(0)), Some(1));
    assert_eq!(panel.next_index(Some(2)), Some(0), "wraps at the end");
    assert_eq!(panel.next_index(Some(99)), Some(0), "out of range wraps");
    assert_eq!(panel.prev_index(None), Some(2));
    assert_eq!(panel.prev_index(Some(0)), Some(2), "wraps at the start");
    assert_eq!(panel.prev_index(Some(2)), Some(1));
}

#[test]
fn summary_reports_lanes_collapsed_and_errors() {
    let mut store = LaneFoldStore::new();
    store.collapse(LaneKey::new(1, LaneId::ToolOutput));
    let sources = vec![
        lane(LaneId::AssistantText, 2, "answer"),
        lane(LaneId::ToolOutput, 9, "out"),
        LaneEntry {
            id: LaneId::Error,
            line_count: 1,
            is_error: true,
            preview: "boom".to_string(),
        },
        LaneEntry {
            id: LaneId::Error,
            line_count: 1,
            is_error: true,
            preview: "kaboom".to_string(),
        },
    ];
    let mut panel = LanePanel::new();
    panel.rebuild_if_stale(
        LanePanel::fingerprint_of(Some(1), &sources, store.generation()),
        Some(1),
        &sources,
        &store,
    );
    let summary = panel.summary();
    assert!(summary.starts_with("4 lanes"), "{summary}");
    assert!(summary.contains("1 collapsed"), "{summary}");
    assert!(summary.contains("2 errors"), "{summary}");
}

#[test]
fn singular_lane_word_when_only_one_lane() {
    let store = LaneFoldStore::new();
    let sources = vec![lane(LaneId::AssistantText, 1, "hi")];
    let mut panel = LanePanel::new();
    panel.rebuild_if_stale(
        LanePanel::fingerprint_of(Some(1), &sources, store.generation()),
        Some(1),
        &sources,
        &store,
    );
    assert!(panel.summary().starts_with("1 lane"), "{}", panel.summary());
    assert!(
        !panel.summary().starts_with("1 lanes"),
        "singular: {}",
        panel.summary()
    );
}
