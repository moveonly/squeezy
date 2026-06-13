use std::{
    collections::BTreeMap,
    env, fs,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use squeezy_core::{AppConfig, ProviderConfig, TelemetryConfig, TurnMetrics};
use tokio::{sync::Mutex, time};

const SCHEMA_VERSION: u32 = 1;
const TELEMETRY_STORE_SCHEMA_VERSION: u64 = 1;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const FLUSH_INTERVAL: Duration = Duration::from_secs(5);
const MAX_BATCH_EVENTS: usize = 50;
const MAX_LOCAL_QUEUE_EVENTS: usize = 512;
const MAX_SUMMARY_MAP_ENTRIES: usize = 16;
const PENDING_SEND_LIMIT: usize = 20;
const PENDING_LEASE_MS: u128 = 60_000;

const STORE_META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const STORE_SESSIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("sessions");
const STORE_EVENTS: TableDefinition<&str, &[u8]> = TableDefinition::new("events");
const STORE_PENDING: TableDefinition<&str, &[u8]> = TableDefinition::new("pending_summaries");

#[derive(Debug, Clone)]
pub struct TelemetryClient {
    state: Option<Arc<TelemetryState>>,
}

#[derive(Debug)]
struct TelemetryState {
    endpoint: String,
    install_id: String,
    session_id: String,
    /// Per-session trace id (W3C-shaped 32-hex-char string) stamped on
    /// every emitted event. Lets an operator pivot from a single Worker-side
    /// event back to all other events emitted from the same Squeezy session,
    /// and lets local `tracing` logs that record the same id pull in their
    /// matching aggregate counters.
    trace_id: String,
    session_started_at_ms: u128,
    session_registered: std::sync::Mutex<bool>,
    /// Per-turn span id stamped on every event enqueued between
    /// [`TelemetryClient::begin_turn`] and [`TelemetryClient::end_turn`].
    /// `None` outside an active turn (e.g. `app_started`). 16-hex-char string.
    current_span_id: std::sync::Mutex<Option<String>>,
    /// Durable on-disk session ID from `SessionHandle::session_id()`. Set
    /// once by the agent layer after building the session log, then stamped
    /// on every subsequent event so any individual PostHog event can be
    /// joined back to the session file without needing the summary.
    store_session_id: std::sync::Mutex<Option<String>>,
    next_event_sequence: AtomicU64,
    queue: Mutex<TelemetryQueue>,
    /// Serializes concurrent calls to `send_pending_summaries` so that the
    /// startup-retry task, the periodic 5-second flush, and the exit flush
    /// cannot simultaneously lease and double-send the same pending summary.
    flush_lock: Mutex<()>,
    store: Option<Arc<TelemetryStore>>,
    http: reqwest::Client,
}

#[derive(Debug, Clone)]
pub struct FeedbackClient {
    state: Option<Arc<FeedbackState>>,
}

#[derive(Debug)]
struct FeedbackState {
    feedback_endpoint: String,
    report_endpoint: String,
    max_feedback_bytes: usize,
    max_report_bytes: usize,
    install_id: String,
    session_id: String,
    http: reqwest::Client,
}

#[derive(Debug, Default)]
struct TelemetryQueue {
    events: Vec<TelemetryEvent>,
    flush_scheduled: bool,
}

impl TelemetryClient {
    pub fn from_config(config: &AppConfig) -> Self {
        Self::from_config_with_install_path(config, default_install_id_path())
    }

    pub fn disabled() -> Self {
        Self { state: None }
    }

    pub fn from_config_with_install_path(
        config: &AppConfig,
        install_id_path: impl AsRef<Path>,
    ) -> Self {
        if !config.telemetry.enabled {
            purge_telemetry_store_for_install_path(install_id_path.as_ref());
            return Self::disabled();
        }
        // If we cannot persist a stable install_id, treat telemetry as
        // unavailable rather than fabricating a fresh anonymous user per
        // process. The previous fallback to `random_uuid_like()` silently
        // violated the documented "stable across sessions on that machine"
        // guarantee and inflated unique-user counts in degraded environments
        // (CI, read-only $HOME, missing $HOME, ENOSPC).
        let install_id = match load_or_create_install_id(install_id_path.as_ref()) {
            Ok(id) => id,
            Err(_) => return Self::disabled(),
        };
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        let session_id = random_uuid_like();
        let trace_id = random_trace_id();
        let store = TelemetryStore::open(default_telemetry_store_path_for_install_path(
            install_id_path.as_ref(),
        ))
        .ok()
        .map(Arc::new);
        if let Some(store) = store.as_ref() {
            let _ = store.synthesize_abnormal_sessions(&session_id);
        }
        let state = Arc::new(TelemetryState {
            endpoint: config.telemetry.endpoint.clone(),
            install_id,
            session_id,
            trace_id,
            session_started_at_ms: now_ms(),
            session_registered: std::sync::Mutex::new(false),
            current_span_id: std::sync::Mutex::new(None),
            store_session_id: std::sync::Mutex::new(None),
            next_event_sequence: AtomicU64::new(1),
            queue: Mutex::new(TelemetryQueue::default()),
            flush_lock: Mutex::new(()),
            store,
            http,
        });
        schedule_pending_retry(state.clone());
        Self { state: Some(state) }
    }

    pub fn enabled(&self) -> bool {
        self.state.is_some()
    }

    /// Open a per-turn span. The returned id is stamped on every event
    /// recorded between this call and [`Self::end_turn`]. Calling
    /// `begin_turn` again replaces the active span (the prior span is
    /// abandoned without a close event — this matches `tracing::Span`
    /// drop semantics). Returns `None` when telemetry is disabled.
    pub fn begin_turn(&self) -> Option<String> {
        let state = self.state.as_ref()?;
        let id = random_span_id();
        if let Ok(mut guard) = state.current_span_id.lock() {
            *guard = Some(id.clone());
        }
        Some(id)
    }

    /// Close the active per-turn span so subsequent events (e.g.
    /// post-turn `failure_seen`) carry no `span_id` until the next
    /// [`Self::begin_turn`].
    pub fn end_turn(&self) {
        let Some(state) = self.state.as_ref() else {
            return;
        };
        if let Ok(mut guard) = state.current_span_id.lock() {
            *guard = None;
        }
    }

    /// Per-session trace id. `None` when telemetry is disabled. Exposed
    /// so the agent/CLI layer can include it in local `tracing` spans for
    /// log-to-aggregate correlation without re-deriving the id.
    pub fn trace_id(&self) -> Option<String> {
        self.state.as_ref().map(|state| state.trace_id.clone())
    }

    /// The session id assigned to this telemetry client. `None` when telemetry
    /// is disabled. Exposed so call sites can thread the same id into
    /// `FeedbackClient::from_config_with_session` so that feedback and bug
    /// reports in PostHog are correlated with the product session summary.
    pub fn session_id(&self) -> Option<String> {
        self.state.as_ref().map(|state| state.session_id.clone())
    }

    /// Bind the durable on-disk session ID to this client. Once set, every
    /// subsequent event is stamped with `store_session_id` so any individual
    /// PostHog event can be correlated with the session file without needing
    /// the summary. Called by the agent layer immediately after the session
    /// log is opened.
    pub fn set_store_session_id(&self, id: &str) {
        let Some(state) = self.state.as_ref() else {
            return;
        };
        if let Ok(mut guard) = state.store_session_id.lock() {
            *guard = Some(id.to_string());
        }
    }

    pub fn spawn(&self, event: TelemetryEvent) {
        let Some(state) = self.state.clone() else {
            return;
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                enqueue_event(state, event).await;
            });
        } else {
            enqueue_event_without_runtime(&state, event);
        }
    }

    pub async fn record(&self, event: TelemetryEvent) {
        let Some(state) = self.state.clone() else {
            return;
        };
        enqueue_event(state, event).await;
    }

    pub async fn send(&self, events: Vec<TelemetryEvent>) -> Result<(), TelemetryError> {
        let Some(state) = self.state.clone() else {
            return Ok(());
        };
        let mut stamped = events;
        for event in &mut stamped {
            stamp_trace_ids(&state, event);
        }
        if stamped.is_empty() {
            return Ok(());
        }
        let summary = build_summary_from_events(
            &StoredTelemetrySession::from_state(&state),
            stamped,
            false,
            Some(SessionStatusKind::Completed),
        );
        send_batch_for_session(state.clone(), state.session_id.clone(), vec![summary]).await
    }

    pub async fn flush(&self) -> Result<(), TelemetryError> {
        let Some(state) = self.state.clone() else {
            return Ok(());
        };
        if let Some(store) = state.store.as_ref() {
            store.finalize_session_summary(&state.session_id, false)?;
            send_pending_summaries(state).await
        } else {
            let events = drain_queued_events(&state).await;
            if events.is_empty() {
                return Ok(());
            }
            let summary = build_summary_from_events(
                &StoredTelemetrySession::from_state(&state),
                events,
                false,
                Some(SessionStatusKind::Completed),
            );
            send_batch_for_session(state.clone(), state.session_id.clone(), vec![summary]).await
        }
    }

    pub async fn retry_pending_from_config(config: &AppConfig) -> Result<(), TelemetryError> {
        if !config.telemetry.enabled {
            purge_telemetry_store_for_install_path(&default_install_id_path());
            return Ok(());
        }
        let install_id = match load_or_create_install_id(&default_install_id_path()) {
            Ok(id) => id,
            Err(_) => return Ok(()),
        };
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        let store = TelemetryStore::open(default_telemetry_store_path_for_install_path(
            &default_install_id_path(),
        ))
        .ok()
        .map(Arc::new);
        let Some(store) = store else {
            return Ok(());
        };
        let state = Arc::new(TelemetryState {
            endpoint: config.telemetry.endpoint.clone(),
            install_id,
            session_id: random_uuid_like(),
            trace_id: random_trace_id(),
            session_started_at_ms: now_ms(),
            session_registered: std::sync::Mutex::new(false),
            current_span_id: std::sync::Mutex::new(None),
            store_session_id: std::sync::Mutex::new(None),
            next_event_sequence: AtomicU64::new(1),
            queue: Mutex::new(TelemetryQueue::default()),
            flush_lock: Mutex::new(()),
            store: Some(store),
            http,
        });
        send_pending_summaries(state).await
    }

    /// Snapshot of the events the client has accepted but not yet
    /// flushed. Exposed for integration tests in sibling crates so they
    /// can assert that a code path enqueued a specific
    /// [`TelemetryEvent`] without standing up an HTTP server. Returns an
    /// empty `Vec` when the client is disabled.
    pub async fn pending_events_snapshot(&self) -> Vec<TelemetryEvent> {
        let Some(state) = self.state.as_ref() else {
            return Vec::new();
        };
        state.queue.lock().await.events.clone()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedFeedback {
    pub feedback_id: String,
    pub message: String,
    pub message_bytes: usize,
    pub redactions: u64,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportUpload<'a> {
    pub report_id: &'a str,
    pub session_id: &'a str,
    pub archive_bytes: &'a [u8],
    pub redactions: u64,
    pub sections: Vec<String>,
    pub source: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedbackSubmitResult {
    pub id: String,
}

#[derive(Debug)]
pub enum FeedbackError {
    Disabled,
    Io(std::io::Error),
    Http(reqwest::Error),
    Status(reqwest::StatusCode),
    InvalidResponse(String),
    TooLarge { bytes: usize, max_bytes: usize },
}

impl std::fmt::Display for FeedbackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => write!(f, "feedback is disabled"),
            Self::Io(error) => write!(f, "feedback setup failed: {error}"),
            Self::Http(error) => write!(f, "feedback request failed: {error}"),
            Self::Status(status) => write!(f, "feedback endpoint returned HTTP {status}"),
            Self::InvalidResponse(message) => {
                write!(f, "feedback endpoint response was invalid: {message}")
            }
            Self::TooLarge { bytes, max_bytes } => {
                write!(
                    f,
                    "feedback payload is {bytes} bytes, exceeding limit {max_bytes}"
                )
            }
        }
    }
}

impl std::error::Error for FeedbackError {}

impl FeedbackClient {
    pub fn from_config(config: &AppConfig) -> Self {
        Self::from_config_with_session(config, None)
    }

    /// Like [`Self::from_config`] but binds `session_id` to the caller's
    /// telemetry session so that feedback and bug reports in PostHog are
    /// correlated with the matching product session summary. Pass
    /// `telemetry_client.session_id().as_deref()` at call sites that have an
    /// active `TelemetryClient`.
    pub fn from_config_with_session(config: &AppConfig, session_id: Option<&str>) -> Self {
        Self::from_config_with_install_path_and_session(
            config,
            default_install_id_path(),
            session_id,
        )
    }

    pub fn disabled() -> Self {
        Self { state: None }
    }

    pub fn from_config_with_install_path(
        config: &AppConfig,
        install_id_path: impl AsRef<Path>,
    ) -> Self {
        Self::from_config_with_install_path_and_session(config, install_id_path, None)
    }

