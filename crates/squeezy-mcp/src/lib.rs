use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    pin::Pin,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use futures_util::stream::{FuturesUnordered, StreamExt};
use http::{HeaderName, HeaderValue};
use rmcp::{
    ClientHandler, ServiceExt,
    model::{
        CallToolRequestParams, CancelledNotificationParam, ClientCapabilities, ClientInfo,
        CreateElicitationRequestParams, CreateElicitationResult, ElicitationAction, Implementation,
        JsonObject, LoggingLevel, LoggingMessageNotificationParam, PaginatedRequestParams,
        ProgressNotificationParam, ReadResourceRequestParams, Resource,
        ResourceUpdatedNotificationParam, ServerCapabilities, Tool as RmcpTool,
    },
    service::{NotificationContext, RequestContext, RoleClient},
    transport::{
        StreamableHttpClientTransport, TokioChildProcess,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use squeezy_core::{McpServerConfig, McpTransport, PermissionMode};
use squeezy_store::SqueezyStore;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::{Mutex as TokioMutex, watch},
};
use tokio_util::sync::CancellationToken;

mod sse;

const DEFAULT_MCP_TIMEOUT_MS: u64 = 30_000;

/// Timeout applied to MCP tool discovery (including the implicit session
/// bring-up on the first call). Falls back to `timeout_ms`, then to the
/// crate-wide default.
fn discovery_timeout_ms(server: &McpServerConfig) -> u64 {
    server
        .discovery_timeout_ms
        .or(server.timeout_ms)
        .unwrap_or(DEFAULT_MCP_TIMEOUT_MS)
}

/// Timeout applied to MCP tool invocations and follow-on requests (tool
/// calls, resource listing, resource reads). Falls back to `timeout_ms`,
/// then to the crate-wide default.
fn tool_call_timeout_ms(server: &McpServerConfig) -> u64 {
    server
        .tool_call_timeout_ms
        .or(server.timeout_ms)
        .unwrap_or(DEFAULT_MCP_TIMEOUT_MS)
}

const MCP_TOOL_CACHE_SCHEMA_VERSION: u64 = 1;
const MAX_MODEL_TOOL_NAME_BYTES: usize = 64;
const HASH_SUFFIX_BYTES: usize = 12;
const RESOURCE_READ_CACHE_TTL: Duration = Duration::from_secs(300);
const RESOURCE_DECLARATION_CACHE_TTL: Duration = Duration::from_secs(30);
/// Cap on retained resource-read cache entries. The registry lives for the
/// whole session, so without a bound a server (or agent loop) that reads many
/// distinct URIs would accumulate their full bodies indefinitely. Mirrors the
/// `MCP_AUDIT_LOG_CAPACITY` defense: prune expired entries, then evict the
/// oldest, so memory stays bounded by the working set.
const RESOURCE_READ_CACHE_CAPACITY: usize = 256;
const MAX_TOOL_SCHEMA_BYTES: usize = 4096;

pub type McpResult<T> = Result<T, McpError>;

type McpService = rmcp::service::RunningService<RoleClient, SqueezyMcpClientHandler>;
type McpElicitationFuture = Pin<Box<dyn Future<Output = McpElicitationResponse> + Send>>;
pub type McpElicitationHandler =
    Arc<dyn Fn(McpElicitationRequest) -> McpElicitationFuture + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("MCP server {server:?} is missing command for stdio transport")]
    MissingCommand { server: String },
    #[error("MCP server {server:?} is missing url for {transport} transport")]
    MissingUrl {
        server: String,
        transport: &'static str,
    },
    #[error("MCP server {server:?} timed out after {timeout_ms}ms")]
    Timeout { server: String, timeout_ms: u64 },
    #[error("MCP server {server:?} call was cancelled")]
    Cancelled { server: String },
    #[error("MCP tool {tool:?} expects object arguments")]
    InvalidArguments { tool: String },
    #[error("unknown MCP tool {tool:?}")]
    UnknownTool { tool: String },
    #[error("unknown MCP server {server:?}")]
    UnknownServer { server: String },
    #[error("MCP resource {uri:?} is not declared by server {server:?}")]
    UndeclaredResource { server: String, uri: String },
    #[error("MCP server {server:?} streamable HTTP session expired")]
    SessionExpired { server: String },
    #[error("MCP server {server:?}: {message}")]
    Transport { server: String, message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalMcpTool {
    pub server: String,
    pub raw_name: String,
    pub model_name: String,
    pub description: String,
    pub parameters: Value,
    pub transport: McpTransport,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalMcpToolResult {
    pub server: String,
    pub raw_name: String,
    pub model_name: String,
    pub is_error: bool,
    pub content: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpServerStatus {
    Starting,
    Ready {
        tools_count: usize,
        cached: bool,
    },
    Stale {
        tools_count: usize,
        outcome: McpStaleOutcome,
    },
    Failed {
        error: String,
    },
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpStaleOutcome {
    Failed { error: String },
    Cancelled,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpStatusSnapshot {
    pub per_server: BTreeMap<String, McpServerStatus>,
    pub generated_unix_millis: u128,
}

/// Aggregated discovery statistics for product telemetry.
/// Plain data — no `squeezy-telemetry` dependency. The agent
/// converts this into a `McpDiscoveryReport` and fires the event.
#[derive(Debug, Clone, Default)]
pub struct McpDiscoveryStats {
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
    /// Coarse error kind tokens: `"timeout"`, `"transport"`, `"cancelled"`.
    pub error_kind_tokens: Vec<String>,
    pub has_resources: bool,
    pub has_elicitation: bool,
    pub has_experimental: bool,
    pub duration_ms: u64,
}

#[derive(Debug, Clone)]
pub struct McpRefreshOutcome {
    pub errors: Vec<String>,
    pub status: McpStatusSnapshot,
    pub discovery_stats: Option<McpDiscoveryStats>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpElicitationKind {
    Form,
    Url,
}

impl McpElicitationKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Form => "form",
            Self::Url => "url",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpElicitationRequest {
    pub server: String,
    pub request_id: String,
    pub kind: McpElicitationKind,
    pub message: String,
    pub schema: Option<Value>,
    pub url: Option<String>,
    pub elicitation_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpElicitationAction {
    Accept,
    Decline,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpElicitationResponse {
    pub action: McpElicitationAction,
    pub content: Option<Value>,
}

impl McpElicitationResponse {
    pub fn accept(content: Option<Value>) -> Self {
        Self {
            action: McpElicitationAction::Accept,
            content,
        }
    }

    pub fn decline() -> Self {
        Self {
            action: McpElicitationAction::Decline,
            content: None,
        }
    }

    pub fn cancel() -> Self {
        Self {
            action: McpElicitationAction::Cancel,
            content: None,
        }
    }
}

/// Outcome of a single elicitation policy check, surfaced for audit/UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpElicitationAuditOutcome {
    /// Policy + content allowed silent acceptance; no user was prompted.
    AutoAccepted,
    /// Policy denied without prompting.
    AutoDeclined,
    /// Forwarded to the host handler (UI) for a user decision.
    Forwarded,
}

impl McpElicitationAuditOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AutoAccepted => "auto_accepted",
            Self::AutoDeclined => "auto_declined",
            Self::Forwarded => "forwarded",
        }
    }
}

/// Record emitted every time the MCP client takes an elicitation decision.
///
/// Provides a structured audit trail so a malicious server spamming empty
/// `Form` elicitations cannot silently flip behavior — each decision is
/// observable from the host without scraping `tracing` logs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpElicitationAuditEvent {
    pub server: String,
    pub request_id: String,
    pub kind: McpElicitationKind,
    pub policy: PermissionMode,
    pub outcome: McpElicitationAuditOutcome,
    pub unix_millis: u128,
}

/// Cap on retained audit entries. Older records are dropped FIFO; this is
/// purely a defense against runaway memory if a misbehaving server floods
/// elicitations — the host is expected to drain via `audit_log_snapshot`.
const MCP_AUDIT_LOG_CAPACITY: usize = 256;

#[derive(Clone)]
pub struct McpClientRegistry {
    /// Live server map. Wrapped in `RwLock<Arc<…>>` so reads stay cheap
    /// (clone the `Arc` under a short lock, then release it before any
    /// `.await`) while admin mutations (`set_server_enabled`,
    /// `restart_server`, `replace_servers`) can swap in a new map mid-
    /// session without rebuilding the whole `McpClientRegistry`.
    servers: Arc<std::sync::RwLock<Arc<BTreeMap<String, McpServerConfig>>>>,
    cache: Arc<Mutex<BTreeMap<String, ExternalMcpTool>>>,
    sessions: Arc<TokioMutex<BTreeMap<String, Arc<SessionEntry>>>>,
    store: Option<Arc<SqueezyStore>>,
    status_tx: watch::Sender<McpStatusSnapshot>,
    elicitation_handler: Arc<Mutex<Option<McpElicitationHandler>>>,
    /// Approval gate consulted before auto-accepting an elicitation. `Ask`
    /// (the conservative default) forces every request through the host
    /// handler; `Allow` keeps the historical fast-path for empty-form
    /// confirmations; `Deny` short-circuits to a decline so a misbehaving
    /// server cannot block the agent waiting on user input.
    elicitation_policy: Arc<Mutex<PermissionMode>>,
    elicitation_audit: Arc<Mutex<std::collections::VecDeque<McpElicitationAuditEvent>>>,
    pause_state: ElicitationPauseState,
    resource_reads: Arc<Mutex<BTreeMap<(String, String), CachedResourceRead>>>,
    resource_declarations: Arc<Mutex<BTreeMap<String, CachedResourceDeclarations>>>,
}

impl Default for McpClientRegistry {
    fn default() -> Self {
        Self::new(BTreeMap::new())
    }
}

impl McpClientRegistry {
    pub fn new(servers: BTreeMap<String, McpServerConfig>) -> Self {
        Self::new_with_store(servers, None)
    }

    pub fn new_with_store(
        servers: BTreeMap<String, McpServerConfig>,
        store: Option<Arc<SqueezyStore>>,
    ) -> Self {
        let (status_tx, _) = watch::channel(McpStatusSnapshot::default());
        let registry = Self {
            servers: Arc::new(std::sync::RwLock::new(Arc::new(servers))),
            cache: Arc::new(Mutex::new(BTreeMap::new())),
            sessions: Arc::new(TokioMutex::new(BTreeMap::new())),
            store,
            status_tx,
            elicitation_handler: Arc::new(Mutex::new(None)),
            elicitation_policy: Arc::new(Mutex::new(PermissionMode::Ask)),
            elicitation_audit: Arc::new(Mutex::new(std::collections::VecDeque::with_capacity(
                MCP_AUDIT_LOG_CAPACITY,
            ))),
            pause_state: ElicitationPauseState::default(),
            resource_reads: Arc::new(Mutex::new(BTreeMap::new())),
            resource_declarations: Arc::new(Mutex::new(BTreeMap::new())),
        };
        registry.load_cached_tools();
        registry
    }

    /// Clone the current `Arc<BTreeMap<…>>` of configured servers under a
    /// short read lock. Callers should hold this `Arc` across `.await`
    /// points instead of repeatedly reaching back into `self.servers`,
    /// both to avoid lock contention and to keep the iteration stable if
    /// a concurrent `replace_servers` swaps the inner map.
    fn servers_snapshot(&self) -> Arc<BTreeMap<String, McpServerConfig>> {
        self.servers
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_else(|poison| poison.into_inner().clone())
    }

    /// Snapshot of the configured MCP servers — useful for surfaces (the
    /// `/mcp` config page, eval drivers) that need to render the live
    /// server map without going through `AppConfig`.
    pub fn servers(&self) -> BTreeMap<String, McpServerConfig> {
        (*self.servers_snapshot()).clone()
    }

    pub fn has_no_enabled_servers(&self) -> bool {
        self.servers_snapshot()
            .iter()
            .all(|(_, server)| !server.enabled)
    }

    pub fn tools(&self) -> Vec<ExternalMcpTool> {
        self.cache
            .lock()
            .map(|cache| cache.values().cloned().collect())
            .unwrap_or_default()
    }

    pub fn tool(&self, model_name: &str) -> Option<ExternalMcpTool> {
        self.cache
            .lock()
            .ok()
            .and_then(|cache| cache.get(model_name).cloned())
    }

    pub fn set_elicitation_handler(&self, handler: Option<McpElicitationHandler>) {
        if let Ok(mut slot) = self.elicitation_handler.lock() {
            *slot = handler;
        }
    }

    /// Update the approval gate that decides how MCP server elicitations are
    /// handled. Callers should plumb their host policy here (typically the
    /// `permissions.mcp` mode) so a malicious server cannot bypass user
    /// consent by spamming empty `Form` elicitations.
    pub fn set_elicitation_policy(&self, policy: PermissionMode) {
        if let Ok(mut slot) = self.elicitation_policy.lock() {
            *slot = policy;
        }
    }

    pub fn elicitation_policy(&self) -> PermissionMode {
        self.elicitation_policy
            .lock()
            .map(|policy| *policy)
            .unwrap_or(PermissionMode::Ask)
    }

    /// Snapshot of the most recent elicitation decisions (oldest first).
    /// Intended for audit surfaces; the underlying ring is capped at
    /// `MCP_AUDIT_LOG_CAPACITY` so a flood cannot exhaust memory.
    pub fn elicitation_audit_log(&self) -> Vec<McpElicitationAuditEvent> {
        self.elicitation_audit
            .lock()
            .map(|log| log.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Drain and return all elicitation audit events since the last drain.
    /// Unlike `elicitation_audit_log`, this clears the ring so subsequent
    /// calls return only new events — preventing per-turn re-emission of the
    /// same decisions.
    pub fn drain_elicitation_audit_log(&self) -> Vec<McpElicitationAuditEvent> {
        self.elicitation_audit
            .lock()
            .map(|mut log| std::mem::take(&mut *log).into_iter().collect())
            .unwrap_or_default()
    }

    pub fn status_snapshot(&self) -> McpStatusSnapshot {
        self.status_tx.borrow().clone()
    }

    pub fn status_watch(&self) -> watch::Receiver<McpStatusSnapshot> {
        self.status_tx.subscribe()
    }

    pub async fn refresh_tools(&self, cancel: CancellationToken) -> McpRefreshOutcome {
        let discovery_started = Instant::now();
        let prior_cache = self
            .cache
            .lock()
            .map(|cache| cache.clone())
            .unwrap_or_default();

        // Build per-transport server counts up-front.
        let mut stats = McpDiscoveryStats::default();
        let current_servers = self.servers_snapshot();
        for server in current_servers.values() {
            if server.enabled {
                stats.servers_enabled += 1;
                match server.transport {
                    McpTransport::Stdio => stats.servers_stdio += 1,
                    McpTransport::Http => stats.servers_http += 1,
                    McpTransport::Sse => stats.servers_sse += 1,
                }
            } else {
                stats.servers_disabled += 1;
            }
        }

        if self.has_no_enabled_servers() {
            if let Ok(mut cache) = self.cache.lock() {
                cache.clear();
            }
            let status = McpStatusSnapshot {
                per_server: BTreeMap::new(),
                generated_unix_millis: unix_millis(),
            };
            self.publish_status(status.clone());
            stats.duration_ms = discovery_started.elapsed().as_millis() as u64;
            return McpRefreshOutcome {
                errors: Vec::new(),
                status,
                discovery_stats: Some(stats),
            };
        }

        let servers = self.servers_snapshot();
        let starting = starting_status_snapshot(&servers, self.status_snapshot().per_server);
        self.publish_status(McpStatusSnapshot {
            per_server: starting,
            generated_unix_millis: unix_millis(),
        });

        let mut futures = FuturesUnordered::new();
        for (server_name, server) in servers.iter() {
            if !server.enabled {
                continue;
            }
            let cancel = cancel.clone();
            let registry = self.clone();
            let name = server_name.clone();
            let server = server.clone();
            futures.push(async move {
                let result = registry.discover_one(&name, &server, cancel).await;
                (name, result)
            });
        }

        let mut raw_tools = Vec::new();
        let mut succeeded: BTreeSet<String> = BTreeSet::new();
        let mut per_server = BTreeMap::new();
        let mut errors = Vec::new();
        while let Some((name, result)) = futures.next().await {
            match result {
                Ok(tools) => {
                    stats.tools_discovered += tools.len() as u32;
                    self.write_tool_cache(&name, &tools);
                    per_server.insert(
                        name.clone(),
                        McpServerStatus::Ready {
                            tools_count: tools.len(),
                            cached: false,
                        },
                    );
                    succeeded.insert(name);
                    raw_tools.extend(tools);
                }
                Err(error) => {
                    tracing::warn!(
                        target: "squeezy::mcp",
                        server = %name,
                        error = %error,
                        "failed to discover MCP tools"
                    );
                    stats.discovery_errors += 1;
                    let error_kind = match &error {
                        McpError::Timeout { .. } => "timeout",
                        McpError::Cancelled { .. } => "cancelled",
                        _ => "transport",
                    };
                    stats.error_kind_tokens.push(error_kind.to_string());
                    let status = if matches!(error, McpError::Cancelled { .. }) {
                        McpServerStatus::Cancelled
                    } else {
                        McpServerStatus::Failed {
                            error: error.to_string(),
                        }
                    };
                    per_server.insert(name, status);
                    errors.push(error.to_string());
                }
            }
        }

        let mut next = normalize_palette(raw_tools);
        let mut cached_tool_counts = BTreeMap::<String, usize>::new();
        for tool in prior_cache.values() {
            *cached_tool_counts.entry(tool.server.clone()).or_default() += 1;
        }

        for (model_name, tool) in &prior_cache {
            let server_still_enabled = servers
                .get(&tool.server)
                .map(|server| server.enabled)
                .unwrap_or(false);
            if server_still_enabled
                && !succeeded.contains(&tool.server)
                && !next.contains_key(model_name)
            {
                stats.tools_stale_retained += 1;
                next.insert(model_name.clone(), tool.clone());
                let tools_count = cached_tool_counts
                    .get(&tool.server)
                    .copied()
                    .unwrap_or_default();
                match per_server.get(&tool.server).cloned() {
                    Some(McpServerStatus::Failed { error }) => {
                        per_server.insert(
                            tool.server.clone(),
                            McpServerStatus::Stale {
                                tools_count,
                                outcome: McpStaleOutcome::Failed { error },
                            },
                        );
                    }
                    Some(McpServerStatus::Cancelled) => {
                        per_server.insert(
                            tool.server.clone(),
                            McpServerStatus::Stale {
                                tools_count,
                                outcome: McpStaleOutcome::Cancelled,
                            },
                        );
                    }
                    Some(McpServerStatus::Stale { .. }) => {}
                    _ => {
                        per_server.insert(
                            tool.server.clone(),
                            McpServerStatus::Ready {
                                tools_count,
                                cached: true,
                            },
                        );
                    }
                }
            } else if !server_still_enabled {
                stats.tools_dropped_disabled += 1;
            }
        }
        // Count cached tool entries (tools loaded from persisted cache on startup).
        stats.tools_cached = prior_cache.len() as u32;

        if let Ok(mut cache) = self.cache.lock() {
            *cache = next;
        }

        for (name, server) in servers.iter() {
            if !server.enabled {
                continue;
            }
            per_server
                .entry(name.clone())
                .or_insert(McpServerStatus::Failed {
                    error: "discovery did not complete".to_string(),
                });
        }
        let status = McpStatusSnapshot {
            per_server,
            generated_unix_millis: unix_millis(),
        };
        self.publish_status(status.clone());
        stats.duration_ms = discovery_started.elapsed().as_millis() as u64;
        McpRefreshOutcome {
            errors,
            status,
            discovery_stats: Some(stats),
        }
    }

    /// Toggle the `enabled` flag for a single configured server and
    /// re-run tool discovery. Returns the refresh outcome (the same
    /// shape `refresh_tools` produces) so callers can surface
    /// per-server status without an extra round-trip.
    ///
    /// If `enabled = false` the existing session is torn down so the
    /// child process / HTTP keep-alive does not linger after the user
    /// has switched the server off. Unknown server names return a
    /// `UnknownServer` error without touching the live map.
    pub async fn set_server_enabled(
        &self,
        server_name: &str,
        enabled: bool,
        cancel: CancellationToken,
    ) -> McpResult<McpRefreshOutcome> {
        let prior = self.servers_snapshot();
        if !prior.contains_key(server_name) {
            return Err(McpError::UnknownServer {
                server: server_name.to_string(),
            });
        }
        if prior.get(server_name).map(|s| s.enabled) == Some(enabled) {
            // No-op: still run a refresh so callers see an updated
            // status snapshot, but skip the map swap and session
            // teardown.
            return Ok(self.refresh_tools(cancel).await);
        }
        let mut next: BTreeMap<String, McpServerConfig> = (*prior).clone();
        if let Some(server) = next.get_mut(server_name) {
            server.enabled = enabled;
        }
        self.swap_servers(Arc::new(next));
        if !enabled {
            self.invalidate_session(server_name).await;
        }
        Ok(self.refresh_tools(cancel).await)
    }

    /// Tear down the live session for `server_name` (if any) and re-run
    /// tool discovery. The next discovery call brings up a fresh
    /// child process / HTTP session, which is what a user means when
    /// they ask the `/mcp` page to "restart" a server.
    pub async fn restart_server(
        &self,
        server_name: &str,
        cancel: CancellationToken,
    ) -> McpResult<McpRefreshOutcome> {
        if !self.servers_snapshot().contains_key(server_name) {
            return Err(McpError::UnknownServer {
                server: server_name.to_string(),
            });
        }
        self.invalidate_session(server_name).await;
        Ok(self.refresh_tools(cancel).await)
    }

    /// Replace the entire configured-server map and refresh tool
    /// discovery. Used for bulk operations (add/remove from the
    /// `/mcp` config page; reacting to an external `settings.toml`
    /// edit). Sessions whose servers vanish or whose config changes
    /// are dropped so the next call recreates them against the new
    /// config.
    pub async fn replace_servers(
        &self,
        servers: BTreeMap<String, McpServerConfig>,
        cancel: CancellationToken,
    ) -> McpRefreshOutcome {
        let prior = self.servers_snapshot();
        let next = Arc::new(servers);
        // Determine which sessions to drop *before* swapping so we
        // do not race against an in-flight discovery using the new
        // map.
        let mut to_invalidate: Vec<String> = Vec::new();
        for (name, prev_server) in prior.iter() {
            match next.get(name) {
                None => to_invalidate.push(name.clone()),
                Some(new_server) if new_server != prev_server => {
                    to_invalidate.push(name.clone());
                }
                _ => {}
            }
        }
        self.swap_servers(next);
        for name in to_invalidate {
            self.invalidate_session(&name).await;
        }
        self.refresh_tools(cancel).await
    }

    /// Replace the inner `Arc<BTreeMap<…>>`. Held in a tiny helper so
    /// every mutator goes through the same lock-acquisition path; if
    /// the lock is poisoned we recover the inner data rather than
    /// panic — the registry must stay usable across config edits.
    fn swap_servers(&self, next: Arc<BTreeMap<String, McpServerConfig>>) {
        match self.servers.write() {
            Ok(mut guard) => {
                *guard = next;
            }
            Err(poison) => {
                *poison.into_inner() = next;
            }
        }
    }

    pub async fn call_tool(
        &self,
        model_name: &str,
        arguments: Value,
        cancel: CancellationToken,
    ) -> McpResult<ExternalMcpToolResult> {
        let tool = self.tool(model_name).ok_or_else(|| McpError::UnknownTool {
            tool: model_name.to_string(),
        })?;
        let server = self
            .servers_snapshot()
            .get(&tool.server)
            .ok_or_else(|| McpError::UnknownTool {
                tool: model_name.to_string(),
            })?
            .clone();
        let args = arguments_object(&tool.model_name, arguments)?;
        let result = self
            .call_one(&tool.server, &server, &tool.raw_name, args, cancel)
            .await?;
        Ok(ExternalMcpToolResult {
            server: tool.server,
            raw_name: tool.raw_name,
            model_name: tool.model_name,
            is_error: result
                .get("isError")
                .and_then(Value::as_bool)
                .or_else(|| result.get("is_error").and_then(Value::as_bool))
                .unwrap_or(false),
            content: strip_untrusted_meta(result),
        })
    }

    pub async fn list_resources(
        &self,
        server_name: &str,
        cursor: Option<String>,
        cancel: CancellationToken,
    ) -> McpResult<Value> {
        let server = self.server_config(server_name)?;
        let result = self
            .list_resources_page(server_name, &server, cursor, cancel)
            .await?;
        serde_json::to_value(result).map_err(|err| McpError::Transport {
            server: server_name.to_string(),
            message: err.to_string(),
        })
    }

    pub async fn list_resource_templates(
        &self,
        server_name: &str,
        cursor: Option<String>,
        cancel: CancellationToken,
    ) -> McpResult<Value> {
        let server = self.server_config(server_name)?;
        let result = self
            .list_resource_templates_page(server_name, &server, cursor, cancel)
            .await?;
        serde_json::to_value(result).map_err(|err| McpError::Transport {
            server: server_name.to_string(),
            message: err.to_string(),
        })
    }

    pub async fn read_resource(
        &self,
        server_name: &str,
        uri: &str,
        cancel: CancellationToken,
    ) -> McpResult<Value> {
        let key = (server_name.to_string(), uri.to_string());
        if let Ok(cache) = self.resource_reads.lock()
            && let Some(cached) = cache.get(&key)
            && cached.fetched_at.elapsed() <= RESOURCE_READ_CACHE_TTL
        {
            return Ok(cached.value.clone());
        }

        let server = self.server_config(server_name)?;
        if !self
            .resource_uri_is_declared(server_name, &server, uri, cancel.clone())
            .await?
        {
            return Err(McpError::UndeclaredResource {
                server: server_name.to_string(),
                uri: uri.to_string(),
            });
        }
        let timeout_ms = tool_call_timeout_ms(&server);
        let registry = self.clone();
        let server_name_owned = server_name.to_string();
        let server_for_call = server.clone();
        let uri_owned = uri.to_string();
        let result = with_timeout(
            server_name,
            timeout_ms,
            cancel,
            self.pause_state.clone(),
            async move {
                let service = registry
                    .session_for(&server_name_owned, &server_for_call)
                    .await?;
                let response = service
                    .read_resource(ReadResourceRequestParams::new(uri_owned))
                    .await
                    .map_err(|err| service_error_to_mcp(&server_name_owned, err))?;
                serde_json::to_value(response).map_err(|err| McpError::Transport {
                    server: server_name_owned.clone(),
                    message: err.to_string(),
                })
            },
        )
        .await?;
        let result = strip_untrusted_meta(result);
        if let Ok(mut cache) = self.resource_reads.lock() {
            insert_resource_read(
                &mut cache,
                key,
                CachedResourceRead {
                    value: result.clone(),
                    fetched_at: Instant::now(),
                },
            );
        }
        Ok(result)
    }

    async fn discover_one(
        &self,
        server_name: &str,
        server: &McpServerConfig,
        cancel: CancellationToken,
    ) -> McpResult<Vec<ExternalMcpTool>> {
        let timeout_ms = discovery_timeout_ms(server);
        let registry = self.clone();
        let server_for_call = server.clone();
        let server_name_owned = server_name.to_string();
        let result = with_timeout(
            server_name,
            timeout_ms,
            cancel,
            self.pause_state.clone(),
            async move {
                let service = registry
                    .session_for(&server_name_owned, &server_for_call)
                    .await?;
                let tools = service
                    .list_all_tools()
                    .await
                    .map_err(|err| service_error_to_mcp(&server_name_owned, err))?;
                Ok(tools)
            },
        )
        .await;
        match result {
            Ok(tools) => Ok(convert_tools(server_name, server, tools)),
            Err(error) => {
                self.invalidate_session(server_name).await;
                Err(error)
            }
        }
    }

    async fn call_one(
        &self,
        server_name: &str,
        server: &McpServerConfig,
        tool_name: &str,
        arguments: JsonObject,
        cancel: CancellationToken,
    ) -> McpResult<Value> {
        let result = self
            .call_one_once(
                server_name,
                server,
                tool_name,
                arguments.clone(),
                cancel.clone(),
            )
            .await;
        match result {
            Err(McpError::SessionExpired { .. })
                if matches!(server.transport, McpTransport::Http | McpTransport::Sse) =>
            {
                self.invalidate_session(server_name).await;
                self.call_one_once(server_name, server, tool_name, arguments, cancel)
                    .await
            }
            Err(error) => {
                self.invalidate_session(server_name).await;
                Err(error)
            }
            ok => ok,
        }
    }

    async fn call_one_once(
        &self,
        server_name: &str,
        server: &McpServerConfig,
        tool_name: &str,
        arguments: JsonObject,
        cancel: CancellationToken,
    ) -> McpResult<Value> {
        let timeout_ms = tool_call_timeout_ms(server);
        let registry = self.clone();
        let server_for_call = server.clone();
        let server_name_owned = server_name.to_string();
        let tool_name_owned = tool_name.to_string();
        with_timeout(
            server_name,
            timeout_ms,
            cancel,
            self.pause_state.clone(),
            async move {
                let service = registry
                    .session_for(&server_name_owned, &server_for_call)
                    .await?;
                let response = service
                    .call_tool(
                        CallToolRequestParams::new(tool_name_owned).with_arguments(arguments),
                    )
                    .await
                    .map_err(|err| service_error_to_mcp(&server_name_owned, err))?;
                serde_json::to_value(response).map_err(|err| McpError::Transport {
                    server: server_name_owned.clone(),
                    message: err.to_string(),
                })
            },
        )
        .await
    }

    async fn session_for(
        &self,
        server_name: &str,
        server: &McpServerConfig,
    ) -> McpResult<Arc<McpService>> {
        {
            let sessions = self.sessions.lock().await;
            if let Some(entry) = sessions.get(server_name) {
                return Ok(entry.service.clone());
            }
        }

        let handler = SqueezyMcpClientHandler {
            server_name: server_name.to_string(),
            elicitation_handler: self.elicitation_handler.clone(),
            elicitation_policy: self.elicitation_policy.clone(),
            elicitation_audit: self.elicitation_audit.clone(),
            pause_state: self.pause_state.clone(),
            resource_reads: self.resource_reads.clone(),
            resource_declarations: self.resource_declarations.clone(),
        };
        let entry = match server.transport {
            McpTransport::Stdio => start_stdio_service(server_name, server, handler).await?,
            McpTransport::Http => start_http_service(server_name, server, handler).await?,
            McpTransport::Sse => start_sse_service(server_name, server, handler).await?,
        };
        let arc = Arc::new(entry);
        let mut sessions = self.sessions.lock().await;
        if let Some(existing) = sessions.get(server_name) {
            return Ok(existing.service.clone());
        }
        sessions.insert(server_name.to_string(), arc.clone());
        Ok(arc.service.clone())
    }

    async fn invalidate_session(&self, server_name: &str) {
        let mut sessions = self.sessions.lock().await;
        sessions.remove(server_name);
        if let Ok(mut cache) = self.resource_declarations.lock() {
            cache.remove(server_name);
        }
    }

    pub async fn shutdown(&self) {
        let mut sessions = self.sessions.lock().await;
        sessions.clear();
        if let Ok(mut cache) = self.resource_declarations.lock() {
            cache.clear();
        }
    }

    /// Server-advertised capabilities captured during `initialize`, or `None`
    /// if no session is currently held for `server_name`. Returns the live
    /// `ServerCapabilities` so callers can inspect `experimental` flags (e.g.
    /// `claude/channel/permission`-style opt-ins) without re-handshaking.
    /// Callers must drive a connection first — either via `refresh_tools` or
    /// any other path that runs `session_for` — before this returns `Some`.
    pub async fn server_capabilities(&self, server_name: &str) -> Option<ServerCapabilities> {
        let sessions = self.sessions.lock().await;
        sessions
            .get(server_name)
            .and_then(|entry| entry.server_capabilities.clone())
    }

    /// Aggregate capability presence booleans across all connected servers.
    /// Returns (has_resources, has_elicitation, has_experimental).
    pub async fn aggregate_capabilities(&self) -> (bool, bool, bool) {
        let sessions = self.sessions.lock().await;
        let mut has_resources = false;
        // Squeezy advertises client-side elicitation support during
        // initialize, so this means "a connected session can ask us to
        // elicit" rather than "a server declared an elicitation capability."
        let has_elicitation = !sessions.is_empty();
        let mut has_experimental = false;
        for entry in sessions.values() {
            if let Some(caps) = &entry.server_capabilities {
                if caps.resources.is_some() {
                    has_resources = true;
                }
                if caps.experimental.is_some() {
                    has_experimental = true;
                }
            }
        }
        (has_resources, has_elicitation, has_experimental)
    }

    #[doc(hidden)]
    pub fn insert_cached_tool_for_test(&self, tool: ExternalMcpTool) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(tool.model_name.clone(), tool);
        }
    }

    #[cfg(test)]
    fn seed_resource_read_for_test(&self, server: &str, uri: &str, value: Value) {
        if let Ok(mut cache) = self.resource_reads.lock() {
            cache.insert(
                (server.to_string(), uri.to_string()),
                CachedResourceRead {
                    value,
                    fetched_at: Instant::now(),
                },
            );
        }
    }

    #[cfg(test)]
    fn cached_resource_read_for_test(&self, server: &str, uri: &str) -> Option<Value> {
        self.resource_reads.lock().ok().and_then(|cache| {
            cache
                .get(&(server.to_string(), uri.to_string()))
                .map(|c| c.value.clone())
        })
    }

    #[cfg(test)]
    fn seed_resource_declarations_for_test(
        &self,
        server: &str,
        resource_uris: &[&str],
        resource_templates: &[&str],
    ) {
        if let Ok(mut cache) = self.resource_declarations.lock() {
            cache.insert(
                server.to_string(),
                CachedResourceDeclarations {
                    resource_uris: resource_uris.iter().map(|uri| uri.to_string()).collect(),
                    resource_templates: resource_templates
                        .iter()
                        .map(|template| template.to_string())
                        .collect(),
                    resource_uris_complete: true,
                    resource_templates_complete: true,
                    fetched_at: Instant::now(),
                },
            );
        }
    }

    #[cfg(test)]
    fn cached_resource_declarations_for_test(&self, server: &str) -> bool {
        self.resource_declarations
            .lock()
            .is_ok_and(|cache| cache.contains_key(server))
    }

    #[cfg(test)]
    fn client_handler_for_test(&self, server_name: &str) -> SqueezyMcpClientHandler {
        SqueezyMcpClientHandler {
            server_name: server_name.to_string(),
            elicitation_handler: self.elicitation_handler.clone(),
            elicitation_policy: self.elicitation_policy.clone(),
            elicitation_audit: self.elicitation_audit.clone(),
            pause_state: self.pause_state.clone(),
            resource_reads: self.resource_reads.clone(),
            resource_declarations: self.resource_declarations.clone(),
        }
    }

    fn server_config(&self, server_name: &str) -> McpResult<McpServerConfig> {
        self.servers_snapshot()
            .get(server_name)
            .filter(|server| server.enabled)
            .cloned()
            .ok_or_else(|| McpError::UnknownServer {
                server: server_name.to_string(),
            })
    }

    fn publish_status(&self, snapshot: McpStatusSnapshot) {
        self.status_tx.send_replace(snapshot);
    }

    fn load_cached_tools(&self) {
        let Some(store) = &self.store else {
            return;
        };
        let mut raw_tools = Vec::new();
        let mut status = BTreeMap::new();
        let servers = self.servers_snapshot();
        for (name, server) in servers.iter().filter(|(_, server)| server.enabled) {
            let key = tool_cache_key(name, server);
            let Ok(Some(record)) = store.mcp_tool_cache::<McpToolCacheRecord>(&key) else {
                continue;
            };
            if record.schema_version != MCP_TOOL_CACHE_SCHEMA_VERSION {
                continue;
            }
            status.insert(
                name.clone(),
                McpServerStatus::Ready {
                    tools_count: record.tools.len(),
                    cached: true,
                },
            );
            raw_tools.extend(record.tools);
        }
        if !raw_tools.is_empty() {
            if let Ok(mut cache) = self.cache.lock() {
                *cache = normalize_palette(raw_tools);
            }
            self.publish_status(McpStatusSnapshot {
                per_server: status,
                generated_unix_millis: unix_millis(),
            });
        }
    }

    fn write_tool_cache(&self, server_name: &str, tools: &[ExternalMcpTool]) {
        let Some(store) = &self.store else {
            return;
        };
        let servers = self.servers_snapshot();
        let Some(server) = servers.get(server_name) else {
            return;
        };
        let record = McpToolCacheRecord {
            schema_version: MCP_TOOL_CACHE_SCHEMA_VERSION,
            fetched_unix_millis: unix_millis(),
            tools: tools.to_vec(),
        };
        if let Err(error) = store.put_mcp_tool_cache(&tool_cache_key(server_name, server), &record)
        {
            tracing::warn!(
                target: "squeezy::mcp",
                server = %server_name,
                error = %error,
                "failed to write MCP tool cache"
            );
        }
    }

    async fn list_resources_page(
        &self,
        server_name: &str,
        server: &McpServerConfig,
        cursor: Option<String>,
        cancel: CancellationToken,
    ) -> McpResult<rmcp::model::ListResourcesResult> {
        let timeout_ms = tool_call_timeout_ms(server);
        let registry = self.clone();
        let server_name_owned = server_name.to_string();
        let server_for_call = server.clone();
        with_timeout(
            server_name,
            timeout_ms,
            cancel,
            self.pause_state.clone(),
            async move {
                let service = registry
                    .session_for(&server_name_owned, &server_for_call)
                    .await?;
                service
                    .list_resources(Some(PaginatedRequestParams::default().with_cursor(cursor)))
                    .await
                    .map_err(|err| service_error_to_mcp(&server_name_owned, err))
            },
        )
        .await
    }

    async fn list_resource_templates_page(
        &self,
        server_name: &str,
        server: &McpServerConfig,
        cursor: Option<String>,
        cancel: CancellationToken,
    ) -> McpResult<rmcp::model::ListResourceTemplatesResult> {
        let timeout_ms = tool_call_timeout_ms(server);
        let registry = self.clone();
        let server_name_owned = server_name.to_string();
        let server_for_call = server.clone();
        with_timeout(
            server_name,
            timeout_ms,
            cancel,
            self.pause_state.clone(),
            async move {
                let service = registry
                    .session_for(&server_name_owned, &server_for_call)
                    .await?;
                service
                    .list_resource_templates(Some(
                        PaginatedRequestParams::default().with_cursor(cursor),
                    ))
                    .await
                    .map_err(|err| service_error_to_mcp(&server_name_owned, err))
            },
        )
        .await
    }

    async fn all_resources(
        &self,
        server_name: &str,
        server: &McpServerConfig,
        cancel: CancellationToken,
    ) -> McpResult<Vec<Resource>> {
        let mut collected = Vec::new();
        let mut cursor = None;
        loop {
            let result = self
                .list_resources_page(server_name, server, cursor.clone(), cancel.clone())
                .await?;
            collected.extend(result.resources);
            match result.next_cursor {
                Some(next) if cursor.as_ref() == Some(&next) => {
                    return Err(McpError::Transport {
                        server: server_name.to_string(),
                        message: "resources/list returned duplicate cursor".to_string(),
                    });
                }
                Some(next) => cursor = Some(next),
                None => return Ok(collected),
            }
        }
    }

    async fn all_resource_templates(
        &self,
        server_name: &str,
        server: &McpServerConfig,
        cancel: CancellationToken,
    ) -> McpResult<Vec<rmcp::model::ResourceTemplate>> {
        let mut collected = Vec::new();
        let mut cursor = None;
        loop {
            let result = self
                .list_resource_templates_page(server_name, server, cursor.clone(), cancel.clone())
                .await?;
            collected.extend(result.resource_templates);
            match result.next_cursor {
                Some(next) if cursor.as_ref() == Some(&next) => {
                    return Err(McpError::Transport {
                        server: server_name.to_string(),
                        message: "resources/templates/list returned duplicate cursor".to_string(),
                    });
                }
                Some(next) => cursor = Some(next),
                None => return Ok(collected),
            }
        }
    }

    async fn resource_uri_is_declared(
        &self,
        server_name: &str,
        server: &McpServerConfig,
        uri: &str,
        cancel: CancellationToken,
    ) -> McpResult<bool> {
        if let Some(cached) = self.cached_resource_declarations_match(server_name, uri) {
            return Ok(cached);
        }

        let resources_result = self
            .all_resources(server_name, server, cancel.clone())
            .await;
        let resources = resources_result.as_ref().map(Vec::as_slice).unwrap_or(&[]);
        if let Ok(resources) = &resources_result {
            self.store_resource_declarations_partial(server_name, Some(resources), None);
            if resources.iter().any(|resource| resource.raw.uri == uri) {
                return Ok(true);
            }
        }

        let templates_result = self
            .all_resource_templates(server_name, server, cancel)
            .await;
        let templates = templates_result.as_ref().map(Vec::as_slice).unwrap_or(&[]);

        if let Ok(templates) = &templates_result {
            self.store_resource_declarations_partial(server_name, None, Some(templates));
        }

        Ok(resources.iter().any(|resource| resource.raw.uri == uri)
            || templates
                .iter()
                .any(|template| uri_matches_template(uri, &template.raw.uri_template)))
    }

    fn cached_resource_declarations_match(&self, server_name: &str, uri: &str) -> Option<bool> {
        let mut cache = self.resource_declarations.lock().ok()?;
        let cached = cache.get(server_name)?;
        if cached.fetched_at.elapsed() > RESOURCE_DECLARATION_CACHE_TTL {
            cache.remove(server_name);
            return None;
        }
        if cached.resource_uris.contains(uri)
            || cached
                .resource_templates
                .iter()
                .any(|template| uri_matches_template(uri, template))
        {
            return Some(true);
        }
        if cached.resource_uris_complete && cached.resource_templates_complete {
            return Some(false);
        }
        None
    }

    fn store_resource_declarations_partial(
        &self,
        server_name: &str,
        resources: Option<&[Resource]>,
        templates: Option<&[rmcp::model::ResourceTemplate]>,
    ) {
        if let Ok(mut cache) = self.resource_declarations.lock() {
            let entry = cache.entry(server_name.to_string()).or_insert_with(|| {
                CachedResourceDeclarations {
                    resource_uris: BTreeSet::new(),
                    resource_templates: Vec::new(),
                    resource_uris_complete: false,
                    resource_templates_complete: false,
                    fetched_at: Instant::now(),
                }
            });
            if let Some(resources) = resources {
                entry.resource_uris = resources
                    .iter()
                    .map(|resource| resource.raw.uri.clone())
                    .collect();
                entry.resource_uris_complete = true;
            }
            if let Some(templates) = templates {
                entry.resource_templates = templates
                    .iter()
                    .map(|template| template.raw.uri_template.clone())
                    .collect();
                entry.resource_templates_complete = true;
            }
            entry.fetched_at = Instant::now();
        }
    }
}

struct SessionEntry {
    service: Arc<McpService>,
    _process: Option<StdioProcessHandle>,
    /// Server-advertised capabilities captured from the `initialize` response
    /// (`peer_info().capabilities`). Stored at session bring-up so callers
    /// asking `server_capabilities` after a `tools/list` cannot trigger another
    /// round-trip; rmcp's `OnceCell` is populated synchronously when `serve`
    /// returns. `None` only if the peer never delivered an `initialize` result,
    /// which would have failed the bring-up before this `SessionEntry` exists.
    server_capabilities: Option<ServerCapabilities>,
}

#[derive(Debug)]
struct CachedResourceRead {
    value: Value,
    fetched_at: Instant,
}

#[derive(Debug, Clone)]
struct CachedResourceDeclarations {
    resource_uris: BTreeSet<String>,
    resource_templates: Vec<String>,
    resource_uris_complete: bool,
    resource_templates_complete: bool,
    fetched_at: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct McpToolCacheRecord {
    schema_version: u64,
    fetched_unix_millis: u128,
    tools: Vec<ExternalMcpTool>,
}

#[derive(Clone)]
struct SqueezyMcpClientHandler {
    server_name: String,
    elicitation_handler: Arc<Mutex<Option<McpElicitationHandler>>>,
    elicitation_policy: Arc<Mutex<PermissionMode>>,
    elicitation_audit: Arc<Mutex<std::collections::VecDeque<McpElicitationAuditEvent>>>,
    pause_state: ElicitationPauseState,
    /// Shared with `McpClientRegistry` so resource-change notifications can
    /// evict stale cached reads before their TTL lapses.
    resource_reads: Arc<Mutex<BTreeMap<(String, String), CachedResourceRead>>>,
    /// Shared with `McpClientRegistry` so resource-list changes invalidate the
    /// declaration gate cache alongside cached reads.
    resource_declarations: Arc<Mutex<BTreeMap<String, CachedResourceDeclarations>>>,
}

impl SqueezyMcpClientHandler {
    /// Drop the cached `read_resource` entry for `uri` so the next read
    /// re-fetches instead of serving content the server just signalled as
    /// changed (otherwise it would stay cached until the TTL lapses).
    fn evict_resource_read(&self, uri: &str) {
        if let Ok(mut cache) = self.resource_reads.lock() {
            cache.remove(&(self.server_name.clone(), uri.to_string()));
        }
    }

    /// Drop every cached `read_resource` entry for this server. Used when the
    /// resource list changes, which can invalidate any prior read.
    fn evict_server_resource_reads(&self) {
        if let Ok(mut cache) = self.resource_reads.lock() {
            cache.retain(|(server, _), _| server != &self.server_name);
        }
    }

    fn evict_server_resource_declarations(&self) {
        if let Ok(mut cache) = self.resource_declarations.lock() {
            cache.remove(&self.server_name);
        }
    }
}

impl ClientHandler for SqueezyMcpClientHandler {
    async fn create_elicitation(
        &self,
        request: CreateElicitationRequestParams,
        context: RequestContext<RoleClient>,
    ) -> Result<CreateElicitationResult, rmcp::ErrorData> {
        let request_id = format!("{:?}", context.id);
        let policy = self
            .elicitation_policy
            .lock()
            .map(|policy| *policy)
            .unwrap_or(PermissionMode::Ask);
        let kind = elicitation_kind(&request);
        let auto = classify_elicitation(policy, &request);
        if matches!(auto, AutoElicitationDecision::AutoAccept) {
            tracing::info!(
                target: "squeezy::mcp",
                server = %self.server_name,
                request_id = %request_id,
                kind = ?kind,
                policy = %policy.as_str(),
                "auto-accepting MCP elicitation: policy allows and form has no required fields"
            );
        } else if matches!(auto, AutoElicitationDecision::AutoDecline) {
            tracing::info!(
                target: "squeezy::mcp",
                server = %self.server_name,
                request_id = %request_id,
                kind = ?kind,
                policy = %policy.as_str(),
                "auto-declining MCP elicitation: policy denies all elicitations"
            );
        }
        match auto {
            AutoElicitationDecision::AutoAccept => {
                push_elicitation_audit(
                    &self.elicitation_audit,
                    McpElicitationAuditEvent {
                        server: self.server_name.clone(),
                        request_id,
                        kind,
                        policy,
                        outcome: McpElicitationAuditOutcome::AutoAccepted,
                        unix_millis: unix_millis(),
                    },
                );
                Ok(CreateElicitationResult {
                    action: ElicitationAction::Accept,
                    content: Some(json!({})),
                    meta: None,
                })
            }
            AutoElicitationDecision::AutoDecline => {
                push_elicitation_audit(
                    &self.elicitation_audit,
                    McpElicitationAuditEvent {
                        server: self.server_name.clone(),
                        request_id,
                        kind,
                        policy,
                        outcome: McpElicitationAuditOutcome::AutoDeclined,
                        unix_millis: unix_millis(),
                    },
                );
                Ok(CreateElicitationResult {
                    action: ElicitationAction::Decline,
                    content: None,
                    meta: None,
                })
            }
            AutoElicitationDecision::Forward => {
                let handler = self
                    .elicitation_handler
                    .lock()
                    .ok()
                    .and_then(|handler| handler.clone());
                let Some(handler) = handler else {
                    push_elicitation_audit(
                        &self.elicitation_audit,
                        McpElicitationAuditEvent {
                            server: self.server_name.clone(),
                            request_id,
                            kind,
                            policy,
                            outcome: McpElicitationAuditOutcome::AutoDeclined,
                            unix_millis: unix_millis(),
                        },
                    );
                    return Ok(CreateElicitationResult {
                        action: ElicitationAction::Decline,
                        content: None,
                        meta: None,
                    });
                };
                push_elicitation_audit(
                    &self.elicitation_audit,
                    McpElicitationAuditEvent {
                        server: self.server_name.clone(),
                        request_id,
                        kind,
                        policy,
                        outcome: McpElicitationAuditOutcome::Forwarded,
                        unix_millis: unix_millis(),
                    },
                );
                let ui_request = elicitation_request_for_ui(&self.server_name, &context, &request);
                let _pause = self.pause_state.enter(&self.server_name);
                let response = handler(ui_request).await;
                Ok(CreateElicitationResult {
                    action: match response.action {
                        McpElicitationAction::Accept => ElicitationAction::Accept,
                        McpElicitationAction::Decline => ElicitationAction::Decline,
                        McpElicitationAction::Cancel => ElicitationAction::Cancel,
                    },
                    content: response.content,
                    meta: None,
                })
            }
        }
    }

    async fn on_cancelled(
        &self,
        params: CancelledNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        tracing::info!(
            target: "squeezy::mcp",
            server = %self.server_name,
            request_id = ?params.request_id,
            reason = ?params.reason,
            "MCP server cancelled request"
        );
    }

    async fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        tracing::info!(
            target: "squeezy::mcp",
            server = %self.server_name,
            token = ?params.progress_token,
            progress = params.progress,
            total = ?params.total,
            message = ?params.message,
            "MCP server progress"
        );
    }

    async fn on_resource_updated(
        &self,
        params: ResourceUpdatedNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.evict_resource_read(&params.uri);
        tracing::info!(
            target: "squeezy::mcp",
            server = %self.server_name,
            uri = %params.uri,
            "MCP server resource updated"
        );
    }

    async fn on_resource_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.evict_server_resource_reads();
        self.evict_server_resource_declarations();
        tracing::info!(
            target: "squeezy::mcp",
            server = %self.server_name,
            "MCP server resource list changed"
        );
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        tracing::info!(
            target: "squeezy::mcp",
            server = %self.server_name,
            "MCP server tool list changed"
        );
    }

    async fn on_prompt_list_changed(&self, _context: NotificationContext<RoleClient>) {
        tracing::info!(
            target: "squeezy::mcp",
            server = %self.server_name,
            "MCP server prompt list changed"
        );
    }

    async fn on_logging_message(
        &self,
        params: LoggingMessageNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        let logger = params.logger.as_deref();
        match params.level {
            LoggingLevel::Emergency
            | LoggingLevel::Alert
            | LoggingLevel::Critical
            | LoggingLevel::Error => tracing::error!(
                target: "squeezy::mcp",
                server = %self.server_name,
                logger = ?logger,
                data = %params.data,
                "MCP server log"
            ),
            LoggingLevel::Warning => tracing::warn!(
                target: "squeezy::mcp",
                server = %self.server_name,
                logger = ?logger,
                data = %params.data,
                "MCP server log"
            ),
            LoggingLevel::Notice | LoggingLevel::Info => tracing::info!(
                target: "squeezy::mcp",
                server = %self.server_name,
                logger = ?logger,
                data = %params.data,
                "MCP server log"
            ),
            LoggingLevel::Debug => tracing::debug!(
                target: "squeezy::mcp",
                server = %self.server_name,
                logger = ?logger,
                data = %params.data,
                "MCP server log"
            ),
        }
    }

    fn get_info(&self) -> ClientInfo {
        // Advertise Squeezy as the client implementation so servers logging
        // peer identity see "squeezy-mcp" rather than the default
        // `CARGO_CRATE_NAME` (`rmcp`). Capabilities are declared explicitly so
        // a server gating on `client.capabilities.elicitation` knows the
        // handler at `create_elicitation` will respond instead of timing out;
        // `experimental` defaults to an empty map so future opt-in flags have
        // a stable slot to land in without changing the wire shape. The
        // struct is `#[non_exhaustive]` upstream, so we mutate `default()`
        // rather than naming every field.
        let mut capabilities = ClientCapabilities::default();
        capabilities.experimental = Some(rmcp::model::ExperimentalCapabilities::new());
        capabilities.elicitation = Some(rmcp::model::ElicitationCapability {
            form: Some(rmcp::model::FormElicitationCapability {
                schema_validation: None,
            }),
            url: Some(rmcp::model::UrlElicitationCapability {}),
        });
        ClientInfo::new(
            capabilities,
            Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
        )
    }
}

#[derive(Clone)]
struct ElicitationPauseState {
    active: Arc<Mutex<BTreeMap<String, usize>>>,
    sequence: Arc<AtomicU64>,
    tx: Arc<watch::Sender<u64>>,
}

impl Default for ElicitationPauseState {
    fn default() -> Self {
        let (tx, _) = watch::channel(0u64);
        Self {
            active: Arc::new(Mutex::new(BTreeMap::new())),
            sequence: Arc::new(AtomicU64::new(0)),
            tx: Arc::new(tx),
        }
    }
}

impl ElicitationPauseState {
    fn enter(&self, server_name: &str) -> ElicitationPauseGuard {
        if let Ok(mut active) = self.active.lock() {
            *active.entry(server_name.to_string()).or_default() += 1;
        }
        self.notify();
        ElicitationPauseGuard {
            state: self.clone(),
            server_name: server_name.to_string(),
        }
    }

    fn is_paused(&self, server_name: &str) -> bool {
        self.active
            .lock()
            .is_ok_and(|active| active.get(server_name).copied().unwrap_or_default() > 0)
    }

    fn subscribe(&self) -> watch::Receiver<u64> {
        self.tx.subscribe()
    }

    fn notify(&self) {
        let next = self.sequence.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.tx.send(next);
    }
}

struct ElicitationPauseGuard {
    state: ElicitationPauseState,
    server_name: String,
}

impl Drop for ElicitationPauseGuard {
    fn drop(&mut self) {
        if let Ok(mut active) = self.state.active.lock()
            && let Some(count) = active.get_mut(&self.server_name)
        {
            *count = count.saturating_sub(1);
            if *count == 0 {
                active.remove(&self.server_name);
            }
        }
        self.state.notify();
    }
}

#[derive(Debug)]
struct StdioProcessHandle {
    pid: u32,
    terminated: AtomicBool,
    /// On Windows, the process is assigned to a Job Object with
    /// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` so the whole process tree
    /// (including grandchildren spawned by wrapper launchers such as
    /// `cmd.exe`, PowerShell, `npx`, etc.) is terminated when this handle
    /// drops. Direct `TerminateProcess` is used as a fallback if Job Object
    /// assignment fails.
    ///
    /// This field exists purely for its Drop side-effect (RAII guard); the
    /// value itself is never read after construction.
    #[cfg(windows)]
    #[allow(dead_code)]
    win_job: Option<win_job::McpJob>,
}

impl StdioProcessHandle {
    #[cfg(not(windows))]
    fn new(pid: u32) -> Self {
        Self {
            pid,
            terminated: AtomicBool::new(false),
        }
    }

    #[cfg(windows)]
    fn new(pid: u32, win_job: Option<win_job::McpJob>) -> Self {
        Self {
            pid,
            terminated: AtomicBool::new(false),
            win_job,
        }
    }

    fn terminate(&self) {
        if self.terminated.swap(true, Ordering::SeqCst) {
            return;
        }
        // On Windows, primary cleanup is the Job Object (win_job field):
        // dropping it closes the handle, firing KILL_ON_JOB_CLOSE and killing
        // the entire process tree. Only fall back to direct PID termination
        // when Job Object assignment failed at spawn time (win_job == None).
        // The field drop happens after this method returns (Rust struct drop
        // order: body runs first, then fields in declaration order).
        #[cfg(windows)]
        if self.win_job.is_none() {
            terminate_process_group(self.pid);
        }
        #[cfg(not(windows))]
        terminate_process_group(self.pid);
    }
}

impl Drop for StdioProcessHandle {
    fn drop(&mut self) {
        self.terminate();
    }
}

async fn start_stdio_service(
    server_name: &str,
    server: &McpServerConfig,
    handler: SqueezyMcpClientHandler,
) -> McpResult<SessionEntry> {
    let command = server
        .command
        .as_ref()
        .ok_or_else(|| McpError::MissingCommand {
            server: server_name.to_string(),
        })?;
    let mut process = tokio::process::Command::new(command);
    process
        .args(&server.args)
        .envs(&server.env)
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    if let Some(cwd) = &server.cwd {
        process.current_dir(cwd);
    }
    #[cfg(unix)]
    process.process_group(0);
    #[cfg(windows)]
    warn_duplicate_env_keys(server_name, &server.env);
    let (transport, stderr) = TokioChildProcess::builder(process)
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| McpError::Transport {
            server: server_name.to_string(),
            message: err.to_string(),
        })?;
    #[cfg(not(windows))]
    let process_handle = transport.id().map(StdioProcessHandle::new);
    #[cfg(windows)]
    let process_handle = {
        let pid = transport.id();
        // NOTE: There is an inherent race window between process spawn and
        // `AssignProcessToJobObject`. Grandchildren spawned by the child before
        // assignment completes (e.g. `cmd.exe` immediately launching `node`)
        // will not be members of the job and will survive cleanup. The proper
        // fix (CREATE_SUSPENDED + assign + ResumeThread) is not accessible
        // through `tokio::process::Command`, so this best-effort coverage is
        // the best we can do without a custom spawn helper. The window is
        // typically a few microseconds in practice.
        let job = pid.and_then(|pid| match win_job::McpJob::new_and_assign(pid) {
            Ok(job) => Some(job),
            Err(err) => {
                tracing::warn!(
                    target: "squeezy::mcp",
                    server = %server_name,
                    error = %err,
                    "failed to assign MCP stdio process to Job Object; \
                     process-tree cleanup will fall back to direct PID termination"
                );
                None
            }
        });
        pid.map(|pid| StdioProcessHandle::new(pid, job))
    };
    if let Some(stderr) = stderr {
        let server_name = server_name.to_string();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => tracing::info!(
                        target: "squeezy::mcp",
                        server = %server_name,
                        stderr = %line,
                        "MCP server stderr"
                    ),
                    Ok(None) => break,
                    Err(error) => {
                        tracing::warn!(
                            target: "squeezy::mcp",
                            server = %server_name,
                            error = %error,
                            "failed to read MCP server stderr"
                        );
                        break;
                    }
                }
            }
        });
    }
    let service = handler
        .serve(transport)
        .await
        .map_err(|err| McpError::Transport {
            server: server_name.to_string(),
            message: err.to_string(),
        })?;
    let server_capabilities = service.peer_info().map(|info| info.capabilities.clone());
    Ok(SessionEntry {
        service: Arc::new(service),
        _process: process_handle,
        server_capabilities,
    })
}

