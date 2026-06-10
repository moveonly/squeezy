use super::*;

#[test]
fn state_starts_pinned_against_live_at_the_top() {
    let state = PinnedCompareState::new(7);
    assert_eq!(state.pinned_id, 7);
    assert_eq!(state.compare_id, None, "defaults to comparing against live");
    assert_eq!(state.focus, ComparePane::Pinned);
    assert_eq!(state.mode, CompareMode::Content);
    assert_eq!(state.pinned_scroll, 0);
    assert_eq!(state.compare_scroll, 0);
}

#[test]
fn focused_scroll_reads_and_writes_the_active_pane() {
    let mut state = PinnedCompareState::new(1);
    // Pinned is active by default.
    state.set_focused_scroll(4);
    assert_eq!(state.pinned_scroll, 4);
    assert_eq!(state.compare_scroll, 0, "the inactive pane is untouched");
    assert_eq!(state.focused_scroll(), 4);

    // Flip focus: now the compare pane is the one read/written, independently.
    state.focus = state.focus.toggled();
    assert_eq!(state.focus, ComparePane::Compare);
    assert_eq!(
        state.focused_scroll(),
        0,
        "the compare pane has its own offset"
    );
    state.set_focused_scroll(9);
    assert_eq!(state.compare_scroll, 9);
    assert_eq!(
        state.pinned_scroll, 4,
        "the pinned pane's offset is preserved"
    );
}

#[test]
fn pane_and_mode_toggles_round_trip() {
    assert_eq!(ComparePane::Pinned.toggled(), ComparePane::Compare);
    assert_eq!(ComparePane::Compare.toggled(), ComparePane::Pinned);
    assert_eq!(ComparePane::Pinned.toggled().toggled(), ComparePane::Pinned);
    assert_eq!(CompareMode::Content.toggled(), CompareMode::Diff);
    assert_eq!(CompareMode::Diff.toggled(), CompareMode::Content);
}

#[test]
fn wide_content_splits_into_two_equal_columns_that_tile() {
    let content = Rect::new(0, 0, 100, 30);
    let layout = split_overlay_content(content).expect("wide enough to split");
    let CompareLayout::Split {
        first,
        separator,
        second,
    } = layout
    else {
        panic!("a wide terminal must split side-by-side, got {layout:?}");
    };
    // Three columns tile the content with no overlap and no gap.
    assert_eq!(first.x, 0);
    assert_eq!(separator.x, first.x + first.width);
    assert_eq!(second.x, separator.x + separator.width);
    assert_eq!(
        first.width + separator.width + second.width,
        content.width,
        "columns must exactly tile the content width"
    );
    assert_eq!(separator.width, 1);
    // The two panes are equal to within the odd separator cell.
    assert!(
        second.width.abs_diff(first.width) <= 1,
        "compare panes are equal-width: {} vs {}",
        first.width,
        second.width
    );
    // Both columns span the full content height.
    assert_eq!(first.height, content.height);
    assert_eq!(second.height, content.height);
}

#[test]
fn split_honours_origin_offset() {
    let content = Rect::new(5, 4, 90, 20);
    let CompareLayout::Split { first, second, .. } = split_overlay_content(content).expect("split")
    else {
        panic!("expected a wide split");
    };
    assert_eq!(first.x, 5);
    assert_eq!(first.y, 4);
    assert_eq!(second.y, 4);
    assert_eq!(
        second.x + second.width,
        content.x + content.width,
        "the right column ends flush with the content's right edge"
    );
}

#[test]
fn narrow_but_tall_content_stacks_the_panes() {
    // One cell under the split threshold but tall: stack top/bottom instead.
    let content = Rect::new(0, 0, MIN_SPLIT_WIDTH - 1, 24);
    let layout = split_overlay_content(content).expect("tall enough to stack");
    assert!(layout.is_stacked(), "a narrow terminal stacks: {layout:?}");
    let CompareLayout::Stacked {
        first,
        separator,
        second,
    } = layout
    else {
        unreachable!();
    };
    // Rows tile the height with a one-row divider between them.
    assert_eq!(first.y, 0);
    assert_eq!(separator.y, first.y + first.height);
    assert_eq!(second.y, separator.y + separator.height);
    assert_eq!(
        first.height + separator.height + second.height,
        content.height,
        "rows must exactly tile the content height"
    );
    assert_eq!(separator.height, 1);
    // Each stacked pane spans the full (narrow) width.
    assert_eq!(first.width, content.width);
    assert_eq!(second.width, content.width);
}

#[test]
fn too_small_content_refuses_to_split() {
    // Narrow AND short: neither a split nor a stack fits.
    assert!(
        split_overlay_content(Rect::new(0, 0, MIN_SPLIT_WIDTH - 1, MIN_STACK_HEIGHT - 1)).is_none()
    );
    // Zero width / height always refuse.
    assert!(split_overlay_content(Rect::new(0, 0, 0, 30)).is_none());
    assert!(split_overlay_content(Rect::new(0, 0, 100, 0)).is_none());
}

