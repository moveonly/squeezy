use std::collections::VecDeque;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::*;

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn press_with(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, mods)
}

fn queue_of(items: &[&str]) -> VecDeque<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn empty_queue_handles_nav_without_panic() {
    let mut state = PromptQueueState::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    assert_eq!(
        state.dispatch(&mut queue, press(KeyCode::Up)),
        QueueDispatch::Handled
    );
    assert_eq!(
        state.dispatch(&mut queue, press(KeyCode::Down)),
        QueueDispatch::Handled,
    );
    assert_eq!(
        state.dispatch(&mut queue, press_with(KeyCode::Up, KeyModifiers::SHIFT)),
        QueueDispatch::Handled,
    );
    assert_eq!(state.selected, 0);
    assert!(queue.is_empty());
}

#[test]
fn arrow_keys_move_selection() {
    let mut state = PromptQueueState::new();
    let mut queue = queue_of(&["a", "b", "c"]);
    state.dispatch(&mut queue, press(KeyCode::Down));
    assert_eq!(state.selected, 1);
    state.dispatch(&mut queue, press(KeyCode::Down));
    assert_eq!(state.selected, 2);
    // Already at the bottom; further Down is a no-op.
    state.dispatch(&mut queue, press(KeyCode::Down));
    assert_eq!(state.selected, 2);
    state.dispatch(&mut queue, press(KeyCode::Up));
    assert_eq!(state.selected, 1);
}

#[test]
fn shift_down_swaps_with_neighbor_and_follows_selection() {
    let mut state = PromptQueueState::new();
    let mut queue = queue_of(&["a", "b", "c"]);
    state.dispatch(&mut queue, press_with(KeyCode::Down, KeyModifiers::SHIFT));
    assert_eq!(queue, queue_of(&["b", "a", "c"]));
    assert_eq!(state.selected, 1);
    state.dispatch(&mut queue, press_with(KeyCode::Down, KeyModifiers::SHIFT));
    assert_eq!(queue, queue_of(&["b", "c", "a"]));
    assert_eq!(state.selected, 2);
    // At the bottom — Shift+Down is a no-op.
    state.dispatch(&mut queue, press_with(KeyCode::Down, KeyModifiers::SHIFT));
    assert_eq!(queue, queue_of(&["b", "c", "a"]));
    assert_eq!(state.selected, 2);
}

#[test]
fn shift_up_swaps_upward() {
    let mut state = PromptQueueState { selected: 2 };
    let mut queue = queue_of(&["a", "b", "c"]);
    state.dispatch(&mut queue, press_with(KeyCode::Up, KeyModifiers::SHIFT));
    assert_eq!(queue, queue_of(&["a", "c", "b"]));
    assert_eq!(state.selected, 1);
}

#[test]
fn delete_removes_selected_and_clamps() {
    let mut state = PromptQueueState { selected: 2 };
    let mut queue = queue_of(&["a", "b", "c"]);
    state.dispatch(&mut queue, press(KeyCode::Delete));
    assert_eq!(queue, queue_of(&["a", "b"]));
    assert_eq!(state.selected, 1);
    state.dispatch(&mut queue, press(KeyCode::Delete));
    assert_eq!(queue, queue_of(&["a"]));
    assert_eq!(state.selected, 0);
    state.dispatch(&mut queue, press(KeyCode::Delete));
    assert!(queue.is_empty());
    assert_eq!(state.selected, 0);
}

#[test]
fn enter_and_esc_request_close() {
    let mut state = PromptQueueState::new();
    let mut queue = queue_of(&["a"]);
    assert_eq!(
        state.dispatch(&mut queue, press(KeyCode::Esc)),
        QueueDispatch::Close,
    );
    assert_eq!(
        state.dispatch(&mut queue, press(KeyCode::Enter)),
        QueueDispatch::Close,
    );
}

#[test]
fn unrelated_keys_are_ignored() {
    let mut state = PromptQueueState::new();
    let mut queue = queue_of(&["a"]);
    assert_eq!(
        state.dispatch(&mut queue, press(KeyCode::Char('x'))),
        QueueDispatch::Ignored,
    );
}

