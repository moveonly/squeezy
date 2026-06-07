use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions, TryLockError},
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use squeezy_core::{Result, SqueezyError};

pub mod worktree;
pub use worktree::{Worktree, WorktreeCleanup, validate_worktree_slug};

pub const CRATE_NAME: &str = "squeezy-vcs";
const DEFAULT_MAX_PATCH_BYTES: usize = 1_000_000;
const DEFAULT_CHECKPOINT_RETENTION_DAYS: u64 = 7;
const DEFAULT_MAX_CHECKPOINT_FILE_BYTES: u64 = 2 * 1024 * 1024;
const SHADOW_LOCK_FILENAME: &str = "shadow.lock";
const SHADOW_STALE_DIR_RETENTION_DAYS: u64 = 14;
static SHADOW_REPO_INIT_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static CHECKPOINT_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn crate_name() -> &'static str {
    CRATE_NAME
}

/// Canonicalize a workspace root and strip the Windows verbatim (`\\?\`)
/// prefix so the resulting path is safe to hand to Git for Windows and
/// `Command::current_dir`, both of which mishandle the extended-path form
/// produced by `fs::canonicalize` on Windows.
pub fn canonicalize_workspace_root(path: impl AsRef<Path>) -> std::io::Result<PathBuf> {
    fs::canonicalize(path.as_ref()).map(strip_verbatim_prefix)
}

/// Remove the `\\?\` Windows extended-path prefix from a canonical path so
/// downstream tools that still rely on legacy Win32 path parsing (such as
/// Git for Windows) can resolve it. UNC paths (`\\?\UNC\...`) keep their
/// prefix because the legacy form has no equivalent.
pub fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    if !cfg!(windows) {
        return path;
    }
    let s = path.to_string_lossy();
    let Some(rest) = s.strip_prefix(r"\\?\") else {
        return path;
    };
    if rest.starts_with("UNC\\") || rest.starts_with("UNC/") {
        return path;
    }
    PathBuf::from(rest.to_string())
}

#[derive(Debug, Clone)]
pub struct GitVcs {
    root: PathBuf,
}

