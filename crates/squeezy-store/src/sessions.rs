use std::{
    collections::HashMap,
    env,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use squeezy_core::{
    AppConfig, ContextAttachment, ContextCompactionState, CostSnapshot, ReasoningPayload, Result,
    SessionMetrics, SessionMode, SqueezyError, TranscriptItem,
};

static NEXT_SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);
pub const SESSION_REPLAY_SCHEMA_VERSION: u32 = 1;
/// Schema version stamped onto every `RolloutEvent` emitted by
/// [`SessionStore::bundle_rollout_trace`]. The reducer is additive over
/// `events.jsonl` + `replay.jsonl`, so bumping this only requires changing
/// the merge logic or the wire shape of `RolloutEvent` itself.
pub const ROLLOUT_TRACE_SCHEMA_VERSION: u32 = 1;
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

    /// Path to the cross-project session index, an append-only JSONL file
    /// at `~/.squeezy/sessions/index.jsonl`. Per-project session roots
    /// live under each workspace, so a global index is the only way the
    /// resume picker can show sessions started from sibling repos.
    /// Returns `None` when `HOME` is unset — same condition under which
    /// the user-global memory file declines to operate.
    pub fn global_index_path() -> Option<PathBuf> {
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
        let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) else {
            return;
        };
        let _ = file.write_all(&payload);
    }

    /// Read the cross-project session index, deduping by `session_id` and
    /// keeping the entry with the largest `last_event_at_ms` for each id.
    /// When the file exceeds [`GLOBAL_INDEX_COMPACT_THRESHOLD_BYTES`], the
    /// deduped snapshot is rewritten atomically (tmp + rename) so the
    /// next read stays fast. Returns entries newest-first by
    /// `started_at_ms` so callers can take a recency-prefixed slice
    /// without re-sorting.
    pub fn list_global_index() -> Vec<GlobalSessionIndexEntry> {
        let Some(path) = Self::global_index_path() else {
            return Vec::new();
        };
        if !path.exists() {
            return Vec::new();
        }
        let Ok(text) = fs::read_to_string(&path) else {
            return Vec::new();
        };
        let mut by_id: HashMap<String, GlobalSessionIndexEntry> = HashMap::new();
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<GlobalSessionIndexEntry>(line) else {
                continue;
            };
            match by_id.get(&entry.session_id) {
                Some(existing) if existing.last_event_at_ms >= entry.last_event_at_ms => continue,
                _ => {
                    by_id.insert(entry.session_id.clone(), entry);
                }
            }
        }
        let should_compact = fs::metadata(&path)
            .map(|meta| meta.len() > GLOBAL_INDEX_COMPACT_THRESHOLD_BYTES)
            .unwrap_or(false);
        if should_compact {
            let mut entries: Vec<&GlobalSessionIndexEntry> = by_id.values().collect();
            // Compact in oldest-first order so future appends keep the
            // newest entries at the tail — matches how readers see time.
            entries.sort_by_key(|entry| entry.started_at_ms);
            let _ = rewrite_global_index(&path, &entries);
        }
        let mut entries: Vec<GlobalSessionIndexEntry> = by_id.into_values().collect();
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.started_at_ms));
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
    fn record_global_index(metadata: &SessionMetadata) {
        if skip_global_index_for_test_workspace(&metadata.workspace_root) {
            return;
        }
        let entry = GlobalSessionIndexEntry::from_metadata(metadata, now_ms());
        Self::append_global_index_entry(&entry);
    }

    pub fn start_session(&self, mut metadata: SessionMetadata) -> Result<SessionHandle> {
        metadata.session_id = next_session_id();
        metadata.started_at_ms = now_ms();
        metadata.status = SessionStatus::Running;
        metadata.resume_available = true;
        let dir = self.session_dir(&metadata.session_id);
        fs::create_dir_all(&dir)?;
        write_json(&dir.join("metadata.json"), &metadata)?;
        write_json(
            &dir.join("resume_state.json"),
            &SessionResumeState {
                resume_available: true,
                ..SessionResumeState::default()
            },
        )?;
        Self::record_global_index(&metadata);
        Ok(SessionHandle {
            store: self.clone(),
            session_id: metadata.session_id,
            counters: Arc::new(HandleCounters::default()),
            event_writer: SessionLogWriter::spawn(self.clone(), dir),
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
            read_json::<SessionMetadata>(&self.session_dir(&session_id).join("metadata.json"))
        {
            counters
                .event_count
                .store(metadata.event_count, Ordering::Relaxed);
            counters
                .has_first_user_task
                .store(metadata.first_user_task.is_some(), Ordering::Relaxed);
        }
        if let Ok(tape) = self.replay_tape(&session_id) {
            counters
                .replay_count
                .store(tape.events.len() as u64, Ordering::Relaxed);
        }
        SessionHandle {
            store: self.clone(),
            session_id: session_id.clone(),
            counters: Arc::new(counters),
            event_writer: SessionLogWriter::spawn(self.clone(), self.session_dir(&session_id)),
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
        let dir = self.session_dir(handle.session_id());
        write_json(&dir.join("resume_state.json"), &parent_resume)?;
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
            let Ok(metadata) = serde_json::from_str::<SessionMetadata>(&text) else {
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
                    let Ok(metadata) = serde_json::from_str::<SessionMetadata>(&text) else {
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
            && let Ok(mut metadata) = serde_json::from_str::<SessionMetadata>(&text)
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
            && let Ok(mut metadata) = serde_json::from_str::<SessionMetadata>(&text)
        {
            metadata.status = SessionStatus::Completed;
            metadata.archived_at_ms = None;
            let _ = write_json(&metadata_path, &metadata);
            Self::record_global_index(&metadata);
        }
        Ok(())
    }

    pub fn show(&self, session_id: &str) -> Result<SessionRecord> {
        let dir = self.locate_session_dir(session_id);
        let metadata = read_json(&dir.join("metadata.json"))?;
        let (events, event_warnings) = read_jsonl(&dir.join("events.jsonl"))?;
        let resume_state = read_json(&dir.join("resume_state.json")).ok();
        let attachments = read_context_attachments(&dir.join("attachments"))?;
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
    event_writer: Arc<SessionLogWriter>,
}

#[derive(Debug, Default)]
struct HandleCounters {
    event_count: AtomicU64,
    replay_count: AtomicU64,
    has_first_user_task: AtomicBool,
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
    for command in rx {
        match command {
            SessionLogCmd::Append(append) => {
                if terminal_failure.is_some() {
                    continue;
                }
                if let Err(error) =
                    write_session_log_append(&store, &dir, &path, &mut current_size, append)
                {
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
    append: SessionLogAppend,
) -> Result<()> {
    fs::create_dir_all(dir)?;
    if current_size.saturating_add(append.payload.len()) > store.max_session_bytes {
        update_metadata_file(dir, |metadata| {
            metadata.status = SessionStatus::Truncated;
            metadata.resume_available = false;
            metadata.resume_unavailable_reason =
                Some("session exceeded max_session_bytes".to_string());
        })?;
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
    let mut metadata: SessionMetadata = read_json(&path)?;
    update(&mut metadata);
    write_json(&path, &metadata)
}

impl SessionHandle {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn metadata(&self) -> Result<SessionMetadata> {
        let mut metadata: SessionMetadata = read_json(&self.dir().join("metadata.json"))?;
        // Surface the in-memory event_count even when we have intentionally
        // skipped writing metadata.json for routine events.
        let cached = self.counters.event_count.load(Ordering::Relaxed);
        if cached > metadata.event_count {
            metadata.event_count = cached;
        }
        Ok(metadata)
    }

    pub fn update_metadata(&self, update: impl FnOnce(&mut SessionMetadata)) -> Result<()> {
        let mut metadata = self.metadata()?;
        update(&mut metadata);
        write_json(&self.dir().join("metadata.json"), &metadata)
    }

    pub fn flush_events(&self) -> Result<()> {
        self.event_writer.flush()
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

        self.event_writer.append(SessionLogAppend { payload })?;
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

        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        file.write_all(&payload)?;
        Ok(())
    }

    pub fn write_resume_state(&self, state: &SessionResumeState) -> Result<()> {
        write_json(&self.dir().join("resume_state.json"), state)
    }

    pub fn read_resume_state(&self) -> Result<SessionResumeState> {
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
        let (events, _warnings) = read_jsonl(&self.dir().join("events.jsonl"))?;
        let mut conversation: Vec<ResumeItem> = Vec::new();
        let mut transcript: Vec<TranscriptItem> = Vec::new();
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
                    apply_event_to_replay(forward, &mut conversation, &mut transcript);
                }
                return Ok(SessionResumeState {
                    resume_available: true,
                    previous_response_id: None,
                    conversation,
                    transcript,
                    context_attachments: self.context_attachments().unwrap_or_default(),
                    context_compaction: ContextCompactionState::default(),
                });
            }
        }
        for event in &events {
            apply_event_to_replay(event, &mut conversation, &mut transcript);
        }
        Ok(SessionResumeState {
            resume_available: true,
            previous_response_id: None,
            conversation,
            transcript,
            context_attachments: self.context_attachments().unwrap_or_default(),
            context_compaction: ContextCompactionState::default(),
        })
    }

    pub fn write_context_attachment(
        &self,
        attachment: &ContextAttachment,
        redacted_text: Option<&str>,
    ) -> Result<()> {
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
        read_context_attachments(&self.dir().join("attachments"))
    }

    pub fn finish(
        &self,
        status: SessionStatus,
        cost: CostSnapshot,
        metrics: SessionMetrics,
        redactions: u64,
    ) -> Result<()> {
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
            started_at_ms: metadata.started_at_ms,
            last_event_at_ms,
            turn_count: metadata.metrics.turns,
            resume_available,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionMetadata {
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
    pub transcript: Vec<TranscriptItem>,
    #[serde(default)]
    pub context_attachments: Vec<ContextAttachment>,
    #[serde(default)]
    pub context_compaction: ContextCompactionState,
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
/// The CLI/TUI surfaces this as `/session-cleanup --archive` (default)
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

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(value).map_err(json_error)?;
    fs::write(path, bytes)?;
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
    let home_under_temp = env::var_os("HOME")
        .and_then(|home| Path::new(&home).canonicalize().ok())
        .map(|canonical| canonical.starts_with(&temp_dir))
        .unwrap_or(false);
    !home_under_temp
}

/// Replace the global session index file with the supplied entries via a
/// tmp + rename so concurrent readers never see a half-written file. The
/// caller chooses the iteration order; readers re-sort by
/// `started_at_ms`.
fn rewrite_global_index(path: &Path, entries: &[&GlobalSessionIndexEntry]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("jsonl.tmp");
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
    fs::rename(&tmp, path)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let text = fs::read_to_string(path)?;
    serde_json::from_str(&text).map_err(json_error)
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

fn apply_event_to_replay(
    event: &SessionEvent,
    conversation: &mut Vec<ResumeItem>,
    transcript: &mut Vec<TranscriptItem>,
) {
    let Some(typed) = SessionEventKind::try_from_event(event) else {
        return;
    };
    match typed {
        SessionEventKind::UserMessage { text } => {
            conversation.push(ResumeItem::UserText { text: text.clone() });
            transcript.push(TranscriptItem::user(text));
        }
        SessionEventKind::AssistantCompleted { text, .. } => {
            if text.is_empty() {
                return;
            }
            conversation.push(ResumeItem::AssistantText { text: text.clone() });
            transcript.push(TranscriptItem::assistant(text));
        }
        SessionEventKind::ToolCall {
            call_id,
            tool,
            arguments,
        } => {
            if call_id.is_empty() {
                return;
            }
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
            let body = output
                .get("output")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| output.to_string());
            conversation.push(ResumeItem::FunctionCallOutput {
                call_id: call_id.to_string(),
                output: body,
            });
        }
        // Compaction events are handled by the snap-to-checkpoint path in
        // `replay_resume_state`; appearing here means a checkpoint with
        // no `conversation` field, so we treat it as a no-op and let the
        // linear replay continue.
        SessionEventKind::ContextCompacted { .. } => {}
        SessionEventKind::Reasoning { payload } => {
            conversation.push(ResumeItem::Reasoning { payload });
        }
        // Approval and session-lifecycle events are bookkeeping rather
        // than conversation items; they do not modify the resume state's
        // conversation/transcript but still need to be enumerated so the
        // match is exhaustive (catches future kinds at compile time).
        SessionEventKind::ApprovalRequested { .. }
        | SessionEventKind::ApprovalDecided { .. }
        | SessionEventKind::SessionStarted
        | SessionEventKind::SessionEnded { .. }
        | SessionEventKind::Cancelled
        | SessionEventKind::Failed { .. }
        | SessionEventKind::SessionResumed
        | SessionEventKind::Unknown => {}
    }
}

fn read_jsonl(path: &Path) -> Result<(Vec<SessionEvent>, u64)> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(error) => return Err(error.into()),
    };
    let mut events = Vec::new();
    let mut warnings = 0;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<SessionEvent>(line) {
            Ok(event) => events.push(event),
            Err(_) => warnings += 1,
        }
    }
    Ok((events, warnings))
}

fn read_replay_jsonl(path: &Path) -> Result<(Vec<SessionReplayEvent>, u64)> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(error) => return Err(error.into()),
    };
    let mut events = Vec::new();
    let mut warnings = 0;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<SessionReplayEvent>(line) {
            Ok(event)
                if event.schema_version == SESSION_REPLAY_SCHEMA_VERSION
                    && event.payload_sha256 == replay_payload_sha256(&event.payload) =>
            {
                events.push(event)
            }
            Ok(_) | Err(_) => warnings += 1,
        }
    }
    Ok((events, warnings))
}

fn replay_payload_sha256(payload: &Value) -> String {
    let bytes = serde_json::to_vec(payload).unwrap_or_default();
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn next_session_id() -> String {
    let counter = NEXT_SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{}-{counter}", now_ms(), std::process::id())
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

fn json_error(error: serde_json::Error) -> SqueezyError {
    SqueezyError::Tool(format!("session store JSON error: {error}"))
}

#[cfg(test)]
#[path = "sessions_tests.rs"]
mod tests;
