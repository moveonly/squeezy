use std::{
    collections::HashMap,
    env,
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex as StdMutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use squeezy_core::{
    AppConfig, ContextAttachment, ContextCompactionState, CostSnapshot, ReasoningPayload,
    ReasoningSnapshot, Result, SessionMetrics, SessionMode, SqueezyError, TranscriptItem,
};

static NEXT_SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);
pub const SESSION_REPLAY_SCHEMA_VERSION: u32 = 1;
/// Schema version stamped onto every `RolloutEvent` emitted by
/// [`SessionStore::bundle_rollout_trace`]. The reducer is additive over
/// `events.jsonl` + `replay.jsonl`, so bumping this only requires changing
/// the merge logic or the wire shape of `RolloutEvent` itself.
pub const ROLLOUT_TRACE_SCHEMA_VERSION: u32 = 1;
/// Schema version stamped onto every `SessionMetadata` written via the
/// store. A missing `schema_version` field on disk is treated as v0 (the
/// pre-versioning shape); the reader runs [`SESSION_METADATA_MIGRATIONS`]
/// over the raw JSON before deserialization so older `metadata.json`
/// files keep loading after future schema changes without filename
/// sniffing.
pub const SESSION_METADATA_SCHEMA_VERSION: u32 = 1;
/// Subdirectory under the session root that holds archived sessions.
/// Sibling to live session ids; never used as a session id itself.
pub const ARCHIVED_SUBDIR: &str = "archived";

/// Rewrite the cross-project session index when it grows beyond this many
/// bytes. Append-only writes are cheap, but unbounded growth would slow
/// every `list_global_index` startup read, so the next call after the
/// cap is exceeded dedupes by `session_id` and rewrites the file with
/// the latest snapshot per session. The threshold trades a rare full
/// rewrite for a fast hot path; at ~500B/line a 256KiB cap holds roughly
/// 500 unique sessions before compaction kicks in.
pub const GLOBAL_INDEX_COMPACT_THRESHOLD_BYTES: u64 = 256 * 1024;

/// Hard ceiling on entries retained when the index is compacted. Dedup by
/// `session_id` cannot shrink a file of all-distinct sessions, so a user with
/// thousands of sessions would otherwise rewrite the entire multi-megabyte
/// index on every startup for no benefit (the original 256KiB threshold
/// silently assumed re-appends would keep the unique count near ~500). The
/// resume picker only ever surfaces the newest few within its recency window,
/// and the per-project scoped view is served by the authoritative on-disk
/// session list rather than this cross-project cache, so trimming the oldest
/// entries here is invisible to it. Sized below the byte threshold
/// (~500B/line) so a compacted index drops back under
/// [`GLOBAL_INDEX_COMPACT_THRESHOLD_BYTES`] and stops rewriting until it grows
/// again.
pub const GLOBAL_INDEX_MAX_ENTRIES: usize = 400;

static GLOBAL_INDEX_CACHE: OnceLock<StdMutex<Option<GlobalIndexCache>>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq)]
struct GlobalIndexFingerprint {
    len: u64,
    modified: Option<SystemTime>,
}

#[derive(Debug, Clone)]
struct GlobalIndexCache {
    path: PathBuf,
    fingerprint: GlobalIndexFingerprint,
    entries: Vec<GlobalSessionIndexEntry>,
}

#[derive(Debug, Clone)]
pub struct SessionStore {
    root: PathBuf,
    retention_days: u64,
    retention_archive_days: u64,
    max_event_bytes: usize,
    max_session_bytes: usize,
}

impl SessionStore {
    pub fn open(config: &AppConfig) -> Self {
        let root = session_root(config);
        Self {
            root,
            retention_days: config.session_logs.log_retention_days,
            retention_archive_days: config.session_logs.log_retention_archive_days,
            max_event_bytes: config.session_logs.max_event_bytes,
            max_session_bytes: config.session_logs.max_session_bytes,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path to the cross-session token calibration file. Lives next to the
    /// per-session directories so the same retention/cleanup story applies.
    fn calibration_path(&self) -> PathBuf {
        self.root.join("calibration.json")
    }

    /// Load the cross-session `TokenCalibration` if present. Missing or
    /// malformed files yield `TokenCalibration::default()` rather than an
    /// error: the calibration is a best-effort cache, not a source of truth.
    pub fn load_global_calibration(&self) -> squeezy_llm::TokenCalibration {
        let path = self.calibration_path();
        if !path.exists() {
            return squeezy_llm::TokenCalibration::default();
        }
        read_json(&path).unwrap_or_default()
    }

    /// Atomically persist the cross-session `TokenCalibration`. Errors are
    /// returned to the caller but expected to be logged-and-ignored — a
    /// failed write only costs us the next session's warm-start.
    pub fn save_global_calibration(
        &self,
        calibration: &squeezy_llm::TokenCalibration,
    ) -> Result<()> {
        fs::create_dir_all(&self.root)?;
        write_json(&self.calibration_path(), calibration)
    }

    /// Path to the user-global memory file. Returns `None` when `HOME` is
    /// unset — the same condition under which the agent's prompt-side
    /// ingestion (`ingest_user_memory`) declines to do anything. The file
    /// itself is the single static memory store described in
    /// `docs/internal/MEMORY_SCOPE.md`; this primitive does not introduce a
    /// new directory or partition scheme.
    pub fn memory_path() -> Option<PathBuf> {
        let home = env::var_os("HOME")?;
        Some(PathBuf::from(home).join(".squeezy").join("memory.md"))
    }

    /// Append one normalized line to the user-global memory file
    /// (`~/.squeezy/memory.md`). The input is trimmed; empty input is a
    /// no-op that returns `Ok(0)`. The store ensures the file ends with a
    /// newline before the append so each `remember` call sits on its own
    /// line and the byte-cap ingestion path stays predictable. Returns
    /// the number of bytes appended (line body plus the trailing newline).
    ///
    /// This is the canonical cross-session memory write primitive; higher
    /// layers (e.g. the deferred `memory_append` tool from
    /// `MEMORY_SCOPE.md`) wrap this rather than re-implement the path or
    /// the newline discipline.
    pub fn remember(line: &str) -> Result<usize> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Ok(0);
        }
        let Some(path) = Self::memory_path() else {
            return Err(SqueezyError::Agent(
                "remember requires HOME to be set to locate ~/.squeezy/memory.md".to_string(),
            ));
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let needs_leading_newline = match fs::metadata(&path) {
            Ok(meta) if meta.len() > 0 => !memory_file_ends_with_newline(&path)?,
            _ => false,
        };
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        let mut written = 0;
        if needs_leading_newline {
            file.write_all(b"\n")?;
            written += 1;
        }
        file.write_all(trimmed.as_bytes())?;
        file.write_all(b"\n")?;
        written += trimmed.len() + 1;
        Ok(written)
    }

    /// Read the user-global memory file (`~/.squeezy/memory.md`) and
    /// return its body truncated to `max_bytes` at a char boundary,
    /// appending `\n[truncated]` when the file is larger than the cap.
    /// Matches the semantics of the agent's prompt-side
    /// `ingest_user_memory` so call sites can rely on a single source of
    /// truth for the recall shape. Returns `None` when ingestion is
    /// disabled (`max_bytes == 0`), `HOME` is unset, or the file is
    /// absent / empty / unreadable. Errors are silent on purpose — recall
    /// is best-effort enrichment, never load-bearing.
    pub fn recall(max_bytes: usize) -> Option<String> {
        if max_bytes == 0 {
            return None;
        }
        let path = Self::memory_path()?;
        let body = fs::read_to_string(&path).ok()?;
        if body.is_empty() {
            return None;
        }
        if body.len() <= max_bytes {
            return Some(body);
        }
        let mut end = max_bytes;
        while end > 0 && !body.is_char_boundary(end) {
            end -= 1;
        }
        let mut truncated = String::with_capacity(end + "\n[truncated]".len());
        truncated.push_str(&body[..end]);
        truncated.push_str("\n[truncated]");
        Some(truncated)
    }

    /// Path to the cross-project session index. On Linux (and any platform
    /// that honours the XDG Base Directory Specification), this resolves to
    /// `$XDG_STATE_HOME/squeezy/sessions/index.jsonl`; when `XDG_STATE_HOME`
    /// is not set it falls back to `$HOME/.local/state/squeezy/sessions/index.jsonl`.
    /// For non-XDG platforms the legacy `$HOME/.squeezy/sessions/index.jsonl`
    /// path is used so that existing macOS and Windows state is undisturbed.
    ///
    /// Per-project session roots live under each workspace, so a global index
    /// is the only way the resume picker can show sessions started from sibling
    /// repos. Returns `None` when `HOME` is unset.
    pub fn global_index_path() -> Option<PathBuf> {
        xdg_global_index_path()
    }

    /// Legacy cross-project index path (`$HOME/.squeezy/sessions/index.jsonl`).
    /// Used as a migration source when the active path has moved to an XDG
    /// location; callers read from this path too so old entries remain visible
    /// after the first XDG-aware launch.
    fn legacy_global_index_path() -> Option<PathBuf> {
        let home = env::var_os("HOME")?;
        Some(
            PathBuf::from(home)
                .join(".squeezy")
                .join("sessions")
                .join("index.jsonl"),
        )
    }

    /// Append a snapshot of a session to the global cross-project index.
    /// Errors are intentionally swallowed: the index is best-effort
    /// enrichment, the per-project session store is authoritative.
    /// Append-only writes keep the hot path cheap; readers dedupe by
    /// `session_id` and compaction is deferred to `list_global_index`.
    /// An advisory exclusive lock on a sidecar `.lock` file serialises
    /// concurrent same-host appends so large multi-`write` records cannot
    /// interleave across processes.
    pub fn append_global_index_entry(entry: &GlobalSessionIndexEntry) {
        let Some(path) = Self::global_index_path() else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let Ok(mut payload) = serde_json::to_vec(entry) else {
            return;
        };
        payload.push(b'\n');
        let lock_path = path.with_extension("lock");
        let lock_file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .ok();
        if let Some(ref lf) = lock_file {
            let _ = lf.lock_exclusive();
        }
        let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) else {
            return;
        };
        let _ = file.write_all(&payload);
        if let Some(ref lf) = lock_file {
            let _ = lf.unlock();
        }
    }