async fn start_http_service(
    server_name: &str,
    server: &McpServerConfig,
    handler: SqueezyMcpClientHandler,
) -> McpResult<SessionEntry> {
    let url = server.url.as_ref().ok_or_else(|| McpError::MissingUrl {
        server: server_name.to_string(),
        transport: match server.transport {
            McpTransport::Http => "http",
            McpTransport::Sse => "sse",
            McpTransport::Stdio => "stdio",
        },
    })?;
    let config = build_streamable_http_config(server_name, url.clone(), server, |name| {
        std::env::var(name).ok()
    });
    let transport = StreamableHttpClientTransport::from_config(config);
    let service = handler
        .serve(transport)
        .await
        .map_err(|err| McpError::Transport {
            server: server_name.to_string(),
            message: err.to_string(),
        })?;
    let server_capabilities = service.peer_info().map(|info| info.capabilities.clone());
    Ok(SessionEntry {
        service: Arc::new(service),
        _process: None,
        server_capabilities,
    })
}

/// Start a session against a legacy MCP HTTP+SSE server (2024-11-05 spec).
///
/// The transport opens a GET against `server.url` for the `text/event-stream`
/// channel, waits for the server's `event: endpoint` frame, then routes
/// outbound JSON-RPC messages through POSTs to the advertised endpoint. The
/// stream is parsed via `crate::sse::SseDecoder`, which honours `event:` and
/// `data:` lines per the HTML SSE grammar — distinct from the 2025-03-26
/// streamable-HTTP transport, where SSE handling is internal to rmcp.
async fn start_sse_service(
    server_name: &str,
    server: &McpServerConfig,
    handler: SqueezyMcpClientHandler,
) -> McpResult<SessionEntry> {
    let url = server.url.as_ref().ok_or_else(|| McpError::MissingUrl {
        server: server_name.to_string(),
        transport: "sse",
    })?;
    let (auth_header, custom_headers) =
        resolve_http_auth_and_headers(server_name, server, |name| std::env::var(name).ok());
    let worker =
        sse::build_sse_worker(url.clone(), auth_header, custom_headers).map_err(|err| {
            McpError::Transport {
                server: server_name.to_string(),
                message: err.to_string(),
            }
        })?;
    let service = handler
        .serve(worker)
        .await
        .map_err(|err| McpError::Transport {
            server: server_name.to_string(),
            message: err.to_string(),
        })?;
    let server_capabilities = service.peer_info().map(|info| info.capabilities.clone());
    Ok(SessionEntry {
        service: Arc::new(service),
        _process: None,
        server_capabilities,
    })
}