#[derive(Debug)]
pub struct CheckpointStore {
    root: PathBuf,
    git_dir: PathBuf,
    journal_path: PathBuf,
    lock_path: PathBuf,
    options: CheckpointStoreOptions,
    /// OS-advisory lock held for the lifetime of the store. Dropping the
    /// [`File`] releases the lock; the [`Drop`] impl also unlinks
    /// [`Self::lock_path`] so a fresh process sees a clean directory.
    _lock: Option<File>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointStoreOptions {
    pub retention_days: u64,
    pub max_file_bytes: u64,
    pub cleanup_interval_secs: u64,
}

impl Default for CheckpointStoreOptions {
    fn default() -> Self {
        Self {
            retention_days: DEFAULT_CHECKPOINT_RETENTION_DAYS,
            max_file_bytes: DEFAULT_MAX_CHECKPOINT_FILE_BYTES,
            cleanup_interval_secs: 60 * 60,
        }
    }
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
    Renamed,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_path: Option<String>,
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
pub struct UnifiedDiffOutcome {
    pub applied: bool,
    pub stdout: String,
    pub stderr: String,
    pub conflicted_paths: Vec<String>,
    pub skipped_paths: Vec<String>,
}

/// Kind of `apply_patch` operation surfaced to a preview consumer.
///
/// Mirrors the op tags accepted by the `apply_patch` tool so the preview
/// stream stays meaningful even when the consumer never sees the full
/// argument payload. Tag values match the `ApplyPatchOperation` JSON shape
/// in `squeezy-tools` so the TUI can render a single label for both
/// streamed previews and post-apply receipts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchOpKind {
    SearchReplace,
    CreateFile,
    DeleteFile,
    MoveFile,
}

/// One streamed preview item produced by [`preview_patch_stream`].
///
/// Each entry stands for a single op the model has fully described in the
/// JSON tool-arg payload so far. Hashes are sha256 hex digests of the raw
/// `search`/`replace`/`contents` bytes — emitting hashes instead of payload
/// text keeps the preview channel cheap when an op rewrites a large block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchOpPreview {
    /// Zero-based op index in the payload order.
    pub index: usize,
    pub kind: PatchOpKind,
    /// Primary path: the file being mutated for in-place ops, or the
    /// destination path for `MoveFile`.
    pub path: String,
    /// Source path for `MoveFile`. `None` for every other op.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_path: Option<String>,
    /// sha256 of the `search` body for `SearchReplace`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_hash: Option<String>,
    /// sha256 of the `replace` body for `SearchReplace`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replace_hash: Option<String>,
    /// sha256 of the `contents` body for `CreateFile`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contents_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointJournal {
    pub checkpoints: Vec<CheckpointRecord>,
    #[serde(default)]
    pub rollbacks: Vec<RollbackJournalRecord>,
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
pub struct RollbackJournalRecord {
    pub created_at_ms: u128,
    pub result: RollbackResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointIntegrityReport {
    pub ok: bool,
    pub checkpoints_checked: usize,
    pub journal_warnings: u64,
    pub missing_refs: Vec<CheckpointIntegrityProblem>,
    pub missing_objects: Vec<CheckpointIntegrityProblem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointIntegrityProblem {
    pub checkpoint_id: String,
    pub path: Option<String>,
    pub side: String,
    pub reason: String,
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
        let root = canonicalize_workspace_root(root.as_ref())
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

        // Bulk-fetch patches for all tracked files in a single `git diff`
        // invocation. Spawning git once per file ran the snapshot at
        // 20-50ms × N files on macOS; one batched call collapses that to
        // a single subprocess regardless of N.
        let mut bulk_patches: BTreeMap<String, Patch> = BTreeMap::new();
        if let Some(refish_str) = refish {
            let tracked: Vec<String> = files
                .iter()
                .filter(|file| file.code != "??")
                .map(|file| file.path.clone())
                .collect();
            if !tracked.is_empty() {
                bulk_patches = self.patches_bulk(
                    &git_root,
                    refish_str,
                    mode == DiffMode::Index,
                    &tracked,
                    options.max_patch_bytes,
                    &mut errors,
                );
            }
        }

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
            } else if let Some(patch) = bulk_patches.remove(&file.path) {
                Some(patch)
            } else {
                // Fall back to a per-file call if the bulk parser missed
                // this path (unusual filenames that the splitter could
                // not match back). Preserves behavior for the long tail.
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

    // Design note — unified-diff fallback as the explicit escape hatch.
    //
    // The primary patch surface in `squeezy-tools::apply_patch` is strict
    // literal search-replace gated by `expected_sha256`: the `search` block
    // must substring-match the on-disk file byte-for-byte, and the pre-edit
    // hash must match the current on-disk hash. A mismatch yields
    // `ToolStatus::Stale` and refuses to write. This guarantees the agent
    // can never silently overwrite a file whose contents drifted away from
    // what it planned against — including drift produced by the user
    // between turns or by a concurrent tool.
    //
    // Progressive line-pattern fuzz (exact → rstrip → trimmed →
    // Unicode-normalised) is intentionally NOT the default. It trades a
    // pre-mutation hash check for opportunistic typographic recovery,
    // and the resulting "the model intended this so we wrote it"
    // semantics weaken the cross-turn safety property above.
    //
    // The unified-diff path below is the deliberate escape hatch: callers
    // opt in per-block via `fallback:"unified_diff"` when their literal
    // search misses. It runs `git apply --3way --ignore-whitespace`, which
    // recovers the typographic-drift cases (smart quotes, em-dashes, NBSP,
    // tab/space drift) without giving up the sha256 contract — the caller
    // still gates around this call with checkpoint capture and post-apply
    // hash tracking. Promote this fallback in error messages and docs
    // rather than loosening the literal default.

    /// Run `git apply --check --3way` against the user worktree to see whether
    /// the given unified-diff body would apply cleanly. No files are mutated.
    pub fn preflight_unified_diff(&self, diff: &str) -> Result<UnifiedDiffOutcome> {
        self.run_unified_diff(diff, true)
    }

    /// Run `git apply --3way` against the user worktree. Mutates the worktree
    /// if the diff applies. The caller is responsible for sha256-gating and
    /// checkpoint tracking around this call.
    pub fn apply_unified_diff(&self, diff: &str) -> Result<UnifiedDiffOutcome> {
        self.run_unified_diff(diff, false)
    }

    fn run_unified_diff(&self, diff: &str, preflight: bool) -> Result<UnifiedDiffOutcome> {
        use std::io::Write as _;
        use std::process::Stdio;
        // `--3way` falls back to a three-way merge when blob hashes are present
        // in the diff; `--ignore-whitespace` lets the same body apply against a
        // worktree that has accumulated minor whitespace drift — the typical
        // case this fallback is meant to recover.
        let mut args: Vec<&str> = vec!["apply", "--3way", "--ignore-whitespace"];
        if preflight {
            args.push("--check");
        }
        let mut child = Command::new("git")
            .args(&args)
            .current_dir(&self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| SqueezyError::Tool(format!("failed to spawn git apply: {err}")))?;
        {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| SqueezyError::Tool("git apply stdin unavailable".to_string()))?;
            stdin
                .write_all(diff.as_bytes())
                .map_err(|err| SqueezyError::Tool(format!("write to git apply: {err}")))?;
        }
        let output = child
            .wait_with_output()
            .map_err(|err| SqueezyError::Tool(format!("git apply wait failed: {err}")))?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let applied = output.status.success();
        let mut conflicted_paths: Vec<String> = Vec::new();
        let mut skipped_paths: Vec<String> = Vec::new();
        for line in stderr.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("error: ")
                && let Some(path) = rest.split(':').next()
            {
                skipped_paths.push(path.trim().to_string());
            }
            if let Some(rest) = trimmed.strip_prefix("CONFLICT (")
                && let Some(path) = rest.split_once("): ").map(|(_, p)| p.trim().to_string())
            {
                conflicted_paths.push(path);
            }
        }
        conflicted_paths.sort();
        conflicted_paths.dedup();
        skipped_paths.sort();
        skipped_paths.dedup();
        Ok(UnifiedDiffOutcome {
            applied,
            stdout,
            stderr,
            conflicted_paths,
            skipped_paths,
        })
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

    /// Fetch patches for `files` in a single `git diff` invocation. Splits
    /// the combined output on each `diff --git a/<path> b/<path>` header,
    /// caps each per-file slice at `max_bytes`, and returns the result
    /// keyed by file path. Files that the splitter can't map back to an
    /// entry in `files` are silently dropped — the caller falls back to
    /// per-file `patch_file` for those. Errors from git are surfaced as
    /// warnings via `errors`; on failure the map is empty and the caller
    /// falls back transparently.
    fn patches_bulk(
        &self,
        git_root: &Path,
        refish: &str,
        cached: bool,
        files: &[String],
        max_bytes: usize,
        errors: &mut Vec<String>,
    ) -> BTreeMap<String, Patch> {
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
        args.push(refish.to_string());
        args.push("--".to_string());
        args.extend(files.iter().cloned());
        let output = match git_output_vec_allow_status(git_root, args, &[0]) {
            Ok(output) => output,
            Err(err) => {
                errors.push(format!("git diff (bulk) failed: {err}"));
                return BTreeMap::new();
            }
        };
        split_unified_patch(&output.stdout, files, max_bytes)
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
        Self::open_with_options(root, CheckpointStoreOptions::default())
    }

    pub fn open_with_options(
        root: impl AsRef<Path>,
        options: CheckpointStoreOptions,
    ) -> Result<Self> {
        let root = canonicalize_workspace_root(root.as_ref())
            .map_err(|err| SqueezyError::Tool(format!("invalid workspace root: {err}")))?;
        let dir = root.join(".squeezy").join("checkpoints");
        let git_dir = dir.join("git");
        let journal_path = dir.join("journal.jsonl");
        let lock_path = dir.join(SHADOW_LOCK_FILENAME);
        fs::create_dir_all(&git_dir)?;
        // Acquire the per-workspace shadow-repo lock before any cleanup or
        // git work — two squeezy processes pointed at the same workspace
        // must not race on `git add --all` + `write-tree`.
        let lock = acquire_shadow_lock(&lock_path)?;
        cleanup_stale_shadow_dirs(&dir, SHADOW_STALE_DIR_RETENTION_DAYS);
        let store = Self {
            root,
            git_dir,
            journal_path,
            lock_path,
            options: CheckpointStoreOptions {
                retention_days: nonzero_u64(
                    options.retention_days,
                    DEFAULT_CHECKPOINT_RETENTION_DAYS,
                ),
                max_file_bytes: nonzero_u64(
                    options.max_file_bytes,
                    DEFAULT_MAX_CHECKPOINT_FILE_BYTES,
                ),
                cleanup_interval_secs: options.cleanup_interval_secs,
            },
            _lock: Some(lock),
        };
        store.ensure_shadow_repo()?;
        store.cleanup_old_checkpoints_if_due()?;
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
        // Git exits 1 with an "addIgnoredFile" advisory when a workspace
        // `.gitignore` matches `.squeezy/` (squeezy ships exactly that rule
        // in its own repo). The exclude pathspec is matched literally before
        // the gitignore check, so the advisory fires even though every
        // non-excluded file was staged successfully. Allow status 1 here and
        // treat the run as a success when stderr only carries the advisory.
        let add_output = self.git_vec_allow_status(add_args, &[0, 1])?;
        if add_output.status.code() == Some(1) {
            let stderr = String::from_utf8_lossy(&add_output.stderr);
            if !is_add_ignored_advisory_only(&stderr) {
                return Err(SqueezyError::Tool(stderr.trim().to_string()));
            }
        }
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
        self.cleanup_old_checkpoints_if_due()?;
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
                    rollbacks: Vec::new(),
                    journal_warnings: 0,
                });
            }
            Err(err) => return Err(err.into()),
        };
        let mut records = Vec::new();
        let mut rollbacks = Vec::new();
        let mut journal_warnings = 0;
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                journal_warnings += 1;
                continue;
            };
            match value.get("kind").and_then(|kind| kind.as_str()) {
                Some("checkpoint") => {
                    if let Some(record) = value.get("record")
                        && let Ok(mut record) =
                            serde_json::from_value::<CheckpointRecord>(record.clone())
                    {
                        record.journal_warnings = journal_warnings;
                        records.push(record);
                    } else {
                        journal_warnings += 1;
                    }
                }
                Some("rollback") => {
                    if let Some(result) = value.get("result")
                        && let Ok(result) = serde_json::from_value::<RollbackResult>(result.clone())
                    {
                        rollbacks.push(RollbackJournalRecord {
                            created_at_ms: value
                                .get("created_at_ms")
                                .and_then(|value| value.as_u64())
                                .map(u128::from)
                                .unwrap_or(0),
                            result,
                        });
                    } else {
                        journal_warnings += 1;
                    }
                }
                _ => {}
            }
        }
        Ok(CheckpointJournal {
            checkpoints: records,
            rollbacks,
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
        let selected = self.selected_rollback_records(target)?;
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
        let conflicts = self.preflight_conflicts(&selected)?;
        let planned_files = selected.iter().map(|record| record.files.len()).sum();
        let mut operations = Vec::new();

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
            self.plan_rollback_record(record, &result.conflicts, &mut operations)?;
        }
        match mode {
            RollbackMode::Atomic => {
                self.apply_atomic_operations(&operations, &mut result)?;
            }
            RollbackMode::BestEffort => {
                self.apply_operations(&operations, &mut result)?;
            }
        }
        self.track_tree()?;
        result.applied = !result.restored_files.is_empty() || !result.deleted_files.is_empty();
        self.append_journal(json!({
            "kind": "rollback",
            "created_at_ms": now_ms(),
            "result": result,
        }))?;
        Ok(result)
    }

    pub fn rollback_paths(&self, target: RollbackTarget<'_>) -> Result<Vec<String>> {
        let mut paths = BTreeSet::new();
        for record in self.selected_rollback_records(target)? {
            for file in record.files {
                paths.insert(file.path);
                if let Some(from_path) = file.from_path {
                    paths.insert(from_path);
                }
            }
        }
        Ok(paths.into_iter().collect())
    }

    pub fn restore_checkpoint_file(
        &self,
        checkpoint_id: &str,
        path: &str,
        mode: RollbackMode,
    ) -> Result<RollbackResult> {
        let Some(record) = self.show_checkpoint(checkpoint_id)? else {
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
        };
        let files = record
            .files
            .iter()
            .filter(|file| file.path == path || file.from_path.as_deref() == Some(path))
            .cloned()
            .collect::<Vec<_>>();
        if files.is_empty() {
            return Ok(RollbackResult {
                mode,
                checkpoint_ids: vec![record.id],
                planned_files: 0,
                restored_files: Vec::new(),
                deleted_files: Vec::new(),
                conflicts: Vec::new(),
                skipped: true,
                applied: false,
            });
        }
        let record = CheckpointRecord { files, ..record };
        let selected = vec![record];
        let conflicts = self.preflight_conflicts(&selected)?;
        let mut result = RollbackResult {
            mode,
            checkpoint_ids: vec![checkpoint_id.to_string()],
            planned_files: selected[0].files.len(),
            restored_files: Vec::new(),
            deleted_files: Vec::new(),
            conflicts,
            skipped: false,
            applied: false,
        };
        if mode == RollbackMode::Atomic && !result.conflicts.is_empty() {
            return Ok(result);
        }
        let mut operations = Vec::new();
        self.plan_rollback_record(&selected[0], &result.conflicts, &mut operations)?;
        match mode {
            RollbackMode::Atomic => self.apply_atomic_operations(&operations, &mut result)?,
            RollbackMode::BestEffort => self.apply_operations(&operations, &mut result)?,
        }
        self.track_tree()?;
        result.applied = !result.restored_files.is_empty() || !result.deleted_files.is_empty();
        Ok(result)
    }

    pub fn restore_checkpoint_file_paths(
        &self,
        checkpoint_id: &str,
        path: &str,
    ) -> Result<Vec<String>> {
        let Some(record) = self.show_checkpoint(checkpoint_id)? else {
            return Ok(Vec::new());
        };
        let mut paths = BTreeSet::new();
        for file in record
            .files
            .iter()
            .filter(|file| file.path == path || file.from_path.as_deref() == Some(path))
        {
            paths.insert(file.path.clone());
            if let Some(from_path) = file.from_path.as_ref() {
                paths.insert(from_path.clone());
            }
        }
        Ok(paths.into_iter().collect())
    }

    pub fn integrity_report(&self) -> Result<CheckpointIntegrityReport> {
        let journal = self.read_journal()?;
        let mut missing_refs = Vec::new();
        let mut missing_objects = Vec::new();
        for record in &journal.checkpoints {
            for side in ["before", "after"] {
                let reference = checkpoint_ref(&record.id, side);
                if self
                    .git_vec_allow_status(
                        vec![
                            "show-ref".to_string(),
                            "--verify".to_string(),
                            "--quiet".to_string(),
                            reference.clone(),
                        ],
                        &[0, 1],
                    )?
                    .status
                    .code()
                    != Some(0)
                {
                    missing_refs.push(CheckpointIntegrityProblem {
                        checkpoint_id: record.id.clone(),
                        path: None,
                        side: side.to_string(),
                        reason: format!("{reference} is missing"),
                    });
                }
            }
            for file in &record.files {
                let before_path = file.from_path.as_deref().unwrap_or(&file.path);
                if file.before_sha256.is_some()
                    && self.blob_bytes(&record.before_tree, before_path).is_err()
                {
                    missing_objects.push(CheckpointIntegrityProblem {
                        checkpoint_id: record.id.clone(),
                        path: Some(before_path.to_string()),
                        side: "before".to_string(),
                        reason: "before blob is missing".to_string(),
                    });
                }
                if file.after_sha256.is_some()
                    && self.blob_bytes(&record.after_tree, &file.path).is_err()
                {
                    missing_objects.push(CheckpointIntegrityProblem {
                        checkpoint_id: record.id.clone(),
                        path: Some(file.path.clone()),
                        side: "after".to_string(),
                        reason: "after blob is missing".to_string(),
                    });
                }
            }
        }
        Ok(CheckpointIntegrityReport {
            ok: journal.journal_warnings == 0
                && missing_refs.is_empty()
                && missing_objects.is_empty(),
            checkpoints_checked: journal.checkpoints.len(),
            journal_warnings: journal.journal_warnings,
            missing_refs,
            missing_objects,
        })
    }

    fn selected_rollback_records(
        &self,
        target: RollbackTarget<'_>,
    ) -> Result<Vec<CheckpointRecord>> {
        let journal = self.read_journal()?;
        let rolled_back = journal
            .rollbacks
            .iter()
            .filter(|rollback| {
                // A rollback that applied at least one file with no conflicts is fully consumed.
                // A rollback where all checkpoint files were skipped (planned_files == 0) is also
                // consumed: the files were too large to store, so subsequent Latest selections
                // would loop forever returning 0 restored/deleted rather than moving on.
                (rollback.result.applied || rollback.result.planned_files == 0)
                    && rollback.result.conflicts.is_empty()
            })
            .flat_map(|rollback| rollback.result.checkpoint_ids.iter().cloned())
            .collect::<BTreeSet<_>>();
        let records = journal.checkpoints;
        let mut selected = match target {
            RollbackTarget::Latest => records
                .into_iter()
                .rev()
                .find(|record| !rolled_back.contains(&record.id))
                .into_iter()
                .collect::<Vec<_>>(),
            RollbackTarget::Group(group_id) => records
                .into_iter()
                .filter(|record| record.group_id == group_id)
                .collect::<Vec<_>>(),
            RollbackTarget::Checkpoint(id) => records
                .into_iter()
                .filter(|record| record.id == id)
                .collect::<Vec<_>>(),
        };
        selected.sort_by_key(|record| Reverse(record.created_at_ms));
        Ok(selected)
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
        // `from_for_new` maps a rename's destination path to its source path so the
        // CheckpointFile entry can record both ends in a single row.
        let mut from_for_new = BTreeMap::<String, String>::new();
        let output = self.git_vec(vec![
            "diff".to_string(),
            "--no-ext-diff".to_string(),
            "--find-renames".to_string(),
            "--name-status".to_string(),
            "-z".to_string(),
            before_tree.to_string(),
            after_tree.to_string(),
            "--".to_string(),
            ".".to_string(),
        ])?;
        let fields = nul_fields(&output.stdout);
        let mut index = 0usize;
        while index < fields.len() {
            let code = fields[index].clone();
            // Rename / copy records emit three fields: `R\d+`, old_path, new_path.
            // Everything else is two fields.
            if (code.starts_with('R') || code.starts_with('C')) && index + 2 < fields.len() {
                let old_path = fields[index + 1].clone();
                let new_path = fields[index + 2].clone();
                statuses.insert(new_path.clone(), DiffFileStatus::Renamed);
                from_for_new.insert(new_path, old_path);
                index += 3;
                continue;
            }
            if index + 1 >= fields.len() {
                break;
            }
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
            let from_path = from_for_new.get(&path).cloned();
            // For renames the "before" blob lives at the source path in the
            // before_tree, while the "after" blob is at the destination in the
            // after_tree.
            let before_lookup = from_path.as_deref().unwrap_or(path.as_str());
            let before = self.blob_bytes(before_tree, before_lookup).ok();
            let after = self.blob_bytes(after_tree, &path).ok();
            let patch = match status {
                DiffFileStatus::Renamed => match (&from_path, &before, &after) {
                    (Some(_old), Some(_), Some(_)) => self
                        .diff_patch_renamed(before_tree, after_tree, before_lookup, &path)
                        .unwrap_or_else(|_| Patch {
                            text: String::new(),
                            truncated: false,
                        }),
                    _ => Patch {
                        text: String::new(),
                        truncated: false,
                    },
                },
                _ => self.diff_patch(before_tree, after_tree, &path)?,
            };
            files.push(CheckpointFile {
                path,
                status,
                from_path,
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
                    continue;
                }
                if file.status == DiffFileStatus::Renamed {
                    if let Some(conflict) =
                        self.rollback_rename_source_conflict(record, file, &mut virtual_hashes)?
                    {
                        conflicts.push(conflict);
                        continue;
                    }
                    virtual_hashes.insert(file.path.clone(), None);
                    if let Some(from_path) = file.from_path.as_ref() {
                        virtual_hashes.insert(from_path.clone(), file.before_sha256.clone());
                    }
                } else {
                    virtual_hashes.insert(file.path.clone(), file.before_sha256.clone());
                }
            }
        }
        Ok(conflicts)
    }

    fn plan_rollback_record(
        &self,
        record: &CheckpointRecord,
        conflicts: &[RollbackConflict],
        operations: &mut Vec<RollbackOperation>,
    ) -> Result<()> {
        for file in &record.files {
            if self.file_has_rollback_conflict(record, file, conflicts) {
                continue;
            }

            if file.status == DiffFileStatus::Renamed {
                operations.push(RollbackOperation::Delete {
                    path: file.path.clone(),
                });
                if let Some(from_path) = file.from_path.as_deref()
                    && let Ok(bytes) = self.blob_bytes(&record.before_tree, from_path)
                {
                    operations.push(RollbackOperation::Write {
                        path: from_path.to_string(),
                        bytes,
                    });
                }
                continue;
            }

            match self.blob_bytes(&record.before_tree, &file.path) {
                Ok(bytes) => operations.push(RollbackOperation::Write {
                    path: file.path.clone(),
                    bytes,
                }),
                Err(_) => operations.push(RollbackOperation::Delete {
                    path: file.path.clone(),
                }),
            }
        }
        Ok(())
    }

    fn apply_operations(
        &self,
        operations: &[RollbackOperation],
        result: &mut RollbackResult,
    ) -> Result<()> {
        for operation in operations {
            self.apply_operation(operation, result)?;
        }
        Ok(())
    }

    fn apply_atomic_operations(
        &self,
        operations: &[RollbackOperation],
        result: &mut RollbackResult,
    ) -> Result<()> {
        let touched = operations
            .iter()
            .map(RollbackOperation::path)
            .collect::<BTreeSet<_>>();
        let backups = touched
            .iter()
            .map(|path| {
                let abs = self.root.join(path);
                let bytes = if abs.exists() {
                    Some(fs::read(&abs)?)
                } else {
                    None
                };
                Ok(((*path).to_string(), bytes))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut applied = RollbackResult {
            mode: result.mode,
            checkpoint_ids: result.checkpoint_ids.clone(),
            planned_files: result.planned_files,
            restored_files: Vec::new(),
            deleted_files: Vec::new(),
            conflicts: result.conflicts.clone(),
            skipped: result.skipped,
            applied: false,
        };
        if let Err(err) = self.apply_operations(operations, &mut applied) {
            let _ = self.restore_backups(&backups);
            return Err(err);
        }
        result.restored_files = applied.restored_files;
        result.deleted_files = applied.deleted_files;
        Ok(())
    }

    fn apply_operation(
        &self,
        operation: &RollbackOperation,
        result: &mut RollbackResult,
    ) -> Result<()> {
        match operation {
            RollbackOperation::Write { path, bytes } => {
                let abs = self.root.join(path);
                if let Some(parent) = abs.parent() {
                    fs::create_dir_all(parent)?;
                }
                let tmp = abs.with_extension(format!(
                    "{}.squeezy-rollback-tmp",
                    CHECKPOINT_ID_COUNTER.fetch_add(1, Ordering::SeqCst)
                ));
                fs::write(&tmp, bytes)?;
                if abs.exists() {
                    fs::remove_file(&abs)?;
                }
                fs::rename(&tmp, &abs)?;
                result.restored_files.push(path.clone());
            }
            RollbackOperation::Delete { path } => {
                let abs = self.root.join(path);
                if abs.exists() {
                    fs::remove_file(&abs)?;
                }
                result.deleted_files.push(path.clone());
            }
        }
        Ok(())
    }

    fn restore_backups(&self, backups: &[(String, Option<Vec<u8>>)]) -> Result<()> {
        for (path, bytes) in backups {
            let abs = self.root.join(path);
            match bytes {
                Some(bytes) => {
                    if let Some(parent) = abs.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::write(abs, bytes)?;
                }
                None => {
                    if abs.exists() {
                        fs::remove_file(abs)?;
                    }
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
        let before_path = file.from_path.as_deref().unwrap_or(&file.path);
        if self.tree_has_path(&record.before_tree, before_path)? {
            let blob = self.blob_bytes(&record.before_tree, before_path);
            if blob.is_err() {
                return Ok(Some(RollbackConflict {
                    checkpoint_id: record.id.clone(),
                    path: before_path.to_string(),
                    expected_sha256: file.after_sha256.clone(),
                    current_sha256,
                    reason: "checkpoint object is missing; leaving current content untouched"
                        .to_string(),
                }));
            }
        }
        Ok(None)
    }

    fn rollback_rename_source_conflict(
        &self,
        record: &CheckpointRecord,
        file: &CheckpointFile,
        virtual_hashes: &mut BTreeMap<String, Option<String>>,
    ) -> Result<Option<RollbackConflict>> {
        let Some(from_path) = file.from_path.as_ref() else {
            return Ok(None);
        };
        let current_sha256 = match virtual_hashes.get(from_path) {
            Some(hash) => hash.clone(),
            None => {
                let path = self.root.join(from_path);
                let hash = if path.exists() {
                    Some(sha256_hex(&fs::read(&path)?))
                } else {
                    None
                };
                virtual_hashes.insert(from_path.clone(), hash.clone());
                hash
            }
        };
        if current_sha256.is_some() {
            return Ok(Some(RollbackConflict {
                checkpoint_id: record.id.clone(),
                path: from_path.clone(),
                expected_sha256: None,
                current_sha256,
                reason: "file exists at rename source path; rollback would overwrite it"
                    .to_string(),
            }));
        }
        Ok(None)
    }

    fn file_has_rollback_conflict(
        &self,
        record: &CheckpointRecord,
        file: &CheckpointFile,
        conflicts: &[RollbackConflict],
    ) -> bool {
        conflicts.iter().any(|conflict| {
            conflict.checkpoint_id == record.id
                && (conflict.path == file.path || file.from_path.as_ref() == Some(&conflict.path))
        })
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
            if let Some(parent) = self.git_dir.parent() {
                fs::create_dir_all(parent)?;
            }
            self.init_shadow_git_dir()?;
            self.git_raw(["config", "core.autocrlf", "false"])?;
            self.git_raw(["config", "core.fsmonitor", "false"])?;
            self.git_raw(["config", "core.quotepath", "false"])?;
            // The shadow repo must not trigger user-configured hooks, GPG signing,
            // or commit-graph regeneration: those are tied to the user's worktree
            // semantics, not Squeezy's internal checkpoint book-keeping.
            self.git_raw(["config", "core.hooksPath", hooks_off_value()])?;
            self.git_raw(["config", "commit.gpgsign", "false"])?;
            self.git_raw(["config", "core.commitGraph", "false"])?;
        }
        if !exclude_path.exists() {
            if let Some(parent) = exclude_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&exclude_path, "/.squeezy/\n")?;
        }
        Ok(())
    }

    fn init_shadow_git_dir(&self) -> Result<()> {
        git_output_vec_allow_status(
            &self.root,
            vec![
                "init".to_string(),
                "--bare".to_string(),
                self.git_dir.to_string_lossy().to_string(),
            ],
            &[0],
        )
        .map(|_| ())
        .map_err(SqueezyError::Tool)
    }

    fn large_file_fingerprints(&self) -> Result<Vec<LargeFileFingerprint>> {
        let mut files = Vec::new();
        self.collect_large_file_fingerprints(&self.root, &mut files)?;
        files.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(files)
    }

    fn collect_large_file_fingerprints(
        &self,
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
                if self.is_git_ignored(&path)? {
                    continue;
                }
                self.collect_large_file_fingerprints(&path, files)?;
            } else if metadata.is_file() && metadata.len() > self.options.max_file_bytes {
                if self.is_git_ignored(&path)? {
                    continue;
                }
                let (mtime_secs, mtime_nanos) = mtime_parts(&metadata);
                files.push(LargeFileFingerprint {
                    path: rel_path(&self.root, &path),
                    size_bytes: metadata.len(),
                    mtime_secs,
                    mtime_nanos,
                });
            }
        }
        Ok(())
    }

    fn is_git_ignored(&self, path: &Path) -> Result<bool> {
        let rel = rel_path(&self.root, path);
        let output = self.git_vec_allow_status(
            vec![
                "check-ignore".to_string(),
                "--quiet".to_string(),
                "--".to_string(),
                rel,
            ],
            &[0, 1],
        )?;
        Ok(output.status.code() == Some(0))
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
        self.rewrite_checkpoint_journal_with_rollbacks(&keep, &journal.rollbacks)?;
        let _ = self.git(["gc", "--prune=now"]);
        Ok(())
    }

    fn cleanup_old_checkpoints_if_due(&self) -> Result<()> {
        if self.options.cleanup_interval_secs == 0 {
            return self.cleanup_old_checkpoints(self.options.retention_days);
        }
        let marker = self
            .journal_path
            .parent()
            .unwrap_or(&self.root)
            .join("last-cleanup");
        let fresh = fs::metadata(&marker)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|elapsed| elapsed.as_secs() < self.options.cleanup_interval_secs);
        if fresh {
            return Ok(());
        }
        self.cleanup_old_checkpoints(self.options.retention_days)?;
        let _ = fs::write(marker, now_ms().to_string());
        Ok(())
    }

    #[cfg(test)]
    fn rewrite_checkpoint_journal(&self, records: &[CheckpointRecord]) -> Result<()> {
        self.rewrite_checkpoint_journal_with_rollbacks(records, &[])
    }

    fn rewrite_checkpoint_journal_with_rollbacks(
        &self,
        records: &[CheckpointRecord],
        rollbacks: &[RollbackJournalRecord],
    ) -> Result<()> {
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
        for rollback in rollbacks {
            serde_json::to_writer(
                &mut file,
                &json!({
                    "kind": "rollback",
                    "created_at_ms": rollback.created_at_ms,
                    "result": rollback.result,
                }),
            )
            .map_err(|err| {
                SqueezyError::Tool(format!("failed to rewrite checkpoint journal: {err}"))
            })?;
            file.write_all(b"\n")?;
        }
        Ok(())
    }

    fn diff_patch_renamed(
        &self,
        before_tree: &str,
        after_tree: &str,
        from_path: &str,
        to_path: &str,
    ) -> Result<Patch> {
        let output = self.git_vec_allow_status(
            vec![
                "diff".to_string(),
                "--patch".to_string(),
                "--no-ext-diff".to_string(),
                "--find-renames".to_string(),
                "--unified=3".to_string(),
                before_tree.to_string(),
                after_tree.to_string(),
                "--".to_string(),
                from_path.to_string(),
                to_path.to_string(),
            ],
            &[0],
        )?;
        Ok(capped_patch(output.stdout, DEFAULT_MAX_PATCH_BYTES))
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
            std::iter::once("--bare".to_string())
                .chain(std::iter::once("--git-dir".to_string()))
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

#[derive(Debug, Clone)]
enum RollbackOperation {
    Write { path: String, bytes: Vec<u8> },
    Delete { path: String },
}

impl RollbackOperation {
    fn path(&self) -> &str {
        match self {
            RollbackOperation::Write { path, .. } | RollbackOperation::Delete { path } => path,
        }
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
    let digest = hasher.finalize();
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        push_hex_byte(&mut output, byte);
    }
    output
}

fn push_hex_byte(output: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    output.push(HEX[(byte >> 4) as usize] as char);
    output.push(HEX[(byte & 0x0f) as usize] as char);
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

fn hooks_off_value() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
}

/// Acquire an OS-advisory exclusive lock on the per-workspace shadow-repo
/// lock file. Returns the open [`File`] so the caller can hold the lock by
/// keeping the handle alive; dropping the file releases the lock.
///
/// The lock file body contains `<pid>\n<unix_millis>\n` for human / log
/// diagnostics — neither field is consulted for correctness, the OS lock
/// is the source of truth.
fn acquire_shadow_lock(lock_path: &Path) -> Result<File> {
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            return Err(SqueezyError::Tool(format!(
                "another squeezy process is holding the shadow-repo lock at {} \
                 — start one squeezy per workspace or wait for it to exit",
                lock_path.display()
            )));
        }
        Err(TryLockError::Error(err)) => {
            return Err(SqueezyError::Tool(format!(
                "failed to acquire shadow-repo lock at {}: {err}",
                lock_path.display()
            )));
        }
    }
    // Best-effort PID/timestamp diagnostics. A failure here must not
    // release the lock we just took, so we log + continue.
    let body = format!("{}\n{}\n", std::process::id(), now_ms());
    if let Ok(mut handle) = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(lock_path)
    {
        let _ = handle.write_all(body.as_bytes());
    }
    Ok(file)
}