    /// Read the cross-project session index, deduping by `session_id` and
    /// keeping the entry with the largest `last_event_at_ms` for each id.
    /// When the file exceeds [`GLOBAL_INDEX_COMPACT_THRESHOLD_BYTES`], the
    /// deduped snapshot is rewritten atomically (tmp + rename) so the
    /// next read stays fast. Returns entries newest-first by
    /// `started_at_ms` so callers can take a recency-prefixed slice
    /// without re-sorting.
    ///
    /// Also merges any entries from the legacy `$HOME/.squeezy/sessions/index.jsonl`
    /// path when the active index has moved to an XDG location, so sessions
    /// recorded before the migration remain visible.
    pub fn list_global_index() -> Vec<GlobalSessionIndexEntry> {
        let Some(path) = Self::global_index_path() else {
            return Vec::new();
        };
        // If neither the primary nor legacy path exists yet, there is nothing to list.
        let legacy_path = Self::legacy_global_index_path();
        let primary_exists = path.exists();
        let legacy_differs = legacy_path
            .as_ref()
            .is_some_and(|lp| *lp != path && lp.exists());
        if !primary_exists && !legacy_differs {
            return Vec::new();
        }
        let initial_fingerprint = fs::metadata(&path)
            .ok()
            .map(|metadata| global_index_fingerprint(&metadata));
        if !legacy_differs
            && let Some(fingerprint) = &initial_fingerprint
            && let Some(entries) = cached_global_index(&path, fingerprint)
        {
            return entries;
        }
        let mut by_id: HashMap<String, GlobalSessionIndexEntry> = HashMap::new();
        let mut raw_lines = 0usize;
        // Read the legacy path first (lower priority) so primary-path entries win.
        if legacy_differs && let Some(ref lp) = legacy_path {
            read_global_index_into(lp, &mut by_id, &mut raw_lines);
        }
        if primary_exists {
            read_global_index_into(&path, &mut by_id, &mut raw_lines);
        }
        let mut entries: Vec<GlobalSessionIndexEntry> = by_id.into_values().collect();
        // Drop all but the most-recent `GLOBAL_INDEX_MAX_ENTRIES` so the index
        // can never grow unbounded with a user's lifetime session count. The
        // newest-first return order below is what the picker consumes.
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.last_event_at_ms));
        let trimmed_to_cap = entries.len() > GLOBAL_INDEX_MAX_ENTRIES;
        entries.truncate(GLOBAL_INDEX_MAX_ENTRIES);
        let oversized = fs::metadata(&path)
            .map(|meta| meta.len() > GLOBAL_INDEX_COMPACT_THRESHOLD_BYTES)
            .unwrap_or(false);
        // Rewrite only when compaction actually removes lines (we trimmed past
        // the cap, or dedup collapsed duplicate appends). Rewriting an already
        // minimal, all-distinct index on every read — as the byte-threshold
        // alone did — is pure write+fsync waste that scales with history.
        // Use an exclusive lock so concurrent `list_global_index` calls from
        // different processes don't race to rewrite the same file.
        if primary_exists && oversized && (trimmed_to_cap || raw_lines > entries.len()) {
            let mut ordered: Vec<&GlobalSessionIndexEntry> = entries.iter().collect();
            // Compact in oldest-first order so future appends keep the newest
            // entries at the tail — matches how readers see time.
            ordered.sort_by_key(|entry| entry.started_at_ms);
            let lock_path = path.with_extension("lock");
            let lock_file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&lock_path)
                .ok();
            if let Some(ref lf) = lock_file {
                let _ = lf.try_lock_exclusive();
            }
            let _ = rewrite_global_index(&path, &ordered);
            if let Some(ref lf) = lock_file {
                let _ = lf.unlock();
            }
        }
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.started_at_ms));
        if !legacy_differs {
            cache_global_index(&path, &entries);
        }
        entries
    }

    /// Append the metadata snapshot to the cross-project session index.
    /// Failures are silent — see [`Self::append_global_index_entry`].
    ///
    /// Skips the write when the workspace_root is under the system temp
    /// dir but the resolved global index lives under the user's real
    /// HOME — that combination is unique to `cargo test` runs whose
    /// session stores point at sandboxed workspaces but never redirected
    /// HOME. The guard prevents test runs from polluting a developer's
    /// `~/.squeezy/sessions/index.jsonl`; tests that want to exercise
    /// the global index redirect HOME explicitly so the destination
    /// also lives under temp, and the guard becomes a no-op.
    pub(crate) fn record_global_index(metadata: &SessionMetadata) {
        if skip_global_index_for_test_workspace(&metadata.workspace_root) {
            return;
        }
        let entry = GlobalSessionIndexEntry::from_metadata(metadata, now_ms());
        Self::append_global_index_entry(&entry);
    }

    /// Start a fresh session.
    ///
    /// The handle returned is in the *pending* state: no `metadata.json`
    /// or `resume_state.json` has been written and no events.jsonl writer
    /// thread has been spawned. The on-disk session directory is created
    /// lazily by [`SessionHandle::ensure_live`], which is called the
    /// first time the handle observes a substantive append (i.e. any
    /// event whose kind is not a pure lifecycle marker like
    /// `session_started`). Quick-exit code paths — for example
    /// `squeezy --prompt --help`, which constructs an `Agent` and bails
    /// before the model loop runs — therefore leave no on-disk stub
    /// behind, while any real interaction materialises the session in
    /// place before the first substantive event is recorded.
    /// Start a session AND immediately materialise it to disk. Test
    /// fixtures and any caller that expects `list_sessions` /
    /// `SessionStore::show` to see the new session right away should
    /// use this instead of [`SessionStore::start_session`] (which is
    /// lazy per F12-pi-lazy-session-file-creation).
    pub fn start_session_eager(&self, metadata: SessionMetadata) -> Result<SessionHandle> {
        let handle = self.start_session(metadata)?;
        handle.materialize_now()?;
        Ok(handle)
    }

    pub fn start_session(&self, mut metadata: SessionMetadata) -> Result<SessionHandle> {
        metadata.session_id = next_session_id();
        metadata.started_at_ms = now_ms();
        metadata.status = SessionStatus::Running;
        metadata.resume_available = true;
        let session_id = metadata.session_id.clone();
        Ok(SessionHandle {
            store: self.clone(),
            session_id,
            counters: Arc::new(HandleCounters::default()),
            state: Arc::new(InnerStateGuard {
                inner: StdMutex::new(InnerState::Pending(Box::new(PendingState {
                    metadata,
                    buffered_events: Vec::new(),
                }))),
            }),
        })
    }

    pub fn open_session(&self, session_id: impl Into<String>) -> SessionHandle {
        let session_id = session_id.into();
        // Seed the in-memory counters from disk so the first call to
        // `append_event` after `open_session` (e.g. on resume) does not bump
        // an unrelated baseline or re-trigger the `first_user_task` capture
        // for a session that already recorded it.
        let counters = HandleCounters::default();
        if let Ok(metadata) =
            read_session_metadata(&self.session_dir(&session_id).join("metadata.json"))
        {
            counters
                .event_count
                .store(metadata.event_count, Ordering::Relaxed);
            counters
                .has_first_user_task
                .store(metadata.first_user_task.is_some(), Ordering::Relaxed);
        }
        if let Ok((replay_count, _warnings)) =
            count_replay_jsonl(&self.locate_session_dir(&session_id).join("replay.jsonl"))
        {
            counters.replay_count.store(replay_count, Ordering::Relaxed);
        }
        let dir = self.session_dir(&session_id);
        let writer = SessionLogWriter::spawn(self.clone(), dir);
        SessionHandle {
            store: self.clone(),
            session_id,
            counters: Arc::new(counters),
            state: Arc::new(InnerStateGuard {
                inner: StdMutex::new(InnerState::Live(writer)),
            }),
        }
    }

    /// Create a child session that branches off `parent_session_id`. The
    /// new session copies the parent's resume state (conversation +
    /// transcript) and any context attachments so the user can keep
    /// talking from the same point without disturbing the parent. The
    /// child's metadata carries `parent_id = Some(parent_session_id)` so
    /// the TUI session list can render fork chains. Used by `squeezy
    /// sessions fork <id>` for the offline fork path; the in-process
    /// `Agent::fork_current` writes the same `parent_id` directly on the
    /// metadata it constructs.
    pub fn fork_session(
        &self,
        parent_session_id: &str,
        mut metadata: SessionMetadata,
    ) -> Result<SessionHandle> {
        let parent_dir = self.session_dir(parent_session_id);
        if !parent_dir.exists() {
            return Err(SqueezyError::Tool(format!(
                "fork_session: parent {parent_session_id} not found at {}",
                parent_dir.display()
            )));
        }
        let parent_resume: SessionResumeState = read_json(&parent_dir.join("resume_state.json"))
            .or_else(|_| {
                // Fall back to a replay when the snapshot is missing or
                // corrupt — matches `Agent::resume`'s recovery path so an
                // intact event log keeps forks possible.
                let handle = self.open_session(parent_session_id.to_string());
                handle.replay_resume_state()
            })?;
        metadata.parent_id = Some(parent_session_id.to_string());
        let handle = self.start_session(metadata)?;
        // Route the parent resume snapshot through the handle so the
        // child session materialises (creates dir, writes metadata.json,
        // writes resume_state.json) before any sibling files are written
        // alongside it. Fork is itself a substantive event, so deferring
        // here would not save any disk activity.
        handle.write_resume_state(&parent_resume)?;
        let dir = self.session_dir(handle.session_id());
        let parent_attachments = parent_dir.join("attachments");
        if parent_attachments.exists() {
            let child_attachments = dir.join("attachments");
            fs::create_dir_all(&child_attachments)?;
            for entry in fs::read_dir(&parent_attachments)? {
                let entry = entry?;
                if !entry.file_type()?.is_file() {
                    continue;
                }
                let from = entry.path();
                let Some(name) = from.file_name() else {
                    continue;
                };
                fs::copy(&from, child_attachments.join(name))?;
            }
        }
        handle.append_event(SessionEvent::new(
            "session_forked",
            None,
            Some(format!("forked from {parent_session_id}")),
            json!({ "parent_session_id": parent_session_id }),
        ))?;
        Ok(handle)
    }

    pub fn list(&self, query: &SessionQuery) -> Result<Vec<SessionMetadata>> {
        let mut sessions = Vec::new();
        if !self.root.exists() {
            return Ok(sessions);
        }
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            // Skip the `archived/` subdir; it isn't a session, just a
            // sibling tree that holds the archived ones.
            if entry.file_name() == ARCHIVED_SUBDIR {
                continue;
            }
            let path = entry.path().join("metadata.json");
            let Ok(text) = fs::read_to_string(path) else {
                continue;
            };
            let Ok(metadata) = deserialize_session_metadata(&text) else {
                continue;
            };
            if query.matches(&metadata) {
                sessions.push(metadata);
            }
        }
        if query.include_archived || matches!(query.status, Some(SessionStatus::Archived)) {
            let archived_root = self.root.join(ARCHIVED_SUBDIR);
            if archived_root.exists() {
                for entry in fs::read_dir(&archived_root)? {
                    let entry = entry?;
                    if !entry.file_type()?.is_dir() {
                        continue;
                    }
                    let path = entry.path().join("metadata.json");
                    let Ok(text) = fs::read_to_string(path) else {
                        continue;
                    };
                    let Ok(metadata) = deserialize_session_metadata(&text) else {
                        continue;
                    };
                    if query.matches(&metadata) {
                        sessions.push(metadata);
                    }
                }
            }
        }
        sessions.sort_by_key(|session| std::cmp::Reverse(session.started_at_ms));
        Ok(sessions)
    }

    /// Move a session out of the live root into `archived/<id>/` and flip
    /// its metadata status to `Archived`. Archived sessions are excluded
    /// from `list` (unless `include_archived` is true) and skipped by
    /// `cleanup` retention sweeps. The on-disk session id is preserved so
    /// `unarchive_session` is symmetric.
    pub fn archive_session(&self, session_id: &str) -> Result<()> {
        let src = self.session_dir(session_id);
        if !src.exists() {
            return Err(SqueezyError::Tool(format!(
                "archive_session: session {session_id} not found at {}",
                src.display()
            )));
        }
        let archived_root = self.root.join(ARCHIVED_SUBDIR);
        fs::create_dir_all(&archived_root)?;
        let dest = archived_root.join(session_id);
        if dest.exists() {
            return Err(SqueezyError::Tool(format!(
                "archive_session: archived session already exists at {}",
                dest.display()
            )));
        }
        fs::rename(&src, &dest)?;
        let metadata_path = dest.join("metadata.json");
        if let Ok(text) = fs::read_to_string(&metadata_path)
            && let Ok(mut metadata) = deserialize_session_metadata(&text)
        {
            let stamp = now_ms();
            metadata.status = SessionStatus::Archived;
            metadata.archived_at_ms = Some(stamp);
            if metadata.ended_at_ms.is_none() {
                metadata.ended_at_ms = Some(stamp);
            }
            let _ = write_json(&metadata_path, &metadata);
            Self::record_global_index(&metadata);
        }
        Ok(())
    }

    /// Reverse of [`archive_session`]. Moves the session back to the live
    /// root and restores the metadata status to `Completed`.
    pub fn unarchive_session(&self, session_id: &str) -> Result<()> {
        let src = self.root.join(ARCHIVED_SUBDIR).join(session_id);
        if !src.exists() {
            return Err(SqueezyError::Tool(format!(
                "unarchive_session: archived session {session_id} not found"
            )));
        }
        let dest = self.session_dir(session_id);
        if dest.exists() {
            return Err(SqueezyError::Tool(format!(
                "unarchive_session: a live session already exists at {}",
                dest.display()
            )));
        }
        fs::rename(&src, &dest)?;
        let metadata_path = dest.join("metadata.json");
        if let Ok(text) = fs::read_to_string(&metadata_path)
            && let Ok(mut metadata) = deserialize_session_metadata(&text)
        {
            metadata.status = SessionStatus::Completed;
            metadata.archived_at_ms = None;
            let _ = write_json(&metadata_path, &metadata);
            Self::record_global_index(&metadata);
        }
        Ok(())
    }

    pub fn show(&self, session_id: &str) -> Result<SessionRecord> {
        self.read_session_record(session_id, true)
    }

    pub(crate) fn show_without_context_attachments(
        &self,
        session_id: &str,
    ) -> Result<SessionRecord> {
        self.read_session_record(session_id, false)
    }

    fn read_session_record(
        &self,
        session_id: &str,
        load_context_attachments: bool,
    ) -> Result<SessionRecord> {
        let dir = self.locate_session_dir(session_id);
        let metadata_path = dir.join("metadata.json");
        // Lazy materialisation means a session can exist in memory (held by
        // a live `SessionHandle`) without any on-disk footprint until its
        // first substantive event. From the store's perspective those
        // sessions are simply not visible; return a clean "not found"
        // error rather than letting the underlying `read_to_string`
        // failure surface as a generic IO error.
        if !metadata_path.exists() {
            return Err(SqueezyError::Tool(format!(
                "session {session_id} not found (no metadata.json at {})",
                metadata_path.display(),
            )));
        }
        let metadata = read_session_metadata(&metadata_path)?;
        let (events, event_warnings) = read_jsonl(&dir.join("events.jsonl"))?;
        let resume_state = read_json(&dir.join("resume_state.json")).ok();
        let attachments = if load_context_attachments {
            read_context_attachments(&dir.join("attachments"))?
        } else {
            Vec::new()
        };
        let replay = self.replay_tape(session_id).ok();
        Ok(SessionRecord {
            metadata,
            events,
            event_warnings,
            resume_state,
            attachments,
            replay,
        })
    }

    /// Read just `metadata.json` for `session_id` without loading events,
    /// the resume snapshot, attachments, or the replay tape. Used by the
    /// CLI cross-project resume confirmation prompt where only
    /// `metadata.cwd` is needed and pulling in megabytes of events would
    /// be wasteful right before the TUI resumes the session anyway.
    /// Resolves through `locate_session_dir` so archived sessions stay
    /// readable.
    pub fn read_metadata(&self, session_id: &str) -> Result<SessionMetadata> {
        let dir = self.locate_session_dir(session_id);
        let metadata_path = dir.join("metadata.json");
        if !metadata_path.exists() {
            return Err(SqueezyError::Tool(format!(
                "session {session_id} not found (no metadata.json at {})",
                metadata_path.display(),
            )));
        }
        read_json(&metadata_path)
    }

    pub fn replay_tape(&self, session_id: &str) -> Result<SessionReplayTape> {
        let (events, warnings) =
            read_replay_jsonl(&self.locate_session_dir(session_id).join("replay.jsonl"))?;
        Ok(SessionReplayTape {
            schema_version: SESSION_REPLAY_SCHEMA_VERSION,
            session_id: session_id.to_string(),
            events,
            warnings,
        })
    }

    /// Resolve the on-disk directory for a session whether it currently
    /// lives under the live root or under `archived/<id>/`. Used by
    /// read-only callers (`show`, `replay_tape`) so an archived session
    /// stays inspectable; producers continue to use [`Self::session_dir`]
    /// so they create new sessions under the live root.
    fn locate_session_dir(&self, session_id: &str) -> PathBuf {
        let live = self.session_dir(session_id);
        if live.exists() {
            return live;
        }
        let archived = self.root.join(ARCHIVED_SUBDIR).join(session_id);
        if archived.exists() {
            return archived;
        }
        // No directory exists at either location. Return the live path
        // so downstream `read_*` calls produce a consistent "not found"
        // error rather than a spurious archive-tree error message.
        live
    }

    /// Merge the session's `events.jsonl` and `replay.jsonl` into a single
    /// ordered, normalized [`RolloutEvent`] stream. Stable-sorted on
    /// `(ts_unix_ms, source, sequence_or_insertion)`; within a tied
    /// millisecond the replay tape sorts before the lifecycle event.
    pub fn bundle_rollout_trace(&self, session_id: &str) -> Result<Vec<RolloutEvent>> {
        let dir = self.locate_session_dir(session_id);
        let (events, _event_warnings) = read_jsonl(&dir.join("events.jsonl"))?;
        let (replay, _replay_warnings) = read_replay_jsonl(&dir.join("replay.jsonl"))?;

        let mut bundle: Vec<RolloutEvent> = Vec::with_capacity(events.len() + replay.len());
        for (insertion, event) in events.into_iter().enumerate() {
            bundle.push(RolloutEvent::from_session_event(event, insertion));
        }
        for replay_event in replay {
            bundle.push(RolloutEvent::from_replay_event(replay_event));
        }
        bundle.sort_by(|left, right| {
            left.ts_unix_ms
                .cmp(&right.ts_unix_ms)
                .then_with(|| left.source_order().cmp(&right.source_order()))
                .then_with(|| left.tie_breaker().cmp(&right.tie_breaker()))
        });
        Ok(bundle)
    }

    pub fn export(&self, session_id: &str) -> Result<Value> {
        let record = self.show(session_id)?;
        Ok(json!({
            "metadata": record.metadata,
            "events": record.events,
            "event_warnings": record.event_warnings,
            "replay": record.replay,
            "attachments": record.attachments,
            "resume_available": record
                .resume_state
                .as_ref()
                .is_some_and(|state| state.resume_available),
        }))
    }

    pub fn cleanup(&self, ids: &[String]) -> Result<CleanupReport> {
        self.cleanup_excluding(ids, None)
    }

    /// Like [`cleanup`] but skips `protected_id` even if it would otherwise
    /// match (used to keep the currently active session from being removed
    /// out from under a live agent).
    ///
    /// Defaults to [`CleanupMode::Archive`]: live sessions that expire or are
    /// explicitly named in `ids` are moved into `archived/<id>/`. Use
    /// [`Self::cleanup_with`] with [`CleanupMode::Purge`] to hard-delete
    /// instead.
    pub fn cleanup_excluding(
        &self,
        ids: &[String],
        protected_id: Option<&str>,
    ) -> Result<CleanupReport> {
        self.cleanup_with(ids, protected_id, CleanupMode::Archive)
    }

    /// Run the cleanup sweep with explicit control over the soft-archive vs
    /// hard-delete decision.
    ///
    /// [`CleanupMode::Archive`] (the default) moves expired or explicitly
    /// named live sessions into `archived/<id>/` and flips their status to
    /// [`SessionStatus::Archived`]. They survive until the archive retention
    /// sweep removes them after `retention_archive_days`. This gives users a
    /// window to recover a session that the retention policy would otherwise
    /// destroy: live retention reduces disk pressure, archive retention
    /// bounds the recoverable history. Setting `retention_archive_days` to
    /// `0` disables the archive sweep so archived sessions are kept until
    /// the user removes them by hand.
    ///
    /// [`CleanupMode::Purge`] skips the soft-archive step and hard-deletes
    /// live sessions outright. Sessions already in `archived/<id>/` are also
    /// hard-deleted irrespective of `retention_archive_days`, so `--purge`
    /// is the explicit "I want this gone" escape hatch from the
    /// archive-by-default policy.
    pub fn cleanup_with(
        &self,
        ids: &[String],
        protected_id: Option<&str>,
        mode: CleanupMode,
    ) -> Result<CleanupReport> {
        let mut archived = Vec::new();
        let mut removed = Vec::new();
        let cutoff = now_ms().saturating_sub(self.retention_days.saturating_mul(86_400_000));
        let explicit: std::collections::BTreeSet<&str> = ids.iter().map(String::as_str).collect();
        for metadata in self.list(&SessionQuery {
            include_archived: true,
            ..SessionQuery::default()
        })? {
            if protected_id == Some(metadata.session_id.as_str()) {
                continue;
            }
            if matches!(metadata.status, SessionStatus::Archived) {
                let is_explicit = explicit.contains(metadata.session_id.as_str());
                // `--purge` hard-deletes archived sessions regardless of
                // archive retention so the user has an explicit "I want
                // this gone now" path. The `Archive` default mode keeps
                // them around until the retention sweep removes them.
                let force_remove = matches!(mode, CleanupMode::Purge) && is_explicit;
                if force_remove {
                    let dir = self.root.join(ARCHIVED_SUBDIR).join(&metadata.session_id);
                    if dir.exists() {
                        fs::remove_dir_all(&dir)?;
                    }
                    removed.push(metadata.session_id);
                    continue;
                }
                if self.retention_archive_days == 0 {
                    continue;
                }
                let archive_cutoff =
                    now_ms().saturating_sub(self.retention_archive_days.saturating_mul(86_400_000));
                // Prefer the dedicated archival timestamp. Older metadata
                // files written before `archived_at_ms` existed fall back
                // to `ended_at_ms` (set when `archive_session` flips the
                // status) and finally `started_at_ms` so the sweep keeps
                // working on legacy on-disk data.
                let archived_at = metadata
                    .archived_at_ms
                    .or(metadata.ended_at_ms)
                    .unwrap_or(metadata.started_at_ms);
                if archived_at < archive_cutoff {
                    let dir = self.root.join(ARCHIVED_SUBDIR).join(&metadata.session_id);
                    if dir.exists() {
                        fs::remove_dir_all(&dir)?;
                    }
                    removed.push(metadata.session_id);
                }
                continue;
            }
            let is_explicit = explicit.contains(metadata.session_id.as_str());
            // Never sweep a `Running` session through retention alone: it may
            // belong to a long-lived process whose `ended_at_ms` simply isn't
            // set yet. Explicit ids still win so users can force-archive a
            // crashed or stuck session.
            let expired = match metadata.ended_at_ms {
                Some(end) => end < cutoff,
                None => {
                    !matches!(metadata.status, SessionStatus::Running)
                        && metadata.started_at_ms < cutoff
                }
            };
            if is_explicit || expired {
                match mode {
                    CleanupMode::Archive => {
                        // `archive_session` is idempotent for the live ->
                        // archived move; a destination collision means
                        // another caller raced us, which we surface so the
                        // operator can investigate.
                        self.archive_session(&metadata.session_id)?;
                        archived.push(metadata.session_id);
                    }
                    CleanupMode::Purge => {
                        let dir = self.session_dir(&metadata.session_id);
                        if dir.exists() {
                            fs::remove_dir_all(&dir)?;
                        }
                        removed.push(metadata.session_id);
                    }
                }
            }
        }
        Ok(CleanupReport { archived, removed })
    }

    /// Resolve a free-form session id *prefix* to the full session id of
    /// the matching session. An exact match always wins — so a full id
    /// is returned verbatim even if it would also be a prefix of a
    /// longer id — and ties on the prefix produce
    /// [`ResolveError::AmbiguousPrefix`] with every candidate listed so
    /// the CLI can render an actionable disambiguation hint.
    ///
    /// Both the live root and the `archived/` subtree are searched so
    /// `squeezy sessions resume abc12` works the same way for a recent
    /// session and for one that has since been soft-archived. The empty
    /// prefix is rejected as [`ResolveError::NotFound`] rather than
    /// silently picking an arbitrary session — accidentally typing
    /// `squeezy sessions resume ""` should fail loudly.
    ///
    /// Filesystem failures (permission errors, unreadable directories)
    /// are surfaced as [`ResolveError::Io`]; missing roots are not an
    /// error because a fresh install simply has no sessions yet.
    pub fn resolve_session_id_prefix(
        &self,
        prefix: &str,
    ) -> std::result::Result<String, ResolveError> {
        if prefix.is_empty() {
            return Err(ResolveError::NotFound {
                prefix: prefix.to_string(),
            });
        }
        let ids = self.collect_session_ids()?;
        if let Some(exact) = ids.iter().find(|id| id.as_str() == prefix) {
            return Ok(exact.clone());
        }
        let mut matches: Vec<String> = ids
            .into_iter()
            .filter(|id| id.starts_with(prefix))
            .collect();
        matches.sort();
        match matches.len() {
            0 => Err(ResolveError::NotFound {
                prefix: prefix.to_string(),
            }),
            1 => Ok(matches.into_iter().next().expect("len == 1")),
            _ => Err(ResolveError::AmbiguousPrefix {
                prefix: prefix.to_string(),
                matches,
            }),
        }
    }

    /// Enumerate every session id known to this store across the live
    /// root and the `archived/` subtree. Missing roots silently return
    /// an empty list — a brand-new install has no sessions yet and that
    /// should not be a hard error. The list mirrors what
    /// [`resolve_session_id_prefix`] needs and stays intentionally tiny
    /// (no metadata reads): the resolver only cares about directory
    /// names.
    fn collect_session_ids(&self) -> std::result::Result<Vec<String>, ResolveError> {
        let mut ids = Vec::new();
        if self.root.exists() {
            for entry in fs::read_dir(&self.root)? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                if entry.file_name() == ARCHIVED_SUBDIR {
                    continue;
                }
                if let Some(name) = entry.file_name().to_str() {
                    ids.push(name.to_string());
                }
            }
        }
        let archived_root = self.root.join(ARCHIVED_SUBDIR);
        if archived_root.exists() {
            for entry in fs::read_dir(&archived_root)? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                if let Some(name) = entry.file_name().to_str() {
                    ids.push(name.to_string());
                }
            }
        }
        Ok(ids)
    }

    /// Soft-delete that prefers archiving over permanent removal.
    /// Live sessions are moved into `archived/<id>/` (same path as
    /// [`archive_session`]); archived sessions are left in place because
    /// the retention sweep is the only path that permanently deletes
    /// history. Missing sessions are a no-op so callers can drive this
    /// from a stale id without erroring.
    pub fn remove_session(&self, session_id: &str) -> Result<()> {
        let live_dir = self.session_dir(session_id);
        if live_dir.exists() {
            return self.archive_session(session_id);
        }
        // Already archived (or never existed) — nothing to do. The
        // archive retention sweep handles the eventual hard delete.
        Ok(())
    }

    fn session_dir(&self, session_id: &str) -> PathBuf {
        self.root.join(session_id)
    }
}