/// Resolve `bearer_token_env_var`, `http_headers`, and `env_http_headers`
/// against the supplied env lookup. Missing env vars are skipped (not fatal);
/// invalid header names/values log a warning and are dropped so a single bad
/// entry never blocks the whole server from connecting. Env-sourced headers
/// override the static map on name conflict so secret rotation does not lose
/// to a stale literal. Shared by the streamable-HTTP and legacy-SSE clients
/// so both transports honour the same auth/header config surface.
fn resolve_http_auth_and_headers<F>(
    server_name: &str,
    server: &McpServerConfig,
    lookup_env: F,
) -> (
    Option<String>,
    std::collections::HashMap<HeaderName, HeaderValue>,
)
where
    F: Fn(&str) -> Option<String>,
{
    let auth_header = server.bearer_token_env_var.as_deref().and_then(|name| {
        let value = lookup_env(name)?;
        if value.trim().is_empty() {
            tracing::warn!(
                server = server_name,
                env_var = name,
                "bearer_token_env_var resolved to empty value; skipping Authorization header"
            );
            return None;
        }
        Some(value)
    });

    let mut custom_headers: std::collections::HashMap<HeaderName, HeaderValue> =
        std::collections::HashMap::with_capacity(
            server.http_headers.len() + server.env_http_headers.len(),
        );
    for (name, value) in &server.http_headers {
        match (
            HeaderName::try_from(name.as_str()),
            HeaderValue::try_from(value.as_str()),
        ) {
            (Ok(header_name), Ok(header_value)) => {
                custom_headers.insert(header_name, header_value);
            }
            (Err(err), _) => {
                tracing::warn!(
                    server = server_name,
                    header = name,
                    "invalid HTTP header name: {err}"
                );
            }
            (_, Err(err)) => {
                tracing::warn!(
                    server = server_name,
                    header = name,
                    "invalid HTTP header value: {err}"
                );
            }
        }
    }
    for (header_name, env_var) in &server.env_http_headers {
        let Some(value) = lookup_env(env_var) else {
            continue;
        };
        if value.trim().is_empty() {
            continue;
        }
        match (
            HeaderName::try_from(header_name.as_str()),
            HeaderValue::try_from(value.as_str()),
        ) {
            (Ok(name), Ok(val)) => {
                custom_headers.insert(name, val);
            }
            (Err(err), _) => {
                tracing::warn!(
                    server = server_name,
                    header = header_name,
                    "invalid HTTP header name: {err}"
                );
            }
            (_, Err(err)) => {
                tracing::warn!(
                    server = server_name,
                    header = header_name,
                    env_var = env_var,
                    "invalid HTTP header value resolved from env: {err}"
                );
            }
        }
    }
    (auth_header, custom_headers)
}