#[test]
fn indicator_line_present_when_queue_non_empty() {
    let tokens = crate::glyph_mode::GlyphMode::Unicode.tokens();
    let queue = queue_of(&["a", "b"]);
    assert!(indicator_line(&queue, true, false, None, tokens).is_some());
    assert!(indicator_line(&queue, true, true, None, tokens).is_some());
    assert!(indicator_line(&VecDeque::new(), true, false, None, tokens).is_none());
    // A group summary rides along in the strip when present.
    let line =
        indicator_line(&queue, false, false, Some("Group 1 (2, paused)"), tokens).expect("line");
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(
        text.contains("Group 1 (2, paused)"),
        "summary in strip: {text}"
    );
}

#[test]
fn render_lines_includes_header_and_empty_marker() {
    let state = PromptQueueState::new();
    let queue: VecDeque<String> = VecDeque::new();
    let lines = render_lines(&state, &queue, None, None, None, None, None);
    assert!(lines.len() >= 2);
}

#[test]
fn render_lines_paints_multiselect_checkbox() {
    let state = PromptQueueState::new();
    let queue = queue_of(&["alpha", "beta", "gamma"]);
    // Tag the middle item only.
    let tagged = [false, true, false];
    let lines = render_lines(&state, &queue, Some(&tagged), None, None, None, None);
    let text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    // The tagged row shows the filled checkbox; an untagged row the empty one.
    assert!(text.contains("[x]"), "tagged row must show [x]: {text}");
    assert!(text.contains("[ ]"), "untagged rows must show [ ]: {text}");
    // The header switches to the multi-select cheatsheet once a group is active.
    assert!(
        text.contains("delete group"),
        "active-group header hint missing: {text}"
    );
}

#[test]
fn render_lines_paints_right_aligned_delete_glyph_at_the_hit_zone_column() {
    // With a known content width, each item row paints a visible `[x]` delete
    // affordance in its trailing DELETE_AFFORDANCE_WIDTH cells — flush right at
    // `width - DELETE_AFFORDANCE_WIDTH`, the exact column the `QueueDelete` hit
    // rect (registered at `x = handle_w = width - delete_w`) occupies. Before the
    // fix rows were left-aligned `NN. preview` with empty trailing cells.
    let state = PromptQueueState::new();
    let queue = queue_of(&["alpha", "beta"]);
    let width: u16 = 40;
    let lines = render_lines(&state, &queue, None, None, None, None, Some(width));
    // Skip the header (line 0); inspect the first item row.
    let row: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
    let row_w = unicode_width::UnicodeWidthStr::width(row.as_str());
    assert_eq!(
        row_w, width as usize,
        "the row fills the full content width so the glyph lands flush right: {row:?}"
    );
    assert!(
        row.ends_with("[x]"),
        "the row's trailing cells paint the delete glyph: {row:?}"
    );
    // The glyph occupies exactly the trailing DELETE_AFFORDANCE_WIDTH cells, i.e.
    // it begins at the same x the QueueDelete hit rect does.
    let glyph_start = row_w - DELETE_AFFORDANCE_WIDTH as usize;
    assert_eq!(
        &row[row.char_indices().nth(glyph_start).map(|(i, _)| i).unwrap()..],
        "[x]",
        "delete glyph starts at width - DELETE_AFFORDANCE_WIDTH"
    );
}

#[test]
fn render_lines_truncates_long_preview_so_it_never_overwrites_the_delete_glyph() {
    // A preview longer than the row must be truncated to leave room for the
    // trailing `[x]`, so the glyph stays at the hit-zone column.
    let state = PromptQueueState::new();
    let long = "x".repeat(200);
    let queue = queue_of(&[long.as_str()]);
    let width: u16 = 30;
    let lines = render_lines(&state, &queue, None, None, None, None, Some(width));
    let row: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
    let row_w = unicode_width::UnicodeWidthStr::width(row.as_str());
    assert_eq!(row_w, width as usize, "row clamped to width: {row:?}");
    assert!(row.ends_with("[x]"), "glyph survives truncation: {row:?}");
}

