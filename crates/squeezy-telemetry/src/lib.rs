use std::{
    env, fs,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use squeezy_core::{AppConfig, ProviderConfig, TelemetryConfig, TurnMetrics};
use tokio::{sync::Mutex, time};

const SCHEMA_VERSION: u32 = 1;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const FLUSH_INTERVAL: Duration = Duration::from_secs(5);
const MAX_BATCH_EVENTS: usize = 50;

#[derive(Debug, Clone)]
pub struct TelemetryClient {
    state: Option<Arc<TelemetryState>>,
}

#[derive(Debug)]
struct TelemetryState {
    endpoint: String,
    install_id: String,
    session_id: String,
    next_event_sequence: AtomicU64,
    queue: Mutex<TelemetryQueue>,
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
        Self {
            state: Some(Arc::new(TelemetryState {
                endpoint: config.telemetry.endpoint.clone(),
                install_id,
                session_id: random_uuid_like(),
                next_event_sequence: AtomicU64::new(1),
                queue: Mutex::new(TelemetryQueue::default()),
                http,
            })),
        }
    }

    pub fn enabled(&self) -> bool {
        self.state.is_some()
    }

    pub fn spawn(&self, event: TelemetryEvent) {
        let Some(state) = self.state.clone() else {
            return;
        };
        tokio::spawn(async move {
            enqueue_event(state, event).await;
        });
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
        send_batch(state, events).await
    }

    pub async fn flush(&self) -> Result<(), TelemetryError> {
        let Some(state) = self.state.clone() else {
            return Ok(());
        };
        let events = drain_queued_events(&state).await;
        send_batch(state, events).await
    }
}

#[derive(Debug)]
pub enum TelemetryError {
    Http(reqwest::Error),
    Status(reqwest::StatusCode),
}

async fn enqueue_event(state: Arc<TelemetryState>, event: TelemetryEvent) {
    let action = {
        let mut queue = state.queue.lock().await;
        queue.events.push(event);
        if queue.events.len() >= MAX_BATCH_EVENTS {
            let events = std::mem::take(&mut queue.events);
            queue.flush_scheduled = false;
            TelemetryAction::Flush(events)
        } else if !queue.flush_scheduled {
            queue.flush_scheduled = true;
            TelemetryAction::Schedule
        } else {
            TelemetryAction::None
        }
    };

    match action {
        TelemetryAction::Flush(events) => {
            let _ = send_batch(state, events).await;
        }
        TelemetryAction::Schedule => {
            tokio::spawn(async move {
                time::sleep(FLUSH_INTERVAL).await;
                let events = drain_queued_events(&state).await;
                let _ = send_batch(state, events).await;
            });
        }
        TelemetryAction::None => {}
    }
}

#[derive(Debug)]
enum TelemetryAction {
    Flush(Vec<TelemetryEvent>),
    Schedule,
    None,
}

async fn drain_queued_events(state: &TelemetryState) -> Vec<TelemetryEvent> {
    let mut queue = state.queue.lock().await;
    queue.flush_scheduled = false;
    std::mem::take(&mut queue.events)
}

async fn send_batch(
    state: Arc<TelemetryState>,
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
        session_id: state.session_id.as_str(),
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TelemetryEventName {
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
    #[serde(rename = "squeezy_failure_seen")]
    FailureSeen,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelemetryProperties {
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
    pub refresh_kind: Option<RefreshKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_sequence_scope: Option<GraphSequenceScope>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<OutcomeStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<ErrorKind>,
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
            python_files: Some(report.language_distribution.python_files),
            rust_files: Some(report.language_distribution.rust_files),
            supported_files: Some(report.language_distribution.supported_files),
            unsupported_files: Some(report.language_distribution.unsupported_files),
            unknown_files: Some(report.language_distribution.unknown_files),
            files_changed: Some(report.files_changed),
            files_parsed: Some(report.files_parsed),
            bytes_parsed: Some(report.bytes_parsed),
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
    pub go_files: u64,
    pub python_files: u64,
    pub rust_files: u64,
    pub supported_files: u64,
    pub unsupported_files: u64,
    pub unknown_files: u64,
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
}

impl ProviderKind {
    fn from_provider(provider: &ProviderConfig) -> Self {
        match provider {
            ProviderConfig::OpenAi(_) => Self::OpenAi,
            ProviderConfig::Anthropic(_) => Self::Anthropic,
            ProviderConfig::Google(_) => Self::Google,
            ProviderConfig::AzureOpenAi(_) => Self::AzureOpenAi,
            ProviderConfig::Bedrock(_) => Self::Bedrock,
            ProviderConfig::Ollama(_) => Self::Ollama,
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
        let model = model.to_ascii_lowercase();
        if model.starts_with("gpt") || model.starts_with("o1") || model.starts_with("o3") {
            Self::Gpt
        } else if model.starts_with("claude") || matches!(provider, ProviderConfig::Anthropic(_)) {
            Self::Claude
        } else if model.starts_with("gemini") || matches!(provider, ProviderConfig::Google(_)) {
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
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    )
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

pub fn telemetry_config(enabled: bool, endpoint: impl Into<String>) -> TelemetryConfig {
    TelemetryConfig {
        enabled,
        endpoint: endpoint.into(),
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