/// Build a `StreamableHttpClientTransportConfig` from an MCP server config.
fn build_streamable_http_config<F>(
    server_name: &str,
    url: String,
    server: &McpServerConfig,
    lookup_env: F,
) -> StreamableHttpClientTransportConfig
where
    F: Fn(&str) -> Option<String>,
{
    let (auth_header, custom_headers) =
        resolve_http_auth_and_headers(server_name, server, lookup_env);
    let mut config = StreamableHttpClientTransportConfig::with_uri(url);
    config.auth_header = auth_header;
    config.custom_headers = custom_headers;
    config
}

async fn with_timeout<T>(
    server_name: &str,
    timeout_ms: u64,
    cancel: CancellationToken,
    pause_state: ElicitationPauseState,
    future: impl Future<Output = McpResult<T>>,
) -> McpResult<T> {
    let mut pause_rx = pause_state.subscribe();
    let mut remaining = Duration::from_millis(timeout_ms);
    let mut future = Box::pin(future);
    loop {
        if pause_state.is_paused(server_name) {
            tokio::select! {
                _ = cancel.cancelled() => return Err(McpError::Cancelled { server: server_name.to_string() }),
                changed = pause_rx.changed() => {
                    if changed.is_err() {
                        return Err(McpError::Transport {
                            server: server_name.to_string(),
                            message: "MCP timeout pause watcher closed".to_string(),
                        });
                    }
                }
                result = &mut future => return result,
            }
        } else {
            if remaining.is_zero() {
                return Err(McpError::Timeout {
                    server: server_name.to_string(),
                    timeout_ms,
                });
            }
            let started = Instant::now();
            tokio::select! {
                _ = cancel.cancelled() => return Err(McpError::Cancelled { server: server_name.to_string() }),
                _ = tokio::time::sleep(remaining) => return Err(McpError::Timeout {
                    server: server_name.to_string(),
                    timeout_ms,
                }),
                changed = pause_rx.changed() => {
                    if changed.is_err() {
                        return Err(McpError::Transport {
                            server: server_name.to_string(),
                            message: "MCP timeout pause watcher closed".to_string(),
                        });
                    }
                    remaining = remaining.saturating_sub(started.elapsed());
                }
                result = &mut future => return result,
            }
        }
    }
}

