//! Unit tests for the Clickable Breadcrumbs model (§12.1.5). Pure, terminal-free
//! coverage of trail construction across the spec's contexts (tail-following,
//! scrolled/focused, Ctrl+T overlay, active search, empty session), the
//! keyboard-traversal index math, target mapping, label cleaning, and the
//! full-width measure that drives middle truncation.

use super::*;

/// A bare context (empty session, no focus, not following tail) — every field at
/// its resting default. Helper so each test tweaks only what it exercises.
fn ctx() -> BreadcrumbContext {
    BreadcrumbContext::default()
}

#[test]
fn empty_session_still_shows_session_root() {
    // No focus, not following tail, no overlay/search: the trail is still
    // non-empty — it always carries the orienting session root.
    let model = BreadcrumbModel::build(&ctx());
    assert_eq!(model.len(), 1, "exactly the session root crumb");
    assert!(!model.is_empty());
    let root = model.get(0).expect("root crumb");
    assert_eq!(root.label, "session");
    assert_eq!(root.target, BreadcrumbTarget::Home);
}

#[test]
fn session_label_uses_supplied_token_cleaned() {
    let model = BreadcrumbModel::build(&BreadcrumbContext {
        session_label: Some("  abc 123  ".to_string()),
        ..ctx()
    });
    // Interior whitespace collapsed, trimmed.
    assert_eq!(model.get(0).unwrap().label, "abc 123");
}

#[test]
fn following_tail_appends_a_clickable_tail_crumb() {
    let model = BreadcrumbModel::build(&BreadcrumbContext {
        following_tail: true,
        ..ctx()
    });
    assert_eq!(model.len(), 2, "session ▸ tail");
    let tail = model.get(1).expect("tail crumb");
    assert_eq!(tail.label, "tail");
    assert_eq!(tail.target, BreadcrumbTarget::Tail);
}

#[test]
fn focused_entry_wins_over_tail_and_jumps_by_id() {
    // A focused entry is the precise location: it appears as the entry crumb and
    // suppresses the `tail` crumb even if `following_tail` were set.
    let model = BreadcrumbModel::build(&BreadcrumbContext {
        following_tail: true,
        focused_entry: Some((42, "tool".to_string())),
        ..ctx()
    });
    assert_eq!(model.len(), 2, "session ▸ entry (no tail crumb)");
    let entry = model.get(1).expect("entry crumb");
    assert_eq!(entry.label, "tool");
    assert_eq!(entry.target, BreadcrumbTarget::Entry(42));
    // The tail crumb must NOT appear when an entry is focused.
    assert!(
        model
            .crumbs()
            .iter()
            .all(|c| c.target != BreadcrumbTarget::Tail),
        "focused entry suppresses the tail crumb",
    );
}

#[test]
fn focused_entry_with_blank_kind_falls_back_to_entry_label() {
    let model = BreadcrumbModel::build(&BreadcrumbContext {
        focused_entry: Some((7, "   ".to_string())),
        ..ctx()
    });
    assert_eq!(model.get(1).unwrap().label, "entry");
    assert_eq!(model.get(1).unwrap().target, BreadcrumbTarget::Entry(7));
}

#[test]
fn overlay_open_appends_overlay_crumb_that_closes_it() {
    let model = BreadcrumbModel::build(&BreadcrumbContext {
        focused_entry: Some((1, "user".to_string())),
        overlay_open: true,
        ..ctx()
    });
    // session ▸ entry ▸ overlay
    assert_eq!(model.len(), 3);
    let overlay = model.get(2).expect("overlay crumb");
    assert_eq!(overlay.label, "overlay");
    assert_eq!(overlay.target, BreadcrumbTarget::CloseOverlay);
}

#[test]
fn active_search_appends_search_crumb_with_query() {
    let model = BreadcrumbModel::build(&BreadcrumbContext {
        following_tail: true,
        search_query: Some("parser".to_string()),
        ..ctx()
    });
    // session ▸ tail ▸ search:parser
    let last = model.get(model.len() - 1).expect("search crumb");
    assert_eq!(last.label, "search:parser");
    assert_eq!(last.target, BreadcrumbTarget::Search);
}

#[test]
fn empty_search_query_still_labels_search() {
    let model = BreadcrumbModel::build(&BreadcrumbContext {
        search_query: Some(String::new()),
        ..ctx()
    });
    let last = model.get(model.len() - 1).expect("search crumb");
    assert_eq!(last.label, "search");
    assert_eq!(last.target, BreadcrumbTarget::Search);
}

#[test]
fn full_trail_orders_session_entry_overlay_search() {
    let model = BreadcrumbModel::build(&BreadcrumbContext {
        session_label: Some("sess".to_string()),
        focused_entry: Some((9, "assistant".to_string())),
        overlay_open: true,
        search_query: Some("x".to_string()),
        ..ctx()
    });
    let labels: Vec<&str> = model.crumbs().iter().map(|c| c.label.as_str()).collect();
    assert_eq!(labels, vec!["sess", "assistant", "overlay", "search:x"]);
}

