use super::*;

#[test]
fn parse_splits_literals_and_slots_in_order() {
    let segs = parse("Review {file} for {issue}.");
    assert_eq!(
        segs,
        vec![
            Segment::Literal("Review ".to_string()),
            Segment::Slot("file".to_string()),
            Segment::Literal(" for ".to_string()),
            Segment::Slot("issue".to_string()),
            Segment::Literal(".".to_string()),
        ]
    );
}

#[test]
fn parse_treats_lone_or_empty_brace_as_literal() {
    // A lone `{` with no closing brace stays literal — no body text is lost.
    assert_eq!(parse("a { b"), vec![Segment::Literal("a { b".to_string())]);
    // An empty `{}` is not a slot.
    assert_eq!(parse("x{}y"), vec![Segment::Literal("x{}y".to_string())]);
    // A space inside the braces disqualifies it as a slot.
    assert_eq!(
        parse("{not a slot}"),
        vec![Segment::Literal("{not a slot}".to_string())]
    );
}

#[test]
fn parse_decodes_double_brace_escapes() {
    let segs = parse("use {{braces}} around {name}");
    assert_eq!(
        segs,
        vec![
            Segment::Literal("use {braces} around ".to_string()),
            Segment::Slot("name".to_string()),
        ]
    );
}

#[test]
fn parse_rejects_overlong_slot_name_as_literal() {
    let long = "x".repeat(SLOT_NAME_CHARS + 1);
    let body = format!("{{{long}}}");
    // An over-long `{name}` is kept literal verbatim, not turned into a slot.
    assert_eq!(parse(&body), vec![Segment::Literal(body.clone())]);
}

#[test]
fn slot_names_dedupes_in_first_appearance_order() {
    let segs = parse("{a} {b} {a} {c} {b}");
    assert_eq!(slot_names(&segs), vec!["a", "b", "c"]);
}

#[test]
fn slot_names_caps_at_max_slots() {
    let mut body = String::new();
    for i in 0..(MAX_SLOTS + 5) {
        body.push_str(&format!("{{s{i}}} "));
    }
    let names = slot_names(&parse(&body));
    assert_eq!(names.len(), MAX_SLOTS, "slot list is capped");
}

#[test]
fn resolve_substitutes_filled_slots_and_trims() {
    let segs = parse("Review {file}.");
    let out = resolve(&segs, |name| {
        (name == "file").then(|| "  src/lib.rs  ".to_string())
    });
    assert_eq!(out, Ok("Review src/lib.rs.".to_string()));
}

#[test]
fn resolve_blocks_on_missing_or_blank_slots_in_body_order() {
    let segs = parse("{a} {b} {c}");
    // `b` blank, `c` absent — both reported, in body order, `a` filled.
    let out = resolve(&segs, |name| match name {
        "a" => Some("one".to_string()),
        "b" => Some("   ".to_string()),
        _ => None,
    });
    assert_eq!(
        out,
        Err(ResolveError::MissingSlots(vec![
            "b".to_string(),
            "c".to_string()
        ]))
    );
}

#[test]
fn resolve_shares_value_across_duplicate_slot_names() {
    let segs = parse("{x} and {x} again");
    let out = resolve(&segs, |name| (name == "x").then(|| "Z".to_string()));
    assert_eq!(out, Ok("Z and Z again".to_string()));
}

#[test]
fn resolve_slotless_body_is_runnable_immediately() {
    let segs = parse("just a plain prompt");
    let out = resolve(&segs, |_| None);
    assert_eq!(out, Ok("just a plain prompt".to_string()));
}

#[test]
fn derive_name_uses_first_non_empty_line_and_clips() {
    assert_eq!(derive_name("\n\n  hello {x}  \nsecond"), "hello {x}");
    assert_eq!(derive_name("   "), "(empty template)");
    let long = "y".repeat(NAME_CHARS + 20);
    let name = derive_name(&long);
    assert_eq!(name.chars().count(), NAME_CHARS);
    assert!(name.ends_with('…'));
}

#[test]
fn card_focus_ring_wraps_both_directions() {
    let mut card = TemplateCard::new(0, "t".to_string(), "{a} {b} {c}");
    assert_eq!(card.slot_count(), 3);
    assert_eq!(card.focused_index(), 0);
    card.focus_next();
    assert_eq!(card.focused_index(), 1);
    card.focus_prev();
    card.focus_prev();
    assert_eq!(card.focused_index(), 2, "prev from 0 wraps to last");
    card.focus_next();
    assert_eq!(card.focused_index(), 0, "next from last wraps to first");
}

#[test]
fn card_focus_index_ignores_out_of_range() {
    let mut card = TemplateCard::new(0, "t".to_string(), "{a} {b}");
    assert!(card.focus_index(1));
    assert_eq!(card.focused_index(), 1);
    assert!(!card.focus_index(9), "out-of-range focus is ignored");
    assert_eq!(card.focused_index(), 1, "focus unchanged after bad index");
}

#[test]
fn card_edit_only_touches_focused_slot() {
    let mut card = TemplateCard::new(0, "t".to_string(), "{a} {b}");
    card.insert_char('h');
    card.insert_char('i');
    assert_eq!(card.value_at(0), "hi");
    assert_eq!(card.value_at(1), "");
    card.focus_next();
    card.insert_char('y');
    assert_eq!(card.value_at(0), "hi");
    assert_eq!(card.value_at(1), "y");
    card.delete_back();
    assert_eq!(card.value_at(1), "");
    card.focus_prev();
    card.clear_focused();
    assert_eq!(card.value_at(0), "");
}

