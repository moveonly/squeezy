use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::stream::{FuturesUnordered, StreamExt};
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, JsonObject, Tool as RmcpTool},
    transport::{StreamableHttpClientTransport, TokioChildProcess},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use squeezy_core::{McpServerConfig, McpTransport};
use tokio::sync::Mutex as TokioMutex;
use tokio_util::sync::CancellationToken;

const DEFAULT_MCP_TIMEOUT_MS: u64 = 30_000;

pub type McpResult<T> = Result<T, McpError>;

type McpService = rmcp::service::RunningService<rmcp::service::RoleClient, ()>;

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

#[derive(Clone, Default)]
pub struct McpClientRegistry {
    servers: Arc<BTreeMap<String, McpServerConfig>>,
    cache: Arc<Mutex<BTreeMap<String, ExternalMcpTool>>>,
    // Long-lived service handles keyed by server name. We reuse them across
    // discovery and tool calls so we don't pay process-spawn (stdio) or
    // session-handshake (HTTP) cost on every invocation. Entries are removed
    // on transport error so the next call transparently reconnects.
    sessions: Arc<TokioMutex<BTreeMap<String, Arc<McpService>>>>,
}

impl McpClientRegistry {
    pub fn new(servers: BTreeMap<String, McpServerConfig>) -> Self {
        Self {
            servers: Arc::new(servers),
            cache: Arc::new(Mutex::new(BTreeMap::new())),
            sessions: Arc::new(TokioMutex::new(BTreeMap::new())),
        }
    }

    pub fn has_no_enabled_servers(&self) -> bool {
        self.servers.iter().all(|(_, server)| !server.enabled)
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

    /// Refresh the cached tool list by listing tools on every enabled server
    /// in parallel. On success, the cache is updated with the fresh listing.
    /// On per-server failure, previously cached entries for the failing
    /// server are preserved so that a single transient timeout does not
    /// blow up tools that the model has already learned about this session.
    pub async fn refresh_tools(&self, cancel: CancellationToken) -> Vec<McpError> {
        let prior_cache = self
            .cache
            .lock()
            .map(|cache| cache.clone())
            .unwrap_or_default();

        if self.has_no_enabled_servers() {
            if let Ok(mut cache) = self.cache.lock() {
                cache.clear();
            }
            return Vec::new();
        }

        let mut futures = FuturesUnordered::new();
        for (server_name, server) in self.servers.iter() {
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

        let mut next: BTreeMap<String, ExternalMcpTool> = BTreeMap::new();
        let mut succeeded: BTreeSet<String> = BTreeSet::new();
        let mut errors = Vec::new();
        while let Some((name, result)) = futures.next().await {
            match result {
                Ok(tools) => {
                    succeeded.insert(name.clone());
                    for tool in tools {
                        let model_name = unique_model_name(&next, &tool.model_name);
                        next.insert(model_name.clone(), ExternalMcpTool { model_name, ..tool });
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        target: "squeezy::mcp",
                        server = %name,
                        error = %error,
                        "failed to discover MCP tools"
                    );
                    errors.push(error);
                }
            }
        }

        // Preserve prior tools for enabled servers whose refresh failed this
        // turn so a flaky server does not vanish mid-session.
        for (model_name, tool) in &prior_cache {
            let server_still_enabled = self
                .servers
                .get(&tool.server)
                .map(|server| server.enabled)
                .unwrap_or(false);
            if server_still_enabled
                && !succeeded.contains(&tool.server)
                && !next.contains_key(model_name)
            {
                next.insert(model_name.clone(), tool.clone());
            }
        }

        if let Ok(mut cache) = self.cache.lock() {
            *cache = next;
        }
        errors
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
            .servers
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
            content: result,
        })
    }