    fn from_config_with_install_path_and_session(
        config: &AppConfig,
        install_id_path: impl AsRef<Path>,
        session_id: Option<&str>,
    ) -> Self {
        if !config.feedback.enabled {
            return Self::disabled();
        }
        let install_id = match load_or_create_install_id(install_id_path.as_ref()) {
            Ok(id) => id,
            Err(_) => return Self::disabled(),
        };
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            state: Some(Arc::new(FeedbackState {
                feedback_endpoint: config.feedback.feedback_endpoint.clone(),
                report_endpoint: config.feedback.report_endpoint.clone(),
                max_feedback_bytes: config.feedback.max_feedback_bytes,
                max_report_bytes: config.feedback.max_report_bytes,
                install_id,
                session_id: session_id
                    .map(str::to_string)
                    .unwrap_or_else(random_uuid_like),
                http,
            })),
        }
    }

    pub fn enabled(&self) -> bool {
        self.state.is_some()
    }

    pub async fn submit_feedback(
        &self,
        feedback: &PreparedFeedback,
    ) -> Result<FeedbackSubmitResult, FeedbackError> {
        let Some(state) = &self.state else {
            return Err(FeedbackError::Disabled);
        };
        if feedback.message_bytes > state.max_feedback_bytes {
            return Err(FeedbackError::TooLarge {
                bytes: feedback.message_bytes,
                max_bytes: state.max_feedback_bytes,
            });
        }
        let request = FeedbackRequest {
            schema_version: SCHEMA_VERSION,
            feedback_id: feedback.feedback_id.as_str(),
            user_id: state.install_id.as_str(),
            install_id: state.install_id.as_str(),
            session_id: state.session_id.as_str(),
            app_version: env!("CARGO_PKG_VERSION"),
            os: env::consts::OS,
            arch: env::consts::ARCH,
            source: feedback.source.as_str(),
            timestamp_ms: now_ms(),
            message: feedback.message.as_str(),
            message_bytes: feedback.message_bytes,
            redactions: feedback.redactions,
        };
        let response = state
            .http
            .post(&state.feedback_endpoint)
            .json(&request)
            .send()
            .await
            .map_err(FeedbackError::Http)?;
        parse_submit_response(response, &feedback.feedback_id).await
    }

    pub async fn submit_report(
        &self,
        report: ReportUpload<'_>,
    ) -> Result<FeedbackSubmitResult, FeedbackError> {
        let Some(state) = &self.state else {
            return Err(FeedbackError::Disabled);
        };
        if report.archive_bytes.len() > state.max_report_bytes {
            return Err(FeedbackError::TooLarge {
                bytes: report.archive_bytes.len(),
                max_bytes: state.max_report_bytes,
            });
        }
        let response = state
            .http
            .post(&state.report_endpoint)
            .header("content-type", "application/x-tar")
            .header("x-squeezy-schema-version", SCHEMA_VERSION.to_string())
            .header("x-squeezy-report-id", report.report_id)
            .header("x-squeezy-session-id", report.session_id)
            .header("x-squeezy-source", report.source)
            .header("x-squeezy-app-version", env!("CARGO_PKG_VERSION"))
            .header("x-squeezy-os", env::consts::OS)
            .header("x-squeezy-arch", env::consts::ARCH)
            .header("x-squeezy-install-id", state.install_id.as_str())
            .header("x-squeezy-user-id", state.install_id.as_str())
            .header("x-squeezy-client-session-id", state.session_id.as_str())
            .header(
                "x-squeezy-archive-bytes",
                report.archive_bytes.len().to_string(),
            )
            .header("x-squeezy-redactions", report.redactions.to_string())
            .header("x-squeezy-sections", report.sections.join(","))
            .body(report.archive_bytes.to_vec())
            .send()
            .await
            .map_err(FeedbackError::Http)?;
        parse_submit_response(response, report.report_id).await
    }
}

#[derive(Debug, Serialize)]
struct FeedbackRequest<'a> {
    schema_version: u32,
    feedback_id: &'a str,
    user_id: &'a str,
    install_id: &'a str,
    session_id: &'a str,
    app_version: &'static str,
    os: &'static str,
    arch: &'static str,
    source: &'a str,
    timestamp_ms: u128,
    message: &'a str,
    message_bytes: usize,
    redactions: u64,
}

#[derive(Debug, Deserialize)]
struct SubmitResponse {
    id: Option<String>,
    feedback_id: Option<String>,
    report_id: Option<String>,
}

pub fn prepare_feedback(
    config: &AppConfig,
    message: &str,
    source: impl Into<String>,
) -> squeezy_core::Result<PreparedFeedback> {
    if !config.feedback.enabled {
        return Err(squeezy_core::SqueezyError::Tool(
            "feedback is disabled".to_string(),
        ));
    }
    let trimmed = message.trim();
    if trimmed.is_empty() {
        return Err(squeezy_core::SqueezyError::Tool(
            "feedback message cannot be empty".to_string(),
        ));
    }
    let redactor = config.redaction.redactor()?;
    let redacted = redactor.redact(trimmed);
    let message_bytes = redacted.text.len();
    if message_bytes > config.feedback.max_feedback_bytes {
        return Err(squeezy_core::SqueezyError::Tool(format!(
            "feedback message is {message_bytes} bytes, exceeding max_feedback_bytes {}",
            config.feedback.max_feedback_bytes
        )));
    }
    Ok(PreparedFeedback {
        feedback_id: random_uuid_like(),
        message: redacted.text,
        message_bytes,
        redactions: redacted.redactions,
        source: source.into(),
    })
}

async fn parse_submit_response(
    response: reqwest::Response,
    fallback_id: &str,
) -> Result<FeedbackSubmitResult, FeedbackError> {
    if !response.status().is_success() {
        return Err(FeedbackError::Status(response.status()));
    }
    if response.status() == reqwest::StatusCode::NO_CONTENT {
        return Ok(FeedbackSubmitResult {
            id: fallback_id.to_string(),
        });
    }
    let parsed = response
        .json::<SubmitResponse>()
        .await
        .map_err(FeedbackError::Http)?;
    let id = parsed
        .id
        .or(parsed.feedback_id)
        .or(parsed.report_id)
        .unwrap_or_else(|| fallback_id.to_string());
    Ok(FeedbackSubmitResult { id })
}

#[derive(Debug)]
pub enum TelemetryError {
    Http(reqwest::Error),
    Status(reqwest::StatusCode),
    Store(String),
}

impl From<TelemetryStoreError> for TelemetryError {
    fn from(error: TelemetryStoreError) -> Self {
        Self::Store(error.to_string())
    }
}

async fn enqueue_event(state: Arc<TelemetryState>, mut event: TelemetryEvent) {
    stamp_trace_ids(&state, &mut event);
    event.event_sequence = state.next_event_sequence.fetch_add(1, Ordering::Relaxed);
    persist_local_event(&state, &event);
    let action = {
        let mut queue = state.queue.lock().await;
        queue.events.push(event);
        if queue.events.len() > MAX_LOCAL_QUEUE_EVENTS {
            let extra = queue.events.len() - MAX_LOCAL_QUEUE_EVENTS;
            queue.events.drain(0..extra);
        }
        if !queue.flush_scheduled {
            queue.flush_scheduled = true;
            TelemetryAction::Schedule
        } else {
            TelemetryAction::None
        }
    };

    match action {
        TelemetryAction::Schedule => {
            tokio::spawn(async move {
                time::sleep(FLUSH_INTERVAL).await;
                {
                    let mut queue = state.queue.lock().await;
                    queue.flush_scheduled = false;
                }
                let _ = send_pending_summaries(state).await;
            });
        }
        TelemetryAction::None => {}
    }
}

fn enqueue_event_without_runtime(state: &Arc<TelemetryState>, mut event: TelemetryEvent) {
    stamp_trace_ids(state, &mut event);
    event.event_sequence = state.next_event_sequence.fetch_add(1, Ordering::Relaxed);
    persist_local_event(state, &event);
    let mut queue = state.queue.blocking_lock();
    queue.events.push(event);
    if queue.events.len() > MAX_LOCAL_QUEUE_EVENTS {
        let extra = queue.events.len() - MAX_LOCAL_QUEUE_EVENTS;
        queue.events.drain(0..extra);
    }
}

#[derive(Debug)]
enum TelemetryAction {
    Schedule,
    None,
}

async fn drain_queued_events(state: &TelemetryState) -> Vec<TelemetryEvent> {
    let mut queue = state.queue.lock().await;
    queue.flush_scheduled = false;
    drain_event_buffer(&mut queue.events)
}

fn drain_event_buffer(events: &mut Vec<TelemetryEvent>) -> Vec<TelemetryEvent> {
    let mut drained = Vec::with_capacity(MAX_BATCH_EVENTS);
    std::mem::swap(events, &mut drained);
    drained
}

fn persist_local_event(state: &TelemetryState, event: &TelemetryEvent) {
    let Some(store) = state.store.as_ref() else {
        return;
    };
    let registered = state
        .session_registered
        .lock()
        .map(|guard| *guard)
        .unwrap_or(false);
    if !registered
        && store
            .mark_session_started(
                &state.session_id,
                &state.trace_id,
                state.session_started_at_ms,
            )
            .is_ok()
        && let Ok(mut guard) = state.session_registered.lock()
    {
        *guard = true;
    }
    if store.append_event(&state.session_id, event).is_err() {
        return;
    }
    if event.event == TelemetryEventName::SessionEnded {
        let _ = store.mark_session_ended(&state.session_id, event.timestamp_ms);
    }
}

fn schedule_pending_retry(state: Arc<TelemetryState>) {
    if state.store.is_none() {
        return;
    }
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            let _ = send_pending_summaries(state).await;
        });
    }
}

async fn send_pending_summaries(state: Arc<TelemetryState>) -> Result<(), TelemetryError> {
    let Some(store) = state.store.as_ref() else {
        return Ok(());
    };
    // Serialise concurrent callers (startup-retry, periodic flush, exit flush)
    // so that the read-then-write lease acquisition in `lease_due_summaries`
    // cannot hand the same summary to two senders simultaneously.
    let _flush_guard = state.flush_lock.lock().await;
    let pending = store.lease_due_summaries(now_ms(), PENDING_SEND_LIMIT, PENDING_LEASE_MS)?;
    let mut first_error: Option<TelemetryError> = None;
    for summary in pending {
        let result = send_batch_for_session(
            state.clone(),
            summary.source_session_id.clone(),
            vec![summary.event.clone()],
        )
        .await;
        match result {
            Ok(()) => {
                let _ = store.mark_summary_sent(&summary);
            }
            Err(error) => {
                let _ = store.mark_summary_failed(&summary.summary_id, now_ms());
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

async fn send_batch_for_session(
    state: Arc<TelemetryState>,
    session_id: String,
    mut events: Vec<TelemetryEvent>,
) -> Result<(), TelemetryError> {
    if events.is_empty() {
        return Ok(());
    }
    events.truncate(MAX_BATCH_EVENTS);
    for event in &mut events {
        event.event_sequence = state.next_event_sequence.fetch_add(1, Ordering::Relaxed);
    }
    let batch = TelemetryBatch {
        schema_version: SCHEMA_VERSION,
        user_id: state.install_id.as_str(),
        install_id: state.install_id.as_str(),
        session_id: session_id.as_str(),
        app_version: env!("CARGO_PKG_VERSION"),
        os: env::consts::OS,
        arch: env::consts::ARCH,
        events,
    };
    let response = state
        .http
        .post(&state.endpoint)
        .json(&batch)
        .send()
        .await
        .map_err(TelemetryError::Http)?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(TelemetryError::Status(response.status()))
    }
}

#[derive(Debug, Serialize)]
struct TelemetryBatch<'a> {
    schema_version: u32,
    user_id: &'a str,
    install_id: &'a str,
    session_id: &'a str,
    app_version: &'static str,
    os: &'static str,
    arch: &'static str,
    events: Vec<TelemetryEvent>,
}

#[derive(Debug)]
struct TelemetryStore {
    _path: PathBuf,
    database: Database,
}

#[derive(Debug)]
enum TelemetryStoreError {
    Io(std::io::Error),
    Redb(redb::Error),
    RedbDatabase(redb::DatabaseError),
    RedbTable(redb::TableError),
    RedbTransaction(redb::TransactionError),
    RedbStorage(redb::StorageError),
    RedbCommit(redb::CommitError),
    Serde(serde_json::Error),
}

impl std::fmt::Display for TelemetryStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "telemetry store I/O failed: {error}"),
            Self::Redb(error) => write!(f, "telemetry store open failed: {error}"),
            Self::RedbDatabase(error) => write!(f, "telemetry store database failed: {error}"),
            Self::RedbTable(error) => write!(f, "telemetry store table failed: {error}"),
            Self::RedbTransaction(error) => {
                write!(f, "telemetry store transaction failed: {error}")
            }
            Self::RedbStorage(error) => write!(f, "telemetry store storage failed: {error}"),
            Self::RedbCommit(error) => write!(f, "telemetry store commit failed: {error}"),
            Self::Serde(error) => write!(f, "telemetry store JSON failed: {error}"),
        }
    }
}

impl std::error::Error for TelemetryStoreError {}

impl From<std::io::Error> for TelemetryStoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<redb::Error> for TelemetryStoreError {
    fn from(error: redb::Error) -> Self {
        Self::Redb(error)
    }
}

impl From<redb::DatabaseError> for TelemetryStoreError {
    fn from(error: redb::DatabaseError) -> Self {
        Self::RedbDatabase(error)
    }
}

impl From<redb::TableError> for TelemetryStoreError {
    fn from(error: redb::TableError) -> Self {
        Self::RedbTable(error)
    }
}

impl From<redb::TransactionError> for TelemetryStoreError {
    fn from(error: redb::TransactionError) -> Self {
        Self::RedbTransaction(error)
    }
}

impl From<redb::StorageError> for TelemetryStoreError {
    fn from(error: redb::StorageError) -> Self {
        Self::RedbStorage(error)
    }
}

impl From<redb::CommitError> for TelemetryStoreError {
    fn from(error: redb::CommitError) -> Self {
        Self::RedbCommit(error)
    }
}

impl From<serde_json::Error> for TelemetryStoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Serde(error)
    }
}

type StoreResult<T> = std::result::Result<T, TelemetryStoreError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredTelemetrySession {
    session_id: String,
    trace_id: String,
    started_at_ms: u128,
    ended_at_ms: Option<u128>,
    clean_end: bool,
    summary_id: Option<String>,
}