fn convert_tools(
    server_name: &str,
    server: &McpServerConfig,
    tools: Vec<RmcpTool>,
) -> Vec<ExternalMcpTool> {
    let mut model_name_prefix = None;
    let mut converted = Vec::with_capacity(tools.len());
    for tool in tools {
        if !tool_allowed(server, &tool.name) {
            continue;
        }
        let raw_name = tool.name.to_string();
        let description = tool
            .description
            .as_ref()
            .map(|description| description.to_string())
            .unwrap_or_else(|| format!("MCP tool {server_name}/{raw_name}"));
        let raw_parameters = schema_object(tool.schema_as_json_value());
        let (parameters, stats) = compact_tool_schema(&raw_parameters, MAX_TOOL_SCHEMA_BYTES);
        if stats.compacted_bytes < stats.original_bytes {
            tracing::debug!(
                target: "squeezy::mcp",
                server = %server_name,
                tool = %raw_name,
                original_bytes = stats.original_bytes,
                compacted_bytes = stats.compacted_bytes,
                ratio = stats.ratio,
                "compacted MCP tool schema"
            );
        }
        let prefix =
            model_name_prefix.get_or_insert_with(|| external_tool_name_prefix(server_name));
        let model_name = external_tool_name_with_prefix(prefix, &raw_name);
        converted.push(ExternalMcpTool {
            server: server_name.to_string(),
            raw_name,
            model_name,
            description,
            parameters,
            transport: server.transport,
        });
    }
    converted
}