#[test]
fn panes_and_separator_accessors_match_the_variant() {
    let split = split_overlay_content(Rect::new(0, 0, 100, 30)).expect("split");
    let (a, b) = split.panes();
    let CompareLayout::Split {
        first,
        second,
        separator,
    } = split
    else {
        unreachable!();
    };
    assert_eq!((a, b), (first, second));
    assert_eq!(split.separator(), separator);
    assert!(!split.is_stacked());
}

#[test]
fn pane_inner_insets_one_cell_on_every_side() {
    let inner = pane_inner(Rect::new(40, 2, 30, 20));
    assert_eq!(inner, Rect::new(41, 3, 28, 18));
    // A tiny pane saturates to zero area rather than underflowing.
    let tiny = pane_inner(Rect::new(0, 0, 1, 1));
    assert_eq!(tiny.width, 0);
    assert_eq!(tiny.height, 0);
}

#[test]
fn scroll_clamp_pins_into_the_valid_range() {
    assert_eq!(pane_max_scroll(50, 20), 30);
    assert_eq!(pane_max_scroll(10, 20), 0);
    assert_eq!(clamp_pane_scroll(999, 50, 20), 30);
    assert_eq!(clamp_pane_scroll(5, 50, 20), 5);
    assert_eq!(clamp_pane_scroll(7, 10, 20), 0);
}

#[test]
fn rect_contains_is_half_open() {
    let rect = Rect::new(10, 5, 4, 3);
    assert!(rect_contains(rect, 10, 5));
    assert!(rect_contains(rect, 13, 7));
    assert!(!rect_contains(rect, 14, 5));
    assert!(!rect_contains(rect, 10, 8));
    assert!(!rect_contains(rect, 9, 5));
    assert!(!rect_contains(Rect::new(0, 0, 0, 5), 0, 0));
}

fn s(lines: &[&str]) -> Vec<String> {
    lines.iter().map(|l| l.to_string()).collect()
}

#[test]
fn identical_sides_diff_to_all_same_rows() {
    let old = s(&["a", "b", "c"]);
    let new = s(&["a", "b", "c"]);
    let diff = clean_text_diff(&old, &new).expect("within the line limit");
    assert!(
        diff.iter().all(|line| line.tag == DiffTag::Same),
        "identical content has no changes: {diff:?}"
    );
    assert_eq!(diff.len(), 3);
}

#[test]
fn diff_marks_additions_and_removals_in_order() {
    // old: a b c   new: a x c d  → b removed, x added, d added; a/c shared.
    let old = s(&["a", "b", "c"]);
    let new = s(&["a", "x", "c", "d"]);
    let diff = clean_text_diff(&old, &new).expect("within the line limit");
    let tagged: Vec<(DiffTag, &str)> = diff.iter().map(|l| (l.tag, l.text.as_str())).collect();
    assert_eq!(
        tagged,
        vec![
            (DiffTag::Same, "a"),
            (DiffTag::Removed, "b"),
            (DiffTag::Added, "x"),
            (DiffTag::Same, "c"),
            (DiffTag::Added, "d"),
        ]
    );
    // The gutter markers read like a unified diff.
    assert_eq!(DiffTag::Same.marker(), ' ');
    assert_eq!(DiffTag::Added.marker(), '+');
    assert_eq!(DiffTag::Removed.marker(), '-');
}

#[test]
fn empty_old_side_is_all_additions_and_vice_versa() {
    let added = clean_text_diff(&[], &s(&["one", "two"])).expect("ok");
    assert!(added.iter().all(|l| l.tag == DiffTag::Added));
    assert_eq!(added.len(), 2);

    let removed = clean_text_diff(&s(&["one", "two"]), &[]).expect("ok");
    assert!(removed.iter().all(|l| l.tag == DiffTag::Removed));
    assert_eq!(removed.len(), 2);

    // Two empty sides diff to nothing.
    assert!(clean_text_diff(&[], &[]).expect("ok").is_empty());
}

#[test]
fn oversized_sides_refuse_to_diff() {
    let big: Vec<String> = (0..DIFF_LINE_LIMIT + 1).map(|n| n.to_string()).collect();
    let small = s(&["a"]);
    assert!(
        clean_text_diff(&big, &small).is_none(),
        "an oversized old side refuses the diff (size-limit mitigation)"
    );
    assert!(
        clean_text_diff(&small, &big).is_none(),
        "an oversized new side refuses the diff"
    );
    // Exactly at the limit on both sides still diffs.
    let at_limit: Vec<String> = (0..DIFF_LINE_LIMIT).map(|n| n.to_string()).collect();
    assert!(clean_text_diff(&at_limit, &at_limit).is_some());
}
