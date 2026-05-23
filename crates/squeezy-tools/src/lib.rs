use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    env,
    ffi::OsString,
    fs::{self, OpenOptions},
    future::Future,
    io::{Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, Instant, SystemTime},
};

use futures_util::StreamExt;
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use regex::Regex;
use reqwest::{
    Url,
    header::{ACCEPT, HeaderMap, HeaderValue},
    redirect::Policy,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use squeezy_core::{
    DEFAULT_EXA_MCP_URL, DEFAULT_TOOL_OUTPUT_RETENTION_DAYS, DEFAULT_TOOL_PREVIEW_BYTES,
    DEFAULT_TOOL_SPILL_THRESHOLD_BYTES, FileId, GraphConfig, PermissionCapability, PermissionMode,
    PermissionRequest, PermissionRisk, PermissionRule, PermissionRuleSource, PermissionScope,
    Redactor, Result, ShellSandboxConfig, ShellSandboxMode, ShellSandboxNetworkPolicy,
    SkillsConfig, SqueezyError,
};
use squeezy_graph::{
    DirtyAnnotation, DirtyRange, GraphManager, GraphSymbol, ReferenceHit, SignatureQuery,
};
use squeezy_skills::{LoadedSkill, SkillActivation, SkillCatalog};
use squeezy_vcs::{
    CheckpointRecord, CheckpointStore, DiffFile, DiffFileStatus, DiffMode, DiffOptions,
    DiffSnapshot, GitVcs, RollbackMode, RollbackTarget, WorkspaceSnapshot,
};
use squeezy_workspace::{
    CompiledIndexingPolicy, CrawlOptions, ExclusionReason, IndexCoverage, IndexingPolicy,
};
use tokio::{io::AsyncReadExt, process::Command, sync::Mutex, time};
use tokio_util::sync::CancellationToken;
use tree_sitter::{Node, Parser};

#[cfg(target_os = "linux")]
use std::os::unix::process::CommandExt;

const DEFAULT_MAX_FILES: usize = 10_000;
const DEFAULT_MAX_BYTES_PER_FILE: usize = 1_000_000;
const DEFAULT_MAX_MATCHES: usize = 100;
const DEFAULT_OUTPUT_BYTE_CAP: usize = 24_000;
const DEFAULT_READ_LIMIT: usize = 32_000;
const MAX_READ_LIMIT: usize = 128_000;
const DEFAULT_SHELL_TIMEOUT_MS: u64 = 30_000;
const MAX_SHELL_TIMEOUT_MS: u64 = 120_000;
const VERIFY_SHELL_TIMEOUT_MS: u64 = 600_000;
const DEFAULT_SHELL_OUTPUT_BYTE_CAP: usize = 32_000;
const MAX_SHELL_OUTPUT_BYTE_CAP: usize = 128_000;
const DEFAULT_WEB_SEARCH_RESULTS: usize = 8;
const MAX_WEB_SEARCH_RESULTS: usize = 20;
const DEFAULT_WEB_SEARCH_CONTEXT_CHARS: usize = 10_000;
const MAX_WEB_SEARCH_CONTEXT_CHARS: usize = 50_000;
const DEFAULT_WEB_SEARCH_TIMEOUT_MS: u64 = 25_000;
const DEFAULT_WEB_SEARCH_MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const DEFAULT_WEB_FETCH_TIMEOUT_MS: u64 = 30_000;
const MAX_WEB_TIMEOUT_MS: u64 = 120_000;
const DEFAULT_WEB_FETCH_MAX_RESPONSE_BYTES: usize = 5 * 1024 * 1024;
const MAX_WEB_FETCH_MAX_RESPONSE_BYTES: usize = 10 * 1024 * 1024;
const DEFAULT_WEB_FETCH_OUTPUT_BYTE_CAP: usize = 32_000;
const MAX_WEB_REDIRECTS: usize = 5;
const DIFF_SNAPSHOT_TTL: Duration = Duration::from_millis(500);
const POLICY_PREFIX_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    /// The capability that approximates this tool's lowest-risk form, used at
    /// advertisement time (before any arguments are bound) to decide whether a
    /// tool should be visible to the model in a given session mode. Runtime
    /// permission decisions still flow through `permission_request` and can
    /// reclassify a specific call to a higher-risk capability (for example
    /// shell → git via the shell classifier); session mode gating in the agent
    /// applies on top of both layers.
    pub capability: PermissionCapability,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolStatus {
    Success,
    Error,
    Denied,
    Stale,
    Cancelled,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCostHint {
    pub files_scanned: u64,
    pub bytes_read: u64,
    pub matches_returned: u64,
    pub output_bytes: u64,
    pub redactions: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolReceipt {
    pub output_sha256: String,
    pub content_sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutputConfig {
    pub spill_threshold_bytes: usize,
    pub preview_bytes: usize,
    pub retention_days: u64,
    pub output_dir: Option<PathBuf>,
}

impl Default for ToolOutputConfig {
    fn default() -> Self {
        Self {
            spill_threshold_bytes: DEFAULT_TOOL_SPILL_THRESHOLD_BYTES,
            preview_bytes: DEFAULT_TOOL_PREVIEW_BYTES,
            retention_days: DEFAULT_TOOL_OUTPUT_RETENTION_DAYS,
            output_dir: None,
        }
    }
}

impl ToolOutputConfig {
    fn normalized(self) -> Self {
        Self {
            spill_threshold_bytes: nonzero_or(
                self.spill_threshold_bytes,
                DEFAULT_TOOL_SPILL_THRESHOLD_BYTES,
            ),
            preview_bytes: nonzero_or(self.preview_bytes, DEFAULT_TOOL_PREVIEW_BYTES),
            retention_days: nonzero_or_u64(self.retention_days, DEFAULT_TOOL_OUTPUT_RETENTION_DAYS),
            output_dir: self.output_dir,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebToolConfig {
    pub exa_mcp_url: String,
    pub exa_api_key: Option<String>,
}

impl Default for WebToolConfig {
    fn default() -> Self {
        Self {
            exa_mcp_url: DEFAULT_EXA_MCP_URL.to_string(),
            exa_api_key: None,
        }
    }
}

impl WebToolConfig {
    fn normalized(self) -> Self {
        let exa_mcp_url = self.exa_mcp_url.trim();
        let exa_mcp_url = if exa_mcp_url.is_empty() {
            DEFAULT_EXA_MCP_URL.to_string()
        } else {
            exa_mcp_url.to_string()
        };
        Self {
            exa_mcp_url,
            exa_api_key: self.exa_api_key.and_then(|key| {
                let key = key.trim();
                (!key.is_empty()).then(|| key.to_string())
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub tool_name: String,
    pub status: ToolStatus,
    pub content: Value,
    pub cost_hint: ToolCostHint,
    pub receipt: ToolReceipt,
}

impl ToolResult {
    pub fn model_output(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| {
            json!({
                "call_id": self.call_id,
                "tool_name": self.tool_name,
                "status": "error",
                "content": {"error": "tool result serialization failed"},
            })
            .to_string()
        })
    }

    pub fn denied(call: &ToolCall, reason: impl Into<String>) -> Self {
        let reason = reason.into();
        make_result(
            call,
            ToolStatus::Denied,
            json!({
                "error": reason.clone(),
                "reason": reason,
                "permission_denied": true,
            }),
            ToolCostHint::default(),
            None,
        )
    }

    pub fn cancelled(call: &ToolCall) -> Self {
        make_result(
            call,
            ToolStatus::Cancelled,
            json!({ "error": "tool call cancelled" }),
            ToolCostHint::default(),
            None,
        )
    }

    pub fn aggregate_budget_exceeded(&self, budget_bytes: usize, actual_bytes: usize) -> Self {
        let call = ToolCall {
            call_id: self.call_id.clone(),
            name: self.tool_name.clone(),
            arguments: Value::Null,
        };
        make_result(
            &call,
            ToolStatus::Error,
            json!({
                "error": "tool result omitted because aggregate tool-result budget was exceeded",
                "budget_bytes": budget_bytes,
                "actual_bytes": actual_bytes,
                "original_status": &self.status,
                "original_output_sha256": self.receipt.output_sha256,
            }),
            ToolCostHint {
                truncated: true,
                ..ToolCostHint::default()
            },
            self.receipt.content_sha256.clone(),
        )
    }
}

type WebHttpFuture<'a> =
    Pin<Box<dyn Future<Output = std::result::Result<WebHttpResponse, String>> + Send + 'a>>;

trait WebHttpClient: Send + Sync + std::fmt::Debug {
    fn post_json<'a>(
        &'a self,
        url: &'a str,
        headers: Vec<(String, String)>,
        body: Value,
        max_response_bytes: usize,
    ) -> WebHttpFuture<'a>;

    fn get<'a>(&'a self, url: Url, max_response_bytes: usize) -> WebHttpFuture<'a>;
}

#[derive(Debug, Clone)]
struct WebHttpResponse {
    status: u16,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

impl WebHttpResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    fn is_redirection(&self) -> bool {
        (300..400).contains(&self.status)
    }
}

#[derive(Debug)]
struct ReqwestWebHttpClient {
    client: reqwest::Client,
}

impl ReqwestWebHttpClient {
    fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .redirect(Policy::none())
            .build()
            .map_err(|err| SqueezyError::Tool(format!("failed to create HTTP client: {err}")))?;
        Ok(Self { client })
    }
}

impl WebHttpClient for ReqwestWebHttpClient {
    fn post_json<'a>(
        &'a self,
        url: &'a str,
        headers: Vec<(String, String)>,
        body: Value,
        max_response_bytes: usize,
    ) -> WebHttpFuture<'a> {
        Box::pin(async move {
            let mut request_headers = HeaderMap::new();
            for (name, value) in headers {
                let name = name
                    .parse::<reqwest::header::HeaderName>()
                    .map_err(|err| format!("invalid request header name: {err}"))?;
                let value = HeaderValue::from_str(&value)
                    .map_err(|err| format!("invalid request header value: {err}"))?;
                request_headers.insert(name, value);
            }
            let response = self
                .client
                .post(url)
                .headers(request_headers)
                .json(&body)
                .send()
                .await
                .map_err(|err| format!("websearch request failed: {err}"))?;
            let status = response.status().as_u16();
            let headers = response_headers(response.headers());
            let body = read_response_bytes(response, max_response_bytes).await?;
            Ok(WebHttpResponse {
                status,
                headers,
                body,
            })
        })
    }

    fn get<'a>(&'a self, url: Url, max_response_bytes: usize) -> WebHttpFuture<'a> {
        Box::pin(async move {
            let response = self
                .client
                .get(url)
                .header(
                    ACCEPT,
                    "text/plain;q=1.0, text/html;q=0.9, application/json;q=0.8, application/xml;q=0.7, */*;q=0.1",
                )
                .header("user-agent", "squeezy/0.1")
                .send()
                .await
                .map_err(|err| format!("webfetch request failed: {err}"))?;
            let status = response.status().as_u16();
            let headers = response_headers(response.headers());
            let body = read_response_bytes(response, max_response_bytes).await?;
            Ok(WebHttpResponse {
                status,
                headers,
                body,
            })
        })
    }
}

fn response_headers(headers: &HeaderMap) -> BTreeMap<String, String> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_ascii_lowercase(), value.to_string()))
        })
        .collect()
}

#[derive(Clone)]
pub struct ToolRegistry {
    root: Arc<PathBuf>,
    output_store: Arc<ToolOutputStore>,
    web_config: Arc<WebToolConfig>,
    http: Arc<dyn WebHttpClient>,
    graph: Arc<StdMutex<Option<GraphManager>>>,
    vcs: Arc<GitVcs>,
    checkpoints: Arc<CheckpointStore>,
    diff_cache: Arc<StdMutex<DiffSnapshotCache>>,
    skills: Arc<SkillCatalog>,
    redactor: Arc<Redactor>,
    crawl_options: Arc<CrawlOptions>,
    compiled_policy: Arc<CompiledIndexingPolicy>,
    shell_sandbox: Arc<ShellSandboxConfig>,
    shell_audit: Arc<ShellAuditStore>,
}

#[derive(Debug, Default)]
struct DiffSnapshotCache {
    entries: HashMap<DiffSnapshotKey, CachedDiffSnapshot>,
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
struct DiffSnapshotKey {
    mode: DiffMode,
    include_patch: bool,
    max_patch_bytes: usize,
}

#[derive(Debug)]
struct CachedDiffSnapshot {
    snapshot: Arc<DiffSnapshot>,
    fetched_at: Instant,
}

const SHELL_AUDIT_MAX_BYTES: u64 = 8 * 1024 * 1024;
const SHELL_AUDIT_RETAINED_ROTATIONS: usize = 4;

/// Append-only JSONL store for shell audit records.
///
/// Each entry is serialised to a single `Vec<u8>` (terminated by `\n`) and
/// written through a single `write_all` call under a process-wide
/// `Mutex<()>`. Two interleaved concurrent calls therefore produce two
/// distinct lines, not one corrupted hybrid. When the file exceeds
/// `SHELL_AUDIT_MAX_BYTES`, the current file is rotated to a numbered
/// suffix and a fresh log is started, keeping at most
/// `SHELL_AUDIT_RETAINED_ROTATIONS` archived files.
#[derive(Debug)]
struct ShellAuditStore {
    path: PathBuf,
    lock: StdMutex<()>,
}

impl ShellAuditStore {
    fn new(root: &Path) -> Self {
        Self {
            path: root.join(".squeezy").join("audit").join("shell.jsonl"),
            lock: StdMutex::new(()),
        }
    }

    fn append(&self, entry: &Value) -> std::io::Result<()> {
        let mut payload = serde_json::to_vec(entry)?;
        payload.push(b'\n');

        let _guard = self.lock.lock().unwrap_or_else(|err| err.into_inner());

        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        self.maybe_rotate(payload.len() as u64)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(&payload)
    }

    fn maybe_rotate(&self, incoming_bytes: u64) -> std::io::Result<()> {
        let metadata = match fs::metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err),
        };
        if metadata.len() + incoming_bytes <= SHELL_AUDIT_MAX_BYTES {
            return Ok(());
        }
        // Shift suffix N-1 → N, dropping the oldest.
        for i in (1..SHELL_AUDIT_RETAINED_ROTATIONS).rev() {
            let src = self.rotated_path(i);
            let dst = self.rotated_path(i + 1);
            if src.exists() {
                let _ = fs::rename(&src, &dst);
            }
        }
        fs::rename(&self.path, self.rotated_path(1))?;
        Ok(())
    }

    fn rotated_path(&self, index: usize) -> PathBuf {
        let mut name = self
            .path
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_default();
        name.push(format!(".{index}"));
        self.path.with_file_name(name)
    }
}

#[derive(Debug, Clone)]
struct ShellSandboxPlan {
    program: String,
    args: Vec<String>,
    backend: &'static str,
    mode: &'static str,
    network: &'static str,
    required: bool,
}

impl ShellSandboxPlan {
    fn direct(command: &str, mode: ShellSandboxMode) -> Self {
        Self {
            program: "sh".to_string(),
            args: vec!["-lc".to_string(), command.to_string()],
            backend: "none",
            mode: mode.as_str(),
            network: "not_enforced",
            required: false,
        }
    }

    fn metadata(&self) -> Value {
        json!({
            "backend": self.backend,
            "mode": self.mode,
            "network": self.network,
            "required": self.required,
        })
    }
}

