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
