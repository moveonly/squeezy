//! Unit tests for the Promote Subagent Result To Prompt (§12.8.4) pure
//! projection: destination selection (idle vs running), status framing,
//! decoration stripping, excerpt bounding, and the empty-body edge.

use super::*;

#[test]
fn destination_idle_fills_composer_running_queues() {
    // The spec's rule: idle fills the composer; an active turn queues the prompt.
    assert_eq!(
        PromoteDestination::for_turn(false),
        PromoteDestination::Composer
    );
    assert_eq!(
        PromoteDestination::for_turn(true),
        PromoteDestination::Queue
    );
}

#[test]
fn destination_verbs_are_ascii_words() {
    assert_eq!(PromoteDestination::Composer.verb(), "filled composer");
    assert_eq!(PromoteDestination::Queue.verb(), "queued");
    for dest in [PromoteDestination::Composer, PromoteDestination::Queue] {
        assert!(dest.verb().is_ascii(), "{dest:?}");
    }
}

#[test]
fn status_labels_and_nouns_are_ascii() {
    for status in [
        PromoteStatus::Running,
        PromoteStatus::Done,
        PromoteStatus::Failed,
        PromoteStatus::Capped,
    ] {
        assert!(status.label().is_ascii(), "{status:?}");
        assert!(status.body_noun().is_ascii(), "{status:?}");
    }
    // The body noun frames what the promoted text *is* for each run state.
    assert_eq!(PromoteStatus::Done.body_noun(), "result");
    assert_eq!(PromoteStatus::Failed.body_noun(), "failure");
    assert_eq!(PromoteStatus::Running.body_noun(), "latest activity");
    assert_eq!(PromoteStatus::Capped.body_noun(), "note");
}

#[test]
fn project_done_frames_result_under_attribution_header() {
    let source = PromoteSource::new(
        "explore #1".to_string(),
        PromoteStatus::Done,
        "found the bug in parser.rs".to_string(),
    );
    let prompt = source.project();
    assert_eq!(
        prompt,
        "From explore #1 (done result):\n\nfound the bug in parser.rs"
    );
    // Header first, then a blank line, then the body.
    let mut lines = prompt.lines();
    assert_eq!(lines.next(), Some("From explore #1 (done result):"));
    assert_eq!(lines.next(), Some(""));
    assert_eq!(lines.next(), Some("found the bug in parser.rs"));
}

#[test]
fn project_failed_uses_failure_noun() {
    let source = PromoteSource::new(
        "reviewer #3".to_string(),
        PromoteStatus::Failed,
        "could not reach host".to_string(),
    );
    let prompt = source.project();
    assert!(prompt.starts_with("From reviewer #3 (failed failure):"));
    assert!(prompt.ends_with("could not reach host"));
}

#[test]
fn project_strips_status_prefix_bullets_and_blockquote() {
    // A failure diagnostic the renderer prefixed + bulleted + quoted.
    let source = PromoteSource::new(
        "reviewer #2".to_string(),
        PromoteStatus::Failed,
        "subagent failed: > - the build broke".to_string(),
    );
    let prompt = source.project();
    // The status prefix, blockquote marker, and bullet are all gone; the bare
    // diagnostic survives.
    assert!(prompt.ends_with("the build broke"), "{prompt}");
    assert!(!prompt.contains("subagent failed:"), "{prompt}");
    assert!(!prompt.contains('>'), "{prompt}");
}

#[test]
fn project_strips_case_insensitive_prefix() {
    let source = PromoteSource::new(
        "reviewer #2".to_string(),
        PromoteStatus::Failed,
        "Subagent Failed: host unreachable".to_string(),
    );
    let prompt = source.project();
    assert!(prompt.ends_with("host unreachable"), "{prompt}");
    assert!(
        !prompt.to_lowercase().contains("subagent failed:"),
        "{prompt}"
    );
}

#[test]
fn project_collapses_whitespace_and_joins_lines() {
    let source = PromoteSource::new(
        "explore #1".to_string(),
        PromoteStatus::Done,
        "first   line\n\n   second line  ".to_string(),
    );
    let prompt = source.project();
    // Lines join with a single space; interior whitespace collapses.
    assert!(prompt.ends_with("first line second line"), "{prompt}");
}

#[test]
fn project_drops_code_fence_rows() {
    let source = PromoteSource::new(
        "explore #1".to_string(),
        PromoteStatus::Done,
        "```\nlet x = 1;\n```".to_string(),
    );
    let prompt = source.project();
    // The fence rows vanish; the code line survives without its backticks.
    assert!(prompt.ends_with("let x = 1;"), "{prompt}");
    assert!(!prompt.contains("```"), "{prompt}");
}

#[test]
fn project_bounds_body_to_excerpt_cap() {
    let long = "word ".repeat(400); // ~2000 chars
    let source = PromoteSource::new("explore #1".to_string(), PromoteStatus::Done, long);
    let prompt = source.project();
    let body = prompt
        .split_once("\n\n")
        .map(|(_, body)| body)
        .expect("body after header");
    // The body is bounded to the cap (+1 for the ellipsis), never the full input.
    assert!(
        body.chars().count() <= PROMOTE_BODY_CAP + 1,
        "body len {} exceeds cap",
        body.chars().count()
    );
    assert!(body.ends_with('\u{2026}'), "truncation ellipsis: {body}");
}

#[test]
fn project_empty_body_frames_header_alone() {
    // A running subagent that has reported nothing yet: no body, header only.
    let source = PromoteSource::new(
        "delegate #2".to_string(),
        PromoteStatus::Running,
        String::new(),
    );
    assert!(!source.has_body());
    let prompt = source.project();
    assert_eq!(prompt, "From delegate #2 (running latest activity):");
    // No trailing blank/body — the header stands alone.
    assert!(!prompt.contains("\n\n"), "{prompt}");
}

#[test]
fn project_decoration_only_body_is_treated_as_empty() {
    // A result that is pure decoration (fence + blockquote markers) has no body.
    let source = PromoteSource::new(
        "explore #1".to_string(),
        PromoteStatus::Done,
        "```\n>\n```".to_string(),
    );
    assert!(!source.has_body());
    assert_eq!(source.project(), "From explore #1 (done result):");
}

#[test]
fn has_body_true_for_real_text() {
    let source = PromoteSource::new(
        "explore #1".to_string(),
        PromoteStatus::Done,
        "found the bug".to_string(),
    );
    assert!(source.has_body());
}

#[test]
fn clean_body_is_char_boundary_safe_on_multibyte_input() {
    // A multibyte body longer than the cap must truncate on a char boundary
    // (never panic mid-codepoint).
    let body = "é".repeat(PROMOTE_BODY_CAP + 50);
    let cleaned = clean_body(&body);
    assert!(cleaned.chars().count() <= PROMOTE_BODY_CAP + 1);
    assert!(cleaned.ends_with('\u{2026}'));
}