fn tool_allowed(server: &McpServerConfig, raw_name: &str) -> bool {
    if let Some(enabled) = &server.enabled_tools
        && !enabled.iter().any(|tool| tool == raw_name)
    {
        return false;
    }
    !server.disabled_tools.iter().any(|tool| tool == raw_name)
}

fn schema_object(value: Value) -> Value {
    if value.as_object().is_some() {
        value
    } else {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": true,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CompactionStats {
    pub original_bytes: usize,
    pub compacted_bytes: usize,
    pub ratio: f32,
}

/// Strip explicit `null` fields and empty-string `description` entries from a
/// JSON schema, recursively. Returns a new value; the input is left intact.
pub(crate) fn sanitize_tool_schema(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (key, child) in map {
                if matches!(child, Value::Null) {
                    continue;
                }
                if key == "description"
                    && matches!(child, Value::String(text) if text.trim().is_empty())
                {
                    continue;
                }
                out.insert(key.clone(), sanitize_tool_schema(child));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(sanitize_tool_schema).collect()),
        _ => value.clone(),
    }
}

/// Run the three-pass compactor:
///   (1) sanitize — strip null / empty-description fields,
///   (2) `$defs` hoist — drop unreachable definitions,
///   (3) minify — handled implicitly by `Value::to_string()` at emission time.
/// Reports the byte cost before and after compaction. The cap is informational
/// for now (used to gate deeper passes in future work); the function always
/// returns a schema whose serialized size is ≤ the original.
pub(crate) fn compact_tool_schema(value: &Value, _max_bytes: usize) -> (Value, CompactionStats) {
    let original_bytes = value.to_string().len();
    let sanitized = sanitize_tool_schema(value);
    let pruned = prune_unreachable_defs(sanitized);
    let compacted_bytes = pruned.to_string().len();
    let ratio = if original_bytes == 0 {
        1.0
    } else {
        compacted_bytes as f32 / original_bytes as f32
    };
    (
        pruned,
        CompactionStats {
            original_bytes,
            compacted_bytes,
            ratio,
        },
    )
}