    async fn discover_one(
        &self,
        server_name: &str,
        server: &McpServerConfig,
        cancel: CancellationToken,
    ) -> McpResult<Vec<ExternalMcpTool>> {
        let timeout_ms = server.timeout_ms.unwrap_or(DEFAULT_MCP_TIMEOUT_MS);
        let registry = self.clone();
        let server_for_call = server.clone();
        let server_name_owned = server_name.to_string();
        let result = with_timeout(server_name, timeout_ms, cancel, async move {
            let service = registry
                .session_for(&server_name_owned, &server_for_call)
                .await?;
            let tools = service
                .list_all_tools()
                .await
                .map_err(|err| McpError::Transport {
                    server: server_name_owned.clone(),
                    message: err.to_string(),
                })?;
            Ok((server_name_owned, tools))
        })
        .await;
        match result {
            Ok((_, tools)) => Ok(convert_tools(server_name, server.transport, tools)),
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
        let timeout_ms = server.timeout_ms.unwrap_or(DEFAULT_MCP_TIMEOUT_MS);
        let registry = self.clone();
        let server_for_call = server.clone();
        let server_name_owned = server_name.to_string();
        let tool_name_owned = tool_name.to_string();
        let result = with_timeout(server_name, timeout_ms, cancel, async move {
            let service = registry
                .session_for(&server_name_owned, &server_for_call)
                .await?;
            let response = service
                .call_tool(
                    CallToolRequestParams::new(tool_name_owned.clone()).with_arguments(arguments),
                )
                .await
                .map_err(|err| McpError::Transport {
                    server: server_name_owned.clone(),
                    message: err.to_string(),
                })?;
            serde_json::to_value(response).map_err(|err| McpError::Transport {
                server: server_name_owned.clone(),
                message: err.to_string(),
            })
        })
        .await;
        if result.is_err() {
            self.invalidate_session(server_name).await;
        }
        result
    }

    async fn session_for(
        &self,
        server_name: &str,
        server: &McpServerConfig,
    ) -> McpResult<Arc<McpService>> {
        // Fast path: a previously started session is already cached.
        {
            let sessions = self.sessions.lock().await;
            if let Some(svc) = sessions.get(server_name) {
                return Ok(svc.clone());
            }
        }

        // Slow path: open a new session outside the lock so concurrent
        // discovery on other servers is not serialized behind this start.
        let svc = match server.transport {
            McpTransport::Stdio => start_stdio_service(server_name, server).await?,
            McpTransport::Http | McpTransport::Sse => {
                start_http_service(server_name, server).await?
            }
        };
        let arc = Arc::new(svc);
        let mut sessions = self.sessions.lock().await;
        if let Some(existing) = sessions.get(server_name) {
            // Lost the race against a concurrent caller. Use the existing
            // session and drop the duplicate; the unused service is reaped
            // by rmcp on Drop.
            return Ok(existing.clone());
        }
        sessions.insert(server_name.to_string(), arc.clone());
        Ok(arc)
    }

    async fn invalidate_session(&self, server_name: &str) {
        let mut sessions = self.sessions.lock().await;
        sessions.remove(server_name);
    }

    /// Tear down every cached session. Useful at shutdown so child processes
    /// reap promptly instead of waiting for the registry Arc to drop.
    pub async fn shutdown(&self) {
        let mut sessions = self.sessions.lock().await;
        sessions.clear();
    }

    /// Seed the in-memory tool cache directly. Intended for tests that need
    /// to pre-populate cached entries to verify refresh/preservation logic
    /// without spawning real MCP servers.
    #[doc(hidden)]
    pub fn insert_cached_tool_for_test(&self, tool: ExternalMcpTool) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(tool.model_name.clone(), tool);
        }
    }
}

async fn start_stdio_service(server_name: &str, server: &McpServerConfig) -> McpResult<McpService> {
    let command = server
        .command
        .as_ref()
        .ok_or_else(|| McpError::MissingCommand {
            server: server_name.to_string(),
        })?;
    let mut process = tokio::process::Command::new(command);
    process.args(&server.args);
    process.envs(&server.env);
    let transport = TokioChildProcess::new(process).map_err(|err| McpError::Transport {
        server: server_name.to_string(),
        message: err.to_string(),
    })?;
    ().serve(transport)
        .await
        .map_err(|err| McpError::Transport {
            server: server_name.to_string(),
            message: err.to_string(),
        })
}

async fn start_http_service(server_name: &str, server: &McpServerConfig) -> McpResult<McpService> {
    let url = server.url.as_ref().ok_or_else(|| McpError::MissingUrl {
        server: server_name.to_string(),
        transport: match server.transport {
            McpTransport::Http => "http",
            McpTransport::Sse => "sse",
            McpTransport::Stdio => "stdio",
        },
    })?;
    let transport = StreamableHttpClientTransport::from_uri(url.clone());
    ().serve(transport)
        .await
        .map_err(|err| McpError::Transport {
            server: server_name.to_string(),
            message: err.to_string(),
        })
}

async fn with_timeout<T>(
    server_name: &str,
    timeout_ms: u64,
    cancel: CancellationToken,
    future: impl std::future::Future<Output = McpResult<T>>,
) -> McpResult<T> {
    tokio::select! {
        _ = cancel.cancelled() => Err(McpError::Cancelled { server: server_name.to_string() }),
        result = tokio::time::timeout(Duration::from_millis(timeout_ms), future) => {
            result.map_err(|_| McpError::Timeout {
                server: server_name.to_string(),
                timeout_ms,
            })?
        }
    }
}

fn convert_tools(
    server_name: &str,
    transport: McpTransport,
    tools: Vec<RmcpTool>,
) -> Vec<ExternalMcpTool> {
    tools
        .into_iter()
        .map(|tool| {
            let raw_name = tool.name.to_string();
            let description = tool
                .description
                .as_ref()
                .map(|description| description.to_string())
                .unwrap_or_else(|| format!("MCP tool {server_name}/{raw_name}"));
            let parameters = schema_object(tool.schema_as_json_value());
            ExternalMcpTool {
                server: server_name.to_string(),
                raw_name: raw_name.clone(),
                model_name: external_tool_name(server_name, &raw_name),
                description,
                parameters,
                transport,
            }
        })
        .collect()
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

fn arguments_object(tool: &str, arguments: Value) -> McpResult<JsonObject> {
    match arguments {
        Value::Null => Ok(JsonObject::new()),
        Value::Object(map) => Ok(map),
        _ => Err(McpError::InvalidArguments {
            tool: tool.to_string(),
        }),
    }
}

fn external_tool_name(server: &str, tool: &str) -> String {
    format!("mcp__{}__{}", sanitize_name(server), sanitize_name(tool))
}

fn sanitize_name(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push('_');
        }
    }
    let out = out.trim_matches('_').to_string();
    if out.is_empty() {
        "tool".to_string()
    } else {
        out
    }
}

fn unique_model_name(existing: &BTreeMap<String, ExternalMcpTool>, candidate: &str) -> String {
    if !existing.contains_key(candidate) {
        return candidate.to_string();
    }
    for index in 2usize.. {
        let next = format!("{candidate}__{index}");
        if !existing.contains_key(&next) {
            return next;
        }
    }
    unreachable!("unbounded suffix search must find a unique name")
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
