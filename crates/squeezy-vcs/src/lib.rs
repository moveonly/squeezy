use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fs::{self, File, OpenOptions, TryLockError},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
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

mod git_command;
pub mod worktree;
use git_command::{
    git_output, git_output_allow_status, git_output_vec_allow_status,
    git_output_vec_with_stdin_allow_status, git_text, hooks_off_value,
    is_add_ignored_advisory_only,
};
pub use worktree::{Worktree, WorktreeCleanup, validate_worktree_slug};

pub const CRATE_NAME: &str = "squeezy-vcs";
const DEFAULT_MAX_PATCH_BYTES: usize = 1_000_000;
const DEFAULT_CHECKPOINT_RETENTION_DAYS: u64 = 7;
const DEFAULT_MAX_CHECKPOINT_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Upper bound on how many sibling-tempfile / sibling-symlink / sibling-
/// hardlink candidates the rollback path will try before giving up. Each
/// `create_sibling_*` helper draws a fresh `(pid, counter)` candidate per
/// attempt; in practice the first attempt almost always succeeds because
/// the counter is process-wide and monotonic. The cap is here to bound
/// the loop in the pathological case where another process is racing the
/// rollback (e.g. running `cleanup` while we restore) and to surface a
/// clean `Tool` error instead of spinning indefinitely.
const MAX_RESTORE_TEMPFILE_ATTEMPTS: usize = 128;
const SHADOW_LOCK_FILENAME: &str = "shadow.lock";
const SHADOW_LAST_CLEANUP_FILENAME: &str = "last-cleanup";
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
    raw_blob_dir: PathBuf,
    journal_path: PathBuf,
    lock_path: PathBuf,
    raw_cache: Mutex<BTreeMap<String, CachedRawFile>>,
    cleanup_last_run_ms: Mutex<Option<u128>>,
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
    pub hardlinks: BTreeMap<String, Vec<String>>,
    pub raw_files: BTreeMap<String, RawFileSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LargeFileFingerprint {
    pub path: String,
    pub size_bytes: u64,
    pub mtime_secs: i64,
    pub mtime_nanos: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawFileSnapshot {
    pub sha256: String,
    pub size_bytes: u64,
    pub mtime_secs: i64,
    pub mtime_nanos: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CachedRawFile {
    size_bytes: u64,
    mtime_secs: i64,
    mtime_nanos: u32,
    sha256: String,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_file_type: Option<CheckpointFileType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_file_type: Option<CheckpointFileType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_symlink_target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_symlink_target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_hardlink_paths: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_hardlink_paths: Option<Vec<String>>,
    pub before_sha256: Option<String>,
    pub after_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_worktree_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_worktree_sha256: Option<String>,
    pub additions: u64,
    pub deletions: u64,
    pub binary: bool,
    pub patch: Option<String>,
    pub patch_truncated: bool,
}

impl CheckpointFile {
    fn before_entry_state(&self) -> WorkspaceEntryState {
        WorkspaceEntryState {
            sha256: self
                .before_worktree_sha256
                .clone()
                .or_else(|| self.before_sha256.clone()),
            file_type: self.before_file_type,
            mode: self.before_mode.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointFileType {
    RegularFile,
    Symlink,
    Other,
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
    #[serde(default)]
    pub file_actions: Vec<RollbackFileAction>,
    pub conflicts: Vec<RollbackConflict>,
    pub skipped: bool,
    pub applied: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackFileAction {
    pub checkpoint_id: String,
    pub path: String,
    pub action: RollbackFileActionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_type: Option<CheckpointFileType>,
    pub verified_after_rollback: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollbackFileActionKind {
    RestoreRegular,
    RestoreSymlink,
    RestoreHardlink,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackJournalRecord {
    pub created_at_ms: u128,
    pub result: RollbackResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointIntegrityReport {
    pub ok: bool,
    /// `true` when no malformed journal lines were observed during
    /// recovery. Decoupled from [`Self::ok`] so a workspace upgraded
    /// from a previous squeezy version with a stray malformed line
    /// does not look like an integrity failure when every ref and
    /// blob is present.
    #[serde(default = "default_journal_clean")]
    pub journal_clean: bool,
    pub checkpoints_checked: usize,
    pub journal_warnings: u64,
    pub missing_refs: Vec<CheckpointIntegrityProblem>,
    pub missing_objects: Vec<CheckpointIntegrityProblem>,
}

fn default_journal_clean() -> bool {
    true
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_hash_basis: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_hash_basis: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<RollbackConflictReason>,
    #[serde(default)]
    pub retryable: bool,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollbackConflictReason {
    WorktreeChanged,
    GitFilterOrEolMismatch,
    CheckpointObjectMissing,
    AccessDenied,
    PermissionDenied,
    WouldBlock,
    ReadOnly,
    FileInUse,
    ReparsePoint,
    Filesystem,
    ShadowRefreshFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointDoctorReport {
    pub platform: String,
    pub workspace_root: String,
    pub workspace_root_slash: String,
    pub shadow_git_dir: String,
    pub shadow_git_dir_slash: String,
    pub git_path_mode: String,
    pub core_autocrlf: Option<String>,
    pub core_ignorecase: Option<String>,
    pub core_longpaths: Option<String>,
    pub gitattributes: Vec<String>,
    pub checkpoints_dir_writable: bool,
    pub protected_ref_roundtrip: bool,
    pub smoke: CheckpointSmokeReport,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointSmokeReport {
    pub ran: bool,
    pub passed: bool,
    pub crlf_preserved: bool,
    pub git_filter_or_eol_mismatch_detected: bool,
    pub error: Option<String>,
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
        let raw_blob_dir = dir.join("raw-blobs");
        let journal_path = dir.join("journal.jsonl");
        let lock_path = dir.join(SHADOW_LOCK_FILENAME);
        fs::create_dir_all(&git_dir)?;
        fs::create_dir_all(&raw_blob_dir)?;
        // Acquire the per-workspace shadow-repo lock before any cleanup or
        // git work — two squeezy processes pointed at the same workspace
        // must not race on `git add --all` + `write-tree`.
        let lock = acquire_shadow_lock(&lock_path)?;
        cleanup_stale_shadow_dirs(&dir, SHADOW_STALE_DIR_RETENTION_DAYS);
        let store = Self {
            root,
            git_dir,
            raw_blob_dir,
            journal_path,
            lock_path,
            raw_cache: Mutex::new(BTreeMap::new()),
            cleanup_last_run_ms: Mutex::new(None),
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
        let WorkspaceFileFingerprints {
            large_files,
            raw_files,
        } = self.workspace_file_fingerprints()?;
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
        let hardlinks = self.hardlink_groups()?;
        let output = self.git(["write-tree"])?;
        let tree = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(WorkspaceSnapshot {
            tree,
            large_files,
            hardlinks,
            raw_files,
        })
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
        let changed_raw_paths = diff_raw_files(&before.raw_files, &after.raw_files);
        if before.tree == after.tree
            && changed_large_paths.is_empty()
            && changed_raw_paths.is_empty()
        {
            return Ok(None);
        }
        let (files, skipped_files) = self.checkpoint_files(
            &before.tree,
            &after.tree,
            &before.hardlinks,
            &after.hardlinks,
            &before.raw_files,
            &after.raw_files,
            &after.large_files,
            &changed_large_paths,
            &changed_raw_paths,
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

    pub fn doctor(&self) -> Result<CheckpointDoctorReport> {
        self.ensure_shadow_repo()?;
        let mut warnings = Vec::new();
        let snapshot = match self.track_tree() {
            Ok(snapshot) => snapshot,
            Err(err) => {
                warnings.push(format!("no-op shadow snapshot failed: {err}"));
                WorkspaceSnapshot {
                    tree: String::new(),
                    large_files: Vec::new(),
                    hardlinks: BTreeMap::new(),
                    raw_files: BTreeMap::new(),
                }
            }
        };
        let protected_ref_roundtrip = if snapshot.tree.is_empty() {
            false
        } else {
            // Use a dedicated `refs/squeezy/doctor/` namespace so a leaked
            // probe ref (e.g. when the second `update-ref -d` fails) is
            // distinguishable from a real checkpoint ref. A future doctor
            // run can sweep this namespace for orphans without risking
            // genuine `refs/squeezy/checkpoints/<id>` refs.
            let probe_ref = doctor_probe_ref(now_ms());
            let created = self.git_vec(vec![
                "update-ref".to_string(),
                probe_ref.clone(),
                snapshot.tree.clone(),
            ]);
            let deleted = self.git_vec(vec![
                "update-ref".to_string(),
                "-d".to_string(),
                probe_ref.clone(),
            ]);
            if let Err(err) = &created {
                warnings.push(format!(
                    "protected-ref create failed for {probe_ref}: {err}"
                ));
            }
            if created.is_ok()
                && let Err(err) = &deleted
            {
                warnings.push(format!(
                    "protected-ref delete failed for {probe_ref}: {err}"
                ));
            }
            created.is_ok() && deleted.is_ok()
        };
        // Test writability via a probe file in the checkpoints directory rather
        // than re-opening the lock file — the same process already holds the
        // lock, so reopening it always succeeds and tells us nothing useful.
        // The field name reflects what is actually probed.
        let checkpoints_dir = self.lock_path.parent().unwrap_or(&self.root);
        let probe_path = checkpoints_dir.join(".squeezy-doctor-probe");
        let checkpoints_dir_writable = fs::write(&probe_path, b"").is_ok();
        let _ = fs::remove_file(&probe_path);
        if !checkpoints_dir_writable {
            warnings.push(format!(
                "checkpoints directory is not writable: {}",
                checkpoints_dir.display()
            ));
        }
        let gitattributes = collect_gitattributes(&self.root);
        if !gitattributes.is_empty() {
            warnings.push(
                "workspace has .gitattributes; checkpoint rollback uses worktree byte hashes \
                 for safety and keeps Git blob hashes for object diagnostics"
                    .to_string(),
            );
        }
        let smoke = run_checkpoint_smoke();
        if !smoke.passed {
            warnings.push(format!(
                "checkpoint smoke failed: {}",
                smoke.error.as_deref().unwrap_or("unknown error")
            ));
        }
        Ok(CheckpointDoctorReport {
            platform: std::env::consts::OS.to_string(),
            workspace_root: self.root.to_string_lossy().to_string(),
            workspace_root_slash: slash_path(&self.root),
            shadow_git_dir: self.git_dir.to_string_lossy().to_string(),
            shadow_git_dir_slash: slash_path(&self.git_dir),
            git_path_mode: if cfg!(windows) {
                "windows-legacy"
            } else {
                "native"
            }
            .to_string(),
            core_autocrlf: self.git_config_value("core.autocrlf"),
            core_ignorecase: self.git_config_value("core.ignorecase"),
            core_longpaths: self.git_config_value("core.longpaths"),
            gitattributes,
            checkpoints_dir_writable,
            protected_ref_roundtrip,
            smoke,
            warnings,
        })
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
                file_actions: Vec::new(),
                conflicts: Vec::new(),
                skipped: true,
                applied: false,
            });
        }
        let conflicts = self.preflight_conflicts(&selected)?;
        let planned_files = selected.iter().map(|record| record.files.len()).sum();
        let mut result = RollbackResult {
            mode,
            checkpoint_ids: selected.iter().map(|record| record.id.clone()).collect(),
            planned_files,
            restored_files: Vec::new(),
            deleted_files: Vec::new(),
            file_actions: Vec::new(),
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
        result
            .conflicts
            .extend(self.preflight_filesystem_conflicts(&selected, &result.conflicts)?);
        if mode == RollbackMode::Atomic && !result.conflicts.is_empty() {
            self.append_journal(json!({
                "kind": "rollback",
                "created_at_ms": now_ms(),
                "result": result,
            }))?;
            return Ok(result);
        }
        let backups = if mode == RollbackMode::Atomic {
            Some(self.backup_rollback_paths(&selected, &result)?)
        } else {
            None
        };
        let applied_result: Result<()> = (|| {
            for record in &selected {
                self.rollback_record(record, &mut result)?;
            }
            Ok(())
        })();
        if let Err(err) = applied_result {
            if let Some(backups) = backups.as_ref() {
                let _ = self.restore_rollback_backups(backups);
            }
            return Err(err);
        }
        // `applied` reports whether the rollback finished as planned. A pass
        // that emitted at least one per-file action clearly applied, but a
        // no-op pass with no conflicts is also a success and should stay
        // `applied` rather than tricking the tool layer into reporting `Stale`.
        result.applied = !result.file_actions.is_empty() || result.conflicts.is_empty();
        if result.applied
            && let Err(err) = self.track_tree()
        {
            Self::record_shadow_refresh_failure(&mut result, err);
        }
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
                for path in rollback_write_paths(&file) {
                    paths.insert(path);
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
                file_actions: Vec::new(),
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
                file_actions: Vec::new(),
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
            file_actions: Vec::new(),
            conflicts,
            skipped: false,
            applied: false,
        };
        if mode == RollbackMode::Atomic && !result.conflicts.is_empty() {
            return Ok(result);
        }
        let backups = if mode == RollbackMode::Atomic {
            Some(self.backup_rollback_paths(&selected, &result)?)
        } else {
            None
        };
        let applied_result: Result<()> = (|| {
            self.rollback_record(&selected[0], &mut result)?;
            Ok(())
        })();
        if let Err(err) = applied_result {
            if let Some(backups) = backups.as_ref() {
                let _ = self.restore_rollback_backups(backups);
            }
            return Err(err);
        }
        result.applied = !result.file_actions.is_empty() || result.conflicts.is_empty();
        if result.applied
            && let Err(err) = self.track_tree()
        {
            Self::record_shadow_refresh_failure(&mut result, err);
        }
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
            for path in rollback_write_paths(file) {
                paths.insert(path);
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
                if !self.checkpoint_ref_exists(&reference)? {
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
                    && !self.checkpoint_blob_exists(&record.before_tree, before_path)?
                {
                    missing_objects.push(CheckpointIntegrityProblem {
                        checkpoint_id: record.id.clone(),
                        path: Some(before_path.to_string()),
                        side: "before".to_string(),
                        reason: "before blob is missing".to_string(),
                    });
                }
                if file.after_sha256.is_some()
                    && !self.checkpoint_blob_exists(&record.after_tree, &file.path)?
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
        // `journal_clean` is the structural-purity bit: zero malformed
        // journal lines. `ok` stays the unified storage-integrity bit so
        // legacy callers see the same field, but it no longer flips
        // false on `journal_warnings` alone — a single malformed line
        // from a previous squeezy version should not look like a check
        // failure when every ref and blob is present.
        Ok(CheckpointIntegrityReport {
            ok: missing_refs.is_empty() && missing_objects.is_empty(),
            journal_clean: journal.journal_warnings == 0,
            checkpoints_checked: journal.checkpoints.len(),
            journal_warnings: journal.journal_warnings,
            missing_refs,
            missing_objects,
        })
    }

    fn checkpoint_ref_exists(&self, reference: &str) -> Result<bool> {
        Ok(self
            .git_vec_allow_status(
                vec![
                    "show-ref".to_string(),
                    "--verify".to_string(),
                    "--quiet".to_string(),
                    reference.to_string(),
                ],
                &[0, 1],
            )?
            .status
            .code()
            == Some(0))
    }

    fn checkpoint_blob_exists(&self, tree: &str, path: &str) -> Result<bool> {
        // `cat-file -e` exits 0 when the object exists and 1 otherwise
        // without streaming the blob contents, so this scales to
        // workspaces with many large checkpoint blobs without paying
        // the read-cost just to test existence.
        Ok(self
            .git_vec_allow_status(
                vec![
                    "cat-file".to_string(),
                    "-e".to_string(),
                    format!("{tree}:{path}"),
                ],
                &[0, 1],
            )?
            .status
            .code()
            == Some(0))
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
        // Apply the consumed-rollback filter to every target. Without it,
        // `/revert-turn <group_id>` immediately after `/undo` re-selects
        // a checkpoint whose only journal entry already marks it
        // restored, then surfaces a sha256 conflict (current bytes match
        // `before_sha256`, not `after_sha256`) instead of returning a
        // clean "nothing in this group still pending" result.
        let mut selected = match target {
            RollbackTarget::Latest => records
                .into_iter()
                .rev()
                .find(|record| !rolled_back.contains(&record.id))
                .into_iter()
                .collect::<Vec<_>>(),
            RollbackTarget::Group(group_id) => records
                .into_iter()
                .filter(|record| record.group_id == group_id && !rolled_back.contains(&record.id))
                .collect::<Vec<_>>(),
            RollbackTarget::Checkpoint(id) => records
                .into_iter()
                .filter(|record| record.id == id && !rolled_back.contains(&record.id))
                .collect::<Vec<_>>(),
        };
        selected.sort_by_key(|record| Reverse(record.created_at_ms));
        Ok(selected)
    }

    #[allow(clippy::too_many_arguments)]
    fn checkpoint_files(
        &self,
        before_tree: &str,
        after_tree: &str,
        before_hardlinks: &BTreeMap<String, Vec<String>>,
        after_hardlinks: &BTreeMap<String, Vec<String>>,
        before_raw: &BTreeMap<String, RawFileSnapshot>,
        after_raw: &BTreeMap<String, RawFileSnapshot>,
        large_after: &[LargeFileFingerprint],
        changed_large_paths: &[String],
        changed_raw_paths: &[String],
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
        for path in changed_raw_paths {
            // Raw-path tracking still lists rename sources even though git
            // name-status already collapsed the pair into one Renamed row.
            if from_for_new.values().any(|source| source == path) {
                continue;
            }
            statuses.entry(path.clone()).or_insert_with(|| {
                if before_raw.contains_key(path) && !after_raw.contains_key(path) {
                    DiffFileStatus::Deleted
                } else if !before_raw.contains_key(path) && after_raw.contains_key(path) {
                    DiffFileStatus::Added
                } else {
                    DiffFileStatus::Modified
                }
            });
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
            let before_entry = self.tree_entry(before_tree, before_lookup)?;
            let after_entry = self.tree_entry(after_tree, &path)?;
            let before_hardlink_paths = hardlink_paths_for(before_hardlinks, before_lookup);
            let after_hardlink_paths = hardlink_paths_for(after_hardlinks, &path);
            let before_worktree_sha256 = before_raw
                .get(before_lookup)
                .map(|file| file.sha256.clone());
            let after_worktree_sha256 = after_raw.get(&path).map(|file| file.sha256.clone());
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
                before_file_type: before_entry.as_ref().map(TreeEntry::checkpoint_file_type),
                after_file_type: after_entry.as_ref().map(TreeEntry::checkpoint_file_type),
                before_mode: before_entry.as_ref().map(|entry| entry.mode.clone()),
                after_mode: after_entry.as_ref().map(|entry| entry.mode.clone()),
                before_symlink_target: before_entry
                    .as_ref()
                    .filter(|entry| entry.is_symlink())
                    .and(before.as_deref())
                    .map(symlink_target_display),
                after_symlink_target: after_entry
                    .as_ref()
                    .filter(|entry| entry.is_symlink())
                    .and(after.as_deref())
                    .map(symlink_target_display),
                before_hardlink_paths,
                after_hardlink_paths,
                before_sha256: before.as_deref().map(sha256_hex),
                after_sha256: after.as_deref().map(sha256_hex),
                before_worktree_sha256,
                after_worktree_sha256,
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
        let mut virtual_states = BTreeMap::<String, WorkspaceEntryState>::new();
        let mut planned_paths = BTreeSet::new();
        for record in records {
            for file in &record.files {
                planned_paths.insert(file.path.clone());
                if let Some(from_path) = file.from_path.as_deref() {
                    planned_paths.insert(from_path.to_string());
                }
            }
        }
        for record in records {
            for file in &record.files {
                let identity = path_identity_key(&file.path);
                let path = safe_workspace_path(&self.root, &file.path)?;
                let restoring_symlink = file.before_file_type == Some(CheckpointFileType::Symlink);
                if let Some(conflict) = reparse_path_conflict(
                    &record.id,
                    &file.path,
                    &self.root,
                    &path,
                    restoring_symlink,
                ) {
                    conflicts.push(conflict);
                    continue;
                }
                let current_state = match virtual_states.get(&identity) {
                    Some(state) => state.clone(),
                    None => {
                        let state = workspace_entry_state(&path)?;
                        virtual_states.insert(identity.clone(), state.clone());
                        state
                    }
                };
                if let Some(conflict) =
                    self.rollback_conflict(record, file, &file.path, &current_state)?
                {
                    conflicts.push(conflict);
                    continue;
                }
                if let Some(conflict) = self.hardlink_peer_conflict(record, file, &planned_paths)? {
                    conflicts.push(conflict);
                    continue;
                }

                if let Some(from_path) = file.from_path.as_deref() {
                    let source_path = safe_workspace_path(&self.root, from_path)?;
                    let source_identity = path_identity_key(from_path);
                    if let Some(conflict) = reparse_path_conflict(
                        &record.id,
                        from_path,
                        &self.root,
                        &source_path,
                        restoring_symlink,
                    ) {
                        conflicts.push(conflict);
                        continue;
                    }
                    let source_state = match virtual_states.get(&source_identity) {
                        Some(state) => state.clone(),
                        None => {
                            let state = workspace_entry_state(&source_path)?;
                            virtual_states.insert(source_identity.clone(), state.clone());
                            state
                        }
                    };
                    if !source_state.is_absent() {
                        conflicts.push(RollbackConflict {
                            checkpoint_id: record.id.clone(),
                            path: from_path.to_string(),
                            expected_sha256: None,
                            current_sha256: source_state.sha256.clone(),
                            expected_hash_basis: None,
                            current_hash_basis: Some("current worktree byte hash".to_string()),
                            reason_code: Some(RollbackConflictReason::WorktreeChanged),
                            retryable: false,
                            reason: "rename source path changed after checkpoint; leaving current content untouched".to_string(),
                        });
                        continue;
                    }
                    virtual_states.insert(source_identity, file.before_entry_state());
                    virtual_states.insert(identity, WorkspaceEntryState::absent());
                } else {
                    virtual_states.insert(identity, file.before_entry_state());
                }
            }
        }
        Ok(conflicts)
    }

    fn hardlink_peer_conflict(
        &self,
        record: &CheckpointRecord,
        file: &CheckpointFile,
        planned_paths: &BTreeSet<String>,
    ) -> Result<Option<RollbackConflict>> {
        let Some(group) = file.before_hardlink_paths.as_ref() else {
            return Ok(None);
        };
        let expected = file.before_entry_state();
        for peer in group {
            if peer == &file.path || file.from_path.as_deref() == Some(peer.as_str()) {
                continue;
            }
            if planned_paths.contains(peer) {
                continue;
            }
            let peer_path = safe_workspace_path(&self.root, peer)?;
            let current = workspace_entry_state(&peer_path)?;
            if current != expected {
                return Ok(Some(RollbackConflict {
                    checkpoint_id: record.id.clone(),
                    path: file.path.clone(),
                    expected_sha256: expected.sha256.clone(),
                    current_sha256: current.sha256,
                    expected_hash_basis: Some("checkpoint worktree byte hash".to_string()),
                    current_hash_basis: Some("current worktree byte hash".to_string()),
                    reason_code: Some(RollbackConflictReason::WorktreeChanged),
                    retryable: false,
                    reason: format!(
                        "hardlink peer {peer} changed after checkpoint; leaving current content untouched"
                    ),
                }));
            }
        }
        Ok(None)
    }

    fn rollback_record(
        &self,
        record: &CheckpointRecord,
        result: &mut RollbackResult,
    ) -> Result<()> {
        for file in &record.files {
            if rollback_file_has_conflict(result, record, file) {
                continue;
            }
            let path = safe_workspace_path(&self.root, &file.path)?;

            if file.status == DiffFileStatus::Renamed {
                // Reverse a rename: remove the new path, restore the source path
                // (whose original content is at `from_path` in the before tree).
                if remove_workspace_file(&path)? {
                    result.deleted_files.push(file.path.clone());
                    result.file_actions.push(RollbackFileAction {
                        checkpoint_id: record.id.clone(),
                        path: file.path.clone(),
                        action: RollbackFileActionKind::Delete,
                        mode: None,
                        file_type: None,
                        verified_after_rollback: !path_exists_no_follow(&path),
                    });
                }
                if let Some(from_path) = file.from_path.as_deref()
                    && let Some(action) = self.restore_tree_path(
                        &record.before_tree,
                        from_path,
                        &record.id,
                        file.before_worktree_sha256.as_deref(),
                    )?
                {
                    result.restored_files.push(from_path.to_string());
                    result.file_actions.push(action);
                }
                continue;
            }

            if let Some(action) = self.restore_tree_path(
                &record.before_tree,
                &file.path,
                &record.id,
                file.before_worktree_sha256.as_deref(),
            )? {
                result.restored_files.push(file.path.clone());
                result.file_actions.push(action);
            } else if remove_workspace_file(&path)? {
                result.deleted_files.push(file.path.clone());
                result.file_actions.push(RollbackFileAction {
                    checkpoint_id: record.id.clone(),
                    path: file.path.clone(),
                    action: RollbackFileActionKind::Delete,
                    mode: None,
                    file_type: None,
                    verified_after_rollback: !path_exists_no_follow(&path),
                });
            }
        }
        self.restore_record_hardlinks(record, result)?;
        Ok(())
    }

    fn restore_record_hardlinks(
        &self,
        record: &CheckpointRecord,
        result: &mut RollbackResult,
    ) -> Result<()> {
        // Collect every workspace path that any conflict in this record's
        // preflight already protected. The per-group loop below must skip
        // those paths so a non-conflicted peer never overwrites a peer the
        // gate told us to leave alone (see review N1).
        let conflicted_paths: BTreeSet<&str> = result
            .conflicts
            .iter()
            .filter(|conflict| conflict.checkpoint_id == record.id)
            .map(|conflict| conflict.path.as_str())
            .collect();

        let mut groups = BTreeSet::<Vec<String>>::new();
        for file in &record.files {
            if rollback_file_has_conflict(result, record, file) {
                continue;
            }
            if let Some(group) = file.before_hardlink_paths.as_ref()
                && group.len() > 1
            {
                groups.insert(group.clone());
            }
        }

        for group in groups {
            // If any peer in this group was preflighted as conflicted, skip
            // the whole group rather than restoring some peers and relinking
            // the rest to the freshly overwritten content.
            if group
                .iter()
                .any(|peer| conflicted_paths.contains(peer.as_str()))
            {
                continue;
            }
            for rel in &group {
                let Some(entry) = self.tree_entry(&record.before_tree, rel)? else {
                    continue;
                };
                if entry.object_type != "blob" || entry.is_symlink() {
                    continue;
                }
                let path = safe_workspace_path(&self.root, rel)?;
                let bytes = self.blob_bytes(&record.before_tree, rel).map_err(|err| {
                    SqueezyError::Tool(format!("checkpoint object for {rel} is missing: {err}"))
                })?;
                let expected = WorkspaceEntryState {
                    sha256: Some(sha256_hex(&bytes)),
                    file_type: Some(entry.checkpoint_file_type()),
                    mode: Some(entry.mode),
                };
                if workspace_entry_state(&path)? != expected
                    && let Some(action) =
                        self.restore_tree_path(&record.before_tree, rel, &record.id, None)?
                {
                    result.restored_files.push(rel.clone());
                    result.file_actions.push(action);
                }
            }

            // `restore_hardlink_group` returns only after verifying that
            // every relinked peer shares the source's inode, so we can
            // record the verified result without a second walk.
            let relinked = restore_hardlink_group(&self.root, &group)?;
            for rel in relinked {
                let Some(entry) = self.tree_entry(&record.before_tree, &rel)? else {
                    continue;
                };
                let file_type = entry.checkpoint_file_type();
                let mode = entry.mode;
                result.file_actions.push(RollbackFileAction {
                    checkpoint_id: record.id.clone(),
                    path: rel,
                    action: RollbackFileActionKind::RestoreHardlink,
                    mode: Some(mode),
                    file_type: Some(file_type),
                    verified_after_rollback: true,
                });
            }
        }
        Ok(())
    }

    fn backup_rollback_paths(
        &self,
        records: &[CheckpointRecord],
        result: &RollbackResult,
    ) -> Result<Vec<RollbackBackup>> {
        let mut paths = BTreeSet::new();
        for record in records {
            for file in &record.files {
                if rollback_file_has_conflict(result, record, file) {
                    continue;
                }
                for path in rollback_write_paths(file) {
                    paths.insert(path);
                }
            }
        }
        paths
            .into_iter()
            .map(|rel| {
                let path = safe_workspace_path(&self.root, &rel)?;
                let bytes = if path_exists_no_follow(&path) {
                    Some(fs::read(&path)?)
                } else {
                    None
                };
                Ok(RollbackBackup { path: rel, bytes })
            })
            .collect()
    }

    fn restore_rollback_backups(&self, backups: &[RollbackBackup]) -> Result<()> {
        for backup in backups {
            let path = safe_workspace_path(&self.root, &backup.path)?;
            match backup.bytes.as_deref() {
                Some(bytes) => restore_regular_file_atomic(&path, bytes, None)?,
                None => {
                    let _ = remove_workspace_file(&path)?;
                }
            }
        }
        Ok(())
    }

    fn record_shadow_refresh_failure(result: &mut RollbackResult, err: impl std::fmt::Display) {
        result.conflicts.push(RollbackConflict {
            checkpoint_id: result
                .checkpoint_ids
                .first()
                .cloned()
                .unwrap_or_else(|| "unknown".to_string()),
            path: ".".to_string(),
            expected_sha256: None,
            current_sha256: None,
            expected_hash_basis: None,
            current_hash_basis: None,
            reason_code: Some(RollbackConflictReason::ShadowRefreshFailed),
            retryable: true,
            reason: format!(
                "rollback changed files, but refreshing the shadow checkpoint tree failed: {err}"
            ),
        });
    }

    fn rollback_conflict(
        &self,
        record: &CheckpointRecord,
        file: &CheckpointFile,
        path: &str,
        current_state: &WorkspaceEntryState,
    ) -> Result<Option<RollbackConflict>> {
        // Collect every failed predicate so a multi-cause divergence (e.g.
        // both type and mode changed) shows up in a single conflict reason
        // instead of silently collapsing to whichever check ran first
        // (see review N10).
        let mut reasons: Vec<String> = Vec::new();
        let expected_sha256 = file
            .after_worktree_sha256
            .clone()
            .or_else(|| file.after_sha256.clone());
        let expected_basis = if file.after_worktree_sha256.is_some() {
            "checkpoint worktree byte hash"
        } else {
            "checkpoint git blob hash"
        };
        let mut reason_code = RollbackConflictReason::WorktreeChanged;
        if let Some(expected_type) = file.after_file_type
            && current_state.file_type != Some(expected_type)
        {
            if current_state.file_type.is_none() {
                reasons.push(format!(
                    "file was deleted after checkpoint; expected {:?}; leaving it deleted",
                    expected_type
                ));
            } else {
                reasons.push(format!(
                    "file type changed after checkpoint; expected {:?}, got {:?}; leaving current content untouched",
                    expected_type, current_state.file_type
                ));
            }
        }
        if let Some(expected_mode) = file.after_mode.as_deref()
            && current_state.mode.as_deref() != Some(expected_mode)
        {
            reasons.push(format!(
                "file mode changed after checkpoint; expected {expected_mode}, got {}; leaving current content untouched",
                current_state.mode.as_deref().unwrap_or("absent")
            ));
        }
        if current_state.sha256 != expected_sha256 {
            if file.after_worktree_sha256.is_some()
                && file.after_sha256.is_some()
                && file.after_worktree_sha256 != file.after_sha256
            {
                reason_code = RollbackConflictReason::GitFilterOrEolMismatch;
                reasons.push(
                    "checkpoint git blob hash differs from current worktree bytes, likely because \
                     Git filters or eol normalization changed the byte basis; leaving current \
                     content untouched"
                        .to_string(),
                );
            } else {
                reasons.push(
                    "file changed after checkpoint; leaving current content untouched".to_string(),
                );
            }
        }
        let before_lookup = file.from_path.as_deref().unwrap_or(file.path.as_str());
        let checkpoint_object_missing = if file.before_worktree_sha256.is_some() {
            self.restore_bytes(
                file.before_worktree_sha256.as_deref(),
                &record.before_tree,
                before_lookup,
            )
            .is_err()
        } else if self.tree_has_path(&record.before_tree, before_lookup)? {
            self.blob_bytes(&record.before_tree, before_lookup).is_err()
        } else {
            false
        };
        if checkpoint_object_missing {
            reason_code = RollbackConflictReason::CheckpointObjectMissing;
            reasons.push(
                "checkpoint object is missing; leaving current content untouched".to_string(),
            );
        }
        if reasons.is_empty() {
            return Ok(None);
        }
        Ok(Some(RollbackConflict {
            checkpoint_id: record.id.clone(),
            path: path.to_string(),
            expected_sha256,
            current_sha256: current_state.sha256.clone(),
            expected_hash_basis: Some(expected_basis.to_string()),
            current_hash_basis: Some("current worktree byte hash".to_string()),
            reason_code: Some(reason_code),
            retryable: false,
            reason: reasons.join("; "),
        }))
    }

    fn restore_tree_path(
        &self,
        tree: &str,
        rel: &str,
        checkpoint_id: &str,
        worktree_sha256: Option<&str>,
    ) -> Result<Option<RollbackFileAction>> {
        let Some(entry) = self.tree_entry(tree, rel)? else {
            return Ok(None);
        };
        if entry.object_type != "blob" {
            return Err(SqueezyError::Tool(format!(
                "checkpoint path {rel} is a {}, not a file blob",
                entry.object_type
            )));
        }
        let path = safe_workspace_path(&self.root, rel)?;
        let bytes = self
            .restore_bytes(worktree_sha256, tree, rel)
            .map_err(|err| {
                SqueezyError::Tool(format!("checkpoint object for {rel} is missing: {err}"))
            })?;
        let file_type = entry.checkpoint_file_type();
        let mode = entry.mode.clone();
        let action = if entry.is_symlink() {
            restore_symlink_atomic(&path, &bytes)?;
            RollbackFileActionKind::RestoreSymlink
        } else {
            restore_regular_file_atomic(&path, &bytes, entry.unix_mode())?;
            RollbackFileActionKind::RestoreRegular
        };
        verify_restored_entry(
            rel,
            &path,
            &WorkspaceEntryState {
                sha256: Some(
                    worktree_sha256
                        .map(str::to_string)
                        .unwrap_or_else(|| sha256_hex(&bytes)),
                ),
                file_type: Some(file_type),
                mode: Some(mode.clone()),
            },
        )?;
        Ok(Some(RollbackFileAction {
            checkpoint_id: checkpoint_id.to_string(),
            path: rel.to_string(),
            action,
            mode: Some(mode),
            file_type: Some(file_type),
            verified_after_rollback: true,
        }))
    }

    fn preflight_filesystem_conflicts(
        &self,
        records: &[CheckpointRecord],
        existing_conflicts: &[RollbackConflict],
    ) -> Result<Vec<RollbackConflict>> {
        let mut conflicts = Vec::new();
        for record in records {
            for file in &record.files {
                if existing_conflicts
                    .iter()
                    .chain(conflicts.iter())
                    .any(|conflict| {
                        conflict.checkpoint_id == record.id && conflict.path == file.path
                    })
                {
                    continue;
                }
                let restoring_symlink = file.before_file_type == Some(CheckpointFileType::Symlink);
                for path in rollback_write_paths(file) {
                    let absolute = self.root.join(&path);
                    if let Some(conflict) = filesystem_preflight_conflict(
                        &record.id,
                        &path,
                        &self.root,
                        &absolute,
                        restoring_symlink,
                    ) {
                        conflicts.push(conflict);
                    }
                }
            }
        }
        Ok(conflicts)
    }

    fn restore_bytes(
        &self,
        worktree_sha256: Option<&str>,
        tree: &str,
        path: &str,
    ) -> std::result::Result<Vec<u8>, String> {
        if let Some(sha256) = worktree_sha256 {
            // The checkpoint promised raw worktree bytes for this path, so
            // silently falling back to the Git blob would re-introduce the
            // exact CRLF/normalization surprise the worktree-hash store is
            // designed to prevent. Surface the missing blob so the caller
            // converts it into a `CheckpointObjectMissing` conflict instead.
            //
            // A hand-edited / partially-written / corrupted journal line can
            // carry a digest that is not a 64-char hex string. `raw_blob_path`
            // slices `sha256[..2]` to derive the shard prefix, which would
            // panic on a length-0/1 digest in release builds (the only guard
            // there is a debug-only assert). Validate the digest here and
            // surface malformed input as an error so the caller turns it into
            // a recoverable `CheckpointObjectMissing` conflict rather than a
            // hard process panic.
            if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(format!(
                    "raw worktree byte blob hash {sha256:?} for {path} is malformed"
                ));
            }
            return fs::read(self.raw_blob_path(sha256)).map_err(|err| {
                format!("raw worktree byte blob {sha256} for {path} unreadable: {err}")
            });
        }
        self.blob_bytes(tree, path)
    }

    fn tree_has_path(&self, tree: &str, path: &str) -> Result<bool> {
        let output = self.git_vec(vec![
            "ls-tree".to_string(),
            "-z".to_string(),
            tree.to_string(),
            "--".to_string(),
            path.to_string(),
        ])?;
        Ok(parse_tree_entry(&output.stdout)?.is_some())
    }

    fn tree_entry(&self, tree: &str, path: &str) -> Result<Option<TreeEntry>> {
        let output = self.git_vec(vec![
            "ls-tree".to_string(),
            "-z".to_string(),
            tree.to_string(),
            "--".to_string(),
            path.to_string(),
        ])?;
        parse_tree_entry(&output.stdout)
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

    fn workspace_file_fingerprints(&self) -> Result<WorkspaceFileFingerprints> {
        let mut large_files = Vec::new();
        let mut raw_files = BTreeMap::new();
        let output = self.git_vec(vec![
            "ls-files".to_string(),
            "-z".to_string(),
            "--cached".to_string(),
            "--others".to_string(),
            "--exclude-standard".to_string(),
            "--".to_string(),
            ".".to_string(),
            ":(exclude).squeezy".to_string(),
        ])?;
        for rel in nul_fields(&output.stdout) {
            if rel.is_empty() || rel == ".squeezy" || rel.starts_with(".squeezy/") {
                continue;
            }
            let path = safe_workspace_path(&self.root, &rel)?;
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata_is_reparse_or_symlink(&metadata) || !metadata.is_file() {
                continue;
            }
            let (mtime_secs, mtime_nanos) = mtime_parts(&metadata);
            let entry = WorkspaceFileEntry {
                rel,
                absolute: path,
                size_bytes: metadata.len(),
                mtime_secs,
                mtime_nanos,
            };
            if entry.size_bytes > self.options.max_file_bytes {
                large_files.push(LargeFileFingerprint {
                    path: entry.rel,
                    size_bytes: entry.size_bytes,
                    mtime_secs,
                    mtime_nanos,
                });
                continue;
            }
            let raw = self.raw_file_snapshot(&entry)?;
            raw_files.insert(entry.rel, raw);
        }
        large_files.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(WorkspaceFileFingerprints {
            large_files,
            raw_files,
        })
    }

    fn raw_file_snapshot(&self, entry: &WorkspaceFileEntry) -> Result<RawFileSnapshot> {
        let mut cache = self
            .raw_cache
            .lock()
            .map_err(|err| SqueezyError::Tool(format!("checkpoint raw cache poisoned: {err}")))?;
        if let Some(cached) = cache.get(&entry.rel)
            && cached.size_bytes == entry.size_bytes
            && cached.mtime_secs == entry.mtime_secs
            && cached.mtime_nanos == entry.mtime_nanos
            && self.raw_blob_path(&cached.sha256).exists()
        {
            return Ok(RawFileSnapshot {
                sha256: cached.sha256.clone(),
                size_bytes: cached.size_bytes,
                mtime_secs: cached.mtime_secs,
                mtime_nanos: cached.mtime_nanos,
            });
        }
        let bytes = fs::read(&entry.absolute)?;
        let sha256 = sha256_hex(&bytes);
        self.write_raw_blob(&sha256, &bytes)?;
        cache.insert(
            entry.rel.clone(),
            CachedRawFile {
                size_bytes: entry.size_bytes,
                mtime_secs: entry.mtime_secs,
                mtime_nanos: entry.mtime_nanos,
                sha256: sha256.clone(),
            },
        );
        Ok(RawFileSnapshot {
            sha256,
            size_bytes: entry.size_bytes,
            mtime_secs: entry.mtime_secs,
            mtime_nanos: entry.mtime_nanos,
        })
    }

    fn write_raw_blob(&self, sha256: &str, bytes: &[u8]) -> Result<()> {
        let path = self.raw_blob_path(sha256);
        if path.exists() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        // Write to a sibling temp file then rename so that a crash mid-write
        // cannot leave a partial blob that restore_bytes would later read.
        let tmp_path = path.with_extension("tmp");
        fs::write(&tmp_path, bytes)?;
        if let Err(err) = fs::rename(&tmp_path, &path) {
            let _ = fs::remove_file(&tmp_path);
            return Err(SqueezyError::Tool(format!("raw blob rename failed: {err}")));
        }
        Ok(())
    }

    fn raw_blob_path(&self, sha256: &str) -> PathBuf {
        // `sha256_hex` always returns a 64-hex string, so the slice is safe in
        // every path that goes through `write_raw_blob` / `restore_bytes`. The
        // debug assertion keeps a future caller honest if they try to thread a
        // truncated digest through here.
        debug_assert!(
            sha256.len() >= 2,
            "raw_blob_path called with truncated sha256 {sha256:?}"
        );
        let prefix = &sha256[..2];
        self.raw_blob_dir.join(prefix).join(sha256)
    }

    fn git_config_value(&self, key: &str) -> Option<String> {
        let output = self
            .git_vec_allow_status(
                vec!["config".to_string(), "--get".to_string(), key.to_string()],
                &[0, 1],
            )
            .ok()?;
        if output.status.code() != Some(0) {
            return None;
        }
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        (!value.is_empty()).then_some(value)
    }

    #[cfg(unix)]
    fn hardlink_groups(&self) -> Result<BTreeMap<String, Vec<String>>> {
        use std::os::unix::fs::MetadataExt;

        let output = self.git_vec(vec![
            "ls-files".to_string(),
            "-z".to_string(),
            "--cached".to_string(),
            "--others".to_string(),
            "--exclude-standard".to_string(),
            "--".to_string(),
            ".".to_string(),
            ":(exclude).squeezy".to_string(),
        ])?;
        let mut by_inode = BTreeMap::<(u64, u64), Vec<String>>::new();
        for rel in nul_fields(&output.stdout) {
            if rel.is_empty() || rel == ".squeezy" || rel.starts_with(".squeezy/") {
                continue;
            }
            let path = safe_workspace_path(&self.root, &rel)?;
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.is_file() && metadata.nlink() > 1 {
                by_inode
                    .entry((metadata.dev(), metadata.ino()))
                    .or_default()
                    .push(rel);
            }
        }

        let mut by_path = BTreeMap::new();
        for mut paths in by_inode.into_values() {
            paths.sort();
            paths.dedup();
            if paths.len() < 2 {
                continue;
            }
            for path in &paths {
                by_path.insert(path.clone(), paths.clone());
            }
        }
        Ok(by_path)
    }

    #[cfg(not(unix))]
    fn hardlink_groups(&self) -> Result<BTreeMap<String, Vec<String>>> {
        Ok(BTreeMap::new())
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
            // No journal entries means every raw blob on disk is an orphan
            // (e.g. a stale `.squeezy/` from a previous workspace lifetime).
            self.prune_orphan_raw_blobs(&[]);
            if let Ok(mut last_run) = self.cleanup_last_run_ms.lock() {
                *last_run = Some(now_ms());
            }
            return Ok(());
        }
        let cutoff = now_ms().saturating_sub(retention_days as u128 * 24 * 60 * 60 * 1_000);
        let (keep, prune): (Vec<_>, Vec<_>) = journal
            .checkpoints
            .into_iter()
            .partition(|record| record.created_at_ms >= cutoff);
        if prune.is_empty() {
            // Even when no journal records aged out, blobs can be orphaned by
            // a previous partial cleanup (e.g. a crash between journal rewrite
            // and blob unlink). Re-run the orphan sweep so leaked blobs do not
            // accumulate forever.
            self.prune_orphan_raw_blobs(&keep);
            if let Ok(mut last_run) = self.cleanup_last_run_ms.lock() {
                *last_run = Some(now_ms());
            }
            return Ok(());
        }
        let mut keep = keep;
        let mut pruned_any = false;
        for record in &prune {
            let before_deleted = self.git_vec(vec![
                "update-ref".to_string(),
                "-d".to_string(),
                checkpoint_ref(&record.id, "before"),
            ]);
            let after_deleted = self.git_vec(vec![
                "update-ref".to_string(),
                "-d".to_string(),
                checkpoint_ref(&record.id, "after"),
            ]);
            if before_deleted.is_ok() && after_deleted.is_ok() {
                pruned_any = true;
            } else {
                // `journal_warnings` is overloaded here: the journal itself is
                // fine — it's the protected-ref deletion that failed. We bump
                // the counter so the warning is auditable in `checkpoint_list`,
                // and append a `coverage_warning` carrying the actual reason.
                let mut retained = record.clone();
                retained.journal_warnings += 1;
                retained.coverage_warnings.push(
                    "retention cleanup could not delete one or more protected checkpoint refs; \
                     keeping this journal record so shadow refs are still auditable"
                        .to_string(),
                );
                keep.push(retained);
            }
        }
        keep.sort_by_key(|record| record.created_at_ms);
        // Drop rollback records whose `checkpoint_ids` no longer appear in
        // the kept set so the journal does not accumulate audit history
        // for already-pruned checkpoints. A rollback that referenced a
        // mix of kept and pruned ids stays as long as at least one id
        // still resolves; otherwise it is pure dead weight.
        let keep_ids: BTreeSet<&str> = keep.iter().map(|record| record.id.as_str()).collect();
        let kept_rollbacks: Vec<RollbackJournalRecord> = journal
            .rollbacks
            .into_iter()
            .filter(|rollback| {
                rollback
                    .result
                    .checkpoint_ids
                    .iter()
                    .any(|id| keep_ids.contains(id.as_str()))
            })
            .collect();
        // Update the throttle timestamp only after successfully rewriting the
        // journal, so a failed rewrite does not suppress the next retry.
        self.rewrite_checkpoint_journal_with_rollbacks(&keep, &kept_rollbacks)?;
        // Walk `raw-blobs/` and remove any blob whose sha256 is not referenced
        // by a kept record. Without this sweep the content-addressed raw byte
        // store grows without bound: every distinct before/after worktree byte
        // body the agent has ever seen would otherwise live forever, surviving
        // both `update-ref -d` and `git gc --prune=now` because raw blobs sit
        // outside the Git object store.
        self.prune_orphan_raw_blobs(&keep);
        if let Ok(mut last_run) = self.cleanup_last_run_ms.lock() {
            *last_run = Some(now_ms());
        }
        if pruned_any {
            // Skip `git gc --prune=now` when no record actually pruned: gc on
            // a packed shadow repo can take minutes, and reclaiming nothing
            // means the cost would be pure waste against the cleanup hot path.
            let _ = self.git(["gc", "--prune=now"]);
        }
        Ok(())
    }

    fn prune_orphan_raw_blobs(&self, keep: &[CheckpointRecord]) {
        if !self.raw_blob_dir.exists() {
            return;
        }
        let mut referenced = BTreeSet::new();
        for record in keep {
            for file in &record.files {
                if let Some(sha) = file.before_worktree_sha256.as_deref() {
                    referenced.insert(sha.to_string());
                }
                if let Some(sha) = file.after_worktree_sha256.as_deref() {
                    referenced.insert(sha.to_string());
                }
            }
        }
        let Ok(prefix_entries) = fs::read_dir(&self.raw_blob_dir) else {
            return;
        };
        for prefix_entry in prefix_entries.flatten() {
            let prefix_path = prefix_entry.path();
            if !prefix_path.is_dir() {
                continue;
            }
            let Ok(blob_entries) = fs::read_dir(&prefix_path) else {
                continue;
            };
            for blob_entry in blob_entries.flatten() {
                let blob_path = blob_entry.path();
                let Some(name) = blob_path.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                // Skip mid-write temp blobs (`<sha>.tmp`) so a concurrent
                // `write_raw_blob` does not race with the cleanup.
                if name.ends_with(".tmp") {
                    continue;
                }
                if referenced.contains(name) {
                    continue;
                }
                let _ = fs::remove_file(&blob_path);
            }
            let _ = fs::remove_dir(&prefix_path);
        }
    }

    fn cleanup_old_checkpoints_if_due(&self) -> Result<()> {
        if self.options.cleanup_interval_secs == 0 {
            return self.cleanup_old_checkpoints(self.options.retention_days);
        }
        let marker = self
            .journal_path
            .parent()
            .unwrap_or(&self.root)
            .join(SHADOW_LAST_CLEANUP_FILENAME);
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

    #[allow(dead_code)]
    fn git_vec_with_stdin_allow_status(
        &self,
        args: Vec<String>,
        stdin: Vec<u8>,
        success: &[i32],
    ) -> Result<Output> {
        let full_args = std::iter::once("--git-dir".to_string())
            .chain(std::iter::once(self.git_dir.to_string_lossy().to_string()))
            .chain(std::iter::once("--work-tree".to_string()))
            .chain(std::iter::once(self.root.to_string_lossy().to_string()))
            .chain(args)
            .collect();
        git_output_vec_with_stdin_allow_status(&self.root, full_args, stdin, success)
            .map_err(SqueezyError::Tool)
    }
}

#[derive(Debug, Clone)]
struct RollbackBackup {
    path: String,
    bytes: Option<Vec<u8>>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct TreeEntry {
    mode: String,
    object_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkspaceEntryState {
    sha256: Option<String>,
    file_type: Option<CheckpointFileType>,
    mode: Option<String>,
}

impl WorkspaceEntryState {
    fn absent() -> Self {
        Self {
            sha256: None,
            file_type: None,
            mode: None,
        }
    }

    fn is_absent(&self) -> bool {
        // `workspace_entry_state` only ever returns `None` for `file_type`
        // when the path itself was missing, so the `file_type.is_none()`
        // check captures the absent state without re-asserting `sha256`
        // and `mode` (which are correlated with `file_type` by
        // construction).
        self.file_type.is_none()
    }
}

impl TreeEntry {
    fn checkpoint_file_type(&self) -> CheckpointFileType {
        if self.is_symlink() {
            CheckpointFileType::Symlink
        } else if self.object_type == "blob" {
            CheckpointFileType::RegularFile
        } else {
            CheckpointFileType::Other
        }
    }

    fn is_symlink(&self) -> bool {
        self.mode == "120000"
    }

    fn unix_mode(&self) -> Option<u32> {
        u32::from_str_radix(&self.mode, 8)
            .ok()
            .map(|mode| mode & 0o7777)
    }
}

fn parse_tree_entry(output: &[u8]) -> Result<Option<TreeEntry>> {
    let Some(record) = output
        .split(|byte| *byte == 0)
        .find(|field| !field.is_empty())
    else {
        return Ok(None);
    };
    let Some(tab) = record.iter().position(|byte| *byte == b'\t') else {
        return Err(SqueezyError::Tool(
            "malformed checkpoint tree entry: missing path separator".to_string(),
        ));
    };
    let header = String::from_utf8_lossy(&record[..tab]);
    let mut parts = header.split_whitespace();
    let Some(mode) = parts.next() else {
        return Err(SqueezyError::Tool(
            "malformed checkpoint tree entry: missing mode".to_string(),
        ));
    };
    let Some(object_type) = parts.next() else {
        return Err(SqueezyError::Tool(
            "malformed checkpoint tree entry: missing object type".to_string(),
        ));
    };
    Ok(Some(TreeEntry {
        mode: mode.to_string(),
        object_type: object_type.to_string(),
    }))
}

fn hardlink_paths_for(groups: &BTreeMap<String, Vec<String>>, path: &str) -> Option<Vec<String>> {
    groups.get(path).filter(|paths| paths.len() > 1).cloned()
}

fn verify_restored_entry(rel: &str, path: &Path, expected: &WorkspaceEntryState) -> Result<()> {
    let actual = workspace_entry_state(path)?;
    if &actual != expected {
        return Err(SqueezyError::Tool(format!(
            "checkpoint rollback verification failed for {rel}: expected {:?}, got {:?}",
            expected, actual
        )));
    }
    Ok(())
}

fn workspace_entry_state(path: &Path) -> Result<WorkspaceEntryState> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(WorkspaceEntryState::absent());
        }
        Err(err) => return Err(err.into()),
    };
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(path)?;
        return Ok(WorkspaceEntryState {
            sha256: Some(sha256_hex(&path_bytes(target.as_os_str()))),
            file_type: Some(CheckpointFileType::Symlink),
            mode: Some("120000".to_string()),
        });
    }
    if metadata.is_file() {
        return Ok(WorkspaceEntryState {
            sha256: Some(sha256_file(path)?),
            file_type: Some(CheckpointFileType::RegularFile),
            mode: Some(workspace_regular_file_git_mode(&metadata)),
        });
    }
    // For non-regular, non-symlink workspace entries (sockets, fifos,
    // block/char devices) we hash a fixed sentinel string keyed by the
    // path. The conflict gate only needs a *stable* sha that compares
    // equal to itself across rollback attempts on the same path; the
    // exact bytes are immaterial. Using the path keeps two unrelated
    // device files from colliding into the same hash.
    Ok(WorkspaceEntryState {
        sha256: Some(sha256_hex(
            format!("unsupported-file-type:{}", rel_display(path)).as_bytes(),
        )),
        file_type: Some(CheckpointFileType::Other),
        mode: None,
    })
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    let digest = hasher.finalize();
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        push_hex_byte(&mut output, byte);
    }
    Ok(output)
}

#[cfg(unix)]
fn path_bytes(path: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_bytes(path: &OsStr) -> Vec<u8> {
    // FIXME: `to_string_lossy` replaces non-UTF-8 sequences with U+FFFD,
    // producing a sha256 that will never match the Git-blob hash for symlink
    // targets that contain non-Unicode bytes. Acceptable for now because
    // symlink restore is not supported on non-Unix platforms.
    path.to_string_lossy().as_bytes().to_vec()
}

#[cfg(unix)]
fn workspace_regular_file_git_mode(metadata: &fs::Metadata) -> String {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o111 != 0 {
        "100755".to_string()
    } else {
        "100644".to_string()
    }
}

#[cfg(not(unix))]
fn workspace_regular_file_git_mode(_metadata: &fs::Metadata) -> String {
    "100644".to_string()
}

fn symlink_target_display(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Returns `root.join(rel)` after verifying that `rel` contains no absolute
/// prefix, parent-dir components, or `.squeezy` protected-metadata prefix.
///
/// **Limitation:** this check is purely lexical. It does not resolve symlinks
/// in intermediate directory components. A journal path whose parent directory
/// is a symlink pointing outside `root` would pass these checks. Callers that
/// create parent directories (e.g. `restore_regular_file_atomic`) may
/// inadvertently follow such a symlink. Fully eliminating that risk requires
/// walking and stat-checking each component before descent.
fn safe_workspace_path(root: &Path, rel: &str) -> Result<PathBuf> {
    let path = Path::new(rel);
    let reason = if path.is_absolute() {
        Some("absolute path")
    } else if path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        Some("path escapes workspace via parent or root component")
    } else if rel == ".squeezy" || rel.starts_with(".squeezy/") {
        Some("path targets the .squeezy protected metadata tree")
    } else {
        None
    };
    if let Some(reason) = reason {
        return Err(SqueezyError::Tool(format!(
            "checkpoint path is not safe to roll back ({reason}): {rel}"
        )));
    }
    Ok(root.join(path))
}

fn restore_regular_file_atomic(path: &Path, bytes: &[u8], mode: Option<u32>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let (tmp, mut file) = create_sibling_tempfile(path)?;
    {
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    {
        // No POSIX mode bits to apply on non-Unix; consume the parameter
        // so `cargo clippy` does not flag it as dead.
        let _ = mode;
    }
    if let Err(err) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(err.into());
    }
    sync_parent_dir(path);
    Ok(())
}

#[cfg(unix)]
fn restore_symlink_atomic(path: &Path, target: &[u8]) -> Result<()> {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = create_sibling_symlink(path, OsString::from_vec(target.to_vec()))?;
    if let Err(err) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(err.into());
    }
    sync_parent_dir(path);
    Ok(())
}

#[cfg(not(unix))]
fn restore_symlink_atomic(_path: &Path, _target: &[u8]) -> Result<()> {
    Err(SqueezyError::Tool(
        "checkpoint rollback cannot restore symlinks on this platform".to_string(),
    ))
}

fn remove_workspace_file(path: &Path) -> Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err.into()),
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        return Err(SqueezyError::Tool(format!(
            "checkpoint rollback refuses to remove directory {}",
            path.display()
        )));
    }
    fs::remove_file(path)?;
    sync_parent_dir(path);
    Ok(true)
}

#[cfg(unix)]
fn restore_hardlink_group(root: &Path, group: &[String]) -> Result<Vec<String>> {
    let Some(source_rel) = group.first() else {
        return Ok(Vec::new());
    };
    let source = safe_workspace_path(root, source_rel)?;
    let mut relinked = Vec::new();
    for rel in group.iter().skip(1) {
        let target = safe_workspace_path(root, rel)?;
        if same_inode(&source, &target)? {
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        // Crash-atomic relink: hard-link `source` to a sibling tempfile
        // first, then `rename` over `target`. A power loss between the
        // two steps leaves `target` either in its pre-rollback state or
        // pointing at the source inode — never absent (see review N3).
        atomic_relink(&source, &target)?;
        sync_parent_dir(&target);
        relinked.push(rel.clone());
    }
    if !verify_hardlink_group(root, group)? {
        return Err(SqueezyError::Tool(format!(
            "checkpoint rollback hardlink verification failed for {:?}",
            group
        )));
    }
    Ok(relinked)
}

#[cfg(unix)]
fn atomic_relink(source: &Path, target: &Path) -> Result<()> {
    let tmp = create_sibling_hardlink(source, target)?;
    if let Err(err) = fs::rename(&tmp, target) {
        let _ = fs::remove_file(&tmp);
        return Err(err.into());
    }
    Ok(())
}

#[cfg(unix)]
fn create_sibling_hardlink(source: &Path, target: &Path) -> Result<PathBuf> {
    let mut saw_collision = false;
    for _ in 0..MAX_RESTORE_TEMPFILE_ATTEMPTS {
        let tmp = next_sibling_tempfile(target);
        match fs::hard_link(source, &tmp) {
            Ok(()) => return Ok(tmp),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                saw_collision = true;
            }
            Err(err) => return Err(err.into()),
        }
    }
    let message = if saw_collision {
        "exhausted checkpoint restore hardlink tempfile candidates"
    } else {
        "no checkpoint restore hardlink tempfile candidates available"
    };
    Err(SqueezyError::Tool(message.to_string()))
}

#[cfg(not(unix))]
fn restore_hardlink_group(_root: &Path, _group: &[String]) -> Result<Vec<String>> {
    Ok(Vec::new())
}

#[cfg(unix)]
fn verify_hardlink_group(root: &Path, group: &[String]) -> Result<bool> {
    let Some(first) = group.first() else {
        return Ok(true);
    };
    let first_path = safe_workspace_path(root, first)?;
    for rel in group.iter().skip(1) {
        let path = safe_workspace_path(root, rel)?;
        if !same_inode(&first_path, &path)? {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(unix)]
fn same_inode(left: &Path, right: &Path) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;

    let left = match fs::symlink_metadata(left) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err.into()),
    };
    let right = match fs::symlink_metadata(right) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err.into()),
    };
    Ok(left.dev() == right.dev() && left.ino() == right.ino())
}

fn rollback_file_has_conflict(
    result: &RollbackResult,
    record: &CheckpointRecord,
    file: &CheckpointFile,
) -> bool {
    result.conflicts.iter().any(|conflict| {
        conflict.checkpoint_id == record.id
            && (conflict.path == file.path
                || file
                    .from_path
                    .as_deref()
                    .is_some_and(|from_path| conflict.path == from_path))
    })
}

fn path_exists_no_follow(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn sync_parent_dir(path: &Path) {
    if let Some(parent) = path.parent()
        && let Ok(dir) = File::open(parent)
    {
        let _ = dir.sync_all();
    }
}

fn create_sibling_tempfile(target: &Path) -> std::io::Result<(PathBuf, File)> {
    create_sibling_tempfile_from_candidates(
        (0..MAX_RESTORE_TEMPFILE_ATTEMPTS).map(|_| next_sibling_tempfile(target)),
    )
}

fn create_sibling_tempfile_from_candidates<I>(candidates: I) -> std::io::Result<(PathBuf, File)>
where
    I: IntoIterator<Item = PathBuf>,
{
    let mut last_exists = None;
    for tmp in candidates {
        match OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(file) => return Ok((tmp, file)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                last_exists = Some(err);
            }
            Err(err) => return Err(err),
        }
    }
    Err(last_exists.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "exhausted checkpoint restore tempfile candidates",
        )
    }))
}

#[cfg(unix)]
fn create_sibling_symlink(target: &Path, link_target: std::ffi::OsString) -> Result<PathBuf> {
    create_sibling_symlink_from_candidates(
        (0..MAX_RESTORE_TEMPFILE_ATTEMPTS).map(|_| next_sibling_tempfile(target)),
        link_target,
    )
}

#[cfg(unix)]
fn create_sibling_symlink_from_candidates<I>(
    candidates: I,
    link_target: std::ffi::OsString,
) -> Result<PathBuf>
where
    I: IntoIterator<Item = PathBuf>,
{
    use std::os::unix::fs::symlink;

    let mut saw_collision = false;
    for tmp in candidates {
        match symlink(&link_target, &tmp) {
            Ok(()) => return Ok(tmp),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                saw_collision = true;
            }
            Err(err) => return Err(err.into()),
        }
    }
    let message = if saw_collision {
        "exhausted checkpoint restore symlink candidates"
    } else {
        "no checkpoint restore symlink candidates provided"
    };
    Err(SqueezyError::Tool(message.to_string()))
}

fn next_sibling_tempfile(target: &Path) -> PathBuf {
    let unique = CHECKPOINT_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    sibling_tempfile_candidate(target, unique)
}

fn sibling_tempfile_candidate(target: &Path, unique: u64) -> PathBuf {
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("checkpoint-restore");
    target.with_file_name(format!(
        ".{name}.squeezy-restore-{}-{unique}.tmp",
        std::process::id()
    ))
}

fn rel_display(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

#[derive(Debug, Clone)]
struct WorkspaceFileEntry {
    rel: String,
    absolute: PathBuf,
    size_bytes: u64,
    mtime_secs: i64,
    mtime_nanos: u32,
}

#[derive(Debug, Clone)]
struct WorkspaceFileFingerprints {
    large_files: Vec<LargeFileFingerprint>,
    raw_files: BTreeMap<String, RawFileSnapshot>,
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

fn doctor_probe_ref(now_ms_value: u128) -> String {
    format!("refs/squeezy/doctor/probe-{now_ms_value}")
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
            let holder = fs::read_to_string(lock_path)
                .ok()
                .and_then(|body| {
                    let mut lines = body.lines();
                    let pid = lines.next()?.trim();
                    let created_at_ms = lines.next()?.trim();
                    if pid.is_empty() || created_at_ms.is_empty() {
                        None
                    } else {
                        Some(format!(" holder pid={pid}, locked_at_ms={created_at_ms}"))
                    }
                })
                .unwrap_or_else(|| " holder details unavailable".to_string());
            return Err(SqueezyError::Tool(format!(
                "another squeezy process is holding the shadow-repo lock at {} \
                 — start one squeezy per workspace or wait for it to exit;{holder}",
                lock_path.display(),
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
        if matches!(
            name,
            "git"
                | "raw-blobs"
                | "journal.jsonl"
                | SHADOW_LOCK_FILENAME
                | SHADOW_LAST_CLEANUP_FILENAME
        ) {
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if modified >= cutoff {
            continue;
        }
        if metadata_is_reparse_or_symlink(&metadata) {
            let _ = fs::remove_file(&path).or_else(|_| fs::remove_dir(&path));
            continue;
        }
        if metadata.is_dir() {
            let _ = fs::remove_dir_all(&path);
        } else {
            let _ = fs::remove_file(&path);
        }
    }
}

fn run_checkpoint_smoke() -> CheckpointSmokeReport {
    match run_checkpoint_smoke_inner() {
        Ok(report) => report,
        Err(err) => CheckpointSmokeReport {
            ran: true,
            passed: false,
            crlf_preserved: false,
            git_filter_or_eol_mismatch_detected: false,
            error: Some(err.to_string()),
        },
    }
}

fn run_checkpoint_smoke_inner() -> Result<CheckpointSmokeReport> {
    let root = std::env::temp_dir().join(format!(
        "squeezy-checkpoint-smoke-{}-{}",
        std::process::id(),
        now_ms()
    ));
    let result = (|| {
        fs::create_dir_all(&root)?;
        fs::write(root.join(".gitattributes"), "*.txt text eol=lf\n")?;
        fs::write(root.join("crlf.txt"), b"before\r\n")?;
        let store = CheckpointStore::open(&root)?;
        let before = store.track_tree()?;
        fs::write(root.join("crlf.txt"), b"agent\r\n")?;
        let Some(record) = store.create_checkpoint(
            &before,
            "checkpoint_doctor",
            "smoke",
            "doctor",
            "success",
            Vec::new(),
        )?
        else {
            return Err(SqueezyError::Tool(
                "checkpoint smoke did not create a checkpoint".to_string(),
            ));
        };
        let file = record
            .files
            .iter()
            .find(|file| file.path == "crlf.txt")
            .ok_or_else(|| {
                SqueezyError::Tool("checkpoint smoke did not record crlf.txt".to_string())
            })?;
        let mismatch = file.after_sha256 != file.after_worktree_sha256;
        let rollback = store.rollback(RollbackTarget::Latest, RollbackMode::Atomic)?;
        let bytes = fs::read(root.join("crlf.txt"))?;
        let crlf_preserved = bytes == b"before\r\n";
        let passed = rollback.applied && rollback.conflicts.is_empty() && crlf_preserved;
        Ok(CheckpointSmokeReport {
            ran: true,
            passed,
            crlf_preserved,
            git_filter_or_eol_mismatch_detected: mismatch,
            error: (!passed).then(|| {
                format!(
                    "applied={} conflicts={} crlf_preserved={crlf_preserved}",
                    rollback.applied,
                    rollback.conflicts.len()
                )
            }),
        })
    })();
    let _ = fs::remove_dir_all(&root);
    result
}

#[allow(dead_code)]
fn collect_workspace_file_entries(
    root: &Path,
    dir: &Path,
    entries: &mut Vec<WorkspaceFileEntry>,
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
        let metadata = fs::symlink_metadata(&path)?;
        if metadata_is_reparse_or_symlink(&metadata) {
            continue;
        }
        if metadata.is_dir() {
            collect_workspace_file_entries(root, &path, entries)?;
        } else if metadata.is_file() {
            let (mtime_secs, mtime_nanos) = mtime_parts(&metadata);
            entries.push(WorkspaceFileEntry {
                rel: rel_path(root, &path),
                absolute: path,
                size_bytes: metadata.len(),
                mtime_secs,
                mtime_nanos,
            });
        }
    }
    Ok(())
}

fn rollback_write_paths(file: &CheckpointFile) -> Vec<String> {
    let mut paths = if file.status == DiffFileStatus::Renamed {
        let mut paths = vec![file.path.clone()];
        if let Some(from_path) = file.from_path.clone() {
            paths.push(from_path);
        }
        paths
    } else {
        vec![file.path.clone()]
    };
    if let Some(group) = file.before_hardlink_paths.as_ref() {
        paths.extend(group.iter().cloned());
    }
    paths.sort();
    paths.dedup();
    paths
}

fn filesystem_preflight_conflict(
    checkpoint_id: &str,
    path: &str,
    root: &Path,
    absolute: &Path,
    allow_symlink_leaf: bool,
) -> Option<RollbackConflict> {
    if let Some(conflict) =
        reparse_path_conflict(checkpoint_id, path, root, absolute, allow_symlink_leaf)
    {
        return Some(conflict);
    }
    match fs::symlink_metadata(absolute) {
        Ok(metadata) => {
            if metadata.permissions().readonly() {
                return Some(RollbackConflict {
                    checkpoint_id: checkpoint_id.to_string(),
                    path: path.to_string(),
                    expected_sha256: None,
                    current_sha256: None,
                    expected_hash_basis: None,
                    current_hash_basis: None,
                    reason_code: Some(RollbackConflictReason::ReadOnly),
                    retryable: true,
                    reason: windows_retry_message(
                        "file is read-only; rollback would not be able to overwrite or delete it",
                    ),
                });
            }
            if metadata.is_file()
                && let Err(err) = OpenOptions::new().write(true).open(absolute)
            {
                return Some(filesystem_rollback_conflict(
                    checkpoint_id,
                    path,
                    err,
                    "preflight file writability",
                ));
            }
        }
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
            return Some(filesystem_rollback_conflict(
                checkpoint_id,
                path,
                err,
                "preflight path metadata",
            ));
        }
        Err(_) => {}
    }
    if let Some(parent) = absolute.parent()
        && let Some(conflict) = parent_writability_conflict(checkpoint_id, path, root, parent)
    {
        return Some(conflict);
    }
    None
}

fn parent_writability_conflict(
    checkpoint_id: &str,
    path: &str,
    root: &Path,
    parent: &Path,
) -> Option<RollbackConflict> {
    let mut current = parent;
    loop {
        match fs::symlink_metadata(current) {
            Ok(_) => break,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                current = current.parent()?;
            }
            Err(err) => {
                return Some(filesystem_rollback_conflict(
                    checkpoint_id,
                    path,
                    err,
                    "preflight parent metadata",
                ));
            }
        }
    }
    // `current` here is an ancestor directory of the file being restored, so a
    // symlink at this level is never an acceptable leaf — always treat it as a
    // reparse conflict (pass `false`).
    if let Some(conflict) = reparse_path_conflict(checkpoint_id, path, root, current, false) {
        return Some(conflict);
    }
    let metadata = fs::symlink_metadata(current).ok()?;
    if metadata.permissions().readonly() {
        return Some(RollbackConflict {
            checkpoint_id: checkpoint_id.to_string(),
            path: path.to_string(),
            expected_sha256: None,
            current_sha256: None,
            expected_hash_basis: None,
            current_hash_basis: None,
            reason_code: Some(RollbackConflictReason::ReadOnly),
            retryable: true,
            reason: windows_retry_message(
                "parent directory is read-only; rollback cannot create or replace the file",
            ),
        });
    }
    None
}

fn filesystem_rollback_conflict(
    checkpoint_id: &str,
    path: &str,
    err: std::io::Error,
    operation: &str,
) -> RollbackConflict {
    let reason_code = rollback_io_reason(&err);
    RollbackConflict {
        checkpoint_id: checkpoint_id.to_string(),
        path: path.to_string(),
        expected_sha256: None,
        current_sha256: None,
        expected_hash_basis: None,
        current_hash_basis: None,
        reason_code: Some(reason_code),
        // Filesystem is the catch-all arm (ENOSPC, corruption, etc.) and is
        // not retryable by closing editors or pausing sync agents.
        retryable: matches!(
            reason_code,
            RollbackConflictReason::AccessDenied
                | RollbackConflictReason::PermissionDenied
                | RollbackConflictReason::WouldBlock
                | RollbackConflictReason::ReadOnly
                | RollbackConflictReason::FileInUse
        ),
        reason: windows_retry_message(&format!("{operation} failed: {err}")),
    }
}

fn reparse_path_conflict(
    checkpoint_id: &str,
    path: &str,
    root: &Path,
    absolute: &Path,
    allow_symlink_leaf: bool,
) -> Option<RollbackConflict> {
    let relative = absolute.strip_prefix(root).ok()?;
    let components: Vec<_> = relative.components().collect();
    let last_idx = components.len().saturating_sub(1);
    let mut current = root.to_path_buf();
    for (idx, component) in components.iter().enumerate() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata_is_reparse_or_symlink(&metadata) {
                    // A symlinked *ancestor* always blocks: restoring through it
                    // could resolve outside the workspace. A symlinked *leaf*
                    // only blocks when we are not deliberately restoring a
                    // symlink to that path. Restoring a tracked symlink replaces
                    // the leaf atomically (temp sibling + rename) and never
                    // follows it, so it is safe; a leaf that became a symlink
                    // when the checkpoint expected a regular file still
                    // conflicts (`allow_symlink_leaf` is false in that case).
                    if allow_symlink_leaf && idx == last_idx {
                        continue;
                    }
                    return Some(reparse_point_conflict(checkpoint_id, path));
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => break,
            Err(err) => {
                return Some(filesystem_rollback_conflict(
                    checkpoint_id,
                    path,
                    err,
                    "inspect path for reparse point",
                ));
            }
        }
    }
    None
}

fn reparse_point_conflict(checkpoint_id: &str, path: &str) -> RollbackConflict {
    RollbackConflict {
        checkpoint_id: checkpoint_id.to_string(),
        path: path.to_string(),
        expected_sha256: None,
        current_sha256: None,
        expected_hash_basis: None,
        current_hash_basis: None,
        reason_code: Some(RollbackConflictReason::ReparsePoint),
        retryable: false,
        reason: "path is a symlink or filesystem reparse point; rollback will not follow it"
            .to_string(),
    }
}

fn metadata_is_reparse_or_symlink(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    metadata_has_windows_reparse_point(metadata)
}

#[cfg(windows)]
fn metadata_has_windows_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_has_windows_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

fn rollback_io_reason(err: &std::io::Error) -> RollbackConflictReason {
    match err.kind() {
        std::io::ErrorKind::PermissionDenied => {
            if cfg!(windows) {
                match err.raw_os_error() {
                    Some(5) => RollbackConflictReason::AccessDenied,
                    Some(32) | Some(33) => RollbackConflictReason::FileInUse,
                    _ => RollbackConflictReason::PermissionDenied,
                }
            } else {
                RollbackConflictReason::PermissionDenied
            }
        }
        std::io::ErrorKind::WouldBlock => RollbackConflictReason::WouldBlock,
        _ => RollbackConflictReason::Filesystem,
    }
}

fn windows_retry_message(detail: &str) -> String {
    if cfg!(windows) {
        format!(
            "{detail}; close editors or terminals holding the file, pause OneDrive/Defender sync if applicable, then retry /undo or inspect the file list with checkpoint_show"
        )
    } else {
        detail.to_string()
    }
}

fn path_identity_key(path: &str) -> String {
    let slash = path.replace('\\', "/");
    if cfg!(windows) {
        // Use Unicode-aware lowering so non-ASCII paths (e.g. `Über.rs`,
        // `café/index.tsx`) collapse to the same identity. NTFS's default
        // case-insensitivity uses Windows's upper-mapping rather than full
        // Unicode case folding, but `to_lowercase()` is a safe superset for
        // identity-only purposes (we only ever compare keys to other keys
        // produced by this function, never to OS-derived names).
        slash.to_lowercase()
    } else {
        slash
    }
}

fn slash_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn collect_gitattributes(root: &Path) -> Vec<String> {
    let mut paths = Vec::new();
    collect_gitattributes_inner(root, root, &mut paths);
    paths.sort();
    paths
}

fn collect_gitattributes_inner(root: &Path, dir: &Path, paths: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if name
            .to_str()
            .is_some_and(|name| matches!(name, ".git" | ".squeezy"))
        {
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata_is_reparse_or_symlink(&metadata) {
            continue;
        }
        if metadata.is_dir() {
            collect_gitattributes_inner(root, &path, paths);
        } else if metadata.is_file() && name.to_str() == Some(".gitattributes") {
            paths.push(rel_path(root, &path));
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

fn diff_raw_files(
    before: &BTreeMap<String, RawFileSnapshot>,
    after: &BTreeMap<String, RawFileSnapshot>,
) -> Vec<String> {
    let mut changed = BTreeSet::<String>::new();
    for path in before.keys().chain(after.keys()) {
        if before.get(path) != after.get(path) {
            changed.insert(path.clone());
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
                // `max_bytes` is a raw byte cap; snap it down to the nearest
                // UTF-8 char boundary so slicing never lands mid-codepoint.
                // Indices 0..=max_bytes are valid here (max_bytes <= len) and
                // 0 is always a boundary, so the search always succeeds.
                let end = (0..=max_bytes)
                    .rev()
                    .find(|&i| buffer.is_char_boundary(i))
                    .unwrap_or(0);
                buffer[..end].to_string()
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

/// Treat a user-supplied `0` as "use the built-in default" rather than
/// `0`. Used to floor [`CheckpointStoreOptions::retention_days`] and
/// [`CheckpointStoreOptions::max_file_bytes`] on store open so a config
/// with `checkpoint_retention_days = 0` does not silently disable
/// retention or shrink the per-file size cap to zero. Cleanup interval
/// `0` is intentionally a real "always clean" opt-in, so it is not run
/// through this floor.
fn nonzero_u64(value: u64, fallback: u64) -> u64 {
    if value == 0 { fallback } else { value }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