impl ToolRegistry {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        Self::new_with_configs(root, ToolOutputConfig::default(), WebToolConfig::default())
    }

    pub fn new_with_output_config(
        root: impl Into<PathBuf>,
        output_config: ToolOutputConfig,
    ) -> Result<Self> {
        Self::new_with_configs(root, output_config, WebToolConfig::default())
    }

    pub fn new_with_configs(
        root: impl Into<PathBuf>,
        output_config: ToolOutputConfig,
        web_config: WebToolConfig,
    ) -> Result<Self> {
        Self::new_inner(
            root,
            output_config,
            web_config,
            ShellSandboxConfig::default(),
            SkillCatalog::empty(),
            CrawlOptions::default(),
            Arc::new(Redactor::default()),
        )
    }

    pub fn new_with_graph_config(
        root: impl Into<PathBuf>,
        output_config: ToolOutputConfig,
        web_config: WebToolConfig,
        graph_config: &GraphConfig,
    ) -> Result<Self> {
        Self::new_inner(
            root,
            output_config,
            web_config,
            ShellSandboxConfig::default(),
            SkillCatalog::empty(),
            crawl_options_from_graph_config(graph_config),
            Arc::new(Redactor::default()),
        )
    }

    pub fn new_with_configs_and_skills(
        root: impl Into<PathBuf>,
        output_config: ToolOutputConfig,
        web_config: WebToolConfig,
        skills_config: SkillsConfig,
        graph_config: &GraphConfig,
        shell_sandbox: ShellSandboxConfig,
        redactor: Arc<Redactor>,
    ) -> Result<Self> {
        let root = root.into();
        let root = root
            .canonicalize()
            .map_err(|err| SqueezyError::Tool(format!("invalid workspace root: {err}")))?;
        let skills = SkillCatalog::discover(&root, &skills_config);
        Self::new_inner_canonical(
            root,
            output_config,
            web_config,
            shell_sandbox,
            skills,
            crawl_options_from_graph_config(graph_config),
            redactor,
        )
    }

    fn new_inner(
        root: impl Into<PathBuf>,
        output_config: ToolOutputConfig,
        web_config: WebToolConfig,
        shell_sandbox: ShellSandboxConfig,
        skills: SkillCatalog,
        crawl_options: CrawlOptions,
        redactor: Arc<Redactor>,
    ) -> Result<Self> {
        let root = root.into();
        let root = root
            .canonicalize()
            .map_err(|err| SqueezyError::Tool(format!("invalid workspace root: {err}")))?;
        Self::new_inner_canonical(
            root,
            output_config,
            web_config,
            shell_sandbox,
            skills,
            crawl_options,
            redactor,
        )
    }

    fn new_inner_canonical(
        root: PathBuf,
        output_config: ToolOutputConfig,
        web_config: WebToolConfig,
        shell_sandbox: ShellSandboxConfig,
        skills: SkillCatalog,
        crawl_options: CrawlOptions,
        redactor: Arc<Redactor>,
    ) -> Result<Self> {
        let output_store = ToolOutputStore::new(&root, output_config)?;
        let http = Arc::new(ReqwestWebHttpClient::new()?);
        // Compile the policy once up front. Invalid user globs surface as a
        // `SqueezyError::Config` here instead of silently disabling the
        // policy on every hot-path call.
        let compiled_policy = Arc::new(crawl_options.policy.compile()?);
        let graph =
            GraphManager::open_with_crawl_options(&root, Default::default(), crawl_options.clone())
                .ok();
        let vcs = GitVcs::open(&root)?;
        let shell_audit = ShellAuditStore::new(&root);
        let checkpoints = CheckpointStore::open(&root)?;
        Ok(Self {
            root: Arc::new(root),
            output_store: Arc::new(output_store),
            web_config: Arc::new(web_config.normalized()),
            http,
            graph: Arc::new(StdMutex::new(graph)),
            vcs: Arc::new(vcs),
            checkpoints: Arc::new(checkpoints),
            diff_cache: Arc::new(StdMutex::new(DiffSnapshotCache::default())),
            skills: Arc::new(skills),
            redactor,
            crawl_options: Arc::new(crawl_options),
            compiled_policy,
            shell_sandbox: Arc::new(shell_sandbox),
            shell_audit: Arc::new(shell_audit),
        })
    }

    #[cfg(test)]
    fn new_with_http_client(
        root: impl Into<PathBuf>,
        output_config: ToolOutputConfig,
        web_config: WebToolConfig,
        http: Arc<dyn WebHttpClient>,
    ) -> Result<Self> {
        let root = root.into();
        let root = root
            .canonicalize()
            .map_err(|err| SqueezyError::Tool(format!("invalid workspace root: {err}")))?;
        let output_store = ToolOutputStore::new(&root, output_config)?;
        let crawl_options = CrawlOptions::default();
        let compiled_policy = Arc::new(crawl_options.policy.compile()?);
        let graph =
            GraphManager::open_with_crawl_options(&root, Default::default(), crawl_options.clone())
                .ok();
        let vcs = GitVcs::open(&root)?;
        let shell_audit = ShellAuditStore::new(&root);
        let checkpoints = CheckpointStore::open(&root)?;
        Ok(Self {
            root: Arc::new(root),
            output_store: Arc::new(output_store),
            web_config: Arc::new(web_config.normalized()),
            http,
            graph: Arc::new(StdMutex::new(graph)),
            vcs: Arc::new(vcs),
            checkpoints: Arc::new(checkpoints),
            diff_cache: Arc::new(StdMutex::new(DiffSnapshotCache::default())),
            skills: Arc::new(SkillCatalog::empty()),
            redactor: Arc::new(Redactor::default()),
            crawl_options: Arc::new(crawl_options),
            compiled_policy,
            shell_sandbox: Arc::new(ShellSandboxConfig::default()),
            shell_audit: Arc::new(shell_audit),
        })
    }

    fn diff_snapshot(&self, mode: DiffMode, options: DiffOptions) -> DiffSnapshot {
        let key = DiffSnapshotKey {
            mode,
            include_patch: options.include_patch,
            max_patch_bytes: options.max_patch_bytes,
        };
        if let Ok(cache) = self.diff_cache.lock()
            && let Some(entry) = cache.entries.get(&key)
            && entry.fetched_at.elapsed() < DIFF_SNAPSHOT_TTL
        {
            return (*entry.snapshot).clone();
        }
        let snapshot = self.vcs.snapshot(mode, options);
        if let Ok(mut cache) = self.diff_cache.lock() {
            cache.entries.insert(
                key,
                CachedDiffSnapshot {
                    snapshot: Arc::new(snapshot.clone()),
                    fetched_at: Instant::now(),
                },
            );
        }
        snapshot
    }

    fn invalidate_diff_cache(&self) {
        if let Ok(mut cache) = self.diff_cache.lock() {
            cache.entries.clear();
        }
    }

    fn prepare_shell_sandbox(
        &self,
        command: &str,
        analysis: &ShellPermissionAnalysis,
    ) -> std::result::Result<ShellSandboxPlan, String> {
        match self.shell_sandbox.mode {
            ShellSandboxMode::Off => Ok(ShellSandboxPlan::direct(command, ShellSandboxMode::Off)),
            ShellSandboxMode::BestEffort | ShellSandboxMode::Required => {
                prepare_shell_sandbox_plan(command, analysis, &self.root, &self.shell_sandbox)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn audit_shell(
        &self,
        call: &ToolCall,
        args: &ShellArgs,
        workdir: &Path,
        analysis: &ShellPermissionAnalysis,
        sandbox: Value,
        timeout_ms: u64,
        output_cap: usize,
        outcome: &str,
        reason: Option<&str>,
        exit_code: Option<i32>,
        stdout: &[u8],
        stderr: &[u8],
    ) {
        if !self.shell_sandbox.audit {
            return;
        }
        let env_names = self.shell_sandbox.env_allowlist.clone();
        let entry = json!({
            "ts_unix_ms": unix_timestamp_millis(SystemTime::now()),
            "call_id": call.call_id,
            "tool": call.name,
            "command": truncate_text(&self.redactor.redact(&args.command).text, 500),
            "cwd": self.relative(workdir).to_string_lossy(),
            "description": args.description.as_deref().map(|value| self.redactor.redact(value).text),
            "classification": {
                "capability": analysis.capability.as_str(),
                "target": analysis.rule_target,
                "risk": analysis.risk.as_str(),
                "network": analysis.network,
                "destructive": analysis.destructive,
                "parser_backed": analysis.parser_backed,
                "dynamic": analysis.dynamic,
            },
            "sandbox": sandbox,
            "env": {
                "policy": "allowlist",
                "names": env_names,
            },
            "limits": {
                "timeout_ms": timeout_ms,
                "output_byte_cap": output_cap,
            },
            "outcome": outcome,
            "reason": reason,
            "exit_code": exit_code,
            "output": {
                "stdout_bytes": stdout.len(),
                "stderr_bytes": stderr.len(),
                "stdout_sha256": sha256_hex(stdout),
                "stderr_sha256": sha256_hex(stderr),
            },
        });
        let _ = self.shell_audit.append(&entry);
    }

    /// Cheap permission-scope predicate. Looks only at the user-supplied
    /// path string: no file I/O, no canonicalization, no glob recompile.
    /// Files that are excluded by *content* (binary / generated) but live
    /// at a perfectly normal path still receive the regular Read scope and
    /// are surfaced to the model via `ignored=true` from `execute_read_file`.
    fn read_file_targets_ignored_policy(&self, arguments: &Value) -> bool {
        let Ok(args) = serde_json::from_value::<ReadFileArgs>(arguments.clone()) else {
            return false;
        };
        let normalized = args.path.replace('\\', "/");
        self.compiled_policy
            .path_reason(&normalized, false)
            .is_some()
    }

    fn policy_exclusion_for_file(
        &self,
        path: &Path,
        rel: &Path,
        prefix: Option<&[u8]>,
    ) -> Option<ExclusionReason> {
        let size_bytes = file_len(path).ok()?;
        self.compiled_policy.file_reason(
            &rel.to_string_lossy(),
            size_bytes,
            self.crawl_options.max_file_bytes,
            prefix,
        )
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = vec![
            checkpoint_list_spec(),
            checkpoint_revert_spec(),
            checkpoint_show_spec(),
            checkpoint_undo_spec(),
            diff_context_spec(),
            glob_spec(),
            grep_spec(),
            read_file_spec(),
            read_tool_output_spec(),
            write_file_spec(),
            symbol_context_spec(),
            verify_spec(),
            shell_spec(),
            webfetch_spec(),
            websearch_spec(),
            list_skills_spec(),
            load_skill_spec(),
        ];
        specs.sort_by(|left, right| left.name.cmp(&right.name));
        specs
    }

    pub fn permission_scope(&self, call: &ToolCall) -> PermissionScope {
        match call.name.as_str() {
            "checkpoint_undo" | "checkpoint_revert" => PermissionScope::Edit,
            "write_file" => PermissionScope::Edit,
            "shell" | "verify" => PermissionScope::Shell,
            "webfetch" | "websearch" => PermissionScope::Web,
            "glob" if tool_include_ignored(&call.arguments) => PermissionScope::IgnoredSearch,
            "grep" if grep_include_ignored(&call.arguments) => PermissionScope::IgnoredSearch,
            "read_file" if self.read_file_targets_ignored_policy(&call.arguments) => {
                PermissionScope::IgnoredSearch
            }
            "checkpoint_list" | "checkpoint_show" | "diff_context" | "glob" | "grep"
            | "read_file" | "read_tool_output" | "symbol_context" | "list_skills"
            | "load_skill" => PermissionScope::Read,
            _ => PermissionScope::Read,
        }
    }

    pub fn permission_request(&self, call: &ToolCall) -> PermissionRequest {
        let mut metadata = BTreeMap::new();
        let mut suggested_rules = Vec::new();
        let summary = self.describe_call(call);
        let (capability, target, risk) = match call.name.as_str() {
            "checkpoint_undo" | "checkpoint_revert" => (
                PermissionCapability::Edit,
                "workspace:*".to_string(),
                PermissionRisk::High,
            ),
            "write_file" => {
                let args = serde_json::from_value::<WriteFileArgs>(call.arguments.clone()).ok();
                let path = args.as_ref().map(|args| args.path.as_str()).unwrap_or("*");
                let rule_target = format!("path:{path}");
                metadata.insert("path".to_string(), path.to_string());
                suggested_rules.push(PermissionRule::new(
                    "edit",
                    rule_target.clone(),
                    PermissionMode::Allow,
                    PermissionRuleSource::Session,
                    Some("approved edit path".to_string()),
                ));
                (
                    PermissionCapability::Edit,
                    rule_target,
                    PermissionRisk::High,
                )
            }
            "shell" => {
                let args = serde_json::from_value::<ShellArgs>(call.arguments.clone()).ok();
                let command = args
                    .as_ref()
                    .map(|args| args.command.as_str())
                    .unwrap_or("");
                let analysis = analyze_shell_command(command);
                let workdir = args
                    .as_ref()
                    .and_then(|args| args.workdir.as_deref())
                    .unwrap_or(".");
                metadata.insert("command".to_string(), command.to_string());
                metadata.insert("cwd".to_string(), workdir.to_string());
                metadata.insert("shell_prefix".to_string(), analysis.rule_target.clone());
                metadata.insert("env".to_string(), "allowlist (values redacted)".to_string());
                metadata.insert(
                    "network".to_string(),
                    if analysis.network {
                        "classified".to_string()
                    } else {
                        "none".to_string()
                    },
                );
                metadata.insert("destructive".to_string(), analysis.destructive.to_string());
                metadata.insert(
                    "parser_backed".to_string(),
                    analysis.parser_backed.to_string(),
                );
                metadata.insert("dynamic".to_string(), analysis.dynamic.to_string());
                metadata.insert(
                    "sandbox".to_string(),
                    self.shell_sandbox.mode.as_str().to_string(),
                );
                metadata.insert(
                    "sandbox_network".to_string(),
                    self.shell_sandbox.network.as_str().to_string(),
                );
                if let Some(timeout_ms) = args.as_ref().and_then(|args| args.timeout_ms) {
                    metadata.insert("timeout_ms".to_string(), timeout_ms.to_string());
                }
                if let Some(output_byte_cap) = args.as_ref().and_then(|args| args.output_byte_cap) {
                    metadata.insert("output_byte_cap".to_string(), output_byte_cap.to_string());
                }
                if let Some(description) =
                    args.as_ref().and_then(|args| args.description.as_deref())
                {
                    metadata.insert("description".to_string(), description.to_string());
                }
                suggested_rules.push(PermissionRule::new(
                    analysis.capability.as_str(),
                    analysis.rule_target.clone(),
                    PermissionMode::Allow,
                    PermissionRuleSource::Session,
                    Some("approved shell command prefix".to_string()),
                ));
                (analysis.capability, analysis.rule_target, analysis.risk)
            }
            "verify" => {
                let target = "cargo verify:*".to_string();
                suggested_rules.push(PermissionRule::new(
                    "compiler",
                    target.clone(),
                    PermissionMode::Allow,
                    PermissionRuleSource::Session,
                    Some("approved verification command family".to_string()),
                ));
                (
                    PermissionCapability::Compiler,
                    target,
                    PermissionRisk::Medium,
                )
            }
            "webfetch" => {
                let args = serde_json::from_value::<WebFetchArgs>(call.arguments.clone()).ok();
                let target = args
                    .as_ref()
                    .and_then(|args| web_url_host(&args.url).ok())
                    .map(|host| format!("domain:{host}"))
                    .unwrap_or_else(|| "domain:*".to_string());
                suggested_rules.push(PermissionRule::new(
                    "network",
                    target.clone(),
                    PermissionMode::Allow,
                    PermissionRuleSource::Session,
                    Some("approved web domain".to_string()),
                ));
                (
                    PermissionCapability::Network,
                    target,
                    PermissionRisk::Medium,
                )
            }
            "websearch" => {
                let args = serde_json::from_value::<WebSearchArgs>(call.arguments.clone()).ok();
                let query = args.as_ref().map(|args| args.query.as_str()).unwrap_or("*");
                metadata.insert("query".to_string(), truncate_text(query, 200));
                (
                    PermissionCapability::Network,
                    "search:exa".to_string(),
                    PermissionRisk::Medium,
                )
            }
            "glob" if tool_include_ignored(&call.arguments) => (
                PermissionCapability::Search,
                "ignored:*".to_string(),
                PermissionRisk::Medium,
            ),
            "grep" if grep_include_ignored(&call.arguments) => (
                PermissionCapability::Search,
                "ignored:*".to_string(),
                PermissionRisk::Medium,
            ),
            "grep" | "glob" => (
                PermissionCapability::Search,
                "workspace:*".to_string(),
                PermissionRisk::Low,
            ),
            "checkpoint_list" | "checkpoint_show" | "diff_context" | "read_file"
            | "read_tool_output" | "symbol_context" | "list_skills" | "load_skill" => (
                PermissionCapability::Read,
                "workspace:*".to_string(),
                PermissionRisk::Low,
            ),
            _ => (
                PermissionCapability::Read,
                format!("tool:{}", call.name),
                PermissionRisk::Medium,
            ),
        };
        PermissionRequest {
            call_id: call.call_id.clone(),
            tool_name: call.name.clone(),
            capability,
            target,
            risk,
            summary,
            metadata,
            suggested_rules,
        }
    }

    pub fn is_parallel_safe(&self, call: &ToolCall) -> bool {
        matches!(
            call.name.as_str(),
            "checkpoint_list"
                | "checkpoint_show"
                | "diff_context"
                | "glob"
                | "grep"
                | "read_file"
                | "read_tool_output"
                | "symbol_context"
                | "webfetch"
                | "websearch"
                | "list_skills"
                | "load_skill"
        )
    }

    pub fn describe_call(&self, call: &ToolCall) -> String {
        match call.name.as_str() {
            "checkpoint_list" => "checkpoint_list".to_string(),
            "checkpoint_show" => {
                let args =
                    serde_json::from_value::<CheckpointShowArgs>(call.arguments.clone()).ok();
                let checkpoint_id = args
                    .as_ref()
                    .map(|args| args.checkpoint_id.as_str())
                    .unwrap_or("?");
                format!("checkpoint_show checkpoint_id={checkpoint_id:?}")
            }
            "checkpoint_undo" => "checkpoint_undo".to_string(),
            "checkpoint_revert" => {
                let args =
                    serde_json::from_value::<CheckpointRevertArgs>(call.arguments.clone()).ok();
                let group_id = args.as_ref().and_then(|args| args.group_id.as_deref());
                let checkpoint_id = args.as_ref().and_then(|args| args.checkpoint_id.as_deref());
                match (group_id, checkpoint_id) {
                    (Some(group_id), None) => format!("checkpoint_revert group_id={group_id:?}"),
                    (None, Some(checkpoint_id)) => {
                        format!("checkpoint_revert checkpoint_id={checkpoint_id:?}")
                    }
                    (Some(group_id), Some(checkpoint_id)) => format!(
                        "checkpoint_revert group_id={group_id:?} checkpoint_id={checkpoint_id:?}"
                    ),
                    (None, None) => "checkpoint_revert".to_string(),
                }
            }
            "glob" => {
                let args = serde_json::from_value::<GlobArgs>(call.arguments.clone()).ok();
                let pattern = args
                    .as_ref()
                    .map(|args| args.pattern.as_str())
                    .unwrap_or("?");
                let path = args
                    .as_ref()
                    .and_then(|args| args.path.as_deref())
                    .unwrap_or(".");
                format!("glob pattern={pattern:?} path={path:?}")
            }
            "diff_context" => {
                let args = serde_json::from_value::<DiffContextArgs>(call.arguments.clone()).ok();
                let mode = args
                    .as_ref()
                    .and_then(|args| args.mode)
                    .map(diff_mode_str)
                    .unwrap_or("worktree");
                format!("diff_context mode={mode:?}")
            }
            "grep" => {
                let args = serde_json::from_value::<GrepArgs>(call.arguments.clone()).ok();
                let pattern = args
                    .as_ref()
                    .map(|args| args.pattern.as_str())
                    .unwrap_or("?");
                let path = args
                    .as_ref()
                    .and_then(|args| args.path.as_deref())
                    .unwrap_or(".");
                format!("grep pattern={pattern:?} path={path:?}")
            }
            "read_file" => {
                let args = serde_json::from_value::<ReadFileArgs>(call.arguments.clone()).ok();
                let path = args.as_ref().map(|args| args.path.as_str()).unwrap_or("?");
                format!("read_file path={path:?}")
            }
            "read_tool_output" => {
                let args =
                    serde_json::from_value::<ReadToolOutputArgs>(call.arguments.clone()).ok();
                let handle = args
                    .as_ref()
                    .map(|args| args.handle.as_str())
                    .unwrap_or("?");
                format!("read_tool_output handle={handle:?}")
            }
            "symbol_context" => {
                let args = serde_json::from_value::<SymbolContextArgs>(call.arguments.clone()).ok();
                let query = args.as_ref().map(|args| args.query.as_str()).unwrap_or("?");
                format!("symbol_context query={query:?}")
            }
            "verify" => {
                let args = serde_json::from_value::<VerifyArgs>(call.arguments.clone()).ok();
                let scope = args
                    .as_ref()
                    .and_then(|args| args.scope)
                    .map(verify_scope_str)
                    .unwrap_or("diff");
                let level = args
                    .as_ref()
                    .and_then(|args| args.level)
                    .map(verify_level_str)
                    .unwrap_or("quick");
                format!("verify scope={scope:?} level={level:?}")
            }
            "write_file" => {
                let args = serde_json::from_value::<WriteFileArgs>(call.arguments.clone()).ok();
                let path = args.as_ref().map(|args| args.path.as_str()).unwrap_or("?");
                format!("write_file path={path:?}")
            }
            "shell" => {
                let args = serde_json::from_value::<ShellArgs>(call.arguments.clone()).ok();
                let description = args
                    .as_ref()
                    .and_then(|args| args.description.as_deref())
                    .unwrap_or("run shell command");
                // Only the description goes in the summary now; the rest
                // (command, cwd, env policy, network, destructive, …) is
                // emitted via `permission_request().metadata` so the UI
                // doesn't render the same field twice.
                format!("shell description={description:?}")
            }
            "webfetch" => {
                let args = serde_json::from_value::<WebFetchArgs>(call.arguments.clone()).ok();
                let host = args
                    .as_ref()
                    .and_then(|args| web_url_host(&args.url).ok())
                    .unwrap_or_else(|| "?".to_string());
                format!("webfetch host={host:?}")
            }
            "websearch" => {
                let args = serde_json::from_value::<WebSearchArgs>(call.arguments.clone()).ok();
                let query = args.as_ref().map(|args| args.query.as_str()).unwrap_or("?");
                format!("websearch query={query:?}")
            }
            "list_skills" => "list_skills".to_string(),
            "load_skill" => {
                let args = serde_json::from_value::<LoadSkillArgs>(call.arguments.clone()).ok();
                let name = args.as_ref().map(|args| args.name.as_str()).unwrap_or("?");
                format!("load_skill name={name:?}")
            }
            _ => format!("{} {}", call.name, call.arguments),
        }
    }

    pub fn activate_skills_for_input(&self, input: &str) -> Result<SkillActivation> {
        self.skills.activate_for_input(input)
    }

    pub fn format_active_skills(&self, skills: &[LoadedSkill]) -> Option<String> {
        if skills.is_empty() {
            return None;
        }
        Some(format!(
            "<active_skills>\n{}\n</active_skills>",
            skills
                .iter()
                .map(LoadedSkill::prompt_block)
                .collect::<Vec<_>>()
                .join("\n")
        ))
    }

    pub async fn execute(&self, call: ToolCall, cancel: CancellationToken) -> ToolResult {
        self.execute_for_group(call, cancel, "manual".to_string())
            .await
    }

    pub async fn execute_for_group(
        &self,
        call: ToolCall,
        cancel: CancellationToken,
        group_id: String,
    ) -> ToolResult {
        if cancel.is_cancelled() {
            return ToolResult::cancelled(&call);
        }

        let result = match call.name.as_str() {
            "checkpoint_list" => self.execute_checkpoint_list(&call).await,
            "checkpoint_show" => self.execute_checkpoint_show(&call).await,
            "checkpoint_undo" => self.execute_checkpoint_undo(&call).await,
            "checkpoint_revert" => self.execute_checkpoint_revert(&call).await,
            "diff_context" => self.execute_diff_context(&call).await,
            "glob" => self.execute_glob(&call, cancel).await,
            "grep" => self.execute_grep(&call, cancel).await,
            "read_file" => self.execute_read_file(&call).await,
            "read_tool_output" => self.execute_read_tool_output(&call).await,
            "symbol_context" => self.execute_symbol_context(&call).await,
            "verify" => self.execute_verify(&call, cancel, &group_id).await,
            "write_file" => self.execute_write_file(&call, &group_id).await,
            "shell" => self.execute_shell(&call, cancel, &group_id).await,
            "webfetch" => self.execute_webfetch(&call, cancel).await,
            "websearch" => self.execute_websearch(&call, cancel).await,
            "list_skills" => self.execute_list_skills(&call).await,
            "load_skill" => self.execute_load_skill(&call).await,
            _ => make_result(
                &call,
                ToolStatus::Error,
                json!({ "error": format!("unknown tool: {}", call.name) }),
                ToolCostHint::default(),
                None,
            ),
        };

        if call.name == "read_tool_output" {
            result
        } else {
            self.finalize_result(result)
        }
    }

    async fn execute_checkpoint_list(&self, call: &ToolCall) -> ToolResult {
        if let Err(err) = serde_json::from_value::<CheckpointListArgs>(call.arguments.clone()) {
            return tool_arg_error(call, err);
        }
        match self.checkpoints.read_journal() {
            Ok(journal) => {
                let mut checkpoints = journal.checkpoints;
                checkpoints.sort_by_key(|record| std::cmp::Reverse(record.created_at_ms));
                make_result(
                    call,
                    ToolStatus::Success,
                    json!({
                        "checkpoints": checkpoints,
                        "journal_warnings": journal.journal_warnings,
                    }),
                    ToolCostHint {
                        matches_returned: checkpoints.len() as u64,
                        ..ToolCostHint::default()
                    },
                    None,
                )
            }
            Err(err) => tool_error(call, err),
        }
    }

    async fn execute_checkpoint_show(&self, call: &ToolCall) -> ToolResult {
        let args = match serde_json::from_value::<CheckpointShowArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        match self.checkpoints.show_checkpoint(&args.checkpoint_id) {
            Ok(Some(checkpoint)) => make_result(
                call,
                ToolStatus::Success,
                json!({ "checkpoint": checkpoint }),
                ToolCostHint::default(),
                None,
            ),
            Ok(None) => make_result(
                call,
                ToolStatus::Stale,
                json!({
                    "error": "checkpoint not found",
                    "checkpoint_id": args.checkpoint_id,
                }),
                ToolCostHint::default(),
                None,
            ),
            Err(err) => tool_error(call, err),
        }
    }

    async fn execute_checkpoint_undo(&self, call: &ToolCall) -> ToolResult {
        let args = match serde_json::from_value::<CheckpointUndoArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        match self
            .checkpoints
            .rollback(RollbackTarget::Latest, args.mode.unwrap_or_default())
        {
            Ok(result) => {
                self.invalidate_diff_cache();
                make_result(
                    call,
                    if result.conflicts.is_empty() && !result.skipped && result.applied {
                        ToolStatus::Success
                    } else {
                        ToolStatus::Stale
                    },
                    json!({ "rollback": result }),
                    ToolCostHint::default(),
                    None,
                )
            }
            Err(err) => tool_error(call, err),
        }
    }

    async fn execute_checkpoint_revert(&self, call: &ToolCall) -> ToolResult {
        let args = match serde_json::from_value::<CheckpointRevertArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        let target = match (args.group_id.as_deref(), args.checkpoint_id.as_deref()) {
            (Some(group_id), None) => RollbackTarget::Group(group_id),
            (None, Some(checkpoint_id)) => RollbackTarget::Checkpoint(checkpoint_id),
            _ => {
                return tool_error(
                    call,
                    "provide exactly one of group_id or checkpoint_id for checkpoint_revert",
                );
            }
        };
        match self
            .checkpoints
            .rollback(target, args.mode.unwrap_or_default())
        {
            Ok(result) => {
                self.invalidate_diff_cache();
                make_result(
                    call,
                    if result.conflicts.is_empty() && !result.skipped && result.applied {
                        ToolStatus::Success
                    } else {
                        ToolStatus::Stale
                    },
                    json!({ "rollback": result }),
                    ToolCostHint::default(),
                    None,
                )
            }
            Err(err) => tool_error(call, err),
        }
    }

    async fn execute_diff_context(&self, call: &ToolCall) -> ToolResult {
        let args = match serde_json::from_value::<DiffContextArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        let registry = self.clone();
        let call = call.clone();
        tokio::task::spawn_blocking(move || registry.execute_diff_context_blocking(&call, args))
            .await
            .unwrap_or_else(|err| {
                make_result(
                    &ToolCall {
                        call_id: String::new(),
                        name: "diff_context".to_string(),
                        arguments: Value::Null,
                    },
                    ToolStatus::Error,
                    json!({ "error": format!("diff_context join failed: {err}") }),
                    ToolCostHint::default(),
                    None,
                )
            })
    }

    fn execute_diff_context_blocking(&self, call: &ToolCall, args: DiffContextArgs) -> ToolResult {
        let max_patch_bytes = args.max_patch_bytes.unwrap_or(1_000_000).min(5_000_000);
        let snapshot = self.diff_snapshot(
            args.mode.unwrap_or_default(),
            DiffOptions {
                include_patch: args.include_patch.unwrap_or(false),
                max_patch_bytes,
            },
        );
        let max_files = args.max_files.unwrap_or(100).min(500);
        let max_symbols_per_file = args.max_symbols_per_file.unwrap_or(12).min(100);
        let max_references = args.max_references_per_symbol.unwrap_or(8).min(50);
        let graph_context =
            self.graph_context_for_snapshot(&snapshot, max_symbols_per_file, max_references);
        let files = snapshot
            .files
            .iter()
            .take(max_files)
            .map(diff_file_json)
            .collect::<Vec<_>>();
        let truncated = snapshot.truncated || snapshot.files.len() > max_files;

        make_result(
            call,
            ToolStatus::Success,
            json!({
                "vcs": snapshot.vcs,
                "mode": diff_mode_str(snapshot.mode),
                "summary": snapshot.summary,
                "files": files,
                "graph": graph_context,
                "truncated": truncated,
                "errors": snapshot.errors,
            }),
            ToolCostHint {
                matches_returned: snapshot.files.len().min(max_files) as u64,
                truncated,
                ..ToolCostHint::default()
            },
            None,
        )
    }

    async fn execute_list_skills(&self, call: &ToolCall) -> ToolResult {
        if let Err(err) = serde_json::from_value::<ListSkillsArgs>(call.arguments.clone()) {
            return tool_arg_error(call, err);
        }
        make_result(
            call,
            ToolStatus::Success,
            self.skills.summaries_json(),
            ToolCostHint::default(),
            None,
        )
    }

    async fn execute_symbol_context(&self, call: &ToolCall) -> ToolResult {
        let args = match serde_json::from_value::<SymbolContextArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        let registry = self.clone();
        let call = call.clone();
        tokio::task::spawn_blocking(move || registry.execute_symbol_context_blocking(&call, args))
            .await
            .unwrap_or_else(|err| {
                make_result(
                    &ToolCall {
                        call_id: String::new(),
                        name: "symbol_context".to_string(),
                        arguments: Value::Null,
                    },
                    ToolStatus::Error,
                    json!({ "error": format!("symbol_context join failed: {err}") }),
                    ToolCostHint::default(),
                    None,
                )
            })
    }

    fn execute_symbol_context_blocking(
        &self,
        call: &ToolCall,
        args: SymbolContextArgs,
    ) -> ToolResult {
        let mode = args.mode.unwrap_or_default();
        let snapshot = self.diff_snapshot(mode, DiffOptions::default());
        let dirty_paths = diff_path_set(&snapshot);
        let max_references = args.max_references.unwrap_or(12).min(50);
        let mut graph = match self.graph.lock() {
            Ok(graph) => graph,
            Err(_) => {
                return make_result(
                    call,
                    ToolStatus::Error,
                    json!({"error": "semantic graph lock poisoned"}),
                    ToolCostHint::default(),
                    None,
                );
            }
        };
        let Some(manager) = graph.as_mut() else {
            return make_result(
                call,
                ToolStatus::Success,
                json!({
                    "query": args.query,
                    "symbols": [],
                    "graph_available": false,
                    "reason": "semantic graph is unavailable for this workspace",
                }),
                ToolCostHint::default(),
                None,
            );
        };
        let _ = manager.refresh_before_query();
        annotate_graph(manager, &snapshot);
        let graph = manager.graph();
        let path_filter = args.path.as_deref();
        let diff_only = args.diff_only.unwrap_or(false);
        let mut symbols = graph
            .signature_search(&SignatureQuery {
                text: args.query.clone(),
                kind: None,
                visibility: None,
                attribute: None,
            })
            .into_iter()
            .filter(|symbol| symbol_matches_path_filter(symbol, path_filter))
            .filter(|symbol| {
                !diff_only || symbol.dirty.is_some() || dirty_paths.contains(&symbol.file_id.0)
            })
            .take(25)
            .collect::<Vec<_>>();
        if symbols.is_empty() && diff_only {
            symbols = graph
                .dirty_symbols()
                .into_iter()
                .filter(|symbol| symbol_matches_path_filter(symbol, path_filter))
                .filter(|symbol| {
                    symbol.name.contains(&args.query) || symbol.signature.contains(&args.query)
                })
                .take(25)
                .collect();
        }
        let content = symbols
            .iter()
            .map(|symbol| symbol_context_json(graph, symbol, max_references))
            .collect::<Vec<_>>();
        let mut payload = serde_json::Map::new();
        payload.insert("query".to_string(), json!(args.query));
        payload.insert("mode".to_string(), json!(diff_mode_str(mode)));
        payload.insert("diff_only".to_string(), json!(diff_only));
        payload.insert("symbols".to_string(), json!(content));
        if let Some(coverage) = coverage_json(&manager.build_report().coverage) {
            payload.insert("coverage".to_string(), coverage);
        }
        payload.insert("graph_available".to_string(), json!(true));
        make_result(
            call,
            ToolStatus::Success,
            Value::Object(payload),
            ToolCostHint {
                matches_returned: symbols.len() as u64,
                ..ToolCostHint::default()
            },
            None,
        )
    }

    fn graph_context_for_snapshot(
        &self,
        snapshot: &DiffSnapshot,
        max_symbols_per_file: usize,
        max_references: usize,
    ) -> Value {
        let mut graph = match self.graph.lock() {
            Ok(graph) => graph,
            Err(_) => return json!({"available": false, "error": "semantic graph lock poisoned"}),
        };
        let Some(manager) = graph.as_mut() else {
            return json!({"available": false, "reason": "semantic graph is unavailable for this workspace"});
        };
        let refresh = manager.refresh_before_query().ok();
        annotate_graph(manager, snapshot);
        let graph = manager.graph();
        let dirty = graph.dirty_symbols();
        let mut by_file: BTreeMap<String, Vec<GraphSymbol>> = BTreeMap::new();
        for symbol in dirty {
            by_file
                .entry(symbol.file_id.0.clone())
                .or_default()
                .push(symbol);
        }
        let files = by_file
            .into_iter()
            .map(|(path, symbols)| {
                let total = symbols.len();
                let symbols = symbols
                    .iter()
                    .take(max_symbols_per_file)
                    .map(|symbol| symbol_context_json(graph, symbol, max_references))
                    .collect::<Vec<_>>();
                json!({
                    "path": path,
                    "symbols": symbols,
                    "truncated": total > max_symbols_per_file,
                })
            })
            .collect::<Vec<_>>();
        let mut payload = serde_json::Map::new();
        payload.insert("available".to_string(), json!(true));
        if let Some(report) = refresh {
            let mut refresh_obj = serde_json::Map::new();
            refresh_obj.insert("refreshed".to_string(), json!(report.refreshed));
            refresh_obj.insert(
                "changed_files".to_string(),
                json!(
                    report
                        .changed_files
                        .iter()
                        .map(|id| id.0.clone())
                        .collect::<Vec<_>>()
                ),
            );
            refresh_obj.insert(
                "removed_files".to_string(),
                json!(
                    report
                        .removed_files
                        .iter()
                        .map(|id| id.0.clone())
                        .collect::<Vec<_>>()
                ),
            );
            refresh_obj.insert("reparsed_files".to_string(), json!(report.reparsed_files));
            refresh_obj.insert("excluded_files".to_string(), json!(report.excluded_files));
            refresh_obj.insert("excluded_dirs".to_string(), json!(report.excluded_dirs));
            refresh_obj.insert("excluded_bytes".to_string(), json!(report.excluded_bytes));
            if let Some(coverage) = coverage_json(&report.coverage) {
                refresh_obj.insert("coverage".to_string(), coverage);
            }
            refresh_obj.insert(
                "budget_exhausted".to_string(),
                json!(report.budget_exhausted),
            );
            payload.insert("refresh".to_string(), Value::Object(refresh_obj));
        }
        if let Some(coverage) = coverage_json(&manager.build_report().coverage) {
            payload.insert("coverage".to_string(), coverage);
        }
        payload.insert("files".to_string(), json!(files));
        Value::Object(payload)
    }

    async fn execute_load_skill(&self, call: &ToolCall) -> ToolResult {
        let args = match serde_json::from_value::<LoadSkillArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        match self.skills.load(&args.name) {
            Ok(skill) => make_result(
                call,
                ToolStatus::Success,
                json!({
                    "name": skill.summary.name,
                    "description": skill.summary.description,
                    "when_to_use": skill.summary.when_to_use,
                    "source": skill.summary.source,
                    "location": skill.summary.location,
                    "base_directory": skill.base_dir,
                    "content": skill.body,
                }),
                ToolCostHint::default(),
                None,
            ),
            Err(err) => tool_error(call, err),
        }
    }

    async fn execute_glob(&self, call: &ToolCall, cancel: CancellationToken) -> ToolResult {
        let args = match serde_json::from_value::<GlobArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        let start = match self.resolve_existing(args.path.as_deref().unwrap_or(".")) {
            Ok(path) => path,
            Err(err) => return tool_error(call, err),
        };
        let pattern = match build_required_glob(&args.pattern) {
            Ok(pattern) => pattern,
            Err(err) => return tool_error(call, err),
        };
        let include_ignored = args.include_ignored.unwrap_or(false);
        let diff_only = args.diff_only.unwrap_or(false);
        let diff_paths = if diff_only {
            diff_path_set(&self.diff_snapshot(DiffMode::Worktree, DiffOptions::default()))
        } else {
            BTreeSet::new()
        };
        let max_paths = args.max_paths.unwrap_or(DEFAULT_MAX_MATCHES).min(1_000);
        let offset = args.offset.unwrap_or(0);

        let mut builder = WalkBuilder::new(&start);
        builder
            .follow_links(false)
            .hidden(false)
            .ignore(!include_ignored)
            .git_ignore(!include_ignored)
            .git_exclude(!include_ignored)
            .require_git(false)
            .parents(true)
            .sort_by_file_path(|left, right| left.cmp(right));

        let mut paths = Vec::new();
        let mut skipped_paths = 0usize;
        let mut skipped_secret_files = 0u64;
        let mut cost = ToolCostHint::default();

        for entry in builder.build() {
            if cancel.is_cancelled() {
                return ToolResult::cancelled(call);
            }
            if paths.len() >= max_paths {
                cost.truncated = true;
                break;
            }

            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let path = entry.path();
            if !path.is_file() || contains_skipped_dir(path) {
                continue;
            }
            let rel = self.relative(path);
            if !include_ignored && self.policy_exclusion_for_file(path, &rel, None).is_some() {
                continue;
            }
            if diff_only && !diff_paths.contains(rel.to_string_lossy().as_ref()) {
                continue;
            }
            if is_secret_path(&rel) {
                skipped_secret_files += 1;
                continue;
            }
            cost.files_scanned += 1;
            if !pattern.is_match(rel.as_path()) {
                continue;
            }
            if skipped_paths < offset {
                skipped_paths += 1;
                continue;
            }
            paths.push(json!(rel.to_string_lossy()));
            cost.matches_returned += 1;
        }

        make_result(
            call,
            ToolStatus::Success,
            json!({
                "paths": paths,
                "metadata": {
                    "pattern": args.pattern,
                    "path": args.path.as_deref().unwrap_or("."),
                    "include_ignored": include_ignored,
                    "diff_only": diff_only,
                    "offset": offset,
                    "skipped_secret_files": skipped_secret_files,
                },
            }),
            cost,
            None,
        )
    }

    async fn execute_grep(&self, call: &ToolCall, cancel: CancellationToken) -> ToolResult {
        let args = match serde_json::from_value::<GrepArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };

        let regex = match Regex::new(&args.pattern) {
            Ok(regex) => regex,
            Err(err) => {
                return make_result(
                    call,
                    ToolStatus::Error,
                    json!({ "error": format!("invalid regex: {err}") }),
                    ToolCostHint::default(),
                    None,
                );
            }
        };

        let start = match self.resolve_existing(args.path.as_deref().unwrap_or(".")) {
            Ok(path) => path,
            Err(err) => return tool_error(call, err),
        };

        let include = match build_include_set(args.include.as_deref()) {
            Ok(include) => include,
            Err(err) => return tool_error(call, err),
        };

        let include_ignored = args.include_ignored.unwrap_or(false);
        let diff_only = args.diff_only.unwrap_or(false);
        let diff_paths = if diff_only {
            diff_path_set(&self.diff_snapshot(DiffMode::Worktree, DiffOptions::default()))
        } else {
            BTreeSet::new()
        };
        let output_mode = args.output_mode.unwrap_or_default();
        let max_files = args
            .max_files
            .unwrap_or(DEFAULT_MAX_FILES)
            .min(DEFAULT_MAX_FILES);
        let max_bytes_per_file = args
            .max_bytes_per_file
            .unwrap_or(DEFAULT_MAX_BYTES_PER_FILE)
            .min(DEFAULT_MAX_BYTES_PER_FILE);
        let max_matches = args.max_matches.unwrap_or(DEFAULT_MAX_MATCHES).min(1_000);
        let offset = args.offset.unwrap_or(0);
        let output_byte_cap = args
            .output_byte_cap
            .unwrap_or(DEFAULT_OUTPUT_BYTE_CAP)
            .min(128_000);

        let mut builder = WalkBuilder::new(&start);
        builder
            .follow_links(false)
            .hidden(false)
            .ignore(!include_ignored)
            .git_ignore(!include_ignored)
            .git_exclude(!include_ignored)
            .require_git(false)
            .parents(true)
            .sort_by_file_path(|left, right| left.cmp(right));

        let mut matches = Vec::new();
        let mut paths = BTreeSet::new();
        let mut count = 0u64;
        let mut skipped_matches = 0usize;
        let mut cost = ToolCostHint::default();
        let mut skipped_secret_files = 0u64;
        let mut scanned_files = 0usize;
        let mut stop_search = false;

        for entry in builder.build() {
            if cancel.is_cancelled() {
                return ToolResult::cancelled(call);
            }
            if scanned_files >= max_files
                || output_mode.is_limited(matches.len(), paths.len(), max_matches)
                || stop_search
            {
                cost.truncated = true;
                break;
            }

            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let path = entry.path();
            if !path.is_file() || contains_skipped_dir(path) {
                continue;
            }
            let rel = self.relative(path);
            if !include_ignored && self.policy_exclusion_for_file(path, &rel, None).is_some() {
                continue;
            }
            if diff_only && !diff_paths.contains(rel.to_string_lossy().as_ref()) {
                continue;
            }
            if include
                .as_ref()
                .is_some_and(|include| !include.is_match(rel.as_path()))
            {
                continue;
            }
            if is_secret_path(&rel) {
                skipped_secret_files += 1;
                continue;
            }

            scanned_files += 1;
            cost.files_scanned += 1;
            let bytes = match read_prefix(path, max_bytes_per_file) {
                Ok(bytes) => bytes,
                Err(_) => continue,
            };
            if !include_ignored {
                let head_len = bytes.len().min(POLICY_PREFIX_BYTES);
                if self
                    .policy_exclusion_for_file(path, &rel, Some(&bytes[..head_len]))
                    .is_some()
                {
                    continue;
                }
            }
            cost.bytes_read += bytes.len() as u64;
            let file_truncated = file_len(path)
                .map(|len| len > bytes.len() as u64)
                .unwrap_or(false);
            if file_truncated {
                cost.truncated = true;
            }

            let text = String::from_utf8_lossy(&bytes);
            for (line_index, line) in text.lines().enumerate() {
                if !regex.is_match(line) {
                    continue;
                }
                if skipped_matches < offset {
                    skipped_matches += 1;
                    continue;
                }
                count += 1;
                match output_mode {
                    GrepOutputMode::Content => {
                        let line = truncate_text(line, 500);
                        let next = json!({
                            "path": rel.to_string_lossy(),
                            "line": line_index + 1,
                            "text": line,
                        });
                        let next_len = serde_json::to_string(&next).map_or(0, |text| text.len());
                        if cost.output_bytes + next_len as u64 > output_byte_cap as u64 {
                            cost.truncated = true;
                            stop_search = true;
                            break;
                        }
                        cost.output_bytes += next_len as u64;
                        cost.matches_returned += 1;
                        matches.push(next);
                    }
                    GrepOutputMode::FilesWithMatches => {
                        if paths.insert(rel.to_string_lossy().to_string()) {
                            cost.matches_returned += 1;
                        }
                    }
                    GrepOutputMode::Count => {
                        cost.matches_returned = count;
                    }
                }
                if output_mode.is_limited(matches.len(), paths.len(), max_matches) {
                    cost.truncated = true;
                    stop_search = true;
                    break;
                }
            }
        }

        let mut metadata = BTreeMap::new();
        metadata.insert("pattern".to_string(), json!(args.pattern));
        metadata.insert(
            "path".to_string(),
            json!(args.path.as_deref().unwrap_or(".")),
        );
        if let Some(include) = args.include.as_ref() {
            metadata.insert("include".to_string(), json!(include));
        }
        metadata.insert("include_ignored".to_string(), json!(include_ignored));
        metadata.insert("diff_only".to_string(), json!(diff_only));
        metadata.insert("output_mode".to_string(), json!(output_mode.as_str()));
        metadata.insert("offset".to_string(), json!(offset));
        metadata.insert(
            "skipped_secret_files".to_string(),
            json!(skipped_secret_files),
        );
        if !include_ignored {
            metadata.insert(
                "hint".to_string(),
                json!(
                    "ignored paths were skipped; retry with include_ignored=true only when needed"
                ),
            );
        }

        let content = match output_mode {
            GrepOutputMode::Content => json!({
                "matches": matches,
                "metadata": metadata,
            }),
            GrepOutputMode::FilesWithMatches => json!({
                "paths": paths.into_iter().collect::<Vec<_>>(),
                "metadata": metadata,
            }),
            GrepOutputMode::Count => json!({
                "count": count,
                "metadata": metadata,
            }),
        };

        make_result(call, ToolStatus::Success, content, cost, None)
    }

    async fn execute_read_file(&self, call: &ToolCall) -> ToolResult {
        let args = match serde_json::from_value::<ReadFileArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        let path = match self.resolve_existing(&args.path) {
            Ok(path) => path,
            Err(err) => return tool_error(call, err),
        };
        let rel = self.relative(&path);
        if args.diff_only.unwrap_or(false) {
            let diff_paths =
                diff_path_set(&self.diff_snapshot(DiffMode::Worktree, DiffOptions::default()));
            if !diff_paths.contains(rel.to_string_lossy().as_ref()) {
                return make_result(
                    call,
                    ToolStatus::Denied,
                    json!({ "error": "refusing to read a clean file because diff_only=true", "path": rel.to_string_lossy() }),
                    ToolCostHint::default(),
                    None,
                );
            }
        }
        if is_secret_path(&rel) {
            return make_result(
                call,
                ToolStatus::Denied,
                json!({ "error": "refusing to read a likely secret file" }),
                ToolCostHint::default(),
                None,
            );
        }

        let total_bytes = match file_len(&path) {
            Ok(len) => len,
            Err(err) => return tool_error(call, err),
        };
        let prefix_bytes = read_prefix(&path, POLICY_PREFIX_BYTES).ok();
        let ignored_reason = self
            .policy_exclusion_for_file(&path, &rel, prefix_bytes.as_deref())
            .map(ExclusionReason::as_str);
        let offset = args.offset.unwrap_or(0).min(total_bytes as usize);
        let limit = args.limit.unwrap_or(DEFAULT_READ_LIMIT).min(MAX_READ_LIMIT);
        let bytes = match read_range(&path, offset as u64, limit) {
            Ok(bytes) => bytes,
            Err(err) => return tool_error(call, err),
        };
        let end = offset.saturating_add(bytes.len());
        let content = String::from_utf8_lossy(&bytes).to_string();
        let content_sha256 = match sha256_file(&path) {
            Ok(hash) => hash,
            Err(err) => return tool_error(call, err),
        };
        let cost = ToolCostHint {
            bytes_read: total_bytes,
            output_bytes: content.len() as u64,
            truncated: end < total_bytes as usize,
            ..ToolCostHint::default()
        };

        let mut payload = serde_json::Map::new();
        payload.insert("path".to_string(), json!(rel.to_string_lossy()));
        payload.insert("offset".to_string(), json!(offset));
        payload.insert("bytes_returned".to_string(), json!(bytes.len()));
        payload.insert("total_bytes".to_string(), json!(total_bytes));
        payload.insert("sha256".to_string(), json!(content_sha256));
        payload.insert("truncated".to_string(), json!(end < total_bytes as usize));
        if let Some(reason) = ignored_reason {
            // Keep this opt-in: most reads are not from ignored paths, so
            // skipping these fields shaves two keys off the common case.
            payload.insert("ignored".to_string(), json!(true));
            payload.insert("ignored_reason".to_string(), json!(reason));
        }
        payload.insert("content".to_string(), json!(content));

        make_result(
            call,
            ToolStatus::Success,
            Value::Object(payload),
            cost,
            Some(content_sha256),
        )
    }

    async fn execute_read_tool_output(&self, call: &ToolCall) -> ToolResult {
        let args = match serde_json::from_value::<ReadToolOutputArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        let output = match self.output_store.read(
            &args.handle,
            args.offset.unwrap_or(0),
            args.limit.unwrap_or(DEFAULT_READ_LIMIT).min(MAX_READ_LIMIT),
        ) {
            Ok(output) => output,
            Err(err) => return tool_error(call, err),
        };
        let cost = ToolCostHint {
            bytes_read: output.bytes_returned as u64,
            output_bytes: output.content.len() as u64,
            truncated: output.truncated,
            ..ToolCostHint::default()
        };

        make_result(
            call,
            ToolStatus::Success,
            json!({
                "handle": args.handle,
                "offset": output.offset,
                "bytes_returned": output.bytes_returned,
                "total_bytes": output.total_bytes,
                "sha256": output.sha256,
                "truncated": output.truncated,
                "content": output.content,
            }),
            cost,
            None,
        )
    }

    async fn execute_verify(
        &self,
        call: &ToolCall,
        cancel: CancellationToken,
        group_id: &str,
    ) -> ToolResult {
        let args = match serde_json::from_value::<VerifyArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        let scope = args.scope.unwrap_or_default();
        let level = args.level.unwrap_or_default();
        let snapshot = self.diff_snapshot(DiffMode::Worktree, DiffOptions::default());
        let changed_paths = snapshot
            .files
            .iter()
            .map(|file| file.path.clone())
            .collect::<Vec<_>>();
        if matches!(scope, VerifyScope::Diff) && changed_paths.is_empty() {
            return make_result(
                call,
                ToolStatus::Success,
                json!({
                    "scope": verify_scope_str(scope),
                    "level": verify_level_str(level),
                    "changed_files": changed_paths,
                    "command": null,
                    "no_op": true,
                    "reason": "no changed files in the current Git worktree diff",
                }),
                ToolCostHint::default(),
                None,
            );
        }
        if matches!(scope, VerifyScope::Diff)
            && !changed_paths
                .iter()
                .any(|path| is_rust_verification_path(path))
        {
            return make_result(
                call,
                ToolStatus::Success,
                json!({
                    "scope": verify_scope_str(scope),
                    "level": verify_level_str(level),
                    "changed_files": changed_paths,
                    "command": null,
                    "no_op": true,
                    "reason": "diff contains no Rust source or Cargo manifest paths",
                }),
                ToolCostHint::default(),
                None,
            );
        }

        let command = verify_command(&self.root, scope, level, &changed_paths);
        let shell_call = ToolCall {
            call_id: call.call_id.clone(),
            name: "shell".to_string(),
            arguments: json!({
                "command": command,
                "description": "run verification scoped by current diff",
                "timeout_ms": VERIFY_SHELL_TIMEOUT_MS,
                "output_byte_cap": DEFAULT_SHELL_OUTPUT_BYTE_CAP,
            }),
        };
        let shell_result = self
            .execute_shell_capped(&shell_call, cancel, VERIFY_SHELL_TIMEOUT_MS, group_id)
            .await;
        let mut content = shell_result.content;
        if let Some(object) = content.as_object_mut() {
            object.insert("scope".to_string(), json!(verify_scope_str(scope)));
            object.insert("level".to_string(), json!(verify_level_str(level)));
            object.insert("changed_files".to_string(), json!(changed_paths));
        }
        make_result(
            call,
            shell_result.status,
            content,
            shell_result.cost_hint,
            None,
        )
    }

    async fn execute_write_file(&self, call: &ToolCall, group_id: &str) -> ToolResult {
        let args = match serde_json::from_value::<WriteFileArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        let path = match self.resolve_for_write(&args.path) {
            Ok(path) => path,
            Err(err) => return tool_error(call, err),
        };
        let rel = self.relative(&path);
        if is_secret_path(&rel) {
            return make_result(
                call,
                ToolStatus::Denied,
                json!({ "error": "refusing to write a likely secret file" }),
                ToolCostHint::default(),
                None,
            );
        }

        let checkpoint_before = match self.checkpoints.track_tree() {
            Ok(snapshot) => snapshot,
            Err(err) => return tool_error(call, err),
        };
        let before = fs::read(&path).ok();
        let before_sha256 = before.as_ref().map(sha256_hex);
        if before.is_some() && args.expected_sha256.as_deref() != before_sha256.as_deref() {
            return make_result(
                call,
                ToolStatus::Stale,
                json!({
                    "error": "expected_sha256 does not match current file",
                    "path": rel.to_string_lossy(),
                    "current_sha256": before_sha256,
                }),
                ToolCostHint::default(),
                before_sha256,
            );
        }

        if let Some(parent) = path.parent()
            && let Err(err) = fs::create_dir_all(parent)
        {
            return tool_error(call, err);
        }
        if let Err(err) = fs::write(&path, args.content.as_bytes()) {
            return tool_error(call, err);
        }
        self.invalidate_diff_cache();

        let after_sha256 = sha256_hex(args.content.as_bytes());
        let cost = ToolCostHint {
            bytes_read: before.as_ref().map_or(0, |bytes| bytes.len() as u64),
            output_bytes: args.content.len() as u64,
            ..ToolCostHint::default()
        };

        let mut content = json!({
            "path": rel.to_string_lossy(),
            "before_sha256": before_sha256,
            "after_sha256": after_sha256,
            "bytes_written": args.content.len(),
        });
        self.append_checkpoint_to_content(
            &mut content,
            &checkpoint_before,
            call,
            group_id,
            ToolStatus::Success,
            Vec::new(),
        );
        make_result(call, ToolStatus::Success, content, cost, Some(after_sha256))
    }

    async fn execute_shell(
        &self,
        call: &ToolCall,
        cancel: CancellationToken,
        group_id: &str,
    ) -> ToolResult {
        self.execute_shell_capped(call, cancel, MAX_SHELL_TIMEOUT_MS, group_id)
            .await
    }

    async fn execute_shell_capped(
        &self,
        call: &ToolCall,
        cancel: CancellationToken,
        max_timeout_ms: u64,
        group_id: &str,
    ) -> ToolResult {
        let args = match serde_json::from_value::<ShellArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        let analysis = analyze_shell_command(&args.command);
        if args.command.trim().is_empty() {
            return shell_policy_denied(call, &analysis, "shell command must not be empty");
        }
        if args.timeout_ms == Some(0) {
            return shell_policy_denied(call, &analysis, "shell timeout_ms must be at least 1");
        }
        if args.output_byte_cap == Some(0) {
            return shell_policy_denied(
                call,
                &analysis,
                "shell output_byte_cap must be at least 1",
            );
        }
        let workdir = match self.resolve_existing(args.workdir.as_deref().unwrap_or(".")) {
            Ok(path) => path,
            Err(err) => {
                return shell_policy_denied(
                    call,
                    &analysis,
                    format!("shell workdir rejected by cwd policy: {err}"),
                );
            }
        };
        let timeout_ms = args
            .timeout_ms
            .unwrap_or(DEFAULT_SHELL_TIMEOUT_MS)
            .min(max_timeout_ms);
        let output_cap = args
            .output_byte_cap
            .unwrap_or(DEFAULT_SHELL_OUTPUT_BYTE_CAP)
            .min(MAX_SHELL_OUTPUT_BYTE_CAP);
        let checkpoint_before = match self.checkpoints.track_tree() {
            Ok(snapshot) => snapshot,
            Err(err) => return tool_error(call, err),
        };
        let coverage_warnings = shell_coverage_warnings(&args.command);

        if let Some(pattern) = shell_command_references_sensitive_path(
            &args.command,
            &self.shell_sandbox.sensitive_path_patterns,
        ) {
            let reason = format!("shell command references sensitive path pattern {pattern:?}");
            self.audit_shell(
                call,
                &args,
                &workdir,
                &analysis,
                json!({"backend": "none", "mode": self.shell_sandbox.mode.as_str(), "status": "denied"}),
                timeout_ms,
                output_cap,
                "denied",
                Some(&reason),
                None,
                &[],
                &[],
            );
            return shell_policy_denied(call, &analysis, reason);
        }

        let sandbox_plan = match self.prepare_shell_sandbox(&args.command, &analysis) {
            Ok(plan) => plan,
            Err(reason) => {
                self.audit_shell(
                    call,
                    &args,
                    &workdir,
                    &analysis,
                    json!({"backend": "none", "mode": self.shell_sandbox.mode.as_str(), "status": "unavailable"}),
                    timeout_ms,
                    output_cap,
                    "denied",
                    Some(&reason),
                    None,
                    &[],
                    &[],
                );
                return shell_policy_denied(call, &analysis, reason);
            }
        };

        let mut command = Command::new(&sandbox_plan.program);
        command
            .args(&sandbox_plan.args)
            .current_dir(&workdir)
            .kill_on_drop(true)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_shell_process_group(&mut command);
        configure_linux_shell_sandbox(&mut command, &sandbox_plan);
        let preserved_env = apply_shell_environment_policy(&mut command, &self.shell_sandbox);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(err) if sandbox_plan.required => {
                let reason = format!(
                    "shell sandbox backend {} failed to start: {err}",
                    sandbox_plan.backend
                );
                self.audit_shell(
                    call,
                    &args,
                    &workdir,
                    &analysis,
                    sandbox_plan.metadata(),
                    timeout_ms,
                    output_cap,
                    "denied",
                    Some(&reason),
                    None,
                    &[],
                    &[],
                );
                return shell_policy_denied(call, &analysis, reason);
            }
            Err(err) => return tool_error(call, err),
        };

        let remaining_output_bytes = Arc::new(Mutex::new(output_cap));
        let stdout_task = tokio::spawn(read_limited_pipe(
            child.stdout.take(),
            remaining_output_bytes.clone(),
        ));
        let stderr_task = tokio::spawn(read_limited_pipe(
            child.stderr.take(),
            remaining_output_bytes,
        ));

        let status = tokio::select! {
            _ = cancel.cancelled() => {
                terminate_shell_child(&mut child, self.shell_sandbox.kill_grace_ms).await;
                stdout_task.abort();
                stderr_task.abort();
                self.audit_shell(
                    call,
                    &args,
                    &workdir,
                    &analysis,
                    sandbox_plan.metadata(),
                    timeout_ms,
                    output_cap,
                    "cancelled",
                    Some("shell command cancelled"),
                    None,
                    &[],
                    &[],
                );
                return ToolResult::cancelled(call);
            }
            result = time::timeout(Duration::from_millis(timeout_ms), child.wait()) => result,
        };

        let timed_out = status.is_err();
        let exit_status = match status {
            Ok(Ok(status)) => Some(status),
            Err(_) => {
                terminate_shell_child(&mut child, self.shell_sandbox.kill_grace_ms).await;
                None
            }
            Ok(Err(err)) => return tool_error(call, err),
        };

        let (stdout_bytes, stdout_truncated) = match join_limited_pipe(stdout_task).await {
            Ok(output) => output,
            Err(err) => return tool_error(call, err),
        };
        let (stderr_bytes, stderr_truncated) = match join_limited_pipe(stderr_task).await {
            Ok(output) => output,
            Err(err) => return tool_error(call, err),
        };

        let stdout = String::from_utf8_lossy(&stdout_bytes).to_string();
        let stderr = String::from_utf8_lossy(&stderr_bytes).to_string();
        let truncated = stdout_truncated || stderr_truncated || timed_out;
        let cost = ToolCostHint {
            output_bytes: (stdout.len() + stderr.len()) as u64,
            truncated,
            ..ToolCostHint::default()
        };
        let exit_code = exit_status.as_ref().and_then(|status| status.code());
        if sandbox_plan.required
            && shell_sandbox_runtime_unavailable(&sandbox_plan, exit_code, &stderr)
        {
            let reason = format!(
                "required shell sandbox backend {} failed at runtime",
                sandbox_plan.backend
            );
            self.audit_shell(
                call,
                &args,
                &workdir,
                &analysis,
                sandbox_plan.metadata(),
                timeout_ms,
                output_cap,
                "denied",
                Some(&reason),
                exit_code,
                &stdout_bytes,
                &stderr_bytes,
            );
            return shell_policy_denied(call, &analysis, reason);
        }
        let status = if exit_status.as_ref().is_some_and(|status| status.success()) {
            ToolStatus::Success
        } else {
            ToolStatus::Error
        };
        let error = timed_out.then(|| format!("shell command timed out after {timeout_ms} ms"));
        self.audit_shell(
            call,
            &args,
            &workdir,
            &analysis,
            sandbox_plan.metadata(),
            timeout_ms,
            output_cap,
            if timed_out {
                "timeout"
            } else if status == ToolStatus::Success {
                "success"
            } else {
                "error"
            },
            error.as_deref(),
            exit_code,
            &stdout_bytes,
            &stderr_bytes,
        );
        self.invalidate_diff_cache();

        let mut content = json!({
            "command": args.command,
            "workdir": self.relative(&workdir).to_string_lossy(),
            "exit_code": exit_code,
            "stdout": stdout,
            "stderr": stderr,
            "error": error,
            "truncated": truncated,
            "policy": {
                "capability": analysis.capability.as_str(),
                "target": analysis.rule_target,
                "risk": analysis.risk.as_str(),
                "network": if analysis.network { "classified" } else { "none" },
                "destructive": analysis.destructive,
                "parser_backed": analysis.parser_backed,
                "dynamic": analysis.dynamic,
                "timeout_ms": timeout_ms,
                "output_byte_cap": output_cap,
            },
            "sandbox": sandbox_plan.metadata(),
            "env": {
                "policy": "allowlist",
                "values": "redacted",
                "preserved": preserved_env,
            },
        });
        self.append_checkpoint_to_content(
            &mut content,
            &checkpoint_before,
            call,
            group_id,
            status,
            coverage_warnings,
        );
        make_result(call, status, content, cost, None)
    }

    async fn execute_websearch(&self, call: &ToolCall, cancel: CancellationToken) -> ToolResult {
        let args = match serde_json::from_value::<WebSearchArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        if args.query.trim().is_empty() {
            return tool_error(call, "query must not be empty");
        }

        let num_results = args
            .num_results
            .unwrap_or(DEFAULT_WEB_SEARCH_RESULTS)
            .clamp(1, MAX_WEB_SEARCH_RESULTS);
        let context_max_characters = args
            .context_max_characters
            .unwrap_or(DEFAULT_WEB_SEARCH_CONTEXT_CHARS)
            .clamp(1, MAX_WEB_SEARCH_CONTEXT_CHARS);
        let timeout_ms = args
            .timeout_ms
            .unwrap_or(DEFAULT_WEB_SEARCH_TIMEOUT_MS)
            .min(MAX_WEB_TIMEOUT_MS);
        let search_type = args.search_type.unwrap_or_default();
        let livecrawl = args.livecrawl.unwrap_or_default();

        let mut request_headers = vec![(
            "accept".to_string(),
            "application/json, text/event-stream".to_string(),
        )];
        if let Some(api_key) = self.web_config.exa_api_key.as_deref() {
            request_headers.push(("x-api-key".to_string(), api_key.to_string()));
        }

        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "web_search_exa",
                "arguments": {
                    "query": args.query,
                    "type": search_type.as_str(),
                    "numResults": num_results,
                    "livecrawl": livecrawl.as_str(),
                    "contextMaxCharacters": context_max_characters,
                },
            },
        });
        let fetch = async {
            let response = self
                .http
                .post_json(
                    &self.web_config.exa_mcp_url,
                    request_headers,
                    body.clone(),
                    DEFAULT_WEB_SEARCH_MAX_RESPONSE_BYTES,
                )
                .await?;
            if !response.is_success() {
                return Err(format!(
                    "websearch provider returned HTTP {}",
                    response.status
                ));
            }
            let response_text = String::from_utf8_lossy(&response.body).to_string();
            let result = parse_mcp_websearch_response(&response_text)
                .ok_or_else(|| "websearch provider returned no text content".to_string())?;
            Ok::<_, String>((response_text.len(), result))
        };

        let (bytes_read, result) = match tokio::select! {
            _ = cancel.cancelled() => return ToolResult::cancelled(call),
            result = time::timeout(Duration::from_millis(timeout_ms), fetch) => result,
        } {
            Ok(Ok(result)) => result,
            Ok(Err(err)) => return tool_error(call, err),
            Err(_) => {
                return tool_error(call, format!("websearch timed out after {timeout_ms} ms"));
            }
        };
        let cost = ToolCostHint {
            bytes_read: bytes_read as u64,
            output_bytes: result.len() as u64,
            ..ToolCostHint::default()
        };

        make_result(
            call,
            ToolStatus::Success,
            json!({
                "provider": "exa",
                "query": body["params"]["arguments"]["query"],
                "result": result,
                "metadata": {
                    "num_results": num_results,
                    "search_type": search_type.as_str(),
                    "livecrawl": livecrawl.as_str(),
                    "context_max_characters": context_max_characters,
                },
            }),
            cost,
            None,
        )
    }

    async fn execute_webfetch(&self, call: &ToolCall, cancel: CancellationToken) -> ToolResult {
        let args = match serde_json::from_value::<WebFetchArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        let mut url = match parse_http_url(&args.url) {
            Ok(url) => url,
            Err(err) => return tool_error(call, err),
        };
        let original_url = url.clone();
        let format = args.format.unwrap_or_default();
        let timeout_ms = args
            .timeout_ms
            .unwrap_or(DEFAULT_WEB_FETCH_TIMEOUT_MS)
            .min(MAX_WEB_TIMEOUT_MS);
        let max_response_bytes = args
            .max_response_bytes
            .unwrap_or(DEFAULT_WEB_FETCH_MAX_RESPONSE_BYTES)
            .clamp(1, MAX_WEB_FETCH_MAX_RESPONSE_BYTES);
        let output_byte_cap = args
            .output_byte_cap
            .unwrap_or(DEFAULT_WEB_FETCH_OUTPUT_BYTE_CAP)
            .min(128_000);

        let fetch = async {
            for redirect_count in 0..=MAX_WEB_REDIRECTS {
                let response = self.http.get(url.clone(), max_response_bytes).await?;
                if response.is_redirection() {
                    let next = redirect_url(&url, &response)?;
                    if next.host_str() != original_url.host_str() {
                        return Ok(WebFetchOutcome::Redirect {
                            status: response.status,
                            original_url: original_url.to_string(),
                            redirect_url: next.to_string(),
                        });
                    }
                    if redirect_count == MAX_WEB_REDIRECTS {
                        return Err("too many redirects".to_string());
                    }
                    url = next;
                    continue;
                }
                if !response.is_success() {
                    return Err(format!("webfetch returned HTTP status {}", response.status));
                }

                let content_type = response.header("content-type").unwrap_or("").to_string();
                if !is_textual_content_type(&content_type) {
                    return Err(format!(
                        "unsupported content type: {}",
                        if content_type.is_empty() {
                            "unknown"
                        } else {
                            content_type.as_str()
                        }
                    ));
                }

                return Ok(WebFetchOutcome::Fetched {
                    final_url: url.to_string(),
                    status: response.status,
                    content_type,
                    bytes: response.body,
                });
            }
            Err("too many redirects".to_string())
        };

        let outcome = match tokio::select! {
            _ = cancel.cancelled() => return ToolResult::cancelled(call),
            result = time::timeout(Duration::from_millis(timeout_ms), fetch) => result,
        } {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(err)) => return tool_error(call, err),
            Err(_) => return tool_error(call, format!("webfetch timed out after {timeout_ms} ms")),
        };

        match outcome {
            WebFetchOutcome::Redirect {
                status,
                original_url,
                redirect_url,
            } => make_result(
                call,
                ToolStatus::Error,
                json!({
                    "error": "redirect to another host detected; call webfetch again with redirect_url if approved",
                    "status": status,
                    "original_url": original_url,
                    "redirect_url": redirect_url,
                }),
                ToolCostHint::default(),
                None,
            ),
            WebFetchOutcome::Fetched {
                final_url,
                status,
                content_type,
                bytes,
            } => {
                let raw_len = bytes.len();
                let decoded = String::from_utf8_lossy(&bytes);
                let rendered = match format {
                    WebFetchFormat::Text if content_type_is_html(&content_type) => {
                        html_to_text(&decoded)
                    }
                    WebFetchFormat::Text => decoded.to_string(),
                    WebFetchFormat::Html => decoded.to_string(),
                };
                let (content, output_truncated) = truncate_to_bytes(&rendered, output_byte_cap);
                let content_sha256 = sha256_hex(&bytes);
                let cost = ToolCostHint {
                    bytes_read: raw_len as u64,
                    output_bytes: content.len() as u64,
                    truncated: output_truncated,
                    ..ToolCostHint::default()
                };
                make_result(
                    call,
                    ToolStatus::Success,
                    json!({
                        "url": final_url,
                        "status": status,
                        "content_type": content_type,
                        "format": format.as_str(),
                        "bytes_read": raw_len,
                        "sha256": content_sha256,
                        "truncated": output_truncated,
                        "content": content,
                    }),
                    cost,
                    Some(content_sha256),
                )
            }
        }
    }

    fn resolve_existing(&self, raw: &str) -> std::result::Result<PathBuf, String> {
        let candidate = self.join_workspace(raw)?;
        let canonical = candidate
            .canonicalize()
            .map_err(|err| format!("path does not exist or is inaccessible: {err}"))?;
        self.ensure_inside(canonical)
    }

    fn resolve_for_write(&self, raw: &str) -> std::result::Result<PathBuf, String> {
        let candidate = self.join_workspace(raw)?;
        if candidate.exists() {
            return self.resolve_existing(raw);
        }
        let parent = candidate
            .parent()
            .ok_or_else(|| "path has no parent".to_string())?;
        let parent = parent
            .canonicalize()
            .map_err(|err| format!("parent directory does not exist or is inaccessible: {err}"))?;
        self.ensure_inside(parent)?;
        Ok(candidate)
    }

    fn join_workspace(&self, raw: &str) -> std::result::Result<PathBuf, String> {
        if raw.trim().is_empty() {
            return Err("path must not be empty".to_string());
        }
        let path = Path::new(raw);
        if path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            return Err("path must stay inside the workspace".to_string());
        }
        Ok(if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        })
    }

    fn ensure_inside(&self, canonical: PathBuf) -> std::result::Result<PathBuf, String> {
        if canonical.starts_with(self.root.as_ref()) {
            Ok(canonical)
        } else {
            Err("path is outside the workspace".to_string())
        }
    }

    fn relative(&self, path: &Path) -> PathBuf {
        path.strip_prefix(self.root.as_ref())
            .unwrap_or(path)
            .to_path_buf()
    }

    fn append_checkpoint_to_content(
        &self,
        content: &mut Value,
        before: &WorkspaceSnapshot,
        call: &ToolCall,
        group_id: &str,
        status: ToolStatus,
        coverage_warnings: Vec<String>,
    ) {
        match self.checkpoints.create_checkpoint(
            before,
            &call.name,
            &call.call_id,
            group_id,
            checkpoint_status_label(status),
            coverage_warnings,
        ) {
            Ok(Some(checkpoint)) => {
                insert_content_field(content, "checkpoint", checkpoint_json(&checkpoint));
            }
            Ok(None) => {}
            Err(err) => {
                insert_content_field(content, "checkpoint_error", json!(err.to_string()));
            }
        }
    }

    fn finalize_result(&self, result: ToolResult) -> ToolResult {
        self.output_store
            .maybe_spill(redact_tool_result(result, &self.redactor))
    }
}