impl StoredTelemetrySession {
    fn from_state(state: &TelemetryState) -> Self {
        Self {
            session_id: state.session_id.clone(),
            trace_id: state.trace_id.clone(),
            started_at_ms: state.session_started_at_ms,
            ended_at_ms: None,
            clean_end: false,
            summary_id: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredTelemetryEvent {
    session_id: String,
    sequence: u64,
    event: TelemetryEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingTelemetrySummary {
    summary_id: String,
    source_session_id: String,
    event: TelemetryEvent,
    attempts: u32,
    next_attempt_ms: u128,
    leased_until_ms: u128,
}

impl TelemetryStore {
    fn open(path: PathBuf) -> StoreResult<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let database = Database::create(&path)?;
        let store = Self {
            _path: path,
            database,
        };
        store.initialize_schema()?;
        Ok(store)
    }

    fn initialize_schema(&self) -> StoreResult<()> {
        let write = self.database.begin_write()?;
        {
            let mut meta = write.open_table(STORE_META)?;
            insert_store_json(&mut meta, "schema_version", &TELEMETRY_STORE_SCHEMA_VERSION)?;
        }
        write.open_table(STORE_SESSIONS)?;
        write.open_table(STORE_EVENTS)?;
        write.open_table(STORE_PENDING)?;
        write.commit()?;
        Ok(())
    }

    fn mark_session_started(
        &self,
        session_id: &str,
        trace_id: &str,
        started_at_ms: u128,
    ) -> StoreResult<()> {
        let write = self.database.begin_write()?;
        {
            let mut sessions = write.open_table(STORE_SESSIONS)?;
            if sessions.get(session_id)?.is_none() {
                let session = StoredTelemetrySession {
                    session_id: session_id.to_string(),
                    trace_id: trace_id.to_string(),
                    started_at_ms,
                    ended_at_ms: None,
                    clean_end: false,
                    summary_id: None,
                };
                insert_store_json(&mut sessions, session_id, &session)?;
            }
        }
        write.commit()?;
        Ok(())
    }

    fn mark_session_ended(&self, session_id: &str, ended_at_ms: u128) -> StoreResult<()> {
        let Some(mut session) = self.session(session_id)? else {
            return Ok(());
        };
        session.ended_at_ms = Some(ended_at_ms);
        session.clean_end = true;
        self.put_session(&session)
    }

    fn append_event(&self, session_id: &str, event: &TelemetryEvent) -> StoreResult<()> {
        let write = self.database.begin_write()?;
        {
            let key = event_key(session_id, event.event_sequence);
            let stored = StoredTelemetryEvent {
                session_id: session_id.to_string(),
                sequence: event.event_sequence,
                event: event.clone(),
            };
            let mut events = write.open_table(STORE_EVENTS)?;
            insert_store_json(&mut events, &key, &stored)?;
        }
        write.commit()?;
        Ok(())
    }

    fn finalize_session_summary(
        &self,
        session_id: &str,
        abnormal: bool,
    ) -> StoreResult<Option<String>> {
        let Some(mut session) = self.session(session_id)? else {
            return Ok(None);
        };
        if let Some(summary_id) = session.summary_id.clone() {
            return Ok(Some(summary_id));
        }
        let events = self.session_events(session_id)?;
        if events.is_empty() {
            return Ok(None);
        }
        let summary_id = random_uuid_like();
        let summary = build_summary_from_events(&session, events, abnormal, None);
        let pending = PendingTelemetrySummary {
            summary_id: summary_id.clone(),
            source_session_id: session_id.to_string(),
            event: with_summary_id(summary, &summary_id),
            attempts: 0,
            next_attempt_ms: 0,
            leased_until_ms: 0,
        };
        session.summary_id = Some(summary_id.clone());
        self.put_pending_and_session(&pending, &session)?;
        Ok(Some(summary_id))
    }

    fn synthesize_abnormal_sessions(&self, current_session_id: &str) -> StoreResult<()> {
        for session in self.sessions()? {
            if session.session_id == current_session_id
                || session.clean_end
                || session.summary_id.is_some()
            {
                continue;
            }
            let _ = self.finalize_session_summary(&session.session_id, true)?;
        }
        Ok(())
    }

    fn lease_due_summaries(
        &self,
        now_ms: u128,
        limit: usize,
        lease_ms: u128,
    ) -> StoreResult<Vec<PendingTelemetrySummary>> {
        let mut due = Vec::new();
        {
            let read = self.database.begin_read()?;
            let table = match read.open_table(STORE_PENDING) {
                Ok(table) => table,
                Err(_) => return Ok(Vec::new()),
            };
            for entry in table.iter()? {
                let (_, value) = entry?;
                let pending: PendingTelemetrySummary = serde_json::from_slice(value.value())?;
                if pending.next_attempt_ms <= now_ms && pending.leased_until_ms <= now_ms {
                    due.push(pending);
                    if due.len() >= limit {
                        break;
                    }
                }
            }
        }
        if due.is_empty() {
            return Ok(due);
        }
        let write = self.database.begin_write()?;
        {
            let mut table = write.open_table(STORE_PENDING)?;
            for pending in &mut due {
                pending.attempts = pending.attempts.saturating_add(1);
                pending.leased_until_ms = now_ms.saturating_add(lease_ms);
                insert_store_json(&mut table, &pending.summary_id, pending)?;
            }
        }
        write.commit()?;
        Ok(due)
    }

    fn mark_summary_failed(&self, summary_id: &str, now_ms: u128) -> StoreResult<()> {
        let Some(mut pending) = self.pending_summary(summary_id)? else {
            return Ok(());
        };
        pending.leased_until_ms = 0;
        pending.next_attempt_ms = now_ms.saturating_add(retry_delay_ms(pending.attempts));
        let write = self.database.begin_write()?;
        {
            let mut table = write.open_table(STORE_PENDING)?;
            insert_store_json(&mut table, summary_id, &pending)?;
        }
        write.commit()?;
        Ok(())
    }

    fn mark_summary_sent(&self, pending: &PendingTelemetrySummary) -> StoreResult<()> {
        let event_keys = self.event_keys_for_session(&pending.source_session_id)?;
        let write = self.database.begin_write()?;
        {
            let mut pending_table = write.open_table(STORE_PENDING)?;
            pending_table.remove(pending.summary_id.as_str())?;
        }
        {
            let mut events = write.open_table(STORE_EVENTS)?;
            for key in &event_keys {
                events.remove(key.as_str())?;
            }
        }
        {
            let mut sessions = write.open_table(STORE_SESSIONS)?;
            sessions.remove(pending.source_session_id.as_str())?;
        }
        write.commit()?;
        Ok(())
    }

    fn session(&self, session_id: &str) -> StoreResult<Option<StoredTelemetrySession>> {
        let read = self.database.begin_read()?;
        let table = match read.open_table(STORE_SESSIONS) {
            Ok(table) => table,
            Err(_) => return Ok(None),
        };
        read_store_json(&table, session_id)
    }

    fn sessions(&self) -> StoreResult<Vec<StoredTelemetrySession>> {
        let read = self.database.begin_read()?;
        let table = match read.open_table(STORE_SESSIONS) {
            Ok(table) => table,
            Err(_) => return Ok(Vec::new()),
        };
        let mut sessions = Vec::new();
        for entry in table.iter()? {
            let (_, value) = entry?;
            sessions.push(serde_json::from_slice(value.value())?);
        }
        Ok(sessions)
    }

    fn put_session(&self, session: &StoredTelemetrySession) -> StoreResult<()> {
        let write = self.database.begin_write()?;
        {
            let mut sessions = write.open_table(STORE_SESSIONS)?;
            insert_store_json(&mut sessions, &session.session_id, session)?;
        }
        write.commit()?;
        Ok(())
    }

    fn put_pending_and_session(
        &self,
        pending: &PendingTelemetrySummary,
        session: &StoredTelemetrySession,
    ) -> StoreResult<()> {
        let write = self.database.begin_write()?;
        {
            let mut pending_table = write.open_table(STORE_PENDING)?;
            insert_store_json(&mut pending_table, &pending.summary_id, pending)?;
        }
        {
            let mut sessions = write.open_table(STORE_SESSIONS)?;
            insert_store_json(&mut sessions, &session.session_id, session)?;
        }
        write.commit()?;
        Ok(())
    }

    fn pending_summary(&self, summary_id: &str) -> StoreResult<Option<PendingTelemetrySummary>> {
        let read = self.database.begin_read()?;
        let table = match read.open_table(STORE_PENDING) {
            Ok(table) => table,
            Err(_) => return Ok(None),
        };
        read_store_json(&table, summary_id)
    }

    fn session_events(&self, session_id: &str) -> StoreResult<Vec<TelemetryEvent>> {
        let read = self.database.begin_read()?;
        let table = match read.open_table(STORE_EVENTS) {
            Ok(table) => table,
            Err(_) => return Ok(Vec::new()),
        };
        let prefix = format!("{session_id}:");
        let mut events = Vec::new();
        for entry in table.iter()? {
            let (key, value) = entry?;
            if key.value().starts_with(&prefix) {
                let stored: StoredTelemetryEvent = serde_json::from_slice(value.value())?;
                events.push(stored.event);
            }
        }
        events.sort_by_key(|event| (event.timestamp_ms, event.event_sequence));
        Ok(events)
    }

    fn event_keys_for_session(&self, session_id: &str) -> StoreResult<Vec<String>> {
        let read = self.database.begin_read()?;
        let table = match read.open_table(STORE_EVENTS) {
            Ok(table) => table,
            Err(_) => return Ok(Vec::new()),
        };
        let prefix = format!("{session_id}:");
        let mut keys = Vec::new();
        for entry in table.iter()? {
            let (key, _) = entry?;
            if key.value().starts_with(&prefix) {
                keys.push(key.value().to_string());
            }
        }
        Ok(keys)
    }
}

fn insert_store_json<T: Serialize>(
    table: &mut redb::Table<'_, &str, &[u8]>,
    key: &str,
    value: &T,
) -> StoreResult<()> {
    let encoded = serde_json::to_vec(value)?;
    table.insert(key, encoded.as_slice())?;
    Ok(())
}

fn read_store_json<T: for<'de> Deserialize<'de>>(
    table: &redb::ReadOnlyTable<&str, &[u8]>,
    key: &str,
) -> StoreResult<Option<T>> {
    let Some(value) = table.get(key)? else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_slice(value.value())?))
}

fn event_key(session_id: &str, sequence: u64) -> String {
    format!("{session_id}:{sequence:020}")
}

fn retry_delay_ms(attempts: u32) -> u128 {
    match attempts {
        0 | 1 => 5_000,
        2 => 30_000,
        3 => 5 * 60_000,
        4 => 60 * 60_000,
        5 => 6 * 60 * 60_000,
        _ => 24 * 60 * 60_000,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelemetryEvent {
    pub event: TelemetryEventName,
    pub timestamp_ms: u128,
    pub event_sequence: u64,
    pub properties: TelemetryProperties,
}

impl TelemetryEvent {
    pub fn app_started(config: &AppConfig) -> Self {
        Self {
            event: TelemetryEventName::AppStarted,
            timestamp_ms: now_ms(),
            event_sequence: 0,
            properties: TelemetryProperties {
                provider: Some(ProviderKind::from_provider(&config.provider)),
                model_family: Some(ModelFamily::from_model(&config.provider, &config.model)),
                ..TelemetryProperties::default()
            },
        }
    }

    pub fn turn_completed(config: &AppConfig, turn_index: u64, metrics: TurnMetrics) -> Self {
        Self {
            event: TelemetryEventName::TurnCompleted,
            timestamp_ms: now_ms(),
            event_sequence: 0,
            properties: TelemetryProperties::from_turn(config, turn_index, &metrics),
        }
    }

    pub fn tool_completed(report: ToolTelemetryReport<'_>) -> Self {
        Self {
            event: TelemetryEventName::ToolCompleted,
            timestamp_ms: now_ms(),
            event_sequence: 0,
            properties: TelemetryProperties {
                provider: Some(ProviderKind::from_provider(report.provider)),
                model_family: Some(ModelFamily::from_model(report.provider, report.model)),
                turn_index: Some(report.turn_index),
                tool_sequence: Some(report.tool_sequence),
                tool_name: Some(FirstPartyToolName::from_tool_name(report.tool_name)),
                tool_family: Some(ToolFamily::from_tool_name(report.tool_name)),
                tool_status: Some(report.status),
                duration_ms: Some(report.duration.as_millis() as u64),
                files_scanned: Some(report.cost.files_scanned),
                bytes_read: Some(report.cost.bytes_read),
                output_bytes: Some(report.cost.output_bytes),
                matches_returned: Some(report.cost.matches_returned),
                ..TelemetryProperties::default()
            },
        }
    }

    pub fn graph_build_completed(report: GraphPerfReport) -> Self {
        Self {
            event: TelemetryEventName::GraphBuildCompleted,
            timestamp_ms: now_ms(),
            event_sequence: 0,
            properties: TelemetryProperties::from_graph(report),
        }
    }

    pub fn graph_refresh_completed(report: GraphPerfReport) -> Self {
        Self {
            event: TelemetryEventName::GraphRefreshCompleted,
            timestamp_ms: now_ms(),
            event_sequence: 0,
            properties: TelemetryProperties::from_graph(report),
        }
    }

    pub fn startup_ready(config: &AppConfig, route: StartupRoute, duration: Duration) -> Self {
        // Read per-phase timings recorded by startup_trace::mark() in-memory.
        let placeholder_ms = squeezy_core::startup_trace::elapsed_ms_for("tui_placeholder_drawn");
        let agent_build_ms = squeezy_core::startup_trace::elapsed_ms_for("agent_built");
        let snapshot_ms = squeezy_core::startup_trace::elapsed_ms_for("snapshots_done");
        Self {
            event: TelemetryEventName::StartupReady,
            timestamp_ms: now_ms(),
            event_sequence: 0,
            properties: TelemetryProperties {
                provider: Some(ProviderKind::from_provider(&config.provider)),
                model_family: Some(ModelFamily::from_model(&config.provider, &config.model)),
                duration_ms: Some(duration.as_millis() as u64),
                startup_route: Some(route),
                status: Some(OutcomeStatus::Success),
                startup_placeholder_ms: placeholder_ms,
                startup_agent_build_ms: agent_build_ms,
                startup_snapshot_ms: snapshot_ms,
                ..TelemetryProperties::default()
            },
        }
    }

    pub fn session_ended(config: &AppConfig, report: SessionTelemetryReport) -> Self {
        let subagent_counts = non_empty_map(report.subagent_kind_counts);
        let subagent_cap_rejections = if report.subagent_cap_rejections > 0 {
            Some(report.subagent_cap_rejections)
        } else {
            None
        };
        Self {
            event: TelemetryEventName::SessionEnded,
            timestamp_ms: now_ms(),
            event_sequence: 0,
            properties: TelemetryProperties {
                provider: Some(ProviderKind::from_provider(&config.provider)),
                model_family: Some(ModelFamily::from_model(&config.provider, &config.model)),
                duration_ms: Some(report.duration_ms),
                session_status: Some(report.status),
                store_session_id: report.store_session_id,
                turn_count: Some(report.turns),
                tool_calls: Some(report.tool_calls),
                tool_successes: Some(report.tool_successes),
                tool_errors: Some(report.tool_errors),
                tool_denials: Some(report.tool_denials),
                tool_cancellations: Some(report.tool_cancellations),
                budget_denials: Some(report.budget_denials),
                subagent_calls: Some(report.subagent_calls),
                subagent_failures: Some(report.subagent_failures),
                subagent_counts,
                subagent_cap_rejections,
                ..TelemetryProperties::default()
            },
        }
    }

    pub fn slash_command_used(report: SlashTelemetryReport<'_>) -> Self {
        Self {
            event: TelemetryEventName::SlashCommandUsed,
            timestamp_ms: now_ms(),
            event_sequence: 0,
            properties: TelemetryProperties {
                slash_command: Some(slash_command_token(report.command)),
                slash_surface: Some(report.surface),
                slash_outcome: Some(report.outcome),
                slash_alias_kind: Some(report.alias_kind),
                slash_arg_shape: Some(report.arg_shape),
                ..TelemetryProperties::default()
            },
        }
    }

    pub fn config_change_committed(report: ConfigChangeReport<'_>) -> Self {
        Self {
            event: TelemetryEventName::ConfigChangeCommitted,
            timestamp_ms: now_ms(),
            event_sequence: 0,
            properties: TelemetryProperties {
                config_scope: Some(report.scope),
                config_section: Some(report.section.to_string()),
                config_field: Some(report.field.to_string()),
                config_apply_tier: Some(report.apply_tier),
                config_change_kind: Some(report.change_kind),
                config_prev_bucket: Some(report.prev_bucket.to_string()),
                config_new_bucket: Some(report.new_bucket.to_string()),
                ..TelemetryProperties::default()
            },
        }
    }

    pub fn failure_seen(kind: ErrorKind) -> Self {
        Self {
            event: TelemetryEventName::FailureSeen,
            timestamp_ms: now_ms(),
            event_sequence: 0,
            properties: TelemetryProperties {
                error_kind: Some(kind),
                ..TelemetryProperties::default()
            },
        }
    }

    /// `approval.best_effort.fallback{tool=shell}` counter event. The
    /// shell tool fires this every time the configured OS sandbox
    /// backend probe/runtime check fails and the call retries without
    /// isolation under `mode = "best_effort"`. The TUI surfaces a
    /// separate one-shot warning to the user; this event lets backend
    /// dashboards count silent degradations across the fleet.
    pub fn shell_sandbox_best_effort_fallback(backend: &str) -> Self {
        Self {
            event: TelemetryEventName::ShellSandboxBestEffortFallback,
            timestamp_ms: now_ms(),
            event_sequence: 0,
            properties: TelemetryProperties {
                tool_name: Some(FirstPartyToolName::Shell),
                tool_family: Some(ToolFamily::Shell),
                sandbox_backend: Some(backend.to_string()),
                ..TelemetryProperties::default()
            },
        }
    }

    /// `shell.windows_degraded{backend}` counter event. Fires once per session
    /// on Windows when the first shell result carries `windows-job-object` or
    /// `best_effort_unavailable` filesystem isolation. Separable from
    /// `shell_sandbox_best_effort_fallback` (which counts Unix runtime sandbox
    /// failures) so Windows shell backend degradation is independently
    /// filterable in dashboards.
    pub fn shell_windows_degraded(backend: &str) -> Self {
        Self {
            event: TelemetryEventName::ShellWindowsDegraded,
            timestamp_ms: now_ms(),
            event_sequence: 0,
            properties: TelemetryProperties {
                tool_name: Some(FirstPartyToolName::Shell),
                tool_family: Some(ToolFamily::Shell),
                sandbox_backend: Some(backend.to_string()),
                ..TelemetryProperties::default()
            },
        }
    }

    /// `ai_reviewer.allow_downgrade{capability}` counter event. Fires when
    /// the AI reviewer model returned `allow` but the requested capability
    /// is not in the operator's `allow_capabilities` allowlist, so the
    /// reviewer silently downgrades the decision to "no decision" and falls
    /// back to the user prompt. Without this counter, operators cannot tell
    /// how often the reviewer would have approved if the allowlist were
    /// wider, so the allowlist cannot evolve.
    pub fn ai_reviewer_allow_downgrade(capability: &str) -> Self {
        Self {
            event: TelemetryEventName::AiReviewerAllowDowngrade,
            timestamp_ms: now_ms(),
            event_sequence: 0,
            properties: TelemetryProperties {
                permission_capability: Some(capability.to_string()),
                ..TelemetryProperties::default()
            },
        }
    }

    /// `routing.routed{reason}` counter event. Fires once per turn
    /// when the per-turn model router dispatches on the provider's
    /// cheap tier instead of the user's configured parent model. The
    /// `reason` payload is the same short token surfaced on
    /// `AgentEvent::TurnRouted` — the matched heuristic verb (e.g.
    /// `"run"`, `"checkout"`), `"llm_judge"`, or `"user_explicit"`.
    /// Aggregated over time this lets us tune the heuristic whitelist:
    /// rare verbs can be pruned, judge-only routes can be promoted to
    /// the whitelist once their false-positive rate is known.
    pub fn routing_routed(reason: &str) -> Self {
        Self {
            event: TelemetryEventName::RoutingRouted,
            timestamp_ms: now_ms(),
            event_sequence: 0,
            properties: TelemetryProperties {
                routing_reason: Some(reason.to_string()),
                ..TelemetryProperties::default()
            },
        }
    }

    /// `routing.escalated{reason}` counter event. Fires when a
    /// cheap-routed turn hits an escalation signal and the agent
    /// swaps back to the parent model. `reason` is one of the
    /// `EscalationReason` short tokens: `"tool_call_ceiling"`,
    /// `"error_threshold"`, or `"refusal_phrase"`. Escalation rate is
    /// the central reliability metric for the router — high rates on
    /// a single reason point to a threshold that needs tuning;
    /// uniform low rates mean the heuristic + judge are calibrated.
    pub fn routing_escalated(reason: &str) -> Self {
        Self {
            event: TelemetryEventName::RoutingEscalated,
            timestamp_ms: now_ms(),
            event_sequence: 0,
            properties: TelemetryProperties {
                routing_reason: Some(reason.to_string()),
                ..TelemetryProperties::default()
            },
        }
    }

    pub fn mcp_discovery(report: McpDiscoveryReport) -> Self {
        let mut mcp_counts = BTreeMap::new();
        if report.servers_stdio > 0 {
            *mcp_counts.entry("transport_stdio".to_string()).or_default() +=
                u64::from(report.servers_stdio);
        }
        if report.servers_http > 0 {
            *mcp_counts.entry("transport_http".to_string()).or_default() +=
                u64::from(report.servers_http);
        }
        if report.servers_sse > 0 {
            *mcp_counts.entry("transport_sse".to_string()).or_default() +=
                u64::from(report.servers_sse);
        }
        if report.servers_enabled > 0 {
            *mcp_counts.entry("server_enabled".to_string()).or_default() +=
                u64::from(report.servers_enabled);
        }
        if report.servers_disabled > 0 {
            *mcp_counts.entry("server_disabled".to_string()).or_default() +=
                u64::from(report.servers_disabled);
        }
        if report.tools_discovered > 0 {
            *mcp_counts
                .entry("tools_discovered".to_string())
                .or_default() += u64::from(report.tools_discovered);
        }
        if report.tools_cached > 0 {
            *mcp_counts.entry("tools_cached".to_string()).or_default() +=
                u64::from(report.tools_cached);
        }
        if report.tools_stale_retained > 0 {
            *mcp_counts
                .entry("tools_stale_retained".to_string())
                .or_default() += u64::from(report.tools_stale_retained);
        }
        if report.tools_dropped_disabled > 0 {
            *mcp_counts
                .entry("tools_dropped_disabled".to_string())
                .or_default() += u64::from(report.tools_dropped_disabled);
        }
        if report.discovery_errors > 0 {
            *mcp_counts
                .entry("discovery_errors".to_string())
                .or_default() += u64::from(report.discovery_errors);
        }
        for (kind, count) in &report.error_kind_counts {
            let key = format!("error_kind_{kind}");
            *mcp_counts.entry(key).or_default() += count;
        }
        if report.has_resources {
            *mcp_counts.entry("cap_resources".to_string()).or_default() += 1;
        }
        if report.has_elicitation {
            *mcp_counts.entry("cap_elicitation".to_string()).or_default() += 1;
        }
        if report.has_experimental {
            *mcp_counts
                .entry("cap_experimental".to_string())
                .or_default() += 1;
        }
        Self {
            event: TelemetryEventName::McpDiscovery,
            timestamp_ms: now_ms(),
            event_sequence: 0,
            properties: TelemetryProperties {
                mcp_counts: non_empty_map(mcp_counts),
                duration_ms: Some(report.duration_ms),
                ..TelemetryProperties::default()
            },
        }
    }

    pub fn mcp_elicitation(kind: &str, policy: &str, outcome: &str) -> Self {
        let key = format!("elicitation_{kind}_{policy}_{outcome}");
        let mut mcp_counts = BTreeMap::new();
        *mcp_counts.entry(key).or_default() += 1u64;
        Self {
            event: TelemetryEventName::McpElicitation,
            timestamp_ms: now_ms(),
            event_sequence: 0,
            properties: TelemetryProperties {
                mcp_counts: Some(mcp_counts),
                ..TelemetryProperties::default()
            },
        }
    }

    pub fn web_request(report: WebRequestReport) -> Self {
        let key = format!(
            "provider_{}_status_{}_bytes_{}",
            report.provider_token, report.status_token, report.response_byte_bucket
        );
        let mut external_counts = BTreeMap::new();
        *external_counts.entry(key).or_default() += 1u64;
        if report.ssrf_blocked {
            *external_counts
                .entry("ssrf_blocked".to_string())
                .or_default() += 1;
        }
        if report.redirect_blocked {
            *external_counts
                .entry("redirect_blocked".to_string())
                .or_default() += 1;
        }
        Self {
            event: TelemetryEventName::WebRequest,
            timestamp_ms: now_ms(),
            event_sequence: 0,
            properties: TelemetryProperties {
                external_counts: non_empty_map(external_counts),
                duration_ms: Some(report.duration_ms),
                ..TelemetryProperties::default()
            },
        }
    }

    pub fn skill_activated(report: SkillActivationReport) -> Self {
        let mut skill_counts = BTreeMap::new();
        if report.total > 0 {
            *skill_counts.entry("activated".to_string()).or_default() += u64::from(report.total);
        }
        if report.included > 0 {
            *skill_counts.entry("included".to_string()).or_default() += u64::from(report.included);
        }
        if report.dropped > 0 {
            *skill_counts.entry("dropped".to_string()).or_default() += u64::from(report.dropped);
        }
        if report.body_truncated > 0 {
            *skill_counts
                .entry("body_truncated".to_string())
                .or_default() += u64::from(report.body_truncated);
        }
        if report.preamble_omitted_count > 0 {
            *skill_counts
                .entry("preamble_omitted".to_string())
                .or_default() += u64::from(report.preamble_omitted_count);
        }
        if report.explicit_count > 0 {
            *skill_counts
                .entry("activation_explicit".to_string())
                .or_default() += u64::from(report.explicit_count);
        }
        if report.trigger_count > 0 {
            *skill_counts
                .entry("activation_trigger".to_string())
                .or_default() += u64::from(report.trigger_count);
        }
        if report.implicit_shell_count > 0 {
            *skill_counts
                .entry("activation_implicit_shell".to_string())
                .or_default() += u64::from(report.implicit_shell_count);
        }
        for (source, count) in &report.source_counts {
            let key = format!("source_{source}");
            *skill_counts.entry(key).or_default() += count;
        }
        Self {
            event: TelemetryEventName::SkillActivated,
            timestamp_ms: now_ms(),
            event_sequence: 0,
            properties: TelemetryProperties {
                skill_counts: non_empty_map(skill_counts),
                ..TelemetryProperties::default()
            },
        }
    }

    pub fn prompt_template_expanded(source: &str, arg_count: u32, queued: bool) -> Self {
        let arg_bucket = match arg_count {
            0 => "args_0",
            1..=5 => "args_1_5",
            6..=20 => "args_6_20",
            _ => "args_many",
        };
        let state = if queued { "queued" } else { "started" };
        let key = format!("source_{source}_{state}_{arg_bucket}");
        let mut prompt_template_counts = BTreeMap::new();
        *prompt_template_counts.entry(key).or_default() += 1u64;
        Self {
            event: TelemetryEventName::PromptTemplateExpanded,
            timestamp_ms: now_ms(),
            event_sequence: 0,
            properties: TelemetryProperties {
                prompt_template_counts: Some(prompt_template_counts),
                ..TelemetryProperties::default()
            },
        }
    }

    /// Approval or permission decision event. `capability`, `risk_bucket`, `decision`,
    /// and `source` are short safe tokens derived from enum `as_str()` methods.
    /// Never includes targets, reasons, rule patterns, or any user data.
    pub fn approval_decided(
        capability: &str,
        risk_bucket: &str,
        decision: &str,
        source: &str,
    ) -> Self {
        let key = format!("{capability}_{risk_bucket}_{decision}_{source}");
        let mut approval_counts = BTreeMap::new();
        *approval_counts.entry(key).or_default() += 1u64;
        Self {
            event: TelemetryEventName::ApprovalDecided,
            timestamp_ms: now_ms(),
            event_sequence: 0,
            properties: TelemetryProperties {
                approval_counts: Some(approval_counts),
                ..TelemetryProperties::default()
            },
        }
    }

    /// Automatic policy-evaluated permission decision. `capability`, `action`, and
    /// `source` are short safe tokens. Never includes targets or rule patterns.
    pub fn permission_decided(capability: &str, action: &str, source: &str) -> Self {
        let key = format!("{capability}_{action}_{source}");
        let mut permission_counts = BTreeMap::new();
        *permission_counts.entry(key).or_default() += 1u64;
        Self {
            event: TelemetryEventName::PermissionDecided,
            timestamp_ms: now_ms(),
            event_sequence: 0,
            properties: TelemetryProperties {
                permission_counts: Some(permission_counts),
                ..TelemetryProperties::default()
            },
        }
    }

    pub fn provider_retry(reason: RetryReasonKind) -> Self {
        let key = reason.as_str().to_string();
        let mut retry_counts = BTreeMap::new();
        *retry_counts.entry(key).or_default() += 1u64;
        Self {
            event: TelemetryEventName::ProviderRetry,
            timestamp_ms: now_ms(),
            event_sequence: 0,
            properties: TelemetryProperties {
                retry_counts: Some(retry_counts),
                ..TelemetryProperties::default()
            },
        }
    }

    pub fn provider_error(kind: ProviderErrorKind) -> Self {
        let key = kind.as_str().to_string();
        let mut provider_error_counts = BTreeMap::new();
        *provider_error_counts.entry(key).or_default() += 1u64;
        Self {
            event: TelemetryEventName::ProviderError,
            timestamp_ms: now_ms(),
            event_sequence: 0,
            properties: TelemetryProperties {
                provider_error_counts: Some(provider_error_counts),
                ..TelemetryProperties::default()
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TelemetryEventName {
    #[serde(rename = "squeezy_session_summary")]
    SessionSummary,
    #[serde(rename = "squeezy_app_started")]
    AppStarted,
    #[serde(rename = "squeezy_turn_completed")]
    TurnCompleted,
    #[serde(rename = "squeezy_tool_completed")]
    ToolCompleted,
    #[serde(rename = "squeezy_graph_build_completed")]
    GraphBuildCompleted,
    #[serde(rename = "squeezy_graph_refresh_completed")]
    GraphRefreshCompleted,
    #[serde(rename = "squeezy_startup_ready")]
    StartupReady,
    #[serde(rename = "squeezy_session_ended")]
    SessionEnded,
    #[serde(rename = "squeezy_slash_command_used")]
    SlashCommandUsed,
    #[serde(rename = "squeezy_config_change_committed")]
    ConfigChangeCommitted,
    #[serde(rename = "squeezy_failure_seen")]
    FailureSeen,
    /// `approval.best_effort.fallback{tool=shell}` — emitted every time
    /// the shell tool's OS sandbox backend probe or runtime check fails
    /// and the call retries without isolation under
    /// `mode = "best_effort"`. Mirrors the codex `policy_transforms`
    /// telemetry counter so the silent degradation surfaces in
    /// dashboards rather than only in audit logs.
    #[serde(rename = "approval_best_effort_fallback")]
    ShellSandboxBestEffortFallback,
    /// `shell.windows_degraded{backend}` — Windows steady-state sandbox
    /// posture. Separable from `ShellSandboxBestEffortFallback` so
    /// Windows shell runs can be filtered independently in dashboards.
    #[serde(rename = "shell_windows_degraded")]
    ShellWindowsDegraded,
    /// `ai_reviewer.allow_downgrade{capability}` — emitted when the AI
    /// reviewer would have approved but the capability was not in the
    /// operator's `allow_capabilities` allowlist, so the verdict silently
    /// fell back to the user prompt. Lets operators see how often a wider
    /// allowlist would have spared an interruption.
    #[serde(rename = "ai_reviewer_allow_downgrade")]
    AiReviewerAllowDowngrade,
    /// `routing.routed{reason}` — emitted once per turn that the
    /// per-turn model router dispatched on the cheap tier. `reason`
    /// distinguishes the heuristic verb, the LLM-judge vote, and the
    /// explicit user override. Aggregated counts drive heuristic
    /// tuning.
    #[serde(rename = "squeezy_routing_routed")]
    RoutingRouted,
    /// `routing.escalated{reason}` — emitted when a cheap-routed
    /// turn hands back to the parent model mid-flight. The dominant
    /// reason at a high rate signals a threshold to tune.
    #[serde(rename = "squeezy_routing_escalated")]
    RoutingEscalated,
    /// Per-refresh MCP discovery summary: server counts by transport,
    /// discovered/cached/retained/dropped tool counts, capability booleans,
    /// elicitation outcome counts, and coarse duration bucket.
    #[serde(rename = "squeezy_mcp_discovery")]
    McpDiscovery,
    /// Per-elicitation MCP event: kind (form/url) × policy × outcome.
    #[serde(rename = "squeezy_mcp_elicitation")]
    McpElicitation,
    /// Per-web-request external network event: provider × status ×
    /// SSRF/redirect blocks × coarse response-byte bucket.
    #[serde(rename = "squeezy_web_request")]
    WebRequest,
    /// Per-turn skill activation summary: source/kind/included/dropped/
    /// body-truncated/preamble counts.
    #[serde(rename = "squeezy_skill_activated")]
    SkillActivated,
    /// Per-expansion prompt-template event: source × arg-count bucket ×
    /// queued-vs-started outcome.
    #[serde(rename = "squeezy_prompt_template_expanded")]
    PromptTemplateExpanded,
    /// Per-approval/permission-verdict event: capability × risk bucket ×
    /// decision × source — never targets or reasons.
    #[serde(rename = "squeezy_approval_decided")]
    ApprovalDecided,
    /// Per-permission-policy verdict event: capability × policy-action ×
    /// source — never targets or reasons.
    #[serde(rename = "squeezy_permission_decided")]
    PermissionDecided,
    /// Per-provider-retry event: retry reason kind.
    #[serde(rename = "squeezy_provider_retry")]
    ProviderRetry,
    /// Per-provider-error event: normalized error kind bucket.
    #[serde(rename = "squeezy_provider_error")]
    ProviderError,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelemetryProperties {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_records: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dropped_buckets: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub abnormal_exit: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub telemetry_truncated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_index: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_sequence: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_family: Option<ModelFamily>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<FirstPartyToolName>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_family: Option<ToolFamily>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_status: Option<ToolStatusKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files_scanned: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub c_files: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub csharp_files: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpp_files: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub go_files: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub python_files: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rust_files: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supported_files: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unsupported_files: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unknown_files: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files_changed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files_parsed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes_read: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes_parsed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matches_returned: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbols: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edges: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_usd_micros: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt_stub_hits: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub negative_receipt_hits: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_denials: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_build_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_refresh_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slash_command_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_change_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub routing_routed_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub routing_escalated_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_successes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_errors: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_denials: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_cancellations: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_calls: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_failures: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_kind: Option<RefreshKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_sequence_scope: Option<GraphSequenceScope>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<OutcomeStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_status: Option<SessionStatusKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup_route: Option<StartupRoute>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<ErrorKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dart_files: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub java_files: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub javascript_files: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jsx_files: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kotlin_files: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub php_files: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ruby_files: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scala_files: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swift_files: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub typescript_files: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tsx_files: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excluded_files: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excluded_dirs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excluded_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub persisted_files_loaded: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub persisted_files_missed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub persistence_rebuilt: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slash_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slash_surface: Option<SlashSurface>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slash_outcome: Option<SlashOutcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slash_alias_kind: Option<SlashAliasKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slash_arg_shape: Option<SlashArgShape>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_scope: Option<ConfigScopeKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_section: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_apply_tier: Option<ConfigApplyTier>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_change_kind: Option<ConfigChangeKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_prev_bucket: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_new_bucket: Option<String>,
    /// Tagged on shell-sandbox events (e.g.
    /// `ShellSandboxBestEffortFallback`) so dashboards can break down by
    /// the OS backend that was attempted (`macos-sandbox-exec`,
    /// `linux-direct-syscalls`, `windows-job-object`, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox_backend: Option<String>,
    /// Tagged on AI-reviewer events (`AiReviewerAllowDowngrade`) so
    /// dashboards can break down silent allow-downgrade rates by the
    /// requested capability (`read`, `edit`, `shell`, ...). Operators
    /// use this to decide which capability to add to the
    /// `allow_capabilities` allowlist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_capability: Option<String>,
    /// Tagged on routing events (`RoutingRouted`, `RoutingEscalated`)
    /// so dashboards can break down per-reason rates: which heuristic
    /// rule fired, which escalation signal tripped, whether the LLM
    /// judge overrode it, whether the user explicitly forced a route.
    /// `None` on every other event type.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub routing_reason: Option<String>,
    /// The durable on-disk store session ID. Set only on `session_ended`
    /// and `session_summary` events. Lets operators correlate a PostHog
    /// session back to the on-disk session file and stitch together a chain
    /// of resumes that all share the same store session (even though each
    /// process run has a distinct telemetry `session_id` and `trace_id`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store_session_id: Option<String>,
    /// Per-session trace id stamped by [`TelemetryClient`] on every event
    /// it accepts. W3C-trace-context-shaped 32-hex-char string. Equal
    /// across every event emitted by a single Squeezy session so an
    /// operator can pivot from one Worker-side event to every other event
    /// for the same session, and so local `tracing::span!` records that
    /// embed this id correlate with the aggregate counters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// Per-turn span id stamped by [`TelemetryClient`] on events recorded
    /// inside an active turn. 16-hex-char string. `None` for events
    /// emitted outside any turn (e.g. `app_started`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_counts: Option<BTreeMap<String, u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slash_counts: Option<BTreeMap<String, u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_counts: Option<BTreeMap<String, u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub routing_counts: Option<BTreeMap<String, u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_counts: Option<BTreeMap<String, u64>>,
    // --- new domain count-maps ---
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_counts: Option<BTreeMap<String, u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_counts: Option<BTreeMap<String, u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill_counts: Option<BTreeMap<String, u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_template_counts: Option<BTreeMap<String, u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_counts: Option<BTreeMap<String, u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_counts: Option<BTreeMap<String, u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_counts: Option<BTreeMap<String, u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_counts: Option<BTreeMap<String, u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_error_counts: Option<BTreeMap<String, u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason_counts: Option<BTreeMap<String, u64>>,
    // --- new scalar/boolean fields ---
    /// `stop_reason` for this turn (safe token from StopReasonKind).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// Whether reasoning-only stop (Qwen3/DeepSeek-R1 style) occurred.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_only_stop: Option<bool>,
    /// Whether prompt caching was supported by the provider for this turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_supported: Option<bool>,
    /// Cache-write (creation) tokens for this turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    /// Reasoning output tokens for this turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_output_tokens: Option<u64>,
    /// Number of subagent concurrency-cap rejections this session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent_cap_rejections: Option<u64>,
    /// Startup duration from process launch to first placeholder draw (ms bucket token).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup_placeholder_ms: Option<u64>,
    /// Startup duration from process launch to agent build complete (ms bucket token).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup_agent_build_ms: Option<u64>,
    /// Startup duration from process launch to snapshots loaded (ms bucket token).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup_snapshot_ms: Option<u64>,
}

impl TelemetryProperties {
    fn from_turn(config: &AppConfig, turn_index: u64, metrics: &TurnMetrics) -> Self {
        Self {
            turn_index: Some(turn_index),
            provider: Some(ProviderKind::from_provider(&config.provider)),
            model_family: Some(ModelFamily::from_model(&config.provider, &config.model)),
            tool_calls: Some(metrics.tool_calls),
            files_scanned: Some(metrics.files_scanned),
            bytes_read: Some(metrics.bytes_read),
            output_bytes: Some(metrics.model_output_bytes),
            matches_returned: Some(metrics.matches_returned),
            input_tokens: metrics.provider.input_tokens,
            output_tokens: metrics.provider.output_tokens,
            cached_tokens: metrics.provider.cached_input_tokens,
            estimated_usd_micros: metrics.provider.estimated_usd_micros,
            receipt_stub_hits: Some(metrics.receipt_stub_hits),
            negative_receipt_hits: Some(metrics.negative_receipt_hits),
            budget_denials: Some(metrics.budget_denials),
            status: Some(OutcomeStatus::Success),
            stop_reason: metrics.stop_reason_token.clone(),
            reasoning_only_stop: if metrics.reasoning_only_stop {
                Some(true)
            } else {
                None
            },
            cache_supported: if metrics.cache_supported {
                Some(true)
            } else {
                None
            },
            cache_write_tokens: metrics.cache_write_tokens,
            reasoning_output_tokens: metrics.reasoning_output_tokens,
            ..Self::default()
        }
    }

    fn from_graph(report: GraphPerfReport) -> Self {
        Self {
            duration_ms: Some(report.duration_ms),
            files_scanned: Some(report.files_seen),
            c_files: Some(report.language_distribution.c_files),
            csharp_files: Some(report.language_distribution.csharp_files),
            cpp_files: Some(report.language_distribution.cpp_files),
            go_files: Some(report.language_distribution.go_files),
            dart_files: Some(report.language_distribution.dart_files),
            java_files: Some(report.language_distribution.java_files),
            javascript_files: Some(report.language_distribution.javascript_files),
            jsx_files: Some(report.language_distribution.jsx_files),
            kotlin_files: Some(report.language_distribution.kotlin_files),
            php_files: Some(report.language_distribution.php_files),
            python_files: Some(report.language_distribution.python_files),
            ruby_files: Some(report.language_distribution.ruby_files),
            rust_files: Some(report.language_distribution.rust_files),
            scala_files: Some(report.language_distribution.scala_files),
            swift_files: Some(report.language_distribution.swift_files),
            typescript_files: Some(report.language_distribution.typescript_files),
            tsx_files: Some(report.language_distribution.tsx_files),
            supported_files: Some(report.language_distribution.supported_files),
            unsupported_files: Some(report.language_distribution.unsupported_files),
            unknown_files: Some(report.language_distribution.unknown_files),
            files_changed: Some(report.files_changed),
            files_parsed: Some(report.files_parsed),
            bytes_parsed: Some(report.bytes_parsed),
            excluded_files: Some(report.excluded_files),
            excluded_dirs: Some(report.excluded_dirs),
            excluded_bytes: Some(report.excluded_bytes),
            persisted_files_loaded: Some(report.persisted_files_loaded),
            persisted_files_missed: Some(report.persisted_files_missed),
            persistence_rebuilt: Some(u64::from(report.persistence_rebuilt)),
            symbols: Some(report.symbols),
            edges: Some(report.edges),
            refresh_kind: Some(report.refresh_kind),
            graph_sequence_scope: Some(report.sequence_scope),
            status: Some(report.status),
            error_kind: report.error_kind,
            ..Self::default()
        }
    }
}

fn build_summary_from_events(
    session: &StoredTelemetrySession,
    events: Vec<TelemetryEvent>,
    abnormal: bool,
    fallback_status: Option<SessionStatusKind>,
) -> TelemetryEvent {
    let mut accumulator = SummaryAccumulator::new();
    for event in &events {
        accumulator.observe(event);
    }
    let started_at_ms = session.started_at_ms.min(
        events
            .first()
            .map(|event| event.timestamp_ms)
            .unwrap_or(u128::MAX),
    );
    let ended_at_ms = session
        .ended_at_ms
        .or_else(|| events.last().map(|event| event.timestamp_ms))
        .unwrap_or(started_at_ms);
    let status = if abnormal {
        SessionStatusKind::Failed
    } else {
        accumulator
            .session_status
            .or(fallback_status)
            .unwrap_or(SessionStatusKind::Completed)
    };
    let (tool_counts, tool_dropped) = capped_count_map(accumulator.tool_counts);
    let (slash_counts, slash_dropped) = capped_count_map(accumulator.slash_counts);
    let (failure_counts, failure_dropped) = capped_count_map(accumulator.failure_counts);
    let (routing_counts, routing_dropped) = capped_count_map(accumulator.routing_counts);
    let (config_counts, config_dropped) = capped_count_map(accumulator.config_counts);
    let (mcp_counts, mcp_dropped) = capped_count_map(accumulator.mcp_counts);
    let (external_counts, external_dropped) = capped_count_map(accumulator.external_counts);
    let (skill_counts, skill_dropped) = capped_count_map(accumulator.skill_counts);
    let (prompt_template_counts, pt_dropped) = capped_count_map(accumulator.prompt_template_counts);
    let (subagent_counts, subagent_dropped) = capped_count_map(accumulator.subagent_counts);
    let (approval_counts, approval_dropped) = capped_count_map(accumulator.approval_counts);
    let (permission_counts, permission_dropped) = capped_count_map(accumulator.permission_counts);
    let (retry_counts, retry_dropped) = capped_count_map(accumulator.retry_counts);
    let (provider_error_counts, provider_error_dropped) =
        capped_count_map(accumulator.provider_error_counts);
    let (stop_reason_counts, stop_reason_dropped) =
        capped_count_map(accumulator.stop_reason_counts);
    let dropped_buckets = tool_dropped
        + slash_dropped
        + failure_dropped
        + routing_dropped
        + config_dropped
        + mcp_dropped
        + external_dropped
        + skill_dropped
        + pt_dropped
        + subagent_dropped
        + approval_dropped
        + permission_dropped
        + retry_dropped
        + provider_error_dropped
        + stop_reason_dropped;
    let truncated = dropped_buckets > 0;
    let tool_calls = accumulator.tool_calls.max(accumulator.turn_tool_calls);
    let files_scanned = accumulator
        .files_scanned
        .max(accumulator.turn_files_scanned);
    let bytes_read = accumulator.bytes_read.max(accumulator.turn_bytes_read);
    let output_bytes = accumulator.output_bytes.max(accumulator.turn_output_bytes);
    let matches_returned = accumulator
        .matches_returned
        .max(accumulator.turn_matches_returned);
    TelemetryEvent {
        event: TelemetryEventName::SessionSummary,
        timestamp_ms: ended_at_ms,
        event_sequence: 0,
        properties: TelemetryProperties {
            trace_id: Some(session.trace_id.clone()),
            store_session_id: accumulator.store_session_id,
            started_at_ms: Some(u128_to_u64(started_at_ms)),
            ended_at_ms: Some(u128_to_u64(ended_at_ms)),
            source_records: Some(events.len() as u64),
            dropped_buckets: Some(dropped_buckets),
            abnormal_exit: Some(abnormal),
            telemetry_truncated: Some(truncated),
            provider: accumulator.provider,
            model_family: accumulator.model_family,
            duration_ms: Some(u128_to_u64(ended_at_ms.saturating_sub(started_at_ms))),
            session_status: Some(status),
            startup_route: accumulator.startup_route,
            turn_count: Some(accumulator.turn_count),
            tool_calls: Some(tool_calls),
            tool_successes: Some(accumulator.tool_successes),
            tool_errors: Some(accumulator.tool_errors),
            tool_denials: Some(accumulator.tool_denials),
            tool_cancellations: Some(accumulator.tool_cancellations),
            files_scanned: Some(files_scanned),
            bytes_read: Some(bytes_read),
            output_bytes: Some(output_bytes),
            matches_returned: Some(matches_returned),
            input_tokens: Some(accumulator.input_tokens),
            output_tokens: Some(accumulator.output_tokens),
            cached_tokens: Some(accumulator.cached_tokens),
            estimated_usd_micros: Some(accumulator.estimated_usd_micros),
            receipt_stub_hits: Some(accumulator.receipt_stub_hits),
            negative_receipt_hits: Some(accumulator.negative_receipt_hits),
            budget_denials: Some(accumulator.budget_denials),
            subagent_calls: Some(accumulator.subagent_calls),
            subagent_failures: Some(accumulator.subagent_failures),
            graph_build_count: Some(accumulator.graph_build_count),
            graph_refresh_count: Some(accumulator.graph_refresh_count),
            slash_command_count: Some(accumulator.slash_command_count),
            config_change_count: Some(accumulator.config_change_count),
            failure_count: Some(accumulator.failure_count),
            routing_routed_count: Some(accumulator.routing_routed_count),
            routing_escalated_count: Some(accumulator.routing_escalated_count),
            symbols: Some(accumulator.symbols),
            edges: Some(accumulator.edges),
            bytes_parsed: Some(accumulator.bytes_parsed),
            excluded_files: Some(accumulator.excluded_files),
            excluded_dirs: Some(accumulator.excluded_dirs),
            excluded_bytes: Some(accumulator.excluded_bytes),
            persisted_files_loaded: Some(accumulator.persisted_files_loaded),
            persisted_files_missed: Some(accumulator.persisted_files_missed),
            tool_counts: non_empty_map(tool_counts),
            slash_counts: non_empty_map(slash_counts),
            failure_counts: non_empty_map(failure_counts),
            routing_counts: non_empty_map(routing_counts),
            config_counts: non_empty_map(config_counts),
            mcp_counts: non_empty_map(mcp_counts),
            external_counts: non_empty_map(external_counts),
            skill_counts: non_empty_map(skill_counts),
            prompt_template_counts: non_empty_map(prompt_template_counts),
            subagent_counts: non_empty_map(subagent_counts),
            approval_counts: non_empty_map(approval_counts),
            permission_counts: non_empty_map(permission_counts),
            retry_counts: non_empty_map(retry_counts),
            provider_error_counts: non_empty_map(provider_error_counts),
            stop_reason_counts: non_empty_map(stop_reason_counts),
            subagent_cap_rejections: if accumulator.subagent_cap_rejections > 0 {
                Some(accumulator.subagent_cap_rejections)
            } else {
                None
            },
            cache_write_tokens: if accumulator.cache_write_tokens > 0 {
                Some(accumulator.cache_write_tokens)
            } else {
                None
            },
            reasoning_output_tokens: if accumulator.reasoning_output_tokens > 0 {
                Some(accumulator.reasoning_output_tokens)
            } else {
                None
            },
            cache_supported: accumulator.cache_supported,
            startup_placeholder_ms: accumulator.startup_placeholder_ms,
            startup_agent_build_ms: accumulator.startup_agent_build_ms,
            startup_snapshot_ms: accumulator.startup_snapshot_ms,
            ..TelemetryProperties::default()
        },
    }
}

fn with_summary_id(mut event: TelemetryEvent, summary_id: &str) -> TelemetryEvent {
    event.properties.summary_id = Some(summary_id.to_string());
    event
}

/// Rolls per-event telemetry into the session summary that callers fold
/// into the final `summary` event.
///
/// `failure_count` is the cumulative counter of "things that meaningfully
/// failed mid-run", and most failure-shaped events both increment it and
/// add a per-kind row in `failure_counts`. There is one deliberate
/// asymmetry: `ShellWindowsDegraded` only ticks the per-kind row, not
/// `failure_count`, because it is a steady-state platform posture (every
/// Windows `windows-job-object` run hits it) rather than a runtime
/// failure. Counting it as a failure would dominate any Windows session's
/// cumulative failure_count and drown out genuine errors. Per-kind rows
/// stay separable so dashboards that want platform breakdowns still get
/// them. `ShellSandboxBestEffortFallback`, by contrast, *is* a runtime
/// degradation (the configured sandbox couldn't start and we fell back),
/// so it bumps both. Any future steady-state-degradation event should
/// mirror the `ShellWindowsDegraded` shape.
#[derive(Debug, Default)]
struct SummaryAccumulator {
    provider: Option<ProviderKind>,
    model_family: Option<ModelFamily>,
    startup_route: Option<StartupRoute>,
    session_status: Option<SessionStatusKind>,
    store_session_id: Option<String>,
    turn_count: u64,
    tool_calls: u64,
    turn_tool_calls: u64,
    tool_successes: u64,
    tool_errors: u64,
    tool_denials: u64,
    tool_cancellations: u64,
    files_scanned: u64,
    turn_files_scanned: u64,
    bytes_read: u64,
    turn_bytes_read: u64,
    output_bytes: u64,
    turn_output_bytes: u64,
    matches_returned: u64,
    turn_matches_returned: u64,
    input_tokens: u64,
    output_tokens: u64,
    cached_tokens: u64,
    estimated_usd_micros: u64,
    receipt_stub_hits: u64,
    negative_receipt_hits: u64,
    budget_denials: u64,
    subagent_calls: u64,
    subagent_failures: u64,
    graph_build_count: u64,
    graph_refresh_count: u64,
    slash_command_count: u64,
    config_change_count: u64,
    failure_count: u64,
    routing_routed_count: u64,
    routing_escalated_count: u64,
    symbols: u64,
    edges: u64,
    bytes_parsed: u64,
    excluded_files: u64,
    excluded_dirs: u64,
    excluded_bytes: u64,
    persisted_files_loaded: u64,
    persisted_files_missed: u64,
    tool_counts: BTreeMap<String, u64>,
    slash_counts: BTreeMap<String, u64>,
    failure_counts: BTreeMap<String, u64>,
    routing_counts: BTreeMap<String, u64>,
    config_counts: BTreeMap<String, u64>,
    // new domain count-maps
    mcp_counts: BTreeMap<String, u64>,
    external_counts: BTreeMap<String, u64>,
    skill_counts: BTreeMap<String, u64>,
    prompt_template_counts: BTreeMap<String, u64>,
    subagent_counts: BTreeMap<String, u64>,
    approval_counts: BTreeMap<String, u64>,
    permission_counts: BTreeMap<String, u64>,
    retry_counts: BTreeMap<String, u64>,
    provider_error_counts: BTreeMap<String, u64>,
    stop_reason_counts: BTreeMap<String, u64>,
    // new scalar accumulators
    subagent_cap_rejections: u64,
    cache_write_tokens: u64,
    reasoning_output_tokens: u64,
    cache_supported: Option<bool>,
    startup_placeholder_ms: Option<u64>,
    startup_agent_build_ms: Option<u64>,
    startup_snapshot_ms: Option<u64>,
}

impl SummaryAccumulator {
    fn new() -> Self {
        Self::default()
    }

    fn observe(&mut self, event: &TelemetryEvent) {
        let props = &event.properties;
        if self.provider.is_none() {
            self.provider = props.provider;
        }
        if self.model_family.is_none() {
            self.model_family = props.model_family;
        }
        match event.event {
            TelemetryEventName::AppStarted => {}
            TelemetryEventName::StartupReady => {
                self.startup_route = props.startup_route;
            }
            TelemetryEventName::TurnCompleted => {
                self.turn_count = self.turn_count.saturating_add(1);
                self.turn_tool_calls = self
                    .turn_tool_calls
                    .saturating_add(props.tool_calls.unwrap_or(0));
                self.turn_files_scanned = self
                    .turn_files_scanned
                    .saturating_add(props.files_scanned.unwrap_or(0));
                self.turn_bytes_read = self
                    .turn_bytes_read
                    .saturating_add(props.bytes_read.unwrap_or(0));
                self.turn_output_bytes = self
                    .turn_output_bytes
                    .saturating_add(props.output_bytes.unwrap_or(0));
                self.turn_matches_returned = self
                    .turn_matches_returned
                    .saturating_add(props.matches_returned.unwrap_or(0));
                self.input_tokens = self
                    .input_tokens
                    .saturating_add(props.input_tokens.unwrap_or(0));
                self.output_tokens = self
                    .output_tokens
                    .saturating_add(props.output_tokens.unwrap_or(0));
                self.cached_tokens = self
                    .cached_tokens
                    .saturating_add(props.cached_tokens.unwrap_or(0));
                self.estimated_usd_micros = self
                    .estimated_usd_micros
                    .saturating_add(props.estimated_usd_micros.unwrap_or(0));
                self.receipt_stub_hits = self
                    .receipt_stub_hits
                    .saturating_add(props.receipt_stub_hits.unwrap_or(0));
                self.negative_receipt_hits = self
                    .negative_receipt_hits
                    .saturating_add(props.negative_receipt_hits.unwrap_or(0));
                self.budget_denials = self
                    .budget_denials
                    .saturating_add(props.budget_denials.unwrap_or(0));
            }
            TelemetryEventName::ToolCompleted => {
                self.tool_calls = self.tool_calls.saturating_add(1);
                self.files_scanned = self
                    .files_scanned
                    .saturating_add(props.files_scanned.unwrap_or(0));
                self.bytes_read = self
                    .bytes_read
                    .saturating_add(props.bytes_read.unwrap_or(0));
                self.output_bytes = self
                    .output_bytes
                    .saturating_add(props.output_bytes.unwrap_or(0));
                self.matches_returned = self
                    .matches_returned
                    .saturating_add(props.matches_returned.unwrap_or(0));
                match props.tool_status {
                    Some(ToolStatusKind::Success) => {
                        self.tool_successes = self.tool_successes.saturating_add(1);
                    }
                    Some(ToolStatusKind::Error | ToolStatusKind::Stale) => {
                        self.tool_errors = self.tool_errors.saturating_add(1);
                    }
                    Some(ToolStatusKind::Denied) => {
                        self.tool_denials = self.tool_denials.saturating_add(1);
                    }
                    Some(ToolStatusKind::Cancelled) => {
                        self.tool_cancellations = self.tool_cancellations.saturating_add(1);
                    }
                    None => {}
                }
                if let Some(name) = props
                    .tool_name
                    .and_then(|name| serde_token(&name))
                    .or_else(|| props.tool_family.and_then(|family| serde_token(&family)))
                {
                    increment_count(&mut self.tool_counts, name);
                }
            }
            TelemetryEventName::GraphBuildCompleted => {
                self.graph_build_count = self.graph_build_count.saturating_add(1);
                self.observe_graph(props);
            }
            TelemetryEventName::GraphRefreshCompleted => {
                self.graph_refresh_count = self.graph_refresh_count.saturating_add(1);
                self.observe_graph(props);
            }
            TelemetryEventName::SessionEnded => {
                self.session_status = props.session_status;
                self.store_session_id = props.store_session_id.clone();
                self.turn_count = self.turn_count.max(props.turn_count.unwrap_or(0));
                self.tool_calls = self.tool_calls.max(props.tool_calls.unwrap_or(0));
                self.tool_successes = self.tool_successes.max(props.tool_successes.unwrap_or(0));
                self.tool_errors = self.tool_errors.max(props.tool_errors.unwrap_or(0));
                self.tool_denials = self.tool_denials.max(props.tool_denials.unwrap_or(0));
                self.tool_cancellations = self
                    .tool_cancellations
                    .max(props.tool_cancellations.unwrap_or(0));
                self.budget_denials = self.budget_denials.max(props.budget_denials.unwrap_or(0));
                self.subagent_calls = self.subagent_calls.max(props.subagent_calls.unwrap_or(0));
                self.subagent_failures = self
                    .subagent_failures
                    .max(props.subagent_failures.unwrap_or(0));
                // Fold per-kind subagent counts from the session_ended event.
                if let Some(counts) = props.subagent_counts.as_ref() {
                    for (k, v) in counts {
                        *self.subagent_counts.entry(k.clone()).or_default() += v;
                    }
                }
            }
            TelemetryEventName::SlashCommandUsed => {
                self.slash_command_count = self.slash_command_count.saturating_add(1);
                if let Some(command) = props.slash_command.as_ref() {
                    increment_count(&mut self.slash_counts, command.clone());
                }
            }
            TelemetryEventName::ConfigChangeCommitted => {
                self.config_change_count = self.config_change_count.saturating_add(1);
                if let Some(field) = props.config_field.as_ref() {
                    increment_count(&mut self.config_counts, field.clone());
                }
            }
            TelemetryEventName::FailureSeen => {
                self.failure_count = self.failure_count.saturating_add(1);
                if let Some(kind) = props.error_kind.and_then(|kind| serde_token(&kind)) {
                    increment_count(&mut self.failure_counts, kind);
                }
            }
            TelemetryEventName::ShellSandboxBestEffortFallback => {
                self.failure_count = self.failure_count.saturating_add(1);
                increment_count(
                    &mut self.failure_counts,
                    "approval_best_effort_fallback".to_string(),
                );
            }
            TelemetryEventName::ShellWindowsDegraded => {
                // Windows steady-state degradation: counted separately from
                // Unix sandbox runtime failures so dashboards can filter by
                // platform without Unix/Windows signals cross-contaminating.
                increment_count(
                    &mut self.failure_counts,
                    "shell_windows_degraded".to_string(),
                );
            }
            TelemetryEventName::AiReviewerAllowDowngrade => {
                self.failure_count = self.failure_count.saturating_add(1);
                increment_count(
                    &mut self.failure_counts,
                    "ai_reviewer_allow_downgrade".to_string(),
                );
            }
            TelemetryEventName::RoutingRouted => {
                self.routing_routed_count = self.routing_routed_count.saturating_add(1);
                if let Some(reason) = props.routing_reason.as_ref() {
                    increment_count(&mut self.routing_counts, format!("routed:{reason}"));
                }
            }
            TelemetryEventName::RoutingEscalated => {
                self.routing_escalated_count = self.routing_escalated_count.saturating_add(1);
                if let Some(reason) = props.routing_reason.as_ref() {
                    increment_count(&mut self.routing_counts, format!("escalated:{reason}"));
                }
            }
            TelemetryEventName::McpDiscovery => {
                if let Some(counts) = props.mcp_counts.as_ref() {
                    for (k, v) in counts {
                        *self.mcp_counts.entry(k.clone()).or_default() += v;
                    }
                }
            }
            TelemetryEventName::McpElicitation => {
                if let Some(counts) = props.mcp_counts.as_ref() {
                    for (k, v) in counts {
                        *self.mcp_counts.entry(k.clone()).or_default() += v;
                    }
                }
            }
            TelemetryEventName::WebRequest => {
                if let Some(counts) = props.external_counts.as_ref() {
                    for (k, v) in counts {
                        *self.external_counts.entry(k.clone()).or_default() += v;
                    }
                }
            }
            TelemetryEventName::SkillActivated => {
                if let Some(counts) = props.skill_counts.as_ref() {
                    for (k, v) in counts {
                        *self.skill_counts.entry(k.clone()).or_default() += v;
                    }
                }
            }
            TelemetryEventName::PromptTemplateExpanded => {
                if let Some(counts) = props.prompt_template_counts.as_ref() {
                    for (k, v) in counts {
                        *self.prompt_template_counts.entry(k.clone()).or_default() += v;
                    }
                }
            }
            TelemetryEventName::ApprovalDecided => {
                if let Some(counts) = props.approval_counts.as_ref() {
                    for (k, v) in counts {
                        *self.approval_counts.entry(k.clone()).or_default() += v;
                    }
                }
            }
            TelemetryEventName::PermissionDecided => {
                if let Some(counts) = props.permission_counts.as_ref() {
                    for (k, v) in counts {
                        *self.permission_counts.entry(k.clone()).or_default() += v;
                    }
                }
            }
            TelemetryEventName::ProviderRetry => {
                if let Some(counts) = props.retry_counts.as_ref() {
                    for (k, v) in counts {
                        *self.retry_counts.entry(k.clone()).or_default() += v;
                    }
                }
            }
            TelemetryEventName::ProviderError => {
                if let Some(counts) = props.provider_error_counts.as_ref() {
                    for (k, v) in counts {
                        *self.provider_error_counts.entry(k.clone()).or_default() += v;
                    }
                }
            }
            TelemetryEventName::SessionSummary => {}
        }
        // Accumulate per-turn scalars present on any event type.
        self.subagent_cap_rejections = self
            .subagent_cap_rejections
            .saturating_add(props.subagent_cap_rejections.unwrap_or(0));
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(props.cache_write_tokens.unwrap_or(0));
        self.reasoning_output_tokens = self
            .reasoning_output_tokens
            .saturating_add(props.reasoning_output_tokens.unwrap_or(0));
        if props.cache_supported == Some(true) {
            self.cache_supported = Some(true);
        }
        if let Some(ms) = props.startup_placeholder_ms {
            self.startup_placeholder_ms = Some(ms);
        }
        if let Some(ms) = props.startup_agent_build_ms {
            self.startup_agent_build_ms = Some(ms);
        }
        if let Some(ms) = props.startup_snapshot_ms {
            self.startup_snapshot_ms = Some(ms);
        }
        // Fold stop-reason into count-map.
        if let Some(reason) = props.stop_reason.as_ref() {
            increment_count(&mut self.stop_reason_counts, reason.clone());
        }
        // Fold reasoning_only_stop into stop_reason_counts so it surfaces in
        // the summary even when reasoning_only_stop is true without a distinct
        // stop_reason token being set.
        if props.reasoning_only_stop == Some(true) {
            increment_count(&mut self.stop_reason_counts, "reasoning_only".to_string());
        }
        // NOTE: subagent_counts are folded inside the SessionEnded branch above
        // and must NOT be folded here too — doing so would double-count them.
    }

    fn observe_graph(&mut self, props: &TelemetryProperties) {
        self.symbols = self.symbols.saturating_add(props.symbols.unwrap_or(0));
        self.edges = self.edges.saturating_add(props.edges.unwrap_or(0));
        self.bytes_parsed = self
            .bytes_parsed
            .saturating_add(props.bytes_parsed.unwrap_or(0));
        self.excluded_files = self
            .excluded_files
            .saturating_add(props.excluded_files.unwrap_or(0));
        self.excluded_dirs = self
            .excluded_dirs
            .saturating_add(props.excluded_dirs.unwrap_or(0));
        self.excluded_bytes = self
            .excluded_bytes
            .saturating_add(props.excluded_bytes.unwrap_or(0));
        self.persisted_files_loaded = self
            .persisted_files_loaded
            .saturating_add(props.persisted_files_loaded.unwrap_or(0));
        self.persisted_files_missed = self
            .persisted_files_missed
            .saturating_add(props.persisted_files_missed.unwrap_or(0));
    }
}

fn increment_count(map: &mut BTreeMap<String, u64>, key: String) {
    *map.entry(key).or_default() += 1;
}

fn serde_token<T: Serialize>(value: &T) -> Option<String> {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
}

fn capped_count_map(map: BTreeMap<String, u64>) -> (BTreeMap<String, u64>, u64) {
    let original_len = map.len();
    let mut entries = map.into_iter().collect::<Vec<_>>();
    entries.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    entries.truncate(MAX_SUMMARY_MAP_ENTRIES);
    let capped = entries.into_iter().collect::<BTreeMap<_, _>>();
    let dropped = original_len.saturating_sub(capped.len()) as u64;
    (capped, dropped)
}

fn non_empty_map(map: BTreeMap<String, u64>) -> Option<BTreeMap<String, u64>> {
    if map.is_empty() { None } else { Some(map) }
}

fn u128_to_u64(value: u128) -> u64 {
    value.min(u64::MAX as u128) as u64
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolCostProperties {
    pub files_scanned: u64,
    pub bytes_read: u64,
    pub matches_returned: u64,
    pub output_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolTelemetryReport<'a> {
    pub provider: &'a ProviderConfig,
    pub model: &'a str,
    pub turn_index: u64,
    pub tool_sequence: u64,
    pub tool_name: &'a str,
    pub status: ToolStatusKind,
    pub duration: Duration,
    pub cost: ToolCostProperties,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphPerfReport {
    pub refresh_kind: RefreshKind,
    pub status: OutcomeStatus,
    pub sequence_scope: GraphSequenceScope,
    pub duration_ms: u64,
    pub files_seen: u64,
    pub files_changed: u64,
    pub files_parsed: u64,
    pub bytes_parsed: u64,
    pub excluded_files: u64,
    pub excluded_dirs: u64,
    pub excluded_bytes: u64,
    pub persisted_files_loaded: u64,
    pub persisted_files_missed: u64,
    pub persistence_rebuilt: bool,
    pub symbols: u64,
    pub edges: u64,
    pub language_distribution: LanguageDistribution,
    pub error_kind: Option<ErrorKind>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LanguageDistribution {
    pub c_files: u64,
    pub csharp_files: u64,
    pub cpp_files: u64,
    pub dart_files: u64,
    pub go_files: u64,
    pub java_files: u64,
    pub javascript_files: u64,
    pub jsx_files: u64,
    pub kotlin_files: u64,
    pub php_files: u64,
    pub python_files: u64,
    pub ruby_files: u64,
    pub rust_files: u64,
    pub scala_files: u64,
    pub swift_files: u64,
    pub typescript_files: u64,
    pub tsx_files: u64,
    pub supported_files: u64,
    pub unsupported_files: u64,
    pub unknown_files: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTelemetryReport {
    pub duration_ms: u64,
    pub status: SessionStatusKind,
    /// The durable on-disk session ID (distinct from the per-process
    /// telemetry `session_id`). Lets operators correlate PostHog sessions
    /// back to the session store file and stitch together resume chains.
    pub store_session_id: Option<String>,
    pub turns: u64,
    pub tool_calls: u64,
    pub tool_successes: u64,
    pub tool_errors: u64,
    pub tool_denials: u64,
    pub tool_cancellations: u64,
    pub budget_denials: u64,
    pub subagent_calls: u64,
    pub subagent_failures: u64,
    /// Per-kind subagent counts keyed by `"<kind>_success"` / `"<kind>_failure"`.
    /// Capped in the summary by `MAX_SUMMARY_MAP_ENTRIES`.
    pub subagent_kind_counts: std::collections::BTreeMap<String, u64>,
    /// Number of subagent calls rejected by the concurrency cap.
    pub subagent_cap_rejections: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashTelemetryReport<'a> {
    pub command: &'a str,
    pub surface: SlashSurface,
    pub outcome: SlashOutcome,
    pub alias_kind: SlashAliasKind,
    pub arg_shape: SlashArgShape,
}

impl<'a> SlashTelemetryReport<'a> {
    pub fn new(
        command: &'a str,
        surface: SlashSurface,
        outcome: SlashOutcome,
        alias_kind: SlashAliasKind,
        arg_shape: SlashArgShape,
    ) -> Self {
        Self {
            command,
            surface,
            outcome,
            alias_kind,
            arg_shape,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigChangeReport<'a> {
    pub scope: ConfigScopeKind,
    pub section: &'a str,
    pub field: &'a str,
    pub apply_tier: ConfigApplyTier,
    pub change_kind: ConfigChangeKind,
    pub prev_bucket: &'a str,
    pub new_bucket: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphSequenceScope {
    OneShot,
    Repeated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    OpenAi,
    Anthropic,
    Google,
    AzureOpenAi,
    Bedrock,
    Ollama,
    OpenRouter,
    Vercel,
    PortKey,
    Groq,
    XAi,
    DeepSeek,
    Vertex,
    Mistral,
    Together,
    Fireworks,
    Cerebras,
    DeepInfra,
    Baseten,
    // Pin wire format so the snake_case derivation doesn't split the
    // multi-capital variant names (`LMStudio` -> `l_m_studio`) and diverge
    // from the canonical preset identifier.
    #[serde(rename = "lmstudio")]
    LMStudio,
    #[serde(rename = "vllm")]
    VLlm,
    #[serde(rename = "llamacpp")]
    LlamaCpp,
    CloudflareWorkersAi,
    CloudflareAiGateway,
    OpenAiCompatible,
    OpenAiCodex,
    GitHubCopilot,
    /// In-process faux provider used by the eval harness and tests.
    /// Reported alongside the real provider kinds so telemetry stays
    /// honest about which sessions touched the network and which ran
    /// against scripted fixtures.
    Faux,
}

impl ProviderKind {
    fn from_provider(provider: &ProviderConfig) -> Self {
        use squeezy_core::OpenAiCompatiblePreset;
        match provider {
            ProviderConfig::OpenAi(_) => Self::OpenAi,
            ProviderConfig::Anthropic(_) => Self::Anthropic,
            ProviderConfig::Google(_) => Self::Google,
            ProviderConfig::AzureOpenAi(_) => Self::AzureOpenAi,
            ProviderConfig::Bedrock(_) => Self::Bedrock,
            ProviderConfig::Ollama(_) => Self::Ollama,
            ProviderConfig::OpenAiCodex(_) => Self::OpenAiCodex,
            ProviderConfig::GitHubCopilot(_) => Self::GitHubCopilot,
            ProviderConfig::OpenAiCompatible(config) => match config.preset {
                OpenAiCompatiblePreset::OpenRouter => Self::OpenRouter,
                OpenAiCompatiblePreset::Vercel => Self::Vercel,
                OpenAiCompatiblePreset::PortKey => Self::PortKey,
                OpenAiCompatiblePreset::Groq => Self::Groq,
                OpenAiCompatiblePreset::XAi => Self::XAi,
                OpenAiCompatiblePreset::DeepSeek => Self::DeepSeek,
                OpenAiCompatiblePreset::Vertex => Self::Vertex,
                OpenAiCompatiblePreset::Mistral => Self::Mistral,
                OpenAiCompatiblePreset::Together => Self::Together,
                OpenAiCompatiblePreset::Fireworks => Self::Fireworks,
                OpenAiCompatiblePreset::Cerebras => Self::Cerebras,
                OpenAiCompatiblePreset::DeepInfra => Self::DeepInfra,
                OpenAiCompatiblePreset::Baseten => Self::Baseten,
                OpenAiCompatiblePreset::LMStudio => Self::LMStudio,
                OpenAiCompatiblePreset::VLlm => Self::VLlm,
                OpenAiCompatiblePreset::LlamaCpp => Self::LlamaCpp,
                OpenAiCompatiblePreset::CloudflareWorkersAi => Self::CloudflareWorkersAi,
                OpenAiCompatiblePreset::CloudflareAiGateway => Self::CloudflareAiGateway,
                OpenAiCompatiblePreset::Custom => Self::OpenAiCompatible,
            },
            ProviderConfig::Faux(_) => Self::Faux,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelFamily {
    Gpt,
    Claude,
    Gemini,
    Bedrock,
    Ollama,
    Other,
}

impl ModelFamily {
    fn from_model(provider: &ProviderConfig, model: &str) -> Self {
        let model_lower = model.to_ascii_lowercase();
        // OpenAI-compatible aggregators often namespace models as
        // "<vendor>/<id>" (e.g. "anthropic/claude-opus-4-7"). Strip the
        // namespace so the family classifier matches on the actual model id.
        let stripped = model_lower
            .split_once('/')
            .map(|(_, id)| id)
            .unwrap_or(&model_lower);
        if stripped.starts_with("gpt") || stripped.starts_with("o1") || stripped.starts_with("o3") {
            Self::Gpt
        } else if stripped.starts_with("claude") || matches!(provider, ProviderConfig::Anthropic(_))
        {
            Self::Claude
        } else if stripped.starts_with("gemini") || matches!(provider, ProviderConfig::Google(_)) {
            Self::Gemini
        } else if matches!(provider, ProviderConfig::Bedrock(_)) {
            Self::Bedrock
        } else if matches!(provider, ProviderConfig::Ollama(_)) {
            Self::Ollama
        } else {
            Self::Other
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FirstPartyToolName {
    Glob,
    Grep,
    ReadFile,
    ReadToolOutput,
    WriteFile,
    Shell,
    Webfetch,
    Websearch,
    Graph,
    Ast,
    Mcp,
    Other,
}

impl FirstPartyToolName {
    fn from_tool_name(name: &str) -> Self {
        match name {
            "glob" => Self::Glob,
            "grep" => Self::Grep,
            "read_file" => Self::ReadFile,
            "read_tool_output" => Self::ReadToolOutput,
            "write_file" => Self::WriteFile,
            "shell" => Self::Shell,
            "webfetch" => Self::Webfetch,
            "websearch" => Self::Websearch,
            "repo_map" | "decl_search" | "definition_search" | "reference_search"
            | "upstream_flow" | "downstream_flow" | "symbol_context" | "hierarchy"
            | "read_slice" => Self::Graph,
            "ast_build" | "graph_refresh" => Self::Ast,
            name if name.starts_with("mcp__") => Self::Mcp,
            _ => Self::Other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolFamily {
    Search,
    Read,
    Write,
    Shell,
    Web,
    Graph,
    Ast,
    Mcp,
    Other,
}

impl ToolFamily {
    fn from_tool_name(name: &str) -> Self {
        match name {
            "glob" | "grep" => Self::Search,
            "read_file" | "read_tool_output" => Self::Read,
            "write_file" => Self::Write,
            "shell" => Self::Shell,
            "webfetch" | "websearch" => Self::Web,
            "repo_map" | "decl_search" | "definition_search" | "reference_search"
            | "upstream_flow" | "downstream_flow" | "symbol_context" | "hierarchy"
            | "read_slice" => Self::Graph,
            "ast_build" | "graph_refresh" => Self::Ast,
            name if name.starts_with("mcp__") => Self::Mcp,
            _ => Self::Other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatusKind {
    Success,
    Error,
    Denied,
    Stale,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefreshKind {
    Cold,
    Incremental,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeStatus {
    Success,
    Error,
    Cancelled,
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatusKind {
    Running,
    Archived,
    Completed,
    Cancelled,
    Failed,
    Truncated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupRoute {
    Fresh,
    DirectResume,
    ResumePickerFresh,
    ResumePickerResume,
    FirstRunSetupFresh,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlashSurface {
    TuiComposer,
    TuiInline,
    AgentRaw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlashOutcome {
    Accepted,
    UsageError,
    BlockedDuringTurn,
    Unknown,
    TemplateExpanded,
    StartedTurn,
    OpenedOverlay,
    StartedJob,
    LocalAction,
    Skipped,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlashAliasKind {
    Canonical,
    CompatOptions,
    Unknown,
    Template,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlashArgShape {
    None,
    Present,
    FixedSubcommand,
    Id,
    Path,
    FreeText,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigScopeKind {
    User,
    Project,
    Local,
    Session,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigApplyTier {
    Immediate,
    NextPrompt,
    Restart,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigChangeKind {
    Set,
    Unset,
    Reset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    Provider,
    Tool,
    Permission,
    Budget,
    Graph,
    Io,
    Config,
    Unknown,
}

fn load_or_create_install_id(path: &Path) -> std::io::Result<String> {
    if let Ok(raw) = fs::read_to_string(path) {
        let id = raw.trim();
        if is_uuid_like(id) {
            return Ok(id.to_string());
        }
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let id = random_uuid_like();
    fs::write(path, format!("{id}\n"))?;
    Ok(id)
}

fn default_install_id_path() -> PathBuf {
    env::var_os("SQUEEZY_TELEMETRY_INSTALL_ID_PATH")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".squeezy/install_id"))
        })
        .unwrap_or_else(|| PathBuf::from(".squeezy/install_id"))
}

fn default_telemetry_store_path_for_install_path(install_id_path: &Path) -> PathBuf {
    env::var_os("SQUEEZY_TELEMETRY_STORE_PATH")
        .map(PathBuf::from)
        .or_else(|| {
            install_id_path
                .parent()
                .map(|parent| parent.join("telemetry.redb"))
        })
        .unwrap_or_else(|| PathBuf::from(".squeezy/telemetry.redb"))
}

fn purge_telemetry_store_for_install_path(install_id_path: &Path) {
    let path = default_telemetry_store_path_for_install_path(install_id_path);
    let _ = fs::remove_file(path);
}

/// Stamp the session-scoped `trace_id` and any active per-turn `span_id`
/// on `event`. Idempotent: a `trace_id` already present on the event
/// (set by a caller for a span they manage themselves) is preserved.
fn stamp_trace_ids(state: &TelemetryState, event: &mut TelemetryEvent) {
    if event.properties.trace_id.is_none() {
        event.properties.trace_id = Some(state.trace_id.clone());
    }
    if event.properties.span_id.is_none()
        && let Ok(guard) = state.current_span_id.lock()
        && let Some(span_id) = guard.as_ref()
    {
        event.properties.span_id = Some(span_id.clone());
    }
    if event.properties.store_session_id.is_none()
        && let Ok(guard) = state.store_session_id.lock()
        && let Some(id) = guard.as_ref()
    {
        event.properties.store_session_id = Some(id.clone());
    }
}

fn slash_command_token(value: &str) -> String {
    let mut token = String::new();
    let trimmed = value.trim().trim_start_matches('/');
    for ch in trimmed.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
            token.push(ch);
        } else {
            return "unknown".to_string();
        }
        if token.len() >= 80 {
            break;
        }
    }
    if token.is_empty() {
        "unknown".to_string()
    } else {
        token
    }
}

/// W3C-trace-context style 32-hex-char trace id. We don't pull in the
/// `opentelemetry` crate — the audit's F10 explicitly notes that plain
/// UUIDs are sufficient — but we shape the id like a real `traceparent`
/// so an operator pasting it into a Jaeger/Honeycomb URL bar gets a
/// match if/when downstream collectors are added.
fn random_trace_id() -> String {
    let mut bytes = [0u8; 16];
    fill_random_bytes(&mut bytes);
    let mut out = String::with_capacity(32);
    push_hex_bytes(&mut out, &bytes);
    out
}

/// W3C-trace-context style 16-hex-char span id.
fn random_span_id() -> String {
    let mut bytes = [0u8; 8];
    fill_random_bytes(&mut bytes);
    let mut out = String::with_capacity(16);
    push_hex_bytes(&mut out, &bytes);
    out
}

fn fill_random_bytes(bytes: &mut [u8]) {
    if fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(bytes))
        .is_ok()
    {
        return;
    }
    // Mix in a per-process counter so a `now_ms` collision under a clock
    // freeze still produces distinct ids.
    static SALT: AtomicU64 = AtomicU64::new(0);
    let salt = SALT.fetch_add(1, Ordering::Relaxed);
    let mix = now_ms() as u64 ^ salt;
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = ((mix >> ((i % 8) * 8)) & 0xff) as u8;
    }
}

fn random_uuid_like() -> String {
    let mut bytes = [0u8; 16];
    if fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .is_err()
    {
        let fallback = now_ms().to_le_bytes();
        bytes[..fallback.len()].copy_from_slice(&fallback);
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let mut out = String::with_capacity(36);
    for (index, byte) in bytes.iter().copied().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        push_hex_byte(&mut out, byte);
    }
    out
}

fn push_hex_bytes(output: &mut String, bytes: &[u8]) {
    for byte in bytes {
        push_hex_byte(output, *byte);
    }
}

fn push_hex_byte(output: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    output.push(HEX[(byte >> 4) as usize] as char);
    output.push(HEX[(byte & 0x0f) as usize] as char);
}

fn is_uuid_like(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

/// Report passed to [`TelemetryEvent::mcp_discovery`].
#[derive(Debug, Clone, Default)]
pub struct McpDiscoveryReport {
    pub servers_stdio: u32,
    pub servers_http: u32,
    pub servers_sse: u32,
    pub servers_enabled: u32,
    pub servers_disabled: u32,
    pub tools_discovered: u32,
    pub tools_cached: u32,
    pub tools_stale_retained: u32,
    pub tools_dropped_disabled: u32,
    pub discovery_errors: u32,
    /// Counts keyed by coarse error kind token (e.g. `"transport"`, `"timeout"`).
    pub error_kind_counts: BTreeMap<String, u64>,
    pub has_resources: bool,
    pub has_elicitation: bool,
    pub has_experimental: bool,
    pub duration_ms: u64,
}

/// Report passed to [`TelemetryEvent::web_request`].
#[derive(Debug, Clone)]
pub struct WebRequestReport {
    /// Safe token for the provider (e.g. `"exa"`, `"parallel"`).
    pub provider_token: String,
    /// Safe token for the outcome status (e.g. `"success"`, `"error"`, `"cancelled"`).
    pub status_token: String,
    pub ssrf_blocked: bool,
    pub redirect_blocked: bool,
    /// Coarse byte bucket token (e.g. `"0_1k"`, `"1k_10k"`, `"10k_100k"`, `"100k_plus"`).
    pub response_byte_bucket: String,
    pub duration_ms: u64,
}

/// Report passed to [`TelemetryEvent::skill_activated`].
#[derive(Debug, Clone, Default)]
pub struct SkillActivationReport {
    pub total: u32,
    pub included: u32,
    pub dropped: u32,
    pub body_truncated: u32,
    pub preamble_emitted: bool,
    pub preamble_omitted_count: u32,
    pub explicit_count: u32,
    pub trigger_count: u32,
    pub implicit_shell_count: u32,
    /// Counts keyed by source token (e.g. `"user"`, `"project"`, `"extra_root"`).
    pub source_counts: BTreeMap<String, u64>,
}

/// Normalized provider retry reason, used in [`TelemetryEvent::provider_retry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryReasonKind {
    RateLimit,
    Server5xx,
    Transport,
    IdleTimeout,
    Truncated,
    Divergence,
    AuthRefresh,
    StreamReconnect,
    TerminalQuota,
    NonRetryable,
}

impl RetryReasonKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RateLimit => "rate_limit",
            Self::Server5xx => "server_5xx",
            Self::Transport => "transport",
            Self::IdleTimeout => "idle_timeout",
            Self::Truncated => "truncated",
            Self::Divergence => "divergence",
            Self::AuthRefresh => "auth_refresh",
            Self::StreamReconnect => "stream_reconnect",
            Self::TerminalQuota => "terminal_quota",
            Self::NonRetryable => "non_retryable",
        }
    }
}

/// Normalized provider error category, used in [`TelemetryEvent::provider_error`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorKind {
    Auth,
    Permission,
    Quota,
    RateLimit,
    ContextOverflow,
    ContentFilter,
    InvalidRequest,
    NotFound,
    Server,
    Transport,
    Parse,
    Unknown,
}

impl ProviderErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Permission => "permission",
            Self::Quota => "quota",
            Self::RateLimit => "rate_limit",
            Self::ContextOverflow => "context_overflow",
            Self::ContentFilter => "content_filter",
            Self::InvalidRequest => "invalid_request",
            Self::NotFound => "not_found",
            Self::Server => "server",
            Self::Transport => "transport",
            Self::Parse => "parse",
            Self::Unknown => "unknown",
        }
    }
}

pub fn telemetry_config(enabled: bool, endpoint: impl Into<String>) -> TelemetryConfig {
    TelemetryConfig {
        enabled,
        endpoint: endpoint.into(),
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
