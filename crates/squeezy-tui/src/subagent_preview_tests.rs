//! Unit tests for the Subagent Hover Preview And Double-Click Jump (§12.8.2) pure
//! model: status distillation, activation classification, return-anchor data, and
//! the preview content / geometry.

use super::*;
use crate::hover_preview::PreviewSource;

fn preview(status: SubagentStatus, source: PreviewSource) -> SubagentPreview {
    SubagentPreview::new(
        2,
        "delegate #3".to_string(),
        status,
        "running grep over src/".to_string(),
        Some("tools=4 · bytes=2048".to_string()),
        source,
    )
}

#[test]
fn status_labels_are_ascii_words() {
    assert_eq!(SubagentStatus::Running.label(), "running");
    assert_eq!(SubagentStatus::Done.label(), "done");
    assert_eq!(SubagentStatus::Failed.label(), "failed");
    assert_eq!(SubagentStatus::Capped.label(), "capped");
    // ASCII-only so meaning never depends on color/glyph.
    for status in [
        SubagentStatus::Running,
        SubagentStatus::Done,
        SubagentStatus::Failed,
        SubagentStatus::Capped,
    ] {
        assert!(status.label().is_ascii(), "{:?}", status);
    }
}

#[test]
fn only_capped_lacks_a_transcript() {
    assert!(SubagentStatus::Running.has_transcript());
    assert!(SubagentStatus::Done.has_transcript());
    assert!(SubagentStatus::Failed.has_transcript());
    assert!(!SubagentStatus::Capped.has_transcript());
}

#[test]
fn activation_target_jumps_for_transcript_else_pins() {
    assert_eq!(
        activation_target(SubagentStatus::Running),
        SubagentActivationTarget::TranscriptDetail
    );
    assert_eq!(
        activation_target(SubagentStatus::Done),
        SubagentActivationTarget::TranscriptDetail
    );
    assert_eq!(
        activation_target(SubagentStatus::Failed),
        SubagentActivationTarget::TranscriptDetail
    );
    // A capped subagent has no transcript: activation is a select/pin, not a jump.
    assert_eq!(
        activation_target(SubagentStatus::Capped),
        SubagentActivationTarget::TimelinePane
    );
}

#[test]
fn can_jump_tracks_transcript_presence() {
    assert!(preview(SubagentStatus::Running, PreviewSource::Hover).can_jump());
    assert!(preview(SubagentStatus::Done, PreviewSource::Hover).can_jump());
    assert!(preview(SubagentStatus::Failed, PreviewSource::Hover).can_jump());
    assert!(!preview(SubagentStatus::Capped, PreviewSource::Hover).can_jump());
}

#[test]
fn activate_hint_is_honest_about_jump() {
    assert_eq!(
        preview(SubagentStatus::Done, PreviewSource::Hover).activate_hint(),
        "double-click / jump to open transcript"
    );
    assert_eq!(
        preview(SubagentStatus::Capped, PreviewSource::Hover).activate_hint(),
        "click to select"
    );
}

#[test]
fn body_lists_activity_then_metrics_bounded() {
    let p = preview(SubagentStatus::Running, PreviewSource::Hover);
    let body = p.body();
    assert_eq!(body.len(), 2);
    assert_eq!(body[0], "running grep over src/");
    assert_eq!(body[1], "tools=4 · bytes=2048");
    assert!(body.len() <= SUBAGENT_PREVIEW_BODY_LINES);
}

#[test]
fn body_skips_empty_activity_and_absent_metrics() {
    let p = SubagentPreview::new(
        0,
        "delegate #1".to_string(),
        SubagentStatus::Running,
        String::new(),
        None,
        PreviewSource::Keyboard,
    );
    // No last activity, no metrics yet (the missing-metrics edge): the body is
    // empty, so the popover shows the status header alone.
    assert!(p.body().is_empty());
}

#[test]
fn keyboard_source_is_sticky() {
    assert!(preview(SubagentStatus::Done, PreviewSource::Keyboard).is_keyboard());
    assert!(!preview(SubagentStatus::Done, PreviewSource::Hover).is_keyboard());
}

#[test]
fn new_clamps_each_line_to_one_bounded_line() {
    let long = "x".repeat(500);
    let p = SubagentPreview::new(
        0,
        long.clone(),
        SubagentStatus::Done,
        format!("line1\nline2\n{long}"),
        Some(long.clone()),
        PreviewSource::Hover,
    );
    // The constructor collapses whitespace/newlines and caps width, so no field can
    // blow the popover's fixed size.
    assert!(p.name.chars().count() <= crate::hover_preview::PREVIEW_LINE_CAP + 1);
    for line in p.body() {
        assert!(line.chars().count() <= crate::hover_preview::PREVIEW_LINE_CAP + 1);
        // Newlines were collapsed into a single line.
        assert!(!line.contains('\n'));
    }
}

#[test]
fn return_anchor_records_prior_position() {
    let anchor = SubagentReturnAnchor::new(3, true);
    assert_eq!(anchor.prior_selected, 3);
    assert!(anchor.prior_was_main);
    let anchor2 = SubagentReturnAnchor::new(0, false);
    assert_eq!(anchor2.prior_selected, 0);
    assert!(!anchor2.prior_was_main);
}