fn insert_content_field(content: &mut Value, key: &str, value: Value) {
    if let Some(object) = content.as_object_mut() {
        object.insert(key.to_string(), value);
    }
}

fn checkpoint_status_label(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Success => "success",
        ToolStatus::Error => "error",
        ToolStatus::Denied => "denied",
        ToolStatus::Stale => "stale",
        ToolStatus::Cancelled => "cancelled",
    }
}

fn checkpoint_json(record: &CheckpointRecord) -> Value {
    let mut value = json!({
        "id": record.id,
        "group_id": record.group_id,
        "tool_name": record.tool_name,
        "call_id": record.call_id,
        "status": record.status,
        "summary": record.summary,
        "files": record.files,
    });
    if let Some(object) = value.as_object_mut() {
        if !record.skipped_files.is_empty() {
            object.insert("skipped_files".to_string(), json!(record.skipped_files));
        }
        if !record.coverage_warnings.is_empty() {
            object.insert(
                "coverage_warnings".to_string(),
                json!(record.coverage_warnings),
            );
        }
    }
    value
}

fn redact_tool_result(mut result: ToolResult, redactor: &Redactor) -> ToolResult {
    let original_content = std::mem::take(&mut result.content);
    let (content, redactions) = redact_json_value(original_content, redactor);
    if redactions == 0 {
        result.content = content;
        return result;
    }
    result.content = content;
    result.cost_hint.redactions += redactions;
    let output = serde_json::to_vec(&result.content).unwrap_or_default();
    result.cost_hint.output_bytes = output.len() as u64;
    result.receipt.output_sha256 = sha256_hex(&output);
    result
}

