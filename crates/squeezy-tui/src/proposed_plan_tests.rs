use super::{
    BUILD_PLAN_STILL_IN_EFFECT_FORMAT, PLAN_DIR, PLAN_RETENTION_LIMIT, ProposedPlanExtractor,
    persist_plan, plan_file_for, plan_id_for, prune_plan_dir,
};

#[test]
fn build_plan_still_in_effect_template_has_path_placeholder() {
    assert!(
        BUILD_PLAN_STILL_IN_EFFECT_FORMAT.contains("{path}"),
        "BUILD_PLAN_STILL_IN_EFFECT_FORMAT must embed a {{path}} placeholder so the TUI can substitute the active plan file"
    );
    assert!(
        BUILD_PLAN_STILL_IN_EFFECT_FORMAT.contains("plan still in effect"),
        "BUILD_PLAN_STILL_IN_EFFECT_FORMAT must keep the recognisable marker"
    );
}

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

fn fresh_workspace(name: &str) -> std::path::PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let root = std::env::temp_dir().join(format!("squeezy_plan_{name}_{nonce}"));
    std::fs::create_dir_all(&root).expect("mkdir workspace");
    root
}

#[test]
fn persist_plan_writes_per_plan_id_under_dot_squeezy_plans() {
    let root = fresh_workspace("persist_per_id");
    let (plan_id, path) = persist_plan(&root, "step 1\nstep 2").expect("persist plan");
    assert_eq!(plan_id, plan_id_for("step 1\nstep 2"));
    assert!(plan_id.starts_with("plan-"));
    assert_eq!(path, plan_file_for(&root, &plan_id));
    assert!(path.starts_with(root.join(PLAN_DIR)));
    let on_disk = std::fs::read_to_string(&path).expect("read plan");
    assert_eq!(on_disk, "step 1\nstep 2\n");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn persist_plan_distinct_bodies_produce_distinct_files() {
    let root = fresh_workspace("persist_distinct");
    let (id_a, path_a) = persist_plan(&root, "first").expect("persist first");
    let (id_b, path_b) = persist_plan(&root, "second").expect("persist second");
    assert_ne!(id_a, id_b, "different bodies must mint different plan ids");
    assert_ne!(path_a, path_b);
    assert_eq!(std::fs::read_to_string(&path_a).unwrap(), "first\n");
    assert_eq!(std::fs::read_to_string(&path_b).unwrap(), "second\n");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn persist_plan_identical_body_reuses_plan_id() {
    let root = fresh_workspace("persist_reuse");
    let (id_a, path_a) = persist_plan(&root, "same body").expect("persist a");
    let (id_b, path_b) = persist_plan(&root, "same body").expect("persist b");
    assert_eq!(id_a, id_b);
    assert_eq!(path_a, path_b);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn prune_plan_dir_keeps_newest_within_retention_limit() {
    let root = fresh_workspace("prune_caps_dir");
    let plans_dir = root.join(PLAN_DIR);
    std::fs::create_dir_all(&plans_dir).expect("mkdir plans");
    let total = PLAN_RETENTION_LIMIT + 5;
    let now = std::time::SystemTime::now();
    let mut newest_paths = Vec::new();
    for idx in 0..total {
        let path = plans_dir.join(format!("plan-{idx:04}.md"));
        std::fs::write(&path, format!("plan body {idx}")).expect("write plan");
        let mtime = now - std::time::Duration::from_secs((total - idx) as u64);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("open plan")
            .set_modified(mtime)
            .expect("set mtime");
        if idx >= total - PLAN_RETENTION_LIMIT {
            newest_paths.push(path);
        }
    }
    let deleted = prune_plan_dir(&root);
    assert_eq!(deleted, total - PLAN_RETENTION_LIMIT);
    let remaining: Vec<_> = std::fs::read_dir(&plans_dir)
        .expect("read plans")
        .flatten()
        .map(|entry| entry.path())
        .collect();
    assert_eq!(remaining.len(), PLAN_RETENTION_LIMIT);
    for path in &newest_paths {
        assert!(path.exists(), "newest plans must survive prune: {path:?}");
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn prune_plan_dir_noops_when_under_limit() {
    let root = fresh_workspace("prune_under_limit");
    let plans_dir = root.join(PLAN_DIR);
    std::fs::create_dir_all(&plans_dir).expect("mkdir plans");
    for idx in 0..3 {
        std::fs::write(plans_dir.join(format!("plan-{idx}.md")), "body").expect("write");
    }
    assert_eq!(prune_plan_dir(&root), 0);
    let count = std::fs::read_dir(&plans_dir).expect("read").count();
    assert_eq!(count, 3);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn prune_plan_dir_noops_when_dir_missing() {
    let root = fresh_workspace("prune_missing");
    assert_eq!(prune_plan_dir(&root), 0);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn extracts_two_blocks_in_one_turn() {
    let mut p = ProposedPlanExtractor::new();
    let out = p.feed(
        "first <proposed_plan>plan A</proposed_plan> middle <proposed_plan>plan B</proposed_plan> tail",
    );
    assert_eq!(out.passthrough, "first  middle  tail");
    assert_eq!(
        out.completed,
        vec!["plan A".to_string(), "plan B".to_string()]
    );
}

#[test]
fn extracts_two_blocks_split_across_deltas() {
    let mut p = ProposedPlanExtractor::new();
    let mut combined_passthrough = String::new();
    let mut completed = Vec::new();
    for delta in [
        "intro <proposed_plan>plan ",
        "A</proposed_plan>between<propos",
        "ed_plan>plan B</proposed_plan>end",
    ] {
        let out = p.feed(delta);
        combined_passthrough.push_str(&out.passthrough);
        completed.extend(out.completed);
    }
    assert_eq!(combined_passthrough, "intro betweenend");
    assert_eq!(completed, vec!["plan A".to_string(), "plan B".to_string()]);
}

#[test]
fn bom_in_narration_passes_through() {
    let mut p = ProposedPlanExtractor::new();
    let out = p.feed("\u{feff}intro <proposed_plan>body</proposed_plan>tail");
    assert_eq!(out.passthrough, "\u{feff}intro tail");
    assert_eq!(out.completed, vec!["body".to_string()]);
}

#[test]
fn crlf_inside_block_body_is_preserved() {
    let mut p = ProposedPlanExtractor::new();
    let out = p.feed("<proposed_plan>\r\nstep 1\r\nstep 2\r\n</proposed_plan>");
    assert_eq!(out.passthrough, "");
    // trim() in feed() strips the leading/trailing \r\n but keeps interior.
    assert_eq!(out.completed, vec!["step 1\r\nstep 2".to_string()]);
}

#[test]
fn multibyte_chars_around_tag_do_not_break_safe_emit() {
    // Plan body contains non-ASCII; narration around tags too. Must not
    // panic on char boundaries and must round-trip cleanly.
    let mut p = ProposedPlanExtractor::new();
    let out = p.feed("café <proposed_plan>étape 1\nétape 2</proposed_plan>résumé");
    assert_eq!(out.passthrough, "café résumé");
    assert_eq!(out.completed, vec!["étape 1\nétape 2".to_string()]);
}

/// Property: feeding the same input one byte at a time yields the same
/// observable output as feeding it in one shot. Covers tag splits across
/// every byte boundary in the input (open tag, body, close tag, and the
/// surrounding narration).
#[test]
fn byte_at_a_time_matches_single_shot() {
    let input = "lead-in <proposed_plan>step 1\nstep 2\nstep 3</proposed_plan> trailing narration <proposed_plan>second\nplan</proposed_plan> end";

    let mut single = ProposedPlanExtractor::new();
    let single_out = single.feed(input);
    let single_leftover = single.finalize();

    let mut streamed = ProposedPlanExtractor::new();
    let mut passthrough = String::new();
    let mut completed: Vec<String> = Vec::new();
    let mut idx = 0;
    while idx < input.len() {
        // Step over multibyte chars one codepoint at a time.
        let mut end = idx + 1;
        while !input.is_char_boundary(end) {
            end += 1;
        }
        let out = streamed.feed(&input[idx..end]);
        passthrough.push_str(&out.passthrough);
        completed.extend(out.completed);
        idx = end;
    }
    let streamed_leftover = streamed.finalize();

    assert_eq!(
        passthrough, single_out.passthrough,
        "passthrough must match"
    );
    assert_eq!(
        completed, single_out.completed,
        "completed blocks must match"
    );
    assert_eq!(streamed_leftover, single_leftover, "finalize must match");
}

/// Property: cutting the stream at every byte offset and calling finalize
/// must never panic and must produce some leftover such that
/// passthrough+leftover ⊇ all input bytes that aren't strictly inside a
/// fully-formed block (with the partial-tag bytes preserved one way or
/// the other). We assert the no-panic invariant strictly, and a weaker
/// "every byte is accounted for" invariant on the reassembled output.
#[test]
fn cancellation_at_every_offset_is_safe() {
    let input = "narration <proposed_plan>body of plan\nwith multiple lines</proposed_plan> tail";

    for cut in 0..=input.len() {
        if !input.is_char_boundary(cut) {
            continue;
        }
        let prefix = &input[..cut];
        let mut p = ProposedPlanExtractor::new();
        let out = p.feed(prefix);
        let leftover = p.finalize();

        // No bytes vanish: the union of passthrough text, completed bodies
        // (with their tags re-added so we can compare), and the leftover
        // must contain the same character set as the prefix.
        let mut reassembled = out.passthrough.clone();
        for body in &out.completed {
            reassembled.push_str("<proposed_plan>");
            reassembled.push_str(body);
            reassembled.push_str("</proposed_plan>");
        }
        reassembled.push_str(&leftover);

        // Bag comparison: passthrough comes from two sides of the block in
        // the input but reassembles in a single span, so order can differ.
        // We do, however, require every non-whitespace character of the
        // prefix to be present somewhere in the reassembly (trim() inside
        // feed() can collapse whitespace, so we ignore it).
        let bag = |s: &str| -> std::collections::BTreeMap<char, usize> {
            let mut m = std::collections::BTreeMap::new();
            for c in s.chars().filter(|c| !c.is_whitespace()) {
                *m.entry(c).or_insert(0) += 1;
            }
            m
        };
        assert_eq!(
            bag(&reassembled),
            bag(prefix),
            "byte-set must round-trip at cut={cut}"
        );

        // finalize() must be idempotent (calling it again is harmless).
        let second = p.finalize();
        assert!(
            second.is_empty(),
            "finalize must be idempotent at cut={cut}"
        );
    }
}

/// Known limitation: a `<proposed_plan>` tag inside a markdown code fence
/// is still extracted as if it were a real plan block. The parser is
/// byte-based and has no markdown awareness. Tracked for plan-mode v3
/// follow-up; ignored so it documents the gap without failing CI.
#[test]
#[ignore = "v3 follow-up: parser is markdown-unaware; code-fenced tags currently get extracted"]
fn code_fenced_tag_is_not_extracted() {
    let mut p = ProposedPlanExtractor::new();
    let out = p.feed("Example:\n```\n<proposed_plan>not a real plan</proposed_plan>\n```\n");
    // Desired behaviour: the whole code fence flows through as narration.
    assert!(
        out.completed.is_empty(),
        "tags inside code fences should not count as real plans"
    );
    assert!(out.passthrough.contains("<proposed_plan>"));
}