#[derive(Debug, Clone)]
pub struct SessionHandle {
    store: SessionStore,
    session_id: String,
    // Process-local counters shared by every clone of the handle so we can
    // avoid the read-mutate-write of `metadata.json` for routine events that
    // don't change any user-visible discovery field.
    counters: Arc<HandleCounters>,
    /// Lazy materialisation state. Shared by every clone of the handle so
    /// the first substantive append from any clone promotes the entire
    /// session to the on-disk Live form.
    state: Arc<InnerStateGuard>,
}

#[derive(Debug, Default)]
struct HandleCounters {
    event_count: AtomicU64,
    replay_count: AtomicU64,
    has_first_user_task: AtomicBool,
}

/// Lazy materialisation state for a `SessionHandle`.
///
/// A fresh session begins as [`InnerState::Pending`] — the metadata
/// lives in memory and no `metadata.json` / `resume_state.json` has
/// been written. The first substantive event (or an explicit
/// materialising call such as `write_resume_state`) promotes the
/// session to [`InnerState::Live`], at which point the on-disk
/// session directory is created, the writer thread is spawned, and
/// any buffered lifecycle events are flushed in arrival order.
///
/// Sessions opened via [`SessionStore::open_session`] start out
/// [`InnerState::Live`] because the on-disk artefact already exists.
#[derive(Debug)]
struct InnerStateGuard {
    inner: StdMutex<InnerState>,
}

#[derive(Debug)]
enum InnerState {
    // `PendingState` is large (it holds an entire `SessionMetadata`
    // and a `Vec<SessionEvent>` buffer) compared to the other variants,
    // so box it to keep `InnerState` itself a small tagged pointer.
    // Promotion is rare enough that the extra indirection is irrelevant
    // next to the cost of writing `metadata.json`.
    Pending(Box<PendingState>),
    Live(Arc<SessionLogWriter>),
    /// Sentinel observed only inside [`SessionHandle::ensure_live`]
    /// while the pending state is being moved out of the mutex during
    /// promotion. The mutex is held throughout the transition so no
    /// other caller can ever read this variant.
    Transitioning,
}

#[derive(Debug)]
struct PendingState {
    /// Metadata snapshot built by `start_session`. Updated in place
    /// by `update_metadata` while the session remains pending; written
    /// to `metadata.json` on promotion.
    metadata: SessionMetadata,
    /// Lifecycle-only events (e.g. `session_started`) appended before
    /// the session materialised. Flushed in arrival order through the
    /// writer on promotion. The in-memory counters track them too, so
    /// the handle's view of `event_count` stays consistent across the
    /// promotion boundary.
    buffered_events: Vec<SessionEvent>,
}

/// Event kinds that on their own should not promote a pending session
/// to live: they are pure lifecycle bookkeeping and represent no real
/// interaction. Any other kind triggers materialisation so the durable
/// log captures it from the first byte forward.
fn is_substantive_event_kind(kind: &str) -> bool {
    !matches!(
        kind,
        "session_started" | "session_resumed" | "session_ended"
    )
}

#[derive(Debug)]
struct SessionLogAppend {
    payload: Vec<u8>,
}

enum SessionLogCmd {
    Append(SessionLogAppend),
    Flush { ack: mpsc::Sender<Result<()>> },
    Shutdown { ack: mpsc::Sender<Result<()>> },
}

#[derive(Debug)]
struct SessionLogWriter {
    tx: mpsc::Sender<SessionLogCmd>,
    worker: StdMutex<Option<thread::JoinHandle<()>>>,
    terminal_failure: StdMutex<Option<String>>,
}

impl SessionLogWriter {
    fn spawn(store: SessionStore, dir: PathBuf) -> Arc<Self> {
        let (tx, rx) = mpsc::channel();
        let terminal_failure = StdMutex::new(None);
        let writer = Arc::new(Self {
            tx,
            worker: StdMutex::new(None),
            terminal_failure,
        });
        let failure = Arc::downgrade(&writer);
        let worker = thread::spawn(move || {
            run_session_log_writer(store, dir, rx, failure);
        });
        *writer.worker.lock().expect("session log writer worker") = Some(worker);
        writer
    }

    fn append(&self, append: SessionLogAppend) -> Result<()> {
        self.check_failure()?;
        self.tx
            .send(SessionLogCmd::Append(append))
            .map_err(|_| SqueezyError::Agent("session log writer stopped".to_string()))?;
        self.check_failure()
    }

    fn flush(&self) -> Result<()> {
        self.check_failure()?;
        let (ack, rx) = mpsc::channel();
        self.tx
            .send(SessionLogCmd::Flush { ack })
            .map_err(|_| SqueezyError::Agent("session log writer stopped".to_string()))?;
        rx.recv()
            .map_err(|_| SqueezyError::Agent("session log writer stopped".to_string()))?
    }

    fn record_failure(&self, error: impl ToString) {
        let mut failure = self
            .terminal_failure
            .lock()
            .expect("session log writer failure");
        if failure.is_none() {
            *failure = Some(error.to_string());
        }
    }

    fn check_failure(&self) -> Result<()> {
        if let Some(error) = self
            .terminal_failure
            .lock()
            .expect("session log writer failure")
            .clone()
        {
            return Err(SqueezyError::Io(std::io::Error::other(error)));
        }
        Ok(())
    }
}

impl Drop for SessionLogWriter {
    fn drop(&mut self) {
        let (ack, rx) = mpsc::channel();
        let _ = self.tx.send(SessionLogCmd::Shutdown { ack });
        let _ = rx.recv();
        if let Some(worker) = self
            .worker
            .lock()
            .expect("session log writer worker")
            .take()
        {
            let _ = worker.join();
        }
    }
}

fn run_session_log_writer(
    store: SessionStore,
    dir: PathBuf,
    rx: mpsc::Receiver<SessionLogCmd>,
    writer: std::sync::Weak<SessionLogWriter>,
) {
    let path = dir.join("events.jsonl");
    let mut current_size = fs::metadata(&path).map_or(0, |metadata| metadata.len() as usize);
    let mut terminal_failure: Option<String> = None;
    let mut truncated = false;
    for command in rx {
        match command {
            SessionLogCmd::Append(append) => {
                if terminal_failure.is_some() {
                    continue;
                }
                if let Err(error) = write_session_log_append(
                    &store,
                    &dir,
                    &path,
                    &mut current_size,
                    &mut truncated,
                    append,
                ) {
                    let message = error.to_string();
                    if let Some(writer) = writer.upgrade() {
                        writer.record_failure(&message);
                    }
                    terminal_failure = Some(message);
                }
            }
            SessionLogCmd::Flush { ack } => {
                let _ = ack.send(session_log_writer_result(terminal_failure.as_deref()));
            }
            SessionLogCmd::Shutdown { ack } => {
                let _ = ack.send(session_log_writer_result(terminal_failure.as_deref()));
                break;
            }
        }
    }
}

fn session_log_writer_result(failure: Option<&str>) -> Result<()> {
    if let Some(failure) = failure {
        return Err(SqueezyError::Io(std::io::Error::other(failure.to_string())));
    }
    Ok(())
}