fn redact_json_value(value: Value, redactor: &Redactor) -> (Value, u64) {
    match value {
        Value::String(text) => {
            let redacted = redactor.redact(&text);
            (Value::String(redacted.text), redacted.redactions)
        }
        Value::Array(items) => {
            let mut redactions = 0;
            let items = items
                .into_iter()
                .map(|item| {
                    let (item, count) = redact_json_value(item, redactor);
                    redactions += count;
                    item
                })
                .collect();
            (Value::Array(items), redactions)
        }
        Value::Object(entries) => {
            let mut redactions = 0;
            let entries = entries
                .into_iter()
                .map(|(key, value)| {
                    let (value, count) = redact_json_value(value, redactor);
                    redactions += count;
                    (key, value)
                })
                .collect();
            (Value::Object(entries), redactions)
        }
        value => (value, 0),
    }
}

#[derive(Debug, Clone)]
struct ToolOutputStore {
    dir: PathBuf,
    spill_threshold_bytes: usize,
    preview_bytes: usize,
    retention: Duration,
}

impl ToolOutputStore {
    fn new(root: &Path, config: ToolOutputConfig) -> Result<Self> {
        let config = config.normalized();
        let dir = match config.output_dir {
            Some(dir) if dir.is_absolute() => dir,
            Some(dir) => root.join(dir),
            None => root.join(".squeezy").join("tool_outputs"),
        };
        let store = Self {
            dir,
            spill_threshold_bytes: config.spill_threshold_bytes,
            preview_bytes: config.preview_bytes,
            retention: Duration::from_secs(config.retention_days * 24 * 60 * 60),
        };
        fs::create_dir_all(&store.dir)?;
        store.cleanup_old_outputs();
        Ok(store)
    }

