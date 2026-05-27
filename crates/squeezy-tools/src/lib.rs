use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs::{self, OpenOptions},
    future::Future,
    io::{Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(test)]
use std::env;

use globset::{Glob, GlobSet, GlobSetBuilder};
#[cfg(test)]
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
#[cfg(test)]
use squeezy_core::ShellSandboxNetworkPolicy;
use squeezy_core::{
    Confidence, DEFAULT_EXA_MCP_URL, DEFAULT_TOOL_OUTPUT_RETENTION_DAYS,
    DEFAULT_TOOL_PREVIEW_BYTES, DEFAULT_TOOL_SPILL_THRESHOLD_BYTES, GraphConfig, McpServerConfig,
    PermissionCapability, PermissionMode, PermissionRequest, PermissionRisk, PermissionRule,
    PermissionRuleSource, PermissionScope, Redactor, Result, ShellSandboxConfig, ShellSandboxMode,
    SkillsConfig, SqueezyError,
};
use squeezy_graph::{CargoFactProvenance, GraphManager};
use squeezy_mcp::{ExternalMcpTool, McpClientRegistry};
pub use squeezy_mcp::{
    McpElicitationAction, McpElicitationHandler, McpElicitationKind, McpElicitationRequest,
    McpElicitationResponse, McpRefreshOutcome, McpServerStatus, McpStatusSnapshot,
};
use squeezy_skills::{LoadedSkill, SkillActivation, SkillCatalog, SkillPreambleRender};
use squeezy_store::{Observation, ObservationKind, SqueezyStore};
use squeezy_vcs::{
    CheckpointRecord, CheckpointStore, DiffFile, DiffFileStatus, DiffMode, DiffOptions,
    DiffSnapshot, GitVcs, WorkspaceSnapshot, canonicalize_workspace_root, strip_verbatim_prefix,
};
use squeezy_workspace::{CompiledIndexingPolicy, CrawlOptions, ExclusionReason, IndexingPolicy};
use tokio::sync::{Mutex, Semaphore};
use tokio_util::sync::CancellationToken;

mod checkpoints;
mod file_ops;
mod graph_tools;
mod ipc;
mod patch;
pub mod preview;
mod safety;
mod schema;
mod shell;
mod shell_output;
mod shell_parse;
mod shell_program;
mod shell_sandbox;
mod specs;
mod truncate;
mod web;
#[cfg(windows)]
mod win_job;
mod windows_cmd;

use checkpoints::{CheckpointRevertArgs, CheckpointShowArgs};
use file_ops::{GlobArgs, GrepArgs, ReadFileArgs, ReadToolOutputArgs};
use graph_tools::{
    ReadSliceArgs, SymbolContextArgs, cargo_facts_summary_json, graph_unavailable_result,
};
pub use ipc::{IpcEndpoint, IpcStream};
use patch::{
    ApplyPatchArgs, ApplyPatchOperation, DiffContextArgs, PATCH_SNIPPET_MAX_CHARS, PatchPlan,
    PlanPatchArgs, SearchReplaceFallback, render_apply_patch_diff, render_write_file_diff,
};
pub use safety::{ShellPreClassification, pre_classify_shell};
use schema::compact_tool_parameters;
pub use shell::direct_user_shell_nonce;
pub(crate) use shell::{ShellArgs, ShellExecutionGuard, ShellRunOutcome};
#[cfg(test)]
pub(crate) use shell::{
    shell_command_needs_checkpoint, shell_command_references_sensitive_path,
    shell_env_should_preserve, shell_termination_reason,
};
use shell_output::insert_content_field;
#[cfg(test)]
use shell_output::shape_shell_output;
use shell_parse::analyze_shell_command;
#[cfg(test)]
use shell_parse::{shell_coverage_warnings, shell_segments};
#[cfg(test)]
use shell_program::ShellProgram;
use specs::{
    apply_patch_spec, checkpoint_list_spec, checkpoint_revert_spec, checkpoint_show_spec,
    checkpoint_undo_spec, decl_search_spec, definition_search_spec, diff_context_spec,
    downstream_flow_spec, glob_spec, grep_spec, hierarchy_spec, list_skills_spec, load_skill_spec,
    mcp_list_resource_templates_spec, mcp_list_resources_spec, mcp_read_resource_spec,
    mcp_tool_spec, notes_recall_spec, notes_remember_spec, observations_spec, plan_patch_spec,
    read_file_spec, read_slice_spec, read_tool_output_spec, reference_search_spec,
    refresh_compiler_facts_spec, repo_map_spec, shell_spec, symbol_context_spec,
    upstream_flow_spec, verify_spec, webfetch_spec, websearch_spec, write_file_spec,
};

#[cfg(all(test, target_os = "macos"))]
use shell_sandbox::macos_shell_sandbox_profile;
#[cfg(test)]
use shell_sandbox::{
    ShellSandboxBackendStatus, prepare_shell_sandbox_plan_with_probe,
    shell_sandbox_runtime_unavailable_with_probe,
};
use shell_sandbox::{
    ShellSandboxHealth, ShellSandboxPlan, apply_shell_sandbox_backend_health,
    prepare_shell_sandbox_plan, shell_sandbox_backend_probe_failure,
};
#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
use shell_sandbox::{
    shell_sandbox_best_effort_fallback_reason, shell_sandbox_direct_fallback_reason,
};
use truncate::truncate_middle_bytes;
pub use web::{DEFAULT_PARALLEL_MCP_URL, WebSearchProvider};
#[cfg(test)]
pub(crate) use web::{
    MAX_WEB_REDIRECTS, WebHttpFuture, WebHttpResponse, extract_http_urls, html_to_text,
    is_textual_content_type, parse_mcp_websearch_response, web_cache_receipt_status,
    web_cache_stale_after_unix_ms, web_stable_output_sha256,
};
use web::{
    ReqwestWebHttpClient, WebFetchArgs, WebHttpClient, WebSearchArgs, enforce_web_quote_limit,
    web_url_host,
};

pub(crate) const DEFAULT_MAX_FILES: usize = 10_000;
pub(crate) const DEFAULT_MAX_BYTES_PER_FILE: usize = 1_000_000;
pub(crate) const CHECKPOINTS_DISABLED_MESSAGE: &str = "checkpointing is disabled by default; commit or stash with git, or set [tools].checkpoints_enabled = true to re-enable Squeezy checkpoints";
pub(crate) const DEFAULT_READ_LIMIT: usize = 32_000;
pub(crate) const MAX_READ_LIMIT: usize = 128_000;
pub(crate) const DEFAULT_SHELL_TIMEOUT_MS: u64 = 30_000;
pub(crate) const MAX_SHELL_TIMEOUT_MS: u64 = 120_000;
pub(crate) const IO_DRAIN_TIMEOUT_MS: u64 = 2_000;
const MAX_INFLIGHT_SHELLS: usize = 4;
const VERIFY_SHELL_TIMEOUT_MS: u64 = 600_000;
pub(crate) const DEFAULT_SHELL_OUTPUT_BYTE_CAP: usize = 32_000;
pub(crate) const MAX_SHELL_OUTPUT_BYTE_CAP: usize = 128_000;
const DIFF_SNAPSHOT_TTL: Duration = Duration::from_millis(500);
pub(crate) const POLICY_PREFIX_BYTES: usize = 4096;
pub(crate) const DEFAULT_GRAPH_MAX_RESULTS: usize = 25;
pub(crate) const MAX_GRAPH_MAX_RESULTS: usize = 100;
pub(crate) const DEFAULT_GRAPH_MAX_DEPTH: usize = 3;
pub(crate) const MAX_GRAPH_MAX_DEPTH: usize = 8;
pub(crate) const GRAPH_READ_SLICE_MAX_LINE_SCAN_BYTES: u64 = 5_000_000;

/// Per-process runtime bits the registry needs alongside its tool-specific
/// configs. Grouping them keeps the public constructor signature under
/// `clippy::too_many_arguments` while leaving each tool config struct as the
/// place to look for that tool's settings.
///
/// `state_store` carries an already-open [`SqueezyStore`] when the caller wants
/// the registry's graph manager to share persistence with the surrounding
/// agent. redb enforces single-handle access per database file (verified by
/// `state_store_open_rejects_a_second_handle_on_the_same_file`), so callers
/// that also need the store outside the registry must open it once and pass
/// the same `Arc` in here rather than open a parallel handle.
#[derive(Debug, Clone, Default)]
pub struct ToolRegistryRuntime {
    /// Shared persistent state store. `None` disables graph persistence in
    /// the registry's `GraphManager` (matches the pre-persistence default).
    pub state_store: Option<Arc<SqueezyStore>>,
    /// Shared redactor used by tools that surface user-visible text.
    pub redactor: Arc<Redactor>,
}

impl ToolRegistryRuntime {
    pub fn new(state_store: Option<Arc<SqueezyStore>>, redactor: Arc<Redactor>) -> Self {
        Self {
            state_store: state_store.clone(),
            redactor,
        }
    }
}

pub const SQUEEZY_ASK_SOCKET_ENV: &str = "SQUEEZY_ASK_SOCKET";
pub const SQUEEZY_ASK_CALL_ID_ENV: &str = "SQUEEZY_ASK_CALL_ID";