fn write_session_log_append(
    store: &SessionStore,
    dir: &Path,
    path: &Path,
    current_size: &mut usize,
    truncated: &mut bool,
    append: SessionLogAppend,
) -> Result<()> {
    fs::create_dir_all(dir)?;
    if current_size.saturating_add(append.payload.len()) > store.max_session_bytes {
        // Record the truncation transition exactly once. `current_size` only
        // ever grows, so every later append would otherwise re-enter this
        // branch and rewrite byte-identical metadata.json for the rest of the
        // session.
        if !*truncated {
            update_metadata_file(dir, |metadata| {
                metadata.status = SessionStatus::Truncated;
                metadata.resume_available = false;
                metadata.resume_unavailable_reason =
                    Some("session exceeded max_session_bytes".to_string());
            })?;
            *truncated = true;
        }
        return Ok(());
    }
    append_payload_with_recovery(path, &append.payload, current_size)?;
    Ok(())
}

fn append_payload_with_recovery(
    path: &Path,
    payload: &[u8],
    current_size: &mut usize,
) -> std::io::Result<()> {
    match append_payload_once(path, payload) {
        Ok(written) => {
            *current_size = current_size.saturating_add(written);
            Ok(())
        }
        Err((written, first_error)) => {
            *current_size = current_size.saturating_add(written);
            if written > 0 {
                return Err(first_error);
            }
            match append_payload_once(path, payload) {
                Ok(written) => {
                    *current_size = current_size.saturating_add(written);
                    Ok(())
                }
                Err((retry_written, retry_error)) => {
                    *current_size = current_size.saturating_add(retry_written);
                    Err(retry_error)
                }
            }
        }
    }
}

fn append_payload_once(
    path: &Path,
    payload: &[u8],
) -> std::result::Result<usize, (usize, std::io::Error)> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| (0, error))?;
    let mut written = 0;
    while written < payload.len() {
        match file.write(&payload[written..]) {
            Ok(0) => {
                return Err((
                    written,
                    std::io::Error::new(std::io::ErrorKind::WriteZero, "failed to write event"),
                ));
            }
            Ok(bytes) => written += bytes,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err((written, error)),
        }
    }
    Ok(written)
}

fn update_metadata_file(dir: &Path, update: impl FnOnce(&mut SessionMetadata)) -> Result<()> {
    let path = dir.join("metadata.json");
    let mut metadata = read_session_metadata(&path)?;
    update(&mut metadata);
    write_json(&path, &metadata)
}

impl SessionHandle {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn metadata(&self) -> Result<SessionMetadata> {
        let pending_snapshot = {
            let guard = self.state.inner.lock().expect("session handle state");
            match &*guard {
                InnerState::Pending(pending) => Some(pending.metadata.clone()),
                InnerState::Live(_) => None,
                InnerState::Transitioning => {
                    unreachable!("SessionHandle observed Transitioning state outside ensure_live")
                }
            }
        };
        let mut metadata = match pending_snapshot {
            Some(metadata) => metadata,
            None => read_session_metadata(&self.dir().join("metadata.json"))?,
        };
        // Surface the in-memory event_count even when we have intentionally
        // skipped writing metadata.json for routine events.
        let cached = self.counters.event_count.load(Ordering::Relaxed);
        if cached > metadata.event_count {
            metadata.event_count = cached;
        }
        Ok(metadata)
    }

    pub fn update_metadata(&self, update: impl FnOnce(&mut SessionMetadata)) -> Result<()> {
        // Pending sessions keep their metadata fully in memory; mutate in
        // place and let materialisation flush it to disk later.
        {
            let mut guard = self.state.inner.lock().expect("session handle state");
            if let InnerState::Pending(pending) = &mut *guard {
                update(&mut pending.metadata);
                return Ok(());
            }
        }
        let mut metadata = self.metadata()?;
        update(&mut metadata);
        write_json(&self.dir().join("metadata.json"), &metadata)
    }

    /// Like [`Self::update_metadata`] but also refreshes the
    /// cross-project global index entry so user-facing fields
    /// (`display_name`, `labels`, …) become visible to the resume
    /// picker without waiting for the next session to start. Returns
    /// the post-update metadata snapshot. Used by user-initiated
    /// metadata mutations (`/session rename`, `/session label`) where
    /// the picker UX depends on the change propagating immediately.
    ///
    /// Internal lifecycle writes that mutate metadata as a side effect
    /// (status transitions, calibration EMA updates, …) keep calling
    /// [`Self::update_metadata`] directly: the global index refresh
    /// pattern only matters when the new value affects the cross-project
    /// picker's row label.
    pub fn update_metadata_and_index(
        &self,
        update: impl FnOnce(&mut SessionMetadata),
    ) -> Result<SessionMetadata> {
        self.update_metadata(update)?;
        let snapshot = self.metadata()?;
        SessionStore::record_global_index(&snapshot);
        Ok(snapshot)
    }

    pub fn flush_events(&self) -> Result<()> {
        let writer = {
            let guard = self.state.inner.lock().expect("session handle state");
            match &*guard {
                // Pending sessions have nothing on disk and no writer thread
                // — a flush is a no-op rather than an error.
                InnerState::Pending(_) => return Ok(()),
                InnerState::Live(writer) => writer.clone(),
                InnerState::Transitioning => unreachable!(),
            }
        };
        writer.flush()
    }

    /// Promote a pending session to the live form: create the session
    /// directory, write `metadata.json` and `resume_state.json`, record
    /// the cross-project global index entry, spawn the events.jsonl
    /// writer thread, and flush any buffered lifecycle events through
    /// the writer in arrival order. No-op when the session is already
    /// live.
    ///
    /// Returns an Arc clone of the writer so the caller can immediately
    /// queue further appends without re-locking the state mutex.
    /// Materialise a pending session to disk (writes `metadata.json` +
    /// `resume_state.json`, records the global index entry, spawns the
    /// writer). Idempotent on already-live sessions. Use this when a
    /// caller wants `SessionStore::show(...)` to succeed before any
    /// substantive event has been appended (e.g., the agent's
    /// `show_session` API surface).
    pub fn materialize_now(&self) -> Result<()> {
        self.ensure_live().map(|_| ())
    }

    fn ensure_live(&self) -> Result<Arc<SessionLogWriter>> {
        let mut guard = self.state.inner.lock().expect("session handle state");
        match &*guard {
            InnerState::Live(writer) => return Ok(writer.clone()),
            InnerState::Pending(_) => {}
            InnerState::Transitioning => {
                unreachable!("ensure_live observed Transitioning state under its own lock")
            }
        }
        let pending = match std::mem::replace(&mut *guard, InnerState::Transitioning) {
            InnerState::Pending(pending) => pending,
            _ => unreachable!(),
        };
        let dir = self.store.session_dir(&self.session_id);
        let prepare = || -> Result<()> {
            fs::create_dir_all(&dir)?;
            write_json(&dir.join("metadata.json"), &pending.metadata)?;
            write_json(
                &dir.join("resume_state.json"),
                &SessionResumeState {
                    resume_available: pending.metadata.resume_available,
                    ..SessionResumeState::default()
                },
            )?;
            Ok(())
        };
        if let Err(error) = prepare() {
            // Restore the pending state so the caller can retry without
            // losing the metadata snapshot or any buffered events.
            *guard = InnerState::Pending(pending);
            return Err(error);
        }
        SessionStore::record_global_index(&pending.metadata);
        let writer = SessionLogWriter::spawn(self.store.clone(), dir);
        // Replay buffered lifecycle events through the writer so any
        // pre-promotion bookkeeping (`session_started`, …) lands in
        // events.jsonl before the substantive event that triggered
        // promotion does. We hold the state mutex throughout, so no
        // other caller can interleave a fresh append between the
        // buffer flush and the state flip.
        for event in &pending.buffered_events {
            let payload = match serialize_event_payload(event, self.store.max_event_bytes) {
                Ok(payload) => payload,
                Err(_) => continue,
            };
            let _ = writer.append(SessionLogAppend { payload });
        }
        *guard = InnerState::Live(writer.clone());
        Ok(writer)
    }

    /// Typed convenience wrapper for [`append_event`]. Producers that want
    /// compile-time guarantees that the kind discriminator and payload
    /// shape stay in sync can construct a [`SessionEventKind`] and let the
    /// store serialise it. The on-disk format is identical to the
    /// string-tagged path so readers (replay, bug-report, telemetry) do
    /// not need to special-case typed appends.
    pub fn append_typed_event(
        &self,
        kind: SessionEventKind,
        turn_id: Option<String>,
        summary: Option<String>,
    ) -> Result<()> {
        self.append_event(SessionEvent::from_typed(kind, turn_id, summary))
    }

    pub fn append_event(&self, event: SessionEvent) -> Result<()> {
        let event_kind = event.kind.clone();
        let event_summary = event.summary.clone();
        let substantive = is_substantive_event_kind(&event_kind);

        // Lifecycle-only events on a pending session do not promote the
        // session; they are buffered in memory and either flushed at
        // materialisation time (when a substantive event arrives) or
        // discarded when the handle drops without ever materialising.
        // This is what keeps quick-exit code paths — `squeezy --prompt
        // --help` and friends — from leaving a stub session directory
        // behind on disk.
        if !substantive {
            let mut guard = self.state.inner.lock().expect("session handle state");
            if let InnerState::Pending(pending) = &mut *guard {
                let new_count = self.counters.event_count.fetch_add(1, Ordering::Relaxed) + 1;
                pending.metadata.event_count = new_count;
                pending.buffered_events.push(event);
                return Ok(());
            }
        }

        // Otherwise: substantive append (or a lifecycle event on an
        // already-live session). Make sure the on-disk artefact exists
        // before queueing this event, so buffered lifecycle events land
        // in events.jsonl first and the ordering invariant holds.
        let writer = self.ensure_live()?;

        let mut payload = to_json_vec(&event)?;
        if payload.len() > self.store.max_event_bytes {
            payload = to_json_vec(&SessionEvent {
                ts_unix_ms: event.ts_unix_ms,
                kind: event.kind,
                turn_id: event.turn_id,
                summary: event.summary,
                payload: json!({
                    "truncated": true,
                    "reason": "event exceeded max_event_bytes",
                    "original_bytes": payload.len(),
                }),
                parent_event_sequence: event.parent_event_sequence,
            })?;
        }
        payload.push(b'\n');

        writer.append(SessionLogAppend { payload })?;
        // Hot-path bookkeeping lives in memory: the on-disk event_count is
        // resynced lazily during `metadata()` / `update_metadata`, and the
        // metadata write below only fires when a discovery-visible field is
        // actually about to change.
        let new_count = self.counters.event_count.fetch_add(1, Ordering::Relaxed) + 1;
        let set_first_user_task = event_kind == "user_message"
            && !self
                .counters
                .has_first_user_task
                .swap(true, Ordering::AcqRel);
        let set_latest_summary = matches!(
            event_kind.as_str(),
            "assistant_completed" | "failed" | "cancelled"
        );

        if set_first_user_task || set_latest_summary {
            self.update_metadata(|metadata| {
                metadata.event_count = new_count;
                if set_first_user_task {
                    metadata.first_user_task = event_summary.clone();
                }
                if set_latest_summary {
                    metadata.latest_summary = event_summary;
                }
            })?;
            // Mirror the title-bearing snapshot into the cross-project
            // index so the resume picker can surface this session from
            // sibling repos. Cheap read of metadata.json keeps the
            // global index decoupled from the in-memory counters.
            if let Ok(metadata) = self.metadata() {
                SessionStore::record_global_index(&metadata);
            }
        }
        Ok(())
    }

    pub fn append_replay_event(&self, mut event: SessionReplayEvent) -> Result<()> {
        // Replay events describe model interaction; they are always
        // substantive enough to promote a pending session to live so
        // the events.jsonl + replay.jsonl pair stays consistent.
        let _ = self.ensure_live()?;
        let dir = self.dir();
        fs::create_dir_all(&dir)?;
        let path = dir.join("replay.jsonl");
        event.sequence = self.counters.replay_count.fetch_add(1, Ordering::Relaxed) + 1;
        let mut payload = to_json_vec(&event)?;
        if payload.len() > self.store.max_event_bytes {
            event.payload = json!({
                    "truncated": true,
                    "reason": "replay event exceeded max_event_bytes",
                    "original_bytes": payload.len(),
            });
            event.payload_sha256 = replay_payload_sha256(&event.payload);
            payload = to_json_vec(&event)?;
        }
        payload.push(b'\n');

        let current_size = fs::metadata(&path).map_or(0, |metadata| metadata.len() as usize);
        if current_size.saturating_add(payload.len()) > self.store.max_session_bytes {
            self.update_metadata(|metadata| {
                metadata.status = SessionStatus::Truncated;
                metadata.resume_available = false;
                metadata.resume_unavailable_reason =
                    Some("replay trace exceeded max_session_bytes".to_string());
            })?;
            return Ok(());
        }

        let mut size = current_size;
        append_payload_with_recovery(&path, &payload, &mut size)?;
        Ok(())
    }

    pub fn write_resume_state(&self, state: &SessionResumeState) -> Result<()> {
        // Materialise before writing: an explicit resume checkpoint means
        // the caller wants this persisted, so there is no benefit to
        // continuing to defer the session directory.
        let _ = self.ensure_live()?;
        write_json(&self.dir().join("resume_state.json"), state)
    }

    pub fn read_resume_state(&self) -> Result<SessionResumeState> {
        // A pending session has no `resume_state.json` on disk yet but
        // is implicitly resumable (no events have been recorded, so
        // resuming yields an empty conversation). Surface that view as
        // the default snapshot rather than erroring with "file not
        // found".
        let pending_snapshot = {
            let guard = self.state.inner.lock().expect("session handle state");
            match &*guard {
                InnerState::Pending(pending) => Some(SessionResumeState {
                    resume_available: pending.metadata.resume_available,
                    ..SessionResumeState::default()
                }),
                InnerState::Live(_) => None,
                InnerState::Transitioning => unreachable!(),
            }
        };
        if let Some(state) = pending_snapshot {
            return Ok(state);
        }
        read_json(&self.dir().join("resume_state.json"))
    }

