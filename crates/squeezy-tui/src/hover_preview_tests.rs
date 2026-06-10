//! Unit tests for the pure Hover Preview And Double-Click Activation model
//! (§12.1.4). These exercise the per-target pointer policy, the destructive-verb
//! safety property, the bounded preview-content builder, and the clamped popover
//! geometry directly, with no terminal — the keyboard/mouse/render integration is
//! covered by the capture-sink suite in `lib_tests.rs`.

use ratatui::layout::Rect;

use super::*;
use crate::interaction::{Action, ChromeKey, TargetKey};
use crate::transcript_surface::{EntryId, RowId};

// ---- per-target pointer policy ----

/// A transcript entry hovers, selects, and activates into the (non-destructive)
/// open-in-detail verb — the same verb the keyboard `Ctrl+Enter` chord reaches.
#[test]
fn entry_policy_hovers_selects_and_activates_into_detail() {
    let policy = policy_for(TargetKey::Entry(EntryId(7)));
    assert!(policy.hover_preview, "an entry reveals a preview on hover");
    assert!(policy.select, "a single click selects/focuses an entry");
    assert_eq!(
        policy.primary_activate,
        Some(Action::OpenEntryInDetail(EntryId(7))),
        "double-click / Enter opens the entry in detail",
    );
    assert!(
        policy.secondary_activate.is_none(),
        "no secondary verb is wired yet",
    );
    assert!(policy.activates_on_double_click());
}

/// A code-block sub-row affordance hovers + selects but has no double-click verb
/// (its copy is an explicit affordance, not a gesture).
#[test]
fn rowspan_policy_hovers_and_selects_without_activating() {
    let policy = policy_for(TargetKey::RowSpan(
        RowId(3),
        crate::interaction::RowSpan::new(0, 4),
    ));
    assert!(policy.hover_preview);
    assert!(policy.select);
    assert!(policy.primary_activate.is_none());
    assert!(
        !policy.activates_on_double_click(),
        "no primary verb -> double-click is a no-op, never a surprise mutation",
    );
}

/// Queue / clipboard / chrome targets are inert under the shared contract so a
/// stray double-click can never fire their (sometimes destructive) verbs by
/// accident — those keep their own explicit single-click dispatch.
#[test]
fn queue_clipboard_and_chrome_targets_are_inert() {
    for key in [
        TargetKey::QueueItem(1),
        TargetKey::ClipboardEntry(2),
        TargetKey::Chrome(ChromeKey::ClipboardDelete),
        TargetKey::Chrome(ChromeKey::ClipboardClear),
        TargetKey::Chrome(ChromeKey::QueueStrip),
    ] {
        let policy = policy_for(key);
        assert!(!policy.hover_preview, "{key:?} reveals no shared preview");
        assert!(
            !policy.select,
            "{key:?} is not selected by the shared policy"
        );
        assert!(
            policy.primary_activate.is_none(),
            "{key:?} has no double-click verb",
        );
        assert!(!policy.activates_on_double_click());
    }
}

/// The double-click path NEVER carries a destructive verb: no target's
/// `primary_activate` is a delete/clear/retry, satisfying the spec's "double-click
/// never triggers a destructive action directly" contract. We assert it for the
/// representative keys plus a sweep of every chrome key classified destructive.
#[test]
fn no_target_activates_a_destructive_verb_on_double_click() {
    // Entry / row span / queue / clipboard / a destructive chrome key: none of
    // them yield a destructive primary verb.
    let keys = [
        TargetKey::Entry(EntryId(0)),
        TargetKey::RowSpan(RowId(0), crate::interaction::RowSpan::new(0, 1)),
        TargetKey::QueueItem(0),
        TargetKey::ClipboardEntry(0),
        TargetKey::Chrome(ChromeKey::ClipboardDelete),
        TargetKey::Chrome(ChromeKey::ClipboardClear),
    ];
    for key in keys {
        let policy = policy_for(key);
        // The only non-None primary verb in the set is the entry's open-in-detail,
        // which is read-only.
        match policy.primary_activate {
            None => {}
            Some(Action::OpenEntryInDetail(_)) => {}
            other => panic!("{key:?} activated an unexpected verb on double-click: {other:?}"),
        }
    }
    // A destructive chrome key is classified destructive AND inert.
    assert!(is_destructive_chrome(ChromeKey::ClipboardDelete));
    assert!(is_destructive_chrome(ChromeKey::ClipboardClear));
    assert!(!is_destructive_chrome(ChromeKey::QueueStrip));
}

// ---- preview content building ----

/// Every `PreviewKind` has a non-empty ASCII noun (meaning never depends on color
/// or a private-use glyph).
#[test]
fn every_preview_kind_has_an_ascii_noun() {
    for kind in [
        PreviewKind::Entry,
        PreviewKind::ToolOutput,
        PreviewKind::Path,
        PreviewKind::Link,
    ] {
        let noun = kind.noun();
        assert!(!noun.is_empty(), "{kind:?} noun is non-empty");
        assert!(noun.is_ascii(), "{kind:?} noun is ASCII: {noun:?}");
    }
}

