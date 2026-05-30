//! Git worktree create/cleanup primitives for isolated agent sessions.
//!
//! Covers slug validation, `git worktree add`, and a fail-closed cleanup
//! path that refuses to remove worktrees with uncommitted changes or
//! ahead-of-base commits unless the caller explicitly opts in via
//! `discard_changes`.
//!
//! The TUI/CLI binding (`/worktree enter` / `/worktree exit`) lives in
//! `squeezy-tui`; this crate intentionally exposes no model-callable
//! tool surface — worktree creation stays a user gesture in the first
//! batch so a runaway agent can't pivot the user out of their current
//! checkout.
//!
//! `AGENTS.md` discovery in `squeezy-agent` walks from `workspace_root`
//! downward, so pointing the agent at a fresh worktree path already gives
//! it an isolated memory-file view; no separate memdir layer is needed.

use std::path::{Path, PathBuf};
use std::process::Command;

use squeezy_core::{Result, SqueezyError};

const MAX_WORKTREE_SLUG_LENGTH: usize = 64;

/// Cleanup action for [`Worktree::cleanup`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeCleanup {
    /// Detach the session from the worktree but leave the directory and
    /// branch in place — the user can keep iterating there manually.
    Keep,
    /// Run `git worktree remove`. Refuses when the worktree has uncommitted
    /// changes or commits ahead of `base_commit` unless `discard_changes`
    /// is `true`.
    Remove { discard_changes: bool },
}

/// Handle to a worktree this session created (or adopted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
    pub base_commit: String,
    pub head_commit: String,
}

/// Validate a worktree slug: each `/`-separated segment must be
/// non-empty and contain only letters, digits, `.`, `_`, `-`. Total
/// length is capped at 64. Rejects `.` / `..` segments so a slug can
/// never escape the `worktrees/` parent via `path::join`.
pub fn validate_worktree_slug(slug: &str) -> Result<()> {
    if slug.len() > MAX_WORKTREE_SLUG_LENGTH {
        return Err(SqueezyError::Tool(format!(
            "invalid worktree name: must be {MAX_WORKTREE_SLUG_LENGTH} characters or fewer (got {})",
            slug.len()
        )));
    }
    for segment in slug.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(SqueezyError::Tool(format!(
                "invalid worktree name {slug:?}: must not contain empty, \".\", or \"..\" segments",
            )));
        }
        if !segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        {
            return Err(SqueezyError::Tool(format!(
                "invalid worktree name {slug:?}: each \"/\"-separated segment must contain only letters, digits, dots, underscores, and dashes",
            )));
        }
    }
    Ok(())
}

/// Create a new worktree at `<repo_root>/.worktrees/<slug>` on a new
/// branch `squeezy/<slug>`. Returns the captured `base_commit` (current
/// HEAD of `repo_root`) and `head_commit` (HEAD of the new worktree),
/// which match at creation time; `cleanup` uses `base_commit` to detect
/// drift later.
pub fn create(repo_root: &Path, slug: &str) -> Result<Worktree> {
    validate_worktree_slug(slug)?;
    let toplevel = git_text(repo_root, &["rev-parse", "--show-toplevel"])
        .map_err(|err| SqueezyError::Tool(format!("not a git repository: {err}")))?;
    let toplevel = PathBuf::from(toplevel);
    let path = toplevel.join(".worktrees").join(slug);
    if path.exists() {
        return Err(SqueezyError::Tool(format!(
            "worktree path already exists: {}",
            path.display()
        )));
    }
    let base_commit = git_text(&toplevel, &["rev-parse", "HEAD"]).map_err(SqueezyError::Tool)?;
    let branch = format!("squeezy/{slug}");
    let path_str = path
        .to_str()
        .ok_or_else(|| SqueezyError::Tool("worktree path is not valid UTF-8".to_string()))?;
    git_text(
        &toplevel,
        &["worktree", "add", "-b", &branch, path_str, &base_commit],
    )
    .map_err(|err| SqueezyError::Tool(format!("git worktree add failed: {err}")))?;
    let head_commit = git_text(&path, &["rev-parse", "HEAD"]).map_err(SqueezyError::Tool)?;
    Ok(Worktree {
        path,
        branch,
        base_commit,
        head_commit,
    })
}

impl Worktree {
    /// Count uncommitted entries (porcelain status lines) and commits
    /// ahead of `base_commit`. Returns `None` (fail-closed) when git
    /// status fails — callers should treat that as "refuse to remove".
    pub fn count_changes(&self) -> Option<(usize, usize)> {
        let status = git_text(&self.path, &["status", "--porcelain"]).ok()?;
        let dirty = status.lines().filter(|line| !line.is_empty()).count();
        let ahead = git_text(
            &self.path,
            &[
                "rev-list",
                "--count",
                &format!("{}..HEAD", self.base_commit),
            ],
        )
        .ok()?
        .parse::<usize>()
        .ok()?;
        Some((dirty, ahead))
    }

    /// Finalise the worktree per `action`. `Keep` is always a no-op at
    /// the VCS layer (the caller restores `cwd`). `Remove` runs
    /// `git worktree remove` and refuses on a dirty/ahead worktree
    /// unless `discard_changes` is set, in which case it passes
    /// `--force` to override.
    pub fn cleanup(&self, action: WorktreeCleanup) -> Result<()> {
        match action {
            WorktreeCleanup::Keep => Ok(()),
            WorktreeCleanup::Remove { discard_changes } => {
                if !discard_changes {
                    let (dirty, ahead) = self.count_changes().ok_or_else(|| {
                        SqueezyError::Tool(
                            "refusing to remove worktree: git status unavailable, set discard_changes to override".to_string(),
                        )
                    })?;
                    if dirty > 0 || ahead > 0 {
                        return Err(SqueezyError::Tool(format!(
                            "refusing to remove worktree with {dirty} uncommitted entr{} and {ahead} commit{} ahead of base; set discard_changes to override",
                            if dirty == 1 { "y" } else { "ies" },
                            if ahead == 1 { "" } else { "s" },
                        )));
                    }
                }
                let path_str = self.path.to_str().ok_or_else(|| {
                    SqueezyError::Tool("worktree path is not valid UTF-8".to_string())
                })?;
                // `git worktree remove` must run from a path that resolves
                // to the main repo (or a sibling worktree), not from inside
                // the worktree being removed. Walk up through `.worktrees`
                // to land back at the main repo root.
                let main_repo = self
                    .path
                    .parent()
                    .and_then(|p| p.parent())
                    .unwrap_or(&self.path);
                let mut args = vec!["worktree", "remove"];
                if discard_changes {
                    args.push("--force");
                }
                args.push(path_str);
                git_text(main_repo, &args).map_err(|err| {
                    SqueezyError::Tool(format!("git worktree remove failed: {err}"))
                })?;
                Ok(())
            }
        }
    }
}

/// Minimal `git` invocation that returns trimmed stdout; mirrors
/// `lib.rs::git_text` but stays local so the module is self-contained.
fn git_text(cwd: &Path, args: &[&str]) -> std::result::Result<String, String> {
    let output = Command::new("git")
        .args(["--no-optional-locks", "-c", "core.quotepath=false"])
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|err| format!("git failed to start: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!(
                "git exited with status {}",
                output.status.code().unwrap_or(-1)
            )
        } else {
            stderr
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
#[path = "worktree_tests.rs"]
mod tests;