    fn maybe_spill(&self, mut result: ToolResult) -> ToolResult {
        let output = result.model_output();
        if output.len() <= self.spill_threshold_bytes {
            return result;
        }

        let sha256 = sha256_hex(output.as_bytes());
        let path = self.path_for(&sha256);
        if let Err(err) = fs::write(&path, output.as_bytes()) {
            result.status = ToolStatus::Error;
            result.content = json!({ "error": format!("failed to spill tool output: {err}") });
            result.cost_hint.truncated = true;
            result.receipt.output_sha256 =
                sha256_hex(serde_json::to_vec(&result.content).unwrap_or_default());
            return result;
        }

        let (preview, _) = truncate_to_bytes(&output, self.preview_bytes);
        let ToolResult {
            call_id,
            tool_name,
            status,
            content: _,
            mut cost_hint,
            receipt,
        } = result;
        let original_output_sha256 = receipt.output_sha256;
        let content_sha256 = receipt.content_sha256;
        let call = ToolCall {
            call_id,
            name: tool_name,
            arguments: Value::Null,
        };
        cost_hint.truncated = true;
        cost_hint.output_bytes = 0;

        make_result(
            &call,
            status,
            json!({
                "spilled": true,
                "handle": sha256,
                "sha256": sha256,
                "original_output_sha256": original_output_sha256,
                "total_bytes": output.len(),
                "preview_bytes": preview.len(),
                "preview": preview,
                "truncated": true,
            }),
            cost_hint,
            content_sha256,
        )
    }

    fn read(
        &self,
        handle: &str,
        offset: usize,
        limit: usize,
    ) -> std::result::Result<StoredToolOutputSlice, String> {
        if !is_valid_handle(handle) {
            return Err("invalid tool output handle".to_string());
        }
        let bytes = fs::read(self.path_for(handle))
            .map_err(|err| format!("tool output handle not found or unreadable: {err}"))?;
        let offset = offset.min(bytes.len());
        let end = offset.saturating_add(limit).min(bytes.len());
        let content = String::from_utf8_lossy(&bytes[offset..end]).to_string();
        Ok(StoredToolOutputSlice {
            offset,
            bytes_returned: end - offset,
            total_bytes: bytes.len(),
            sha256: sha256_hex(&bytes),
            truncated: end < bytes.len(),
            content,
        })
    }

    fn cleanup_old_outputs(&self) {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        let now = SystemTime::now();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let Ok(modified) = metadata.modified() else {
                continue;
            };
            if now
                .duration_since(modified)
                .is_ok_and(|age| age > self.retention)
            {
                let _ = fs::remove_file(path);
            }
        }
    }

    fn path_for(&self, handle: &str) -> PathBuf {
        self.dir.join(format!("{handle}.json"))
    }
}

#[derive(Debug)]
struct StoredToolOutputSlice {
    offset: usize,
    bytes_returned: usize,
    total_bytes: usize,
    sha256: String,
    truncated: bool,
    content: String,
}

