use super::*;

#[test]
fn empty_queue_has_zero_height_and_no_current() {
    let q = NotificationQueue::new();
    assert_eq!(q.height(), 0);
    assert!(q.current().is_none());
    assert!(q.is_empty());
}

#[test]
fn push_makes_pane_one_row_tall() {
    let mut q = NotificationQueue::new();
    q.push("hello", Severity::Info);
    assert_eq!(q.height(), 1);
    assert_eq!(q.current().unwrap().message, "hello");
    assert_eq!(q.len(), 1);
}

#[test]
fn rotation_cycles_through_items() {
    let mut q = NotificationQueue::new();
    q.push("a", Severity::Info);
    q.push("b", Severity::Success);
    q.push("c", Severity::Warn);
    assert_eq!(q.current().unwrap().message, "a");
    q.force_rotate_now();
    let changed = q.tick();
    assert!(changed);
    assert_eq!(q.current().unwrap().message, "b");
    q.force_rotate_now();
    q.tick();
    assert_eq!(q.current().unwrap().message, "c");
    q.force_rotate_now();
    q.tick();
    assert_eq!(q.current().unwrap().message, "a");
}

#[test]
fn expired_items_get_pruned_on_tick() {
    let mut q = NotificationQueue::new();
    q.push_with_ttl("ephemeral", Severity::Info, Duration::from_millis(1), None);
    q.push("permanent", Severity::Info);
    std::thread::sleep(Duration::from_millis(5));
    q.tick();
    assert_eq!(q.len(), 1);
    assert_eq!(q.current().unwrap().message, "permanent");
}

#[test]
fn dismiss_by_id_removes_item() {
    let mut q = NotificationQueue::new();
    let a = q.push("a", Severity::Info);
    let b = q.push("b", Severity::Info);
    q.dismiss(a);
    assert_eq!(q.len(), 1);
    assert_eq!(q.current().unwrap().id, b);
}

#[test]
fn severity_color_matches_palette() {
    assert_eq!(Severity::Info.color(), AMBER);
    assert_eq!(Severity::Success.color(), SUCCESS_GREEN);
    assert_eq!(Severity::Warn.color(), GOLD);
    assert_eq!(Severity::Error.color(), ERROR_RED);
}
