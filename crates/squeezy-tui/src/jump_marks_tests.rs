use super::*;

#[test]
fn new_stack_is_empty() {
    let stack = JumpMarkStack::new();
    assert_eq!(stack.mark_count(), 0);
    assert!(stack.history().is_empty());
    assert_eq!(stack.history_summary(4, |_| None), "");
}

#[test]
fn set_pushes_and_jump_back_pops_lifo() {
    let mut stack = JumpMarkStack::new();
    assert_eq!(stack.set(10), 1);
    assert_eq!(stack.set(20), 2);
    assert_eq!(stack.set(30), 3);
    assert_eq!(stack.mark_count(), 3);

    // LIFO: most recent mark comes back first.
    assert_eq!(stack.jump_back(None), Some(30));
    assert_eq!(stack.jump_back(None), Some(20));
    assert_eq!(stack.jump_back(None), Some(10));
    assert_eq!(stack.jump_back(None), None);
    assert_eq!(stack.mark_count(), 0);
}

#[test]
fn consecutive_duplicate_set_is_collapsed() {
    let mut stack = JumpMarkStack::new();
    stack.set(7);
    stack.set(7);
    stack.set(7);
    assert_eq!(
        stack.mark_count(),
        1,
        "repeated mark on same row must dedup"
    );

    // A different id then the same id again is two distinct marks.
    stack.set(8);
    stack.set(7);
    assert_eq!(stack.mark_count(), 3);
}

#[test]
fn jump_back_skips_a_mark_pointing_at_current_row() {
    let mut stack = JumpMarkStack::new();
    stack.set(1);
    stack.set(2);
    // We are currently sitting on entry 2; jumping back should skip the mark
    // that points right at us and land on entry 1.
    assert_eq!(stack.jump_back(Some(2)), Some(1));
    assert_eq!(stack.mark_count(), 0);
}

#[test]
fn jump_back_with_all_marks_on_current_row_returns_none() {
    let mut stack = JumpMarkStack::new();
    stack.set(5);
    // The only mark is where we already are: nothing to jump to, but the stack
    // is drained.
    assert_eq!(stack.jump_back(Some(5)), None);
    assert_eq!(stack.mark_count(), 0);
}

#[test]
fn mark_stack_is_capped_oldest_falls_off() {
    let mut stack = JumpMarkStack::new();
    for id in 0..(MARK_STACK_CAP as u64 + 5) {
        stack.set(id);
    }
    assert_eq!(stack.mark_count(), MARK_STACK_CAP);
    // The newest mark is on top; the oldest five were dropped from the front.
    assert_eq!(stack.jump_back(None), Some(MARK_STACK_CAP as u64 + 4));
}

#[test]
fn history_records_jump_destinations_newest_first() {
    let mut stack = JumpMarkStack::new();
    stack.set(100);
    stack.set(200);
    stack.set(300);
    stack.jump_back(None); // -> 300
    stack.jump_back(None); // -> 200
    let hist: Vec<u64> = stack.history().iter().copied().collect();
    assert_eq!(hist, vec![200, 300]);
}

#[test]
fn history_collapses_immediate_repeat() {
    let mut stack = JumpMarkStack::new();
    stack.set(1);
    stack.set(1); // collapsed at set time, so stack has one mark
    // Re-mark and jump twice to the same destination.
    stack.set(2);
    stack.jump_back(None); // -> 2
    stack.set(2);
    stack.jump_back(None); // -> 2 again, must not double in history
    let hist: Vec<u64> = stack.history().iter().copied().collect();
    assert_eq!(hist, vec![2]);
}

#[test]
fn history_is_capped() {
    let mut stack = JumpMarkStack::new();
    // Push and jump distinct ids more than the cap.
    for id in 0..(HISTORY_CAP as u64 + 4) {
        stack.set(id);
        stack.jump_back(None);
    }
    assert!(stack.history().len() <= HISTORY_CAP);
    // Newest destination is at the front.
    assert_eq!(
        stack.history().front().copied(),
        Some(HISTORY_CAP as u64 + 3)
    );
}

#[test]
fn history_summary_uses_labels_and_falls_back_to_id() {
    let mut stack = JumpMarkStack::new();
    stack.set(1);
    stack.set(2);
    stack.jump_back(None); // -> 2
    stack.jump_back(None); // -> 1
    // Label entry 1, leave entry 2 unlabeled (falls back to "#2").
    let summary = stack.history_summary(4, |id| (id == 1).then(|| "first".to_string()));
    assert_eq!(summary, "first \u{2190} #2");
}

#[test]
fn history_summary_respects_max() {
    let mut stack = JumpMarkStack::new();
    for id in 1..=5u64 {
        stack.set(id);
        stack.jump_back(None);
    }
    let summary = stack.history_summary(2, |_| None);
    // Two newest only: 5 then 4, with a trailing ellipsis marking that older
    // jumps (3, 2, 1) exist beyond the clipped readout.
    assert_eq!(summary, "#5 \u{2190} #4 \u{2190} \u{2026}");
}

#[test]
fn history_summary_no_ellipsis_when_not_clipped() {
    let mut stack = JumpMarkStack::new();
    for id in 1..=3u64 {
        stack.set(id);
        stack.jump_back(None);
    }
    // Exactly `max` entries: an exhaustive readout carries no trailing ellipsis.
    let summary = stack.history_summary(3, |_| None);
    assert_eq!(summary, "#3 \u{2190} #2 \u{2190} #1");
}

#[test]
fn history_summary_zero_max_is_empty() {
    let mut stack = JumpMarkStack::new();
    stack.set(1);
    stack.jump_back(None);
    assert_eq!(stack.history_summary(0, |_| None), "");
}