fn is_valid_handle(handle: &str) -> bool {
    handle.len() == 64 && handle.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn nonzero_or(value: usize, default: usize) -> usize {
    if value == 0 { default } else { value }
}

fn nonzero_or_u64(value: u64, default: u64) -> u64 {
    if value == 0 { default } else { value }
}

fn parse_mcp_websearch_response(body: &str) -> Option<String> {
    let trimmed = body.trim();
    if trimmed.starts_with('{')
        && let Some(result) = parse_mcp_payload(trimmed)
    {
        return Some(result);
    }

    let mut chunks = Vec::new();
    for line in body.lines() {
        let Some(payload) = line.strip_prefix("data: ") else {
            continue;
        };
        if let Some(result) = parse_mcp_payload(payload) {
            chunks.push(result);
        }
    }
    (!chunks.is_empty()).then(|| chunks.join("\n\n"))
}

fn parse_mcp_payload(payload: &str) -> Option<String> {
    let trimmed = payload.trim();
    if !trimmed.starts_with('{') {
        return None;
    }
    let value = serde_json::from_str::<Value>(trimmed).ok()?;
    let texts = value
        .get("result")?
        .get("content")?
        .as_array()?
        .iter()
        .filter_map(|item| item.get("text")?.as_str())
        .filter(|text| !text.trim().is_empty())
        .map(str::trim)
        .collect::<Vec<_>>();
    (!texts.is_empty()).then(|| texts.join("\n\n"))
}

async fn read_response_bytes(
    response: reqwest::Response,
    max_response_bytes: usize,
) -> std::result::Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|len| len > max_response_bytes as u64)
    {
        return Err(format!(
            "response too large; content-length exceeds {max_response_bytes} bytes"
        ));
    }

    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|err| format!("failed to read response body: {err}"))?;
        if bytes.len().saturating_add(chunk.len()) > max_response_bytes {
            return Err(format!(
                "response too large; exceeded {max_response_bytes} bytes"
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn parse_http_url(raw: &str) -> std::result::Result<Url, String> {
    let url = Url::parse(raw).map_err(|err| format!("invalid URL: {err}"))?;
    match url.scheme() {
        "http" | "https" => Ok(url),
        _ => Err("URL must start with http:// or https://".to_string()),
    }
}

fn web_url_host(raw: &str) -> std::result::Result<String, String> {
    parse_http_url(raw).and_then(|url| {
        url.host_str()
            .map(str::to_string)
            .ok_or_else(|| "URL has no host".to_string())
    })
}

fn redirect_url(current: &Url, response: &WebHttpResponse) -> std::result::Result<Url, String> {
    let location = response
        .header("location")
        .ok_or_else(|| "redirect response did not include a location".to_string())?;
    current
        .join(location)
        .map_err(|err| format!("invalid redirect location: {err}"))
        .and_then(|url| parse_http_url(url.as_str()))
}

fn is_textual_content_type(content_type: &str) -> bool {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    mime.is_empty()
        || mime.starts_with("text/")
        || matches!(
            mime.as_str(),
            "application/json"
                | "application/xml"
                | "application/xhtml+xml"
                | "application/javascript"
                | "application/x-javascript"
                | "image/svg+xml"
        )
        || mime.ends_with("+json")
        || mime.ends_with("+xml")
}

fn content_type_is_html(content_type: &str) -> bool {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    matches!(mime.as_str(), "text/html" | "application/xhtml+xml")
}

fn html_to_text(html: &str) -> String {
    let stripped = strip_html_blocks(html);
    let mut text = String::new();
    let mut in_tag = false;
    for char in stripped.chars() {
        match char {
            '<' => {
                in_tag = true;
                text.push(' ');
            }
            '>' => {
                in_tag = false;
                text.push(' ');
            }
            _ if !in_tag => text.push(char),
            _ => {}
        }
    }
    collapse_whitespace(&decode_html_entities(&text))
}

fn strip_html_blocks(html: &str) -> String {
    let mut output = html.to_string();
    for tag in ["script", "style", "noscript", "iframe", "object", "embed"] {
        output = strip_html_block_tag(&output, tag);
    }
    output
}

fn strip_html_block_tag(input: &str, tag: &str) -> String {
    let mut output = String::new();
    let mut rest = input;
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    loop {
        let lower = rest.to_ascii_lowercase();
        let Some(start) = lower.find(&open) else {
            output.push_str(rest);
            break;
        };
        output.push_str(&rest[..start]);
        let after_start = &rest[start..];
        let lower_after_start = after_start.to_ascii_lowercase();
        let Some(end) = lower_after_start.find(&close) else {
            break;
        };
        rest = &after_start[end + close.len()..];
    }
    output
}

fn decode_html_entities(input: &str) -> String {
    input
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
}

fn unix_timestamp_millis(time: SystemTime) -> u128 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

fn collapse_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShellPermissionAnalysis {
    capability: PermissionCapability,
    risk: PermissionRisk,
    rule_target: String,
    network: bool,
    destructive: bool,
    parser_backed: bool,
    dynamic: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedShellCommand {
    segments: Vec<String>,
    dynamic: bool,
}

fn analyze_shell_command(command: &str) -> ShellPermissionAnalysis {
    let normalized = collapse_whitespace(command);
    // Permission flow calls analyze_shell_command twice for the same
    // command (permission_request, then execute_shell_capped). A tiny
    // thread-local LRU avoids the second tree-sitter parse on the hot
    // path. The cache is bounded so long-running agents don't grow
    // unbounded memory.
    thread_local! {
        static MEMO: std::cell::RefCell<VecDeque<(String, ShellPermissionAnalysis)>> =
            const { std::cell::RefCell::new(VecDeque::new()) };
    }
    const MEMO_CAPACITY: usize = 16;
    if let Some(hit) = MEMO.with(|cache| {
        cache
            .borrow()
            .iter()
            .find(|(key, _)| key == &normalized)
            .map(|(_, analysis)| analysis.clone())
    }) {
        return hit;
    }
    let parsed = parse_shell_command(&normalized);
    let parser_backed = parsed.is_some();
    let dynamic = parsed.as_ref().is_some_and(|parsed| parsed.dynamic);
    let raw_segments = parsed
        .as_ref()
        .map(|parsed| parsed.segments.clone())
        .filter(|segments| !segments.is_empty())
        .unwrap_or_else(|| shell_segments(&normalized));
    // Wrappers (sh -c "...", env BAR=v cmd, nohup cmd, xargs cmd, ...) hide
    // the real command behind boilerplate. Append the recursively unwrapped
    // inner commands so destructive/network/compiler checks fire on the
    // actual payload, not just the wrapper.
    let segments = expand_wrapper_segments(raw_segments);
    let first = segments
        .first()
        .map(|segment| shell_command_prefix(segment))
        .filter(|prefix| !prefix.is_empty())
        .unwrap_or_else(|| "shell".to_string());

    let analysis = if segments.is_empty() {
        ShellPermissionAnalysis {
            capability: PermissionCapability::Shell,
            risk: PermissionRisk::High,
            rule_target: "shell:*".to_string(),
            network: false,
            destructive: false,
            parser_backed,
            dynamic,
        }
    } else if dynamic {
        ShellPermissionAnalysis {
            capability: PermissionCapability::Shell,
            risk: PermissionRisk::High,
            rule_target: "shell:*".to_string(),
            network: segments
                .iter()
                .any(|segment| is_network_shell_segment(segment)),
            destructive: segments
                .iter()
                .any(|segment| is_destructive_shell_segment(segment))
                || shell_segment_has_destructive_redirect(&normalized),
            parser_backed,
            dynamic,
        }
    } else if segments
        .iter()
        .any(|segment| is_destructive_shell_segment(segment))
        || shell_segment_has_destructive_redirect(&normalized)
    {
        ShellPermissionAnalysis {
            capability: PermissionCapability::Destructive,
            risk: PermissionRisk::Critical,
            rule_target: format!("{first}:*"),
            network: segments
                .iter()
                .any(|segment| is_network_shell_segment(segment)),
            destructive: true,
            parser_backed,
            dynamic,
        }
    } else if segments
        .iter()
        .any(|segment| is_network_shell_segment(segment))
    {
        ShellPermissionAnalysis {
            capability: PermissionCapability::Network,
            risk: PermissionRisk::High,
            rule_target: format!("shell:{first}:*"),
            network: true,
            destructive: false,
            parser_backed,
            dynamic,
        }
    } else if segments
        .iter()
        .all(|segment| is_compiler_shell_segment(segment))
    {
        ShellPermissionAnalysis {
            capability: PermissionCapability::Compiler,
            risk: PermissionRisk::Medium,
            rule_target: format!(
                "{}:*",
                shell_command_prefix(segments.first().unwrap_or(&normalized))
            ),
            network: false,
            destructive: false,
            parser_backed,
            dynamic,
        }
    } else if segments.iter().all(|segment| is_git_shell_segment(segment)) {
        ShellPermissionAnalysis {
            capability: PermissionCapability::Git,
            risk: if segments
                .iter()
                .all(|segment| is_git_read_only_segment(segment))
            {
                PermissionRisk::Low
            } else {
                PermissionRisk::High
            },
            rule_target: format!(
                "{}:*",
                shell_command_prefix(segments.first().unwrap_or(&normalized))
            ),
            network: false,
            destructive: false,
            parser_backed,
            dynamic,
        }
    } else {
        ShellPermissionAnalysis {
            capability: PermissionCapability::Shell,
            risk: PermissionRisk::High,
            rule_target: format!("{first}:*"),
            network: false,
            destructive: false,
            parser_backed,
            dynamic,
        }
    };
    MEMO.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() >= MEMO_CAPACITY {
            cache.pop_front();
        }
        cache.push_back((normalized.clone(), analysis.clone()));
    });
    analysis
}

/// For each top-level command segment, append any wrapper-stripped inner
/// command so the rest of the analyzer sees the real argv. Recurses up to
/// `MAX_WRAPPER_DEPTH` times to cover nested wrappers like
/// `nohup sh -c "env BAR=v rm -rf /"`.
fn expand_wrapper_segments(segments: Vec<String>) -> Vec<String> {
    const MAX_WRAPPER_DEPTH: usize = 8;
    let mut out = Vec::with_capacity(segments.len());
    for segment in segments {
        out.push(segment.clone());
        let mut current = segment;
        for _ in 0..MAX_WRAPPER_DEPTH {
            let Some(inner) = unwrap_shell_wrapper(&current) else {
                break;
            };
            // Re-parse the inner: it can contain its own `&&`/`;`/`|`
            // operators, in which case we want each piece as a segment.
            for piece in shell_segments(&inner) {
                if !piece.is_empty() && !out.iter().any(|seg| seg == &piece) {
                    out.push(piece);
                }
            }
            current = inner;
        }
    }
    out
}

/// Try to unwrap one layer of shell wrapping. Returns the inner command
/// string with the wrapper boilerplate removed, or `None` if the segment
/// doesn't begin with a recognized wrapper. The recognized wrappers fall
/// into three families:
///
/// - `sh -c "<cmd>"` / `bash -c '<cmd>'` (and `-lc`, `-ic`) — the script
///   passed to a shell interpreter.
/// - `env [VAR=val …] [-i|-] <argv>` — environment-prefix runners.
/// - `nohup <argv>`, `nice [-n N] <argv>`, `time <argv>`, `timeout <DUR>
///   <argv>`, `stdbuf <opts> <argv>`, `xargs [opts] <argv>`,
///   `sudo [opts] <argv>` — passthrough wrappers.
fn unwrap_shell_wrapper(segment: &str) -> Option<String> {
    let tokens = tokenize_shell_segment(segment);
    let head = tokens.first()?.as_str();
    match head {
        "sh" | "bash" | "zsh" | "fish" | "csh" | "tcsh" | "ksh" | "dash" => {
            // Walk past flag tokens; if any flag contains `c`, the next
            // positional argument is the script we want to surface.
            let mut idx = 1;
            while let Some(tok) = tokens.get(idx) {
                if let Some(flag_body) = tok.strip_prefix('-') {
                    if flag_body.contains('c') {
                        let script = tokens.get(idx + 1)?;
                        return Some(dequote_token(script).to_string());
                    }
                    idx += 1;
                } else {
                    break;
                }
            }
            None
        }
        "env" => {
            let mut idx = 1;
            while let Some(tok) = tokens.get(idx) {
                if tok == "-" || tok == "-i" || tok == "--ignore-environment" {
                    idx += 1;
                } else if tok.starts_with('-') {
                    // Unknown env flag; bail out conservatively to avoid
                    // swallowing the inner command behind a flag we don't
                    // understand.
                    return None;
                } else if shell_env_assignment_token(tok) {
                    idx += 1;
                } else {
                    break;
                }
            }
            let inner = tokens.get(idx..)?;
            if inner.is_empty() {
                None
            } else {
                Some(
                    inner
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(" "),
                )
            }
        }
        "nohup" | "time" | "sudo" => {
            // Skip the wrapper and any leading flags so the inner argv is
            // returned cleanly. `sudo` accepts complex flags but stays a
            // passthrough.
            let mut idx = 1;
            while let Some(tok) = tokens.get(idx) {
                if tok.starts_with('-') {
                    idx += 1;
                } else {
                    break;
                }
            }
            let inner = tokens.get(idx..)?;
            if inner.is_empty() {
                None
            } else {
                Some(
                    inner
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(" "),
                )
            }
        }
        "nice" => {
            let mut idx = 1;
            if tokens.get(idx).map(String::as_str) == Some("-n") {
                idx += 2;
            } else if tokens
                .get(idx)
                .map(String::as_str)
                .is_some_and(|tok| tok.starts_with('-'))
            {
                idx += 1;
            }
            let inner = tokens.get(idx..)?;
            if inner.is_empty() {
                None
            } else {
                Some(
                    inner
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(" "),
                )
            }
        }
        "stdbuf" => {
            let mut idx = 1;
            while tokens
                .get(idx)
                .map(String::as_str)
                .is_some_and(|tok| tok.starts_with('-'))
            {
                idx += 1;
            }
            let inner = tokens.get(idx..)?;
            if inner.is_empty() {
                None
            } else {
                Some(
                    inner
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(" "),
                )
            }
        }
        "timeout" => {
            let mut idx = 1;
            while tokens
                .get(idx)
                .map(String::as_str)
                .is_some_and(|tok| tok.starts_with('-'))
            {
                idx += 1;
            }
            // First non-flag is the duration (e.g. "30", "10s"). Skip it.
            if tokens.get(idx).is_some() {
                idx += 1;
            }
            let inner = tokens.get(idx..)?;
            if inner.is_empty() {
                None
            } else {
                Some(
                    inner
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(" "),
                )
            }
        }
        "xargs" => {
            let mut idx = 1;
            while let Some(tok) = tokens.get(idx) {
                if !tok.starts_with('-') {
                    break;
                }
                let flag = tok.as_str();
                idx += 1;
                if matches!(
                    flag,
                    "-I" | "-L" | "-n" | "-P" | "--max-args" | "--max-procs"
                ) {
                    // Consume the flag's value if present.
                    if tokens.get(idx).is_some() {
                        idx += 1;
                    }
                }
            }
            let inner = tokens.get(idx..)?;
            if inner.is_empty() {
                None
            } else {
                Some(
                    inner
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(" "),
                )
            }
        }
        _ => None,
    }
}

/// True for tokens shaped like `NAME=value` (the env-assignment prefix
/// passed to `env`). Mirrors `split_env_assignment` but operates on owned
/// strings.
fn shell_env_assignment_token(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    if name.is_empty() {
        return false;
    }
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return false;
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

/// Quote-aware tokenizer used by the wrapper unwrapper. Single and double
/// quotes group whitespace-separated runs into a single token; the surrounding
/// quotes are preserved on the token so the caller can `dequote_token` it.
fn tokenize_shell_segment(segment: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut iter = segment.chars().peekable();
    while let Some(ch) = iter.next() {
        match (quote, ch) {
            (Some(q), c) if c == q => {
                current.push(ch);
                quote = None;
            }
            (None, '\'') | (None, '"') => {
                current.push(ch);
                quote = Some(ch);
            }
            (None, c) if c.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            (_, '\\') => {
                current.push(ch);
                if let Some(next) = iter.next() {
                    current.push(next);
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Strip a single pair of matching outer quotes from a token, leaving its
/// contents otherwise unchanged. Bash escape semantics are not interpreted
/// (the classifier is conservative: `sh -c "rm -rf \\"$HOME\\""` will still
/// surface the literal payload, including the escaped backslashes).
fn dequote_token(token: &str) -> &str {
    let bytes = token.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' || first == b'\'') && first == last {
            return &token[1..token.len() - 1];
        }
    }
    token
}

fn parse_shell_command(command: &str) -> Option<ParsedShellCommand> {
    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .is_err()
    {
        return None;
    }
    let tree = parser.parse(command, None)?;
    let root = tree.root_node();
    let mut segments = Vec::new();
    collect_shell_command_nodes(root, command.as_bytes(), &mut segments);
    Some(ParsedShellCommand {
        segments: if segments.is_empty() {
            shell_segments(command)
        } else {
            segments
        },
        dynamic: root.has_error()
            || shell_tree_contains_dynamic(root)
            || shell_text_is_dynamic(command),
    })
}

fn collect_shell_command_nodes(node: Node<'_>, bytes: &[u8], segments: &mut Vec<String>) {
    if node.kind() == "command"
        && let Ok(text) = node.utf8_text(bytes)
    {
        let text = collapse_whitespace(text);
        if !text.is_empty() {
            segments.push(text);
            return;
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_shell_command_nodes(child, bytes, segments);
    }
}

fn shell_tree_contains_dynamic(node: Node<'_>) -> bool {
    if matches!(
        node.kind(),
        "command_substitution"
            | "process_substitution"
            | "expansion"
            | "simple_expansion"
            | "subscript"
            | "heredoc_redirect"
    ) {
        return true;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor).any(shell_tree_contains_dynamic)
}

fn shell_text_is_dynamic(command: &str) -> bool {
    command.contains("$(")
        || command.contains('`')
        || command.contains("${")
        || command.contains("<(")
        || command.contains(">(")
}

fn shell_coverage_warnings(command: &str) -> Vec<String> {
    let segments = shell_segments(&collapse_whitespace(command));
    let suspicious = segments.iter().any(|segment| {
        let words = segment.split_whitespace().collect::<Vec<_>>();
        let mut has_mutation = false;
        let mut has_outside_path = false;
        for word in words {
            let trimmed = word.trim_matches(|ch| matches!(ch, '\'' | '"' | '(' | ')' | ';'));
            if matches!(
                trimmed,
                "rm" | "rmdir" | "mv" | "cp" | "dd" | "truncate" | "touch" | "mkdir"
            ) || matches!(trimmed, ">" | ">>")
            {
                has_mutation = true;
            }
            if trimmed.starts_with('/') || trimmed.contains("../") || trimmed == ".." {
                has_outside_path = true;
            }
        }
        has_mutation && has_outside_path
    });
    if suspicious {
        vec![
            "shell command may mutate paths outside the workspace; checkpoint rollback only protects workspace files"
                .to_string(),
        ]
    } else {
        Vec::new()
    }
}

fn shell_segments(command: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut chars = command.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(ch) = chars.next() {
        match (quote, ch) {
            (Some('\''), '\'') => quote = None,
            (Some('"'), '"') => quote = None,
            (Some(_), '\\') => {
                current.push(ch);
                if let Some(next) = chars.next() {
                    current.push(next);
                }
                continue;
            }
            (None, '\'' | '"') => quote = Some(ch),
            (None, ';') => {
                push_shell_segment(&mut segments, &mut current);
                continue;
            }
            (None, '&') if chars.peek() == Some(&'&') => {
                let _ = chars.next();
                push_shell_segment(&mut segments, &mut current);
                continue;
            }
            (None, '|') if chars.peek() == Some(&'|') => {
                let _ = chars.next();
                push_shell_segment(&mut segments, &mut current);
                continue;
            }
            _ => {}
        }
        current.push(ch);
    }
    push_shell_segment(&mut segments, &mut current);
    segments
}

fn push_shell_segment(segments: &mut Vec<String>, current: &mut String) {
    let segment = current.trim();
    if !segment.is_empty() {
        segments.push(segment.to_string());
    }
    current.clear();
}

fn shell_command_prefix(segment: &str) -> String {
    let mut parts = segment.split_whitespace();
    let mut first = parts.next().unwrap_or("shell");
    while let Some((name, _)) = split_env_assignment(first) {
        if !shell_env_assignment_allowed_for_prefix(name) {
            return "shell".to_string();
        }
        first = parts.next().unwrap_or("shell");
    }
    if is_bare_shell_prefix(first) {
        return "shell".to_string();
    }
    match first {
        "cargo" | "git" | "npm" | "pnpm" | "yarn" | "bun" | "make" | "just" => parts
            .next()
            .map(|sub| format!("{first} {sub}"))
            .unwrap_or_else(|| first.to_string()),
        _ => first.to_string(),
    }
}

fn split_env_assignment(token: &str) -> Option<(&str, &str)> {
    let (name, value) = token.split_once('=')?;
    if name.is_empty() {
        return None;
    }
    let mut chars = name.chars();
    let first = chars.next()?;
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return None;
    }
    if !chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric()) {
        return None;
    }
    Some((name, value))
}

fn shell_env_assignment_allowed_for_prefix(name: &str) -> bool {
    matches!(
        name,
        "CI" | "NO_COLOR"
            | "RUST_BACKTRACE"
            | "RUSTFLAGS"
            | "CARGO_TERM_COLOR"
            | "CARGO_INCREMENTAL"
            | "RUST_LOG"
    )
}

fn is_bare_shell_prefix(prefix: &str) -> bool {
    matches!(
        prefix,
        "sh" | "bash"
            | "zsh"
            | "fish"
            | "csh"
            | "tcsh"
            | "ksh"
            | "dash"
            | "env"
            | "xargs"
            | "nice"
            | "nohup"
            | "time"
            | "timeout"
            | "stdbuf"
            | "sudo"
    )
}

fn is_destructive_shell_segment(segment: &str) -> bool {
    let tokens: Vec<&str> = segment.split_whitespace().collect();
    let first = tokens.first().copied().unwrap_or("");
    if matches!(
        first,
        "rm" | "rmdir" | "mv" | "dd" | "truncate" | "shred" | "chmod" | "chown" | "sudo"
    ) {
        return true;
    }
    if destructive_git_pair(&tokens) || destructive_two_word_command(&tokens) {
        return true;
    }
    if shell_segment_has_destructive_redirect(segment) {
        return true;
    }
    false
}

/// Detects shell output redirects that write to a filename (`>`, `>>`, `>|`,
/// `&>`, `&>>`, `<>`), while ignoring file-descriptor duplications like
/// `2>&1`, `>&-`, and any `>` that appears inside single or double quotes.
fn shell_segment_has_destructive_redirect(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    let mut i = 0usize;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        match (quote, b) {
            (Some(q), c) if c == q => {
                quote = None;
                i += 1;
            }
            (None, b'\'') | (None, b'"') => {
                quote = Some(b);
                i += 1;
            }
            (None, b'\\') if i + 1 < bytes.len() => {
                i += 2;
            }
            (None, b'>') => {
                // Skip the run of `>` characters (handles `>`, `>>`).
                let mut j = i + 1;
                while j < bytes.len() && bytes[j] == b'>' {
                    j += 1;
                }
                // Optional `|` (force overwrite, `>|`).
                if j < bytes.len() && bytes[j] == b'|' {
                    j += 1;
                }
                // Skip whitespace between operator and target.
                while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                    j += 1;
                }
                // `>&N` or `>&-` is a file-descriptor duplication, not a
                // write to a path.
                if j < bytes.len() && bytes[j] == b'&' {
                    let mut k = j + 1;
                    while k < bytes.len() && bytes[k].is_ascii_digit() {
                        k += 1;
                    }
                    let dup_dash = k < bytes.len() && bytes[k] == b'-';
                    if k > j + 1 || dup_dash {
                        i = if dup_dash { k + 1 } else { k };
                        continue;
                    }
                }
                return true;
            }
            _ => {
                i += 1;
            }
        }
    }
    false
}

/// Recognises the destructive git command families we want to surface
/// without misfiring on substrings like `git push -foreign-rule`. Each entry
/// matches `git <verb> [optional flag]` exactly on token boundaries.
fn destructive_git_pair(tokens: &[&str]) -> bool {
    let Some(&"git") = tokens.first() else {
        return false;
    };
    let Some(&verb) = tokens.get(1) else {
        return false;
    };
    match verb {
        "reset" | "clean" | "checkout" | "restore" => true,
        "stash" => matches!(tokens.get(2).copied(), Some("drop" | "clear")),
        "branch" => tokens.iter().skip(2).any(|tok| *tok == "-D"),
        "push" => tokens
            .iter()
            .skip(2)
            .any(|tok| *tok == "-f" || tok.starts_with("--force")),
        _ => false,
    }
}

fn destructive_two_word_command(tokens: &[&str]) -> bool {
    match tokens.first().copied() {
        Some("terraform") => tokens.get(1).copied() == Some("destroy"),
        Some("kubectl") => tokens.get(1).copied() == Some("delete"),
        Some("docker") => matches!(tokens.get(1).copied(), Some("rm" | "rmi" | "system")),
        _ => false,
    }
}

fn is_network_shell_segment(segment: &str) -> bool {
    matches!(
        shell_command_prefix(segment).as_str(),
        "curl"
            | "wget"
            | "nc"
            | "netcat"
            | "ssh"
            | "scp"
            | "sftp"
            | "rsync"
            | "telnet"
            | "ftp"
            | "dig"
            | "nslookup"
            | "ping"
            | "traceroute"
            | "gh"
            | "git fetch"
            | "git pull"
            | "git push"
            | "git clone"
            | "git ls-remote"
            | "cargo fetch"
            | "cargo install"
            | "cargo update"
            | "npm install"
            | "pnpm install"
            | "yarn install"
            | "bun install"
    )
}

fn is_compiler_shell_segment(segment: &str) -> bool {
    matches!(
        shell_command_prefix(segment).as_str(),
        "cargo test"
            | "cargo nextest"
            | "cargo check"
            | "cargo clippy"
            | "cargo fmt"
            | "cargo build"
            | "rustc"
            | "make test"
            | "just test"
    )
}

fn is_git_shell_segment(segment: &str) -> bool {
    segment.split_whitespace().next() == Some("git")
}

fn is_git_read_only_segment(segment: &str) -> bool {
    matches!(
        shell_command_prefix(segment).as_str(),
        "git status" | "git diff" | "git log" | "git show" | "git branch"
    )
}

#[derive(Debug, Deserialize)]
struct GlobArgs {
    pattern: String,
    path: Option<String>,
    include_ignored: Option<bool>,
    diff_only: Option<bool>,
    max_paths: Option<usize>,
    offset: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct GrepArgs {
    pattern: String,
    path: Option<String>,
    include: Option<Vec<String>>,
    include_ignored: Option<bool>,
    diff_only: Option<bool>,
    output_mode: Option<GrepOutputMode>,
    max_files: Option<usize>,
    max_bytes_per_file: Option<usize>,
    max_matches: Option<usize>,
    output_byte_cap: Option<usize>,
    offset: Option<usize>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum GrepOutputMode {
    #[default]
    Content,
    FilesWithMatches,
    Count,
}

impl GrepOutputMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Content => "content",
            Self::FilesWithMatches => "files_with_matches",
            Self::Count => "count",
        }
    }

    const fn is_limited(self, matches: usize, paths: usize, limit: usize) -> bool {
        match self {
            Self::Content => matches >= limit,
            Self::FilesWithMatches => paths >= limit,
            Self::Count => false,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ReadFileArgs {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
    diff_only: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct DiffContextArgs {
    mode: Option<DiffMode>,
    include_patch: Option<bool>,
    max_files: Option<usize>,
    max_symbols_per_file: Option<usize>,
    max_references_per_symbol: Option<usize>,
    max_patch_bytes: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct CheckpointListArgs {}

#[derive(Debug, Deserialize)]
struct CheckpointUndoArgs {
    mode: Option<RollbackMode>,
}

#[derive(Debug, Deserialize)]
struct CheckpointShowArgs {
    checkpoint_id: String,
}

#[derive(Debug, Deserialize)]
struct CheckpointRevertArgs {
    group_id: Option<String>,
    checkpoint_id: Option<String>,
    mode: Option<RollbackMode>,
}

#[derive(Debug, Deserialize)]
struct SymbolContextArgs {
    query: String,
    path: Option<String>,
    diff_only: Option<bool>,
    mode: Option<DiffMode>,
    max_references: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct VerifyArgs {
    scope: Option<VerifyScope>,
    level: Option<VerifyLevel>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum VerifyScope {
    #[default]
    Diff,
    Workspace,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum VerifyLevel {
    #[default]
    Quick,
    Full,
}

#[derive(Debug, Deserialize)]
struct ReadToolOutputArgs {
    handle: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct ListSkillsArgs {}

#[derive(Debug, Deserialize)]
struct LoadSkillArgs {
    name: String,
}

#[derive(Debug, Deserialize)]
struct WriteFileArgs {
    path: String,
    content: String,
    expected_sha256: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ShellArgs {
    command: String,
    workdir: Option<String>,
    timeout_ms: Option<u64>,
    output_byte_cap: Option<usize>,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WebSearchArgs {
    query: String,
    num_results: Option<usize>,
    search_type: Option<WebSearchType>,
    livecrawl: Option<WebSearchLivecrawl>,
    context_max_characters: Option<usize>,
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WebSearchType {
    #[default]
    Auto,
    Fast,
    Deep,
}

impl WebSearchType {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Fast => "fast",
            Self::Deep => "deep",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WebSearchLivecrawl {
    #[default]
    Fallback,
    Preferred,
}

impl WebSearchLivecrawl {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Fallback => "fallback",
            Self::Preferred => "preferred",
        }
    }
}

#[derive(Debug, Deserialize)]
struct WebFetchArgs {
    url: String,
    format: Option<WebFetchFormat>,
    timeout_ms: Option<u64>,
    max_response_bytes: Option<usize>,
    output_byte_cap: Option<usize>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WebFetchFormat {
    #[default]
    Text,
    Html,
}

impl WebFetchFormat {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Html => "html",
        }
    }
}

#[derive(Debug)]
enum WebFetchOutcome {
    Redirect {
        status: u16,
        original_url: String,
        redirect_url: String,
    },
    Fetched {
        final_url: String,
        status: u16,
        content_type: String,
        bytes: Vec<u8>,
    },
}

fn grep_include_ignored(arguments: &Value) -> bool {
    tool_include_ignored(arguments)
}

fn tool_include_ignored(arguments: &Value) -> bool {
    arguments
        .get("include_ignored")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn crawl_options_from_graph_config(config: &GraphConfig) -> CrawlOptions {
    CrawlOptions {
        include_hidden: config.include_hidden,
        max_file_bytes: config.max_file_bytes,
        require_indexing_signal: config.require_indexing_signal,
        policy: IndexingPolicy {
            include: config.include.clone(),
            exclude: config.exclude.clone(),
            include_classes: config.include_classes.clone(),
            exclude_classes: config.exclude_classes.clone(),
        },
    }
}

fn coverage_json(coverage: &IndexCoverage) -> Option<Value> {
    if coverage.skipped_files == 0 && coverage.skipped_dirs == 0 && coverage.reasons.is_empty() {
        return None;
    }
    let reasons = coverage
        .reasons
        .iter()
        .map(|(reason, coverage)| {
            (
                reason.clone(),
                json!({
                    "files": coverage.files,
                    "dirs": coverage.dirs,
                    "bytes": coverage.bytes,
                    "samples": coverage.samples,
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    Some(json!({
        "skipped_files": coverage.skipped_files,
        "skipped_dirs": coverage.skipped_dirs,
        "skipped_bytes": coverage.skipped_bytes,
        "reasons": reasons,
    }))
}

fn diff_mode_str(mode: DiffMode) -> &'static str {
    match mode {
        DiffMode::Worktree => "worktree",
        DiffMode::Branch => "branch",
    }
}

fn diff_path_set(snapshot: &DiffSnapshot) -> BTreeSet<String> {
    snapshot
        .files
        .iter()
        .map(|file| file.path.clone())
        .collect()
}

fn symbol_matches_path_filter(symbol: &GraphSymbol, filter: Option<&str>) -> bool {
    let Some(filter) = filter else {
        return true;
    };
    let path = symbol.file_id.0.as_str();
    path == filter || path.ends_with(&format!("/{filter}"))
}

fn diff_file_json(file: &DiffFile) -> Value {
    json!({
        "path": file.path,
        "status": diff_status_str(file.status),
        "code": file.code,
        "additions": file.additions,
        "deletions": file.deletions,
        "binary": file.binary,
        "hunks": file.hunks,
        "patch": file.patch,
        "patch_truncated": file.patch_truncated,
    })
}

fn diff_status_str(status: DiffFileStatus) -> &'static str {
    match status {
        DiffFileStatus::Added => "added",
        DiffFileStatus::Deleted => "deleted",
        DiffFileStatus::Modified => "modified",
    }
}

fn verify_scope_str(scope: VerifyScope) -> &'static str {
    match scope {
        VerifyScope::Diff => "diff",
        VerifyScope::Workspace => "workspace",
    }
}

fn verify_level_str(level: VerifyLevel) -> &'static str {
    match level {
        VerifyLevel::Quick => "quick",
        VerifyLevel::Full => "full",
    }
}

fn is_rust_verification_path(path: &str) -> bool {
    path.ends_with(".rs") || path.ends_with("Cargo.toml") || path.ends_with("Cargo.lock")
}

fn verify_command(
    root: &Path,
    scope: VerifyScope,
    level: VerifyLevel,
    changed_paths: &[String],
) -> String {
    let test_command = match scope {
        VerifyScope::Workspace => "cargo test --workspace".to_string(),
        VerifyScope::Diff => {
            let packages = diff_package_names(root, changed_paths);
            if packages.is_empty() {
                "cargo test --workspace".to_string()
            } else {
                packages
                    .into_iter()
                    .map(|package| format!("cargo test -p {}", shell_quote(&package)))
                    .collect::<Vec<_>>()
                    .join(" && ")
            }
        }
    };
    match level {
        VerifyLevel::Quick => test_command,
        VerifyLevel::Full => format!(
            "cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && {test_command}"
        ),
    }
}

fn diff_package_names(root: &Path, changed_paths: &[String]) -> BTreeSet<String> {
    changed_paths
        .iter()
        .filter_map(|path| {
            let mut parts = path.split('/');
            match (parts.next(), parts.next()) {
                (Some("crates"), Some(crate_dir)) => package_name(root, crate_dir),
                _ => None,
            }
        })
        .collect()
}

fn package_name(root: &Path, crate_dir: &str) -> Option<String> {
    let manifest =
        fs::read_to_string(root.join("crates").join(crate_dir).join("Cargo.toml")).ok()?;
    manifest.lines().find_map(|line| {
        let line = line.trim();
        let value = line.strip_prefix("name")?.trim_start();
        let value = value.strip_prefix('=')?.trim();
        let value = value.strip_prefix('"')?.strip_suffix('"')?;
        Some(value.to_string())
    })
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn annotate_graph(manager: &mut GraphManager, snapshot: &DiffSnapshot) {
    let dirty = snapshot
        .files
        .iter()
        .map(|file| {
            let ranges = if file.hunks.is_empty() {
                vec![DirtyRange {
                    start_line: 0,
                    end_line: u32::MAX,
                }]
            } else {
                file.hunks
                    .iter()
                    .map(|hunk| DirtyRange {
                        start_line: hunk.start_line,
                        end_line: hunk.end_line,
                    })
                    .collect()
            };
            (
                FileId::new(file.path.clone()),
                DirtyAnnotation {
                    status: diff_status_str(file.status).to_string(),
                    ranges,
                },
            )
        })
        .collect::<HashMap<_, _>>();
    manager.graph_mut().annotate_dirty_ranges(&dirty);
}

fn symbol_context_json(
    graph: &squeezy_graph::SemanticGraph,
    symbol: &GraphSymbol,
    max_references: usize,
) -> Value {
    let references = graph
        .references_to_symbol(&symbol.id)
        .into_iter()
        .take(max_references)
        .map(reference_json)
        .collect::<Vec<_>>();
    let callers = graph
        .callers(&symbol.id)
        .into_iter()
        .take(max_references)
        .filter_map(|hit| hit.caller)
        .map(|caller| {
            json!({
                "id": caller.id.0,
                "name": caller.name,
                "kind": format!("{:?}", caller.kind),
                "path": caller.file_id.0,
                "span": span_json(caller.span),
            })
        })
        .collect::<Vec<_>>();
    json!({
        "id": symbol.id.0,
        "name": symbol.name,
        "kind": format!("{:?}", symbol.kind),
        "path": symbol.file_id.0,
        "signature": symbol.signature,
        "visibility": symbol.visibility,
        "span": span_json(symbol.span),
        "dirty": symbol.dirty.as_ref().map(|dirty| json!({
            "status": dirty.status,
            "ranges": dirty.ranges.iter().map(|range| json!({
                "start_line": range.start_line,
                "end_line": range.end_line,
            })).collect::<Vec<_>>(),
        })),
        "references": references,
        "callers": callers,
        "confidence": format!("{:?}", symbol.confidence),
        "freshness": format!("{:?}", symbol.freshness),
    })
}

fn reference_json(hit: ReferenceHit) -> Value {
    json!({
        "path": hit.reference.file_id.0,
        "text": hit.reference.text,
        "kind": format!("{:?}", hit.reference.kind),
        "span": span_json(hit.reference.span),
        "owner": hit.owner.map(|owner| json!({
            "id": owner.id.0,
            "name": owner.name,
            "kind": format!("{:?}", owner.kind),
        })),
        "confidence": format!("{:?}", hit.confidence),
    })
}

fn span_json(span: squeezy_core::SourceSpan) -> Value {
    json!({
        "start_byte": span.start_byte,
        "end_byte": span.end_byte,
        "start": {"line": span.start.line, "column": span.start.column},
        "end": {"line": span.end.line, "column": span.end.column},
    })
}

fn make_result(
    call: &ToolCall,
    status: ToolStatus,
    content: Value,
    mut cost_hint: ToolCostHint,
    content_sha256: Option<String>,
) -> ToolResult {
    let output = serde_json::to_vec(&content).unwrap_or_default();
    cost_hint.output_bytes = cost_hint.output_bytes.max(output.len() as u64);
    ToolResult {
        call_id: call.call_id.clone(),
        tool_name: call.name.clone(),
        status,
        content,
        cost_hint,
        receipt: ToolReceipt {
            output_sha256: sha256_hex(&output),
            content_sha256,
        },
    }
}

fn tool_arg_error(call: &ToolCall, err: serde_json::Error) -> ToolResult {
    make_result(
        call,
        ToolStatus::Error,
        json!({ "error": format!("invalid tool arguments: {err}") }),
        ToolCostHint::default(),
        None,
    )
}

fn tool_error(call: &ToolCall, err: impl ToString) -> ToolResult {
    make_result(
        call,
        ToolStatus::Error,
        json!({ "error": err.to_string() }),
        ToolCostHint::default(),
        None,
    )
}

fn shell_policy_denied(
    call: &ToolCall,
    analysis: &ShellPermissionAnalysis,
    reason: impl Into<String>,
) -> ToolResult {
    make_result(
        call,
        ToolStatus::Denied,
        json!({
            "error": reason.into(),
            "permission_denied": true,
            "policy_denied": true,
            "capability": analysis.capability.as_str(),
            "target": analysis.rule_target,
            "risk": analysis.risk.as_str(),
            "network": if analysis.network { "detected" } else { "none" },
            "destructive": analysis.destructive,
            "parser_backed": analysis.parser_backed,
            "dynamic": analysis.dynamic,
        }),
        ToolCostHint::default(),
        None,
    )
}

fn prepare_shell_sandbox_plan(
    command: &str,
    analysis: &ShellPermissionAnalysis,
    root: &Path,
    config: &ShellSandboxConfig,
) -> std::result::Result<ShellSandboxPlan, String> {
    let required = config.mode == ShellSandboxMode::Required;
    // The sandbox-level network posture has THREE distinct states:
    //   - "allowed_approved": classified network + user opted into
    //     `allow_when_approved`; the sandbox opens its network namespace.
    //   - "denied_classified": classified network + default
    //     `deny_by_default`; the permission layer may still allow the
    //     command to RUN, but the sandbox keeps network closed so a
    //     misclassified target or a follow-on system() call can't reach
    //     out unnoticed.
    //   - "denied": non-network classification; sandbox always denies.
    let network = match (config.network, analysis.network) {
        (ShellSandboxNetworkPolicy::AllowWhenApproved, true) => "allowed_approved",
        (ShellSandboxNetworkPolicy::DenyByDefault, true) => "denied_classified",
        _ => "denied",
    };

    #[cfg(target_os = "macos")]
    {
        if Path::new("/usr/bin/sandbox-exec").exists() {
            return Ok(ShellSandboxPlan {
                program: "/usr/bin/sandbox-exec".to_string(),
                args: vec![
                    "-p".to_string(),
                    macos_shell_sandbox_profile(root, config, network != "denied"),
                    "sh".to_string(),
                    "-lc".to_string(),
                    command.to_string(),
                ],
                backend: "macos-sandbox-exec",
                mode: config.mode.as_str(),
                network,
                required,
            });
        }
        if required {
            return Err(
                "required shell sandbox unavailable: /usr/bin/sandbox-exec not found".to_string(),
            );
        }
    }

    #[cfg(target_os = "linux")]
    {
        // Probe whether unshare can actually be applied as the current
        // user. If the kernel forbids it (e.g. unprivileged_userns_clone=0
        // or seccomp policy in the container), required mode must fail
        // closed at sandbox-prepare time rather than silently exit 1
        // after fork.
        if !linux_unshare_supported() {
            if required {
                return Err(format!(
                    "required shell sandbox unavailable: linux unshare(CLONE_NEWUSER|CLONE_NEWNS{}) failed",
                    if network == "denied" {
                        " |CLONE_NEWNET"
                    } else {
                        ""
                    }
                ));
            }
            // best_effort: fall through to a direct ShellSandboxPlan below.
        } else {
            return Ok(ShellSandboxPlan {
                program: "sh".to_string(),
                args: vec!["-lc".to_string(), command.to_string()],
                backend: "linux-direct-syscalls",
                mode: config.mode.as_str(),
                network,
                required,
            });
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        if required {
            return Err(format!(
                "required shell sandbox unavailable on {}",
                std::env::consts::OS
            ));
        }
    }

    Ok(ShellSandboxPlan::direct(command, config.mode))
}

#[cfg(target_os = "macos")]
fn macos_shell_sandbox_profile(
    root: &Path,
    config: &ShellSandboxConfig,
    allow_network: bool,
) -> String {
    let mut profile = String::from("(version 1)\n(deny default)\n");
    // Process-level capabilities every build/run/test needs.
    profile.push_str("(allow process-exec)\n");
    profile.push_str("(allow process-fork)\n");
    profile.push_str("(allow signal (target self))\n");
    profile.push_str("(allow sysctl-read)\n");
    profile.push_str("(allow mach-lookup)\n");
    profile.push_str("(allow ipc-posix-shm)\n");
    profile.push_str("(allow iokit-open)\n");
    profile.push_str("(allow system-socket)\n");
    profile.push_str("(allow file-read-metadata)\n");
    // Reads from system / toolchain prefixes: required so compilers,
    // shells, dynamic linker, and certificate stores can do their job.
    let mut read_roots = macos_read_roots();
    read_roots.sort();
    read_roots.dedup();
    for path in read_roots {
        profile.push_str(&format!(
            "(allow file-read* (subpath {}))\n",
            sandbox_profile_string(&path.display().to_string())
        ));
    }
    // Read+write inside the workspace, tmp dirs, and toolchain caches.
    let mut write_roots = shell_writable_roots(root);
    write_roots.sort();
    write_roots.dedup();
    for path in write_roots {
        let escaped = sandbox_profile_string(&path.display().to_string());
        profile.push_str(&format!("(allow file-read* (subpath {escaped}))\n"));
        profile.push_str(&format!("(allow file-write* (subpath {escaped}))\n"));
    }
    // Sensitive paths get an EXPLICIT deny on top of the default deny so
    // even if a future allow rule widens reads, these subpaths stay
    // blocked.
    let mut denied_paths = sensitive_absolute_paths(root, config);
    denied_paths.sort();
    denied_paths.dedup();
    for path in denied_paths {
        profile.push_str(&format!(
            "(deny file-read* file-write* (subpath {}))\n",
            sandbox_profile_string(&path.display().to_string())
        ));
    }
    if allow_network {
        profile.push_str("(allow network*)\n");
    } else {
        // The kernel still expects an explicit allow for AF_UNIX so that
        // local sockets (DNS resolver shared memory, IPC) work; only the
        // network-family connections are denied by default.
        profile.push_str("(allow network* (local unix))\n");
        profile.push_str("(allow network-inbound (local unix))\n");
    }
    profile
}

/// Read-only roots every shell needs to look at: system libraries, the
/// dynamic linker, certificate stores, the toolchain prefix, and the user's
/// rustup / cargo prefixes. We add the prefixes as reads here AND as
/// writable roots below so cargo can read its registry index even when
/// not invoked under `cargo build`.
#[cfg(target_os = "macos")]
fn macos_read_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = [
        "/usr",
        "/bin",
        "/sbin",
        "/System",
        "/Library",
        "/private/etc",
        "/private/var/db",
        "/private/var/folders",
        "/opt",
        "/dev/null",
        "/dev/zero",
        "/dev/random",
        "/dev/urandom",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    // Toolchain prefixes the user may have configured.
    for name in ["CARGO_HOME", "RUSTUP_HOME"] {
        if let Some(path) = env::var_os(name).map(PathBuf::from) {
            roots.push(path);
        }
    }
    // Default toolchain locations under $HOME.
    if let Some(home) = env::var_os("HOME").map(PathBuf::from) {
        roots.push(home.join(".cargo"));
        roots.push(home.join(".rustup"));
    }
    roots
}

#[cfg(target_os = "macos")]
fn shell_writable_roots(root: &Path) -> Vec<PathBuf> {
    let mut roots = vec![
        root.to_path_buf(),
        PathBuf::from("/tmp"),
        PathBuf::from("/private/tmp"),
        PathBuf::from("/private/var/folders"),
    ];
    for name in ["TMPDIR", "TEMP", "TMP", "CARGO_HOME", "RUSTUP_HOME"] {
        if let Some(path) = env::var_os(name).map(PathBuf::from) {
            roots.push(path);
        }
    }
    if let Some(home) = env::var_os("HOME").map(PathBuf::from) {
        // The toolchain writes through `cargo build` / `cargo test` etc.;
        // adding these by default avoids breaking the canonical use case
        // when `mode = "required"`.
        roots.push(home.join(".cargo"));
        roots.push(home.join(".rustup"));
    }
    roots.sort();
    roots.dedup();
    roots
}

fn sensitive_absolute_paths(root: &Path, config: &ShellSandboxConfig) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for pattern in &config.sensitive_path_patterns {
        let base = sensitive_pattern_base(pattern);
        if base.is_empty() {
            continue;
        }
        paths.push(root.join(&base));
        if let Some(home) = env::var_os("HOME").map(PathBuf::from) {
            paths.push(home.join(&base));
        }
    }
    paths
}

fn sensitive_pattern_base(pattern: &str) -> String {
    let trimmed = pattern
        .trim()
        .trim_end_matches('*')
        .trim_end_matches('/')
        .trim_end_matches("/**");
    trimmed.trim_start_matches('/').to_string()
}

#[cfg(target_os = "macos")]
fn sandbox_profile_string(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// Check whether the command text references any configured sensitive path
/// pattern. The matcher splits the command into tokens (respecting quotes),
/// normalises each token (expands `~` and `$HOME` against the parent env,
/// collapses `\\` to `/`), and then tests each token against each pattern's
/// base. This avoids the original implementation's substring-in-haystack
/// problem (where `.env*` matched any string containing `.env`, including
/// unrelated package or option names like `.environment`), while still
/// catching common bypasses like `$HOME/.ssh/id_rsa` and `~/.aws/config`.
fn shell_command_references_sensitive_path(command: &str, patterns: &[String]) -> Option<String> {
    let tokens = tokenize_shell_segment(command);
    let home = env::var_os("HOME").map(|s| s.to_string_lossy().into_owned());
    for raw in &tokens {
        let stripped = dequote_token(raw);
        let normalized = normalize_path_token(stripped, home.as_deref());
        for pattern in patterns {
            let base = sensitive_pattern_base(pattern);
            if !base.is_empty() && token_contains_sensitive_base(&normalized, &base) {
                return Some(pattern.clone());
            }
        }
    }
    // Backstop: also scan the raw command (with backslashes normalised)
    // for unquoted occurrences of each pattern base preceded by a path
    // separator. This catches uses like `tar --exclude='*.cache' .ssh/`
    // and unquoted `cat ~/.ssh/id_rsa`.
    let normalized_command = command.replace('\\', "/");
    for pattern in patterns {
        let base = sensitive_pattern_base(pattern);
        if base.is_empty() {
            continue;
        }
        if normalized_command_references_base(&normalized_command, &base) {
            return Some(pattern.clone());
        }
    }
    None
}

/// Normalises a path-like token for sensitive-path matching:
///   - replaces backslashes with `/`,
///   - expands a leading `~/` or `~` against `$HOME`,
///   - expands a leading `$HOME` or `${HOME}` against `$HOME`.
fn normalize_path_token(token: &str, home: Option<&str>) -> String {
    let token = token.replace('\\', "/");
    if let Some(home) = home {
        if let Some(rest) = token.strip_prefix("$HOME/") {
            return format!("{home}/{rest}");
        }
        if token == "$HOME" {
            return home.to_string();
        }
        if let Some(rest) = token.strip_prefix("${HOME}/") {
            return format!("{home}/{rest}");
        }
        if token == "${HOME}" {
            return home.to_string();
        }
        if let Some(rest) = token.strip_prefix("~/") {
            return format!("{home}/{rest}");
        }
        if token == "~" {
            return home.to_string();
        }
    }
    token
}

/// Token-side check: does `token` reference `base` either as a path
/// segment or as an exact match? Avoids matching `.env` inside
/// `.environment` or `Cargo.envelope`.
fn token_contains_sensitive_base(token: &str, base: &str) -> bool {
    if token == base {
        return true;
    }
    // Strip leading `/` so absolute and relative both compare segment-wise.
    let token = token.trim_start_matches('/');
    let base = base.trim_end_matches('/');
    for piece in token.split('/') {
        if piece == base {
            return true;
        }
        // Trailing wildcard support for patterns like `.env*` → base
        // `.env`: require the segment to begin with `.env.` or `.env-`
        // or be exactly `.env`, not match `.environment`.
        if let Some(rest) = piece.strip_prefix(base)
            && (rest.is_empty()
                || rest.starts_with('.')
                || rest.starts_with('-')
                || rest.starts_with('_'))
        {
            return true;
        }
    }
    false
}

/// Command-side check: matches `base` when preceded by a path separator
/// (or appearing at the start of a token). Handles unquoted uses like
/// `tar -czf out.tgz ~/.ssh` even when the tokenizer would otherwise
/// have split `~/.ssh` away from the path matcher.
fn normalized_command_references_base(command: &str, base: &str) -> bool {
    let needles = [format!("/{base}"), format!(" {base}"), format!("\t{base}")];
    for needle in &needles {
        if let Some(idx) = command.find(needle.as_str()) {
            let next = command[idx + needle.len()..].chars().next();
            if next
                .map(|c| matches!(c, '/' | ' ' | '\t' | '\0' | '"' | '\''))
                .unwrap_or(true)
            {
                return true;
            }
            // Allow segment-style follow-ups (e.g. `.env.production`).
            if next.map(|c| matches!(c, '.' | '-' | '_')).unwrap_or(false) {
                return true;
            }
        }
    }
    false
}

/// Recognises the on-process signals that the sandbox backend itself
/// failed to apply (as opposed to the user's command failing). Used in
/// `mode = "required"` to deny the call rather than silently letting it
/// run unsandboxed.
fn shell_sandbox_runtime_unavailable(
    plan: &ShellSandboxPlan,
    exit_code: Option<i32>,
    stderr: &str,
) -> bool {
    match plan.backend {
        "macos-sandbox-exec" => {
            // sandbox-exec returns 71 with a `sandbox_apply` message when
            // the kernel refuses to apply the SBPL profile.
            exit_code == Some(71) && stderr.contains("sandbox_apply")
        }
        "linux-direct-syscalls" => {
            // The pre_exec hook returns Err with an EPERM/EINVAL when
            // unshare fails after a successful spawn handshake. Tokio's
            // child reports this as a Unix `_exit(1)`/wait status with
            // empty stdout/stderr; we can't distinguish that perfectly
            // from a legitimate `exit 1`. Fall back to a probe: re-check
            // the supported-flag at the parent level, and report
            // unavailable if the kernel no longer supports unshare.
            !linux_unshare_supported() && exit_code == Some(1) && stderr.is_empty()
        }
        _ => false,
    }
}

fn configure_shell_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        command.process_group(0);
    }
}

fn configure_linux_shell_sandbox(command: &mut Command, plan: &ShellSandboxPlan) {
    #[cfg(target_os = "linux")]
    if plan.backend == "linux-direct-syscalls" {
        let deny_network = plan.network == "denied";
        // `Command::process_group(0)` already arranges a `setpgid(0, 0)` in
        // the child's pre_exec, so we don't duplicate it here. We focus on
        // the namespace unshare, which is the additional isolation step.
        // CLONE_NEWUSER + uid_map is required for an unprivileged process
        // to call unshare(CLONE_NEWNS) on stock distros; we fall back to a
        // single-step unshare if user-namespace setup is forbidden so that
        // best-effort mode does not hard-fail on every call.
        unsafe {
            command.pre_exec(move || linux_unshare_pre_exec(deny_network));
        }
    }

    #[cfg(not(target_os = "linux"))]
    let _ = (command, plan);
}

#[cfg(target_os = "linux")]
fn linux_unshare_pre_exec(deny_network: bool) -> std::io::Result<()> {
    // Capture the parent's uid/gid before any namespace switch.
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };

    // Preferred path: open a user namespace first so the subsequent mount
    // and network namespace creation are allowed without CAP_SYS_ADMIN.
    let mut flags = libc::CLONE_NEWUSER | libc::CLONE_NEWNS;
    if deny_network {
        flags |= libc::CLONE_NEWNET;
    }
    if unsafe { libc::unshare(flags) } == 0 {
        // Best effort: drop the inherited setgroups capability and map our
        // uid/gid into the new user namespace. If any of these writes fail
        // (e.g. /proc not yet mounted), continue — the sandbox is still in
        // place; the only effect is that uid/gid inside the namespace look
        // unmapped.
        let _ = linux_write_proc("/proc/self/setgroups", b"deny");
        let _ = linux_write_proc("/proc/self/uid_map", format!("0 {uid} 1").as_bytes());
        let _ = linux_write_proc("/proc/self/gid_map", format!("0 {gid} 1").as_bytes());
        return Ok(());
    }

    // Fallback path: try the privileged form. Will succeed in containers
    // launched with CAP_SYS_ADMIN, fail with EPERM otherwise.
    let mut fallback = libc::CLONE_NEWNS;
    if deny_network {
        fallback |= libc::CLONE_NEWNET;
    }
    if unsafe { libc::unshare(fallback) } == 0 {
        return Ok(());
    }
    Err(std::io::Error::last_os_error())
}

#[cfg(target_os = "linux")]
fn linux_write_proc(path: &str, contents: &[u8]) -> std::io::Result<()> {
    let mut file = OpenOptions::new().write(true).open(path)?;
    file.write_all(contents)
}

/// Probe whether the kernel currently permits unprivileged user-namespace
/// creation. We do this from the parent process by reading the well-known
/// sysctl knob; this is the same signal that controls whether the eventual
/// child `unshare(CLONE_NEWUSER|...)` will succeed. If the sysctl is
/// missing (older kernels, namespaces unsupported altogether) we treat
/// that as "not supported" so required mode denies pre-spawn instead of
/// silently failing inside the child.
#[cfg(target_os = "linux")]
fn linux_unshare_supported() -> bool {
    if let Ok(value) = std::fs::read_to_string("/proc/sys/kernel/unprivileged_userns_clone") {
        if value.trim() == "0" {
            return false;
        }
    }
    // /proc/self/ns/user existing is necessary for the syscall to do
    // anything useful; this also covers WSL1 (no namespaces).
    std::path::Path::new("/proc/self/ns/user").exists()
}

/// Stub for non-Linux compilation so the macOS / cross-compile builds keep
/// working without `#[cfg]` everywhere in callers.
#[cfg(not(target_os = "linux"))]
#[allow(dead_code)]
fn linux_unshare_supported() -> bool {
    false
}

async fn terminate_shell_child(child: &mut tokio::process::Child, grace_ms: u64) {
    if let Some(pid) = child.id() {
        kill_process_group(pid, libc::SIGTERM);
        if time::timeout(Duration::from_millis(grace_ms), child.wait())
            .await
            .is_ok()
        {
            return;
        }
        kill_process_group(pid, libc::SIGKILL);
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

fn kill_process_group(pid: u32, signal: libc::c_int) {
    #[cfg(unix)]
    unsafe {
        let _ = libc::kill(-(pid as libc::pid_t), signal);
    }

    #[cfg(not(unix))]
    let _ = (pid, signal);
}

fn apply_shell_environment_policy(
    command: &mut Command,
    config: &ShellSandboxConfig,
) -> Vec<String> {
    let mut preserved = BTreeMap::<String, OsString>::new();
    for (name, value) in env::vars_os() {
        let Some(name) = name.to_str() else {
            continue;
        };
        if shell_env_should_preserve(name, &config.env_allowlist) {
            preserved.insert(name.to_string(), value);
        }
    }

    command.env_clear();
    for (name, value) in &preserved {
        command.env(name, value);
    }
    preserved.into_keys().collect()
}

fn shell_env_should_preserve(name: &str, allowlist: &[String]) -> bool {
    allowlist.iter().any(|pattern| {
        if let Some(prefix) = pattern.strip_suffix('*') {
            name.starts_with(prefix)
        } else {
            name == pattern
        }
    })
}

fn build_required_glob(pattern: &str) -> std::result::Result<GlobSet, String> {
    let mut builder = GlobSetBuilder::new();
    if pattern.contains('/') {
        builder.add(Glob::new(pattern).map_err(|err| err.to_string())?);
    } else {
        builder.add(Glob::new(pattern).map_err(|err| err.to_string())?);
        builder.add(Glob::new(&format!("**/{pattern}")).map_err(|err| err.to_string())?);
    }
    builder.build().map_err(|err| err.to_string())
}

fn build_include_set(patterns: Option<&[String]>) -> std::result::Result<Option<GlobSet>, String> {
    let Some(patterns) = patterns else {
        return Ok(None);
    };
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        if pattern.contains('/') {
            builder.add(Glob::new(pattern).map_err(|err| err.to_string())?);
        } else {
            builder.add(Glob::new(pattern).map_err(|err| err.to_string())?);
            builder.add(Glob::new(&format!("**/{pattern}")).map_err(|err| err.to_string())?);
        }
    }
    builder.build().map(Some).map_err(|err| err.to_string())
}

fn read_prefix(path: &Path, limit: usize) -> std::result::Result<Vec<u8>, std::io::Error> {
    let mut file = fs::File::open(path)?;
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(limit as u64)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn read_range(
    path: &Path,
    offset: u64,
    limit: usize,
) -> std::result::Result<Vec<u8>, std::io::Error> {
    let mut file = fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(limit as u64)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn sha256_file(path: &Path) -> std::result::Result<String, std::io::Error> {
    let mut file = fs::File::open(path)?;
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
        output.push_str(&format!("{byte:02x}"));
    }
    Ok(output)
}

fn file_len(path: &Path) -> std::result::Result<u64, std::io::Error> {
    Ok(fs::metadata(path)?.len())
}

async fn read_limited_pipe<R>(
    mut reader: Option<R>,
    remaining_bytes: Arc<Mutex<usize>>,
) -> std::result::Result<(Vec<u8>, bool), std::io::Error>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let Some(mut reader) = reader.take() else {
        return Ok((Vec::new(), false));
    };
    let mut output = Vec::new();
    let mut buffer = vec![0u8; 8192];
    let mut truncated = false;

    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        let mut remaining = remaining_bytes.lock().await;
        let keep = count.min(*remaining);
        if keep > 0 {
            output.extend_from_slice(&buffer[..keep]);
            *remaining -= keep;
        }
        if keep < count {
            truncated = true;
        }
    }

    Ok((output, truncated))
}

async fn join_limited_pipe(
    handle: tokio::task::JoinHandle<std::result::Result<(Vec<u8>, bool), std::io::Error>>,
) -> std::result::Result<(Vec<u8>, bool), std::io::Error> {
    handle
        .await
        .map_err(|err| std::io::Error::other(format!("shell output reader failed: {err}")))?
}

fn contains_skipped_dir(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|part| matches!(part, ".git" | ".hg" | ".svn" | ".squeezy"))
    })
}

fn is_secret_path(path: &Path) -> bool {
    path.components().any(|component| {
        let Some(part) = component.as_os_str().to_str() else {
            return false;
        };
        let part = part.to_ascii_lowercase();
        part == ".env"
            || part.starts_with(".env.")
            || part.contains("secret")
            || part.contains("credential")
            || part == "id_rsa"
            || part == "id_ed25519"
            || part.ends_with(".pem")
            || part.ends_with(".key")
            || part.ends_with(".p12")
    })
}

fn truncate_text(value: &str, max_chars: usize) -> String {
    let mut output = String::new();
    for ch in value.chars().take(max_chars) {
        output.push(ch);
    }
    if output.len() < value.len() {
        output.push_str("...");
    }
    output
}

fn truncate_to_bytes(value: &str, cap: usize) -> (String, bool) {
    if value.len() <= cap {
        return (value.to_string(), false);
    }
    let mut end = cap;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    (value[..end].to_string(), true)
}

pub fn sha256_hex(bytes: impl AsRef<[u8]>) -> String {
    let digest = Sha256::digest(bytes.as_ref());
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

fn checkpoint_list_spec() -> ToolSpec {
    ToolSpec {
        name: "checkpoint_list".to_string(),
        description: "List recent recoverable checkpoints created by mutation tools.".to_string(),
        capability: PermissionCapability::Read,
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {}
        }),
    }
}

fn checkpoint_undo_spec() -> ToolSpec {
    ToolSpec {
        name: "checkpoint_undo".to_string(),
        description: "Undo the latest checkpoint. Default mode is atomic: any conflict leaves all files unchanged. Use best_effort to restore clean files and skip conflicts.".to_string(),
        capability: PermissionCapability::Edit,
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "mode": {"type": "string", "enum": ["atomic", "best_effort"], "description": "Rollback mode. Default atomic."}
            }
        }),
    }
}

fn checkpoint_show_spec() -> ToolSpec {
    ToolSpec {
        name: "checkpoint_show".to_string(),
        description: "Inspect one checkpoint, including file metadata, patch text when available, skipped files, and rollback coverage warnings.".to_string(),
        capability: PermissionCapability::Read,
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "checkpoint_id": {"type": "string", "description": "Checkpoint id returned by checkpoint_list or mutation tool output."}
            },
            "required": ["checkpoint_id"]
        }),
    }
}

fn checkpoint_revert_spec() -> ToolSpec {
    ToolSpec {
        name: "checkpoint_revert".to_string(),
        description: "Revert either a checkpoint_id or all checkpoints in a group_id. Default mode is atomic: any conflict leaves all files unchanged. Use best_effort to restore clean files and skip conflicts.".to_string(),
        capability: PermissionCapability::Edit,
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "group_id": {"type": "string", "description": "Checkpoint group id, usually the agent turn id."},
                "checkpoint_id": {"type": "string", "description": "Specific checkpoint id to revert."},
                "mode": {"type": "string", "enum": ["atomic", "best_effort"], "description": "Rollback mode. Default atomic."}
            }
        }),
    }
}

