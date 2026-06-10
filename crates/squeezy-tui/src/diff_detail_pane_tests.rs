use super::*;

#[test]
fn split_carves_pane_off_the_right_with_a_separator() {
    let content = Rect::new(0, 0, 100, 30);
    let layout = split_overlay_content(content).expect("wide enough to split");

    // The three columns tile the content with no overlap and no gap.
    assert_eq!(layout.transcript.x, 0);
    assert_eq!(
        layout.separator.x,
        layout.transcript.x + layout.transcript.width
    );
    assert_eq!(layout.pane.x, layout.separator.x + layout.separator.width);
    assert_eq!(
        layout.transcript.width + layout.separator.width + layout.pane.width,
        content.width,
        "columns must exactly tile the content width"
    );
    assert_eq!(layout.separator.width, 1);
    // The pane takes 2/5 of the width; the transcript keeps the majority.
    assert_eq!(layout.pane.width, 100 * 2 / 5);
    assert!(layout.transcript.width > layout.pane.width);
    // Every column spans the full content height.
    assert_eq!(layout.transcript.height, content.height);
    assert_eq!(layout.pane.height, content.height);
}

#[test]
fn split_honours_origin_offset() {
    let content = Rect::new(7, 3, 90, 20);
    let layout = split_overlay_content(content).expect("split");
    assert_eq!(layout.transcript.x, 7);
    assert_eq!(layout.transcript.y, 3);
    assert_eq!(layout.pane.y, 3);
    assert_eq!(
        layout.pane.x + layout.pane.width,
        content.x + content.width,
        "the pane must end flush with the content's right edge"
    );
}

#[test]
fn split_refuses_when_too_narrow() {
    // One cell under the threshold: no split, transcript keeps the full width.
    let content = Rect::new(0, 0, MIN_SPLIT_WIDTH - 1, 30);
    assert!(split_overlay_content(content).is_none());
    // Zero height also refuses.
    let content = Rect::new(0, 0, 120, 0);
    assert!(split_overlay_content(content).is_none());
}

#[test]
fn split_at_exactly_the_threshold_is_allowed() {
    let content = Rect::new(0, 0, MIN_SPLIT_WIDTH, 10);
    let layout = split_overlay_content(content).expect("threshold width splits");
    assert!(layout.transcript.width >= 1);
    assert!(layout.pane.width >= 1);
}

#[test]
fn pane_inner_insets_one_cell_on_every_side() {
    let pane = Rect::new(40, 2, 30, 20);
    let inner = pane_inner(pane);
    assert_eq!(inner.x, 41);
    assert_eq!(inner.y, 3);
    assert_eq!(inner.width, 28);
    assert_eq!(inner.height, 18);
}

#[test]
fn pane_inner_saturates_for_a_tiny_pane() {
    let inner = pane_inner(Rect::new(0, 0, 1, 1));
    assert_eq!(inner.width, 0);
    assert_eq!(inner.height, 0);
}

#[test]
fn max_scroll_is_overflow_or_zero() {
    // Body taller than the viewport: scroll up to the overflow.
    assert_eq!(pane_max_scroll(50, 20), 30);
    // Body shorter than (or equal to) the viewport: no scroll.
    assert_eq!(pane_max_scroll(10, 20), 0);
    assert_eq!(pane_max_scroll(20, 20), 0);
}

#[test]
fn clamp_pins_into_the_valid_range() {
    // Over the max clamps down to the overflow.
    assert_eq!(clamp_pane_scroll(999, 50, 20), 30);
    // In range passes through.
    assert_eq!(clamp_pane_scroll(5, 50, 20), 5);
    // A fitting body clamps every request to the top.
    assert_eq!(clamp_pane_scroll(7, 10, 20), 0);
}

#[test]
fn rect_contains_is_half_open() {
    let rect = Rect::new(10, 5, 4, 3);
    // Inside.
    assert!(rect_contains(rect, 10, 5));
    assert!(rect_contains(rect, 13, 7));
    // Past the right / bottom edge (half-open) is outside.
    assert!(!rect_contains(rect, 14, 5));
    assert!(!rect_contains(rect, 10, 8));
    // Before the origin is outside.
    assert!(!rect_contains(rect, 9, 5));
    assert!(!rect_contains(rect, 10, 4));
    // A zero-area rect contains nothing.
    assert!(!rect_contains(Rect::new(0, 0, 0, 5), 0, 0));
}

#[test]
fn state_starts_pinned_at_the_top() {
    let state = DiffDetailPaneState::new(42);
    assert_eq!(state.entry_id, 42);
    assert_eq!(state.scroll, 0);
}