pub type ShellAskFuture = Pin<Box<dyn Future<Output = ShellAskDecision> + Send>>;
pub type ShellAskApprover = Arc<dyn Fn(ShellAskRequest) -> ShellAskFuture + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellAskRequest {
    pub call_id: String,
    pub parent_command: String,
    pub command: String,
    pub justification: String,
    pub workdir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellAskDecision {
    pub allow: bool,
    pub reason: String,
}

impl ShellAskDecision {
    pub fn allow() -> Self {
        Self {
            allow: true,
            reason: "approved".to_string(),
        }
    }

    pub fn deny(reason: impl Into<String>) -> Self {
        Self {
            allow: false,
            reason: reason.into(),
        }
    }
}

#[derive(Clone, Default)]
pub struct ToolExecutionOptions {
    pub shell_ask_approver: Option<ShellAskApprover>,
}

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

impl ToolSpec {
    /// Apply the schema-compaction pipeline to `parameters`. Idempotent — safe
    /// to call on a spec that has already been compacted.
    pub(crate) fn with_compacted_parameters(mut self) -> Self {
        compact_tool_parameters(&mut self.parameters);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    pub arguments: Value,
}

/// Render a one-phrase English description of a tool call from its name
/// and arguments. Pure function — the same `(name, args)` always produces
/// the same string, so the live printer and post-run view can render
/// identical labels without round-tripping anything through the result.
///
/// Falls back to the tool name when no specific template applies; never
/// returns an empty string.
pub fn human_label_for_call(name: &str, args: &Value) -> String {
    let s = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    let n = |key: &str| args.get(key).and_then(Value::as_u64);
    let qualify_path = |label: &mut String| {
        if let Some(path) = s("path") {
            label.push_str(" in `");
            label.push_str(&path);
            label.push('`');
        }
    };
    let display_symbol = |key: &str| -> Option<String> {
        // Symbol ids like `crate::module::Type::method` — render as-is;
        // they are already human-readable. Fall back to the raw value.
        s(key)
    };
    match name {
        "definition_search" => {
            let query = s("query").unwrap_or_else(|| "?".to_string());
            let mut label = String::from("looking up ");
            if let Some(kind) = s("kind") {
                label.push_str(&kind);
                label.push(' ');
            }
            label.push('`');
            label.push_str(&query);
            label.push('`');
            qualify_path(&mut label);
            label
        }
        "symbol_context" => {
            let query = s("query").unwrap_or_else(|| "?".to_string());
            let mut label = format!("getting context for `{query}`");
            qualify_path(&mut label);
            label
        }
        "decl_search" => {
            let kind = s("kind").unwrap_or_else(|| "any".to_string());
            let query = s("query")
                .map(|q| format!(" matching `{q}`"))
                .unwrap_or_default();
            let mut label = format!("searching {kind} declarations{query}");
            qualify_path(&mut label);
            label
        }
        "reference_search" => {
            let target = display_symbol("symbol_id")
                .or_else(|| s("query"))
                .unwrap_or_else(|| "?".to_string());
            let mut label = format!("finding references to `{target}`");
            qualify_path(&mut label);
            label
        }
        "downstream_flow" => {
            let q = s("query").unwrap_or_else(|| "?".to_string());
            format!("tracing flow downstream from `{q}`")
        }
        "upstream_flow" => {
            let q = s("query").unwrap_or_else(|| "?".to_string());
            format!("tracing flow upstream from `{q}`")
        }
        "hierarchy" => {
            let q = s("query").unwrap_or_else(|| "?".to_string());
            format!("walking the call hierarchy of `{q}`")
        }
        "repo_map" => match n("max_depth") {
            Some(d) => format!("building a repo map (depth {d})"),
            None => "building a repo map".to_string(),
        },
        "read_slice" => {
            if let Some(symbol) = display_symbol("symbol_id") {
                let span = s("span_kind").unwrap_or_else(|| "slice".to_string());
                format!("reading {span} of `{symbol}`")
            } else if let Some(path) = s("path") {
                match (n("start_line"), n("end_line")) {
                    (Some(start), Some(end)) => format!("reading `{path}:{start}-{end}`"),
                    (Some(start), None) => format!("reading `{path}` from line {start}"),
                    _ => format!("reading `{path}`"),
                }
            } else {
                "reading a slice".to_string()
            }
        }
        "read_file" => match s("path") {
            Some(path) => format!("reading `{path}`"),
            None => "reading a file".to_string(),
        },
        "grep" => {
            let pat = s("pattern")
                .or_else(|| s("query"))
                .unwrap_or_else(|| "?".to_string());
            let mut label = format!("grepping for `{pat}`");
            qualify_path(&mut label);
            label
        }
        "glob" => {
            let pat = s("pattern").unwrap_or_else(|| "?".to_string());
            format!("globbing for `{pat}`")
        }
        "shell" | "verify" => match s("command") {
            Some(cmd) => format!("running `{cmd}`"),
            None => format!("running {name}"),
        },
        "websearch" => match s("query") {
            Some(q) => format!("searching the web for `{q}`"),
            None => "searching the web".to_string(),
        },
        "webfetch" => match s("url") {
            Some(u) => format!("fetching `{u}`"),
            None => "fetching a URL".to_string(),
        },
        "plan_patch" => match s("objective") {
            Some(o) => format!("planning a patch for `{o}`"),
            None => "planning a patch".to_string(),
        },
        "apply_patch" => "applying a patch".to_string(),
        "write_file" => match s("path") {
            Some(p) => format!("writing `{p}`"),
            None => "writing a file".to_string(),
        },
        "read_tool_output" => "expanding earlier tool output".to_string(),
        "diff_context" => "gathering diff context".to_string(),
        "checkpoint_list" => "listing checkpoints".to_string(),
        "checkpoint_show" => "inspecting a checkpoint".to_string(),
        "checkpoint_undo" => "undoing to a checkpoint".to_string(),
        "checkpoint_revert" => "reverting to a checkpoint".to_string(),
        "list_skills" => "listing skills".to_string(),
        "load_skill" => match s("name") {
            Some(n) => format!("loading skill `{n}`"),
            None => "loading a skill".to_string(),
        },
        _ => name.to_string(),
    }
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
    /// Per-`Confidence`-variant counts across the returned packets. Empty
    /// for tools that do not surface graph confidence (grep, glob,
    /// read_file, etc.); populated by graph-anchored tools so the model
    /// can reason about result quality without re-walking every packet.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub confidence_distribution: BTreeMap<String, u32>,
}

impl ToolCostHint {
    /// Build a `confidence_distribution` from an iterator of
    /// `squeezy_core::Confidence` values. Zero-count buckets are omitted.
    pub fn confidence_distribution_from(
        confidences: impl IntoIterator<Item = Confidence>,
    ) -> BTreeMap<String, u32> {
        let mut map: BTreeMap<String, u32> = BTreeMap::new();
        for c in confidences {
            *map.entry(c.id().to_string()).or_insert(0) += 1;
        }
        map
    }

    /// Build a `confidence_distribution` by reading the `confidence`
    /// field from each packet JSON value. Useful for traversal-shaped
    /// tools (upstream/downstream flow) that already have packets in
    /// hand. Unknown values are skipped.
    pub fn confidence_distribution_from_packets(packets: &[Value]) -> BTreeMap<String, u32> {
        let mut map: BTreeMap<String, u32> = BTreeMap::new();
        for packet in packets {
            let Some(label) = packet.get("confidence").and_then(Value::as_str) else {
                continue;
            };
            if let Some(c) = confidence_from_label(label) {
                *map.entry(c.id().to_string()).or_insert(0) += 1;
            }
        }
        map
    }
}

/// Map a packet's `confidence` string back to the typed variant. Accepts
/// the canonical snake_case `id()` form (e.g. `"exact_syntax"`) as well as
/// the legacy `{:?}`-formatted variant name (`"ExactSyntax"`) so older
/// captured packets continue to aggregate. Returns `None` for unknown
/// strings.
fn confidence_from_label(label: &str) -> Option<Confidence> {
    Confidence::ALL
        .iter()
        .copied()
        .find(|c| c.id() == label || format!("{c:?}") == label)
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolRuntimeConfig {
    pub output: ToolOutputConfig,
    pub web: WebToolConfig,
    pub shell_sandbox: ShellSandboxConfig,
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
    pub checkpoints_enabled: bool,
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
    pub provider: WebSearchProvider,
    pub exa_mcp_url: String,
    pub exa_api_key: Option<String>,
    pub parallel_mcp_url: String,
    pub parallel_api_key: Option<String>,
}

impl Default for WebToolConfig {
    fn default() -> Self {
        Self {
            provider: WebSearchProvider::default(),
            exa_mcp_url: DEFAULT_EXA_MCP_URL.to_string(),
            exa_api_key: None,
            parallel_mcp_url: DEFAULT_PARALLEL_MCP_URL.to_string(),
            parallel_api_key: None,
        }
    }
}

impl WebToolConfig {
    fn normalized(self) -> Self {
        fn trimmed_or(value: &str, fallback: &str) -> String {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                fallback.to_string()
            } else {
                trimmed.to_string()
            }
        }
        fn trimmed_opt(value: Option<String>) -> Option<String> {
            value.and_then(|raw| {
                let raw = raw.trim().to_string();
                (!raw.is_empty()).then_some(raw)
            })
        }
        Self {
            provider: self.provider,
            exa_mcp_url: trimmed_or(&self.exa_mcp_url, DEFAULT_EXA_MCP_URL),
            exa_api_key: trimmed_opt(self.exa_api_key),
            parallel_mcp_url: trimmed_or(&self.parallel_mcp_url, DEFAULT_PARALLEL_MCP_URL),
            parallel_api_key: trimmed_opt(self.parallel_api_key),
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
    #[serde(skip)]
    pub spill_model_output: Option<String>,
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

    pub fn with_spill_model_output(mut self, output: String) -> Self {
        self.spill_model_output = Some(output);
        self
    }

    pub fn denied(call: &ToolCall, reason: impl Into<String>) -> Self {
        let reason = reason.into();
        let guidance = format!(
            "The user denied the `{tool}` call. Do not retry the same call. \
             Consider an alternative: ask the user to clarify their preferred approach, \
             try a different tool, propose a smaller or different change, or explain \
             what you were attempting so the user can guide next steps. The turn is \
             not over — continue.",
            tool = call.name
        );
        make_result(
            call,
            ToolStatus::Denied,
            json!({
                "error": reason.clone(),
                "reason": reason,
                "permission_denied": true,
                "guidance": guidance,
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

#[derive(Clone)]
pub struct ToolRegistry {
    pub(crate) root: Arc<PathBuf>,
    pub(crate) output_store: Arc<ToolOutputStore>,
    pub(crate) web_config: Arc<WebToolConfig>,
    pub(crate) http: Arc<dyn WebHttpClient>,
    pub(crate) graph: Arc<StdMutex<Option<GraphManager>>>,
    vcs: Arc<GitVcs>,
    /// Shared persistent state store. When `None`, `read_mode=diff` with
    /// `diff_baseline=last_receipt` cannot reach any stored read snapshots and
    /// silently falls back to the `worktree` baseline (with a
    /// `baseline_fallback.reason = "last_receipt_store_unavailable"` label on
    /// the result). Several test-only registry constructors leave this as
    /// `None` on purpose — integration tests that need the receipt-stub path
    /// should build the registry through `new_with_configs_and_skills`
    /// (or `new_with_configs_skills_and_mcp`) with a populated
    /// [`ToolRegistryRuntime`].
    pub(crate) state_store: Option<Arc<SqueezyStore>>,
    pub(crate) checkpoints: Option<Arc<CheckpointStore>>,
    diff_cache: Arc<StdMutex<DiffSnapshotCache>>,
    pub(crate) skills: Arc<SkillCatalog>,
    pub(crate) redactor: Arc<Redactor>,
    pub(crate) crawl_options: Arc<CrawlOptions>,
    compiled_policy: Arc<CompiledIndexingPolicy>,
    pub(crate) shell_sandbox: Arc<ShellSandboxConfig>,
    pub(crate) shell_sandbox_health: Arc<ShellSandboxHealth>,
    pub(crate) shell_audit: Arc<ShellAuditStore>,
    pub(crate) shell_workdir_locks: Arc<StdMutex<HashMap<PathBuf, Arc<Mutex<()>>>>>,
    pub(crate) shell_inflight: Arc<Semaphore>,
    mcp: Arc<McpClientRegistry>,
    /// F04: cache for the per-turn `specs()` advertisement. The agent calls
    /// this at least once per round for cost accounting plus once more when
    /// building the LLM request; recomputing means cloning ~30 `ToolSpec`s
    /// with their `parameters: Value` blobs every time. The cache is
    /// invalidated whenever MCP refresh changes the external tool set.
    cached_specs: Arc<StdMutex<Option<Arc<Vec<ToolSpec>>>>>,
    /// Plans registered by `plan_patch` and consulted by `apply_patch` to enforce
    /// the model's stated semantic neighborhood. Keyed by `plan_id`; entries
    /// expire after [`PATCH_PLAN_TTL`].
    pub(crate) patch_plans: Arc<StdMutex<HashMap<String, PatchPlan>>>,
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
pub(crate) struct ShellAuditStore {
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

fn path_list_metadata(paths: &[PathBuf]) -> String {
    if paths.is_empty() {
        "none".to_string()
    } else {
        paths
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(",")
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
            ToolRegistryRuntime::default(),
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
            ToolRegistryRuntime::default(),
        )
    }

    pub fn new_with_configs_and_skills(
        root: impl Into<PathBuf>,
        output_config: ToolOutputConfig,
        web_config: WebToolConfig,
        skills_config: SkillsConfig,
        graph_config: &GraphConfig,
        shell_sandbox: ShellSandboxConfig,
        runtime: ToolRegistryRuntime,
    ) -> Result<Self> {
        let root = root.into();
        let root = canonicalize_workspace_root(&root)
            .map_err(|err| SqueezyError::Tool(format!("invalid workspace root: {err}")))?;
        let skills = SkillCatalog::discover(&root, &skills_config);
        Self::new_inner_canonical(
            root,
            ToolRuntimeConfig {
                output: output_config,
                web: web_config,
                shell_sandbox,
                mcp_servers: BTreeMap::new(),
                checkpoints_enabled: false,
            },
            skills,
            crawl_options_from_graph_config(graph_config),
            runtime,
        )
    }

    pub fn new_with_configs_skills_and_mcp(
        root: impl Into<PathBuf>,
        config: ToolRuntimeConfig,
        skills_config: SkillsConfig,
        graph_config: &GraphConfig,
        runtime: ToolRegistryRuntime,
    ) -> Result<Self> {
        let root = root.into();
        let root = canonicalize_workspace_root(&root)
            .map_err(|err| SqueezyError::Tool(format!("invalid workspace root: {err}")))?;
        let skills = SkillCatalog::discover(&root, &skills_config);
        Self::new_inner_canonical(
            root,
            config,
            skills,
            crawl_options_from_graph_config(graph_config),
            runtime,
        )
    }

    fn new_inner(
        root: impl Into<PathBuf>,
        output_config: ToolOutputConfig,
        web_config: WebToolConfig,
        shell_sandbox: ShellSandboxConfig,
        skills: SkillCatalog,
        crawl_options: CrawlOptions,
        runtime: ToolRegistryRuntime,
    ) -> Result<Self> {
        let root = root.into();
        let root = canonicalize_workspace_root(&root)
            .map_err(|err| SqueezyError::Tool(format!("invalid workspace root: {err}")))?;
        Self::new_inner_canonical(
            root,
            ToolRuntimeConfig {
                output: output_config,
                web: web_config,
                shell_sandbox,
                mcp_servers: BTreeMap::new(),
                checkpoints_enabled: false,
            },
            skills,
            crawl_options,
            runtime,
        )
    }

    fn new_inner_canonical(
        root: PathBuf,
        config: ToolRuntimeConfig,
        skills: SkillCatalog,
        crawl_options: CrawlOptions,
        runtime: ToolRegistryRuntime,
    ) -> Result<Self> {
        let ToolRegistryRuntime {
            state_store,
            redactor,
        } = runtime;
        let output_store = ToolOutputStore::new(&root, config.output)?;
        let http = Arc::new(ReqwestWebHttpClient::new()?);
        // Compile the policy once up front. Invalid user globs surface as a
        // `SqueezyError::Config` here instead of silently disabling the
        // policy on every hot-path call.
        let compiled_policy = Arc::new(crawl_options.policy.compile()?);
        let graph = GraphManager::open_with_store(
            &root,
            Default::default(),
            crawl_options.clone(),
            state_store.clone(),
        )
        .ok();
        let vcs = GitVcs::open(&root)?;
        let shell_audit = ShellAuditStore::new(&root);
        let checkpoints = if config.checkpoints_enabled {
            Some(Arc::new(CheckpointStore::open(&root)?))
        } else {
            None
        };
        let shell_sandbox = normalize_shell_sandbox_paths(config.shell_sandbox);
        Ok(Self {
            root: Arc::new(root),
            output_store: Arc::new(output_store),
            web_config: Arc::new(config.web.normalized()),
            http,
            graph: Arc::new(StdMutex::new(graph)),
            vcs: Arc::new(vcs),
            state_store: state_store.clone(),
            checkpoints,
            diff_cache: Arc::new(StdMutex::new(DiffSnapshotCache::default())),
            skills: Arc::new(skills),
            redactor,
            crawl_options: Arc::new(crawl_options),
            compiled_policy,
            shell_sandbox: Arc::new(shell_sandbox),
            shell_sandbox_health: Arc::new(ShellSandboxHealth::default()),
            shell_audit: Arc::new(shell_audit),
            shell_workdir_locks: Arc::new(StdMutex::new(HashMap::new())),
            shell_inflight: Arc::new(Semaphore::new(MAX_INFLIGHT_SHELLS)),
            mcp: Arc::new(McpClientRegistry::new_with_store(
                config.mcp_servers,
                state_store.clone(),
            )),
            cached_specs: Arc::new(StdMutex::new(None)),
            patch_plans: Arc::new(StdMutex::new(HashMap::new())),
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
        let root = canonicalize_workspace_root(&root)
            .map_err(|err| SqueezyError::Tool(format!("invalid workspace root: {err}")))?;
        let output_store = ToolOutputStore::new(&root, output_config)?;
        let crawl_options = CrawlOptions::default();
        let compiled_policy = Arc::new(crawl_options.policy.compile()?);
        let graph =
            GraphManager::open_with_crawl_options(&root, Default::default(), crawl_options.clone())
                .ok();
        let vcs = GitVcs::open(&root)?;
        let shell_audit = ShellAuditStore::new(&root);
        Ok(Self {
            root: Arc::new(root),
            output_store: Arc::new(output_store),
            web_config: Arc::new(web_config.normalized()),
            http,
            graph: Arc::new(StdMutex::new(graph)),
            vcs: Arc::new(vcs),
            state_store: None,
            checkpoints: None,
            diff_cache: Arc::new(StdMutex::new(DiffSnapshotCache::default())),
            skills: Arc::new(SkillCatalog::empty()),
            redactor: Arc::new(Redactor::default()),
            crawl_options: Arc::new(crawl_options),
            compiled_policy,
            shell_sandbox: Arc::new(ShellSandboxConfig::default()),
            shell_sandbox_health: Arc::new(ShellSandboxHealth::default()),
            shell_audit: Arc::new(shell_audit),
            shell_workdir_locks: Arc::new(StdMutex::new(HashMap::new())),
            shell_inflight: Arc::new(Semaphore::new(MAX_INFLIGHT_SHELLS)),
            mcp: Arc::new(McpClientRegistry::new(BTreeMap::new())),
            cached_specs: Arc::new(StdMutex::new(None)),
            patch_plans: Arc::new(StdMutex::new(HashMap::new())),
        })
    }

    pub(crate) fn diff_snapshot(&self, mode: DiffMode, options: DiffOptions) -> DiffSnapshot {
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

    pub(crate) fn invalidate_diff_cache(&self) {
        if let Ok(mut cache) = self.diff_cache.lock() {
            cache.entries.clear();
        }
    }

    pub(crate) async fn prepare_shell_sandbox(
        &self,
        command: &str,
        analysis: &ShellPermissionAnalysis,
    ) -> std::result::Result<ShellSandboxPlan, String> {
        match self.shell_sandbox.mode {
            ShellSandboxMode::Off => Ok(ShellSandboxPlan::direct(
                command,
                ShellSandboxMode::Off,
                &self.shell_sandbox,
            )),
            ShellSandboxMode::External => {
                Ok(ShellSandboxPlan::external(command, &self.shell_sandbox))
            }
            ShellSandboxMode::BestEffort | ShellSandboxMode::Required => {
                let plan =
                    prepare_shell_sandbox_plan(command, analysis, &self.root, &self.shell_sandbox)?;
                apply_shell_sandbox_backend_health(
                    command,
                    &self.shell_sandbox,
                    &self.shell_sandbox_health,
                    plan,
                    shell_sandbox_backend_probe_failure,
                )
                .await
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn audit_shell(
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

    pub(crate) async fn shell_execution_guard(
        &self,
        workdir: &Path,
    ) -> std::io::Result<ShellExecutionGuard> {
        let permit = self
            .shell_inflight
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| std::io::Error::other("shell execution limiter is closed"))?;
        let key = fs::canonicalize(workdir).unwrap_or_else(|_| workdir.to_path_buf());
        let lock = {
            let mut locks = self
                .shell_workdir_locks
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            locks
                .entry(key)
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let workdir = lock.lock_owned().await;
        Ok(ShellExecutionGuard {
            _permit: permit,
            _workdir: workdir,
        })
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

    fn read_slice_targets_ignored_policy(&self, arguments: &Value) -> bool {
        let Ok(args) = serde_json::from_value::<ReadSliceArgs>(arguments.clone()) else {
            return false;
        };
        let Some(path) = args.path else {
            return false;
        };
        let normalized = path.replace('\\', "/");
        self.compiled_policy
            .path_reason(&normalized, false)
            .is_some()
    }

    pub(crate) fn policy_exclusion_for_file(
        &self,
        path: &Path,
        rel: &Path,
        prefix: Option<&[u8]>,
    ) -> Option<ExclusionReason> {
        let size_bytes = file_len(path).ok()?;
        self.compiled_policy.file_reason(
            &workspace_path(rel),
            size_bytes,
            self.crawl_options.max_file_bytes,
            prefix,
        )
    }

    /// Return the advertised tool list. The result is cached behind an
    /// `Arc<Vec<ToolSpec>>` and re-used across turns; the cache is
    /// invalidated when [`refresh_mcp_tools`] changes the external tool set.
    pub fn specs(&self) -> Arc<Vec<ToolSpec>> {
        if let Ok(mut slot) = self.cached_specs.lock() {
            if let Some(cached) = slot.as_ref() {
                return Arc::clone(cached);
            }
            let built = Arc::new(self.build_specs());
            *slot = Some(Arc::clone(&built));
            return built;
        }
        // Lock poisoned — recover by rebuilding without caching.
        Arc::new(self.build_specs())
    }

    fn build_specs(&self) -> Vec<ToolSpec> {
        let mut specs = vec![
            apply_patch_spec(),
            decl_search_spec(),
            definition_search_spec(),
            diff_context_spec(),
            downstream_flow_spec(),
            glob_spec(),
            grep_spec(),
            hierarchy_spec(),
            plan_patch_spec(),
            read_file_spec(),
            read_slice_spec(),
            read_tool_output_spec(),
            reference_search_spec(),
            refresh_compiler_facts_spec(),
            repo_map_spec(),
            write_file_spec(),
            symbol_context_spec(),
            upstream_flow_spec(),
            verify_spec(),
            shell_spec(),
            webfetch_spec(),
            websearch_spec(),
            list_skills_spec(),
            load_skill_spec(),
            notes_remember_spec(),
            notes_recall_spec(),
            observations_spec(),
        ];
        if !self.mcp.has_no_enabled_servers() {
            specs.extend([
                mcp_list_resources_spec(),
                mcp_list_resource_templates_spec(),
                mcp_read_resource_spec(),
            ]);
        }
        if self.checkpoints.is_some() {
            specs.extend([
                checkpoint_list_spec(),
                checkpoint_revert_spec(),
                checkpoint_show_spec(),
                checkpoint_undo_spec(),
            ]);
        }
        // First-party specs are statically defined inline above. Funnel them
        // through the compaction pipeline so the budget contract holds
        // uniformly regardless of how a spec was built.
        for spec in specs.iter_mut() {
            compact_tool_parameters(&mut spec.parameters);
        }
        // `mcp_tool_spec` already compacts at construction; append after the
        // first-party loop to avoid double work.
        specs.extend(self.mcp.tools().into_iter().map(mcp_tool_spec));
        // Partition first-party before MCP, alphabetic within each group. The
        // contiguous first-party prefix lets the Anthropic adapter place its
        // tools-array `cache_control` breakpoint on the last first-party tool
        // (clear-code's `assembleToolPool` invariant in `src/tools.ts:345-367`),
        // so a mid-session MCP `tools/list` refresh churns only bytes after the
        // breakpoint instead of invalidating the cached prefix for every turn.
        specs.sort_by(|left, right| {
            let left_mcp = left.name.starts_with("mcp__");
            let right_mcp = right.name.starts_with("mcp__");
            left_mcp
                .cmp(&right_mcp)
                .then_with(|| left.name.cmp(&right.name))
        });
        specs
    }

    fn invalidate_cached_specs(&self) {
        if let Ok(mut slot) = self.cached_specs.lock() {
            *slot = None;
        }
    }

    pub async fn refresh_mcp_tools(&self, cancel: CancellationToken) -> McpRefreshOutcome {
        let outcome = self.mcp.refresh_tools(cancel).await;
        // Drop any cached `specs()` so the next call sees the refreshed MCP
        // tool set.
        self.invalidate_cached_specs();
        outcome
    }

    pub fn mcp_status_snapshot(&self) -> McpStatusSnapshot {
        self.mcp.status_snapshot()
    }

    pub fn set_mcp_elicitation_handler(&self, handler: Option<McpElicitationHandler>) {
        self.mcp.set_elicitation_handler(handler);
    }

    /// Plumb the host's MCP approval policy into the MCP client so it can
    /// short-circuit elicitations the user has globally denied and avoid
    /// silently auto-accepting empty-form prompts when the user expects to
    /// be asked first.
    pub fn set_mcp_elicitation_policy(&self, policy: squeezy_core::PermissionMode) {
        self.mcp.set_elicitation_policy(policy);
    }

    fn mcp_tool(&self, name: &str) -> Option<ExternalMcpTool> {
        self.mcp.tool(name)
    }

    pub fn permission_scope(&self, call: &ToolCall) -> PermissionScope {
        if self.mcp_tool(&call.name).is_some() {
            return PermissionScope::Mcp;
        }
        match call.name.as_str() {
            "apply_patch" | "checkpoint_undo" | "checkpoint_revert" => PermissionScope::Edit,
            "write_file" => PermissionScope::Edit,
            "shell" | "verify" | "refresh_compiler_facts" => PermissionScope::Shell,
            "webfetch" | "websearch" => PermissionScope::Web,
            "mcp_read_resource" => PermissionScope::Mcp,
            "mcp_list_resources" | "mcp_list_resource_templates" => PermissionScope::Read,
            "glob" if tool_include_ignored(&call.arguments) => PermissionScope::IgnoredSearch,
            "grep" if grep_include_ignored(&call.arguments) => PermissionScope::IgnoredSearch,
            "read_file" if self.read_file_targets_ignored_policy(&call.arguments) => {
                PermissionScope::IgnoredSearch
            }
            "read_slice" if self.read_slice_targets_ignored_policy(&call.arguments) => {
                PermissionScope::IgnoredSearch
            }
            "checkpoint_list" | "checkpoint_show" | "decl_search" | "definition_search"
            | "diff_context" | "downstream_flow" | "glob" | "grep" | "hierarchy" | "plan_patch"
            | "read_file" | "read_slice" | "read_tool_output" | "reference_search" | "repo_map"
            | "symbol_context" | "upstream_flow" | "list_skills" | "load_skill"
            | "observations" => PermissionScope::Read,
            _ => PermissionScope::Read,
        }
    }

    pub fn permission_request(&self, call: &ToolCall) -> PermissionRequest {
        let mut metadata = BTreeMap::new();
        let mut suggested_rules = Vec::new();
        let summary = self.describe_call(call);
        if let Some(tool) = self.mcp_tool(&call.name) {
            metadata.insert("server".to_string(), tool.server.clone());
            metadata.insert("tool".to_string(), tool.raw_name.clone());
            metadata.insert("transport".to_string(), tool.transport.as_str().to_string());
            metadata.insert(
                "target".to_string(),
                format!("{}/{}", tool.server, tool.raw_name),
            );
            metadata.insert(
                "arguments".to_string(),
                truncate_text(&call.arguments.to_string(), 500),
            );
            suggested_rules.push(PermissionRule::new(
                "mcp",
                format!("{}/{}", tool.server, tool.raw_name),
                PermissionMode::Allow,
                PermissionRuleSource::Session,
                Some("approved MCP server tool".to_string()),
            ));
            return PermissionRequest {
                call_id: call.call_id.clone(),
                tool_name: call.name.clone(),
                capability: PermissionCapability::Mcp,
                target: format!("{}/{}", tool.server, tool.raw_name),
                risk: PermissionRisk::Medium,
                summary,
                metadata,
                suggested_rules,
            };
        }
        let (capability, target, risk) = match call.name.as_str() {
            "apply_patch" => {
                let args = serde_json::from_value::<ApplyPatchArgs>(call.arguments.clone()).ok();
                let paths = args
                    .as_ref()
                    .map(|args| {
                        args.patches
                            .iter()
                            .map(|patch| patch.path.as_str())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                let target = if paths.len() == 1 {
                    format!("path:{}", paths[0])
                } else {
                    "workspace:patches".to_string()
                };
                metadata.insert(
                    "paths".to_string(),
                    if paths.is_empty() {
                        "*".to_string()
                    } else {
                        paths.join(", ")
                    },
                );
                if let Some(diff) = args.as_ref().and_then(render_apply_patch_diff) {
                    metadata.insert("unified_diff".to_string(), diff);
                }
                for path in paths.iter().take(5) {
                    suggested_rules.push(PermissionRule::new(
                        "edit",
                        format!("path:{path}"),
                        PermissionMode::Allow,
                        PermissionRuleSource::Session,
                        Some("approved patch path".to_string()),
                    ));
                }
                (PermissionCapability::Edit, target, PermissionRisk::High)
            }
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
                if let Some(diff) = args
                    .as_ref()
                    .and_then(|args| render_write_file_diff(&args.path, &args.content))
                {
                    metadata.insert("unified_diff".to_string(), diff);
                }
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
                    "tty".to_string(),
                    args.as_ref()
                        .map(|args| args.tty)
                        .unwrap_or(false)
                        .to_string(),
                );
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
                metadata.insert(
                    "sandbox_read_roots".to_string(),
                    path_list_metadata(&self.shell_sandbox.read_roots),
                );
                metadata.insert(
                    "sandbox_write_roots".to_string(),
                    path_list_metadata(&self.shell_sandbox.write_roots),
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
            "refresh_compiler_facts" => {
                let args =
                    serde_json::from_value::<RefreshCompilerFactsArgs>(call.arguments.clone()).ok();
                let diagnostics = args
                    .as_ref()
                    .and_then(|args| args.diagnostics)
                    .unwrap_or(false);
                metadata.insert("diagnostics".to_string(), diagnostics.to_string());
                metadata.insert(
                    "commands".to_string(),
                    if diagnostics {
                        "cargo metadata --format-version=1 --no-deps; cargo check --message-format=json"
                            .to_string()
                    } else {
                        "cargo metadata --format-version=1 --no-deps".to_string()
                    },
                );
                let target = if diagnostics {
                    "cargo facts+check:*"
                } else {
                    "cargo facts:*"
                }
                .to_string();
                suggested_rules.push(PermissionRule::new(
                    "compiler",
                    target.clone(),
                    PermissionMode::Allow,
                    PermissionRuleSource::Session,
                    Some("approved compiler fact refresh".to_string()),
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
            "mcp_read_resource" => {
                let server = call
                    .arguments
                    .get("server")
                    .and_then(Value::as_str)
                    .unwrap_or("*")
                    .to_string();
                let uri = call
                    .arguments
                    .get("uri")
                    .and_then(Value::as_str)
                    .unwrap_or("*")
                    .to_string();
                metadata.insert("server".to_string(), server.clone());
                metadata.insert("uri".to_string(), uri.clone());
                suggested_rules.push(PermissionRule::new(
                    "mcp",
                    format!("{server}/resource"),
                    PermissionMode::Allow,
                    PermissionRuleSource::Session,
                    Some("approved MCP resource read".to_string()),
                ));
                (
                    PermissionCapability::Mcp,
                    format!("{server}/resource"),
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
            "decl_search" | "definition_search" | "reference_search" => (
                PermissionCapability::Search,
                "workspace:*".to_string(),
                PermissionRisk::Low,
            ),
            "grep" | "glob" => (
                PermissionCapability::Search,
                "workspace:*".to_string(),
                PermissionRisk::Low,
            ),
            "checkpoint_list"
            | "checkpoint_show"
            | "diff_context"
            | "downstream_flow"
            | "hierarchy"
            | "plan_patch"
            | "read_file"
            | "read_slice"
            | "read_tool_output"
            | "repo_map"
            | "symbol_context"
            | "upstream_flow"
            | "list_skills"
            | "load_skill"
            | "mcp_list_resources"
            | "mcp_list_resource_templates" => (
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

    /// Per-tool preview lines for the approval dialog. See
    /// [`preview::CatalogPreview`] for the dispatch table.
    pub fn preview_for(
        &self,
        call: &ToolCall,
        request: &PermissionRequest,
    ) -> Vec<preview::PreviewLine> {
        use preview::PermissionPreview;
        preview::CatalogPreview.preview_lines(request, call, self.root.as_ref())
    }

    pub fn is_parallel_safe(&self, call: &ToolCall) -> bool {
        matches!(
            call.name.as_str(),
            "checkpoint_list"
                | "checkpoint_show"
                | "decl_search"
                | "definition_search"
                | "diff_context"
                | "downstream_flow"
                | "glob"
                | "grep"
                | "hierarchy"
                | "plan_patch"
                | "read_file"
                | "read_slice"
                | "read_tool_output"
                | "reference_search"
                | "repo_map"
                | "symbol_context"
                | "upstream_flow"
                | "webfetch"
                | "websearch"
                | "mcp_list_resources"
                | "mcp_list_resource_templates"
                | "mcp_read_resource"
                | "list_skills"
                | "load_skill"
        )
    }

    pub fn describe_call(&self, call: &ToolCall) -> String {
        if let Some(tool) = self.mcp_tool(&call.name) {
            return format!("mcp server={:?} tool={:?}", tool.server, tool.raw_name);
        }
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
            "apply_patch" => {
                let args = serde_json::from_value::<ApplyPatchArgs>(call.arguments.clone()).ok();
                let paths = args
                    .as_ref()
                    .map(|args| {
                        args.patches
                            .iter()
                            .map(|patch| patch.path.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .filter(|paths| !paths.is_empty())
                    .unwrap_or_else(|| "?".to_string());
                format!("apply_patch paths={paths:?}")
            }
            "repo_map" => "repo_map".to_string(),
            "decl_search" | "definition_search" | "reference_search" => {
                let query = call
                    .arguments
                    .get("query")
                    .or_else(|| call.arguments.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                format!("{} query={query:?}", call.name)
            }
            "upstream_flow" | "downstream_flow" | "hierarchy" => {
                let symbol_id = call
                    .arguments
                    .get("symbol_id")
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                format!("{} symbol_id={symbol_id:?}", call.name)
            }
            "read_slice" => {
                let path = call
                    .arguments
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                format!("read_slice path={path:?}")
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
            "plan_patch" => {
                let args = serde_json::from_value::<PlanPatchArgs>(call.arguments.clone()).ok();
                let objective = args
                    .as_ref()
                    .map(|args| args.objective.as_str())
                    .unwrap_or("?");
                format!("plan_patch objective={objective:?}")
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
            "refresh_compiler_facts" => {
                let args =
                    serde_json::from_value::<RefreshCompilerFactsArgs>(call.arguments.clone()).ok();
                let diagnostics = args
                    .as_ref()
                    .and_then(|args| args.diagnostics)
                    .unwrap_or(false);
                format!("refresh_compiler_facts diagnostics={diagnostics}")
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
            "mcp_list_resources" | "mcp_list_resource_templates" => {
                let args =
                    serde_json::from_value::<McpListResourcesArgs>(call.arguments.clone()).ok();
                let server = args
                    .as_ref()
                    .map(|args| args.server.as_str())
                    .unwrap_or("?");
                format!("{} server={server:?}", call.name)
            }
            "mcp_read_resource" => {
                let args =
                    serde_json::from_value::<McpReadResourceArgs>(call.arguments.clone()).ok();
                let server = args
                    .as_ref()
                    .map(|args| args.server.as_str())
                    .unwrap_or("?");
                let uri = args.as_ref().map(|args| args.uri.as_str()).unwrap_or("?");
                format!("mcp_read_resource server={server:?} uri={uri:?}")
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
        self.skills.render_active_skills(skills)
    }

    pub fn skills_preamble(&self) -> Option<SkillPreambleRender> {
        self.skills.render_preamble()
    }

    pub fn load_skill_for_instructions(&self, name: &str) -> Result<LoadedSkill> {
        self.skills.load(name)
    }

    pub fn ambiguous_skill_names(&self) -> Vec<String> {
        self.skills.ambiguous_names().iter().cloned().collect()
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
        self.execute_for_group_with_options(call, cancel, group_id, ToolExecutionOptions::default())
            .await
    }

    pub async fn execute_for_group_with_options(
        &self,
        call: ToolCall,
        cancel: CancellationToken,
        group_id: String,
        options: ToolExecutionOptions,
    ) -> ToolResult {
        if cancel.is_cancelled() {
            return ToolResult::cancelled(&call);
        }

        let result = if self.mcp_tool(&call.name).is_some() {
            self.execute_mcp_tool(&call, cancel).await
        } else {
            match call.name.as_str() {
                "apply_patch" => self.execute_apply_patch(&call, &group_id).await,
                "checkpoint_list" => self.execute_checkpoint_list(&call).await,
                "checkpoint_show" => self.execute_checkpoint_show(&call).await,
                "checkpoint_undo" => self.execute_checkpoint_undo(&call).await,
                "checkpoint_revert" => self.execute_checkpoint_revert(&call).await,
                "repo_map" | "decl_search" | "definition_search" | "reference_search"
                | "upstream_flow" | "downstream_flow" | "hierarchy" | "read_slice"
                | "symbol_context" => self.execute_graph_tool(&call).await,
                "diff_context" => self.execute_diff_context(&call).await,
                "plan_patch" => self.execute_plan_patch(&call).await,
                "glob" => self.execute_glob(&call, cancel).await,
                "grep" => self.execute_grep(&call, cancel).await,
                "read_file" => self.execute_read_file(&call).await,
                "read_tool_output" => self.execute_read_tool_output(&call).await,
                "refresh_compiler_facts" => {
                    self.execute_refresh_compiler_facts(&call, cancel, &group_id)
                        .await
                }
                "verify" => self.execute_verify(&call, cancel, &group_id).await,
                "write_file" => self.execute_write_file(&call, &group_id).await,
                "shell" => {
                    self.execute_shell(&call, cancel, &group_id, options.shell_ask_approver.clone())
                        .await
                }
                "webfetch" => self.execute_webfetch(&call, cancel).await,
                "websearch" => self.execute_websearch(&call, cancel).await,
                "mcp_list_resources" => self.execute_mcp_list_resources(&call, cancel).await,
                "mcp_list_resource_templates" => {
                    self.execute_mcp_list_resource_templates(&call, cancel)
                        .await
                }
                "mcp_read_resource" => self.execute_mcp_read_resource(&call, cancel).await,
                "list_skills" => self.execute_list_skills(&call).await,
                "load_skill" => self.execute_load_skill(&call).await,
                "notes_remember" => self.execute_notes_remember(&call).await,
                "notes_recall" => self.execute_notes_recall(&call).await,
                "observations" => self.execute_observations(&call).await,
                _ => make_result(
                    &call,
                    ToolStatus::Error,
                    json!({ "error": format!("unknown tool: {}", call.name) }),
                    ToolCostHint::default(),
                    None,
                ),
            }
        };

        if call.name == "read_tool_output" {
            result
        } else {
            self.finalize_result(result)
        }
    }

    async fn execute_mcp_tool(&self, call: &ToolCall, cancel: CancellationToken) -> ToolResult {
        match self
            .mcp
            .call_tool(&call.name, call.arguments.clone(), cancel)
            .await
        {
            Ok(result) => {
                let status = if result.is_error {
                    ToolStatus::Error
                } else {
                    ToolStatus::Success
                };
                make_result(
                    call,
                    status,
                    json!({
                        "source": "mcp",
                        "server": result.server,
                        "tool": result.raw_name,
                        "model_tool": result.model_name,
                        "is_error": result.is_error,
                        "result": result.content,
                    }),
                    ToolCostHint::default(),
                    None,
                )
            }
            Err(error) => tool_error(call, error),
        }
    }

    async fn execute_mcp_list_resources(
        &self,
        call: &ToolCall,
        cancel: CancellationToken,
    ) -> ToolResult {
        let args = match serde_json::from_value::<McpListResourcesArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        match self
            .mcp
            .list_resources(&args.server, args.cursor, cancel)
            .await
        {
            Ok(result) => make_result(
                call,
                ToolStatus::Success,
                json!({
                    "source": "mcp",
                    "server": args.server,
                    "resources": result,
                    "untrusted": true,
                }),
                ToolCostHint::default(),
                None,
            ),
            Err(error) => tool_error(call, error),
        }
    }

    async fn execute_mcp_list_resource_templates(
        &self,
        call: &ToolCall,
        cancel: CancellationToken,
    ) -> ToolResult {
        let args = match serde_json::from_value::<McpListResourcesArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        match self
            .mcp
            .list_resource_templates(&args.server, args.cursor, cancel)
            .await
        {
            Ok(result) => make_result(
                call,
                ToolStatus::Success,
                json!({
                    "source": "mcp",
                    "server": args.server,
                    "resource_templates": result,
                    "untrusted": true,
                }),
                ToolCostHint::default(),
                None,
            ),
            Err(error) => tool_error(call, error),
        }
    }

    async fn execute_mcp_read_resource(
        &self,
        call: &ToolCall,
        cancel: CancellationToken,
    ) -> ToolResult {
        let args = match serde_json::from_value::<McpReadResourceArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        match self
            .mcp
            .read_resource(&args.server, &args.uri, cancel)
            .await
        {
            Ok(result) => make_result(
                call,
                ToolStatus::Success,
                json!({
                    "source": "mcp",
                    "server": args.server,
                    "uri": args.uri,
                    "result": result,
                    "untrusted": true,
                }),
                ToolCostHint::default(),
                None,
            ),
            Err(error) => tool_error(call, error),
        }
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

    async fn execute_notes_remember(&self, call: &ToolCall) -> ToolResult {
        let args = match serde_json::from_value::<NotesRememberArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        let Some(store) = self.state_store.as_deref() else {
            return make_result(
                call,
                ToolStatus::Error,
                json!({ "error": "notes_remember requires the persistent store; no store handle available" }),
                ToolCostHint::default(),
                None,
            );
        };
        let kind = match parse_observation_kind(&args.kind) {
            Some(kind) => kind,
            None => {
                return tool_error(
                    call,
                    format!(
                        "notes_remember: unknown kind {:?}; expected one of preference, decision, convention, dead_end, note",
                        args.kind
                    ),
                );
            }
        };
        let observation = Observation {
            id: String::new(),
            kind,
            text: args.text,
            tags: args.tags.unwrap_or_default(),
            source: args.source.unwrap_or_default(),
            created_unix_millis: 0,
            updated_unix_millis: 0,
        };
        match store.put_observation(observation) {
            Ok(stored) => make_result(
                call,
                ToolStatus::Success,
                json!({
                    "id": stored.id,
                    "kind": format!("{:?}", stored.kind).to_ascii_lowercase(),
                    "tags": stored.tags,
                    "created_unix_millis": stored.created_unix_millis,
                }),
                ToolCostHint::default(),
                None,
            ),
            Err(err) => tool_error(call, format!("notes_remember failed: {err}")),
        }
    }

    async fn execute_notes_recall(&self, call: &ToolCall) -> ToolResult {
        let args = match serde_json::from_value::<NotesRecallArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        let Some(store) = self.state_store.as_deref() else {
            return make_result(
                call,
                ToolStatus::Error,
                json!({ "error": "notes_recall requires the persistent store; no store handle available" }),
                ToolCostHint::default(),
                None,
            );
        };
        let limit = args.limit.unwrap_or(5).clamp(1, 20) as usize;
        let query = args.query.trim();
        let lookup = if query.is_empty() {
            store.list_recent_observations(limit)
        } else {
            store.search_observations(query, limit)
        };
        match lookup {
            Ok(matches) => {
                let items: Vec<Value> = matches
                    .into_iter()
                    .map(|obs| {
                        json!({
                            "id": obs.id,
                            "kind": format!("{:?}", obs.kind).to_ascii_lowercase(),
                            "text": obs.text,
                            "tags": obs.tags,
                            "source": obs.source,
                            "created_unix_millis": obs.created_unix_millis,
                            "updated_unix_millis": obs.updated_unix_millis,
                        })
                    })
                    .collect();
                make_result(
                    call,
                    ToolStatus::Success,
                    json!({ "matches": items }),
                    ToolCostHint::default(),
                    None,
                )
            }
            Err(err) => tool_error(call, format!("notes_recall failed: {err}")),
        }
    }

    async fn execute_observations(&self, call: &ToolCall) -> ToolResult {
        let args = match serde_json::from_value::<ObservationsArgs>(call.arguments.clone()) {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        let Some(store) = self.state_store.as_deref() else {
            return make_result(
                call,
                ToolStatus::Error,
                json!({ "error": "observations requires the persistent store; no store handle available" }),
                ToolCostHint::default(),
                None,
            );
        };
        let limit = args.limit.unwrap_or(10).clamp(1, 50) as usize;
        let query = args.query.as_deref().map(str::trim).unwrap_or("");
        let lookup = if query.is_empty() {
            store.list_recent_observations(limit)
        } else {
            store.search_observations(query, limit)
        };
        match lookup {
            Ok(matches) => {
                let items: Vec<Value> = matches
                    .into_iter()
                    .map(|obs| {
                        json!({
                            "id": obs.id,
                            "timestamp": obs.updated_unix_millis,
                            "kind": format!("{:?}", obs.kind).to_ascii_lowercase(),
                            "summary": obs.text,
                            "tags": obs.tags,
                        })
                    })
                    .collect();
                make_result(
                    call,
                    ToolStatus::Success,
                    json!({ "observations": items }),
                    ToolCostHint::default(),
                    None,
                )
            }
            Err(err) => tool_error(call, format!("observations failed: {err}")),
        }
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

    async fn execute_refresh_compiler_facts(
        &self,
        call: &ToolCall,
        cancel: CancellationToken,
        group_id: &str,
    ) -> ToolResult {
        let args = match serde_json::from_value::<RefreshCompilerFactsArgs>(call.arguments.clone())
        {
            Ok(args) => args,
            Err(err) => return tool_arg_error(call, err),
        };
        let include_diagnostics = args.diagnostics.unwrap_or(false);
        let metadata_command = "cargo metadata --format-version=1 --no-deps";
        let metadata_result = self
            .execute_compiler_fact_command(
                call,
                metadata_command,
                120_000,
                MAX_SHELL_OUTPUT_BYTE_CAP,
                cancel.clone(),
                group_id,
            )
            .await;
        if metadata_result.status != ToolStatus::Success {
            return compiler_fact_command_error(call, "cargo metadata failed", metadata_result);
        }
        if metadata_result
            .content
            .get("truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return make_result(
                call,
                ToolStatus::Error,
                json!({"error": "cargo metadata output was truncated"}),
                metadata_result.cost_hint,
                None,
            );
        }
        let metadata_stdout = shell_stdout(&metadata_result).to_string();

        let diagnostics_result = if include_diagnostics {
            Some(
                self.execute_compiler_fact_command(
                    call,
                    "cargo check --message-format=json",
                    VERIFY_SHELL_TIMEOUT_MS,
                    MAX_SHELL_OUTPUT_BYTE_CAP,
                    cancel.clone(),
                    group_id,
                )
                .await,
            )
        } else {
            None
        };
        if diagnostics_result
            .as_ref()
            .and_then(|result| result.content.get("truncated").and_then(Value::as_bool))
            .unwrap_or(false)
        {
            return make_result(
                call,
                ToolStatus::Error,
                json!({"error": "cargo check output was truncated"}),
                diagnostics_result
                    .as_ref()
                    .map(|result| result.cost_hint.clone())
                    .unwrap_or_default(),
                None,
            );
        }
        let diagnostics_stdout = diagnostics_result.as_ref().map(shell_stdout);

        let cargo_version = self
            .compiler_version("cargo", "cargo --version", cancel.clone(), group_id)
            .await;
        let rustc_version = self
            .compiler_version("rustc", "rustc --version", cancel.clone(), group_id)
            .await;
        let command = if include_diagnostics {
            "cargo metadata --format-version=1 --no-deps; cargo check --message-format=json"
        } else {
            metadata_command
        };
        let provenance = CargoFactProvenance {
            command: command.to_string(),
            cargo_version,
            rustc_version,
            captured_unix_millis: unix_millis(),
        };

        let report = {
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
                return graph_unavailable_result(call);
            };
            if let Err(err) = manager.refresh_before_query() {
                return tool_error(call, err);
            }
            match manager.graph_mut().refresh_cargo_facts_from_json(
                &metadata_stdout,
                diagnostics_stdout,
                provenance,
                &self.root,
            ) {
                Ok(report) => report,
                Err(err) => return tool_error(call, err),
            }
        };

        let diagnostics_exit_code = if report.diagnostics_loaded {
            diagnostics_result
                .as_ref()
                .and_then(|result| result.content.get("exit_code").and_then(Value::as_i64))
        } else {
            None
        };
        let metadata_bytes =
            shell_stdout(&metadata_result).len() + shell_stderr(&metadata_result).len();
        let diagnostics_bytes = diagnostics_result.as_ref().map_or(0, |result| {
            shell_stdout(result).len() + shell_stderr(result).len()
        });
        let output_bytes = metadata_bytes + diagnostics_bytes;
        make_result(
            call,
            ToolStatus::Success,
            json!({
                "tool": "refresh_compiler_facts",
                "metadata_command": metadata_command,
                "diagnostics_command": include_diagnostics.then_some("cargo check --message-format=json"),
                "diagnostics_exit_code": diagnostics_exit_code,
                "diagnostics_loaded": report.diagnostics_loaded,
                "summary": cargo_facts_summary_json(&report.summary),
            }),
            ToolCostHint {
                bytes_read: output_bytes as u64,
                output_bytes: output_bytes as u64,
                matches_returned: report.summary.diagnostics as u64,
                truncated: false,
                ..ToolCostHint::default()
            },
            None,
        )
    }

    async fn execute_compiler_fact_command(
        &self,
        call: &ToolCall,
        command: &str,
        timeout_ms: u64,
        output_byte_cap: usize,
        cancel: CancellationToken,
        group_id: &str,
    ) -> ToolResult {
        let shell_call = ToolCall {
            call_id: call.call_id.clone(),
            name: "shell".to_string(),
            arguments: json!({
                "command": command,
                "description": "refresh cached cargo compiler facts",
                "timeout_ms": timeout_ms,
                "output_byte_cap": output_byte_cap,
                "output_mode": "raw",
            }),
        };
        self.execute_shell_capped(&shell_call, cancel, timeout_ms, group_id, None)
            .await
    }

    async fn compiler_version(
        &self,
        tool: &str,
        command: &str,
        cancel: CancellationToken,
        group_id: &str,
    ) -> Option<String> {
        let call = ToolCall {
            call_id: format!("compiler-version-{tool}"),
            name: "shell".to_string(),
            arguments: json!({
                "command": command,
                "description": "capture compiler fact provenance version",
                "timeout_ms": 10_000,
                "output_byte_cap": 1024,
                "output_mode": "raw",
            }),
        };
        let result = self
            .execute_shell_capped(&call, cancel, 10_000, group_id, None)
            .await;
        (result.status == ToolStatus::Success)
            .then(|| shell_stdout(&result).trim().to_string())
            .filter(|version| !version.is_empty())
    }

    /// Return the most-recent `(content_sha256, call_id)` the model has seen
    /// for `rel_path` via a `read_file` or `read_slice` receipt in the current
    /// session. The mutation tools (`apply_patch`, `write_file`) consult this
    /// to gate edits when the caller omitted `expected_sha256`: if the seen
    /// hash matches the current file hash the model demonstrably has fresh
    /// state, so the write can proceed; if it does not match (or no snapshot
    /// exists) the call short-circuits to `ToolStatus::Stale` with a
    /// "call read_file first" hint. Returns `None` when no store is wired up
    /// or no snapshot has been recorded yet.
    pub(crate) fn latest_seen_sha256_for_path(&self, rel_path: &str) -> Option<(String, String)> {
        let store = self.state_store.as_deref()?;
        let snapshots = store.read_snapshots_for_path(rel_path).ok()?;
        let latest = snapshots
            .into_iter()
            .filter(|snap| {
                matches!(snap.tool_name.as_str(), "read_file" | "read_slice")
                    && snap.content_sha256.is_some()
            })
            .max_by_key(|snap| snap.created_unix_millis)?;
        Some((latest.content_sha256?, latest.call_id))
    }

    /// Server-side fallback for the `expected_sha256` staleness gate. Called
    /// when the model omits `expected_sha256`: if the latest read snapshot
    /// for `rel_path` matches the current on-disk hash, the model
    /// demonstrably has fresh state and the caller may proceed (`Ok(())`).
    /// Otherwise the caller short-circuits to `ToolStatus::Stale` with a
    /// payload that names the snapshot the model last saw vs the live hash
    /// and points the model at `read_file` for recovery.
    #[allow(clippy::result_large_err)]
    fn gate_on_seen_sha256(
        &self,
        call: &ToolCall,
        index: usize,
        rel: &str,
        current_sha256: &str,
        op_label: &str,
    ) -> std::result::Result<(), ToolResult> {
        match self.latest_seen_sha256_for_path(rel) {
            Some((ref seen_sha, _)) if seen_sha.as_str() == current_sha256 => Ok(()),
            Some((seen_sha, last_call_id)) => Err(make_result(
                call,
                ToolStatus::Stale,
                json!({
                    "error": "file changed since the model last saw it; call read_file first",
                    "path": rel,
                    "patch_index": index,
                    "operation": op_label,
                    "current_sha256": current_sha256,
                    "last_seen_sha256": seen_sha,
                    "last_read_call_id": last_call_id,
                }),
                ToolCostHint::default(),
                Some(current_sha256.to_string()),
            )),
            None => Err(make_result(
                call,
                ToolStatus::Stale,
                json!({
                    "error": format!(
                        "expected_sha256 not provided and no read_file/read_slice snapshot found; call read_file first for {op_label}"
                    ),
                    "path": rel,
                    "patch_index": index,
                    "operation": op_label,
                    "current_sha256": current_sha256,
                }),
                ToolCostHint::default(),
                Some(current_sha256.to_string()),
            )),
        }
    }

    /// Validate a single operation and append it to the staged plan. On any
    /// validation failure, the returned `Err` is the final tool result the
    /// caller should return verbatim — no writes have happened yet.
    #[allow(clippy::result_large_err)]
    pub(crate) fn stage_apply_patch_op(
        &self,
        call: &ToolCall,
        index: usize,
        op: &ApplyPatchOperation,
        staged: &mut StagedApply,
        preview_ops: &mut Vec<Value>,
    ) -> std::result::Result<(), ToolResult> {
        match op {
            ApplyPatchOperation::SearchReplace {
                path,
                search,
                replace,
                expected_sha256,
                allow_multiple,
                fallback,
            } => {
                if search.is_empty() {
                    return Err(make_result(
                        call,
                        ToolStatus::Error,
                        json!({
                            "error": "search text must not be empty",
                            "patch_index": index,
                            "path": path,
                        }),
                        ToolCostHint::default(),
                        None,
                    ));
                }
                let abs_path = self.resolve_existing(path).map_err(|err| {
                    make_result(
                        call,
                        ToolStatus::Error,
                        json!({
                            "error": format!("search-replace patches require an existing file: {err}"),
                            "path": path,
                        }),
                        ToolCostHint::default(),
                        None,
                    )
                })?;
                let rel = self
                    .relative(&abs_path)
                    .to_string_lossy()
                    .replace('\\', "/");

                let file_idx = staged
                    .ensure_search_replace(&rel, &abs_path)
                    .map_err(|err| {
                        tool_error(call, format!("failed to read text file {rel}: {err}"))
                    })?;
                let state = &mut staged.files[file_idx];
                let before_sha256 = state.before_sha256.clone();
                match expected_sha256.as_deref() {
                    Some(expected) if expected == before_sha256 => {}
                    Some(_) => {
                        return Err(make_result(
                            call,
                            ToolStatus::Stale,
                            json!({
                                "error": "expected_sha256 does not match current file",
                                "path": rel,
                                "current_sha256": before_sha256,
                            }),
                            ToolCostHint::default(),
                            Some(before_sha256),
                        ));
                    }
                    None => {
                        if let Err(result) = self.gate_on_seen_sha256(
                            call,
                            index,
                            &rel,
                            &before_sha256,
                            "search-replace patches",
                        ) {
                            return Err(result);
                        }
                    }
                }
                let matches = state.current.match_indices(search.as_str()).count();
                if matches == 0 {
                    // Quote-normalize fallback (F14-cc): the byte-exact search
                    // failed, retry after collapsing curly quotes on both
                    // sides. Common case: the file has `don\u{2019}t` but the
                    // model emitted ASCII `don't`. Deterministic and free —
                    // the sha256 staleness gate above stays in place because
                    // we still hash the final file in the commit phase. Only
                    // rescue single-match cases so the multi-match safeguard
                    // below still applies; the unified-diff path remains for
                    // wider drift the model opts into explicitly.
                    if let Some((qstart, qend, qcount)) =
                        find_with_quote_normalization(&state.current, search)
                        && qcount == 1
                    {
                        let original_slice = &state.current[qstart..qend];
                        let replacement = preserve_quote_style(replace, original_slice);
                        let before_len = state.current.len();
                        let mut new_contents = String::with_capacity(
                            state.current.len() - (qend - qstart) + replacement.len(),
                        );
                        new_contents.push_str(&state.current[..qstart]);
                        new_contents.push_str(&replacement);
                        new_contents.push_str(&state.current[qend..]);
                        state.current = new_contents;
                        let after_len = state.current.len();
                        staged.mark_last_op_inexact(Some("quote_normalize"));
                        preview_ops.push(json!({
                            "patch_index": index,
                            "kind": "search_replace",
                            "path": rel,
                            "matches": qcount,
                            "allow_multiple": allow_multiple.unwrap_or(false),
                            "bytes_delta": after_len as i64 - before_len as i64,
                            "fallback": "quote_normalize",
                            "exact": false,
                            "preview": {
                                "search": truncate_text(search, PATCH_SNIPPET_MAX_CHARS),
                                "replace": truncate_text(replace, PATCH_SNIPPET_MAX_CHARS),
                            }
                        }));
                        return Ok(());
                    }
                    // Optional unified-diff fallback (F89): the search body is
                    // a unified diff; preflight against the live worktree, and
                    // if it would apply, materialise the result by reading the
                    // file after a real `git apply --3way`. The sha256 gate
                    // remains in place because we still recompute hashes.
                    if matches!(fallback, Some(SearchReplaceFallback::UnifiedDiff)) {
                        match self.vcs.apply_unified_diff(search) {
                            Ok(outcome) if outcome.applied => {
                                // Re-read after git apply mutated the file in
                                // place; treat as authoritative new content.
                                let new_contents = match fs::read_to_string(&abs_path) {
                                    Ok(text) => text,
                                    Err(err) => {
                                        return Err(tool_error(
                                            call,
                                            format!(
                                                "unified-diff fallback applied but file unreadable: {err}"
                                            ),
                                        ));
                                    }
                                };
                                state.current = new_contents;
                                staged.mark_last_op_inexact(Some("unified_diff"));
                                preview_ops.push(json!({
                                    "patch_index": index,
                                    "kind": "search_replace",
                                    "path": rel,
                                    "fallback": "unified_diff",
                                    "applied_via": "git_apply_3way",
                                    "exact": false,
                                }));
                                return Ok(());
                            }
                            Ok(outcome) => {
                                return Err(make_result(
                                    call,
                                    ToolStatus::Stale,
                                    json!({
                                        "error": "unified-diff fallback could not apply cleanly",
                                        "path": rel,
                                        "patch_index": index,
                                        "conflicted_paths": outcome.conflicted_paths,
                                        "skipped_paths": outcome.skipped_paths,
                                        "stderr": outcome.stderr,
                                    }),
                                    ToolCostHint::default(),
                                    Some(before_sha256),
                                ));
                            }
                            Err(err) => {
                                return Err(make_result(
                                    call,
                                    ToolStatus::Stale,
                                    json!({
                                        "error": format!(
                                            "unified-diff fallback invocation failed: {err}"
                                        ),
                                        "path": rel,
                                        "patch_index": index,
                                    }),
                                    ToolCostHint::default(),
                                    Some(before_sha256),
                                ));
                            }
                        }
                    }
                    return Err(make_result(
                        call,
                        ToolStatus::Stale,
                        json!({
                            "error": "search text was not found",
                            "path": rel,
                            "patch_index": index,
                        }),
                        ToolCostHint::default(),
                        Some(before_sha256),
                    ));
                }
                let allow_multi = allow_multiple.unwrap_or(false);
                if matches > 1 && !allow_multi {
                    return Err(make_result(
                        call,
                        ToolStatus::Stale,
                        json!({
                            "error": "search text matched more than once; narrow the search text or set allow_multiple=true to replace all matches",
                            "path": rel,
                            "patch_index": index,
                            "matches": matches,
                            "match_contexts": patch_match_contexts(&state.current, search, 5),
                        }),
                        ToolCostHint::default(),
                        Some(before_sha256),
                    ));
                }
                let before_len = state.current.len();
                state.current = if allow_multi {
                    state.current.replace(search.as_str(), replace.as_str())
                } else {
                    state.current.replacen(search.as_str(), replace.as_str(), 1)
                };
                let after_len = state.current.len();
                preview_ops.push(json!({
                    "patch_index": index,
                    "kind": "search_replace",
                    "path": rel,
                    "matches": matches,
                    "allow_multiple": allow_multi,
                    "bytes_delta": after_len as i64 - before_len as i64,
                    "preview": {
                        "search": truncate_text(search, PATCH_SNIPPET_MAX_CHARS),
                        "replace": truncate_text(replace, PATCH_SNIPPET_MAX_CHARS),
                    }
                }));
                Ok(())
            }
            ApplyPatchOperation::CreateFile {
                path,
                contents,
                expected_absent,
            } => {
                let abs_path = match self.resolve_for_write(path) {
                    Ok(p) => p,
                    Err(err) => {
                        return Err(make_result(
                            call,
                            ToolStatus::Error,
                            json!({
                                "error": format!("invalid create_file path: {err}"),
                                "path": path,
                            }),
                            ToolCostHint::default(),
                            None,
                        ));
                    }
                };
                let rel = self
                    .relative(&abs_path)
                    .to_string_lossy()
                    .replace('\\', "/");
                let expect_absent = expected_absent.unwrap_or(true);
                if expect_absent && abs_path.exists() {
                    return Err(make_result(
                        call,
                        ToolStatus::Stale,
                        json!({
                            "error": "create_file target already exists",
                            "path": rel,
                        }),
                        ToolCostHint::default(),
                        None,
                    ));
                }
                staged.push_create(rel.clone(), abs_path, contents.clone());
                preview_ops.push(json!({
                    "patch_index": index,
                    "kind": "create_file",
                    "path": rel,
                    "bytes_after": contents.len(),
                }));
                Ok(())
            }
            ApplyPatchOperation::DeleteFile {
                path,
                expected_sha256,
            } => {
                let abs_path = match self.resolve_existing(path) {
                    Ok(p) => p,
                    Err(err) => {
                        return Err(make_result(
                            call,
                            ToolStatus::Error,
                            json!({
                                "error": format!("delete_file target missing: {err}"),
                                "path": path,
                            }),
                            ToolCostHint::default(),
                            None,
                        ));
                    }
                };
                let rel = self
                    .relative(&abs_path)
                    .to_string_lossy()
                    .replace('\\', "/");
                let existing = match fs::read(&abs_path) {
                    Ok(bytes) => bytes,
                    Err(err) => {
                        return Err(tool_error(
                            call,
                            format!("failed to read delete target {rel}: {err}"),
                        ));
                    }
                };
                let current_sha256 = sha256_hex(&existing);
                match expected_sha256.as_deref() {
                    Some(expected) if expected == current_sha256 => {}
                    Some(_) => {
                        return Err(make_result(
                            call,
                            ToolStatus::Stale,
                            json!({
                                "error": "expected_sha256 does not match current file",
                                "path": rel,
                                "current_sha256": current_sha256,
                            }),
                            ToolCostHint::default(),
                            Some(current_sha256),
                        ));
                    }
                    None => {
                        if let Err(result) = self.gate_on_seen_sha256(
                            call,
                            index,
                            &rel,
                            &current_sha256,
                            "delete_file",
                        ) {
                            return Err(result);
                        }
                    }
                }
                staged.push_delete(rel.clone(), abs_path, current_sha256, existing.len());
                preview_ops.push(json!({
                    "patch_index": index,
                    "kind": "delete_file",
                    "path": rel,
                }));
                Ok(())
            }
            ApplyPatchOperation::MoveFile {
                from,
                to,
                expected_sha256,
                post_replace,
            } => {
                let abs_from = match self.resolve_existing(from) {
                    Ok(p) => p,
                    Err(err) => {
                        return Err(make_result(
                            call,
                            ToolStatus::Error,
                            json!({
                                "error": format!("move_file source missing: {err}"),
                                "path": from,
                            }),
                            ToolCostHint::default(),
                            None,
                        ));
                    }
                };
                let abs_to = match self.resolve_for_write(to) {
                    Ok(p) => p,
                    Err(err) => {
                        return Err(make_result(
                            call,
                            ToolStatus::Error,
                            json!({
                                "error": format!("invalid move_file destination: {err}"),
                                "path": to,
                            }),
                            ToolCostHint::default(),
                            None,
                        ));
                    }
                };
                let rel_from = self
                    .relative(&abs_from)
                    .to_string_lossy()
                    .replace('\\', "/");
                let rel_to = self.relative(&abs_to).to_string_lossy().replace('\\', "/");
                if abs_to.exists() {
                    return Err(make_result(
                        call,
                        ToolStatus::Stale,
                        json!({
                            "error": "move_file destination already exists",
                            "from": rel_from,
                            "to": rel_to,
                        }),
                        ToolCostHint::default(),
                        None,
                    ));
                }
                let contents = match fs::read_to_string(&abs_from) {
                    Ok(text) => text,
                    Err(err) => {
                        return Err(tool_error(
                            call,
                            format!("failed to read move source {rel_from}: {err}"),
                        ));
                    }
                };
                let before_sha256 = sha256_hex(contents.as_bytes());
                match expected_sha256.as_deref() {
                    Some(expected) if expected == before_sha256 => {}
                    Some(_) => {
                        return Err(make_result(
                            call,
                            ToolStatus::Stale,
                            json!({
                                "error": "expected_sha256 does not match source",
                                "path": rel_from,
                                "current_sha256": before_sha256,
                            }),
                            ToolCostHint::default(),
                            Some(before_sha256),
                        ));
                    }
                    None => {
                        if let Err(result) = self.gate_on_seen_sha256(
                            call,
                            index,
                            &rel_from,
                            &before_sha256,
                            "move_file",
                        ) {
                            return Err(result);
                        }
                    }
                }
                let mut final_contents = contents.clone();
                if let Some(post) = post_replace {
                    let matches = final_contents.match_indices(post.search.as_str()).count();
                    if matches == 0 {
                        return Err(make_result(
                            call,
                            ToolStatus::Stale,
                            json!({
                                "error": "post_replace.search text was not found in move source",
                                "from": rel_from,
                                "to": rel_to,
                                "patch_index": index,
                            }),
                            ToolCostHint::default(),
                            Some(before_sha256),
                        ));
                    }
                    let allow_multi = post.allow_multiple.unwrap_or(false);
                    if matches > 1 && !allow_multi {
                        return Err(make_result(
                            call,
                            ToolStatus::Stale,
                            json!({
                                "error": "post_replace.search matched more than once; narrow it or set allow_multiple=true",
                                "from": rel_from,
                                "to": rel_to,
                                "patch_index": index,
                                "matches": matches,
                            }),
                            ToolCostHint::default(),
                            Some(before_sha256),
                        ));
                    }
                    final_contents = if allow_multi {
                        final_contents.replace(post.search.as_str(), post.replace.as_str())
                    } else {
                        final_contents.replacen(post.search.as_str(), post.replace.as_str(), 1)
                    };
                }
                staged.push_move(
                    rel_from.clone(),
                    abs_from,
                    rel_to.clone(),
                    abs_to,
                    contents,
                    final_contents,
                    before_sha256,
                );
                preview_ops.push(json!({
                    "patch_index": index,
                    "kind": "move_file",
                    "from": rel_from,
                    "to": rel_to,
                    "post_replace": post_replace.is_some(),
                }));
                Ok(())
            }
        }
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
        let output_mode = args.output_mode.unwrap_or_default();
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

        let Some(plan) = verify_command_plan(&self.root, scope, level, &changed_paths) else {
            return make_result(
                call,
                ToolStatus::Success,
                json!({
                    "scope": verify_scope_str(scope),
                    "level": verify_level_str(level),
                    "changed_files": changed_paths,
                    "command": null,
                    "no_op": true,
                    "not_run": true,
                    "reason": "no Cargo.toml found for Rust verification",
                }),
                ToolCostHint::default(),
                None,
            );
        };
        let shell_call = ToolCall {
            call_id: call.call_id.clone(),
            name: "shell".to_string(),
            arguments: json!({
                "command": plan.command,
                "description": "run verification scoped by current diff",
                "timeout_ms": VERIFY_SHELL_TIMEOUT_MS,
                "output_byte_cap": DEFAULT_SHELL_OUTPUT_BYTE_CAP,
                "output_mode": output_mode.as_str(),
            }),
        };
        let shell_result = self
            .execute_shell_capped(&shell_call, cancel, VERIFY_SHELL_TIMEOUT_MS, group_id, None)
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
        if let Err(err) = safety::assess_write_path(&args.path, &self.root, &self.shell_sandbox) {
            return make_result(
                call,
                ToolStatus::Denied,
                json!({
                    "error": err.message(),
                    "path": args.path,
                    "reason": err.code(),
                    "permission_denied": true,
                    "policy_denied": true,
                }),
                ToolCostHint::default(),
                None,
            );
        }
        let path = match self.resolve_for_write(&args.path) {
            Ok(path) => path,
            Err(err) => return tool_error(call, err),
        };
        let rel = self.relative(&path);
        if is_secret_path(&rel)
            || safety::path_targets_protected_metadata(&path, &self.root, &self.shell_sandbox)
                .is_some()
        {
            return make_result(
                call,
                ToolStatus::Denied,
                json!({ "error": "refusing to write a likely secret or protected metadata file" }),
                ToolCostHint::default(),
                None,
            );
        }

        let checkpoint_before = match self.track_checkpoint_tree() {
            Ok(snapshot) => snapshot,
            Err(err) => return tool_error(call, err),
        };
        let before = fs::read(&path).ok();
        let before_sha256 = before.as_ref().map(sha256_hex);
        if let Some(current_sha) = before_sha256.clone() {
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            match args.expected_sha256.as_deref() {
                Some(expected) if expected == current_sha => {}
                Some(_) => {
                    return make_result(
                        call,
                        ToolStatus::Stale,
                        json!({
                            "error": "expected_sha256 does not match current file",
                            "path": rel_str,
                            "current_sha256": before_sha256,
                        }),
                        ToolCostHint::default(),
                        before_sha256,
                    );
                }
                None => {
                    if let Err(result) =
                        self.gate_on_seen_sha256(call, 0, &rel_str, &current_sha, "write_file")
                    {
                        return result;
                    }
                }
            }
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
            checkpoint_before.as_ref(),
            call,
            group_id,
            ToolStatus::Success,
            Vec::new(),
        );
        make_result(call, ToolStatus::Success, content, cost, Some(after_sha256))
    }

    pub(crate) fn resolve_existing(&self, raw: &str) -> std::result::Result<PathBuf, String> {
        let candidate = self.join_workspace(raw)?;
        let canonical = canonicalize_workspace_root(&candidate)
            .map_err(|err| format!("path does not exist or is inaccessible: {err}"))?;
        self.ensure_inside(canonical)
    }

    pub(crate) fn resolve_shell_workdir(&self, raw: &str) -> std::result::Result<PathBuf, String> {
        let candidate = self.join_shell_path(raw)?;
        let canonical = canonicalize_workspace_root(&candidate)
            .map_err(|err| format!("path does not exist or is inaccessible: {err}"))?;
        if !canonical.is_dir() {
            return Err("path is not a directory".to_string());
        }
        if canonical.starts_with(self.root.as_ref())
            || self
                .shell_sandbox
                .read_roots
                .iter()
                .chain(self.shell_sandbox.write_roots.iter())
                .any(|root| canonical.starts_with(root))
        {
            Ok(canonical)
        } else {
            Err("path is outside the workspace and configured shell sandbox roots".to_string())
        }
    }

    fn resolve_for_write(&self, raw: &str) -> std::result::Result<PathBuf, String> {
        let candidate = self.join_workspace(raw)?;
        if candidate.exists() {
            return self.resolve_existing(raw);
        }
        let parent = candidate
            .parent()
            .ok_or_else(|| "path has no parent".to_string())?;
        let parent = canonicalize_workspace_root(parent)
            .map_err(|err| format!("parent directory does not exist or is inaccessible: {err}"))?;
        self.ensure_inside(parent)?;
        Ok(candidate)
    }

    pub(crate) fn join_workspace(&self, raw: &str) -> std::result::Result<PathBuf, String> {
        let path = self.join_shell_path(raw)?;
        if !path.starts_with(self.root.as_ref()) {
            return Err("path must stay inside the workspace".to_string());
        }
        Ok(path)
    }

    fn join_shell_path(&self, raw: &str) -> std::result::Result<PathBuf, String> {
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

    pub(crate) fn relative(&self, path: &Path) -> PathBuf {
        path.strip_prefix(self.root.as_ref())
            .unwrap_or(path)
            .to_path_buf()
    }

    pub(crate) fn track_checkpoint_tree(&self) -> Result<Option<WorkspaceSnapshot>> {
        self.checkpoints
            .as_ref()
            .map(|checkpoints| checkpoints.track_tree())
            .transpose()
    }

    pub(crate) fn append_checkpoint_to_content(
        &self,
        content: &mut Value,
        before: Option<&WorkspaceSnapshot>,
        call: &ToolCall,
        group_id: &str,
        status: ToolStatus,
        coverage_warnings: Vec<String>,
    ) {
        let (Some(checkpoints), Some(before)) = (self.checkpoints.as_ref(), before) else {
            return;
        };
        match checkpoints.create_checkpoint(
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
        let result = redact_tool_result(result, &self.redactor);
        self.output_store
            .maybe_spill(enforce_web_quote_limit(result))
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
        let model_output = result.model_output();
        if model_output.len() <= self.spill_threshold_bytes {
            return result;
        }

        let output = result
            .spill_model_output
            .take()
            .unwrap_or_else(|| model_output.clone());
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

        let (preview, _) = truncate_middle_bytes(&model_output, self.preview_bytes);
        let ToolResult {
            call_id,
            tool_name,
            status,
            content: _,
            mut cost_hint,
            receipt,
            spill_model_output: _,
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

pub(crate) fn unix_timestamp_millis(time: SystemTime) -> u128 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

pub(crate) fn collapse_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShellPermissionAnalysis {
    pub(crate) capability: PermissionCapability,
    pub(crate) risk: PermissionRisk,
    pub(crate) rule_target: String,
    pub(crate) network: bool,
    pub(crate) destructive: bool,
    pub(crate) parser_backed: bool,
    pub(crate) dynamic: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VerifyArgs {
    scope: Option<VerifyScope>,
    level: Option<VerifyLevel>,
    output_mode: Option<OutputMode>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RefreshCompilerFactsArgs {
    diagnostics: Option<bool>,
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

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OutputMode {
    #[default]
    Shaped,
    Raw,
}

impl OutputMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Shaped => "shaped",
            Self::Raw => "raw",
        }
    }

    pub(crate) const fn is_shaped(self) -> bool {
        matches!(self, Self::Shaped)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListSkillsArgs {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LoadSkillArgs {
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NotesRememberArgs {
    kind: String,
    text: String,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    source: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NotesRecallArgs {
    query: String,
    #[serde(default)]
    limit: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ObservationsArgs {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
}

fn parse_observation_kind(raw: &str) -> Option<ObservationKind> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "preference" => Some(ObservationKind::Preference),
        "decision" => Some(ObservationKind::Decision),
        "convention" => Some(ObservationKind::Convention),
        "dead_end" | "dead-end" => Some(ObservationKind::DeadEnd),
        "note" => Some(ObservationKind::Note),
        _ => None,
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct McpListResourcesArgs {
    server: String,
    cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct McpReadResourceArgs {
    server: String,
    uri: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteFileArgs {
    path: String,
    content: String,
    expected_sha256: Option<String>,
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

pub(crate) fn diff_mode_str(mode: DiffMode) -> &'static str {
    match mode {
        DiffMode::Worktree => "worktree",
        DiffMode::Branch => "branch",
        DiffMode::BranchBase => "branch_base",
        DiffMode::Index => "index",
    }
}

pub(crate) fn diff_path_set(snapshot: &DiffSnapshot) -> BTreeSet<String> {
    snapshot
        .files
        .iter()
        .map(|file| file.path.clone())
        .collect()
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

pub(crate) fn diff_status_str(status: DiffFileStatus) -> &'static str {
    match status {
        DiffFileStatus::Added => "added",
        DiffFileStatus::Deleted => "deleted",
        DiffFileStatus::Modified => "modified",
        DiffFileStatus::Renamed => "renamed",
    }
}

#[derive(Debug)]
pub(crate) struct PatchFileState {
    path: PathBuf,
    rel: String,
    before: String,
    current: String,
    before_sha256: String,
}

/// Pending changes accumulated during validation. Each entry corresponds to a
/// single op the model issued; the final `apply_*` methods on `StagedOp` are
/// what actually mutate disk during the commit phase.
#[derive(Debug, Default)]
pub(crate) struct StagedApply {
    pub(crate) files: Vec<PatchFileState>,
    file_index: BTreeMap<String, usize>,
    pub(crate) ops: Vec<StagedOp>,
}

#[derive(Debug)]
pub(crate) enum StagedOp {
    SearchReplace {
        rel: String,
        file_index: usize,
        exact: bool,
        /// Audit tag describing which rescue path matched the search when the
        /// byte-exact lookup failed. `None` for verbatim matches; `Some` for
        /// `unified_diff` / `quote_normalize` so the post-apply `delta` and
        /// log can attribute the inexact apply.
        fallback: Option<&'static str>,
    },
    CreateFile {
        rel: String,
        abs_path: PathBuf,
        contents: String,
    },
    DeleteFile {
        rel: String,
        abs_path: PathBuf,
        before_sha256: String,
        before_len: usize,
    },
    MoveFile {
        rel_from: String,
        abs_from: PathBuf,
        rel_to: String,
        abs_to: PathBuf,
        before_contents: String,
        after_contents: String,
        before_sha256: String,
    },
}

impl StagedApply {
    fn ensure_search_replace(&mut self, rel: &str, abs_path: &Path) -> std::io::Result<usize> {
        if let Some(&idx) = self.file_index.get(rel) {
            // Re-use the existing file entry, and only push a fresh op so the
            // apply phase still tracks each search/replace as a distinct op
            // for `applied_delta`. The op's file_index points into `files`.
            self.ops.push(StagedOp::SearchReplace {
                rel: rel.to_string(),
                file_index: idx,
                exact: true,
                fallback: None,
            });
            return Ok(idx);
        }
        let before = fs::read_to_string(abs_path)?;
        let before_sha256 = sha256_hex(before.as_bytes());
        self.files.push(PatchFileState {
            path: abs_path.to_path_buf(),
            rel: rel.to_string(),
            before: before.clone(),
            current: before,
            before_sha256,
        });
        let idx = self.files.len() - 1;
        self.file_index.insert(rel.to_string(), idx);
        self.ops.push(StagedOp::SearchReplace {
            rel: rel.to_string(),
            file_index: idx,
            exact: true,
            fallback: None,
        });
        Ok(idx)
    }

    /// Mark the most-recently staged op as non-exact and stamp the fallback
    /// tag (e.g., the search-replace matched only via `unified_diff` or
    /// `quote_normalize` rather than verbatim). Both flags surface in
    /// `applied_delta` so the audit log captures the rescue.
    fn mark_last_op_inexact(&mut self, fallback_tag: Option<&'static str>) {
        if let Some(StagedOp::SearchReplace {
            exact, fallback, ..
        }) = self.ops.last_mut()
        {
            *exact = false;
            if fallback_tag.is_some() {
                *fallback = fallback_tag;
            }
        }
    }

    fn push_create(&mut self, rel: String, abs_path: PathBuf, contents: String) {
        self.ops.push(StagedOp::CreateFile {
            rel,
            abs_path,
            contents,
        });
    }

    fn push_delete(
        &mut self,
        rel: String,
        abs_path: PathBuf,
        before_sha256: String,
        before_len: usize,
    ) {
        self.ops.push(StagedOp::DeleteFile {
            rel,
            abs_path,
            before_sha256,
            before_len,
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn push_move(
        &mut self,
        rel_from: String,
        abs_from: PathBuf,
        rel_to: String,
        abs_to: PathBuf,
        before_contents: String,
        after_contents: String,
        before_sha256: String,
    ) {
        self.ops.push(StagedOp::MoveFile {
            rel_from,
            abs_from,
            rel_to,
            abs_to,
            before_contents,
            after_contents,
            before_sha256,
        });
    }

    pub(crate) fn bytes_read(&self) -> u64 {
        let from_files: u64 = self.files.iter().map(|f| f.before.len() as u64).sum();
        let from_ops: u64 = self
            .ops
            .iter()
            .map(|op| match op {
                StagedOp::DeleteFile { before_len, .. } => *before_len as u64,
                StagedOp::MoveFile {
                    before_contents, ..
                } => before_contents.len() as u64,
                _ => 0,
            })
            .sum();
        from_files + from_ops
    }

    pub(crate) fn bytes_written(&self) -> u64 {
        let from_files: u64 = self.files.iter().map(|f| f.current.len() as u64).sum();
        let from_ops: u64 = self
            .ops
            .iter()
            .map(|op| match op {
                StagedOp::CreateFile { contents, .. } => contents.len() as u64,
                StagedOp::MoveFile { after_contents, .. } => after_contents.len() as u64,
                _ => 0,
            })
            .sum();
        from_files + from_ops
    }

    pub(crate) fn changed_files_json(&self) -> Vec<Value> {
        let mut out = Vec::new();
        for state in &self.files {
            out.push(json!({
                "path": state.rel,
                "before_sha256": state.before_sha256,
                "after_sha256": sha256_hex(state.current.as_bytes()),
                "bytes_before": state.before.len(),
                "bytes_after": state.current.len(),
                "changed": state.before != state.current,
            }));
        }
        for op in &self.ops {
            match op {
                StagedOp::CreateFile { rel, contents, .. } => out.push(json!({
                    "path": rel,
                    "before_sha256": Value::Null,
                    "after_sha256": sha256_hex(contents.as_bytes()),
                    "bytes_before": 0,
                    "bytes_after": contents.len(),
                    "changed": true,
                })),
                StagedOp::DeleteFile {
                    rel,
                    before_sha256,
                    before_len,
                    ..
                } => out.push(json!({
                    "path": rel,
                    "before_sha256": before_sha256,
                    "after_sha256": Value::Null,
                    "bytes_before": before_len,
                    "bytes_after": 0,
                    "changed": true,
                })),
                StagedOp::MoveFile {
                    rel_from,
                    rel_to,
                    before_contents,
                    after_contents,
                    before_sha256,
                    ..
                } => out.push(json!({
                    "path": rel_to,
                    "from_path": rel_from,
                    "before_sha256": before_sha256,
                    "after_sha256": sha256_hex(after_contents.as_bytes()),
                    "bytes_before": before_contents.len(),
                    "bytes_after": after_contents.len(),
                    "changed": true,
                })),
                StagedOp::SearchReplace { .. } => {}
            }
        }
        out
    }

    pub(crate) fn delta_preview_json(&self, _applied: bool) -> Vec<Value> {
        self.ops
            .iter()
            .enumerate()
            .map(|(idx, op)| op.delta_json_with_index("staged", idx))
            .collect()
    }
}

impl StagedOp {
    pub(crate) fn primary_path(&self) -> &str {
        match self {
            StagedOp::SearchReplace { rel, .. } => rel,
            StagedOp::CreateFile { rel, .. } => rel,
            StagedOp::DeleteFile { rel, .. } => rel,
            StagedOp::MoveFile { rel_to, .. } => rel_to,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            StagedOp::SearchReplace { .. } => "search_replace",
            StagedOp::CreateFile { .. } => "create_file",
            StagedOp::DeleteFile { .. } => "delete_file",
            StagedOp::MoveFile { .. } => "move_file",
        }
    }

    pub(crate) fn delta_json_with_index(&self, status: &str, index_hint: usize) -> Value {
        self.delta_json_full(status, index_hint, self.exact(), None)
    }

    /// True when the staged op matched the file's pre-image byte-for-byte.
    /// Create/Delete/Move are always exact (no fuzz matching applies); a
    /// SearchReplace becomes inexact when its literal `search` body did not
    /// appear in the file and the `unified_diff` fallback resolved the change
    /// via `git apply --3way`.
    pub(crate) fn exact(&self) -> bool {
        match self {
            StagedOp::SearchReplace { exact, .. } => *exact,
            StagedOp::CreateFile { .. }
            | StagedOp::DeleteFile { .. }
            | StagedOp::MoveFile { .. } => true,
        }
    }

    pub(crate) fn delta_json_full(
        &self,
        status: &str,
        index_hint: usize,
        exact: bool,
        error: Option<&str>,
    ) -> Value {
        let mut value = json!({
            "kind": self.kind(),
            "status": status,
            "path": self.primary_path(),
            "exact": exact,
        });
        if let Some(obj) = value.as_object_mut() {
            if index_hint != usize::MAX {
                obj.insert("patch_index".to_string(), json!(index_hint));
            }
            if let Some(message) = error {
                obj.insert("error".to_string(), json!(message));
            }
            match self {
                StagedOp::SearchReplace {
                    fallback: Some(tag),
                    ..
                } => {
                    obj.insert("fallback".to_string(), json!(tag));
                }
                StagedOp::SearchReplace { .. } => {}
                StagedOp::CreateFile { contents, .. } => {
                    obj.insert(
                        "after_sha256".to_string(),
                        json!(sha256_hex(contents.as_bytes())),
                    );
                }
                StagedOp::DeleteFile { before_sha256, .. } => {
                    obj.insert("before_sha256".to_string(), json!(before_sha256));
                }
                StagedOp::MoveFile {
                    rel_from,
                    before_sha256,
                    after_contents,
                    ..
                } => {
                    obj.insert("from_path".to_string(), json!(rel_from));
                    obj.insert("before_sha256".to_string(), json!(before_sha256));
                    obj.insert(
                        "after_sha256".to_string(),
                        json!(sha256_hex(after_contents.as_bytes())),
                    );
                }
            }
        }
        value
    }

    pub(crate) fn apply(
        &self,
        files: &[PatchFileState],
        written: &mut BTreeSet<usize>,
    ) -> std::io::Result<()> {
        match self {
            StagedOp::SearchReplace { file_index, .. } => {
                if written.contains(file_index) {
                    return Ok(());
                }
                let state = &files[*file_index];
                if state.before == state.current {
                    written.insert(*file_index);
                    return Ok(());
                }
                fs::write(&state.path, state.current.as_bytes())?;
                written.insert(*file_index);
                Ok(())
            }
            StagedOp::CreateFile {
                abs_path, contents, ..
            } => {
                if let Some(parent) = abs_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(abs_path, contents.as_bytes())
            }
            StagedOp::DeleteFile { abs_path, .. } => fs::remove_file(abs_path),
            StagedOp::MoveFile {
                abs_from,
                abs_to,
                after_contents,
                ..
            } => {
                if let Some(parent) = abs_to.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(abs_to, after_contents.as_bytes())?;
                fs::remove_file(abs_from)?;
                Ok(())
            }
        }
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

#[cfg(test)]
fn verify_command(
    root: &Path,
    scope: VerifyScope,
    level: VerifyLevel,
    changed_paths: &[String],
) -> String {
    verify_command_plan(root, scope, level, changed_paths)
        .map(|plan| plan.command)
        .unwrap_or_else(|| match level {
            VerifyLevel::Quick => "cargo test --workspace --message-format=json".to_string(),
            VerifyLevel::Full => "cargo fmt --check && cargo clippy --workspace --all-targets --message-format=json -- -D warnings && cargo test --workspace --message-format=json".to_string(),
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VerifyCommandPlan {
    command: String,
}

fn verify_command_plan(
    root: &Path,
    scope: VerifyScope,
    level: VerifyLevel,
    changed_paths: &[String],
) -> Option<VerifyCommandPlan> {
    if root.join("Cargo.toml").is_file() {
        return Some(VerifyCommandPlan {
            command: workspace_verify_command(root, scope, level, changed_paths),
        });
    }

    let manifest_paths = match scope {
        VerifyScope::Workspace => nested_manifest_paths(root),
        VerifyScope::Diff => diff_manifest_paths(root, changed_paths),
    };
    if manifest_paths.is_empty() {
        return None;
    }
    let test_commands = manifest_paths
        .iter()
        .map(|manifest| {
            format!(
                "cargo test --manifest-path {} --message-format=json",
                shell_quote_path(manifest)
            )
        })
        .collect::<Vec<_>>();
    let command = match level {
        VerifyLevel::Quick => test_commands.join(" && "),
        VerifyLevel::Full => {
            let fmt_commands = manifest_paths
                .iter()
                .map(|manifest| {
                    format!(
                        "cargo fmt --check --manifest-path {}",
                        shell_quote_path(manifest)
                    )
                })
                .collect::<Vec<_>>();
            let clippy_commands = manifest_paths
                .iter()
                .map(|manifest| {
                    format!(
                        "cargo clippy --manifest-path {} --all-targets --message-format=json -- -D warnings",
                        shell_quote_path(manifest)
                    )
                })
                .collect::<Vec<_>>();
            fmt_commands
                .into_iter()
                .chain(clippy_commands)
                .chain(test_commands)
                .collect::<Vec<_>>()
                .join(" && ")
        }
    };
    Some(VerifyCommandPlan { command })
}

fn workspace_verify_command(
    root: &Path,
    scope: VerifyScope,
    level: VerifyLevel,
    changed_paths: &[String],
) -> String {
    let test_command = match scope {
        VerifyScope::Workspace => "cargo test --workspace --message-format=json".to_string(),
        VerifyScope::Diff => {
            let packages = diff_package_names(root, changed_paths);
            if packages.is_empty() {
                "cargo test --workspace --message-format=json".to_string()
            } else {
                packages
                    .into_iter()
                    .map(|package| {
                        format!(
                            "cargo test -p {} --message-format=json",
                            shell_quote(&package)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" && ")
            }
        }
    };
    match level {
        VerifyLevel::Quick => test_command,
        VerifyLevel::Full => format!(
            "cargo fmt --check && cargo clippy --workspace --all-targets --message-format=json -- -D warnings && {test_command}"
        ),
    }
}

fn diff_manifest_paths(root: &Path, changed_paths: &[String]) -> Vec<PathBuf> {
    changed_paths
        .iter()
        .filter(|path| is_rust_verification_path(path))
        .filter_map(|path| nearest_manifest_for_path(root, path))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn nearest_manifest_for_path(root: &Path, path: &str) -> Option<PathBuf> {
    let mut cursor = root.join(path);
    if cursor.extension().is_some() {
        cursor.pop();
    }
    loop {
        let manifest = cursor.join("Cargo.toml");
        if manifest.is_file() {
            return manifest.strip_prefix(root).ok().map(Path::to_path_buf);
        }
        if cursor == root || !cursor.pop() {
            return None;
        }
    }
}

fn nested_manifest_paths(root: &Path) -> Vec<PathBuf> {
    let mut manifests = BTreeSet::new();
    collect_nested_manifest_paths(root, root, &mut manifests);
    manifests.into_iter().collect()
}

fn collect_nested_manifest_paths(root: &Path, dir: &Path, manifests: &mut BTreeSet<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name();
        if matches!(
            name.to_string_lossy().as_ref(),
            ".git" | "target" | "node_modules"
        ) {
            continue;
        }
        let manifest = path.join("Cargo.toml");
        if manifest.is_file()
            && let Ok(relative) = manifest.strip_prefix(root)
        {
            manifests.insert(relative.to_path_buf());
        }
        collect_nested_manifest_paths(root, &path, manifests);
    }
}

fn shell_stdout(result: &ToolResult) -> &str {
    result
        .content
        .get("stdout")
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn shell_stderr(result: &ToolResult) -> &str {
    result
        .content
        .get("stderr")
        .and_then(Value::as_str)
        .unwrap_or("")
}

fn compiler_fact_command_error(call: &ToolCall, reason: &str, result: ToolResult) -> ToolResult {
    make_result(
        call,
        ToolStatus::Error,
        json!({
            "error": reason,
            "exit_code": result.content.get("exit_code").cloned(),
            "stdout": shell_stdout(&result),
            "stderr": shell_stderr(&result),
        }),
        result.cost_hint,
        None,
    )
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
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

pub(crate) fn shell_quote_path(path: &Path) -> String {
    let normalized = workspace_path(path);
    shell_quote(&normalized)
}

pub(crate) fn workspace_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Strip the Windows verbatim (`\\?\`) prefix from configured shell sandbox
/// roots so that comparisons against canonicalized workdirs (which also have
/// the prefix stripped) match. Without this, sandbox config that came from
/// `fs::canonicalize` would not align with workdirs canonicalized through
/// `canonicalize_workspace_root`.
fn normalize_shell_sandbox_paths(mut config: ShellSandboxConfig) -> ShellSandboxConfig {
    if cfg!(windows) {
        for root in config.read_roots.iter_mut() {
            *root = strip_verbatim_prefix(std::mem::take(root));
        }
        for root in config.write_roots.iter_mut() {
            *root = strip_verbatim_prefix(std::mem::take(root));
        }
    }
    config
}

pub(crate) fn make_result(
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
        spill_model_output: None,
    }
}

pub(crate) fn shell_exit_signal(status: Option<&std::process::ExitStatus>) -> Option<i32> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.and_then(|status| status.signal())
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        None
    }
}

pub(crate) fn checkpoints_disabled_result(call: &ToolCall) -> ToolResult {
    make_result(
        call,
        ToolStatus::Stale,
        json!({
            "enabled": false,
            "error": CHECKPOINTS_DISABLED_MESSAGE,
        }),
        ToolCostHint::default(),
        None,
    )
}

pub(crate) fn tool_arg_error(call: &ToolCall, err: serde_json::Error) -> ToolResult {
    make_result(
        call,
        ToolStatus::Error,
        json!({ "error": format!("invalid tool arguments: {err}") }),
        ToolCostHint::default(),
        None,
    )
}

pub(crate) fn tool_error(call: &ToolCall, err: impl ToString) -> ToolResult {
    make_result(
        call,
        ToolStatus::Error,
        json!({ "error": err.to_string() }),
        ToolCostHint::default(),
        None,
    )
}

pub(crate) fn build_required_glob(pattern: &str) -> std::result::Result<GlobSet, String> {
    let mut builder = GlobSetBuilder::new();
    if pattern.contains('/') {
        builder.add(Glob::new(pattern).map_err(|err| err.to_string())?);
    } else {
        builder.add(Glob::new(pattern).map_err(|err| err.to_string())?);
        builder.add(Glob::new(&format!("**/{pattern}")).map_err(|err| err.to_string())?);
    }
    builder.build().map_err(|err| err.to_string())
}

pub(crate) fn build_include_set(
    patterns: Option<&[String]>,
) -> std::result::Result<Option<GlobSet>, String> {
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

pub(crate) fn read_prefix(
    path: &Path,
    limit: usize,
) -> std::result::Result<Vec<u8>, std::io::Error> {
    let mut file = fs::File::open(path)?;
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(limit as u64)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

pub(crate) fn read_range(
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

pub(crate) fn sha256_file(path: &Path) -> std::result::Result<String, std::io::Error> {
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

pub(crate) fn file_len(path: &Path) -> std::result::Result<u64, std::io::Error> {
    Ok(fs::metadata(path)?.len())
}

pub(crate) fn is_secret_path(path: &Path) -> bool {
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

pub(crate) fn truncate_text(value: &str, max_chars: usize) -> String {
    let mut output = String::new();
    for ch in value.chars().take(max_chars) {
        output.push(ch);
    }
    if output.len() < value.len() {
        output.push_str("...");
    }
    output
}

/// Curly-quote → straight-quote map used by the apply_patch search fallback.
/// Mirrors clear-code's `normalizeQuotes` (`src/tools/FileEditTool/utils.ts:31`).
/// Each curly quote in UTF-8 is 3 bytes; the straight counterpart is 1 byte —
/// so a normalized copy of any string is at most the same byte length as the
/// original, never longer. That lets us safely index normalized offsets back
/// into the original via a per-character byte map.
fn map_curly_to_straight(ch: char) -> Option<char> {
    match ch {
        '\u{2018}' | '\u{2019}' => Some('\''),
        '\u{201C}' | '\u{201D}' => Some('"'),
        _ => None,
    }
}
fn normalize_quotes(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for ch in value.chars() {
        output.push(map_curly_to_straight(ch).unwrap_or(ch));
    }
    output
}
/// Locate `search` inside `content` after collapsing curly quotes to their
/// straight ASCII counterparts on both sides. Returns `(byte_start, byte_end)`
/// pointing at the matched slice in the *original* `content`, plus the number
/// of normalized matches — so the caller can refuse to apply when the
/// normalized search matched more than once. Returns `None` when no normalized
/// match exists. Used only after `match_indices(search)` returned zero; never
/// shadows an exact hit.
fn find_with_quote_normalization(content: &str, search: &str) -> Option<(usize, usize, usize)> {
    let normalized_search = normalize_quotes(search);
    if normalized_search.is_empty() {
        return None;
    }
    // Build a per-byte map from normalized-content byte offsets back to
    // original-content byte offsets. We push one entry per byte produced into
    // the normalized buffer; the curly-quote case shrinks 3 bytes → 1.
    let mut normalized = String::with_capacity(content.len());
    let mut byte_map: Vec<usize> = Vec::with_capacity(content.len());
    for (orig_idx, ch) in content.char_indices() {
        let mapped = map_curly_to_straight(ch).unwrap_or(ch);
        let pre_len = normalized.len();
        normalized.push(mapped);
        for _ in 0..(normalized.len() - pre_len) {
            byte_map.push(orig_idx);
        }
    }
    // Map for the byte just past the end so `match_end` can resolve.
    byte_map.push(content.len());
    let mut matches = Vec::new();
    let mut scan_from = 0;
    while let Some(rel) = normalized[scan_from..].find(normalized_search.as_str()) {
        let n_start = scan_from + rel;
        let n_end = n_start + normalized_search.len();
        matches.push((n_start, n_end));
        scan_from = n_start + 1;
    }
    if matches.is_empty() {
        return None;
    }
    let (n_start, n_end) = matches[0];
    let orig_start = *byte_map.get(n_start)?;
    let orig_end = *byte_map.get(n_end)?;
    Some((orig_start, orig_end, matches.len()))
}
/// Re-emit curly quotes in `replace` whenever the matched original slice used
/// them, so a quote-normalized edit preserves the file's typography. Mirrors
/// clear-code's `preserveQuoteStyle` (`src/tools/FileEditTool/utils.ts:104`)
/// with the same open/close heuristic plus the apostrophe-in-contraction
/// special case so `"don't" → "don't"` round-trips correctly.
fn preserve_quote_style(replace: &str, original_slice: &str) -> String {
    let has_curly_double =
        original_slice.contains('\u{201C}') || original_slice.contains('\u{201D}');
    let has_curly_single =
        original_slice.contains('\u{2018}') || original_slice.contains('\u{2019}');
    if !has_curly_double && !has_curly_single {
        return replace.to_string();
    }
    let chars: Vec<char> = replace.chars().collect();
    let mut output = String::with_capacity(replace.len());
    for (idx, ch) in chars.iter().enumerate() {
        match ch {
            '"' if has_curly_double => {
                output.push(if is_opening_quote_context(&chars, idx) {
                    '\u{201C}'
                } else {
                    '\u{201D}'
                });
            }
            '\'' if has_curly_single => {
                let prev = idx.checked_sub(1).and_then(|i| chars.get(i)).copied();
                let next = chars.get(idx + 1).copied();
                let prev_is_letter = prev.map(|c| c.is_alphabetic()).unwrap_or(false);
                let next_is_letter = next.map(|c| c.is_alphabetic()).unwrap_or(false);
                if prev_is_letter && next_is_letter {
                    // Contraction (e.g. "don't") — always the right curly.
                    output.push('\u{2019}');
                } else {
                    output.push(if is_opening_quote_context(&chars, idx) {
                        '\u{2018}'
                    } else {
                        '\u{2019}'
                    });
                }
            }
            _ => output.push(*ch),
        }
    }
    output
}
fn is_opening_quote_context(chars: &[char], index: usize) -> bool {
    if index == 0 {
        return true;
    }
    matches!(
        chars[index - 1],
        ' ' | '\t' | '\n' | '\r' | '(' | '[' | '{' | '\u{2014}' | '\u{2013}'
    )
}
fn patch_match_contexts(content: &str, search: &str, max_matches: usize) -> Vec<Value> {
    content
        .match_indices(search)
        .take(max_matches)
        .enumerate()
        .map(|(index, (byte_index, _))| {
            let line = content[..byte_index]
                .bytes()
                .filter(|byte| *byte == b'\n')
                .count()
                + 1;
            let line_start = content[..byte_index]
                .rfind('\n')
                .map(|position| position + 1)
                .unwrap_or(0);
            let line_end = content[byte_index..]
                .find('\n')
                .map(|position| byte_index + position)
                .unwrap_or(content.len());
            json!({
                "match_index": index + 1,
                "line": line,
                "preview": truncate_text(&content[line_start..line_end], 240),
            })
        })
        .collect()
}

/// Side-table extracted from a shell `ToolResult` describing a single
/// best_effort sandbox fallback. The agent layer uses this to (a) tick the
/// `approval.best_effort.fallback` telemetry counter and (b) decide whether
/// to publish a once-per-session TUI warning.
///
/// `backend` is the OS sandbox backend that was attempted (e.g.
/// `macos-sandbox-exec`); `fallback_count` is the cumulative number of
/// fallbacks across the registry's lifetime (so per session); and
/// `first_in_session` is the one-shot latch indicating whether this is the
/// first time the registry has seen a fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellBestEffortFallback {
    pub backend: String,
    pub fallback_count: u64,
    pub first_in_session: bool,
}

/// Extract the best_effort fallback descriptor from a shell `ToolResult`,
/// or `None` when the call did not degrade to the best_effort path. Reads
/// the same `sandbox.best_effort_fallback` JSON that the audit row carries,
/// so the agent and the audit log stay in lockstep.
pub fn shell_best_effort_fallback_from_result(
    result: &ToolResult,
) -> Option<ShellBestEffortFallback> {
    if result.tool_name != "shell" {
        return None;
    }
    let payload = result.content.get("sandbox")?.get("best_effort_fallback")?;
    let backend = payload.get("backend")?.as_str()?.to_string();
    let fallback_count = payload.get("fallback_count")?.as_u64()?;
    let first_in_session = payload.get("first_in_session")?.as_bool()?;
    Some(ShellBestEffortFallback {
        backend,
        fallback_count,
        first_in_session,
    })
}

pub fn sha256_hex(bytes: impl AsRef<[u8]>) -> String {
    let digest = Sha256::digest(bytes.as_ref());
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
