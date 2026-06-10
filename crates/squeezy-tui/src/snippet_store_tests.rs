use super::*;

fn source(rows: (usize, usize), text: &str) -> SnippetSource {
    SnippetSource {
        surface: SnippetSurface::Main,
        row_start: rows.0,
        row_end: rows.1,
        chars: text.chars().count(),
        bytes: text.len(),
    }
}

#[test]
fn save_inserts_newest_first_and_returns_distinct_ids() {
    let mut store = SnippetStore::new();
    let a = store
        .save("first body", source((0, 0), "first body"))
        .unwrap();
    let b = store
        .save("second body", source((1, 2), "second body"))
        .unwrap();
    assert!(a != b, "ids must be distinct");
    assert_eq!(store.len(), 2);
    // Newest first.
    assert_eq!(store.snippets()[0].text, "second body");
    assert_eq!(store.snippets()[1].text, "first body");
    // The cursor follows the freshest snippet to the front.
    assert_eq!(store.selected_index(), 0);
    assert_eq!(
        store.selected_snippet().map(|s| s.text.as_str()),
        Some("second body")
    );
}

#[test]
fn save_rejects_whitespace_only_text() {
    let mut store = SnippetStore::new();
    assert!(
        store
            .save("   \n\t  ", source((0, 1), "   \n\t  "))
            .is_none()
    );
    assert!(store.is_empty(), "nothing is saved for blank text");
}

#[test]
fn derive_name_uses_first_non_empty_line() {
    assert_eq!(derive_name("\n\n  hello world  \nsecond"), "hello world");
    // Whitespace inside the line is collapsed.
    assert_eq!(derive_name("a\tb   c"), "a b c");
    assert_eq!(derive_name("   "), "(empty snippet)");
}

#[test]
fn derive_name_clips_long_first_line_with_ellipsis() {
    let long = "x".repeat(NAME_CHARS + 20);
    let name = derive_name(&long);
    assert_eq!(name.chars().count(), NAME_CHARS, "clipped to the name cap");
    assert!(name.ends_with('…'), "clipped name ends with an ellipsis");
}

#[test]
fn preview_flattens_newlines_and_clips() {
    let text = format!("line one\nline two\n{}", "y".repeat(PREVIEW_CHARS));
    let snippet = Snippet {
        id: 0,
        name: derive_name(&text),
        source: source((0, 4), &text),
        text,
    };
    let preview = snippet.preview();
    assert!(!preview.contains('\n'), "preview is one line: {preview}");
    assert!(preview.starts_with("line one line two"), "{preview}");
    assert!(
        preview.chars().count() <= PREVIEW_CHARS,
        "preview clipped to the cap: {preview}"
    );
}

#[test]
fn is_large_flags_payloads_at_or_over_the_threshold() {
    let mut store = SnippetStore::new();
    let big = "z".repeat(LARGE_SNIPPET_BYTES);
    let small = "z".repeat(LARGE_SNIPPET_BYTES - 1);
    let big_id = store.save(&big, source((0, 100), &big)).unwrap();
    let small_id = store.save(&small, source((0, 99), &small)).unwrap();
    assert!(
        store.text_of(big_id).is_some(),
        "a large snippet is still saved (warn, not reject)"
    );
    assert!(
        store
            .snippets()
            .iter()
            .find(|s| s.id == big_id)
            .unwrap()
            .is_large(),
        "at/over the threshold is flagged large"
    );
    assert!(
        !store
            .snippets()
            .iter()
            .find(|s| s.id == small_id)
            .unwrap()
            .is_large(),
        "just under the threshold is not flagged"
    );
}

#[test]
fn source_records_provenance_and_row_count() {
    let mut store = SnippetStore::new();
    let text = "alpha\nbeta\ngamma";
    store.save(text, source((3, 5), text)).unwrap();
    let snippet = &store.snippets()[0];
    assert_eq!(snippet.source.surface, SnippetSurface::Main);
    assert_eq!(snippet.source.row_start, 3);
    assert_eq!(snippet.source.row_end, 5);
    assert_eq!(snippet.source.row_count(), 3, "inclusive 3..=5 is 3 rows");
    assert_eq!(snippet.bytes(), text.len());
}

#[test]
fn cap_drops_oldest_when_over() {
    let mut store = SnippetStore::new();
    for i in 0..(MAX_SNIPPETS + 5) {
        let body = format!("snippet-{i}");
        store.save(&body, source((i, i), &body)).unwrap();
    }
    assert_eq!(store.len(), MAX_SNIPPETS, "count is capped");
    // Newest at the front; the oldest survivors are the most recent ones.
    assert_eq!(
        store.snippets()[0].text,
        format!("snippet-{}", MAX_SNIPPETS + 4)
    );
    assert!(
        !store.snippets().iter().any(|s| s.text == "snippet-0"),
        "the very first snippet was dropped"
    );
}

#[test]
fn select_up_down_saturate_at_the_ends() {
    let mut store = SnippetStore::new();
    for i in 0..3 {
        let body = format!("s{i}");
        store.save(&body, source((i, i), &body)).unwrap();
    }
    // Cursor starts at the front after the last save.
    assert_eq!(store.selected_index(), 0);
    store.select_up();
    assert_eq!(store.selected_index(), 0, "saturates at the top");
    store.select_down();
    store.select_down();
    assert_eq!(store.selected_index(), 2);
    store.select_down();
    assert_eq!(store.selected_index(), 2, "saturates at the bottom");
}

#[test]
fn select_id_and_text_of_resolve_by_stable_id() {
    let mut store = SnippetStore::new();
    let a = store.save("aaa", source((0, 0), "aaa")).unwrap();
    let b = store.save("bbb", source((1, 1), "bbb")).unwrap();
    assert_eq!(store.text_of(a), Some("aaa"));
    assert_eq!(store.text_of(b), Some("bbb"));
    assert!(store.select_id(a), "selecting an existing id succeeds");
    assert_eq!(store.selected_snippet().map(|s| s.id), Some(a));
    assert!(!store.select_id(9999), "an unknown id does not select");
}

#[test]
fn delete_removes_by_id_and_keeps_cursor_valid() {
    let mut store = SnippetStore::new();
    let a = store.save("aaa", source((0, 0), "aaa")).unwrap();
    let b = store.save("bbb", source((1, 1), "bbb")).unwrap();
    // bbb is newest (index 0, selected); deleting it slides aaa up into slot 0.
    assert!(store.delete(b));
    assert_eq!(store.len(), 1);
    assert_eq!(store.selected_index(), 0);
    assert_eq!(store.selected_snippet().map(|s| s.id), Some(a));
    assert!(
        !store.delete(b),
        "deleting an already-removed id is a no-op"
    );
}

#[test]
fn clear_drops_everything_and_resets_cursor() {
    let mut store = SnippetStore::new();
    store.save("one", source((0, 0), "one")).unwrap();
    store.save("two", source((1, 1), "two")).unwrap();
    store.select_down();
    store.clear();
    assert!(store.is_empty());
    assert_eq!(store.selected_index(), 0);
    assert!(store.selected_snippet().is_none());
}