    /// Replay `events.jsonl` to reconstruct a `SessionResumeState`. Used as
    /// the fallback when `resume_state.json` is missing or marks the session
    /// non-resumable but the event log on disk is intact. Walks newest-to-
    /// oldest first to find the most recent `ContextCompacted` event that
    /// carries a `conversation` snapshot; replay starts from that snapshot
    /// (snap-to-checkpoint) and forward-applies only the newer events. When
    /// no checkpoint is found, replay starts from an empty conversation.
    pub fn replay_resume_state(&self) -> Result<SessionResumeState> {
        // Pending sessions have no events.jsonl yet; surface a default
        // empty resume state rather than letting `read_jsonl` propagate
        // an arbitrary IO error from the missing file.
        {
            let guard = self.state.inner.lock().expect("session handle state");
            if let InnerState::Pending(pending) = &*guard {
                return Ok(SessionResumeState {
                    resume_available: pending.metadata.resume_available,
                    ..SessionResumeState::default()
                });
            }
        }
        let (events, _warnings) = read_jsonl(&self.dir().join("events.jsonl"))?;
        let mut conversation: Vec<ResumeItem> = Vec::new();
        let mut transcript: Vec<TranscriptItem> = Vec::new();
        let mut hydrated: Vec<HydratedTranscriptItem> = Vec::new();
        let mut replay = ReplayState::default();
        for (idx, event) in events.iter().enumerate().rev() {
            if let Some(SessionEventKind::ContextCompacted {
                conversation: snapshot,
                ..
            }) = SessionEventKind::try_from_event(event)
                && !snapshot.is_empty()
            {
                conversation = snapshot;
                // Replay only events with index > idx, in chronological
                // order — events at idx or earlier are subsumed by the
                // checkpoint snapshot.
                for forward in events.iter().skip(idx + 1) {
                    apply_event_to_replay(
                        forward,
                        &mut conversation,
                        &mut transcript,
                        &mut hydrated,
                        &mut replay,
                    );
                }
                return Ok(SessionResumeState {
                    resume_available: true,
                    previous_response_id: None,
                    conversation,
                    transcript,
                    hydrated_transcript: hydrated,
                    context_attachments: self.context_attachments().unwrap_or_default(),
                    context_compaction: ContextCompactionState::default(),
                    routing_sticky_remaining_turns: 0,
                    routing_session_disabled: false,
                    routing_prior_turn_was_hard: false,
                });
            }
        }
        for event in &events {
            apply_event_to_replay(
                event,
                &mut conversation,
                &mut transcript,
                &mut hydrated,
                &mut replay,
            );
        }
        Ok(SessionResumeState {
            resume_available: true,
            previous_response_id: None,
            conversation,
            transcript,
            hydrated_transcript: hydrated,
            context_attachments: self.context_attachments().unwrap_or_default(),
            context_compaction: ContextCompactionState::default(),
            routing_sticky_remaining_turns: 0,
            routing_session_disabled: false,
            routing_prior_turn_was_hard: false,
        })
    }

    pub fn write_context_attachment(
        &self,
        attachment: &ContextAttachment,
        redacted_text: Option<&str>,
    ) -> Result<()> {
        // Attaching context is substantive — the caller is asking us to
        // pin a real piece of state alongside the session.
        let _ = self.ensure_live()?;
        let dir = self.dir().join("attachments");
        fs::create_dir_all(&dir)?;
        let stem = attachment_file_stem(&attachment.id)?;
        write_json(&dir.join(format!("{stem}.json")), attachment)?;
        if let Some(redacted_text) = redacted_text {
            fs::write(dir.join(format!("{stem}.txt")), redacted_text)?;
        }
        Ok(())
    }

    pub fn context_attachments(&self) -> Result<Vec<ContextAttachment>> {
        // A pending session has no attachments dir on disk yet — return
        // an empty list rather than letting the underlying `read_dir`
        // call surface a NotFound IO error.
        {
            let guard = self.state.inner.lock().expect("session handle state");
            if matches!(&*guard, InnerState::Pending(_)) {
                return Ok(Vec::new());
            }
        }
        read_context_attachments(&self.dir().join("attachments"))
    }

    pub fn finish(
        &self,
        status: SessionStatus,
        cost: CostSnapshot,
        metrics: SessionMetrics,
        redactions: u64,
    ) -> Result<()> {
        // A `finish` on a still-pending session means the session ran
        // but never produced any substantive event (e.g. `--prompt
        // --help`). Keep the no-stub invariant: mutate the in-memory
        // metadata so a same-process caller still sees Completed/Failed
        // bookkeeping, but do not promote the session to its on-disk
        // form just to record an end timestamp.
        {
            let mut guard = self.state.inner.lock().expect("session handle state");
            if let InnerState::Pending(pending) = &mut *guard {
                pending.metadata.ended_at_ms = Some(now_ms());
                if matches!(pending.metadata.status, SessionStatus::Running) {
                    pending.metadata.status = status;
                }
                pending.metadata.cost = cost;
                pending.metadata.metrics = metrics;
                pending.metadata.redactions = redactions;
                return Ok(());
            }
        }
        self.flush_events()?;
        self.update_metadata(|metadata| {
            metadata.ended_at_ms = Some(now_ms());
            // Preserve any terminal status that an earlier event (truncation,
            // turn failure, explicit cancellation) already recorded so the
            // outer "wrap up the session" caller can't silently overwrite a
            // more informative outcome with a generic Completed.
            if matches!(metadata.status, SessionStatus::Running) {
                metadata.status = status;
            }
            metadata.cost = cost;
            metadata.metrics = metrics;
            metadata.redactions = redactions;
        })?;
        if let Ok(metadata) = self.metadata() {
            SessionStore::record_global_index(&metadata);
        }
        Ok(())
    }

    fn dir(&self) -> PathBuf {
        self.store.session_dir(&self.session_id)
    }
}

/// One line of the cross-project session index. Append-only writes are
/// produced by [`SessionStore::record_global_index`] on session create,
/// title-bearing event commit, finish, and archive/unarchive transitions.
/// Readers (`list_global_index`) dedupe by `session_id` keeping the
/// largest `last_event_at_ms`, so missing intermediate writes are
/// tolerable — the most recent snapshot wins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalSessionIndexEntry {
    pub session_id: String,
    pub cwd: String,
    pub workspace_root: String,
    #[serde(default)]
    pub repo_root: Option<String>,
    /// Human-readable label used by the resume picker. Sourced from
    /// `first_user_task` and falling back to `latest_summary` so freshly
    /// started sessions that have not seen a user message yet still get
    /// a placeholder label once any assistant turn completes.
    #[serde(default)]
    pub title: Option<String>,
    /// Mirrors `SessionMetadata::display_name` so cross-project resume
    /// picker entries (which read this index, not the per-project
    /// `metadata.json`) can still surface the user-chosen name. Legacy
    /// index files predate this field — `serde(default)` keeps them
    /// loadable; the picker falls back to `title` when this is `None`.
    #[serde(default)]
    pub display_name: Option<String>,
    pub started_at_ms: u64,
    pub last_event_at_ms: u64,
    #[serde(default)]
    pub turn_count: u64,
    pub resume_available: bool,
}