fn diff_context_spec() -> ToolSpec {
    ToolSpec {
        name: "diff_context".to_string(),
        description: "Return the current Git change set with compact semantic graph cross-references. Use this first for questions like 'what did I change?' or 'what does this diff affect?'.".to_string(),
        capability: PermissionCapability::Read,
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "mode": {"type": "string", "enum": ["worktree", "branch"], "description": "worktree compares current staged/unstaged/untracked changes to HEAD; branch compares the current branch to the default-branch merge base. Default worktree."},
                "include_patch": {"type": "boolean", "description": "Include unified patch text. Default false to keep output compact."},
                "max_files": {"type": "integer", "minimum": 1, "maximum": 500},
                "max_symbols_per_file": {"type": "integer", "minimum": 1, "maximum": 100},
                "max_references_per_symbol": {"type": "integer", "minimum": 1, "maximum": 50},
                "max_patch_bytes": {"type": "integer", "minimum": 1, "maximum": 5000000}
            }
        }),
    }
}

fn grep_spec() -> ToolSpec {
    ToolSpec {
        name: "grep".to_string(),
        description: "Search text files under a workspace path. Respects .gitignore by default; set include_ignored=true only when ignored files are intentionally needed. Use output_mode=count or files_with_matches for broad exploration before reading content.".to_string(),
        capability: PermissionCapability::Search,
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "pattern": {"type": "string", "description": "Rust regex pattern to search for."},
                "path": {"type": "string", "description": "Workspace-relative file or directory to search.", "default": "."},
                "include": {"type": "array", "items": {"type": "string"}, "description": "Optional glob patterns such as *.rs or crates/**/lib.rs."},
                "include_ignored": {"type": "boolean", "description": "When true, include files ignored by .gitignore and other ignore files. Default false."},
                "diff_only": {"type": "boolean", "description": "When true, search only files changed in the current Git worktree diff. Default false."},
                "output_mode": {"type": "string", "enum": ["content", "files_with_matches", "count"], "description": "Return matching lines, only files containing matches, or only a count. Default content."},
                "max_files": {"type": "integer", "minimum": 1, "maximum": DEFAULT_MAX_FILES},
                "max_bytes_per_file": {"type": "integer", "minimum": 1, "maximum": DEFAULT_MAX_BYTES_PER_FILE},
                "max_matches": {"type": "integer", "minimum": 1, "maximum": 1000},
                "output_byte_cap": {"type": "integer", "minimum": 1, "maximum": 128000},
                "offset": {"type": "integer", "minimum": 0, "description": "Number of matching lines to skip for pagination."}
            },
            "required": ["pattern"]
        }),
    }
}

