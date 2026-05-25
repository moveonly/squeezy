use super::{
    PLAN_DIR, PLAN_RETENTION_LIMIT, ProposedPlanExtractor, persist_plan, plan_file_for,
    plan_id_for, prune_plan_dir,
};

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
