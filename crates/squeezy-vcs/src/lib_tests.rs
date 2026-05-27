use std::{
    fs,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use super::*;

static VCS_NONCE: AtomicU64 = AtomicU64::new(0);

#[test]
fn parses_patch_hunks_as_zero_based_line_ranges() {
    let patch = "@@ -1,2 +1,3 @@\n-a\n+b\n+c\n@@ -10 +12,2 @@\n";
    let hunks = parse_patch_hunks(patch);
    assert_eq!(hunks.len(), 2);
    assert_eq!(hunks[0].start_line, 0);
    assert_eq!(hunks[0].end_line, 2);
    assert_eq!(hunks[1].start_line, 11);
    assert_eq!(hunks[1].end_line, 12);
}

#[test]
fn preview_patch_stream_emits_one_item_per_operation() {
    let payload = serde_json::json!({
        "operations": [
            {
                "kind": "search_replace",
                "path": "src/lib.rs",
                "search": "old_fn",
                "replace": "new_fn",
            },
            {
                "kind": "create_file",
                "path": "src/new.rs",
                "contents": "pub fn hello() {}\n",
            },
            {
                "kind": "delete_file",
                "path": "src/dead.rs",
            },
            {
                "kind": "move_file",
                "from": "src/old.rs",
                "to": "src/new_name.rs",
            },
        ],
    })
    .to_string();

    let mut emitted: Vec<PatchOpPreview> = Vec::new();
    let count =
        preview_patch_stream(&payload, |preview| emitted.push(preview)).expect("preview stream");

    assert_eq!(count, 4, "stream item count must match op count");
    assert_eq!(emitted.len(), 4);
    assert_eq!(emitted[0].kind, PatchOpKind::SearchReplace);
    assert_eq!(emitted[0].path, "src/lib.rs");
    assert_eq!(emitted[0].index, 0);
    assert_eq!(
        emitted[0].search_hash.as_deref(),
        Some(sha256_hex(b"old_fn").as_str())
    );
    assert_eq!(
        emitted[0].replace_hash.as_deref(),
        Some(sha256_hex(b"new_fn").as_str())
    );
    assert_eq!(emitted[1].kind, PatchOpKind::CreateFile);
    assert_eq!(emitted[1].path, "src/new.rs");
    assert_eq!(
        emitted[1].contents_hash.as_deref(),
        Some(sha256_hex(b"pub fn hello() {}\n").as_str())
    );
    assert_eq!(emitted[2].kind, PatchOpKind::DeleteFile);
    assert_eq!(emitted[2].path, "src/dead.rs");
    assert_eq!(emitted[3].kind, PatchOpKind::MoveFile);
    assert_eq!(emitted[3].path, "src/new_name.rs");
    assert_eq!(emitted[3].from_path.as_deref(), Some("src/old.rs"));
    for (expected_index, preview) in emitted.iter().enumerate() {
        assert_eq!(preview.index, expected_index);
    }
}

#[test]
fn preview_patch_stream_supports_legacy_patches_array() {
    let payload = serde_json::json!({
        "patches": [
            { "path": "a.txt", "search": "x", "replace": "y" },
            { "path": "b.txt", "search": "p", "replace": "q" },
        ],
    })
    .to_string();
    let mut emitted: Vec<PatchOpPreview> = Vec::new();
    let count =
        preview_patch_stream(&payload, |preview| emitted.push(preview)).expect("preview stream");
    assert_eq!(count, 2);
    assert!(
        emitted
            .iter()
            .all(|preview| preview.kind == PatchOpKind::SearchReplace)
    );
    assert_eq!(emitted[0].path, "a.txt");
    assert_eq!(emitted[1].path, "b.txt");
}

#[test]
fn preview_patch_stream_rejects_non_json() {
    let outcome = preview_patch_stream("{not valid", |_| {});
    assert!(outcome.is_err());
}

#[test]
fn parses_numstat_with_binary_counts() {
    let parsed = parse_numstat(b"2\t3\tsrc/lib.rs\0-\t-\timage.png\0");
    assert_eq!(parsed["src/lib.rs"].additions, 2);
    assert_eq!(parsed["src/lib.rs"].deletions, 3);
    assert!(parsed["image.png"].binary);
}

#[test]
fn branch_mode_snapshot_reports_files_changed_since_default_branch() {
    let root = temp_repo("branch_mode");
    init_repo(&root);
    fs::write(root.join("base.txt"), "base\n").expect("write base");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "initial"]);
    git(&root, &["checkout", "-b", "feature"]);
    fs::write(root.join("feature.txt"), "feature\n").expect("write feature");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "feature work"]);

    let vcs = GitVcs::open(&root).expect("open vcs");
    let snapshot = vcs.snapshot(DiffMode::Branch, DiffOptions::default());

    assert_eq!(snapshot.mode, DiffMode::Branch);
    assert_eq!(snapshot.vcs.kind, VcsKind::Git);
    assert_eq!(snapshot.vcs.branch.as_deref(), Some("feature"));
    assert!(
        snapshot
            .vcs
            .default_branch
            .as_deref()
            .is_some_and(|name| name == "main" || name == "master"),
        "expected main or master, got {:?}",
        snapshot.vcs.default_branch
    );
    assert!(snapshot.vcs.merge_base.is_some());

    let paths = snapshot
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect::<Vec<_>>();
    assert_eq!(paths, vec!["feature.txt"]);
    assert_eq!(snapshot.files[0].status, DiffFileStatus::Added);
    assert_eq!(snapshot.summary.files_changed, 1);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn branch_base_mode_matches_branch_merge_base_diff() {
    let root = temp_repo("branch_base_mode");
    init_repo(&root);
    fs::write(root.join("base.txt"), "base\n").expect("write base");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "initial"]);
    git(&root, &["checkout", "-b", "feature"]);
    fs::write(root.join("feature.txt"), "feature\n").expect("write feature");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "feature work"]);

    let vcs = GitVcs::open(&root).expect("open vcs");
    let snapshot = vcs.snapshot(DiffMode::BranchBase, DiffOptions::default());

    assert_eq!(snapshot.mode, DiffMode::BranchBase);
    assert!(snapshot.vcs.merge_base.is_some());
    assert_eq!(
        snapshot
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        vec!["feature.txt"]
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn index_mode_reports_only_staged_changes() {
    let root = temp_repo("index_mode");
    init_repo(&root);
    fs::write(root.join("staged.txt"), "before\n").expect("write staged");
    fs::write(root.join("unstaged.txt"), "before\n").expect("write unstaged");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "initial"]);
    fs::write(root.join("staged.txt"), "after\n").expect("modify staged");
    git(&root, &["add", "staged.txt"]);
    fs::write(root.join("unstaged.txt"), "after\n").expect("modify unstaged");

    let vcs = GitVcs::open(&root).expect("open vcs");
    let snapshot = vcs.snapshot(DiffMode::Index, DiffOptions::default());

    assert_eq!(snapshot.mode, DiffMode::Index);
    assert_eq!(
        snapshot
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        vec!["staged.txt"]
    );
    assert_eq!(snapshot.summary.files_changed, 1);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn checkpoint_rollback_restores_modified_added_and_deleted_files() {
    let root = temp_repo("checkpoint_restore");
    fs::write(root.join("a.txt"), "A\n").expect("write a");
    fs::write(root.join("b.txt"), "B\n").expect("write b");
    let store = CheckpointStore::open(&root).expect("checkpoint store");
    let before = store.track_tree().expect("track before");

    fs::write(root.join("a.txt"), "A2\n").expect("modify a");
    fs::write(root.join("c.txt"), "C\n").expect("write c");
    fs::remove_file(root.join("b.txt")).expect("remove b");
    let record = store
        .create_checkpoint(&before, "shell", "call", "turn-1", "success", Vec::new())
        .expect("create checkpoint")
        .expect("checkpoint");

    assert_eq!(record.summary.files_changed, 3);
    let rollback = store
        .rollback(RollbackTarget::Latest, RollbackMode::BestEffort)
        .expect("rollback latest");

    assert!(rollback.conflicts.is_empty());
    assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "A\n");
    assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "B\n");
    assert!(!root.join("c.txt").exists());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn checkpoint_tracking_skips_ignored_large_files_without_failing() {
    let root = temp_repo("checkpoint_ignored_large_files");
    fs::write(root.join(".gitignore"), "target\n").expect("write gitignore");
    fs::create_dir(root.join("target")).expect("create target");
    let large = fs::File::create(root.join("target").join("large.bin")).expect("create large");
    large
        .set_len(DEFAULT_MAX_CHECKPOINT_FILE_BYTES + 1)
        .expect("size large");
    fs::write(root.join("src.rs"), "fn before() {}\n").expect("write src");

    let store = CheckpointStore::open(&root).expect("checkpoint store");
    let before = store.track_tree().expect("track before");
    assert!(
        before.large_files.is_empty(),
        "ignored large files must not become explicit checkpoint pathspecs"
    );

    fs::write(root.join("src.rs"), "fn after() {}\n").expect("modify src");
    let record = store
        .create_checkpoint(&before, "shell", "call", "turn-1", "success", Vec::new())
        .expect("create checkpoint")
        .expect("checkpoint");

    assert_eq!(record.summary.files_changed, 1);
    assert_eq!(record.files[0].path, "src.rs");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn checkpoint_rollback_reports_conflicts_without_overwriting_user_changes() {
    let root = temp_repo("checkpoint_conflict");
    fs::write(root.join("a.txt"), "A\n").expect("write a");
    let store = CheckpointStore::open(&root).expect("checkpoint store");
    let before = store.track_tree().expect("track before");

    fs::write(root.join("a.txt"), "agent\n").expect("agent edit");
    store
        .create_checkpoint(
            &before,
            "write_file",
            "call",
            "turn-1",
            "success",
            Vec::new(),
        )
        .expect("create checkpoint")
        .expect("checkpoint");
    fs::write(root.join("a.txt"), "user\n").expect("user edit");

    let rollback = store
        .rollback(RollbackTarget::Latest, RollbackMode::BestEffort)
        .expect("rollback latest");

    assert_eq!(rollback.conflicts.len(), 1);
    assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "user\n");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn atomic_rollback_leaves_all_files_unchanged_when_any_file_conflicts() {
    let root = temp_repo("checkpoint_atomic");
    fs::write(root.join("a.txt"), "A\n").expect("write a");
    fs::write(root.join("b.txt"), "B\n").expect("write b");
    let store = CheckpointStore::open(&root).expect("checkpoint store");
    let before = store.track_tree().expect("track before");

    fs::write(root.join("a.txt"), "agent-a\n").expect("agent edit a");
    fs::write(root.join("b.txt"), "agent-b\n").expect("agent edit b");
    store
        .create_checkpoint(
            &before,
            "write_file",
            "call",
            "turn-1",
            "success",
            Vec::new(),
        )
        .expect("create checkpoint")
        .expect("checkpoint");
    fs::write(root.join("a.txt"), "user-a\n").expect("user edit a");

    let rollback = store
        .rollback(RollbackTarget::Latest, RollbackMode::Atomic)
        .expect("rollback latest");

    assert_eq!(rollback.mode, RollbackMode::Atomic);
    assert!(!rollback.applied);
    assert_eq!(rollback.conflicts.len(), 1);
    assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "user-a\n");
    assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "agent-b\n");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn best_effort_rollback_restores_clean_files_and_skips_conflicts() {
    let root = temp_repo("checkpoint_best_effort");
    fs::write(root.join("a.txt"), "A\n").expect("write a");
    fs::write(root.join("b.txt"), "B\n").expect("write b");
    let store = CheckpointStore::open(&root).expect("checkpoint store");
    let before = store.track_tree().expect("track before");

    fs::write(root.join("a.txt"), "agent-a\n").expect("agent edit a");
    fs::write(root.join("b.txt"), "agent-b\n").expect("agent edit b");
    store
        .create_checkpoint(
            &before,
            "write_file",
            "call",
            "turn-1",
            "success",
            Vec::new(),
        )
        .expect("create checkpoint")
        .expect("checkpoint");
    fs::write(root.join("a.txt"), "user-a\n").expect("user edit a");

    let rollback = store
        .rollback(RollbackTarget::Latest, RollbackMode::BestEffort)
        .expect("rollback latest");

    assert_eq!(rollback.mode, RollbackMode::BestEffort);
    assert!(rollback.applied);
    assert_eq!(rollback.conflicts.len(), 1);
    assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "user-a\n");
    assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "B\n");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn group_atomic_rollback_preflights_reverse_checkpoint_order() {
    let root = temp_repo("checkpoint_group_atomic");
    fs::write(root.join("sample.txt"), "one").expect("write sample");
    let store = CheckpointStore::open(&root).expect("checkpoint store");

    let before = store.track_tree().expect("track one");
    fs::write(root.join("sample.txt"), "two").expect("write two");
    store
        .create_checkpoint(
            &before,
            "write_file",
            "call-1",
            "turn-1",
            "success",
            Vec::new(),
        )
        .expect("create checkpoint")
        .expect("checkpoint one");

    let before = store.track_tree().expect("track two");
    fs::write(root.join("sample.txt"), "three").expect("write three");
    store
        .create_checkpoint(
            &before,
            "write_file",
            "call-2",
            "turn-1",
            "success",
            Vec::new(),
        )
        .expect("create checkpoint")
        .expect("checkpoint two");

    let rollback = store
        .rollback(RollbackTarget::Group("turn-1"), RollbackMode::Atomic)
        .expect("rollback group");

    assert!(rollback.applied);
    assert!(rollback.conflicts.is_empty());
    assert_eq!(fs::read_to_string(root.join("sample.txt")).unwrap(), "one");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn large_files_are_reported_as_skipped_and_not_restored() {
    let root = temp_repo("checkpoint_large");
    let store = CheckpointStore::open(&root).expect("checkpoint store");
    let before = store.track_tree().expect("track before");
    fs::write(
        root.join("huge.bin"),
        vec![b'x'; DEFAULT_MAX_CHECKPOINT_FILE_BYTES as usize + 1],
    )
    .expect("write huge");

    let record = store
        .create_checkpoint(&before, "shell", "call", "turn-1", "success", Vec::new())
        .expect("create checkpoint")
        .expect("checkpoint");

    assert!(record.files.is_empty());
    assert_eq!(record.skipped_files.len(), 1);
    assert!(!record.coverage_warnings.is_empty());
    fs::write(root.join("huge.bin"), b"user").expect("user edit huge");
    let rollback = store
        .rollback(RollbackTarget::Latest, RollbackMode::Atomic)
        .expect("rollback latest");
    assert!(!rollback.applied);
    assert_eq!(fs::read(root.join("huge.bin")).unwrap(), b"user");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn binary_files_restore_without_patch_text() {
    let root = temp_repo("checkpoint_binary");
    fs::write(root.join("image.bin"), [0, 159, 146, 150]).expect("write binary");
    let store = CheckpointStore::open(&root).expect("checkpoint store");
    let before = store.track_tree().expect("track before");
    fs::write(root.join("image.bin"), [1, 2, 3, 4]).expect("modify binary");

    let record = store
        .create_checkpoint(
            &before,
            "write_file",
            "call",
            "turn-1",
            "success",
            Vec::new(),
        )
        .expect("create checkpoint")
        .expect("checkpoint");
    assert!(record.files[0].binary);
    assert!(record.files[0].patch.is_none());

    let rollback = store
        .rollback(RollbackTarget::Latest, RollbackMode::Atomic)
        .expect("rollback latest");
    assert!(rollback.applied);
    assert_eq!(
        fs::read(root.join("image.bin")).unwrap(),
        [0, 159, 146, 150]
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn noop_tool_with_preexisting_large_file_does_not_create_a_checkpoint() {
    let root = temp_repo("checkpoint_noop_phantom");
    fs::write(
        root.join("huge.bin"),
        vec![b'x'; DEFAULT_MAX_CHECKPOINT_FILE_BYTES as usize + 1],
    )
    .expect("write huge");
    let store = CheckpointStore::open(&root).expect("checkpoint store");
    let before = store.track_tree().expect("track before");

    let record = store
        .create_checkpoint(&before, "shell", "noop", "turn-noop", "success", Vec::new())
        .expect("create checkpoint");

    assert!(
        record.is_none(),
        "tool did not change anything; expected no checkpoint, got {record:?}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn changed_large_file_is_reported_but_unchanged_one_is_not() {
    let root = temp_repo("checkpoint_changed_large_only");
    fs::write(
        root.join("static.bin"),
        vec![b'x'; DEFAULT_MAX_CHECKPOINT_FILE_BYTES as usize + 1],
    )
    .expect("write static");
    let store = CheckpointStore::open(&root).expect("checkpoint store");
    let before = store.track_tree().expect("track before");
    fs::write(root.join("small.txt"), "after\n").expect("write small");

    let record = store
        .create_checkpoint(
            &before,
            "shell",
            "call",
            "turn-only-small",
            "success",
            Vec::new(),
        )
        .expect("create checkpoint")
        .expect("checkpoint");

    let skipped_paths: Vec<&str> = record
        .skipped_files
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    assert!(
        skipped_paths.is_empty(),
        "static large file should not be reported as skipped: {skipped_paths:?}"
    );
    assert!(record.coverage_warnings.is_empty());
    assert_eq!(
        record
            .files
            .iter()
            .map(|f| f.path.as_str())
            .collect::<Vec<_>>(),
        vec!["small.txt"]
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn bulk_patch_splitter_groups_each_file_section() {
    let raw = b"diff --git a/src/foo.rs b/src/foo.rs\n\
index 1111111..2222222 100644\n\
--- a/src/foo.rs\n\
+++ b/src/foo.rs\n\
@@ -1,1 +1,1 @@\n\
-old\n\
+new\n\
diff --git a/src/bar.rs b/src/bar.rs\n\
index 3333333..4444444 100644\n\
--- a/src/bar.rs\n\
+++ b/src/bar.rs\n\
@@ -1,1 +1,1 @@\n\
-a\n\
+b\n";
    let files = vec!["src/foo.rs".to_string(), "src/bar.rs".to_string()];
    let map = split_unified_patch(raw, &files, 4096);
    assert_eq!(map.len(), 2);
    let foo = map.get("src/foo.rs").expect("foo patch");
    assert!(foo.text.contains("diff --git a/src/foo.rs"));
    assert!(foo.text.contains("+new"));
    assert!(!foo.text.contains("src/bar.rs"));
    let bar = map.get("src/bar.rs").expect("bar patch");
    assert!(bar.text.contains("diff --git a/src/bar.rs"));
    assert!(bar.text.contains("+b"));
}

#[test]
fn bulk_patch_splitter_handles_paths_with_spaces() {
    let raw = b"diff --git a/my file b/my file\n\
index 0..1 100644\n\
--- a/my file\n\
+++ b/my file\n\
@@ -1 +1 @@\n\
-x\n\
+y\n";
    let files = vec!["my file".to_string()];
    let map = split_unified_patch(raw, &files, 4096);
    assert_eq!(map.len(), 1);
    assert!(
        map.get("my file")
            .expect("spaced patch")
            .text
            .contains("+y")
    );
}

#[test]
fn checkpoint_ids_are_unique_under_rapid_creation() {
    use std::collections::HashSet;
    let mut seen = HashSet::new();
    for _ in 0..2000 {
        let id = checkpoint_id();
        assert!(seen.insert(id.clone()), "duplicate checkpoint id: {id}");
    }
}

#[test]
fn malformed_journal_lines_are_counted_and_ignored() {
    let root = temp_repo("checkpoint_journal");
    let store = CheckpointStore::open(&root).expect("checkpoint store");
    fs::create_dir_all(root.join(".squeezy/checkpoints")).expect("mkdir checkpoints");
    fs::write(store.journal_path.clone(), "{bad json\n").expect("write malformed journal");

    let journal = store.read_journal().expect("read journal");

    assert_eq!(journal.checkpoints.len(), 0);
    assert_eq!(journal.journal_warnings, 1);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn old_checkpoints_are_pruned_from_the_journal() {
    let root = temp_repo("checkpoint_retention");
    fs::write(root.join("sample.txt"), "before").expect("write sample");
    let store = CheckpointStore::open(&root).expect("checkpoint store");
    let before = store.track_tree().expect("track before");
    fs::write(root.join("sample.txt"), "after").expect("write after");
    let mut record = store
        .create_checkpoint(
            &before,
            "write_file",
            "call",
            "turn-1",
            "success",
            Vec::new(),
        )
        .expect("create checkpoint")
        .expect("checkpoint");
    record.created_at_ms = 1;
    store
        .rewrite_checkpoint_journal(std::slice::from_ref(&record))
        .expect("rewrite old journal");

    store.cleanup_old_checkpoints(7).expect("cleanup");

    assert!(store.list_checkpoints().expect("list").is_empty());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn checkpoint_records_rename_as_single_renamed_entry() {
    let root = temp_repo("checkpoint_rename");
    fs::write(root.join("alpha.txt"), "alpha contents\n").expect("seed alpha");
    let store = CheckpointStore::open(&root).expect("checkpoint store");
    let before = store.track_tree().expect("track before");

    fs::rename(root.join("alpha.txt"), root.join("beta.txt")).expect("rename");
    let record = store
        .create_checkpoint(
            &before,
            "apply_patch",
            "call",
            "turn-1",
            "success",
            Vec::new(),
        )
        .expect("create checkpoint")
        .expect("checkpoint");

    assert_eq!(
        record.files.len(),
        1,
        "rename should collapse to a single entry, got {:?}",
        record.files
    );
    let entry = &record.files[0];
    assert_eq!(entry.status, DiffFileStatus::Renamed);
    assert_eq!(entry.path, "beta.txt");
    assert_eq!(entry.from_path.as_deref(), Some("alpha.txt"));
    assert!(entry.before_sha256.is_some());
    assert!(entry.after_sha256.is_some());

    let rollback = store
        .rollback(RollbackTarget::Latest, RollbackMode::BestEffort)
        .expect("rollback");
    assert!(rollback.applied);
    assert!(!root.join("beta.txt").exists());
    assert_eq!(
        fs::read_to_string(root.join("alpha.txt")).expect("alpha restored"),
        "alpha contents\n"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn shadow_repo_ignores_user_hooks() {
    let root = temp_repo("shadow_repo_hooks");
    init_repo(&root);
    fs::write(root.join("seed.txt"), "seed\n").expect("write seed");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-m", "seed"]);

    let hooks_dir = root.join("user-hooks");
    fs::create_dir_all(&hooks_dir).expect("create hooks dir");
    let marker_path = root.join(".user-hook-marker");
    let hook_script = format!("#!/bin/sh\ntouch '{}'\n", marker_path.to_string_lossy());
    for name in ["post-index-change", "pre-commit", "post-commit"] {
        let hook = hooks_dir.join(name);
        fs::write(&hook, &hook_script).expect("write hook");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&hook).expect("hook perms").permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&hook, perms).expect("chmod hook");
        }
    }
    git(
        &root,
        &[
            "config",
            "core.hooksPath",
            hooks_dir.to_string_lossy().as_ref(),
        ],
    );

    let store = CheckpointStore::open(&root).expect("checkpoint store");
    fs::write(root.join("seed.txt"), "updated\n").expect("modify seed");
    let before = store.track_tree().expect("track before");
    fs::write(root.join("seed.txt"), "updated again\n").expect("modify seed again");
    let _ = store
        .create_checkpoint(&before, "shell", "call", "turn-1", "success", Vec::new())
        .expect("create checkpoint");

    assert!(
        !marker_path.exists(),
        "shadow-repo writes must not invoke user hooks at {hooks_dir:?}"
    );

    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn shadow_repo_lock_is_created_on_open_and_removed_on_close() {
    let root = temp_repo("shadow_repo_lock_lifecycle");
    fs::write(root.join("seed.txt"), "seed\n").expect("write seed");
    let lock_path = root
        .join(".squeezy")
        .join("checkpoints")
        .join(SHADOW_LOCK_FILENAME);

    let store = CheckpointStore::open(&root).expect("checkpoint store");
    assert!(
        lock_path.exists(),
        "shadow-repo lock file must exist while the store is open at {lock_path:?}"
    );
    let lock_body = fs::read_to_string(&lock_path).expect("read lock");
    let recorded_pid: u32 = lock_body
        .lines()
        .next()
        .and_then(|line| line.parse().ok())
        .expect("lock file must record our pid on the first line");
    assert_eq!(recorded_pid, std::process::id());

    drop(store);
    assert!(
        !lock_path.exists(),
        "shadow-repo lock file must be removed when the store is dropped"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn shadow_repo_open_rejects_concurrent_process_lock() {
    let root = temp_repo("shadow_repo_concurrent_lock");
    fs::write(root.join("seed.txt"), "seed\n").expect("write seed");

    let first = CheckpointStore::open(&root).expect("first checkpoint store");
    let err = CheckpointStore::open(&root)
        .expect_err("second open must fail while the first store holds the lock");
    let message = format!("{err}");
    assert!(
        message.contains("shadow-repo lock"),
        "expected lock-held error, got: {message}"
    );

    drop(first);
    let second = CheckpointStore::open(&root).expect("re-open after lock released");
    drop(second);

    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn shadow_repo_open_cleans_stale_orphan_dirs() {
    let root = temp_repo("shadow_repo_stale_cleanup");
    fs::write(root.join("seed.txt"), "seed\n").expect("write seed");
    // Pre-create `.squeezy/checkpoints/` so we can plant a stale entry
    // before the store ever opens.
    let checkpoints_dir = root.join(".squeezy").join("checkpoints");
    fs::create_dir_all(&checkpoints_dir).expect("pre-create checkpoints dir");
    let stale_dir = checkpoints_dir.join("orphan-scratch");
    fs::create_dir_all(&stale_dir).expect("create orphan dir");
    fs::write(stale_dir.join("payload"), b"old").expect("write orphan payload");
    let stale_lock = checkpoints_dir.join("crashed-process.lock");
    fs::write(&stale_lock, b"99999\n").expect("write stale lock");
    let recent_dir = checkpoints_dir.join("recent-scratch");
    fs::create_dir_all(&recent_dir).expect("create recent orphan dir");

    let stale_at = SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(
            (SHADOW_STALE_DIR_RETENTION_DAYS + 1) * 24 * 60 * 60,
        ))
        .expect("stale instant");
    fs::File::open(&stale_dir)
        .expect("open stale dir")
        .set_modified(stale_at)
        .expect("backdate stale dir mtime");
    fs::File::options()
        .write(true)
        .open(&stale_lock)
        .expect("open stale lock for mtime")
        .set_modified(stale_at)
        .expect("backdate stale lock mtime");

    let store = CheckpointStore::open(&root).expect("checkpoint store");
    assert!(
        !stale_dir.exists(),
        "stale orphan directory must be removed on open"
    );
    assert!(
        !stale_lock.exists(),
        "stale lock file must be removed on open"
    );
    assert!(
        recent_dir.exists(),
        "recent orphan directory must not be removed on open"
    );
    assert!(
        checkpoints_dir.join("git").exists(),
        "shadow `git/` directory must survive cleanup"
    );
    drop(store);

    let _ = fs::remove_dir_all(root);
}

fn temp_repo(name: &str) -> PathBuf {
    let base = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let counter = VCS_NONCE.fetch_add(1, Ordering::SeqCst);
    let root = std::env::temp_dir().join(format!(
        "squeezy_vcs_{name}_{pid}_{base}_{counter}",
        pid = std::process::id()
    ));
    fs::create_dir_all(&root).expect("create temp repo");
    root
}

fn init_repo(root: &Path) {
    git(root, &["init", "--initial-branch=main"]);
    git(root, &["config", "user.email", "test@example.com"]);
    git(root, &["config", "user.name", "Squeezy Test"]);
    git(root, &["config", "commit.gpgsign", "false"]);
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}