/// Remove orphan entries inside `.squeezy/checkpoints/` that are older
/// than `retention_days`. The active shadow `git/` repo, the journal file,
/// and the current lock file are always preserved; everything else (stray
/// scratch directories, abandoned `.lock` files from crashed processes,
/// stale temporaries) is swept so a long-running workspace does not
/// accumulate untracked bookkeeping.
///
/// Failures are intentionally swallowed: cleanup is best-effort and must
/// never prevent the store from opening.
fn cleanup_stale_shadow_dirs(checkpoints_dir: &Path, retention_days: u64) {
    let Ok(entries) = fs::read_dir(checkpoints_dir) else {
        return;
    };
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(retention_days * 24 * 60 * 60))
        .unwrap_or(UNIX_EPOCH);
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if matches!(name, "git" | "journal.jsonl" | SHADOW_LOCK_FILENAME) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if modified >= cutoff {
            continue;
        }
        if metadata.is_dir() {
            let _ = fs::remove_dir_all(&path);
        } else {
            let _ = fs::remove_file(&path);
        }
    }
}

impl Drop for CheckpointStore {
    fn drop(&mut self) {
        // Release the OS lock first by dropping the file handle, then
        // remove the lock file so a fresh process opening the same
        // workspace does not have to inspect a stale sentinel.
        self._lock.take();
        let _ = fs::remove_file(&self.lock_path);
    }
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
        let mut parts = record.splitn(3, '\t');
        let Some(additions) = parts.next() else {
            continue;
        };
        let Some(deletions) = parts.next() else {
            continue;
        };
        let Some(path) = parts.next() else {
            continue;
        };
        if path.is_empty() {
            continue;
        }
        let binary = additions == "-" || deletions == "-";
        output.insert(
            path.to_string(),
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

/// Split combined `git diff --patch` output into per-file slices. The
/// stream looks like:
///
/// ```text
/// diff --git a/foo b/foo
/// index ...
/// --- a/foo
/// +++ b/foo
/// @@ ...
/// diff --git a/bar b/bar
/// ...
/// ```
///
/// Each `diff --git a/<path> b/<path>` line opens a new file section
/// that runs until the next such line (or EOF). We match the trailing
/// `b/<path>` against `expected_files` so paths with embedded spaces
/// still resolve correctly — git omits the trailing whitespace.
fn split_unified_patch(
    bytes: &[u8],
    expected_files: &[String],
    max_bytes: usize,
) -> BTreeMap<String, Patch> {
    let text = String::from_utf8_lossy(bytes);
    let expected: BTreeSet<&str> = expected_files.iter().map(String::as_str).collect();
    let mut out: BTreeMap<String, Patch> = BTreeMap::new();
    let mut current_path: Option<String> = None;
    let mut buffer = String::new();
    let flush = |path: Option<String>, buffer: &mut String, out: &mut BTreeMap<String, Patch>| {
        if let Some(path) = path {
            let truncated = buffer.len() > max_bytes;
            let text = if truncated {
                buffer[..max_bytes].to_string()
            } else {
                std::mem::take(buffer)
            };
            out.insert(path, Patch { text, truncated });
        }
        buffer.clear();
    };
    for line in text.split_inclusive('\n') {
        if let Some(rest) = line.strip_prefix("diff --git a/")
            && let Some(path) = extract_diff_git_path(rest, &expected)
        {
            flush(current_path.take(), &mut buffer, &mut out);
            current_path = Some(path);
        }
        buffer.push_str(line);
    }
    flush(current_path, &mut buffer, &mut out);
    out
}

/// Extract the `b/<path>` half of a `diff --git a/<path> b/<path>` line,
/// matching against the known file list to handle paths with embedded
/// spaces. The input is the suffix that follows `diff --git a/`.
fn extract_diff_git_path<'a>(suffix: &'a str, expected: &BTreeSet<&'a str>) -> Option<String> {
    let trimmed = suffix.trim_end_matches('\n').trim_end_matches('\r');
    // The canonical no-spaces form: `<path> b/<path>`. Try it first.
    if let Some(idx) = trimmed.find(" b/") {
        let candidate = &trimmed[..idx];
        if expected.contains(candidate) {
            return Some(candidate.to_string());
        }
    }
    // Fallback for paths with spaces: scan every ` b/<known-path>` suffix.
    for &path in expected {
        if let Some(prefix) = trimmed.strip_suffix(path)
            && let Some(prefix) = prefix.strip_suffix(" b/")
            && prefix == path
        {
            return Some(path.to_string());
        }
    }
    None
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

/// Walk an `apply_patch` JSON payload and emit one [`PatchOpPreview`] per op
/// in payload order.
///
/// Used by the TUI to render an incremental preview while the model is still
/// streaming tool-arg tokens: callers feed the fully-parsed payload (or a
/// snapshot of the partial payload that has reached a `{...}` boundary) and
/// the closure is invoked synchronously once per op the walker recognises.
/// `sink` is the stream surface — collect, throttle, or push onto a channel
/// from inside the closure. Returns the number of ops emitted.
///
/// The payload is the same JSON shape `apply_patch` itself consumes. Both
/// the multi-op `operations` array (preferred) and the legacy `patches`
/// array of search/replace blocks are recognised. Unknown fields are
/// ignored so a partial JSON snapshot that omits optional keys still
/// produces a usable preview. Returns `Err` only when the payload is not
/// parseable as JSON.
pub fn preview_patch_stream<F>(payload: &str, mut sink: F) -> Result<usize>
where
    F: FnMut(PatchOpPreview),
{
    let value: serde_json::Value = serde_json::from_str(payload)
        .map_err(|err| SqueezyError::Tool(format!("apply_patch payload not valid JSON: {err}")))?;
    let mut index = 0usize;
    if let Some(operations) = value.get("operations").and_then(|ops| ops.as_array()) {
        for op in operations {
            if let Some(preview) = op_preview_from_operation(index, op) {
                sink(preview);
                index += 1;
            }
        }
    }
    if let Some(patches) = value.get("patches").and_then(|patches| patches.as_array()) {
        for patch in patches {
            if let Some(preview) = op_preview_from_search_replace(index, patch) {
                sink(preview);
                index += 1;
            }
        }
    }
    Ok(index)
}

fn op_preview_from_operation(index: usize, op: &serde_json::Value) -> Option<PatchOpPreview> {
    let kind = op.get("kind").and_then(|kind| kind.as_str())?;
    match kind {
        "search_replace" => {
            let path = op.get("path").and_then(|path| path.as_str())?.to_string();
            Some(PatchOpPreview {
                index,
                kind: PatchOpKind::SearchReplace,
                path,
                from_path: None,
                search_hash: op
                    .get("search")
                    .and_then(|value| value.as_str())
                    .map(|text| sha256_hex(text.as_bytes())),
                replace_hash: op
                    .get("replace")
                    .and_then(|value| value.as_str())
                    .map(|text| sha256_hex(text.as_bytes())),
                contents_hash: None,
            })
        }
        "create_file" => {
            let path = op.get("path").and_then(|path| path.as_str())?.to_string();
            Some(PatchOpPreview {
                index,
                kind: PatchOpKind::CreateFile,
                path,
                from_path: None,
                search_hash: None,
                replace_hash: None,
                contents_hash: op
                    .get("contents")
                    .and_then(|value| value.as_str())
                    .map(|text| sha256_hex(text.as_bytes())),
            })
        }
        "delete_file" => {
            let path = op.get("path").and_then(|path| path.as_str())?.to_string();
            Some(PatchOpPreview {
                index,
                kind: PatchOpKind::DeleteFile,
                path,
                from_path: None,
                search_hash: None,
                replace_hash: None,
                contents_hash: None,
            })
        }
        "move_file" => {
            let from = op.get("from").and_then(|from| from.as_str())?.to_string();
            let to = op.get("to").and_then(|to| to.as_str())?.to_string();
            Some(PatchOpPreview {
                index,
                kind: PatchOpKind::MoveFile,
                path: to,
                from_path: Some(from),
                search_hash: None,
                replace_hash: None,
                contents_hash: None,
            })
        }
        _ => None,
    }
}

fn op_preview_from_search_replace(
    index: usize,
    patch: &serde_json::Value,
) -> Option<PatchOpPreview> {
    let path = patch
        .get("path")
        .and_then(|path| path.as_str())?
        .to_string();
    Some(PatchOpPreview {
        index,
        kind: PatchOpKind::SearchReplace,
        path,
        from_path: None,
        search_hash: patch
            .get("search")
            .and_then(|value| value.as_str())
            .map(|text| sha256_hex(text.as_bytes())),
        replace_hash: patch
            .get("replace")
            .and_then(|value| value.as_str())
            .map(|text| sha256_hex(text.as_bytes())),
        contents_hash: None,
    })
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

fn nonzero_u64(value: u64, fallback: u64) -> u64 {
    if value == 0 { fallback } else { value }
}

/// `true` when every non-empty line of `git add` stderr is part of the
/// "paths are ignored by one of your .gitignore files" advisory.
///
/// The advisory body is: a header line, the listed paths (one per line,
/// flush-left, no leading whitespace), and zero or more `hint: …` lines
/// suggesting `-f` or `git config advice.addIgnoredFile false`. When that
/// is the entire stderr, the add already succeeded for every non-excluded
/// path and the non-zero exit is purely informational.
///
/// Any `fatal:` / `error:` / unknown diagnostic line — even mixed in after
/// the header — causes this to return `false` so the real error still
/// propagates through [`SqueezyError::Tool`].
fn is_add_ignored_advisory_only(stderr: &str) -> bool {
    let mut saw_header = false;
    for raw_line in stderr.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("hint:") {
            continue;
        }
        if line == "The following paths are ignored by one of your .gitignore files:" {
            saw_header = true;
            continue;
        }
        if !saw_header {
            return false;
        }
        // After the header, accept only listed pathspecs: literal path
        // tokens with no whitespace. Anything else (e.g. `fatal: …`,
        // `error: …`, a stray `warning: …`) is a real diagnostic and must
        // propagate.
        if line.contains(char::is_whitespace) {
            return false;
        }
        if line.starts_with("fatal:") || line.starts_with("error:") {
            return false;
        }
    }
    saw_header
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
