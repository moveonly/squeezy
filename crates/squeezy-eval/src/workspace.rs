use std::path::{Path, PathBuf};
use std::process::Command;

use crate::driver::EvalError;
use crate::scenario::WorkspaceSpec;

/// A workspace resolved to a real directory on disk.
pub struct ProvisionedWorkspace {
    pub path: PathBuf,
    pub source: WorkspaceSource,
    /// When `Some`, the directory will be removed when the workspace is dropped.
    pub cleanup: Option<TempDirGuard>,
}

#[derive(Debug, Clone)]
pub enum WorkspaceSource {
    Local(PathBuf),
    Github { repo: String, sha: String },
}

pub struct TempDirGuard {
    pub path: PathBuf,
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        // Best-effort cleanup. Eval runs leave artifacts; the workspace
        // itself is fresh per-run so removing it is safe.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

pub fn provision(
    spec: &WorkspaceSpec,
    scratch_root: &Path,
) -> Result<ProvisionedWorkspace, EvalError> {
    match spec {
        WorkspaceSpec::Local { path } => {
            if !path.exists() {
                return Err(EvalError::Workspace(format!(
                    "local workspace path does not exist: {}",
                    path.display()
                )));
            }
            Ok(ProvisionedWorkspace {
                path: path.clone(),
                source: WorkspaceSource::Local(path.clone()),
                cleanup: None,
            })
        }
        WorkspaceSpec::Github { github } => {
            std::fs::create_dir_all(scratch_root)
                .map_err(|err| EvalError::Io(format!("create_dir_all {scratch_root:?}: {err}")))?;
            let slug = sanitize(&github.repo);
            let target = scratch_root.join(format!(
                "{}-{}",
                slug,
                &github.sha[..short_sha_len(&github.sha)]
            ));
            if !target.exists() {
                let url = format!("https://github.com/{}.git", github.repo);
                run_git(&[
                    "clone",
                    "--no-checkout",
                    &url,
                    target.to_string_lossy().as_ref(),
                ])?;
                run_git_in(&target, &["fetch", "--depth", "1", "origin", &github.sha])?;
                run_git_in(&target, &["checkout", &github.sha])?;
            }
            Ok(ProvisionedWorkspace {
                path: target.clone(),
                source: WorkspaceSource::Github {
                    repo: github.repo.clone(),
                    sha: github.sha.clone(),
                },
                cleanup: Some(TempDirGuard { path: target }),
            })
        }
    }
}

fn sanitize(repo: &str) -> String {
    repo.chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect()
}

fn short_sha_len(sha: &str) -> usize {
    sha.len().min(12)
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
