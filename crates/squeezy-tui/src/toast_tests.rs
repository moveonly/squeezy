use super::*;

#[test]
fn empty_queue_is_empty() {
    let q = ToastQueue::new();
    assert!(q.is_empty());
    assert_eq!(q.len(), 0);
    assert!(q.visible().is_empty());
}

#[test]
fn push_adds_one_visible_entry() {
    let mut q = ToastQueue::new();
    let id = q.push("indexed", ToastVariant::Success);
    assert_eq!(q.len(), 1);
    let visible = q.visible();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].id, id);
    assert_eq!(visible[0].message, "indexed");
    assert_eq!(visible[0].variant, ToastVariant::Success);
}

#[test]
fn visible_orders_newest_first() {
    let mut q = ToastQueue::new();
    q.push("first", ToastVariant::Info);
    q.push("second", ToastVariant::Info);
    q.push("third", ToastVariant::Info);
    let visible = q.visible();
    assert_eq!(visible[0].message, "third");
    assert_eq!(visible[1].message, "second");
    assert_eq!(visible[2].message, "first");
}

#[test]
fn push_evicts_oldest_at_capacity() {
    let mut q = ToastQueue::new();
    q.push("a", ToastVariant::Info);
    q.push("b", ToastVariant::Info);
    q.push("c", ToastVariant::Info);
    q.push("d", ToastVariant::Info);
    assert_eq!(q.len(), MAX_VISIBLE_TOASTS);
    let visible = q.visible();
    assert_eq!(visible[0].message, "d");
    assert_eq!(visible[1].message, "c");
    assert_eq!(visible[2].message, "b");
}

#[test]
fn dismiss_removes_by_id() {
    let mut q = ToastQueue::new();
    let a = q.push("a", ToastVariant::Info);
    let b = q.push("b", ToastVariant::Info);
    assert!(q.dismiss(a));
    assert_eq!(q.len(), 1);
    assert_eq!(q.visible()[0].id, b);
    assert!(!q.dismiss(a), "second dismiss of the same id is a no-op");
}

#[test]
fn clear_drops_everything() {
    let mut q = ToastQueue::new();
    q.push("x", ToastVariant::Info);
    q.push("y", ToastVariant::Info);
    let removed = q.clear();
    assert_eq!(removed, 2);
    assert!(q.is_empty());
}

#[test]
fn variant_color_matches_palette() {
    assert_eq!(ToastVariant::Info.color(), crate::render::theme::accent());
    assert_eq!(ToastVariant::Success.color(), crate::render::theme::green());
    assert_eq!(
        ToastVariant::Warning.color(),
        crate::render::theme::secondary()
    );
    assert_eq!(ToastVariant::Error.color(), crate::render::theme::red());
}

#[test]
fn toast_dismisses_after_five_seconds() {
    // Acceptance test from `audits/.../06-ui.md#f06-toast-notification-queue`.
    // Sleeping for the full default would slow the suite, so we push with a
    // tighter ttl and assert the same dismissal semantics; the second push
    // exercises the configured 5-second DEFAULT_TOAST_TTL deadline.
    let mut q = ToastQueue::new();
    q.push_with_ttl("flushed", ToastVariant::Success, Duration::from_millis(20));
    assert_eq!(q.len(), 1);
    std::thread::sleep(Duration::from_millis(30));
    let changed = q.tick();
    assert!(changed, "tick reports a change when an entry expires");
    assert!(q.is_empty(), "expired entries are removed after tick");

    let id = q.push("default-ttl", ToastVariant::Info);
    let toast = q.visible().into_iter().find(|t| t.id == id).unwrap();
    let remaining = toast.dismissed_at.saturating_duration_since(Instant::now());
    assert!(
        remaining <= DEFAULT_TOAST_TTL
            && remaining + Duration::from_millis(50) >= DEFAULT_TOAST_TTL,
        "default ttl is {DEFAULT_TOAST_TTL:?}, observed remaining {remaining:?}"
    );
}

#[test]
fn tick_returns_false_when_nothing_expires() {
    let mut q = ToastQueue::new();
    q.push("stable", ToastVariant::Info);
    assert!(!q.tick(), "fresh push should not be expired");
    assert_eq!(q.len(), 1);
}
