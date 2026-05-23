use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde::{Deserialize, Serialize};
use squeezy_core::{Result, SqueezyError};

pub const CRATE_NAME: &str = "squeezy-vcs";
const DEFAULT_MAX_PATCH_BYTES: usize = 1_000_000;

pub fn crate_name() -> &'static str {
    CRATE_NAME
}

#[derive(Debug, Clone)]
pub struct GitVcs {
    root: PathBuf,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffMode {
    #[default]
    Worktree,
    Branch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffOptions {
    pub include_patch: bool,
    pub max_patch_bytes: usize,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            include_patch: false,
            max_patch_bytes: DEFAULT_MAX_PATCH_BYTES,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffSnapshot {
    pub vcs: VcsInfo,
    pub mode: DiffMode,
    pub summary: DiffSummary,
    pub files: Vec<DiffFile>,
    pub truncated: bool,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VcsInfo {
    pub kind: VcsKind,
    pub root: Option<String>,
    pub git_dir: Option<String>,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub default_branch: Option<String>,
    pub merge_base: Option<String>,
    pub operation_state: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VcsKind {
    Git,
    #[default]
    None,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffSummary {
    pub files_changed: usize,
    pub additions: u64,
    pub deletions: u64,
    pub untracked_files: usize,
    pub binary_files: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffFile {
    pub path: String,
    pub status: DiffFileStatus,
    pub code: String,
    pub additions: u64,
    pub deletions: u64,
    pub binary: bool,
    pub hunks: Vec<DiffHunk>,
    pub patch: Option<String>,
    pub patch_truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffFileStatus {
    Added,
    Deleted,
    Modified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffHunk {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    pub start_line: u32,
    pub end_line: u32,
}

impl GitVcs {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root
            .as_ref()
            .canonicalize()
            .map_err(|err| SqueezyError::Tool(format!("invalid workspace root: {err}")))?;
        Ok(Self { root })
    }

    pub fn snapshot(&self, mode: DiffMode, options: DiffOptions) -> DiffSnapshot {
        let mut errors = Vec::new();
        let Some(git_root) = self.git_root(&mut errors) else {
            return DiffSnapshot {
                vcs: VcsInfo {
                    kind: VcsKind::None,
                    ..VcsInfo::default()
                },
                mode,
                summary: DiffSummary::default(),
                files: Vec::new(),
                truncated: false,
                errors,
            };
        };

        let vcs = self.vcs_info(&git_root, mode, &mut errors);
        let refish = match mode {
            DiffMode::Worktree => vcs.head.as_deref(),
            DiffMode::Branch => vcs.merge_base.as_deref(),
        };

        let mut by_path = BTreeMap::<String, DiffFile>::new();
        for item in self.status_files(&git_root, &mut errors) {
            by_path.insert(item.path.clone(), item);
        }
        if let Some(refish) = refish {
            for item in self.name_status_files(&git_root, refish, &mut errors) {
                by_path.entry(item.path.clone()).or_insert(item);
            }
            for (path, stat) in self.numstat(&git_root, refish, &mut errors) {
                let entry = by_path.entry(path.clone()).or_insert_with(|| DiffFile {
                    path,
                    status: DiffFileStatus::Modified,
                    code: "M".to_string(),
                    additions: 0,
                    deletions: 0,
                    binary: false,
                    hunks: Vec::new(),
                    patch: None,
                    patch_truncated: false,
                });
                entry.additions = stat.additions;
                entry.deletions = stat.deletions;
                entry.binary = stat.binary;
            }
        }

        let mut files = by_path.into_values().collect::<Vec<_>>();
        for file in &mut files {
            if file.status == DiffFileStatus::Added
                && file.code == "??"
                && let Some(stat) = self.numstat_untracked(&git_root, &file.path)
            {
                file.additions = stat.additions;
                file.deletions = stat.deletions;
                file.binary = stat.binary;
            }

            let patch = if file.code == "??" || refish.is_none() {
                self.patch_untracked(&git_root, &file.path, options.max_patch_bytes)
            } else {
                self.patch_file(
                    &git_root,
                    refish.unwrap_or("HEAD"),
                    &file.path,
                    options.max_patch_bytes,
                )
            };
            match patch {
                Some(patch) => {
                    file.patch_truncated = patch.truncated;
                    file.hunks = parse_patch_hunks(&patch.text);
                    if options.include_patch {
                        file.patch = Some(patch.text);
                    }
                }
                None => {
                    if file.status == DiffFileStatus::Added && file.hunks.is_empty() {
                        file.hunks.push(DiffHunk {
                            old_start: 0,
                            old_lines: 0,
                            new_start: 1,
                            new_lines: file.additions.min(u32::MAX as u64) as u32,
                            start_line: 0,
                            end_line: file.additions.saturating_sub(1).min(u32::MAX as u64) as u32,
                        });
                    }
                }
            }
        }

        files.sort_by(|left, right| left.path.cmp(&right.path));
        let mut summary = DiffSummary {
            files_changed: files.len(),
            ..DiffSummary::default()
        };
        let mut truncated = false;
        for file in &files {
            summary.additions += file.additions;
            summary.deletions += file.deletions;
            if file.code == "??" {
                summary.untracked_files += 1;
            }
            if file.binary {
                summary.binary_files += 1;
            }
            truncated |= file.patch_truncated;
        }

        DiffSnapshot {
            vcs,
            mode,
            summary,
            files,
            truncated,
            errors,
        }
    }

    fn vcs_info(&self, git_root: &Path, mode: DiffMode, errors: &mut Vec<String>) -> VcsInfo {
        let git_dir = git_text(git_root, ["rev-parse", "--git-dir"])
            .ok()
            .and_then(|path| normalize_git_dir(git_root, &path));
        let branch = git_text(git_root, ["symbolic-ref", "--quiet", "--short", "HEAD"]).ok();
        let head = git_text(git_root, ["rev-parse", "--verify", "HEAD"]).ok();
        let default_branch = default_branch(git_root);
        let merge_base = if mode == DiffMode::Branch {
            default_branch
                .as_deref()
                .and_then(|base| git_text(git_root, ["merge-base", base, "HEAD"]).ok())
        } else {
            None
        };
        let operation_state = git_dir
            .as_deref()
            .and_then(|path| transient_operation_state(Path::new(path)));
        if mode == DiffMode::Branch && default_branch.is_none() {
            errors.push("default branch could not be determined for branch diff".to_string());
        }
        VcsInfo {
            kind: VcsKind::Git,
            root: Some(git_root.to_string_lossy().to_string()),
            git_dir,
            branch,
            head,
            default_branch,
            merge_base,
            operation_state,
        }
    }

    fn git_root(&self, errors: &mut Vec<String>) -> Option<PathBuf> {
        match git_text(&self.root, ["rev-parse", "--show-toplevel"]) {
            Ok(root) => Some(PathBuf::from(root)),
            Err(err) => {
                errors.push(err);
                None
            }
        }
    }

    fn status_files(&self, git_root: &Path, errors: &mut Vec<String>) -> Vec<DiffFile> {
        let output = match git_output(
            git_root,
            [
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
                "--no-renames",
                "-z",
                "--",
                ".",
            ],
        ) {
            Ok(output) => output,
            Err(err) => {
                errors.push(err);
                return Vec::new();
            }
        };
        nul_fields(&output.stdout)
            .into_iter()
            .filter_map(|item| {
                if item.len() < 4 {
                    return None;
                }
                let code = item.get(..2)?.to_string();
                let path = item.get(3..)?.to_string();
                Some(DiffFile {
                    path,
                    status: status_kind(&code),
                    code,
                    additions: 0,
                    deletions: 0,
                    binary: false,
                    hunks: Vec::new(),
                    patch: None,
                    patch_truncated: false,
                })
            })
            .collect()
    }

    fn name_status_files(
        &self,
        git_root: &Path,
        refish: &str,
        errors: &mut Vec<String>,
    ) -> Vec<DiffFile> {
        let output = match git_output(
            git_root,
            [
                "diff",
                "--no-ext-diff",
                "--no-renames",
                "--name-status",
                "-z",
                refish,
                "--",
                ".",
            ],
        ) {
            Ok(output) => output,
            Err(err) => {
                errors.push(err);
                return Vec::new();
            }
        };
        let fields = nul_fields(&output.stdout);
        let mut files = Vec::new();
        let mut index = 0usize;
        while index + 1 < fields.len() {
            let code = fields[index].clone();
            let path = fields[index + 1].clone();
            files.push(DiffFile {
                path,
                status: status_kind(&code),
                code,
                additions: 0,
                deletions: 0,
                binary: false,
                hunks: Vec::new(),
                patch: None,
                patch_truncated: false,
            });
            index += 2;
        }
        files
    }

    fn numstat(
        &self,
        git_root: &Path,
        refish: &str,
        errors: &mut Vec<String>,
    ) -> BTreeMap<String, FileStat> {
        let output = match git_output(
            git_root,
            [
                "diff",
                "--no-ext-diff",
                "--no-renames",
                "--numstat",
                "-z",
                refish,
                "--",
                ".",
            ],
        ) {
            Ok(output) => output,
            Err(err) => {
                errors.push(err);
                return BTreeMap::new();
            }
        };
        parse_numstat(&output.stdout)
    }

    fn numstat_untracked(&self, git_root: &Path, file: &str) -> Option<FileStat> {
        let output = git_output_allow_status(
            git_root,
            ["diff", "--no-index", "--numstat", "--", "/dev/null", file],
            &[0, 1],
        )
        .ok()?;
        parse_numstat(&output.stdout).into_values().next()
    }

    fn patch_file(
        &self,
        git_root: &Path,
        refish: &str,
        file: &str,
        max_bytes: usize,
    ) -> Option<Patch> {
        let output = git_output_allow_status(
            git_root,
            [
                "diff",
                "--patch",
                "--no-ext-diff",
                "--no-renames",
                "--unified=3",
                refish,
                "--",
                file,
            ],
            &[0],
        )
        .ok()?;
        Some(capped_patch(output.stdout, max_bytes))
    }

    fn patch_untracked(&self, git_root: &Path, file: &str, max_bytes: usize) -> Option<Patch> {
        let output = git_output_allow_status(
            git_root,
            [
                "diff",
                "--no-index",
                "--patch",
                "--no-ext-diff",
                "--no-renames",
                "--unified=3",
                "--",
                "/dev/null",
                file,
            ],
            &[0, 1],
        )
        .ok()?;
        Some(capped_patch(output.stdout, max_bytes))
    }
}

#[derive(Debug, Clone, Copy)]
struct FileStat {
    additions: u64,
    deletions: u64,
    binary: bool,
}

#[derive(Debug, Clone)]
struct Patch {
    text: String,
    truncated: bool,
}

fn git_text<const N: usize>(cwd: &Path, args: [&str; N]) -> std::result::Result<String, String> {
    let output = git_output(cwd, args)?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_output<const N: usize>(cwd: &Path, args: [&str; N]) -> std::result::Result<Output, String> {
    git_output_allow_status(cwd, args, &[0])
}

fn git_output_allow_status<const N: usize>(
    cwd: &Path,
    args: [&str; N],
    success: &[i32],
) -> std::result::Result<Output, String> {
    let output = Command::new("git")
        .args([
            "--no-optional-locks",
            "-c",
            "core.autocrlf=false",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.quotepath=false",
        ])
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|err| format!("git failed to start: {err}"))?;
    let code = output.status.code().unwrap_or(-1);
    if success.contains(&code) {
        Ok(output)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if stderr.is_empty() {
            format!("git exited with status {code}")
        } else {
            stderr
        })
    }
}

fn default_branch(git_root: &Path) -> Option<String> {
    if let Ok(head) = git_text(git_root, ["symbolic-ref", "refs/remotes/origin/HEAD"]) {
        let name = head.strip_prefix("refs/remotes/origin/")?.to_string();
        if !name.is_empty() {
            return Some(format!("origin/{name}"));
        }
    }
    if let Ok(configured) = git_text(git_root, ["config", "init.defaultBranch"])
        && ref_exists(git_root, &configured)
    {
        return Some(configured);
    }
    for candidate in ["origin/main", "origin/master", "main", "master"] {
        if ref_exists(git_root, candidate) {
            return Some(candidate.to_string());
        }
    }
    None
}

fn ref_exists(git_root: &Path, name: &str) -> bool {
    git_output_allow_status(git_root, ["rev-parse", "--verify", name], &[0]).is_ok()
}

fn normalize_git_dir(git_root: &Path, raw: &str) -> Option<String> {
    let path = PathBuf::from(raw);
    let path = if path.is_absolute() {
        path
    } else {
        git_root.join(path)
    };
    Some(path.to_string_lossy().to_string())
}

fn transient_operation_state(git_dir: &Path) -> Option<String> {
    for (marker, state) in [
        ("rebase-merge", "rebase"),
        ("rebase-apply", "rebase"),
        ("MERGE_HEAD", "merge"),
        ("CHERRY_PICK_HEAD", "cherry_pick"),
        ("REVERT_HEAD", "revert"),
    ] {
        if git_dir.join(marker).exists() {
            return Some(state.to_string());
        }
    }
    None
}

fn status_kind(code: &str) -> DiffFileStatus {
    if code == "??" || (code.contains('A') && !code.contains('D')) {
        DiffFileStatus::Added
    } else if code.contains('D') && !code.contains('A') {
        DiffFileStatus::Deleted
    } else {
        DiffFileStatus::Modified
    }
}

fn nul_fields(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .map(|field| String::from_utf8_lossy(field).to_string())
        .collect()
}

fn parse_numstat(bytes: &[u8]) -> BTreeMap<String, FileStat> {
    let mut output = BTreeMap::new();
    let text = String::from_utf8_lossy(bytes);
    for record in text.split('\0').filter(|record| !record.trim().is_empty()) {
        let mut parts = record.split('\t');
        let Some(additions) = parts.next() else {
            continue;
        };
        let Some(deletions) = parts.next() else {
            continue;
        };
        let path = parts.collect::<Vec<_>>().join("\t");
        if path.is_empty() {
            continue;
        }
        let binary = additions == "-" || deletions == "-";
        output.insert(
            path,
            FileStat {
                additions: parse_count(additions),
                deletions: parse_count(deletions),
                binary,
            },
        );
    }
    output
}

fn parse_count(value: &str) -> u64 {
    if value == "-" {
        0
    } else {
        value.parse().unwrap_or(0)
    }
}

fn capped_patch(bytes: Vec<u8>, max_bytes: usize) -> Patch {
    let truncated = bytes.len() > max_bytes;
    let text = if truncated {
        String::from_utf8_lossy(&bytes[..max_bytes]).to_string()
    } else {
        String::from_utf8_lossy(&bytes).to_string()
    };
    Patch { text, truncated }
}

pub fn parse_patch_hunks(patch: &str) -> Vec<DiffHunk> {
    let mut seen = BTreeSet::new();
    let mut hunks = Vec::new();
    for line in patch.lines() {
        let Some(header) = line.strip_prefix("@@ ") else {
            continue;
        };
        let Some(end) = header.find(" @@") else {
            continue;
        };
        let ranges = &header[..end];
        let mut parts = ranges.split_whitespace();
        let old = parts.next().unwrap_or_default().trim_start_matches('-');
        let new = parts.next().unwrap_or_default().trim_start_matches('+');
        let (old_start, old_lines) = parse_hunk_range(old);
        let (new_start, new_lines) = parse_hunk_range(new);
        let start_line = new_start.saturating_sub(1);
        let end_line = if new_lines == 0 {
            start_line
        } else {
            new_start.saturating_add(new_lines).saturating_sub(2)
        };
        let hunk = DiffHunk {
            old_start,
            old_lines,
            new_start,
            new_lines,
            start_line,
            end_line,
        };
        if seen.insert((
            hunk.old_start,
            hunk.new_start,
            hunk.old_lines,
            hunk.new_lines,
        )) {
            hunks.push(hunk);
        }
    }
    hunks
}

fn parse_hunk_range(value: &str) -> (u32, u32) {
    let mut parts = value.split(',');
    let start = parts
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let lines = parts
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1);
    (start, lines)
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