/// Drop entries from `$defs` / `definitions` that are not referenced anywhere
/// in the schema (by `"$ref": "#/$defs/<name>"` or `"#/definitions/<name>"`).
fn prune_unreachable_defs(mut value: Value) -> Value {
    let object = match value.as_object_mut() {
        Some(object) => object,
        None => return value,
    };
    for key in ["$defs", "definitions"] {
        let Some(defs) = object.get(key).and_then(Value::as_object) else {
            continue;
        };
        if defs.is_empty() {
            object.remove(key);
            continue;
        }
        // Build the set of refs that appear OUTSIDE the defs block itself.
        let prefix = ref_prefix(key);
        let mut referenced = BTreeSet::new();
        collect_refs_outside_key(object, key, &prefix, &mut referenced);
        let Some(defs) = object.get_mut(key).and_then(Value::as_object_mut) else {
            continue;
        };
        let defs = std::mem::take(defs);
        // Walk over def bodies too — a referenced def may itself ref another def.
        let mut frontier: Vec<String> = referenced.iter().cloned().collect();
        while let Some(name) = frontier.pop() {
            let Some(body) = defs.get(&name) else {
                continue;
            };
            let mut nested = BTreeSet::new();
            collect_refs_with_prefix(body, &prefix, &mut nested);
            for next in nested {
                if referenced.insert(next.clone()) {
                    frontier.push(next);
                }
            }
        }
        let kept: serde_json::Map<String, Value> = defs
            .into_iter()
            .filter(|(name, _)| referenced.contains(name))
            .collect();
        if kept.is_empty() {
            object.remove(key);
        } else if let Some(defs) = object.get_mut(key) {
            *defs = Value::Object(kept);
        }
    }
    value
}

fn collect_refs_outside_key(
    object: &serde_json::Map<String, Value>,
    skip_key: &str,
    prefix: &str,
    out: &mut BTreeSet<String>,
) {
    for (key, child) in object {
        if key != skip_key {
            collect_ref_field(key, child, prefix, out);
            collect_refs_with_prefix(child, prefix, out);
        }
    }
}

fn collect_refs_with_prefix(value: &Value, prefix: &str, out: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                collect_ref_field(key, child, prefix, out);
                collect_refs_with_prefix(child, prefix, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_refs_with_prefix(item, prefix, out);
            }
        }
        _ => {}
    }
}

fn collect_ref_field(key: &str, child: &Value, prefix: &str, out: &mut BTreeSet<String>) {
    if key == "$ref"
        && let Some(text) = child.as_str()
        && let Some(name) = text.strip_prefix(prefix)
        && !out.contains(name)
    {
        out.insert(name.to_string());
    }
}

fn ref_prefix(defs_key: &str) -> String {
    format!("#/{defs_key}/")
}

fn arguments_object(tool: &str, arguments: Value) -> McpResult<JsonObject> {
    match arguments {
        Value::Null => Ok(JsonObject::new()),
        Value::Object(map) => Ok(map),
        _ => Err(McpError::InvalidArguments {
            tool: tool.to_string(),
        }),
    }
}

#[cfg(test)]
fn external_tool_name(server: &str, tool: &str) -> String {
    external_tool_name_with_prefix(&external_tool_name_prefix(server), tool)
}

fn external_tool_name_prefix(server: &str) -> String {
    format!("mcp__{}__", sanitize_name(server))
}

fn external_tool_name_with_prefix(prefix: &str, tool: &str) -> String {
    format!("{prefix}{}", sanitize_name(tool))
}

fn sanitize_name(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        "tool".to_string()
    } else if trimmed.len() == out.len() {
        out
    } else {
        trimmed.to_string()
    }
}

fn starting_status_snapshot(
    servers: &BTreeMap<String, McpServerConfig>,
    mut prior: BTreeMap<String, McpServerStatus>,
) -> BTreeMap<String, McpServerStatus> {
    prior.retain(|name, _| servers.get(name).is_some_and(|server| server.enabled));
    for (name, _server) in servers.iter().filter(|(_, server)| server.enabled) {
        prior.insert(name.clone(), McpServerStatus::Starting);
    }
    prior
}

fn normalize_palette(tools: Vec<ExternalMcpTool>) -> BTreeMap<String, ExternalMcpTool> {
    let mut tools = tools;
    tools.sort_by(|left, right| {
        (&left.server, &left.raw_name, &left.model_name).cmp(&(
            &right.server,
            &right.raw_name,
            &right.model_name,
        ))
    });
    let mut by_base: BTreeMap<String, usize> = BTreeMap::new();
    for tool in &tools {
        *by_base.entry(tool.model_name.clone()).or_default() += 1;
    }
    let mut next = BTreeMap::new();
    for mut tool in tools {
        let force_hash = by_base
            .get(tool.model_name.as_str())
            .copied()
            .unwrap_or_default()
            > 1
            || tool.model_name.len() > MAX_MODEL_TOOL_NAME_BYTES;
        if force_hash || next.contains_key(tool.model_name.as_str()) {
            ensure_unique_model_name(&mut tool, &next, force_hash);
        }
        next.insert(tool.model_name.clone(), tool);
    }
    next
}

fn ensure_unique_model_name(
    tool: &mut ExternalMcpTool,
    existing: &BTreeMap<String, ExternalMcpTool>,
    force_initial_hash: bool,
) {
    let base = tool.model_name.clone();
    let raw_identity = format!("{}\0{}", tool.server, tool.raw_name);
    if force_initial_hash {
        tool.model_name = fit_model_name(&base, &raw_identity, true);
    }
    let mut attempt = 0u32;
    while existing.contains_key(tool.model_name.as_str()) {
        attempt = attempt.saturating_add(1);
        tool.model_name = fit_model_name(&base, &format!("{raw_identity}\0{attempt}"), true);
    }
}

fn fit_model_name(base: &str, raw_identity: &str, force_hash: bool) -> String {
    if !force_hash && base.len() <= MAX_MODEL_TOOL_NAME_BYTES {
        return base.to_string();
    }
    let hash = sha256_hex_prefix(raw_identity.as_bytes(), HASH_SUFFIX_BYTES);
    let max_prefix = MAX_MODEL_TOOL_NAME_BYTES.saturating_sub(1 + hash.len());
    let prefix = truncate_ascii(base, max_prefix);
    let mut out = String::with_capacity(prefix.len() + 1 + hash.len());
    out.push_str(&prefix);
    out.push('_');
    out.push_str(&hash);
    out
}

fn truncate_ascii(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    value.chars().take(max_bytes).collect()
}

fn strip_untrusted_meta(mut value: Value) -> Value {
    match &mut value {
        Value::Object(object) => {
            object.remove("_meta");
            object.remove("meta");
            for value in object.values_mut() {
                *value = strip_untrusted_meta(std::mem::take(value));
            }
        }
        Value::Array(values) => {
            for value in values {
                *value = strip_untrusted_meta(std::mem::take(value));
            }
        }
        _ => {}
    }
    value
}

fn service_error_to_mcp(server: &str, err: rmcp::ServiceError) -> McpError {
    let message = err.to_string();
    if message.contains("Session expired (HTTP 404)") {
        return McpError::SessionExpired {
            server: server.to_string(),
        };
    }
    McpError::Transport {
        server: server.to_string(),
        message,
    }
}