impl GlobalSessionIndexEntry {
    /// Project a `SessionMetadata` snapshot onto the wire shape persisted
    /// in the global index. `last_event_at_ms` is the wall-clock moment
    /// the snapshot was taken — each write is itself the "event" that
    /// advances the timeline, so callers do not need a separate event
    /// timestamp argument.
    pub fn from_metadata(metadata: &SessionMetadata, last_event_at_ms: u64) -> Self {
        let resume_available =
            metadata.resume_available && !matches!(metadata.status, SessionStatus::Archived);
        let title = metadata
            .first_user_task
            .clone()
            .or_else(|| metadata.latest_summary.clone());
        Self {
            session_id: metadata.session_id.clone(),
            cwd: metadata.cwd.clone(),
            workspace_root: metadata.workspace_root.clone(),
            repo_root: metadata.repo_root.clone(),
            title,
            display_name: metadata.display_name.clone(),
            started_at_ms: metadata.started_at_ms,
            last_event_at_ms,
            turn_count: metadata.metrics.turns,
            resume_available,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMetadata {
    /// Schema version stamped on every `metadata.json`. Missing in v0
    /// files; the reader migrates them through
    /// `SESSION_METADATA_MIGRATIONS` and stamps the current version.
    #[serde(default = "legacy_session_metadata_schema_version")]
    pub schema_version: u32,
    pub session_id: String,
    pub started_at_ms: u64,
    pub ended_at_ms: Option<u64>,
    /// Timestamp the session was moved into the `archived/` tree. Distinct
    /// from `ended_at_ms` so the lifecycle "session ended at T1, user (or
    /// retention sweep) archived it later at T2" is recoverable. Older
    /// metadata files predate this field, so `serde(default)` keeps them
    /// loadable; the cleanup sweep then falls back to `ended_at_ms` /
    /// `started_at_ms` as it did before.
    #[serde(default)]
    pub archived_at_ms: Option<u64>,
    pub cwd: String,
    pub workspace_root: String,
    pub repo_root: Option<String>,
    pub branch: Option<String>,
    pub provider: String,
    pub model: String,
    pub mode: SessionMode,
    pub status: SessionStatus,
    pub first_user_task: Option<String>,
    pub latest_summary: Option<String>,
    pub cost: CostSnapshot,
    pub metrics: SessionMetrics,
    pub redactions: u64,
    pub resume_available: bool,
    pub resume_unavailable_reason: Option<String>,
    pub event_count: u64,
    /// EMA-calibrated bytes-per-token ratios learned from this session's
    /// provider responses. Loaded on resume so the token estimator stays
    /// warm across runs. `serde(default)` keeps older `metadata.json`
    /// files compatible.
    #[serde(default)]
    pub token_calibration: squeezy_llm::TokenCalibration,
    /// Set on sessions created by `Agent::fork_current` or
    /// `squeezy sessions fork`: the parent's session id. `None` for any
    /// other origin (fresh start, resume of a top-level session). The
    /// structured field lets the TUI session list render fork chains
    /// without re-parsing `session_forked` events. `serde(default)` so
    /// pre-fork `metadata.json` files keep deserializing.
    #[serde(default)]
    pub parent_id: Option<String>,
    /// Human-friendly name set by the user via `/session rename <name>`.
    /// When present, the resume picker prefers it over the inferred
    /// `first_user_task` / `latest_summary` label so memorable sessions
    /// stay easy to find. `serde(default)` keeps pre-rename
    /// `metadata.json` files loadable; the absent-field case continues
    /// to fall back to the inferred label.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Free-form labels attached by the user via `/session label <name>`.
    /// Multiple labels coexist (`bugfix`, `payments`, `wip`, …) so the
    /// user can group sessions across projects without renaming them.
    /// Persisted as a `Vec<String>` so the wire shape is trivially
    /// extensible; legacy `metadata.json` files deserialise with an
    /// empty vec.
    #[serde(default)]
    pub labels: Vec<String>,
}

impl Default for SessionMetadata {
    fn default() -> Self {
        Self {
            schema_version: SESSION_METADATA_SCHEMA_VERSION,
            session_id: String::new(),
            started_at_ms: 0,
            ended_at_ms: None,
            archived_at_ms: None,
            cwd: String::new(),
            workspace_root: String::new(),
            repo_root: None,
            branch: None,
            provider: String::new(),
            model: String::new(),
            mode: SessionMode::default(),
            status: SessionStatus::default(),
            first_user_task: None,
            latest_summary: None,
            cost: CostSnapshot::default(),
            metrics: SessionMetrics::default(),
            redactions: 0,
            resume_available: false,
            resume_unavailable_reason: None,
            event_count: 0,
            token_calibration: squeezy_llm::TokenCalibration::default(),
            parent_id: None,
            display_name: None,
            labels: Vec::new(),
        }
    }
}

impl SessionMetadata {
    pub fn new(config: &AppConfig, provider: impl Into<String>) -> Self {
        let (repo_root, branch) = git_identity(&config.workspace_root);
        Self {
            cwd: std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .display()
                .to_string(),
            workspace_root: config.workspace_root.display().to_string(),
            repo_root,
            branch,
            provider: provider.into(),
            model: config.model.clone(),
            mode: config.session_mode,
            resume_available: true,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    #[default]
    Running,
    Archived,
    Completed,
    Cancelled,
    Failed,
    Truncated,
}

impl SessionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Archived => "archived",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
            Self::Truncated => "truncated",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEvent {
    pub ts_unix_ms: u64,
    pub kind: String,
    pub turn_id: Option<String>,
    pub summary: Option<String>,
    pub payload: Value,
    /// Optional pointer to the parent event's position in `events.jsonl`,
    /// expressed as a zero-based sequence number. `None` (the default and
    /// the wire shape for every legacy log) means the event continues the
    /// previous one linearly, so the implicit parent is `sequence - 1`.
    /// `Some(k)` makes the parent explicit and is used to encode branches
    /// — re-prompting from an earlier turn creates a new event whose
    /// parent is the earlier turn rather than the current tip, so the
    /// resume picker can offer to navigate to either branch.
    ///
    /// Backward compatible: every existing log deserialises with the
    /// field absent, and `skip_serializing_if` keeps the JSONL bytes
    /// byte-identical for linear producers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_event_sequence: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionReplayEventKind {
    UserMessage,
    ModelRequest,
    ModelStarted,
    ModelTextDelta,
    ModelToolCall,
    ModelCompleted,
    ModelCancelled,
    ToolCall,
    ToolResult,
    CostDecision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionReplayEvent {
    pub schema_version: u32,
    pub ts_unix_ms: u64,
    pub sequence: u64,
    pub kind: SessionReplayEventKind,
    pub turn_id: Option<String>,
    pub payload_sha256: String,
    pub payload: Value,
}

impl SessionReplayEvent {
    pub fn new(kind: SessionReplayEventKind, turn_id: Option<String>, payload: Value) -> Self {
        let payload_sha256 = replay_payload_sha256(&payload);
        Self {
            schema_version: SESSION_REPLAY_SCHEMA_VERSION,
            ts_unix_ms: now_ms(),
            sequence: 0,
            kind,
            turn_id,
            payload_sha256,
            payload,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionReplayTape {
    pub schema_version: u32,
    pub session_id: String,
    pub events: Vec<SessionReplayEvent>,
    pub warnings: u64,
}

/// Discriminator for [`RolloutEvent`] — tells consumers which of the two
/// underlying logs (`events.jsonl` vs `replay.jsonl`) the entry originated
/// from. Stored as a tag in the serialised form so JSONL exports remain
/// self-describing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RolloutEventSource {
    /// Originated from `events.jsonl` (coarse session-lifecycle events).
    Event,
    /// Originated from `replay.jsonl` (fine-grained per-turn replay tape).
    Replay,
}

/// Normalized rollout-trace entry: one item per row in the merged
/// `events.jsonl` + `replay.jsonl` stream, with its provenance preserved.
///
/// `RolloutEvent` is the output shape of [`SessionStore::bundle_rollout_trace`].
/// The `payload` keeps the raw JSON Squeezy already persisted; the typed
/// `event_kind` / `replay_kind` enums are populated when the row's
/// discriminator matches a known variant so consumers don't have to re-parse
/// the payload to dispatch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RolloutEvent {
    pub schema_version: u32,
    pub source: RolloutEventSource,
    pub ts_unix_ms: u64,
    /// Strictly-monotonic sequence assigned by the replay writer. Zero for
    /// entries that came from `events.jsonl` (which has no sequence column).
    pub sequence: u64,
    pub turn_id: Option<String>,
    pub summary: Option<String>,
    /// Free-form discriminator from the source log. Mirrors
    /// `SessionEvent.kind` for events and the snake_case rendering of
    /// `SessionReplayEventKind` for replay rows. Always populated.
    pub kind: String,
    /// Typed view of the row when it came from `events.jsonl` and the
    /// discriminator matched a known [`SessionEventKind`] variant.
    pub event_kind: Option<SessionEventKind>,
    /// Typed view of the row when it came from `replay.jsonl`.
    pub replay_kind: Option<SessionReplayEventKind>,
    /// Content-addressed digest of `payload`, only present on replay rows
    /// (events.jsonl entries do not carry one).
    pub payload_sha256: Option<String>,
    pub payload: Value,
}

impl RolloutEvent {
    /// Sort key for the source discriminator. Replay rows sort before events
    /// in the same millisecond so the granular per-turn flow precedes the
    /// coarse lifecycle summary it produced.
    fn source_order(&self) -> u8 {
        match self.source {
            RolloutEventSource::Replay => 0,
            RolloutEventSource::Event => 1,
        }
    }

    /// Secondary sort key. Replay rows already carry a strictly-monotonic
    /// `sequence`; event rows fall back to their insertion index so the
    /// `events.jsonl` order is preserved.
    fn tie_breaker(&self) -> u64 {
        self.sequence
    }

    fn from_session_event(event: SessionEvent, insertion: usize) -> Self {
        let kind = event.kind.clone();
        let event_kind = SessionEventKind::try_from_event(&event);
        Self {
            schema_version: ROLLOUT_TRACE_SCHEMA_VERSION,
            source: RolloutEventSource::Event,
            ts_unix_ms: event.ts_unix_ms,
            sequence: insertion as u64,
            turn_id: event.turn_id,
            summary: event.summary,
            kind,
            event_kind,
            replay_kind: None,
            payload_sha256: None,
            payload: event.payload,
        }
    }

    fn from_replay_event(event: SessionReplayEvent) -> Self {
        Self {
            schema_version: ROLLOUT_TRACE_SCHEMA_VERSION,
            source: RolloutEventSource::Replay,
            ts_unix_ms: event.ts_unix_ms,
            sequence: event.sequence,
            turn_id: event.turn_id,
            summary: None,
            kind: replay_kind_discriminator(event.kind).to_string(),
            event_kind: None,
            replay_kind: Some(event.kind),
            payload_sha256: Some(event.payload_sha256),
            payload: event.payload,
        }
    }
}

fn replay_kind_discriminator(kind: SessionReplayEventKind) -> &'static str {
    match kind {
        SessionReplayEventKind::UserMessage => "user_message",
        SessionReplayEventKind::ModelRequest => "model_request",
        SessionReplayEventKind::ModelStarted => "model_started",
        SessionReplayEventKind::ModelTextDelta => "model_text_delta",
        SessionReplayEventKind::ModelToolCall => "model_tool_call",
        SessionReplayEventKind::ModelCompleted => "model_completed",
        SessionReplayEventKind::ModelCancelled => "model_cancelled",
        SessionReplayEventKind::ToolCall => "tool_call",
        SessionReplayEventKind::ToolResult => "tool_result",
        SessionReplayEventKind::CostDecision => "cost_decision",
    }
}

impl SessionEvent {
    pub fn new(
        kind: impl Into<String>,
        turn_id: Option<String>,
        summary: Option<String>,
        payload: Value,
    ) -> Self {
        Self {
            ts_unix_ms: now_ms(),
            kind: kind.into(),
            turn_id,
            summary,
            payload,
            parent_event_sequence: None,
        }
    }

    /// Attach an explicit parent event sequence to this event. The sequence
    /// is the zero-based position of the parent event inside the session's
    /// `events.jsonl`. Producers that branch off an earlier turn (for
    /// example, re-prompting after navigating to a previous user message)
    /// call this so the resulting tree exposes both branches.
    #[must_use]
    pub fn with_parent_event_sequence(mut self, sequence: u64) -> Self {
        self.parent_event_sequence = Some(sequence);
        self
    }

    /// Build a `SessionEvent` from a typed [`SessionEventKind`]. The kind
    /// discriminator and payload fields are serialised in the same wire
    /// shape that the string-tagged constructor produces, so existing
    /// readers (`try_from_event`, replay, bug-report redaction) round-trip
    /// without change. The `text` field of `UserMessage` /
    /// `AssistantCompleted` is also mirrored into `summary` when no
    /// explicit summary is supplied so discovery surfaces keep working.
    pub fn from_typed(
        kind: SessionEventKind,
        turn_id: Option<String>,
        summary: Option<String>,
    ) -> Self {
        let discriminator = kind.discriminator().to_string();
        let value = serde_json::to_value(&kind).unwrap_or(Value::Null);
        let mut payload = match value {
            Value::Object(mut map) => {
                map.remove("kind");
                Value::Object(map)
            }
            other => other,
        };
        if !matches!(payload, Value::Object(_)) {
            payload = json!({});
        }
        let summary = summary.or_else(|| match &kind {
            SessionEventKind::UserMessage { text } => Some(text.clone()),
            SessionEventKind::AssistantCompleted { text, .. } if !text.is_empty() => {
                Some(text.clone())
            }
            _ => None,
        });
        Self {
            ts_unix_ms: now_ms(),
            kind: discriminator,
            turn_id,
            summary,
            payload,
            parent_event_sequence: None,
        }
    }
}

/// Tip of one branch in a session's event tree. Produced by
/// [`detect_branches`] for sessions that contain at least two branches so
/// the resume picker can offer to navigate to either branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventBranchTip {
    /// Zero-based index of the tip event in the source `events.jsonl`. The
    /// tip is the leaf of one branch — the most recent event on that path
    /// from the root.
    pub tip_sequence: u64,
    /// Index of the deepest ancestor that has multiple children, i.e. the
    /// event from which this branch diverged from its sibling(s). When the
    /// fork is at the root, this is the root's sequence.
    pub branched_from_sequence: u64,
    /// `ts_unix_ms` recorded on the tip event. Used to sort branch tips
    /// newest-first in the picker.
    pub tip_ts_unix_ms: u64,
    /// First user-message text encountered on this branch *after* the
    /// divergence point. Lets the picker label otherwise-identical
    /// branches with the prompt that re-opened that path. `None` when the
    /// branch contains no user message after the fork (rare; only happens
    /// when the tip itself sits directly on the fork).
    pub first_message_after_branch: Option<String>,
}

/// Walk `events` as a tree and return the tips of each branch. Sessions
/// that are purely linear — every event implicitly continues the previous
/// one — return an empty vector, so callers can skip the branch picker UI
/// without an extra check. When the session contains at least two leaves
/// (i.e. two distinct branches), every leaf is reported.
///
/// The implicit-parent rule mirrors the serialised wire shape: an event at
/// index `i` whose `parent_event_sequence` is `None` is assumed to descend
/// from `i - 1`. Only events that set `parent_event_sequence = Some(k)`
/// where `k < i - 1` participate in branching.
pub fn detect_branches(events: &[SessionEvent]) -> Vec<EventBranchTip> {
    if events.len() < 2 {
        return Vec::new();
    }
    let len = events.len();
    let parents: Vec<Option<u64>> = events
        .iter()
        .enumerate()
        .map(|(i, event)| match event.parent_event_sequence {
            Some(parent) if (parent as usize) < len && (parent as usize) != i => Some(parent),
            // An explicit but out-of-range parent is treated as the implicit
            // parent rather than panicking. Self-parent is likewise ignored.
            _ if i == 0 => None,
            _ => Some((i - 1) as u64),
        })
        .collect();

    let mut child_count: Vec<u32> = vec![0; len];
    for parent in parents.iter().flatten() {
        let idx = *parent as usize;
        if idx < len {
            child_count[idx] = child_count[idx].saturating_add(1);
        }
    }

    let leaves: Vec<u64> = (0..len)
        .filter(|i| child_count[*i] == 0)
        .map(|i| i as u64)
        .collect();
    if leaves.len() < 2 {
        return Vec::new();
    }

    let mut tips: Vec<EventBranchTip> = leaves
        .iter()
        .map(|&tip| {
            let mut path: Vec<u64> = vec![tip];
            let mut cur = tip as usize;
            while let Some(parent) = parents[cur] {
                path.push(parent);
                cur = parent as usize;
            }
            path.reverse();

            // Deepest ancestor on this path that has multiple children. When
            // no internal node on the path forks (only possible if `tip`
            // itself is the lone leaf, which `leaves.len() < 2` already
            // guarded against), fall back to the root so callers still get
            // a sensible divergence point.
            let branched_from = path
                .iter()
                .rev()
                .find(|&&node| child_count[node as usize] > 1)
                .copied()
                .unwrap_or(path[0]);

            let first_message_after_branch = path
                .iter()
                .skip_while(|&&node| node != branched_from)
                .skip(1)
                .find_map(|&node| {
                    let event = &events[node as usize];
                    if event.kind == "user_message" {
                        event.summary.clone()
                    } else {
                        None
                    }
                });

            EventBranchTip {
                tip_sequence: tip,
                branched_from_sequence: branched_from,
                tip_ts_unix_ms: events[tip as usize].ts_unix_ms,
                first_message_after_branch,
            }
        })
        .collect();

    // Newest tip first so the picker surfaces the most recent branch at the
    // top of the candidate list.
    tips.sort_by_key(|tip| std::cmp::Reverse(tip.tip_ts_unix_ms));
    tips
}

/// Typed view over `SessionEvent` for the well-known event kinds Squeezy
/// emits. The variants double as a typed *append* API via
/// [`SessionEvent::from_typed`] / [`SessionHandle::append_typed_event`] and
/// as the typed *read* view via [`SessionEventKind::try_from_event`]. The
/// `#[serde(other)] Unknown` arm keeps replay safe even when older sessions
/// carry kinds we have since renamed or retired.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEventKind {
    UserMessage {
        text: String,
    },
    AssistantCompleted {
        #[serde(default)]
        text: String,
        #[serde(default)]
        response_id: Option<String>,
    },
    ToolCall {
        #[serde(default)]
        call_id: String,
        #[serde(default)]
        tool: String,
        #[serde(default)]
        arguments: Value,
    },
    ToolResult {
        #[serde(default)]
        output: Value,
    },
    ContextCompacted {
        #[serde(default)]
        record: Value,
        #[serde(default)]
        summary: Option<String>,
        #[serde(default)]
        replacement_id: Option<String>,
        /// Pre-compaction conversation snapshot. Populated when the
        /// producer wants replay to snap to this checkpoint instead of
        /// linear-replaying older events.
        #[serde(default)]
        conversation: Vec<ResumeItem>,
    },
    /// Provider-tagged reasoning blob emitted at the end of each reasoning
    /// segment (one per `LlmEvent::ReasoningDone`). Persisted so the
    /// `events.jsonl` replay fallback can rebuild the same `ResumeItem::Reasoning`
    /// items that `resume_state.json` would have produced, preserving
    /// the model's prior chain-of-thought (including OpenAI
    /// `encrypted_content` and Anthropic/Google signed blocks) across resume.
    Reasoning {
        payload: ReasoningPayload,
    },
    ApprovalRequested {
        #[serde(default)]
        tool: String,
        #[serde(default)]
        payload: Value,
    },
    ApprovalDecided {
        #[serde(default)]
        tool: String,
        #[serde(default)]
        decision: String,
        #[serde(default)]
        payload: Value,
    },
    SessionStarted,
    SessionEnded {
        #[serde(default)]
        status: String,
    },
    Cancelled,
    Failed {
        #[serde(default)]
        error: String,
    },
    SessionResumed,
    /// Extension-authored sidecar event. The outer `SessionEvent.kind` is
    /// the fixed sentinel `"custom"` so core readers can match on it and
    /// skip the event without having to know every extension's
    /// discriminator. `kind` carries the extension's own free-form
    /// discriminator (e.g. `"telemetry"`, `"my_org.audit_log"`) and
    /// `payload` is the opaque JSON the extension wants to round-trip
    /// through `events.jsonl`. Renamed wire-side to avoid colliding
    /// with the enum's `tag = "kind"` discriminator.
    Custom {
        #[serde(rename = "custom_kind")]
        kind: String,
        #[serde(default, rename = "custom_payload")]
        payload: Value,
    },
    #[serde(other)]
    Unknown,
}

impl SessionEventKind {
    /// Free-form string discriminator that matches what Squeezy already
    /// writes into `SessionEvent::kind`. Centralising the mapping keeps
    /// typed producers and the existing string-tagged producers consistent.
    pub fn discriminator(&self) -> &'static str {
        match self {
            Self::UserMessage { .. } => "user_message",
            Self::AssistantCompleted { .. } => "assistant_completed",
            Self::ToolCall { .. } => "tool_call",
            Self::ToolResult { .. } => "tool_result",
            Self::ContextCompacted { .. } => "context_compacted",
            Self::Reasoning { .. } => "reasoning",
            Self::ApprovalRequested { .. } => "approval_requested",
            Self::ApprovalDecided { .. } => "approval_decided",
            Self::SessionStarted => "session_started",
            Self::SessionEnded { .. } => "session_ended",
            Self::Cancelled => "cancelled",
            Self::Failed { .. } => "failed",
            Self::SessionResumed => "session_resumed",
            Self::Custom { .. } => "custom",
            Self::Unknown => "unknown",
        }
    }

    /// Parse a free-form `SessionEvent` into the typed view. Returns
    /// `None` if the discriminator + payload do not match any known
    /// variant; the caller can then skip the event without erroring.
    /// When the payload omits a `text` field for `user_message` /
    /// `assistant_completed`, the event's `summary` is used as a
    /// best-effort substitute — Squeezy's existing producers carry the
    /// user / assistant text in `summary`, not `payload`.
    pub fn try_from_event(event: &SessionEvent) -> Option<Self> {
        let mut object = serde_json::Map::new();
        object.insert("kind".to_string(), Value::String(event.kind.clone()));
        if let Value::Object(payload) = &event.payload {
            for (key, value) in payload {
                if key == "kind" {
                    continue;
                }
                object.insert(key.clone(), value.clone());
            }
        }
        let needs_text = matches!(event.kind.as_str(), "user_message" | "assistant_completed");
        if needs_text
            && !object.contains_key("text")
            && let Some(summary) = &event.summary
        {
            object.insert("text".to_string(), Value::String(summary.clone()));
        }
        serde_json::from_value(Value::Object(object)).ok()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionResumeState {
    pub resume_available: bool,
    pub previous_response_id: Option<String>,
    pub conversation: Vec<ResumeItem>,
    /// Legacy message-only transcript. Kept for back-compat with
    /// `resume_state.json` files written by older binaries; new
    /// consumers should read `hydrated_transcript` instead, which
    /// also carries tool-result cards needed for full UI parity on
    /// resume. We continue to populate this field on write so a
    /// future binary that rolled back to a pre-hydrated build can
    /// still resume.
    pub transcript: Vec<TranscriptItem>,
    /// Full UI-hydration list — messages, tool results, and (via
    /// the embedded reasoning attached to assistant messages)
    /// reasoning chips. The TUI iterates this on resume and routes
    /// each variant to the matching `TuiApp::push_*` method, so a
    /// resumed session renders the same shape a fresh turn does.
    /// `#[serde(default)]` keeps old session files readable —
    /// missing means the file pre-dates hydration support and the
    /// loader falls back to wrapping `transcript` items.
    #[serde(default)]
    pub hydrated_transcript: Vec<HydratedTranscriptItem>,
    #[serde(default)]
    pub context_attachments: Vec<ContextAttachment>,
    #[serde(default)]
    pub context_compaction: ContextCompactionState,
    /// Remaining turns in the per-turn router's escalation-sticky
    /// window at session-save time. After an escalation hands a
    /// cheap-routed turn back to the parent model, the next few user
    /// prompts skip the router so a follow-up clarification stays on
    /// parent. Persisting it across `/resume` keeps that behaviour
    /// intact when the user reopens a session mid-hard-task. Older
    /// `resume_state.json` files have no field; the
    /// `#[serde(default)]` keeps reads backward-compatible (the
    /// router starts each session with a zeroed sticky window
    /// anyway).
    #[serde(default)]
    pub routing_sticky_remaining_turns: u8,
    /// Session-level `/router off` state. Unlike force-cheap and
    /// force-parent, this is intentionally sticky for the session and
    /// must survive `/resume`; missing means older sessions predate the
    /// router toggle persistence and should resume with routing enabled
    /// according to config.
    #[serde(default)]
    pub routing_session_disabled: bool,
    /// Whether the previous persisted turn was hard enough that
    /// deictic follow-ups should bias back to the parent model.
    #[serde(default)]
    pub routing_prior_turn_was_hard: bool,
}

/// One entry the TUI knows how to push into its transcript on
/// resume. Mirrors the kinds emitted live during a turn: assistant
/// / user / system messages (`TranscriptItem`) and tool-result
/// cards (`ToolResult`). Reasoning isn't a separate variant
/// because the live renderer attaches reasoning to assistant
/// messages via `TranscriptItem.reasoning`, and the resume replay
/// keeps that contract.
///
/// `result` is the raw `serde_json::Value` the agent serialized
/// at write time — `squeezy-store` deliberately doesn't pull in
/// `squeezy-tools` (which defines the typed `ToolResult` struct)
/// to keep the dep graph one-directional, so the TUI consumes
/// these via `serde_json::from_value::<squeezy_tools::ToolResult>`
/// at hydration time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HydratedTranscriptItem {
    Message {
        item: TranscriptItem,
    },
    ToolResult {
        #[serde(default)]
        call: Option<HydratedToolCall>,
        result: Value,
    },
}

/// Lightweight `ToolCall` projection that survives the
/// `squeezy-store` ↔ `squeezy-tools` crate split. The TUI rebuilds
/// a full `squeezy_tools::ToolCall` from these three fields when
/// pushing a tool-result card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HydratedToolCall {
    pub call_id: String,
    pub tool: String,
    #[serde(default)]
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResumeItem {
    UserText {
        text: String,
    },
    AssistantText {
        text: String,
    },
    FunctionCall {
        call_id: String,
        name: String,
        arguments: Value,
    },
    FunctionCallOutput {
        call_id: String,
        output: String,
    },
    Reasoning {
        payload: squeezy_core::ReasoningPayload,
    },
    /// Inline image attachment captured from a `read_file` returning
    /// PNG/JPEG/GIF/WEBP bytes. Stored as base64 so the JSON checkpoint
    /// stays compact and human-debuggable; rehydrates into
    /// `LlmInputItem::Image` on resume.
    Image {
        media_type: String,
        data_base64: String,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionQuery {
    pub since_ms: Option<u64>,
    pub until_ms: Option<u64>,
    pub cwd: Option<String>,
    pub repo: Option<String>,
    pub branch: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub status: Option<SessionStatus>,
    pub query: Option<String>,
    /// When false (default), `list` and `cleanup` skip sessions in the
    /// `archived/` subdir even if `status` does not explicitly exclude
    /// `Archived`. Set to true to include archived sessions in results.
    pub include_archived: bool,
}

impl SessionQuery {
    fn matches(&self, metadata: &SessionMetadata) -> bool {
        if self
            .since_ms
            .is_some_and(|since| metadata.started_at_ms < since)
        {
            return false;
        }
        if self
            .until_ms
            .is_some_and(|until| metadata.started_at_ms > until)
        {
            return false;
        }
        if !contains_if_set(&metadata.cwd, &self.cwd) {
            return false;
        }
        if !contains_if_set(metadata.repo_root.as_deref().unwrap_or(""), &self.repo) {
            return false;
        }
        if !equals_if_set(metadata.branch.as_deref().unwrap_or(""), &self.branch) {
            return false;
        }
        if !equals_if_set(&metadata.provider, &self.provider) {
            return false;
        }
        if !equals_if_set(&metadata.model, &self.model) {
            return false;
        }
        if self.status.is_some_and(|status| metadata.status != status) {
            return false;
        }
        if let Some(query) = &self.query {
            let haystack = format!(
                "{}\n{}\n{}\n{}\n{}\n{}",
                metadata.session_id,
                metadata.cwd,
                metadata.workspace_root,
                metadata.repo_root.as_deref().unwrap_or(""),
                metadata.first_user_task.as_deref().unwrap_or(""),
                metadata.latest_summary.as_deref().unwrap_or("")
            )
            .to_ascii_lowercase();
            return haystack.contains(&query.to_ascii_lowercase());
        }
        true
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub metadata: SessionMetadata,
    pub events: Vec<SessionEvent>,
    pub event_warnings: u64,
    pub resume_state: Option<SessionResumeState>,
    pub attachments: Vec<ContextAttachment>,
    pub replay: Option<SessionReplayTape>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupReport {
    /// Live sessions that were moved into `archived/<id>/` by this
    /// sweep. They still exist on disk and can be restored with
    /// [`SessionStore::unarchive_session`] until the archive retention
    /// sweep deletes them.
    #[serde(default)]
    pub archived: Vec<String>,
    /// Sessions that were permanently deleted by this sweep. Populated
    /// when the archive retention sweep removes a session that has
    /// outlived `retention_archive_days`, and when [`CleanupMode::Purge`]
    /// is requested for explicit `ids`.
    pub removed: Vec<String>,
}

/// Errors produced by [`SessionStore::resolve_session_id_prefix`].
///
/// The CLI / TUI surface bubbles these up directly so the user can see
/// whether the typed prefix matched nothing, matched several sessions
/// (with the conflicting ids listed for follow-up), or hit a filesystem
/// failure while enumerating session ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// No session id starts with the supplied prefix. Also returned for
    /// an empty prefix so accidental `--session ""` invocations fail
    /// loudly instead of silently picking an arbitrary session.
    NotFound { prefix: String },
    /// More than one session id starts with the supplied prefix. The
    /// `matches` vector carries every candidate, sorted ascending so
    /// the disambiguation hint the CLI renders is stable across runs.
    AmbiguousPrefix {
        prefix: String,
        matches: Vec<String>,
    },
    /// Underlying filesystem failure while enumerating session ids
    /// (e.g. unreadable directory). Stored as a string so the error
    /// type stays `Clone + Eq`, which makes it cheap to use in
    /// pattern-matched tests.
    Io(String),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { prefix } => {
                write!(f, "no session matches the id prefix {prefix:?}")
            }
            Self::AmbiguousPrefix { prefix, matches } => {
                write!(
                    f,
                    "session id prefix {prefix:?} is ambiguous; candidates: {}",
                    matches.join(", ")
                )
            }
            Self::Io(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ResolveError {}

impl From<std::io::Error> for ResolveError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

/// Soft-archive vs hard-delete policy for [`SessionStore::cleanup_with`].
/// The CLI surfaces this as `squeezy sessions cleanup --archive` (default)
/// vs `--purge`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CleanupMode {
    /// Move expired or explicitly named live sessions into
    /// `archived/<id>/` rather than deleting them. The archive retention
    /// sweep eventually removes them after `retention_archive_days`.
    #[default]
    Archive,
    /// Hard-delete expired or explicitly named sessions. Live sessions
    /// skip the archive step; already-archived sessions named in `ids`
    /// are removed without waiting for archive retention.
    Purge,
}

fn session_root(config: &AppConfig) -> PathBuf {
    if let Some(path) = &config.session_logs.log_dir {
        return resolve_workspace_path(&config.workspace_root, path);
    }
    if let Some(root) = &config.cache.root {
        return resolve_workspace_path(&config.workspace_root, root).join("sessions");
    }
    config.workspace_root.join(".squeezy").join("sessions")
}

fn resolve_workspace_path(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

/// Return whether the file at `path` ends with a `\n` byte. Used by the
/// memory append path to keep each remembered line on its own row even
/// when the user (or an earlier tool) left a trailing-newline-less file.
fn memory_file_ends_with_newline(path: &Path) -> Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = fs::File::open(path)?;
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(true);
    }
    file.seek(SeekFrom::End(-1))?;
    let mut buf = [0u8; 1];
    file.read_exact(&mut buf)?;
    Ok(buf[0] == b'\n')
}

/// Serialize `value` to `path` atomically: write to a sibling temp file,
/// `sync_all`, then `fs::rename` over the target. A reader (and a crash)
/// therefore only ever observes the previous complete file or the new
/// complete file, never a truncated/torn one. This matters because
/// `metadata.json` is rewritten on essentially every turn; a non-atomic
/// in-place write left a torn metadata file that silently hid an
/// otherwise-recoverable session from `list()`/`resume()`. Mirrors the
/// tmp + `sync_all` + `rename` pattern in [`rewrite_global_index`].
///
/// The temp name includes PID, thread ID, and a random nonce so concurrent
/// writers from the same process to the same path cannot clobber each other's
/// in-flight temp before the rename.
fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(value).map_err(json_error)?;
    let tmp = unique_tmp_path(path);
    {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    if let Err(error) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(error.into());
    }
    fsync_parent(path);
    Ok(())
}

/// Heuristic guard for [`SessionStore::record_global_index`].
///
/// Returns `true` only when the workspace_root resolves under the system
/// temp directory AND `HOME` does not. That combination is overwhelmingly
/// a `cargo test` setup that created its session store via
/// `temp_root(..)` but never redirected `HOME` to a test sandbox — i.e.
/// the test is not exercising the global index and a write would pollute
/// the developer's real `~/.squeezy/sessions/index.jsonl`.
///
/// Production binaries run from a real workspace (not under `temp_dir`)
/// so the guard does not fire. Tests that *do* want to exercise the
/// global index redirect `HOME` to a temp sandbox (see `with_home` in
/// `sessions_tests.rs`); the bypass keeps those tests working without
/// needing a config knob.
fn skip_global_index_for_test_workspace(workspace_root: &str) -> bool {
    let Ok(temp_dir) = std::env::temp_dir().canonicalize() else {
        return false;
    };
    let workspace_under_temp = Path::new(workspace_root)
        .canonicalize()
        .map(|canonical| canonical.starts_with(&temp_dir))
        .unwrap_or(false);
    if !workspace_under_temp {
        return false;
    }
    // Check both HOME and XDG_STATE_HOME: if either resolves under temp, the
    // global index destination is already sandboxed and the guard must not fire.
    let home_under_temp = env::var_os("HOME")
        .and_then(|home| Path::new(&home).canonicalize().ok())
        .map(|canonical| canonical.starts_with(&temp_dir))
        .unwrap_or(false);
    let xdg_under_temp = env::var_os("XDG_STATE_HOME")
        .and_then(|xdg| Path::new(&xdg).canonicalize().ok())
        .map(|canonical| canonical.starts_with(&temp_dir))
        .unwrap_or(false);
    !(home_under_temp || xdg_under_temp)
}

fn global_index_cache() -> &'static StdMutex<Option<GlobalIndexCache>> {
    GLOBAL_INDEX_CACHE.get_or_init(|| StdMutex::new(None))
}

fn global_index_fingerprint(metadata: &fs::Metadata) -> GlobalIndexFingerprint {
    GlobalIndexFingerprint {
        len: metadata.len(),
        modified: metadata.modified().ok(),
    }
}

fn cached_global_index(
    path: &Path,
    fingerprint: &GlobalIndexFingerprint,
) -> Option<Vec<GlobalSessionIndexEntry>> {
    let cache = global_index_cache()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let cache = cache.as_ref()?;
    if cache.path == path && cache.fingerprint == *fingerprint {
        Some(cache.entries.clone())
    } else {
        None
    }
}

fn cache_global_index(path: &Path, entries: &[GlobalSessionIndexEntry]) {
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    let mut cache = global_index_cache()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    *cache = Some(GlobalIndexCache {
        path: path.to_path_buf(),
        fingerprint: global_index_fingerprint(&metadata),
        entries: entries.to_vec(),
    });
}

/// Replace the global session index file with the supplied entries via a
/// tmp + rename so concurrent readers never see a half-written file. The
/// caller chooses the iteration order; readers re-sort by
/// `started_at_ms`. The temp name includes PID and a random nonce so two
/// processes compacting at the same time use distinct temp files and do not
/// clobber each other's in-flight write.
fn rewrite_global_index(path: &Path, entries: &[&GlobalSessionIndexEntry]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = unique_tmp_path(path);
    {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)?;
        for entry in entries {
            let mut payload = match serde_json::to_vec(entry) {
                Ok(payload) => payload,
                Err(_) => continue,
            };
            payload.push(b'\n');
            file.write_all(&payload)?;
        }
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    fsync_parent(path);
    Ok(())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let text = fs::read_to_string(path)?;
    serde_json::from_str(&text).map_err(json_error)
}

/// Serde default for [`SessionMetadata::schema_version`]. Returns 0 — the
/// pre-versioning ("v0") sentinel — so files written before the field
/// existed land in [`apply_session_metadata_migrations`] at the bottom of
/// the migration chain rather than being misread as the current version.
fn legacy_session_metadata_schema_version() -> u32 {
    0
}

/// Reader-side migrations applied in order. `MIGRATIONS[i]` upgrades a
/// session metadata payload from schema version `i` to `i + 1`.
/// [`apply_session_metadata_migrations`] reads the incoming
/// `schema_version` (treating a missing field as 0) and runs every
/// migration in `[from .. SESSION_METADATA_SCHEMA_VERSION)` before the
/// final deserialization step.
///
/// The v0 → v1 entry is a no-op because v1 is byte-for-byte compatible
/// with the pre-versioning shape; only the new `schema_version` field
/// itself is added, and the reader stamps that onto the payload after
/// the chain runs. The slot still exists so future migrations have a
/// clear chain to extend and so the "treat missing field as v0" rule
/// has a concrete code path.
const SESSION_METADATA_MIGRATIONS: &[fn(&mut Value)] = &[migrate_session_metadata_v0_to_v1];

fn migrate_session_metadata_v0_to_v1(_value: &mut Value) {}

/// Migrate `value` in place from whatever `schema_version` it carries on
/// disk up to [`SESSION_METADATA_SCHEMA_VERSION`], then stamp the
/// post-migration version onto the payload so the deserialized struct
/// reflects the upgraded shape. Forward-compatible: a payload that
/// already declares the current (or a future) version is left alone so
/// an older binary does not corrupt a newer file.
fn apply_session_metadata_migrations(value: &mut Value) {
    let from = value
        .get("schema_version")
        .and_then(Value::as_u64)
        .map(|version| version as u32)
        .unwrap_or(0);
    if from >= SESSION_METADATA_SCHEMA_VERSION {
        return;
    }
    let start = from as usize;
    let end = SESSION_METADATA_SCHEMA_VERSION as usize;
    for migration in &SESSION_METADATA_MIGRATIONS[start..end] {
        migration(value);
    }
    if let Value::Object(map) = value {
        map.insert(
            "schema_version".to_string(),
            Value::from(SESSION_METADATA_SCHEMA_VERSION),
        );
    }
}

fn deserialize_session_metadata(text: &str) -> Result<SessionMetadata> {
    let mut value: Value = serde_json::from_str(text).map_err(json_error)?;
    apply_session_metadata_migrations(&mut value);
    serde_json::from_value(value).map_err(json_error)
}

fn read_session_metadata(path: &Path) -> Result<SessionMetadata> {
    let text = fs::read_to_string(path)?;
    deserialize_session_metadata(&text)
}

fn read_context_attachments(dir: &Path) -> Result<Vec<ContextAttachment>> {
    let mut attachments = Vec::new();
    if !dir.exists() {
        return Ok(attachments);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        if let Ok(attachment) = read_json::<ContextAttachment>(&path) {
            attachments.push(attachment);
        }
    }
    attachments.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(attachments)
}

fn attachment_file_stem(id: &str) -> Result<&str> {
    if !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Ok(id);
    }
    Err(SqueezyError::Agent(format!(
        "invalid context attachment id {id:?}"
    )))
}

/// Per-replay state that survives across `apply_event_to_replay`
/// calls. Carries:
///
/// - `pending_reasoning`: provider streams emit `Reasoning` events
///   independently of the assistant message that follows. The
///   rendered transcript only has a place to attach a reasoning
///   snapshot via `TranscriptItem.reasoning` on the assistant
///   message itself, so we buffer reasoning here until the next
///   `AssistantCompleted` drains it.
/// - `pending_tool_calls`: tool-result hydration needs the matching
///   `ToolCall` (for the tool name + arguments) to rebuild the
///   transcript card. The agent emits `ToolCall` and `ToolResult`
///   as separate events linked by `call_id`; we hash the call by
///   id when we see it and look it up when the result lands.
///
/// Without this buffering, resume drops every reasoning chip and
/// every tool-result card the original turn produced — what the
/// LLM had in its context, the user did not see on screen.
#[derive(Debug, Default)]
struct ReplayState {
    pending_reasoning: Vec<ReasoningSnapshot>,
    pending_tool_calls: std::collections::HashMap<String, HydratedToolCall>,
}

impl ReplayState {
    fn drain_combined_reasoning(&mut self) -> Option<ReasoningSnapshot> {
        if self.pending_reasoning.is_empty() {
            return None;
        }
        // Concatenate every buffered segment's display text so the
        // resumed chip carries the full reasoning the user originally
        // watched stream in, separated by blank lines so a reviewer
        // can still see the segment boundaries. The payload comes
        // from the last segment so per-provider metadata (item_id,
        // encrypted_content, thought_signature) stays consistent
        // with what the provider would return on a fresh turn.
        let mut display = String::new();
        for snap in &self.pending_reasoning {
            if !display.is_empty() {
                display.push_str("\n\n");
            }
            display.push_str(&snap.display_text);
        }
        let last_payload = self
            .pending_reasoning
            .last()
            .map(|s| s.payload.clone())
            .expect("non-empty");
        self.pending_reasoning.clear();
        Some(ReasoningSnapshot {
            display_text: display,
            payload: last_payload,
        })
    }
}

fn apply_event_to_replay(
    event: &SessionEvent,
    conversation: &mut Vec<ResumeItem>,
    transcript: &mut Vec<TranscriptItem>,
    hydrated: &mut Vec<HydratedTranscriptItem>,
    replay: &mut ReplayState,
) {
    let Some(typed) = SessionEventKind::try_from_event(event) else {
        return;
    };
    match typed {
        SessionEventKind::UserMessage { text } => {
            conversation.push(ResumeItem::UserText { text: text.clone() });
            let item = TranscriptItem::user(text);
            transcript.push(item.clone());
            hydrated.push(HydratedTranscriptItem::Message { item });
        }
        SessionEventKind::AssistantCompleted { text, .. } => {
            if text.is_empty() {
                return;
            }
            conversation.push(ResumeItem::AssistantText { text: text.clone() });
            // Drain any reasoning buffered since the last assistant
            // message and attach it to this one so the resumed
            // transcript shows the reasoning chip via the
            // `format_assistant_message_entry` embedded-chip path.
            let attached = replay.drain_combined_reasoning();
            let item = TranscriptItem::assistant_with_reasoning(text, attached);
            transcript.push(item.clone());
            hydrated.push(HydratedTranscriptItem::Message { item });
        }
        SessionEventKind::ToolCall {
            call_id,
            tool,
            arguments,
        } => {
            if call_id.is_empty() {
                return;
            }
            // Buffer for the matching ToolResult event so the
            // hydrated transcript can carry the call's name + args
            // alongside the result body — without it the resumed
            // card has no tool label and no command preview.
            replay.pending_tool_calls.insert(
                call_id.clone(),
                HydratedToolCall {
                    call_id: call_id.clone(),
                    tool: tool.clone(),
                    arguments: arguments.clone(),
                },
            );
            conversation.push(ResumeItem::FunctionCall {
                call_id,
                name: tool,
                arguments,
            });
        }
        SessionEventKind::ToolResult { output } => {
            let Some(call_id) = output.get("call_id").and_then(Value::as_str) else {
                return;
            };
            let call_id_owned = call_id.to_string();
            let body = output
                .get("output")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| output.to_string());
            conversation.push(ResumeItem::FunctionCallOutput {
                call_id: call_id_owned.clone(),
                output: body,
            });
            // Pair with the buffered call (if we saw one) so the
            // TUI can rebuild a full tool-result card on hydration.
            // A missing call is rare — the agent always writes
            // `ToolCall` before `ToolResult` for the same id — but
            // we still record the result so a resumed session
            // doesn't silently drop tool output entirely.
            let call = replay.pending_tool_calls.remove(&call_id_owned);
            hydrated.push(HydratedTranscriptItem::ToolResult {
                call,
                result: output,
            });
        }
        // Compaction events are handled by the snap-to-checkpoint path in
        // `replay_resume_state`; appearing here means a checkpoint with
        // no `conversation` field, so we treat it as a no-op and let the
        // linear replay continue.
        SessionEventKind::ContextCompacted { .. } => {}
        SessionEventKind::Reasoning { payload } => {
            conversation.push(ResumeItem::Reasoning {
                payload: payload.clone(),
            });
            replay
                .pending_reasoning
                .push(ReasoningSnapshot::from_payload(payload));
        }
        // Approval and session-lifecycle events are bookkeeping rather
        // than conversation items; they do not modify the resume state's
        // conversation/transcript but still need to be enumerated so the
        // match is exhaustive (catches future kinds at compile time).
        // Custom events are extension-authored sidecar data — core
        // replay must ignore them so an extension cannot corrupt the
        // reconstructed conversation by writing arbitrary payloads.
        SessionEventKind::ApprovalRequested { .. }
        | SessionEventKind::ApprovalDecided { .. }
        | SessionEventKind::SessionStarted
        | SessionEventKind::SessionEnded { .. }
        | SessionEventKind::Cancelled
        | SessionEventKind::Failed { .. }
        | SessionEventKind::SessionResumed
        | SessionEventKind::Custom { .. }
        | SessionEventKind::Unknown => {}
    }
}

fn read_jsonl(path: &Path) -> Result<(Vec<SessionEvent>, u64)> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(error) => return Err(error.into()),
    };
    let mut events = Vec::new();
    let mut warnings = 0;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<SessionEvent>(trimmed) {
            Ok(event) => events.push(event),
            Err(_) => warnings += 1,
        }
    }
    Ok((events, warnings))
}