#[test]
fn card_resolved_blocks_until_all_slots_filled() {
    let mut card = TemplateCard::new(0, "t".to_string(), "Review {file} for {bug}");
    assert_eq!(
        card.missing_slots(),
        vec!["file".to_string(), "bug".to_string()]
    );
    card.insert_char('x');
    assert_eq!(card.missing_slots(), vec!["bug".to_string()]);
    card.focus_next();
    for ch in "y".chars() {
        card.insert_char(ch);
    }
    assert!(card.missing_slots().is_empty());
    assert_eq!(card.resolved(), Ok("Review x for y".to_string()));
}

#[test]
fn card_with_no_slots_is_runnable_and_focus_moves_noop() {
    let mut card = TemplateCard::new(0, "t".to_string(), "no slots here");
    assert!(card.has_no_slots());
    assert!(card.missing_slots().is_empty());
    assert_eq!(card.focused_slot(), None);
    // Focus moves are no-ops on a slot-less card (no panic, index stays 0).
    card.focus_next();
    card.focus_prev();
    assert_eq!(card.focused_index(), 0);
    card.insert_char('z'); // also a no-op
    assert_eq!(card.resolved(), Ok("no slots here".to_string()));
}

#[test]
fn store_save_newest_first_distinct_ids_and_cursor_follows() {
    let mut store = TemplateStore::new();
    let a = store.save(None, "first {x}").unwrap();
    let b = store.save(Some("Named"), "second {y}").unwrap();
    assert_ne!(a, b);
    assert_eq!(store.len(), 2);
    assert_eq!(store.templates()[0].name, "Named");
    assert_eq!(store.templates()[1].name, "first {x}");
    assert_eq!(store.selected_index(), 0);
    assert_eq!(store.selected_template().map(|t| t.id), Some(b));
}

#[test]
fn store_rejects_blank_body() {
    let mut store = TemplateStore::new();
    assert!(store.save(None, "  \n\t ").is_none());
    assert!(store.is_empty());
}

#[test]
fn store_enforces_cap_dropping_oldest() {
    let mut store = TemplateStore::new();
    for i in 0..(MAX_TEMPLATES + 5) {
        store.save(None, &format!("body {i} {{s}}"));
    }
    assert_eq!(store.len(), MAX_TEMPLATES);
    // Newest is on top; the very first saves were dropped.
    assert_eq!(
        store.templates()[0].body,
        format!("body {} {{s}}", MAX_TEMPLATES + 4)
    );
}

#[test]
fn store_select_navigation_and_select_by_id() {
    let mut store = TemplateStore::new();
    let first = store.save(None, "a {x}").unwrap();
    store.save(None, "b {x}");
    store.save(None, "c {x}");
    assert_eq!(store.selected_index(), 0);
    store.select_down();
    assert_eq!(store.selected_index(), 1);
    store.select_up();
    assert_eq!(store.selected_index(), 0);
    // Saturates.
    store.select_up();
    assert_eq!(store.selected_index(), 0);
    // By id resolves to the right row regardless of position.
    assert!(store.select_id(first));
    assert_eq!(store.selected_template().map(|t| t.id), Some(first));
    assert!(!store.select_id(9999));
}

#[test]
fn store_instantiate_builds_card_from_template_by_id() {
    let mut store = TemplateStore::new();
    let id = store.save(None, "Review {file}").unwrap();
    let card = store.instantiate(id).expect("card");
    assert_eq!(card.template_id, id);
    assert_eq!(card.slots(), ["file"]);
    // A deleted id yields no card.
    assert!(store.delete(id));
    assert!(store.instantiate(id).is_none());
}

#[test]
fn store_delete_keeps_cursor_in_range() {
    let mut store = TemplateStore::new();
    store.save(None, "a {x}");
    store.save(None, "b {x}");
    let top = store.selected_template().map(|t| t.id).unwrap();
    store.select_down(); // now on the older row (index 1)
    assert_eq!(store.selected_index(), 1);
    assert!(store.delete(top));
    assert_eq!(store.len(), 1);
    assert_eq!(store.selected_index(), 0, "cursor clamps after a delete");
}

#[test]
fn store_clear_empties_and_resets_cursor() {
    let mut store = TemplateStore::new();
    store.save(None, "a {x}");
    store.save(None, "b {x}");
    store.select_down();
    store.clear();
    assert!(store.is_empty());
    assert_eq!(store.selected_index(), 0);
}

#[test]
fn store_with_starters_is_nonempty_and_filling_resolves() {
    let store = TemplateStore::with_starters();
    assert!(!store.is_empty());
    // The freshest starter is "Review {file}".
    let top = store.selected_template().expect("a starter");
    assert!(top.body.contains("{file}"));
    let mut card = store.instantiate(top.id).expect("card");
    assert_eq!(card.slot_count(), 1);
    for ch in "src/lib.rs".chars() {
        card.insert_char(ch);
    }
    let resolved = card.resolved().expect("resolved");
    assert!(resolved.contains("src/lib.rs"));
    assert!(!resolved.contains("{file}"), "slot marker substituted away");
}

#[test]
fn template_preview_keeps_slot_markers_and_clips() {
    let mut store = TemplateStore::new();
    let id = store.save(None, "Review {file} for issues").unwrap();
    let t = store.templates().iter().find(|t| t.id == id).unwrap();
    assert!(t.preview().contains("{file}"));
    // A long body clips with an ellipsis.
    let long = format!("{} {{x}}", "z".repeat(PREVIEW_CHARS + 20));
    let id2 = store.save(None, &long).unwrap();
    let t2 = store.templates().iter().find(|t| t.id == id2).unwrap();
    assert_eq!(t2.preview().chars().count(), PREVIEW_CHARS);
    assert!(t2.preview().ends_with('…'));
}