fn glob_spec() -> ToolSpec {
    ToolSpec {
        name: "glob".to_string(),
        description: "List workspace file paths matching a glob without reading file contents. Respects .gitignore by default; set include_ignored=true only when ignored paths are intentionally needed.".to_string(),
        capability: PermissionCapability::Search,
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "pattern": {"type": "string", "description": "Glob pattern such as *.rs or crates/**/Cargo.toml."},
                "path": {"type": "string", "description": "Workspace-relative directory to search.", "default": "."},
                "include_ignored": {"type": "boolean", "description": "When true, include files ignored by .gitignore and other ignore files. Default false."},
                "diff_only": {"type": "boolean", "description": "When true, list only files changed in the current Git worktree diff. Default false."},
                "max_paths": {"type": "integer", "minimum": 1, "maximum": 1000},
                "offset": {"type": "integer", "minimum": 0, "description": "Number of matched paths to skip for pagination."}
            },
            "required": ["pattern"]
        }),
    }
}

fn read_file_spec() -> ToolSpec {
    ToolSpec {
        name: "read_file".to_string(),
        description: "Read a bounded byte slice from one workspace file and return its sha256 receipt. Use grep first when locating unknown files.".to_string(),
        capability: PermissionCapability::Read,
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path": {"type": "string", "description": "Workspace-relative file path."},
                "offset": {"type": "integer", "minimum": 0, "description": "Byte offset to start reading from."},
                "limit": {"type": "integer", "minimum": 1, "maximum": MAX_READ_LIMIT, "description": "Maximum bytes to return."},
                "diff_only": {"type": "boolean", "description": "When true, refuse to read paths outside the current Git worktree diff. Default false."}
            },
            "required": ["path"]
        }),
    }
}

fn read_tool_output_spec() -> ToolSpec {
    ToolSpec {
        name: "read_tool_output".to_string(),
        description:
            "Read a bounded byte range from a spilled tool-output handle returned by another tool."
                .to_string(),
        capability: PermissionCapability::Read,
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "handle": {"type": "string", "description": "Tool output handle from a spilled result."},
                "offset": {"type": "integer", "minimum": 0, "description": "Byte offset to start reading from."},
                "limit": {"type": "integer", "minimum": 1, "maximum": MAX_READ_LIMIT, "description": "Maximum bytes to return."}
            },
            "required": ["handle"]
        }),
    }
}

fn symbol_context_spec() -> ToolSpec {
    ToolSpec {
        name: "symbol_context".to_string(),
        description: "Return compact semantic context for symbols matching a signature query, with inline dirty/diff annotations when current Git changes touch the symbol.".to_string(),
        capability: PermissionCapability::Read,
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "query": {"type": "string", "description": "Text to match against indexed symbol signatures."},
                "path": {"type": "string", "description": "Optional workspace-relative file path filter."},
                "diff_only": {"type": "boolean", "description": "When true, return only symbols touched by the current Git diff."},
                "max_references": {"type": "integer", "minimum": 1, "maximum": 50}
            },
            "required": ["query"]
        }),
    }
}

fn list_skills_spec() -> ToolSpec {
    ToolSpec {
        name: "list_skills".to_string(),
        description: "List locally discovered Squeezy skills by metadata only. Use before load_skill when the task may benefit from specialized instructions. Skill bodies are not included in this listing.".to_string(),
        capability: PermissionCapability::Read,
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {}
        }),
    }
}

fn load_skill_spec() -> ToolSpec {
    ToolSpec {
        name: "load_skill".to_string(),
        description: "Load one locally discovered skill body into the conversation when the user explicitly requests it or the task matches a listed skill description. Loading a skill only adds instructions and does not change tool permissions.".to_string(),
        capability: PermissionCapability::Read,
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "name": {"type": "string", "description": "Exact skill name from list_skills."}
            },
            "required": ["name"]
        }),
    }
}

fn write_file_spec() -> ToolSpec {
    ToolSpec {
        name: "write_file".to_string(),
        description: "Replace a workspace file with exact content. Existing files require expected_sha256 from read_file to prevent stale writes.".to_string(),
        capability: PermissionCapability::Edit,
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "path": {"type": "string", "description": "Workspace-relative file path."},
                "content": {"type": "string", "description": "Full replacement file content."},
                "expected_sha256": {"type": "string", "description": "sha256 of the current file content. Required for existing files."}
            },
            "required": ["path", "content"]
        }),
    }
}

fn shell_spec() -> ToolSpec {
    ToolSpec {
        name: "shell".to_string(),
        description: "Run a bounded shell command in the workspace. Use for verification commands after explaining the purpose in description.".to_string(),
        capability: PermissionCapability::Shell,
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "command": {"type": "string", "description": "Command passed to sh -lc."},
                "workdir": {"type": "string", "description": "Workspace-relative working directory.", "default": "."},
                "timeout_ms": {"type": "integer", "minimum": 1, "maximum": MAX_SHELL_TIMEOUT_MS},
                "output_byte_cap": {"type": "integer", "minimum": 1, "maximum": 128000},
                "description": {"type": "string", "description": "Short reason this command is needed."}
            },
            "required": ["command", "description"]
        }),
    }
}

fn verify_spec() -> ToolSpec {
    ToolSpec {
        name: "verify".to_string(),
        description: "Run bounded local verification, defaulting to the current Git diff scope. For Rust diffs this runs package-scoped cargo tests when possible; full mode adds fmt and clippy.".to_string(),
        capability: PermissionCapability::Compiler,
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "scope": {"type": "string", "enum": ["diff", "workspace"], "description": "Verification scope. Default diff."},
                "level": {"type": "string", "enum": ["quick", "full"], "description": "quick runs tests; full adds fmt and clippy. Default quick."}
            }
        }),
    }
}

fn webfetch_spec() -> ToolSpec {
    ToolSpec {
        name: "webfetch".to_string(),
        description: "Fetch a specific HTTP(S) URL with the host/domain shown in the approval summary. Use only for URLs provided by the user, found in local files, or discovered through websearch. Returns bounded text or HTML; redirects to another host are reported for a new approval.".to_string(),
        capability: PermissionCapability::Network,
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "url": {"type": "string", "description": "Fully-qualified http:// or https:// URL to fetch."},
                "format": {"type": "string", "enum": ["text", "html"], "description": "Return cleaned text or raw HTML. Default text."},
                "timeout_ms": {"type": "integer", "minimum": 1, "maximum": MAX_WEB_TIMEOUT_MS},
                "max_response_bytes": {"type": "integer", "minimum": 1, "maximum": MAX_WEB_FETCH_MAX_RESPONSE_BYTES},
                "output_byte_cap": {"type": "integer", "minimum": 1, "maximum": 128000}
            },
            "required": ["url"]
        }),
    }
}

fn websearch_spec() -> ToolSpec {
    ToolSpec {
        name: "websearch".to_string(),
        description: "Search the web for current or external information using Squeezy's permission-gated Exa search backend. Use for discovery; use webfetch when retrieving a specific URL.".to_string(),
        capability: PermissionCapability::Network,
        parameters: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "query": {"type": "string", "description": "Web search query."},
                "num_results": {"type": "integer", "minimum": 1, "maximum": MAX_WEB_SEARCH_RESULTS, "description": "Number of results to request. Default 8."},
                "search_type": {"type": "string", "enum": ["auto", "fast", "deep"], "description": "Search depth. Default auto."},
                "livecrawl": {"type": "string", "enum": ["fallback", "preferred"], "description": "Live crawl behavior. Default fallback."},
                "context_max_characters": {"type": "integer", "minimum": 1, "maximum": MAX_WEB_SEARCH_CONTEXT_CHARS},
                "timeout_ms": {"type": "integer", "minimum": 1, "maximum": MAX_WEB_TIMEOUT_MS}
            },
            "required": ["query"]
        }),
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