fn read_replay_jsonl(path: &Path) -> Result<(Vec<SessionReplayEvent>, u64)> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(error) => return Err(error.into()),
    };
    let mut events = Vec::new();
    let mut warnings = 0;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match parse_replay_jsonl_line(trimmed) {
            Some(event) => events.push(event),
            None => warnings += 1,
        }
    }
    Ok((events, warnings))
}

fn count_replay_jsonl(path: &Path) -> Result<(u64, u64)> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((0, 0)),
        Err(error) => return Err(error.into()),
    };
    let mut events = 0;
    let mut warnings = 0;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if parse_replay_jsonl_line(trimmed).is_some() {
            events += 1;
        } else {
            warnings += 1;
        }
    }
    Ok((events, warnings))
}

fn parse_replay_jsonl_line(line: &str) -> Option<SessionReplayEvent> {
    let event = serde_json::from_str::<SessionReplayEvent>(line).ok()?;
    if event.schema_version == SESSION_REPLAY_SCHEMA_VERSION
        && event.payload_sha256 == replay_payload_sha256(&event.payload)
    {
        Some(event)
    } else {
        None
    }
}

fn replay_payload_sha256(payload: &Value) -> String {
    use std::fmt::Write as _;

    let bytes = serde_json::to_vec(payload).unwrap_or_default();
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn next_session_id() -> String {
    let counter = NEXT_SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nonce = random_nonce_hex();
    format!("{}-{}-{counter}-{nonce}", now_ms(), std::process::id())
}

fn contains_if_set(value: &str, needle: &Option<String>) -> bool {
    needle.as_ref().is_none_or(|needle| {
        value
            .to_ascii_lowercase()
            .contains(&needle.to_ascii_lowercase())
    })
}

fn equals_if_set(value: &str, expected: &Option<String>) -> bool {
    expected
        .as_ref()
        .is_none_or(|expected| value.eq_ignore_ascii_case(expected))
}

fn git_identity(root: &Path) -> (Option<String>, Option<String>) {
    let repo_root = git_output(root, &["rev-parse", "--show-toplevel"]);
    let branch = git_output(root, &["branch", "--show-current"]).or_else(|| {
        git_output(root, &["rev-parse", "--short", "HEAD"]).map(|sha| format!("detached:{sha}"))
    });
    (repo_root, branch)
}

fn git_output(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn to_json_vec(value: &impl Serialize) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(json_error)
}

/// Serialise a `SessionEvent` to its newline-terminated events.jsonl
/// payload, applying the per-event truncation policy that `append_event`
/// uses inline. Extracted so the lazy materialisation path can flush
/// buffered lifecycle events through the writer without duplicating the
/// truncation logic.
fn serialize_event_payload(event: &SessionEvent, max_event_bytes: usize) -> Result<Vec<u8>> {
    let mut payload = to_json_vec(event)?;
    if payload.len() > max_event_bytes {
        payload = to_json_vec(&SessionEvent {
            ts_unix_ms: event.ts_unix_ms,
            kind: event.kind.clone(),
            turn_id: event.turn_id.clone(),
            summary: event.summary.clone(),
            payload: json!({
                "truncated": true,
                "reason": "event exceeded max_event_bytes",
                "original_bytes": payload.len(),
            }),
            parent_event_sequence: event.parent_event_sequence,
        })?;
    }
    payload.push(b'\n');
    Ok(payload)
}

fn json_error(error: serde_json::Error) -> SqueezyError {
    SqueezyError::Tool(format!("session store JSON error: {error}"))
}

/// Resolve the XDG-aware path for the global session index. When
/// `XDG_STATE_HOME` is set it takes precedence (Linux XDG Base Dir Spec);
/// otherwise falls back to `$HOME/.squeezy/sessions/index.jsonl` to preserve
/// existing macOS/Windows state.
fn xdg_global_index_path() -> Option<PathBuf> {
    if let Some(xdg) = env::var_os("XDG_STATE_HOME") {
        return Some(
            PathBuf::from(xdg)
                .join("squeezy")
                .join("sessions")
                .join("index.jsonl"),
        );
    }
    let home = env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".squeezy")
            .join("sessions")
            .join("index.jsonl"),
    )
}