#[test]
fn render_lines_header_is_base_hint_with_no_group() {
    let state = PromptQueueState::new();
    let queue = queue_of(&["alpha"]);
    let lines = render_lines(&state, &queue, Some(&[false]), None, None, None, None);
    let header: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(header.contains("reorder"), "base header hint: {header}");
    assert!(!header.contains("delete group"));
}

#[test]
fn render_lines_collapsed_group_shows_its_header_once() {
    use std::collections::BTreeSet;

    use crate::queue_groups::QueueGroup;

    let state = PromptQueueState::new();
    let queue = queue_of(&["alpha", "beta", "gamma"]);
    let group = QueueGroup {
        id: 7,
        name: "Group 1".to_string(),
        members: BTreeSet::from([0, 1, 2]),
        collapsed: true,
        paused: false,
    };
    let by_row = [Some(&group), Some(&group), Some(&group)];
    let lines = render_lines(&state, &queue, None, Some(&by_row), None, None, None);
    let header_rows = lines
        .iter()
        .filter(|l| l.spans.iter().any(|s| s.content.contains('⊟')))
        .count();
    assert_eq!(
        header_rows, 1,
        "a collapsed group paints its ⊟ header on exactly one row, not every member"
    );
}

// ---- visible_window: the single-source-of-truth overlay windowing ----------
//
// `render_lines` (painting) and `register_queue_item_targets` (hit rects) both
// derive their slice from `visible_window`, so its `(start, count)` math is
// load-bearing for click-to-row alignment. These pin every branch directly.

#[test]
fn visible_window_empty_queue_is_zero_zero() {
    assert_eq!(visible_window(0, 0), (0, 0));
    // A stale selected index against an empty queue must not panic or shift.
    assert_eq!(visible_window(3, 0), (0, 0));
}

#[test]
fn visible_window_total_at_or_below_window_shows_all_from_zero() {
    // total <= WINDOW: the whole queue fits, always starting at 0 regardless of
    // the cursor.
    for total in 0..=WINDOW {
        for selected in 0..=total.saturating_add(1) {
            assert_eq!(
                visible_window(selected, total),
                (0, total),
                "total {total} (<= WINDOW {WINDOW}) must show all from 0, selected {selected}"
            );
        }
    }
}

#[test]
fn visible_window_centers_a_mid_cursor() {
    // total 10, WINDOW 5, half 2: a mid cursor sits centered with `half` rows
    // above it. selected 5 -> start 3, so the cursor is the 3rd of 5 rows.
    let total = 10;
    let (start, count) = visible_window(5, total);
    assert_eq!((start, count), (3, WINDOW));
    assert!(
        start <= 5 && 5 < start + count,
        "the selected row must fall inside the window"
    );
    assert_eq!(5 - start, WINDOW / 2, "cursor is centered (half above it)");
}

#[test]
fn visible_window_clamps_start_at_the_end() {
    // The last index pins the window flush to the end rather than scrolling past
    // it: start = total - count.
    let total = 10;
    let (start, count) = visible_window(total - 1, total);
    assert_eq!((start, count), (total - WINDOW, WINDOW));
    assert_eq!(
        start + count,
        total,
        "window ends exactly at the queue tail"
    );
}

#[test]
fn visible_window_clamps_gracefully_when_selected_past_total() {
    // A selected index beyond the queue (a transient stale cursor) must clamp to
    // the final window, not index out of range.
    let total = 10;
    assert_eq!(visible_window(total, total), (total - WINDOW, WINDOW));
    assert_eq!(visible_window(total + 50, total), (total - WINDOW, WINDOW));
}

#[test]
fn visible_window_start_keeps_window_in_bounds_for_every_cursor() {
    // Exhaustive invariant: for any total and any (even out-of-range) cursor,
    // the returned slice stays within `[0, total]` and never exceeds WINDOW.
    for total in 0..20 {
        for selected in 0..(total + 5) {
            let (start, count) = visible_window(selected, total);
            assert!(count <= WINDOW, "count {count} exceeds WINDOW {WINDOW}");
            assert!(
                count == WINDOW.min(total),
                "count must be WINDOW.min(total)"
            );
            assert!(
                start + count <= total,
                "window [{start}, {}) escapes total {total}",
                start + count
            );
        }
    }
}
