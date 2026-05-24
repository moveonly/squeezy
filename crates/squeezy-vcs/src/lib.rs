use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use squeezy_core::{Result, SqueezyError};

pub const CRATE_NAME: &str = "squeezy-vcs";
const DEFAULT_MAX_PATCH_BYTES: usize = 1_000_000;
const DEFAULT_CHECKPOINT_RETENTION_DAYS: u64 = 7;
const DEFAULT_MAX_CHECKPOINT_FILE_BYTES: u64 = 2 * 1024 * 1024;
static SHADOW_REPO_INIT_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static CHECKPOINT_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn crate_name() -> &'static str {
    CRATE_NAME
}

#[derive(Debug, Clone)]
pub struct GitVcs {
    root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct CheckpointStore {
    root: PathBuf,
    git_dir: PathBuf,
    journal_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSnapshot {
    pub tree: String,
    pub large_files: Vec<LargeFileFingerprint>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LargeFileFingerprint {
    pub path: String,
    pub size_bytes: u64,
    pub mtime_secs: i64,
    pub mtime_nanos: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffMode {
    #[default]
    Worktree,
    Branch,
    /// Alias for [`DiffMode::Branch`] kept so the read-side baseline names
    /// (`worktree`/`branch_base`/`index`/`last_receipt`) line up one-to-one
    /// with `read_slice` arguments. Every dispatch in this crate treats it
    /// identically to `Branch`; if a real distinction is ever needed (for
    /// example diffing the merge base against the branch tip with a different
    /// `refish`), update both call sites in `snapshot` at the same time.
    BranchBase,
    Index,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointRecord {
    pub id: String,
    pub group_id: String,
    pub tool_name: String,
    pub call_id: String,
    pub status: String,
    pub before_tree: String,
    pub after_tree: String,
    pub files: Vec<CheckpointFile>,
    #[serde(default)]
    pub skipped_files: Vec<SkippedCheckpointFile>,
    pub summary: DiffSummary,
    #[serde(default)]
    pub journal_warnings: u64,
    #[serde(default)]
    pub coverage_warnings: Vec<String>,
    pub created_at_ms: u128,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointFile {
    pub path: String,
    pub status: DiffFileStatus,
    pub before_sha256: Option<String>,
    pub after_sha256: Option<String>,
    pub additions: u64,
    pub deletions: u64,
    pub binary: bool,
    pub patch: Option<String>,
    pub patch_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedCheckpointFile {
    pub path: String,
    pub reason: String,
    pub size_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointJournal {
    pub checkpoints: Vec<CheckpointRecord>,
    pub journal_warnings: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackResult {
    pub mode: RollbackMode,
    pub checkpoint_ids: Vec<String>,
    pub planned_files: usize,
    pub restored_files: Vec<String>,
    pub deleted_files: Vec<String>,
    pub conflicts: Vec<RollbackConflict>,
    pub skipped: bool,
    pub applied: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackConflict {
    pub checkpoint_id: String,
    pub path: String,
    pub expected_sha256: Option<String>,
    pub current_sha256: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RollbackTarget<'a> {
    Latest,
    Group(&'a str),
    Checkpoint(&'a str),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollbackMode {
    #[default]
    Atomic,
    BestEffort,
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
            DiffMode::Branch | DiffMode::BranchBase => vcs.merge_base.as_deref(),
            DiffMode::Index => vcs.head.as_deref(),
        };

        let mut by_path = BTreeMap::<String, DiffFile>::new();
        if mode != DiffMode::Index {
            for item in self.status_files(&git_root, &mut errors) {
                by_path.insert(item.path.clone(), item);
            }
        }
        if let Some(refish) = refish {
            let cached = mode == DiffMode::Index;
            for item in self.name_status_files(&git_root, refish, cached, &mut errors) {
                by_path.entry(item.path.clone()).or_insert(item);
            }
            for (path, stat) in self.numstat(&git_root, refish, cached, &mut errors) {
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
                    mode == DiffMode::Index,
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
        let merge_base = if matches!(mode, DiffMode::Branch | DiffMode::BranchBase) {
            default_branch
                .as_deref()
                .and_then(|base| git_text(git_root, ["merge-base", base, "HEAD"]).ok())
        } else {
            None
        };
        let operation_state = git_dir
            .as_deref()
            .and_then(|path| transient_operation_state(Path::new(path)));
        if matches!(mode, DiffMode::Branch | DiffMode::BranchBase) && default_branch.is_none() {
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
                ":(exclude).squeezy",
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
        cached: bool,
        errors: &mut Vec<String>,
    ) -> Vec<DiffFile> {
        let mut args = vec![
            "diff".to_string(),
            "--no-ext-diff".to_string(),
            "--no-renames".to_string(),
            "--name-status".to_string(),
            "-z".to_string(),
        ];
        if cached {
            args.push("--cached".to_string());
        }
        args.extend([
            refish.to_string(),
            "--".to_string(),
            ".".to_string(),
            ":(exclude).squeezy".to_string(),
        ]);
        let output = match git_output_vec_allow_status(git_root, args, &[0]) {
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
        cached: bool,
        errors: &mut Vec<String>,
    ) -> BTreeMap<String, FileStat> {
        let mut args = vec![
            "diff".to_string(),
            "--no-ext-diff".to_string(),
            "--no-renames".to_string(),
            "--numstat".to_string(),
            "-z".to_string(),
        ];
        if cached {
            args.push("--cached".to_string());
        }
        args.extend([
            refish.to_string(),
            "--".to_string(),
            ".".to_string(),
            ":(exclude).squeezy".to_string(),
        ]);
        let output = match git_output_vec_allow_status(git_root, args, &[0]) {
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
        cached: bool,
        file: &str,
        max_bytes: usize,
    ) -> Option<Patch> {
        let mut args = vec![
            "diff".to_string(),
            "--patch".to_string(),
            "--no-ext-diff".to_string(),
            "--no-renames".to_string(),
            "--unified=3".to_string(),
        ];
        if cached {
            args.push("--cached".to_string());
        }
        args.extend([refish.to_string(), "--".to_string(), file.to_string()]);
        let output = git_output_vec_allow_status(git_root, args, &[0]).ok()?;
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

impl CheckpointStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root
            .as_ref()
            .canonicalize()
            .map_err(|err| SqueezyError::Tool(format!("invalid workspace root: {err}")))?;
        let dir = root.join(".squeezy").join("checkpoints");
        let git_dir = dir.join("git");
        let journal_path = dir.join("journal.jsonl");
        fs::create_dir_all(&git_dir)?;
        let store = Self {
            root,
            git_dir,
            journal_path,
        };
        store.ensure_shadow_repo()?;
        store.cleanup_old_checkpoints(DEFAULT_CHECKPOINT_RETENTION_DAYS)?;
        Ok(store)
    }

    pub fn track_tree(&self) -> Result<WorkspaceSnapshot> {
        self.ensure_shadow_repo()?;
        let large_files = self.large_file_fingerprints()?;
        let mut add_args = vec![
            "add".to_string(),
            "--all".to_string(),
            "--".to_string(),
            ".".to_string(),
            ":(exclude).squeezy".to_string(),
        ];
        for file in &large_files {
            add_args.push(format!(":(exclude){}", file.path));
        }
        self.git_vec(add_args)?;
        if !large_files.is_empty() {
            let mut rm_args = vec![
                "rm".to_string(),
                "--cached".to_string(),
                "--force".to_string(),
                "--ignore-unmatch".to_string(),
                "--".to_string(),
            ];
            rm_args.extend(large_files.iter().map(|file| file.path.clone()));
            let _ = self.git_vec(rm_args);
        }
        let output = self.git(["write-tree"])?;
        let tree = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(WorkspaceSnapshot { tree, large_files })
    }

    pub fn create_checkpoint(
        &self,
        before: &WorkspaceSnapshot,
        tool_name: &str,
        call_id: &str,
        group_id: &str,
        status: &str,
        mut coverage_warnings: Vec<String>,
    ) -> Result<Option<CheckpointRecord>> {
        let after = self.track_tree()?;
        let changed_large_paths = diff_large_files(&before.large_files, &after.large_files);
        if before.tree == after.tree && changed_large_paths.is_empty() {
            return Ok(None);
        }
        let (files, skipped_files) = self.checkpoint_files(
            &before.tree,
            &after.tree,
            &after.large_files,
            &changed_large_paths,
        )?;
        if files.is_empty() && skipped_files.is_empty() {
            return Ok(None);
        }
        let mut summary = DiffSummary {
            files_changed: files.len(),
            ..DiffSummary::default()
        };
        for file in &files {
            summary.additions += file.additions;
            summary.deletions += file.deletions;
            if file.before_sha256.is_none() && file.after_sha256.is_some() {
                summary.untracked_files += 1;
            }
            if file.binary {
                summary.binary_files += 1;
            }
        }
        if !skipped_files.is_empty() {
            coverage_warnings.push(format!(
                "{} file(s) exceeded the checkpoint size limit and are not rollback-protected",
                skipped_files.len()
            ));
        }
        let record = CheckpointRecord {
            id: checkpoint_id(),
            group_id: group_id.to_string(),
            tool_name: tool_name.to_string(),
            call_id: call_id.to_string(),
            status: status.to_string(),
            before_tree: before.tree.clone(),
            after_tree: after.tree,
            files,
            skipped_files,
            summary,
            journal_warnings: 0,
            coverage_warnings,
            created_at_ms: now_ms(),
        };
        self.protect_checkpoint_trees(&record)?;
        self.append_journal(json!({
            "kind": "checkpoint",
            "record": record,
        }))?;
        self.cleanup_old_checkpoints(DEFAULT_CHECKPOINT_RETENTION_DAYS)?;
        Ok(Some(record))
    }

    pub fn list_checkpoints(&self) -> Result<Vec<CheckpointRecord>> {
        Ok(self.read_journal()?.checkpoints)
    }

    pub fn read_journal(&self) -> Result<CheckpointJournal> {
        let text = match fs::read_to_string(&self.journal_path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(CheckpointJournal {
                    checkpoints: Vec::new(),
                    journal_warnings: 0,
                });
            }
            Err(err) => return Err(err.into()),
        };
        let mut records = Vec::new();
        let mut journal_warnings = 0;
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                journal_warnings += 1;
                continue;
            };
            if value.get("kind").and_then(|kind| kind.as_str()) != Some("checkpoint") {
                continue;
            }
            if let Some(record) = value.get("record")
                && let Ok(mut record) = serde_json::from_value::<CheckpointRecord>(record.clone())
            {
                record.journal_warnings = journal_warnings;
                records.push(record);
            } else {
                journal_warnings += 1;
            }
        }
        Ok(CheckpointJournal {
            checkpoints: records,
            journal_warnings,
        })
    }

    pub fn show_checkpoint(&self, id: &str) -> Result<Option<CheckpointRecord>> {
        Ok(self
            .read_journal()?
            .checkpoints
            .into_iter()
            .find(|record| record.id == id))
    }

    pub fn rollback(
        &self,
        target: RollbackTarget<'_>,
        mode: RollbackMode,
    ) -> Result<RollbackResult> {
        let records = self.list_checkpoints()?;
        let mut selected = match target {
            RollbackTarget::Latest => records.into_iter().rev().take(1).collect::<Vec<_>>(),
            RollbackTarget::Group(group_id) => records
                .into_iter()
                .filter(|record| record.group_id == group_id)
                .collect::<Vec<_>>(),
            RollbackTarget::Checkpoint(id) => records
                .into_iter()
                .filter(|record| record.id == id)
                .collect::<Vec<_>>(),
        };
        if selected.is_empty() {
            return Ok(RollbackResult {
                mode,
                checkpoint_ids: Vec::new(),
                planned_files: 0,
                restored_files: Vec::new(),
                deleted_files: Vec::new(),
                conflicts: Vec::new(),
                skipped: true,
                applied: false,
            });
        }
        selected.sort_by_key(|record| Reverse(record.created_at_ms));
        let conflicts = self.preflight_conflicts(&selected)?;
        let planned_files = selected.iter().map(|record| record.files.len()).sum();

        let mut result = RollbackResult {
            mode,
            checkpoint_ids: selected.iter().map(|record| record.id.clone()).collect(),
            planned_files,
            restored_files: Vec::new(),
            deleted_files: Vec::new(),
            conflicts,
            skipped: false,
            applied: false,
        };
        if mode == RollbackMode::Atomic && !result.conflicts.is_empty() {
            self.append_journal(json!({
                "kind": "rollback",
                "created_at_ms": now_ms(),
                "result": result,
            }))?;
            return Ok(result);
        }
        if planned_files == 0 {
            self.append_journal(json!({
                "kind": "rollback",
                "created_at_ms": now_ms(),
                "result": result,
            }))?;
            return Ok(result);
        }
        for record in &selected {
            self.rollback_record(record, &mut result)?;
        }
        self.track_tree()?;
        result.applied = true;
        self.append_journal(json!({
            "kind": "rollback",
            "created_at_ms": now_ms(),
            "result": result,
        }))?;
        Ok(result)
    }

    fn checkpoint_files(
        &self,
        before_tree: &str,
        after_tree: &str,
        large_after: &[LargeFileFingerprint],
        changed_large_paths: &[String],
    ) -> Result<(Vec<CheckpointFile>, Vec<SkippedCheckpointFile>)> {
        let large_after_set: BTreeSet<&str> =
            large_after.iter().map(|file| file.path.as_str()).collect();
        let mut statuses = BTreeMap::<String, DiffFileStatus>::new();
        let output = self.git_vec(vec![
            "diff".to_string(),
            "--no-ext-diff".to_string(),
            "--no-renames".to_string(),
            "--name-status".to_string(),
            "-z".to_string(),
            before_tree.to_string(),
            after_tree.to_string(),
            "--".to_string(),
            ".".to_string(),
        ])?;
        let fields = nul_fields(&output.stdout);
        let mut index = 0usize;
        while index + 1 < fields.len() {
            let code = fields[index].clone();
            let path = fields[index + 1].clone();
            statuses.insert(path, status_kind(&code));
            index += 2;
        }

        let mut stats = BTreeMap::<String, FileStat>::new();
        let output = self.git_vec(vec![
            "diff".to_string(),
            "--no-ext-diff".to_string(),
            "--no-renames".to_string(),
            "--numstat".to_string(),
            "-z".to_string(),
            before_tree.to_string(),
            after_tree.to_string(),
            "--".to_string(),
            ".".to_string(),
        ])?;
        stats.extend(parse_numstat(&output.stdout));

        let mut files = Vec::new();
        let mut skipped_files = Vec::new();
        for (path, status) in statuses {
            if large_after_set.contains(path.as_str()) {
                skipped_files.push(SkippedCheckpointFile {
                    size_bytes: file_len(&self.root.join(&path)).ok(),
                    path,
                    reason: "file exceeds checkpoint size limit".to_string(),
                });
                continue;
            }
            let stat = stats.get(&path).copied().unwrap_or(FileStat {
                additions: 0,
                deletions: 0,
                binary: false,
            });
            let patch = self.diff_patch(before_tree, after_tree, &path)?;
            let before = self.blob_bytes(before_tree, &path).ok();
            let after = self.blob_bytes(after_tree, &path).ok();
            files.push(CheckpointFile {
                path,
                status,
                before_sha256: before.as_deref().map(sha256_hex),
                after_sha256: after.as_deref().map(sha256_hex),
                additions: stat.additions,
                deletions: stat.deletions,
                binary: stat.binary,
                patch: (!stat.binary).then_some(patch.text),
                patch_truncated: patch.truncated,
            });
        }
        for path in changed_large_paths {
            if skipped_files.iter().any(|file| file.path == *path) {
                continue;
            }
            skipped_files.push(SkippedCheckpointFile {
                size_bytes: file_len(&self.root.join(path)).ok(),
                path: path.clone(),
                reason: "file exceeds checkpoint size limit".to_string(),
            });
        }
        files.sort_by(|left, right| left.path.cmp(&right.path));
        skipped_files.sort_by(|left, right| left.path.cmp(&right.path));
        Ok((files, skipped_files))
    }

    fn preflight_conflicts(&self, records: &[CheckpointRecord]) -> Result<Vec<RollbackConflict>> {
        let mut conflicts = Vec::new();
        let mut virtual_hashes = BTreeMap::<String, Option<String>>::new();
        for record in records {
            for file in &record.files {
                let current_sha256 = match virtual_hashes.get(&file.path) {
                    Some(hash) => hash.clone(),
                    None => {
                        let path = self.root.join(&file.path);
                        let hash = if path.exists() {
                            Some(sha256_hex(&fs::read(&path)?))
                        } else {
                            None
                        };
                        virtual_hashes.insert(file.path.clone(), hash.clone());
                        hash
                    }
                };
                if let Some(conflict) = self.rollback_conflict(record, file, current_sha256)? {
                    conflicts.push(conflict);
                } else {
                    virtual_hashes.insert(file.path.clone(), file.before_sha256.clone());
                }
            }
        }
        Ok(conflicts)
    }

    fn rollback_record(
        &self,
        record: &CheckpointRecord,
        result: &mut RollbackResult,
    ) -> Result<()> {
        for file in &record.files {
            if result
                .conflicts
                .iter()
                .any(|conflict| conflict.checkpoint_id == record.id && conflict.path == file.path)
            {
                continue;
            }
            let path = self.root.join(&file.path);

            match self.blob_bytes(&record.before_tree, &file.path) {
                Ok(bytes) => {
                    if let Some(parent) = path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::write(&path, bytes)?;
                    result.restored_files.push(file.path.clone());
                }
                Err(_) => {
                    if path.exists() {
                        fs::remove_file(&path)?;
                    }
                    result.deleted_files.push(file.path.clone());
                }
            }
        }
        Ok(())
    }

    fn rollback_conflict(
        &self,
        record: &CheckpointRecord,
        file: &CheckpointFile,
        current_sha256: Option<String>,
    ) -> Result<Option<RollbackConflict>> {
        if current_sha256 != file.after_sha256 {
            return Ok(Some(RollbackConflict {
                checkpoint_id: record.id.clone(),
                path: file.path.clone(),
                expected_sha256: file.after_sha256.clone(),
                current_sha256,
                reason: "file changed after checkpoint; leaving current content untouched"
                    .to_string(),
            }));
        }
        if self.tree_has_path(&record.before_tree, &file.path)? {
            let blob = self.blob_bytes(&record.before_tree, &file.path);
            if blob.is_err() {
                return Ok(Some(RollbackConflict {
                    checkpoint_id: record.id.clone(),
                    path: file.path.clone(),
                    expected_sha256: file.after_sha256.clone(),
                    current_sha256,
                    reason: "checkpoint object is missing; leaving current content untouched"
                        .to_string(),
                }));
            }
        }
        Ok(None)
    }

    fn tree_has_path(&self, tree: &str, path: &str) -> Result<bool> {
        let output = self.git_vec(vec![
            "ls-tree".to_string(),
            tree.to_string(),
            "--".to_string(),
            path.to_string(),
        ])?;
        Ok(!output.stdout.is_empty())
    }

    fn ensure_shadow_repo(&self) -> Result<()> {
        let head_exists = self.git_dir.join("HEAD").exists();
        let exclude_path = self.git_dir.join("info").join("exclude");
        let exclude_exists = exclude_path.exists();
        if head_exists && exclude_exists {
            return Ok(());
        }
        let _guard = SHADOW_REPO_INIT_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .map_err(|err| SqueezyError::Tool(format!("checkpoint init lock poisoned: {err}")))?;
        if !self.git_dir.join("HEAD").exists() {
            fs::create_dir_all(&self.git_dir)?;
            self.git_raw(["init"])?;
            self.git_raw(["config", "core.autocrlf", "false"])?;
            self.git_raw(["config", "core.fsmonitor", "false"])?;
            self.git_raw(["config", "core.quotepath", "false"])?;
        }
        if !exclude_path.exists() {
            if let Some(parent) = exclude_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&exclude_path, "/.squeezy/\n")?;
        }
        Ok(())
    }

    fn large_file_fingerprints(&self) -> Result<Vec<LargeFileFingerprint>> {
        let mut files = Vec::new();
        collect_large_file_fingerprints(&self.root, &self.root, &mut files)?;
        files.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(files)
    }

    fn protect_checkpoint_trees(&self, record: &CheckpointRecord) -> Result<()> {
        self.git_vec(vec![
            "update-ref".to_string(),
            checkpoint_ref(&record.id, "before"),
            record.before_tree.clone(),
        ])?;
        self.git_vec(vec![
            "update-ref".to_string(),
            checkpoint_ref(&record.id, "after"),
            record.after_tree.clone(),
        ])?;
        Ok(())
    }

    fn cleanup_old_checkpoints(&self, retention_days: u64) -> Result<()> {
        let journal = self.read_journal()?;
        if journal.checkpoints.is_empty() {
            return Ok(());
        }
        let cutoff = now_ms().saturating_sub(retention_days as u128 * 24 * 60 * 60 * 1_000);
        let (keep, prune): (Vec<_>, Vec<_>) = journal
            .checkpoints
            .into_iter()
            .partition(|record| record.created_at_ms >= cutoff);
        if prune.is_empty() {
            return Ok(());
        }
        for record in &prune {
            let _ = self.git_vec(vec![
                "update-ref".to_string(),
                "-d".to_string(),
                checkpoint_ref(&record.id, "before"),
            ]);
            let _ = self.git_vec(vec![
                "update-ref".to_string(),
                "-d".to_string(),
                checkpoint_ref(&record.id, "after"),
            ]);
        }
        self.rewrite_checkpoint_journal(&keep)?;
        let _ = self.git(["gc", "--prune=now"]);
        Ok(())
    }

    fn rewrite_checkpoint_journal(&self, records: &[CheckpointRecord]) -> Result<()> {
        if let Some(parent) = self.journal_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = fs::File::create(&self.journal_path)?;
        for record in records {
            serde_json::to_writer(
                &mut file,
                &json!({
                    "kind": "checkpoint",
                    "record": record,
                }),
            )
            .map_err(|err| {
                SqueezyError::Tool(format!("failed to rewrite checkpoint journal: {err}"))
            })?;
            file.write_all(b"\n")?;
        }
        Ok(())
    }

    fn diff_patch(&self, before_tree: &str, after_tree: &str, path: &str) -> Result<Patch> {
        let output = self.git_vec_allow_status(
            vec![
                "diff".to_string(),
                "--patch".to_string(),
                "--no-ext-diff".to_string(),
                "--no-renames".to_string(),
                "--unified=3".to_string(),
                before_tree.to_string(),
                after_tree.to_string(),
                "--".to_string(),
                path.to_string(),
            ],
            &[0],
        )?;
        Ok(capped_patch(output.stdout, DEFAULT_MAX_PATCH_BYTES))
    }

    fn blob_bytes(&self, tree: &str, path: &str) -> std::result::Result<Vec<u8>, String> {
        self.git_vec(vec!["show".to_string(), format!("{tree}:{path}")])
            .map(|output| output.stdout)
            .map_err(|err| err.to_string())
    }

    fn append_journal(&self, value: serde_json::Value) -> Result<()> {
        if let Some(parent) = self.journal_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.journal_path)?;
        serde_json::to_writer(&mut file, &value).map_err(|err| {
            SqueezyError::Tool(format!("failed to write checkpoint journal: {err}"))
        })?;
        file.write_all(b"\n")?;
        Ok(())
    }

    fn git<const N: usize>(&self, args: [&str; N]) -> Result<Output> {
        self.git_vec(args.into_iter().map(str::to_string).collect())
    }

    fn git_raw<const N: usize>(&self, args: [&str; N]) -> Result<Output> {
        git_output_vec_allow_status(
            &self.root,
            std::iter::once("--git-dir".to_string())
                .chain(std::iter::once(self.git_dir.to_string_lossy().to_string()))
                .chain(args.into_iter().map(str::to_string))
                .collect(),
            &[0],
        )
        .map_err(SqueezyError::Tool)
    }

    fn git_vec(&self, args: Vec<String>) -> Result<Output> {
        self.git_vec_allow_status(args, &[0])
    }

    fn git_vec_allow_status(&self, args: Vec<String>, success: &[i32]) -> Result<Output> {
        let full_args = std::iter::once("--git-dir".to_string())
            .chain(std::iter::once(self.git_dir.to_string_lossy().to_string()))
            .chain(std::iter::once("--work-tree".to_string()))
            .chain(std::iter::once(self.root.to_string_lossy().to_string()))
            .chain(args)
            .collect();
        git_output_vec_allow_status(&self.root, full_args, success).map_err(SqueezyError::Tool)
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
    git_output_vec_allow_status(cwd, args.into_iter().map(str::to_string).collect(), success)
}

fn git_output_vec_allow_status(
    cwd: &Path,
    args: Vec<String>,
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

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

fn checkpoint_id() -> String {
    let counter = CHECKPOINT_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("cp-{:013}-{:08x}", now_ms(), counter)
}

fn checkpoint_ref(id: &str, side: &str) -> String {
    format!("refs/squeezy/checkpoints/{id}/{side}")
}

fn collect_large_file_fingerprints(
    root: &Path,
    dir: &Path,
    files: &mut Vec<LargeFileFingerprint>,
) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        if name
            .to_str()
            .is_some_and(|name| matches!(name, ".git" | ".squeezy"))
        {
            continue;
        }
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            collect_large_file_fingerprints(root, &path, files)?;
        } else if metadata.is_file() && metadata.len() > DEFAULT_MAX_CHECKPOINT_FILE_BYTES {
            let (mtime_secs, mtime_nanos) = mtime_parts(&metadata);
            files.push(LargeFileFingerprint {
                path: rel_path(root, &path),
                size_bytes: metadata.len(),
                mtime_secs,
                mtime_nanos,
            });
        }
    }
    Ok(())
}

fn mtime_parts(metadata: &fs::Metadata) -> (i64, u32) {
    let Ok(modified) = metadata.modified() else {
        return (0, 0);
    };
    match modified.duration_since(UNIX_EPOCH) {
        Ok(duration) => (duration.as_secs() as i64, duration.subsec_nanos()),
        Err(err) => {
            let duration = err.duration();
            (-(duration.as_secs() as i64), duration.subsec_nanos())
        }
    }
}

fn diff_large_files(
    before: &[LargeFileFingerprint],
    after: &[LargeFileFingerprint],
) -> Vec<String> {
    let before_map: BTreeMap<&str, &LargeFileFingerprint> = before
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect();
    let after_map: BTreeMap<&str, &LargeFileFingerprint> = after
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect();
    let mut changed = BTreeSet::<String>::new();
    for path in before_map.keys().chain(after_map.keys()) {
        if before_map.get(path) != after_map.get(path) {
            changed.insert((*path).to_string());
        }
    }
    changed.into_iter().collect()
}

fn rel_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn file_len(path: &Path) -> std::io::Result<u64> {
    Ok(fs::metadata(path)?.len())
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
