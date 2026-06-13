use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use ignore::WalkBuilder;

use crate::driver::EvalError;
use crate::scenario::WorkspaceSpec;

/// Monotonic per-process counter appended to scratch directory names so
/// concurrent provisions never collide on the same path even when the
/// wall clock or pid alone would not separate them.
static RUN_SEQ: AtomicU64 = AtomicU64::new(0);

/// A workspace resolved to a real directory on disk.
pub struct ProvisionedWorkspace {
    pub path: PathBuf,
    pub source: WorkspaceSource,
    /// When `Some`, the directory will be removed when the workspace is dropped.
    pub cleanup: Option<WorkspaceCleanup>,
}

#[derive(Debug, Clone)]
pub enum WorkspaceSource {
    Local(PathBuf),
    /// Per-run snapshot of a local directory, materialized via `git
    /// worktree add` (when the source is a git repo) or an
    /// ignore-respecting tree copy. `sha` is the commit the worktree
    /// points at when available.
    Snapshot {
        from: PathBuf,
        sha: Option<String>,
        worktree: bool,
    },
    Github {
        repo: String,
        sha: String,
    },
}

/// Cleanup guard for a per-run scratch directory.
///
/// Variant determines how to remove it: a `git worktree remove --force`
/// for git-backed snapshots, plain directory removal otherwise.
pub enum WorkspaceCleanup {
    Worktree { source_repo: PathBuf, path: PathBuf },
    Directory { path: PathBuf },
}

impl Drop for WorkspaceCleanup {
    fn drop(&mut self) {
        // Best-effort cleanup. Eval runs leave artifacts; the workspace
        // itself is fresh per-run so removing it is safe.
        match self {
            WorkspaceCleanup::Worktree { source_repo, path } => {
                let _ = Command::new("git")
                    .current_dir(source_repo)
                    .args([
                        "worktree",
                        "remove",
                        "--force",
                        path.to_string_lossy().as_ref(),
                    ])
                    .output();
                // Fall through to directory cleanup in case `git
                // worktree remove` quietly left the directory behind.
                let _ = fs::remove_dir_all(path);
            }
            WorkspaceCleanup::Directory { path } => {
                let _ = fs::remove_dir_all(path);
            }
        }
    }
}

pub fn provision(
    spec: &WorkspaceSpec,
    scratch_root: &Path,
) -> Result<ProvisionedWorkspace, EvalError> {
    match spec {
        WorkspaceSpec::Local {
            path,
            snapshot,
            snapshot_ref,
        } => {
            if !path.exists() {
                return Err(EvalError::Workspace(format!(
                    "local workspace path does not exist: {}",
                    path.display()
                )));
            }
            if *snapshot {
                provision_snapshot(path, scratch_root, snapshot_ref.as_deref())
            } else {
                Ok(ProvisionedWorkspace {
                    path: path.clone(),
                    source: WorkspaceSource::Local(path.clone()),
                    cleanup: None,
                })
            }
        }
        WorkspaceSpec::Github { github } => {
            fs::create_dir_all(scratch_root)
                .map_err(|err| EvalError::Io(format!("create_dir_all {scratch_root:?}: {err}")))?;
            let target = github_scratch_dir(scratch_root, &github.repo, &github.sha);
            let url = format!("https://github.com/{}.git", github.repo);
            run_git(&[
                "clone",
                "--no-checkout",
                &url,
                target.to_string_lossy().as_ref(),
            ])?;
            run_git_in(&target, &["fetch", "--depth", "1", "origin", &github.sha])?;
            run_git_in(&target, &["checkout", &github.sha])?;
            Ok(ProvisionedWorkspace {
                path: target.clone(),
                source: WorkspaceSource::Github {
                    repo: github.repo.clone(),
                    sha: github.sha.clone(),
                },
                cleanup: Some(WorkspaceCleanup::Directory { path: target }),
            })
        }
    }
}

