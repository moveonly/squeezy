use super::ProposedPlanExtractor;

#[test]
fn passthrough_when_no_block() {
    let mut p = ProposedPlanExtractor::new();
    let out = p.feed("just narration\n");
    assert_eq!(out.passthrough, "just narration\n");
    assert!(out.completed.is_empty());
}

#[test]
fn extracts_block_spanning_multiple_deltas() {
    let mut p = ProposedPlanExtractor::new();
    let mut combined = String::new();
    let mut completed = Vec::new();
    for delta in [
        "intro <propos",
        "ed_plan>\nstep 1\nstep 2\n</propos",
        "ed_plan>\ntrailing",
    ] {
        let out = p.feed(delta);
        combined.push_str(&out.passthrough);
        completed.extend(out.completed);
    }
    assert_eq!(combined, "intro \ntrailing");
    assert_eq!(completed, vec!["step 1\nstep 2".to_string()]);
}

#[test]
fn split_open_tag_buffers_safely() {
    let mut p = ProposedPlanExtractor::new();
    let out1 = p.feed("hello <");
    assert_eq!(out1.passthrough, "hello ");
    let out2 = p.feed("propose");
    // We do not yet know if this is `<proposed_plan>`; nothing should
    // flow to the assistant yet because the partial tag is still in
    // progress.
    assert!(out2.passthrough.is_empty());
    let out3 = p.feed("d_plan>body</proposed_plan>tail");
    assert_eq!(out3.passthrough, "tail");
    assert_eq!(out3.completed, vec!["body".to_string()]);
}

#[test]
fn non_tag_lt_does_not_buffer_forever() {
    let mut p = ProposedPlanExtractor::new();
    let out = p.feed("x < y");
    assert_eq!(out.passthrough, "x < y");
    assert!(!p.is_open());
}

#[test]
fn finalize_recovers_unterminated_block_into_passthrough() {
    let mut p = ProposedPlanExtractor::new();
    let out = p.feed("intro <proposed_plan>step 1");
    assert_eq!(out.passthrough, "intro ");
    assert!(p.is_open());
    let leftover = p.finalize();
    assert_eq!(leftover, "<proposed_plan>step 1");
    assert!(!p.is_open());
}

#[test]
fn handles_block_with_no_inner_newlines() {
    let mut p = ProposedPlanExtractor::new();
    let out = p.feed("a<proposed_plan>plan body</proposed_plan>b");
    assert_eq!(out.passthrough, "ab");
    assert_eq!(out.completed, vec!["plan body".to_string()]);
}
