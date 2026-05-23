use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use squeezy_core::{
    AppConfig, CostSnapshot, Result, SessionMetrics, SessionMode, SqueezyError, TranscriptItem,
};

static NEXT_SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);

pub const CRATE_NAME: &str = "squeezy-store";

pub fn crate_name() -> &'static str {
    CRATE_NAME
}

#[derive(Debug, Clone)]
pub struct SessionStore {
    root: PathBuf,
    retention_days: u64,
    max_event_bytes: usize,
    max_session_bytes: usize,
}

impl SessionStore {
    pub fn open(config: &AppConfig) -> Self {
        let root = session_root(config);
        Self {
            root,
            retention_days: config.session_logs.log_retention_days,
            max_event_bytes: config.session_logs.max_event_bytes,
            max_session_bytes: config.session_logs.max_session_bytes,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
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
        Ok(SessionHandle {
            store: self.clone(),
            session_id: metadata.session_id,
            counters: Arc::new(HandleCounters::default()),
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
        SessionHandle {
            store: self.clone(),
            session_id,
            counters: Arc::new(counters),
        }
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
        sessions.sort_by_key(|session| std::cmp::Reverse(session.started_at_ms));
        Ok(sessions)
    }

    pub fn show(&self, session_id: &str) -> Result<SessionRecord> {
        let dir = self.session_dir(session_id);
        let metadata = read_json(&dir.join("metadata.json"))?;
        let (events, event_warnings) = read_jsonl(&dir.join("events.jsonl"))?;
        let resume_state = read_json(&dir.join("resume_state.json")).ok();
        Ok(SessionRecord {
            metadata,
            events,
            event_warnings,
            resume_state,
        })
    }

    pub fn export(&self, session_id: &str) -> Result<Value> {
        let record = self.show(session_id)?;
        Ok(json!({
            "metadata": record.metadata,
            "events": record.events,
            "event_warnings": record.event_warnings,
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
    pub fn cleanup_excluding(
        &self,
        ids: &[String],
        protected_id: Option<&str>,
    ) -> Result<CleanupReport> {
        let mut removed = Vec::new();
        let cutoff = now_ms().saturating_sub(self.retention_days.saturating_mul(86_400_000));
        let explicit: std::collections::BTreeSet<&str> = ids.iter().map(String::as_str).collect();
        for metadata in self.list(&SessionQuery::default())? {
            if protected_id == Some(metadata.session_id.as_str()) {
                continue;
            }
            let is_explicit = explicit.contains(metadata.session_id.as_str());
            // Never sweep a `Running` session through retention alone: it may
            // belong to a long-lived process whose `ended_at_ms` simply isn't
            // set yet. Explicit ids still win so users can force-remove a
            // crashed or stuck session.
            let expired = match metadata.ended_at_ms {
                Some(end) => end < cutoff,
                None => {
                    !matches!(metadata.status, SessionStatus::Running)
                        && metadata.started_at_ms < cutoff
                }
            };
            if is_explicit || expired {
                let dir = self.session_dir(&metadata.session_id);
                fs::remove_dir_all(&dir)?;
                removed.push(metadata.session_id);
            }
        }
        Ok(CleanupReport { removed })
    }

    pub fn remove_session(&self, session_id: &str) -> Result<()> {
        let dir = self.session_dir(session_id);
        if dir.exists() {
            fs::remove_dir_all(dir)?;
        }
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
}

#[derive(Debug, Default)]
struct HandleCounters {
    event_count: AtomicU64,
    has_first_user_task: AtomicBool,
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

    pub fn append_event(&self, event: SessionEvent) -> Result<()> {
        let dir = self.dir();
        fs::create_dir_all(&dir)?;
        let path = dir.join("events.jsonl");
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
            })?;
        }
        payload.push(b'\n');

        let current_size = fs::metadata(&path).map_or(0, |metadata| metadata.len() as usize);
        if current_size.saturating_add(payload.len()) > self.store.max_session_bytes {
            self.update_metadata(|metadata| {
                metadata.status = SessionStatus::Truncated;
                metadata.resume_available = false;
                metadata.resume_unavailable_reason =
                    Some("session exceeded max_session_bytes".to_string());
            })?;
            return Ok(());
        }

        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        file.write_all(&payload)?;
        // Hot-path bookkeeping lives in memory: the on-disk event_count is
        // resynced lazily during `metadata()` / `update_metadata`, and the
        // file write below only fires when a discovery-visible field is
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
        }
        Ok(())
    }

    pub fn write_resume_state(&self, state: &SessionResumeState) -> Result<()> {
        write_json(&self.dir().join("resume_state.json"), state)
    }

    pub fn read_resume_state(&self) -> Result<SessionResumeState> {
        read_json(&self.dir().join("resume_state.json"))
    }

    pub fn finish(
        &self,
        status: SessionStatus,
        cost: CostSnapshot,
        metrics: SessionMetrics,
        redactions: u64,
    ) -> Result<()> {
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
        })
    }

    fn dir(&self) -> PathBuf {
        self.store.session_dir(&self.session_id)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMetadata {
    pub session_id: String,
    pub started_at_ms: u64,
    pub ended_at_ms: Option<u64>,
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
    Completed,
    Cancelled,
    Failed,
    Truncated,
}

impl SessionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
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
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionResumeState {
    pub resume_available: bool,
    pub previous_response_id: Option<String>,
    pub conversation: Vec<ResumeItem>,
    pub transcript: Vec<TranscriptItem>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub metadata: SessionMetadata,
    pub events: Vec<SessionEvent>,
    pub event_warnings: u64,
    pub resume_state: Option<SessionResumeState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupReport {
    pub removed: Vec<String>,
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

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(value).map_err(json_error)?;
    fs::write(path, bytes)?;
    Ok(())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let text = fs::read_to_string(path)?;
    serde_json::from_str(&text).map_err(json_error)
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

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;

fn to_json_vec(value: &impl Serialize) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(json_error)
}

fn json_error(error: serde_json::Error) -> SqueezyError {
    SqueezyError::Tool(format!("session store JSON error: {error}"))
}