/// Read one global index file into `by_id`, keeping the entry with the largest
/// `last_event_at_ms` for each session id. `raw_lines` is incremented for each
/// valid entry parsed.
fn read_global_index_into(
    path: &Path,
    by_id: &mut HashMap<String, GlobalSessionIndexEntry>,
    raw_lines: &mut usize,
) {
    let Ok(file) = fs::File::open(path) else {
        return;
    };
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => return,
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<GlobalSessionIndexEntry>(trimmed) else {
            continue;
        };
        *raw_lines += 1;
        match by_id.get(&entry.session_id) {
            Some(existing) if existing.last_event_at_ms >= entry.last_event_at_ms => continue,
            _ => {
                by_id.insert(entry.session_id.clone(), entry);
            }
        }
    }
}

/// Build a unique sibling temp path for `path` using the process id, thread
/// id, and a random 4-byte nonce. This prevents both same-pid concurrent
/// threads and same-millisecond different-pid processes from colliding on the
/// same temp file before the rename.
fn unique_tmp_path(path: &Path) -> PathBuf {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("data");
    let pid = std::process::id();
    let tid = thread_id_u64();
    let nonce = random_nonce_hex();
    path.with_file_name(format!(".{name}.{pid}.{tid}.{nonce}.tmp"))
}

/// Current thread id as a `u64`. Uses the OS thread handle address as a
/// stable discriminator inside a process.
fn thread_id_u64() -> u64 {
    // thread::current().id() doesn't give a numeric value on stable Rust,
    // but its Debug representation embeds the number. We extract it to get
    // a cheap discriminator without unsafe code.
    let id = thread::current().id();
    let debug = format!("{id:?}");
    // ThreadId(N) → extract N
    debug
        .trim_start_matches("ThreadId(")
        .trim_end_matches(')')
        .parse::<u64>()
        .unwrap_or(0)
}

/// Generate 4 random bytes and encode them as an 8-character hex string.
/// Falls back to a timestamp-derived value when the OS RNG is unavailable.
fn random_nonce_hex() -> String {
    use std::fmt::Write as _;
    let mut buf = [0u8; 4];
    if getrandom::fill(&mut buf).is_ok() {
        let mut s = String::with_capacity(8);
        for b in buf {
            let _ = write!(s, "{b:02x}");
        }
        s
    } else {
        // Degenerate fallback: use the low bits of the current timestamp.
        format!("{:08x}", now_ms() & 0xffff_ffff)
    }
}

/// Fsync the parent directory of `path` after a successful rename to ensure
/// the directory entry update is durable on Linux crash-safety scenarios.
/// This is a best-effort operation — the session file content is already
/// durable (synced before rename); a missed dir sync only costs durability
/// of the directory entry, not the file content.
#[cfg_attr(not(unix), allow(unused_variables))]
fn fsync_parent(path: &Path) {
    #[cfg(unix)]
    if let Some(parent) = path.parent()
        && let Ok(dir) = fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
}

/// Threshold in milliseconds after which a `Running` session with no
/// recent events is considered stale. Used to warn in session listing.
pub const STALE_RUNNING_SESSION_THRESHOLD_MS: u64 = 24 * 60 * 60 * 1000;

#[cfg(test)]
#[path = "sessions_tests.rs"]
mod tests;
