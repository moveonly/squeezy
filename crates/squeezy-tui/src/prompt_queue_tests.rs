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
    let queue = queue_of(&["a", "b"]);
    assert!(indicator_line(&queue, true, false).is_some());
    assert!(indicator_line(&queue, true, true).is_some());
    assert!(indicator_line(&VecDeque::new(), true, false).is_none());
}

#[test]
fn render_lines_includes_header_and_empty_marker() {
    let state = PromptQueueState::new();
    let queue: VecDeque<String> = VecDeque::new();
    let lines = render_lines(&state, &queue);
    assert!(lines.len() >= 2);
}
