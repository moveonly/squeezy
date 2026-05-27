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
fn identical_messages_coalesce_even_when_non_adjacent() {
    let mut q = NotificationQueue::new();
    let first = q.push("✓ saved shell", Severity::Success);
    q.push("✓ saved read", Severity::Success);
    q.push("✓ saved web", Severity::Success);
    // Re-push the first message after two intervening entries — it
    // should still coalesce with the original, not create a fourth row.
    let again = q.push("✓ saved shell", Severity::Success);
    assert_eq!(q.len(), 3, "duplicate push anywhere in the queue coalesces");
    assert_eq!(first, again, "coalesced push returns the original id");
    // Same message but different severity still creates a new entry.
    q.push("✓ saved shell", Severity::Warn);
    assert_eq!(q.len(), 4);
}

#[test]
fn severity_color_matches_palette() {
    assert_eq!(Severity::Info.color(), AMBER);
    assert_eq!(Severity::Success.color(), SUCCESS_GREEN);
    assert_eq!(Severity::Warn.color(), GOLD);
    assert_eq!(Severity::Error.color(), ERROR_RED);
}

// ---- DesktopNotifier byte-sequence acceptance ----------------------------

#[test]
fn desktop_notifier_off_emits_nothing() {
    let notifier = DesktopNotifier::new(NotificationMethod::Off);
    let mut buf: Vec<u8> = Vec::new();
    let wrote = notifier
        .write_to(&mut buf, "ignored")
        .expect("write_to should not fail on a Vec");
    assert!(!wrote, "Off must not emit any bytes");
    assert!(buf.is_empty(), "buffer stays empty when notifier is Off");
    assert_eq!(notifier.resolved(), None);
}

#[test]
fn desktop_notifier_bel_emits_bell_byte() {
    let notifier = DesktopNotifier::new(NotificationMethod::Bel);
    let mut buf: Vec<u8> = Vec::new();
    let wrote = notifier.write_to(&mut buf, "turn complete").unwrap();
    assert!(wrote);
    assert_eq!(buf, b"\x07", "Bel writes a single BEL byte");
    assert_eq!(notifier.resolved(), Some(NotificationMethod::Bel));
}

#[test]
fn desktop_notifier_osc9_emits_full_escape_sequence() {
    let notifier = DesktopNotifier::new(NotificationMethod::Osc9);
    let mut buf: Vec<u8> = Vec::new();
    let wrote = notifier.write_to(&mut buf, "turn complete").unwrap();
    assert!(wrote);
    // ESC ] 9 ; <message> BEL — the iTerm-style desktop-notification
    // escape, honoured by Ghostty / Kitty / WezTerm / Warp.
    assert_eq!(buf, b"\x1b]9;turn complete\x07");
    assert_eq!(notifier.resolved(), Some(NotificationMethod::Osc9));
}

#[test]
fn desktop_notifier_osc9_strips_embedded_terminators() {
    // A hostile or accidental BEL / ESC inside the message would either
    // truncate the OSC sequence early (BEL) or break the parser into a
    // new escape (ESC). Both are sanitised out before write.
    let notifier = DesktopNotifier::new(NotificationMethod::Osc9);
    let mut buf: Vec<u8> = Vec::new();
    notifier
        .write_to(&mut buf, "danger\x07inside\x1b[31mred")
        .unwrap();
    assert_eq!(buf, b"\x1b]9;dangerinside[31mred\x07");
}

#[test]
fn desktop_notifier_osc9_collapses_newlines_to_spaces() {
    let notifier = DesktopNotifier::new(NotificationMethod::Osc9);
    let mut buf: Vec<u8> = Vec::new();
    notifier.write_to(&mut buf, "line one\nline two").unwrap();
    assert_eq!(buf, b"\x1b]9;line one line two\x07");
}

#[test]
fn desktop_notifier_auto_resolves_to_a_concrete_backend() {
    let notifier = DesktopNotifier::new(NotificationMethod::Auto);
    let resolved = notifier
        .resolved()
        .expect("Auto must resolve to a concrete backend");
    assert!(matches!(
        resolved,
        NotificationMethod::Bel | NotificationMethod::Osc9
    ));
}

#[test]
fn notification_method_parses_canonical_strings() {
    assert_eq!(
        NotificationMethod::parse("off"),
        Some(NotificationMethod::Off)
    );
    assert_eq!(
        NotificationMethod::parse("bel"),
        Some(NotificationMethod::Bel)
    );
    assert_eq!(
        NotificationMethod::parse("osc9"),
        Some(NotificationMethod::Osc9)
    );
    assert_eq!(
        NotificationMethod::parse("auto"),
        Some(NotificationMethod::Auto)
    );
    assert_eq!(
        NotificationMethod::parse("OSC-9"),
        Some(NotificationMethod::Osc9)
    );
    assert_eq!(
        NotificationMethod::parse("bell"),
        Some(NotificationMethod::Bel)
    );
    assert_eq!(NotificationMethod::parse("nonsense"), None);
}
