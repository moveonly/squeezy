use super::*;

#[test]
fn empty_controller_reports_empty() {
    let c = StreamingController::new();
    assert!(c.is_empty());
    assert!(c.trim_is_empty());
    assert_eq!(c.text(), "");
}

#[test]
fn push_delta_grows_tail_until_newline() {
    let mut c = StreamingController::new();
    assert_eq!(c.push_delta("hel"), StreamingMutation::TailGrew);
    assert_eq!(c.tail(), "hel");
    assert_eq!(c.committed(), "");
    assert_eq!(c.push_delta("lo\nwo"), StreamingMutation::CommittedGrew);
    assert_eq!(c.committed(), "hello\n");
    assert_eq!(c.tail(), "wo");
}

#[test]
fn streaming_finalize_flushes_open_fence() {
    let mut c = StreamingController::new();
    c.push_delta("```rust\nlet x = 1;\n");
    // Unclosed fence — finalize must still drain the held block.
    let out = c.finalize();
    assert!(out.contains("let x = 1;"));
    assert!(c.is_empty());
}

#[test]
fn finalize_drains_tail_into_committed() {
    let mut c = StreamingController::new();
    c.push_delta("hello world");
    let out = c.finalize();
    assert_eq!(out, "hello world");
    assert!(c.is_empty());
}

#[test]
fn streaming_holds_lines_inside_open_fence() {
    let mut c = StreamingController::new();
    c.push_delta("```rust\n");
    c.push_delta("let x = 1;\n");
    // Inside an open fence the lines must not promote into committed
    // — otherwise a code block flashes plain before the closing fence.
    assert_eq!(c.committed(), "");
    assert!(c.tail().contains("let x = 1;"));
    // Closing fence releases the held lines on the next non-fence line.
    c.push_delta("```\n");
    c.push_delta("after\n");
    assert!(c.committed().contains("let x = 1;"));
    assert!(c.committed().contains("after"));
}

#[test]
fn text_matches_concatenation() {
    let mut c = StreamingController::new();
    c.push_delta("alpha\n");
    c.push_delta("beta");
    assert_eq!(c.text(), "alpha\nbeta");
}

#[test]
fn segment_writer_matches_text_without_requiring_concat() {
    let mut c = StreamingController::new();
    c.push_delta("alpha\n");
    c.push_delta("```rust\nlet x = 1;\n");
    c.push_delta("tail");

    let mut out = String::new();
    c.write_to(&mut out).expect("write to string");
    assert_eq!(out, c.text());
    assert_eq!(
        c.segments().collect::<Vec<_>>(),
        vec!["alpha\n", "```rust\nlet x = 1;\n", "tail"]
    );
}

#[test]
fn segmented_hash_matches_full_text_hash() {
    use std::hash::{Hash, Hasher};

    let mut c = StreamingController::new();
    c.push_delta("alpha\n");
    c.push_delta("beta");

    let mut segmented = std::collections::hash_map::DefaultHasher::new();
    c.hash_text(&mut segmented);
    let mut concatenated = std::collections::hash_map::DefaultHasher::new();
    c.text().hash(&mut concatenated);
    assert_eq!(segmented.finish(), concatenated.finish());
}

#[test]
fn empty_delta_is_noop() {
    let mut c = StreamingController::new();
    assert_eq!(c.push_delta(""), StreamingMutation::NoOp);
}

#[test]
fn clear_resets_both_regions() {
    let mut c = StreamingController::new();
    c.push_delta("line1\nline2");
    c.clear();
    assert!(c.is_empty());
    assert!(c.text().is_empty());
}