#[test]
fn next_prev_index_clamp_without_wrapping() {
    let model = BreadcrumbModel::build(&BreadcrumbContext {
        focused_entry: Some((1, "user".to_string())),
        overlay_open: true,
        ..ctx()
    });
    // Three crumbs: indices 0,1,2.
    assert_eq!(model.len(), 3);
    // next clamps at the last index (no wrap).
    assert_eq!(model.next_index(0), Some(1));
    assert_eq!(model.next_index(1), Some(2));
    assert_eq!(
        model.next_index(2),
        Some(2),
        "next clamps at the deepest crumb"
    );
    // prev clamps at the root (no wrap).
    assert_eq!(model.prev_index(2), Some(1));
    assert_eq!(model.prev_index(1), Some(0));
    assert_eq!(model.prev_index(0), Some(0), "prev clamps at the root");
}

#[test]
fn target_at_maps_index_to_navigation() {
    let model = BreadcrumbModel::build(&BreadcrumbContext {
        following_tail: true,
        ..ctx()
    });
    assert_eq!(model.target_at(0), Some(BreadcrumbTarget::Home));
    assert_eq!(model.target_at(1), Some(BreadcrumbTarget::Tail));
    assert_eq!(model.target_at(2), None, "out-of-range index has no target");
}

#[test]
fn full_width_counts_labels_plus_separators() {
    let model = BreadcrumbModel::build(&BreadcrumbContext {
        session_label: Some("ab".to_string()),
        following_tail: true,
        ..ctx()
    });
    // "ab" (2) + separator (3) + "tail" (4) = 9.
    let sep = SEPARATOR.chars().count();
    assert_eq!(sep, 3, "separator is ' ▸ ' (3 chars)");
    assert_eq!(model.full_width(), 2 + sep + 4);
}

#[test]
fn full_width_measures_wide_glyphs_in_display_cells() {
    // A CJK session label occupies TWO terminal cells but is a single char.
    // full_width drives middle truncation, so it must count the cells the
    // renderer actually paints, not chars().count() (which would undercount the
    // wide glyph and let the trail overflow the row).
    let model = BreadcrumbModel::build(&BreadcrumbContext {
        session_label: Some("\u{6f22}".to_string()),
        following_tail: true,
        ..ctx()
    });
    // The root label is the single wide glyph (display width 2), not truncated.
    assert_eq!(
        model.get(0).map(|c| c.label.as_str()),
        Some("\u{6f22}"),
        "root crumb carries the wide glyph verbatim",
    );
    // "漢" (2 cells) + separator (3 cells) + "tail" (4 cells) = 9 cells.
    // chars().count() would give 1 + 3 + 4 = 8 and overflow a width-8 row.
    let sep_cells = unicode_width::UnicodeWidthStr::width(SEPARATOR);
    assert_eq!(sep_cells, 3, "separator ' ▸ ' is 3 display cells");
    assert_eq!(model.full_width(), 2 + sep_cells + 4);
}

#[test]
fn clean_label_collapses_and_caps() {
    // Whitespace collapse + trim.
    assert_eq!(clean_label("  a   b  "), "a b");
    // Cap to LABEL_CAP with an ellipsis when over-length.
    let long = "x".repeat(40);
    let cleaned = clean_label(&long);
    assert!(
        cleaned.chars().count() <= LABEL_CAP + 1,
        "capped to LABEL_CAP plus the ellipsis: {}",
        cleaned.chars().count(),
    );
    assert!(
        cleaned.ends_with('\u{2026}'),
        "over-length label is ellipsised"
    );
}

#[test]
fn clean_label_caps_wide_glyphs_by_display_cells() {
    // A run of 2-cell CJK glyphs over the budget must be capped by DISPLAY CELLS,
    // not chars: chars().count() would let LABEL_CAP wide glyphs through at 2x
    // the columns LABEL_CAP promises, overflowing the row.
    let wide = "\u{6f22}".repeat(20); // 20 chars, 40 cells.
    let cleaned = clean_label(&wide);
    assert!(
        unicode_width::UnicodeWidthStr::width(cleaned.as_str()) <= LABEL_CAP,
        "wide-glyph label capped to LABEL_CAP cells: {} cells",
        unicode_width::UnicodeWidthStr::width(cleaned.as_str()),
    );
    assert!(
        cleaned.ends_with('\u{2026}'),
        "over-width label is ellipsised"
    );
}

#[test]
fn long_session_label_is_truncated_in_the_crumb() {
    let model = BreadcrumbModel::build(&BreadcrumbContext {
        session_label: Some("a".repeat(50)),
        ..ctx()
    });
    let root = model.get(0).unwrap();
    assert!(
        root.label.chars().count() <= LABEL_CAP + 1,
        "session crumb label is bounded",
    );
}