fn can_auto_accept_elicitation(request: &CreateElicitationRequestParams) -> bool {
    match request {
        CreateElicitationRequestParams::FormElicitationParams {
            requested_schema, ..
        } => requested_schema
            .required
            .as_ref()
            .map(|required| required.is_empty())
            .unwrap_or(true),
        CreateElicitationRequestParams::UrlElicitationParams { .. } => false,
    }
}

fn elicitation_kind(request: &CreateElicitationRequestParams) -> McpElicitationKind {
    match request {
        CreateElicitationRequestParams::FormElicitationParams { .. } => McpElicitationKind::Form,
        CreateElicitationRequestParams::UrlElicitationParams { .. } => McpElicitationKind::Url,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoElicitationDecision {
    AutoAccept,
    AutoDecline,
    Forward,
}

/// Resolve the per-request elicitation decision against the host policy. Pure
/// function so the gate can be exercised in tests without spinning up an
/// `rmcp` peer; the audit ring and tracing are applied around it.
fn classify_elicitation(
    policy: PermissionMode,
    request: &CreateElicitationRequestParams,
) -> AutoElicitationDecision {
    match policy {
        PermissionMode::Deny => AutoElicitationDecision::AutoDecline,
        PermissionMode::Allow if can_auto_accept_elicitation(request) => {
            AutoElicitationDecision::AutoAccept
        }
        PermissionMode::Allow | PermissionMode::Ask => AutoElicitationDecision::Forward,
    }
}

fn push_elicitation_audit(
    log: &Arc<Mutex<std::collections::VecDeque<McpElicitationAuditEvent>>>,
    event: McpElicitationAuditEvent,
) {
    if let Ok(mut log) = log.lock() {
        if log.len() >= MCP_AUDIT_LOG_CAPACITY {
            log.pop_front();
        }
        log.push_back(event);
    }
}

/// Insert a resource-read entry while keeping the cache bounded. Expired
/// entries are dropped first (cheap, and keeps the working set roughly
/// proportional to one TTL window), then — if still at capacity — the oldest
/// surviving entry by `fetched_at` is evicted so a session that reads many
/// distinct URIs cannot grow the map without limit.
fn insert_resource_read(
    cache: &mut BTreeMap<(String, String), CachedResourceRead>,
    key: (String, String),
    entry: CachedResourceRead,
) {
    cache.retain(|_, v| v.fetched_at.elapsed() <= RESOURCE_READ_CACHE_TTL);
    while cache.len() >= RESOURCE_READ_CACHE_CAPACITY
        && let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, v)| v.fetched_at)
            .map(|(k, _)| k.clone())
    {
        cache.remove(&oldest);
    }
    cache.insert(key, entry);
}

fn elicitation_request_for_ui(
    server: &str,
    context: &RequestContext<RoleClient>,
    request: &CreateElicitationRequestParams,
) -> McpElicitationRequest {
    match request {
        CreateElicitationRequestParams::FormElicitationParams {
            message,
            requested_schema,
            ..
        } => McpElicitationRequest {
            server: server.to_string(),
            request_id: format!("{:?}", context.id),
            kind: McpElicitationKind::Form,
            message: message.clone(),
            schema: serde_json::to_value(requested_schema).ok(),
            url: None,
            elicitation_id: None,
        },
        CreateElicitationRequestParams::UrlElicitationParams {
            message,
            url,
            elicitation_id,
            ..
        } => McpElicitationRequest {
            server: server.to_string(),
            request_id: format!("{:?}", context.id),
            kind: McpElicitationKind::Url,
            message: message.clone(),
            schema: None,
            url: Some(url.clone()),
            elicitation_id: Some(elicitation_id.clone()),
        },
    }
}

fn uri_matches_template(uri: &str, template: &str) -> bool {
    if uri == template {
        return true;
    }
    let mut uri_rest = uri;
    let mut template_rest = template;
    loop {
        let Some(open) = template_rest.find('{') else {
            return uri_rest == template_rest;
        };
        let literal = &template_rest[..open];
        let Some(after_literal) = uri_rest.strip_prefix(literal) else {
            return false;
        };
        uri_rest = after_literal;

        let Some(close_rel) = template_rest[open + 1..].find('}') else {
            return false;
        };
        let placeholder = &template_rest[open + 1..open + 1 + close_rel];
        let placeholder_allows_slashes = uri_template_placeholder_allows_slashes(placeholder);
        let after_placeholder = open + 1 + close_rel + 1;
        template_rest = &template_rest[after_placeholder..];
        let next_literal = template_rest.split('{').next().unwrap_or_default();
        if next_literal.is_empty() {
            let has_more_placeholders = template_rest.contains('{');
            if has_more_placeholders {
                let Some((_, ch)) = uri_rest.char_indices().next() else {
                    return false;
                };
                if uri_template_placeholder_delimiter(ch, placeholder_allows_slashes) {
                    return false;
                }
                uri_rest = &uri_rest[ch.len_utf8()..];
                continue;
            }
            return uri_template_placeholder_value_valid(uri_rest, placeholder_allows_slashes);
        }

        let Some(match_start) = uri_rest.match_indices(next_literal).find_map(|(index, _)| {
            let value = &uri_rest[..index];
            uri_template_placeholder_value_valid(value, placeholder_allows_slashes).then_some(index)
        }) else {
            return false;
        };
        uri_rest = &uri_rest[match_start..];
    }
}

fn uri_template_placeholder_value_valid(value: &str, allows_slashes: bool) -> bool {
    !value.is_empty()
        && !value
            .chars()
            .any(|ch| uri_template_placeholder_delimiter(ch, allows_slashes))
}

fn uri_template_placeholder_delimiter(ch: char, allows_slashes: bool) -> bool {
    matches!(ch, '?' | '#') || (!allows_slashes && ch == '/')
}

fn uri_template_placeholder_allows_slashes(placeholder: &str) -> bool {
    let placeholder = placeholder.to_ascii_lowercase();
    placeholder == "path" || placeholder.ends_with("_path") || placeholder.ends_with("-path")
}

fn tool_cache_key(server_name: &str, server: &McpServerConfig) -> String {
    let fingerprint = json!({
        "schema": MCP_TOOL_CACHE_SCHEMA_VERSION,
        "server": server_name,
        "transport": server.transport.as_str(),
        "command": &server.command,
        "args": &server.args,
        "url": &server.url,
        "cwd": &server.cwd,
        "timeout_ms": server.timeout_ms,
        "discovery_timeout_ms": server.discovery_timeout_ms,
        "tool_call_timeout_ms": server.tool_call_timeout_ms,
        "env_keys": server.env.keys().collect::<Vec<_>>(),
        "enabled_tools": &server.enabled_tools,
        "disabled_tools": &server.disabled_tools,
    });
    format!(
        "{server_name}\0{}",
        sha256_hex(fingerprint.to_string().as_bytes())
    )
}

fn sha256_hex(bytes: impl AsRef<[u8]>) -> String {
    use std::fmt::Write as _;

    let digest = Sha256::digest(bytes.as_ref());
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

fn sha256_hex_prefix(bytes: impl AsRef<[u8]>, max_hex_chars: usize) -> String {
    use std::fmt::Write as _;

    let digest = Sha256::digest(bytes.as_ref());
    let digest_bytes = max_hex_chars.div_ceil(2).min(digest.len());
    let mut output = String::with_capacity(digest_bytes * 2);
    for &byte in digest.iter().take(digest_bytes) {
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output.truncate(max_hex_chars);
    output
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(unix)]
fn terminate_process_group(pid: u32) {
    let pgid = -(pid as libc::pid_t);
    unsafe {
        libc::kill(pgid, libc::SIGTERM);
    }
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(2));
        unsafe {
            libc::kill(pgid, libc::SIGKILL);
        }
    });
}

#[cfg(windows)]
fn terminate_process_group(pid: u32) {
    use windows_sys::Win32::Foundation::{CloseHandle, FALSE};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};

    // Primary cleanup happens through the Job Object held by StdioProcessHandle
    // (which has KILL_ON_JOB_CLOSE and kills the full process tree when dropped).
    // This direct TerminateProcess call is a belt-and-suspenders fallback for
    // the direct child PID when Job Object assignment failed at spawn time.
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE, FALSE, pid) };
    if handle.is_null() {
        return;
    }
    unsafe {
        let _ = TerminateProcess(handle, 1);
        CloseHandle(handle);
    }
}

#[cfg(not(any(unix, windows)))]
fn terminate_process_group(_pid: u32) {}

/// On Windows, warn if the stdio server's env map contains keys that differ
/// only in case. Windows env lookup is case-insensitive at the OS level, so
/// two entries like `Path` and `PATH` can silently shadow each other and cause
/// unexpected behavior at process startup.
#[cfg(windows)]
fn warn_duplicate_env_keys(server_name: &str, env: &std::collections::BTreeMap<String, String>) {
    let mut lower: std::collections::HashMap<String, &str> = std::collections::HashMap::new();
    for key in env.keys() {
        let lower_key = key.to_lowercase();
        if let Some(existing) = lower.insert(lower_key, key.as_str()) {
            tracing::warn!(
                target: "squeezy::mcp",
                server = %server_name,
                key_a = %existing,
                key_b = %key,
                "MCP server env contains keys that differ only in case; \
                 Windows env lookup is case-insensitive and one will shadow the other"
            );
        }
    }
}

/// Windows Job Object wrapper for MCP stdio process-tree cleanup.
///
/// Mirrors the shell sandbox's `squeezy-tools::win_job::ShellJob` design:
/// the Job Object is created with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` so
/// the entire spawned process tree — including grandchildren launched by
/// `cmd.exe`, PowerShell, `npx`, or other wrapper launchers — is terminated
/// when this handle drops.
#[cfg(windows)]
mod win_job {
    use std::{io, mem};
    use windows_sys::Win32::Foundation::{CloseHandle, FALSE, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    pub(super) struct McpJob {
        handle: HANDLE,
    }

    impl McpJob {
        /// Create a Job Object with `KILL_ON_JOB_CLOSE` and immediately assign
        /// the process with the given PID to it.
        pub(super) fn new_and_assign(pid: u32) -> io::Result<Self> {
            let handle = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { mem::zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let result = unsafe {
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const _,
                    mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if result == 0 {
                let err = io::Error::last_os_error();
                unsafe { CloseHandle(handle) };
                return Err(err);
            }
            let process = unsafe { OpenProcess(PROCESS_TERMINATE | PROCESS_SET_QUOTA, FALSE, pid) };
            if process.is_null() {
                let err = io::Error::last_os_error();
                unsafe { CloseHandle(handle) };
                return Err(err);
            }
            let assigned = unsafe { AssignProcessToJobObject(handle, process) };
            unsafe { CloseHandle(process) };
            if assigned == 0 {
                let err = io::Error::last_os_error();
                unsafe { CloseHandle(handle) };
                return Err(err);
            }
            Ok(Self { handle })
        }
    }

    impl Drop for McpJob {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.handle) };
        }
    }

    impl std::fmt::Debug for McpJob {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("McpJob").finish_non_exhaustive()
        }
    }

    unsafe impl Send for McpJob {}
    unsafe impl Sync for McpJob {}
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
