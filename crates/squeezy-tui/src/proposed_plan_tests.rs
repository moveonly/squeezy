use super::{
    BUILD_PLAN_STILL_IN_EFFECT_FORMAT, CURRENT_POINTER_FILE, LEGACY_PLAN_DIR, PLAN_DIR,
    PLAN_RETENTION_LIMIT, PlanLookupError, PlanMeta, ProposedPlanExtractor, current_pointer_for,
    delete_plan, extract_plan_ids, list_plans, migrate_legacy_plans, persist_plan, plan_file_for,
    plan_id_for, prune_plan_dir, read_current_plan_id, read_plan_body, resolve_plan_prefix,
    session_plan_dir, set_active_plan, strip_front_matter,
};
use std::collections::HashSet;

const TEST_SESSION_ID: &str = "test-sess-tui";
const OTHER_SESSION_ID: &str = "test-sess-other";

fn empty_meta() -> PlanMeta {
    PlanMeta::default()
}

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
fn persist_plan_writes_per_plan_id_under_session_dir() {
    let root = fresh_workspace("persist_per_id");
    let (plan_id, path) = persist_plan(&root, TEST_SESSION_ID, "step 1\nstep 2", &empty_meta())
        .expect("persist plan");
    assert_eq!(plan_id, plan_id_for("step 1\nstep 2"));
    assert!(plan_id.starts_with("plan-"));
    assert_eq!(path, plan_file_for(&root, TEST_SESSION_ID, &plan_id));
    assert!(path.starts_with(root.join(PLAN_DIR).join(TEST_SESSION_ID)));
    let body = read_plan_body(&path).expect("read plan body");
    assert_eq!(body, "step 1\nstep 2\n");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn persist_plan_distinct_bodies_produce_distinct_files() {
    let root = fresh_workspace("persist_distinct");
    let (id_a, path_a) =
        persist_plan(&root, TEST_SESSION_ID, "first", &empty_meta()).expect("persist first");
    let (id_b, path_b) =
        persist_plan(&root, TEST_SESSION_ID, "second", &empty_meta()).expect("persist second");
    assert_ne!(id_a, id_b, "different bodies must mint different plan ids");
    assert_ne!(path_a, path_b);
    assert_eq!(read_plan_body(&path_a).unwrap(), "first\n");
    assert_eq!(read_plan_body(&path_b).unwrap(), "second\n");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn persist_plan_identical_body_reuses_plan_id() {
    let root = fresh_workspace("persist_reuse");
    let (id_a, path_a) =
        persist_plan(&root, TEST_SESSION_ID, "same body", &empty_meta()).expect("persist a");
    let (id_b, path_b) =
        persist_plan(&root, TEST_SESSION_ID, "same body", &empty_meta()).expect("persist b");
    assert_eq!(id_a, id_b);
    assert_eq!(path_a, path_b);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn persist_plan_updates_current_pointer() {
    let root = fresh_workspace("persist_pointer");
    let (id_a, _) =
        persist_plan(&root, TEST_SESSION_ID, "alpha", &empty_meta()).expect("persist a");
    assert_eq!(
        read_current_plan_id(&root, TEST_SESSION_ID).as_deref(),
        Some(id_a.as_str()),
        "current pointer must follow the most recent persist"
    );
    let (id_b, _) = persist_plan(&root, TEST_SESSION_ID, "beta", &empty_meta()).expect("persist b");
    assert_ne!(id_a, id_b);
    assert_eq!(
        read_current_plan_id(&root, TEST_SESSION_ID).as_deref(),
        Some(id_b.as_str()),
        "subsequent persist must repoint the current pointer"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn persist_plan_isolates_sessions() {
    let root = fresh_workspace("session_isolation");
    let (id_a, path_a) =
        persist_plan(&root, TEST_SESSION_ID, "same body", &empty_meta()).expect("persist a");
    let (id_b, path_b) =
        persist_plan(&root, OTHER_SESSION_ID, "same body", &empty_meta()).expect("persist b");
    // Body-derived id collides; per-session subdirs must keep the files
    // apart on disk so concurrent sessions never see each other's plans.
    assert_eq!(id_a, id_b);
    assert_ne!(path_a, path_b);
    assert!(path_a.starts_with(session_plan_dir(&root, TEST_SESSION_ID)));
    assert!(path_b.starts_with(session_plan_dir(&root, OTHER_SESSION_ID)));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn prune_plan_dir_keeps_newest_within_retention_limit() {
    let root = fresh_workspace("prune_caps_dir");
    let plans_dir = session_plan_dir(&root, TEST_SESSION_ID);
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
    let deleted = prune_plan_dir(&root, TEST_SESSION_ID, &HashSet::new());
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
fn prune_plan_dir_does_not_touch_sibling_sessions() {
    let root = fresh_workspace("prune_scopes_to_session");
    let plans_a = session_plan_dir(&root, TEST_SESSION_ID);
    let plans_b = session_plan_dir(&root, OTHER_SESSION_ID);
    std::fs::create_dir_all(&plans_a).expect("mkdir a");
    std::fs::create_dir_all(&plans_b).expect("mkdir b");
    for idx in 0..(PLAN_RETENTION_LIMIT + 2) {
        std::fs::write(plans_a.join(format!("plan-{idx:04}.md")), "body").expect("write a");
    }
    // Sibling session has just a single plan; pruning A must not touch
    // anything in B.
    std::fs::write(plans_b.join("plan-keep.md"), "body b").expect("write b");

    let deleted = prune_plan_dir(&root, TEST_SESSION_ID, &HashSet::new());
    assert!(deleted >= 2, "prune must remove the extras in session A");
    assert!(
        plans_b.join("plan-keep.md").exists(),
        "sibling session must be untouched"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn prune_plan_dir_noops_when_under_limit() {
    let root = fresh_workspace("prune_under_limit");
    let plans_dir = session_plan_dir(&root, TEST_SESSION_ID);
    std::fs::create_dir_all(&plans_dir).expect("mkdir plans");
    for idx in 0..3 {
        std::fs::write(plans_dir.join(format!("plan-{idx}.md")), "body").expect("write");
    }
    assert_eq!(prune_plan_dir(&root, TEST_SESSION_ID, &HashSet::new()), 0);
    let count = std::fs::read_dir(&plans_dir).expect("read").count();
    assert_eq!(count, 3);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn prune_plan_dir_noops_when_dir_missing() {
    let root = fresh_workspace("prune_missing");
    assert_eq!(prune_plan_dir(&root, TEST_SESSION_ID, &HashSet::new()), 0);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn prune_plan_dir_keeps_protected_plan_ids() {
    let root = fresh_workspace("prune_protected");
    let plans_dir = session_plan_dir(&root, TEST_SESSION_ID);
    std::fs::create_dir_all(&plans_dir).expect("mkdir plans");
    let total = PLAN_RETENTION_LIMIT + 3;
    let now = std::time::SystemTime::now();
    let mut protected_id = String::new();
    for idx in 0..total {
        let id = format!("plan-{idx:04}");
        let path = plans_dir.join(format!("{id}.md"));
        std::fs::write(&path, format!("body {idx}")).expect("write");
        let mtime = now - std::time::Duration::from_secs((total - idx) as u64);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("open")
            .set_modified(mtime)
            .expect("set mtime");
        // Pick one of the eldest entries as the "protected" id.
        if idx == 1 {
            protected_id = id;
        }
    }
    let mut protected = HashSet::new();
    protected.insert(protected_id.clone());
    let deleted = prune_plan_dir(&root, TEST_SESSION_ID, &protected);
    // Protected plan would have been deleted under default rules but
    // must survive due to the protected set.
    assert!(
        plans_dir.join(format!("{protected_id}.md")).exists(),
        "protected plan id must not be deleted"
    );
    // Total deletions are one fewer than the unprotected baseline
    // because the protected id was skipped.
    let baseline = total - PLAN_RETENTION_LIMIT;
    assert_eq!(deleted, baseline - 1);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn extract_plan_ids_finds_tokens_anywhere_in_text() {
    let mut text = String::new();
    text.push_str("commit abc123 ref'd plan-abc12345\n");
    text.push_str("diff line plan-deadbeef999 trailing\n");
    text.push_str("the word applan-foo must not match\n");
    text.push_str("standalone plan-0 should match\n");
    let ids = extract_plan_ids(&text);
    assert!(ids.contains("plan-abc12345"));
    assert!(ids.contains("plan-deadbeef999"));
    assert!(ids.contains("plan-0"));
    assert!(
        !ids.iter().any(|id| id.contains("applan")),
        "must not match mid-identifier `applan-foo`: {ids:?}"
    );
}

#[test]
fn read_current_plan_id_returns_none_without_pointer() {
    let root = fresh_workspace("pointer_missing");
    let _ = session_plan_dir(&root, TEST_SESSION_ID); // just for path calc
    assert!(read_current_plan_id(&root, TEST_SESSION_ID).is_none());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn read_current_plan_id_trims_whitespace_and_rejects_empty() {
    let root = fresh_workspace("pointer_trim");
    let plans_dir = session_plan_dir(&root, TEST_SESSION_ID);
    std::fs::create_dir_all(&plans_dir).expect("mkdir");
    let pointer = current_pointer_for(&root, TEST_SESSION_ID);
    std::fs::write(&pointer, "  plan-foo123\n\n").expect("write pointer");
    assert_eq!(
        read_current_plan_id(&root, TEST_SESSION_ID).as_deref(),
        Some("plan-foo123")
    );
    std::fs::write(&pointer, "   \n").expect("rewrite pointer");
    assert!(read_current_plan_id(&root, TEST_SESSION_ID).is_none());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn current_pointer_file_constant_is_stable() {
    // Locked to "current" so the agent-side mirror constant in
    // crates/squeezy-agent/src/plan_mode.rs cannot drift silently.
    assert_eq!(CURRENT_POINTER_FILE, "current");
}

#[test]
fn migrate_legacy_plans_moves_top_level_md_files_only() {
    let root = fresh_workspace("legacy_migrate");
    let plans_dir = root.join(PLAN_DIR);
    std::fs::create_dir_all(&plans_dir).expect("mkdir plans");
    // Two flat-layout plans (pre-v3 shape).
    std::fs::write(plans_dir.join("plan-a.md"), "legacy a").expect("write a");
    std::fs::write(plans_dir.join("plan-b.md"), "legacy b").expect("write b");
    // A session subdir with a plan: must NOT be moved.
    let session_dir = plans_dir.join(TEST_SESSION_ID);
    std::fs::create_dir_all(&session_dir).expect("mkdir session");
    std::fs::write(session_dir.join("plan-keep.md"), "keep").expect("write keep");
    // A non-markdown junk file at top level: must NOT be moved.
    std::fs::write(plans_dir.join("README"), "ignore").expect("write readme");

    let moved = migrate_legacy_plans(&root);
    assert_eq!(moved, 2);

    let legacy = plans_dir.join(LEGACY_PLAN_DIR);
    assert!(legacy.join("plan-a.md").exists());
    assert!(legacy.join("plan-b.md").exists());
    assert!(!plans_dir.join("plan-a.md").exists());
    assert!(!plans_dir.join("plan-b.md").exists());
    assert!(session_dir.join("plan-keep.md").exists());
    assert!(plans_dir.join("README").exists());

    // Second run is a no-op (nothing left at top level).
    assert_eq!(migrate_legacy_plans(&root), 0);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn migrate_legacy_plans_noops_when_dir_missing() {
    let root = fresh_workspace("legacy_missing");
    assert_eq!(migrate_legacy_plans(&root), 0);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn persist_plan_writes_yaml_front_matter() {
    let root = fresh_workspace("front_matter_round_trip");
    let body = "Context: minor doc tweak.\n\n1. Edit README\n2. Verify links";
    let meta = PlanMeta {
        parent_plan_id: Some("plan-parent01".to_string()),
        model: Some("gpt-5-codex".to_string()),
    };
    let (plan_id, path) =
        persist_plan(&root, TEST_SESSION_ID, body, &meta).expect("persist with meta");

    let raw = std::fs::read_to_string(&path).expect("read raw");
    assert!(raw.starts_with("---\n"), "front-matter must open with ---");
    assert!(
        raw.contains(&format!("plan_id: {plan_id}\n")),
        "plan id must appear in front-matter: {raw}"
    );
    assert!(
        raw.contains(&format!("session_id: {TEST_SESSION_ID}\n")),
        "session id must appear in front-matter: {raw}"
    );
    assert!(
        raw.contains("parent_plan_id: plan-parent01\n"),
        "parent_plan_id must round-trip"
    );
    assert!(
        raw.contains("model: gpt-5-codex\n"),
        "model must round-trip"
    );
    assert!(raw.contains("created: "), "created timestamp must be set");
    // Objective is YAML-quoted because the body's first line contains
    // a colon (`Context: …`), which would otherwise be parsed as a
    // nested key. The quoted form still embeds the prefix verbatim.
    assert!(
        raw.contains("objective: 'Context: minor doc tweak.'"),
        "objective must derive from the first non-empty line: {raw}"
    );

    // The body is recoverable via the strip helper.
    let body_only = read_plan_body(&path).expect("read body");
    assert_eq!(body_only, format!("{body}\n"));
}

#[test]
fn persist_plan_omits_parent_and_model_when_unset() {
    let root = fresh_workspace("front_matter_minimal");
    let (_, path) =
        persist_plan(&root, TEST_SESSION_ID, "single step", &empty_meta()).expect("persist plain");
    let raw = std::fs::read_to_string(&path).expect("read raw");
    assert!(!raw.contains("parent_plan_id"));
    assert!(!raw.contains("model:"));
    assert!(raw.contains("plan_id: "));
    assert!(raw.contains("session_id: "));
}

#[test]
fn strip_front_matter_returns_body_only() {
    let input = "---\nplan_id: plan-x\nsession_id: s\n---\nthe body\n";
    assert_eq!(strip_front_matter(input), "the body\n");
}

#[test]
fn strip_front_matter_handles_missing_block() {
    let input = "no front matter here\nsecond line\n";
    assert_eq!(strip_front_matter(input), input);
}

#[test]
fn strip_front_matter_handles_truncated_block() {
    // Open marker but no closer — must not panic and must not chew bytes.
    let input = "---\nplan_id: plan-x\nstill open";
    assert_eq!(strip_front_matter(input), input);
}

#[test]
fn objective_derives_from_first_meaningful_line() {
    let root = fresh_workspace("objective_derivation");
    let body = "\n\n# Heading\n- [ ] Pick a font and validate readability across themes.\n";
    let (_, path) =
        persist_plan(&root, TEST_SESSION_ID, body, &empty_meta()).expect("persist with heading");
    let raw = std::fs::read_to_string(&path).expect("read raw");
    assert!(
        raw.contains("objective: Heading"),
        "heading text should be picked, marker stripped: {raw}"
    );
}

#[test]
fn list_plans_returns_newest_first_and_marks_active() {
    let root = fresh_workspace("list_plans");
    let (id_older, _) =
        persist_plan(&root, TEST_SESSION_ID, "alpha body", &empty_meta()).expect("persist alpha");
    // Force older mtime so ordering is deterministic.
    let alpha_path = plan_file_for(&root, TEST_SESSION_ID, &id_older);
    std::fs::File::options()
        .write(true)
        .open(&alpha_path)
        .expect("open alpha")
        .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(120))
        .expect("set mtime alpha");
    let (id_newer, _) =
        persist_plan(&root, TEST_SESSION_ID, "beta body", &empty_meta()).expect("persist beta");
    let entries = list_plans(&root, TEST_SESSION_ID);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].plan_id, id_newer, "newest first");
    assert_eq!(entries[1].plan_id, id_older);
    assert!(
        entries[0].is_active,
        "the most recently persisted plan is active by default"
    );
    assert!(!entries[1].is_active);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn list_plans_objective_round_trips_through_front_matter() {
    let root = fresh_workspace("list_plans_objective");
    persist_plan(
        &root,
        TEST_SESSION_ID,
        "Fix Foo: tweak the bar",
        &empty_meta(),
    )
    .expect("persist");
    let entries = list_plans(&root, TEST_SESSION_ID);
    assert_eq!(entries.len(), 1);
    // The yaml_scalar quoting must be unwound when read back.
    assert_eq!(entries[0].objective, "Fix Foo: tweak the bar");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn resolve_plan_prefix_finds_unique_match() {
    let root = fresh_workspace("resolve_unique");
    let (id, _) = persist_plan(&root, TEST_SESSION_ID, "body x", &empty_meta()).expect("persist");
    let hex = id.strip_prefix("plan-").unwrap();
    // Full id, hex-only, short prefix all resolve.
    assert_eq!(
        resolve_plan_prefix(&root, TEST_SESSION_ID, &id).unwrap(),
        id
    );
    assert_eq!(
        resolve_plan_prefix(&root, TEST_SESSION_ID, hex).unwrap(),
        id
    );
    assert_eq!(
        resolve_plan_prefix(&root, TEST_SESSION_ID, &hex[..4]).unwrap(),
        id
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn resolve_plan_prefix_reports_not_found_and_ambiguous() {
    let root = fresh_workspace("resolve_disambig");
    // Two plans whose hex tails happen to share the same first character
    // (`plan-0…` collides with `plan-0…` from a different body) — force
    // by writing files directly so we control the ids.
    let plans_dir = session_plan_dir(&root, TEST_SESSION_ID);
    std::fs::create_dir_all(&plans_dir).expect("mkdir");
    std::fs::write(plans_dir.join("plan-aaaaa1.md"), "---\n---\nbody one\n").expect("write a");
    std::fs::write(plans_dir.join("plan-aaaaa2.md"), "---\n---\nbody two\n").expect("write b");

    assert!(matches!(
        resolve_plan_prefix(&root, TEST_SESSION_ID, "plan-aaaaa"),
        Err(PlanLookupError::Ambiguous(_))
    ));
    assert!(matches!(
        resolve_plan_prefix(&root, TEST_SESSION_ID, "plan-zzzz"),
        Err(PlanLookupError::NotFound)
    ));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn delete_plan_removes_file_and_clears_pointer_when_active() {
    let root = fresh_workspace("delete_clears_pointer");
    let (id, path) =
        persist_plan(&root, TEST_SESSION_ID, "body to delete", &empty_meta()).expect("persist");
    assert_eq!(
        read_current_plan_id(&root, TEST_SESSION_ID).as_deref(),
        Some(id.as_str())
    );
    let removed = delete_plan(&root, TEST_SESSION_ID, &id).expect("delete");
    assert_eq!(removed, path);
    assert!(!path.exists());
    assert!(
        read_current_plan_id(&root, TEST_SESSION_ID).is_none(),
        "deleting the active plan must clear the pointer"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn delete_plan_keeps_pointer_when_other_plan_is_active() {
    let root = fresh_workspace("delete_keeps_pointer");
    let (id_first, _) =
        persist_plan(&root, TEST_SESSION_ID, "first body", &empty_meta()).expect("persist first");
    let (id_second, _) =
        persist_plan(&root, TEST_SESSION_ID, "second body", &empty_meta()).expect("persist second");
    // Second persist re-pointed `current` at id_second. Delete the
    // older one and the pointer must keep aiming at id_second.
    delete_plan(&root, TEST_SESSION_ID, &id_first).expect("delete first");
    assert_eq!(
        read_current_plan_id(&root, TEST_SESSION_ID).as_deref(),
        Some(id_second.as_str())
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn set_active_plan_rewrites_pointer_and_rejects_missing_id() {
    let root = fresh_workspace("set_active");
    let (id_a, _) =
        persist_plan(&root, TEST_SESSION_ID, "alpha", &empty_meta()).expect("persist a");
    let (id_b, _) = persist_plan(&root, TEST_SESSION_ID, "beta", &empty_meta()).expect("persist b");
    // Currently `id_b` is active; flip back to `id_a`.
    set_active_plan(&root, TEST_SESSION_ID, &id_a).expect("set active a");
    assert_eq!(
        read_current_plan_id(&root, TEST_SESSION_ID).as_deref(),
        Some(id_a.as_str())
    );
    set_active_plan(&root, TEST_SESSION_ID, &id_b).expect("set active b");
    assert_eq!(
        read_current_plan_id(&root, TEST_SESSION_ID).as_deref(),
        Some(id_b.as_str())
    );
    // Phantom plan must error.
    assert!(set_active_plan(&root, TEST_SESSION_ID, "plan-does-not-exist").is_err());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn objective_truncates_to_80_chars_with_ellipsis() {
    let root = fresh_workspace("objective_truncate");
    let long_line = "x".repeat(120);
    let (_, path) =
        persist_plan(&root, TEST_SESSION_ID, &long_line, &empty_meta()).expect("persist long line");
    let raw = std::fs::read_to_string(&path).expect("read raw");
    assert!(
        raw.contains("objective: ") && raw.contains('…'),
        "long objective must be ellipsised: {raw}"
    );
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("objective: ") {
            // 80 visible chars + ellipsis. Use char count, not byte count.
            assert!(
                rest.chars().count() <= 81,
                "objective must cap at 80 chars + ellipsis: {rest:?}"
            );
        }
    }
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
