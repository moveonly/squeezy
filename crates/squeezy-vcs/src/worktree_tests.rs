#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::process::Command;
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(unix)]
use std::time::{SystemTime, UNIX_EPOCH};

use super::*;

// The helpers below are only referenced by `#[cfg(unix)]` worktree
// tests (git-worktree exercises require POSIX permissions / fork
// semantics that don't translate cleanly to Windows). Gate the
// helpers the same way so Windows builds (which exclude those tests)
// don't trip `-D warnings` on `dead_code`.
#[cfg(unix)]
static WORKTREE_NONCE: AtomicU64 = AtomicU64::new(0);

#[cfg(unix)]
fn temp_repo() -> std::path::PathBuf {
    let nonce = WORKTREE_NONCE.fetch_add(1, Ordering::Relaxed);
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0);
    let root = std::env::temp_dir().join(format!("squeezy-vcs-worktree-{micros}-{nonce}"));
    fs::create_dir_all(&root).expect("create temp repo dir");
    run_git(&root, &["init", "--initial-branch=main"]);
    run_git(&root, &["config", "user.email", "test@example.com"]);
    run_git(&root, &["config", "user.name", "Test"]);
    fs::write(root.join("README.md"), "seed\n").expect("write seed");
    run_git(&root, &["add", "README.md"]);
    run_git(&root, &["commit", "-m", "init"]);
    root
}

#[cfg(unix)]
fn run_git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git available");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn validate_worktree_slug_accepts_simple_names() {
    assert!(validate_worktree_slug("feature-foo").is_ok());
    assert!(validate_worktree_slug("nested/feature_bar").is_ok());
    assert!(validate_worktree_slug("v1.2.3-rc.4").is_ok());
}

#[test]
fn validate_worktree_slug_rejects_path_traversal_and_punctuation() {
    assert!(validate_worktree_slug("../escape").is_err());
    assert!(validate_worktree_slug("a/../b").is_err());
    assert!(validate_worktree_slug("/abs").is_err());
    assert!(validate_worktree_slug("ok/").is_err());
    assert!(validate_worktree_slug("has space").is_err());
    assert!(validate_worktree_slug("ampersand&").is_err());
    assert!(validate_worktree_slug(&"a".repeat(65)).is_err());
    assert!(validate_worktree_slug("").is_err());
}

#[cfg(unix)]
#[test]
fn create_then_remove_clean_worktree() {
    let repo = temp_repo();
    let wt = create(&repo, "feature-x").expect("create worktree");
    assert!(wt.path.exists(), "worktree dir should exist");
    assert_eq!(wt.branch, "squeezy/feature-x");
    assert_eq!(wt.base_commit, wt.head_commit);
    let (dirty, ahead) = wt.count_changes().expect("status available");
    assert_eq!(dirty, 0);
    assert_eq!(ahead, 0);
    wt.cleanup(WorktreeCleanup::Remove {
        discard_changes: false,
    })
    .expect("remove clean worktree");
    assert!(!wt.path.exists(), "worktree dir should be gone");
}

#[cfg(unix)]
#[test]
fn cleanup_refuses_dirty_worktree_without_discard_changes() {
    let repo = temp_repo();
    let wt = create(&repo, "dirty").expect("create worktree");
    fs::write(wt.path.join("scratch.txt"), "uncommitted\n").expect("write scratch");
    let err = wt
        .cleanup(WorktreeCleanup::Remove {
            discard_changes: false,
        })
        .expect_err("dirty worktree refuses removal");
    let msg = format!("{err}");
    assert!(
        msg.contains("uncommitted") || msg.contains("ahead"),
        "{msg}"
    );
    assert!(wt.path.exists(), "worktree should remain after refusal");
    wt.cleanup(WorktreeCleanup::Remove {
        discard_changes: true,
    })
    .expect("force-remove dirty worktree");
}

#[cfg(unix)]
#[test]
fn cleanup_keep_leaves_directory_in_place() {
    let repo = temp_repo();
    let wt = create(&repo, "kept").expect("create worktree");
    wt.cleanup(WorktreeCleanup::Keep).expect("keep is no-op");
    assert!(wt.path.exists(), "Keep must not remove the worktree");
}