/// `HoverPreview::new` clamps the body to `PREVIEW_BODY_LINES`, caps each line at
/// `PREVIEW_LINE_CAP`, and drops empty lines — so a careless caller can never blow
/// the popover's fixed size.
#[test]
fn preview_new_bounds_body_lines_and_widths() {
    let body: Vec<String> = (0..10)
        .map(|i| {
            if i == 1 {
                // An empty/whitespace line that must be filtered out.
                "   ".to_string()
            } else {
                "x".repeat(PREVIEW_LINE_CAP + 30)
            }
        })
        .collect();
    let preview = HoverPreview::new(
        42,
        PreviewKind::Entry,
        "  a very   spaced    title  ".to_string(),
        body,
        Some(Action::OpenEntryInDetail(EntryId(42))),
        PreviewSource::Hover,
    );
    assert!(
        preview.body.len() <= PREVIEW_BODY_LINES,
        "body is capped to {PREVIEW_BODY_LINES}: {}",
        preview.body.len(),
    );
    for line in &preview.body {
        assert!(
            line.chars().count() <= PREVIEW_LINE_CAP + 1,
            "each body line is capped (+1 for the ellipsis): {line:?}",
        );
        assert!(
            !line.trim().is_empty(),
            "empty lines are filtered: {line:?}"
        );
    }
    assert_eq!(
        preview.title, "a very spaced title",
        "the title is whitespace-collapsed",
    );
    assert!(preview.can_activate(), "an open-in-detail verb activates");
    assert_eq!(preview.activate_hint(), "double-click / Enter to open");
}

/// A preview with no primary verb reports read-only and a click-to-select hint.
#[test]
fn preview_without_primary_is_read_only() {
    let preview = HoverPreview::new(
        1,
        PreviewKind::ToolOutput,
        "shell".to_string(),
        vec!["all tests passed".to_string()],
        None,
        PreviewSource::Hover,
    );
    assert!(!preview.can_activate());
    assert_eq!(preview.activate_hint(), "click to select");
    assert!(
        !preview.is_keyboard(),
        "a hover-sourced preview is not keyboard"
    );
}

/// The keyboard source is reported honestly (drives the sticky-against-drift rule).
#[test]
fn keyboard_sourced_preview_is_flagged() {
    let preview = HoverPreview::new(
        1,
        PreviewKind::Entry,
        "t".to_string(),
        vec![],
        None,
        PreviewSource::Keyboard,
    );
    assert!(preview.is_keyboard());
}

/// `clamp_line` collapses whitespace, leaves short ASCII untouched, and truncates
/// an over-long line with an ellipsis without panicking on multi-byte chars.
#[test]
fn clamp_line_collapses_and_truncates() {
    assert_eq!(clamp_line("  hello   world  "), "hello world");
    let long = "é".repeat(PREVIEW_LINE_CAP + 5);
    let clamped = clamp_line(&long);
    assert_eq!(
        clamped.chars().count(),
        PREVIEW_LINE_CAP + 1,
        "truncated to the cap plus a single ellipsis char",
    );
    assert!(clamped.ends_with('\u{2026}'), "ends with an ellipsis");
}

// ---- popover geometry ----

/// The popover prefers BELOW the anchor row and always sits fully inside `area`.
#[test]
fn popover_places_below_and_inside_the_area() {
    let area = Rect::new(0, 0, 120, 40);
    let rect = popover_rect(area, 5, 3).expect("a roomy area hosts the popover");
    assert!(rect.y > 5, "placed below the anchor row: {rect:?}");
    assert!(rect.x >= area.x);
    assert!(rect.y >= area.y);
    assert!(
        rect.x + rect.width <= area.x + area.width,
        "never runs off the right edge: {rect:?}",
    );
    assert!(
        rect.y + rect.height <= area.y + area.height,
        "never runs off the bottom edge: {rect:?}",
    );
}

/// When the anchor sits near the BOTTOM of the area, the popover flips ABOVE it
/// rather than overflowing the bottom edge.
#[test]
fn popover_flips_above_when_no_room_below() {
    let area = Rect::new(0, 0, 120, 20);
    // Anchor at the last row: there is no room below.
    let rect = popover_rect(area, 19, 4).expect("popover still fits above");
    assert!(
        rect.y + rect.height <= area.y + area.height,
        "stays inside even when flipped above: {rect:?}",
    );
    assert!(rect.y < 19, "placed above the bottom anchor: {rect:?}");
}

/// In a non-zero offset area, the popover never escapes the area's origin on any
/// edge (resize / split-pane safety).
#[test]
fn popover_respects_a_non_origin_area() {
    let area = Rect::new(10, 4, 60, 18);
    for anchor in [area.y, area.y + 9, area.y + area.height - 1] {
        let rect = popover_rect(area, anchor, 4).expect("fits in the sub-area");
        assert!(rect.x >= area.x, "left edge inside: {rect:?}");
        assert!(rect.y >= area.y, "top edge inside: {rect:?}");
        assert!(
            rect.x + rect.width <= area.x + area.width,
            "right edge inside: {rect:?}",
        );
        assert!(
            rect.y + rect.height <= area.y + area.height,
            "bottom edge inside: {rect:?}",
        );
    }
}

/// A degenerate (too-small) area yields no popover rather than a clipped one.
#[test]
fn popover_declines_a_too_small_area() {
    assert!(popover_rect(Rect::new(0, 0, 3, 10), 0, 2).is_none());
    assert!(popover_rect(Rect::new(0, 0, 40, 2), 0, 2).is_none());
}

/// The popover width never exceeds the available area width even on a narrow
/// terminal — the "palette fallback on narrow terminals" geometry guarantee.
#[test]
fn popover_width_clamps_to_a_narrow_area() {
    let area = Rect::new(0, 0, 12, 24);
    let rect = popover_rect(area, 2, 3).expect("a narrow area still hosts a peek");
    assert!(
        rect.width <= area.width,
        "width clamps to the narrow area: {rect:?}",
    );
    assert!(rect.x + rect.width <= area.x + area.width);
}
