use std::collections::HashSet;
use std::fs;
use std::sync::Arc;
use std::sync::Barrier;
use std::thread;

use super::*;

fn tempdir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "squeezy-eval-workspace-{}-{}-{}",
        label,
        std::process::id(),
        RUN_SEQ.fetch_add(1, Ordering::Relaxed),
    ));
    fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[test]
fn github_scratch_dir_is_unique_per_call() {
    // Same (repo, sha) produces a different path on every call so two
    // concurrent runs never share a workspace directory.
    let root = tempdir("unique");
    let repo = "octocat/Hello-World";
    let sha = "deadbeefcafef00d";

    let a = github_scratch_dir(&root, repo, sha);
    let b = github_scratch_dir(&root, repo, sha);
    assert_ne!(
        a, b,
        "successive provisions must not collide on the same path"
    );
    assert!(a.starts_with(&root));
    assert!(b.starts_with(&root));

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn github_scratch_dir_unique_under_concurrency() {
    // Spawn many threads racing on the same (repo, sha) and verify every
    // generated path is distinct. The pre-fix code would have produced
    // a single shared path here.
    let root = tempdir("race");
    let repo = "octocat/Hello-World";
    let sha = "deadbeefcafef00d";

    let n = 16;
    let barrier = Arc::new(Barrier::new(n));
    let root = Arc::new(root);
    let handles: Vec<_> = (0..n)
        .map(|_| {
            let barrier = Arc::clone(&barrier);
            let root = Arc::clone(&root);
            thread::spawn(move || {
                barrier.wait();
                github_scratch_dir(&root, repo, sha)
            })
        })
        .collect();
    let paths: Vec<PathBuf> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let unique: HashSet<_> = paths.iter().collect();
    assert_eq!(
        unique.len(),
        paths.len(),
        "concurrent provisions collided on a shared path"
    );

    let _ = fs::remove_dir_all(&*root);
}

#[test]
fn concurrent_provisioned_workspaces_isolated_under_drop() {
    // End-to-end shape of the original bug: two "provisioned" GitHub
    // workspaces with the same repo+SHA must get isolated directories,
    // so dropping one does not yank files out from under the other.
    //
    // The test fabricates two `ProvisionedWorkspace` values pointed at
    // distinct paths produced by `github_scratch_dir`, writes a file
    // into each, drops one, and then verifies the other's file is
    // still readable. This simulates the post-fix contract without
    // hitting the network for a real `git clone`.
    let scratch_root = tempdir("isolation");
    let repo = "torvalds/linux";
    let sha = "0123456789abcdef";

    let path_a = github_scratch_dir(&scratch_root, repo, sha);
    let path_b = github_scratch_dir(&scratch_root, repo, sha);
    assert_ne!(path_a, path_b);
    fs::create_dir_all(&path_a).unwrap();
    fs::create_dir_all(&path_b).unwrap();
    let file_a = path_a.join("qt_sinks.h");
    let file_b = path_b.join("qt_sinks.h");
    fs::write(&file_a, b"// A").unwrap();
    fs::write(&file_b, b"// B").unwrap();

    let ws_a = ProvisionedWorkspace {
        path: path_a.clone(),
        source: WorkspaceSource::Github {
            repo: repo.into(),
            sha: sha.into(),
        },
        cleanup: Some(WorkspaceCleanup::Directory {
            path: path_a.clone(),
        }),
    };
    let ws_b = ProvisionedWorkspace {
        path: path_b.clone(),
        source: WorkspaceSource::Github {
            repo: repo.into(),
            sha: sha.into(),
        },
        cleanup: Some(WorkspaceCleanup::Directory {
            path: path_b.clone(),
        }),
    };

    // Simulate run A finishing first and triggering Drop.
    drop(ws_a);
    assert!(!file_a.exists(), "ws_a directory should be cleaned up");
    let read_b = fs::read(ws_b.path.join("qt_sinks.h"))
        .expect("ws_b file must still be readable after ws_a is dropped");
    assert_eq!(read_b, b"// B");

    drop(ws_b);
    let _ = fs::remove_dir_all(&scratch_root);
}