fn provision_snapshot(
    source: &Path,
    scratch_root: &Path,
    snapshot_ref: Option<&str>,
) -> Result<ProvisionedWorkspace, EvalError> {
    fs::create_dir_all(scratch_root)
        .map_err(|err| EvalError::Io(format!("create_dir_all {scratch_root:?}: {err}")))?;
    let slug = sanitize(&source.to_string_lossy());
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let target = scratch_root.join(format!("snap-{slug}-{ts}"));

    let git_dir = source.join(".git");
    let use_worktree = git_dir.exists();
    let snapshot_ref = snapshot_ref.unwrap_or("HEAD");

    if use_worktree {
        // Add a detached worktree at the requested ref. Note: `git worktree
        // add --detach` cannot point at another worktree's HEAD if the same
        // branch is checked out elsewhere, but `--detach` keeps us safe for
        // arbitrary commit-ish.
        let target_str = target.to_string_lossy().into_owned();
        let args: Vec<&str> = vec!["worktree", "add", "--detach", &target_str, snapshot_ref];
        run_git_in(source, &args)?;
        // Resolve the SHA we landed on (best effort).
        let sha = Command::new("git")
            .current_dir(&target)
            .args(["rev-parse", "HEAD"])
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
                } else {
                    None
                }
            });
        Ok(ProvisionedWorkspace {
            path: target.clone(),
            source: WorkspaceSource::Snapshot {
                from: source.to_path_buf(),
                sha,
                worktree: true,
            },
            cleanup: Some(WorkspaceCleanup::Worktree {
                source_repo: source.to_path_buf(),
                path: target,
            }),
        })
    } else {
        copy_tree_ignore_respecting(source, &target)?;
        Ok(ProvisionedWorkspace {
            path: target.clone(),
            source: WorkspaceSource::Snapshot {
                from: source.to_path_buf(),
                sha: None,
                worktree: false,
            },
            cleanup: Some(WorkspaceCleanup::Directory { path: target }),
        })
    }
}

fn copy_tree_ignore_respecting(source: &Path, target: &Path) -> Result<(), EvalError> {
    fs::create_dir_all(target)
        .map_err(|err| EvalError::Io(format!("create_dir_all {target:?}: {err}")))?;
    let walker = WalkBuilder::new(source)
        .hidden(false) // mimic squeezy's own scanning
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build();
    for entry in walker {
        let entry = entry.map_err(|err| EvalError::Workspace(format!("walk {source:?}: {err}")))?;
        let path = entry.path();
        let rel = match path.strip_prefix(source) {
            Ok(rel) => rel,
            Err(_) => continue,
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let dest = target.join(rel);
        if entry.file_type().is_some_and(|t| t.is_dir()) {
            fs::create_dir_all(&dest)
                .map_err(|err| EvalError::Io(format!("create_dir_all {dest:?}: {err}")))?;
        } else if entry.file_type().is_some_and(|t| t.is_file()) {
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)
                    .map_err(|err| EvalError::Io(format!("create_dir_all {parent:?}: {err}")))?;
            }
            fs::copy(path, &dest)
                .map_err(|err| EvalError::Io(format!("copy {path:?} -> {dest:?}: {err}")))?;
        }
    }
    Ok(())
}

/// Compose the per-run scratch directory for a GitHub workspace. The
/// path embeds the repo slug + short SHA for human readability plus a
/// pid + nanosecond timestamp + monotonic counter so concurrent eval
/// runs targeting the same repo+SHA never share a directory. Sharing
/// is unsafe because [`WorkspaceCleanup::Directory::drop`] rmrf's the
/// path, which would yank the workspace out from under any concurrent
/// run still reading files there.
fn github_scratch_dir(scratch_root: &Path, repo: &str, sha: &str) -> PathBuf {
    let slug = sanitize(repo);
    // Truncate on a char boundary, not a byte index: `sha` is untrusted
    // scenario input and slicing `&sha[..12]` would panic when byte 12
    // lands inside a multibyte UTF-8 char. Mirrors `view::short`.
    let short_sha: String = sha.chars().take(12).collect();
    let ns_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = RUN_SEQ.fetch_add(1, Ordering::Relaxed);
    scratch_root.join(format!(
        "{}-{}-{}-{}-{}",
        slug,
        short_sha,
        std::process::id(),
        ns_ts,
        seq,
    ))
}

fn sanitize(repo: &str) -> String {
    repo.chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .chars()
        .take(40)
        .collect()
}

fn run_git(args: &[&str]) -> Result<(), EvalError> {
    let output = Command::new("git")
        .args(args)
        .output()
        .map_err(|err| EvalError::Workspace(format!("spawn git: {err}")))?;
    if !output.status.success() {
        return Err(EvalError::Workspace(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

fn run_git_in(dir: &Path, args: &[&str]) -> Result<(), EvalError> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .map_err(|err| EvalError::Workspace(format!("spawn git in {dir:?}: {err}")))?;
    if !output.status.success() {
        return Err(EvalError::Workspace(format!(
            "git {} (in {}) failed: {}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

#[cfg(test)]
#[path = "workspace_tests.rs"]
mod tests;
