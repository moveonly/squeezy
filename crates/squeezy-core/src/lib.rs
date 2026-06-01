use std::{
    borrow::Cow,
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    env, fmt, fs,
    path::{Path, PathBuf},
    process,
    time::Duration,
};

use regex::{Captures, Regex};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod config_schema;
mod hardening;
pub mod settings_writer;
pub use hardening::pre_main_hardening;

pub const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_OPENAI_MODEL: &str = "gpt-5.5";
/// ChatGPT Plus/Pro Codex backend. The protocol is OpenAI's Responses
/// API, but the requests go through the ChatGPT backend with the
/// subscription's account id stamped in `chatgpt-account-id`.
pub const DEFAULT_OPENAI_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
pub const DEFAULT_OPENAI_CODEX_MODEL: &str = DEFAULT_OPENAI_MODEL;
/// Originator tag stamped on every Codex request so OpenAI can attribute
/// traffic to squeezy in their dashboards.
pub const DEFAULT_OPENAI_CODEX_ORIGINATOR: &str = "squeezy";
pub const DEFAULT_ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com/v1";
pub const DEFAULT_ANTHROPIC_MODEL: &str = "claude-sonnet-4-6";
pub const DEFAULT_GOOGLE_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
pub const DEFAULT_GOOGLE_MODEL: &str = "gemini-2.5-pro";
pub const DEFAULT_AZURE_OPENAI_BASE_URL: &str = "";
pub const DEFAULT_AZURE_OPENAI_API_VERSION: &str = "v1";
pub const DEFAULT_AZURE_OPENAI_MODEL: &str = DEFAULT_OPENAI_MODEL;
pub const DEFAULT_BEDROCK_REGION: &str = "us-east-1";
pub const DEFAULT_BEDROCK_MODEL: &str = "anthropic.claude-haiku-4-5-20251001-v1:0";
pub const DEFAULT_OLLAMA_BASE_URL: &str = "http://localhost:11434/api";
pub const DEFAULT_OLLAMA_MODEL: &str = "qwen3-coder";
/// Synthetic model id for the in-process faux provider. The faux
/// provider does not consult model registries during a request — this
/// is purely a label that flows through cost/logging surfaces.
pub const DEFAULT_FAUX_MODEL: &str = "faux-1";

// Small-fast-model defaults per provider. Used for low-stakes background calls
// (compaction summaries, classifier prompts, auto-approver) where flagship
// quality is wasted spend. Anthropic Haiku 4.5 is ~15x cheaper than Opus 4.7
// per input token; comparable shape on OpenAI Nano vs flagship.
pub const ANTHROPIC_SMALL_FAST_MODEL: &str = "claude-haiku-4-5-20251001";
pub const OPENAI_SMALL_FAST_MODEL: &str = "gpt-5.4-nano";
pub const GOOGLE_SMALL_FAST_MODEL: &str = "gemini-2.5-flash-lite";
pub const BEDROCK_SMALL_FAST_MODEL: &str = "anthropic.claude-haiku-4-5-20251001-v1:0";
pub const AZURE_OPENAI_SMALL_FAST_MODEL: &str = OPENAI_SMALL_FAST_MODEL;
pub const OPENROUTER_SMALL_FAST_MODEL: &str = "anthropic/claude-haiku-4-5";
pub const VERCEL_SMALL_FAST_MODEL: &str = "anthropic/claude-haiku-4-5";
pub const PORTKEY_SMALL_FAST_MODEL: &str = "anthropic/claude-haiku-4-5";

/// Returns the built-in small-fast-model id for `provider`. `provider` is the
/// canonical short name (`provider_kind`/`LlmProvider::name`). Returns `None`
/// for providers that have no obvious cheaper tier (Ollama serves a single
/// local model; OpenAI-compatible light presets don't ship a curated cheap
/// model). Callers should treat `None` as "fall back to the parent model".
pub fn small_fast_model_for_provider(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" => Some(ANTHROPIC_SMALL_FAST_MODEL),
        "openai" => Some(OPENAI_SMALL_FAST_MODEL),
        "google" => Some(GOOGLE_SMALL_FAST_MODEL),
        "azure_openai" => Some(AZURE_OPENAI_SMALL_FAST_MODEL),
        "bedrock" => Some(BEDROCK_SMALL_FAST_MODEL),
        "openrouter" => Some(OPENROUTER_SMALL_FAST_MODEL),
        "vercel" => Some(VERCEL_SMALL_FAST_MODEL),
        "portkey" => Some(PORTKEY_SMALL_FAST_MODEL),
        _ => None,
    }
}

// OpenAI-compatible aggregators (full preset tier — curated models in models.json, dedicated costly test).
pub const DEFAULT_OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";
pub const DEFAULT_OPENROUTER_MODEL: &str = "anthropic/claude-opus-4-7";
pub const DEFAULT_VERCEL_AI_BASE_URL: &str = "https://ai-gateway.vercel.sh/v1";
pub const DEFAULT_VERCEL_AI_MODEL: &str = "anthropic/claude-opus-4-7";
pub const DEFAULT_PORTKEY_BASE_URL: &str = "https://api.portkey.ai/v1";
pub const DEFAULT_PORTKEY_MODEL: &str = "anthropic/claude-opus-4-7";
// OpenAI-compatible single-vendor (full preset tier).
pub const DEFAULT_GROQ_BASE_URL: &str = "https://api.groq.com/openai/v1";
pub const DEFAULT_GROQ_MODEL: &str = "llama-3.3-70b-versatile";
pub const DEFAULT_XAI_BASE_URL: &str = "https://api.x.ai/v1";
pub const DEFAULT_XAI_MODEL: &str = "grok-4";
pub const DEFAULT_DEEPSEEK_BASE_URL: &str = "https://api.deepseek.com/v1";
pub const DEFAULT_DEEPSEEK_MODEL: &str = "deepseek-chat";
// Google Cloud Vertex AI's OpenAI-compatible endpoint. The base URL is
// per-project + per-region, so users must set `vertex_project` and
// `vertex_location` (or override `base_url` directly).
pub const DEFAULT_VERTEX_LOCATION: &str = "us-central1";
pub const DEFAULT_VERTEX_MODEL: &str = "google/gemini-2.5-pro";
// OpenAI-compatible single-vendor (light preset tier — no curated models, no dedicated costly test).
pub const DEFAULT_MISTRAL_BASE_URL: &str = "https://api.mistral.ai/v1";
pub const DEFAULT_MISTRAL_MODEL: &str = "mistral-large-latest";
pub const DEFAULT_TOGETHER_BASE_URL: &str = "https://api.together.xyz/v1";
pub const DEFAULT_TOGETHER_MODEL: &str = "meta-llama/Llama-3.3-70B-Instruct-Turbo";
pub const DEFAULT_FIREWORKS_BASE_URL: &str = "https://api.fireworks.ai/inference/v1";
pub const DEFAULT_FIREWORKS_MODEL: &str = "accounts/fireworks/models/llama-v3p3-70b-instruct";
pub const DEFAULT_CEREBRAS_BASE_URL: &str = "https://api.cerebras.ai/v1";
pub const DEFAULT_CEREBRAS_MODEL: &str = "llama-3.3-70b";
pub const DEFAULT_DEEPINFRA_BASE_URL: &str = "https://api.deepinfra.com/v1/openai";
pub const DEFAULT_DEEPINFRA_MODEL: &str = "meta-llama/Meta-Llama-3.1-70B-Instruct";
pub const DEFAULT_BASETEN_BASE_URL: &str = "https://inference.baseten.co/v1";
pub const DEFAULT_BASETEN_MODEL: &str = "meta-llama/Meta-Llama-3.1-70B-Instruct";
// OpenAI-compatible local self-hosted (light preset tier — loopback default,
// auth optional, model id depends on whatever the local server has loaded).
pub const DEFAULT_LMSTUDIO_BASE_URL: &str = "http://127.0.0.1:1234/v1";
pub const DEFAULT_VLLM_BASE_URL: &str = "http://127.0.0.1:8000/v1";
pub const DEFAULT_LLAMACPP_BASE_URL: &str = "http://127.0.0.1:8080/v1";
// Cloudflare Workers AI + AI Gateway. Both base URLs are per-account
// (and the Gateway preset additionally per-gateway), so the default
// templates carry `{account_id}` / `{gateway_id}` placeholders that get
// substituted out of the matching `OpenAiCompatibleConfig` fields before
// requests fire. The substitution lives in the LLM client
// (`squeezy-llm::compatible`) so any user override of `base_url` that
// keeps the placeholder syntax behaves the same as the bundled default.
pub const DEFAULT_CLOUDFLARE_WORKERS_AI_BASE_URL: &str =
    "https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/v1";
pub const DEFAULT_CLOUDFLARE_AI_GATEWAY_BASE_URL: &str =
    "https://gateway.ai.cloudflare.com/v1/{account_id}/{gateway_id}/compat";
pub const DEFAULT_CLOUDFLARE_AI_GATEWAY_ID: &str = "default";
pub const DEFAULT_CLOUDFLARE_WORKERS_AI_MODEL: &str = "@cf/meta/llama-3.3-70b-instruct-fp8-fast";
pub const DEFAULT_CLOUDFLARE_AI_GATEWAY_MODEL: &str = "@cf/meta/llama-3.3-70b-instruct-fp8-fast";

/// Vertex AI's OpenAI-compatible chat completions endpoint lives behind a
/// regional URL that names the project. Returns the resolved base URL for a
/// `(project, location)` pair, ready for `/chat/completions` to be appended.
///
/// The `global` location lives at the bare host `aiplatform.googleapis.com`
/// (no `{location}-` prefix) because Google does not run a regional Anycast
/// frontend named `global`. Gemini 3.x is GA only via this location, so a
/// caller passing `location = "global"` builds the correct host instead of
/// a `https://global-aiplatform.googleapis.com/...` URL that DNS-fails.
/// Other locations (regional like `us-central1`, plus the continental
/// pseudo-regions `us`/`eu`) keep the historical `{location}-aiplatform`
/// shape so production deployments are unchanged.
pub fn vertex_base_url(project: &str, location: &str) -> String {
    let trimmed = location.trim();
    if trimmed.eq_ignore_ascii_case("global") {
        format!(
            "https://aiplatform.googleapis.com/v1/projects/{project}/locations/global/endpoints/openapi"
        )
    } else {
        format!(
            "https://{trimmed}-aiplatform.googleapis.com/v1/projects/{project}/locations/{trimmed}/endpoints/openapi"
        )
    }
}

/// Resolve a bare-name model alias (e.g. `opus`, `sonnet`, `haiku`) to the
/// provider-preferred full model ID, so `squeezy --model opus` resolves to
/// `claude-opus-4-7` on Anthropic (or `claude-sonnet-4-6` for `sonnet`)
/// instead of being sent verbatim and
/// 404-ing downstream. Lookup is case-insensitive on the alias. Returns
/// `None` for inputs that don't match any alias, in which case callers
/// should pass the string through unchanged (it's presumed to be a full
/// model ID).
pub fn resolve_model_alias(provider: &str, alias: &str) -> Option<&'static str> {
    let normalized = alias.trim().to_ascii_lowercase();
    match (provider, normalized.as_str()) {
        ("anthropic", "opus") => Some("claude-opus-4-7"),
        ("anthropic", "sonnet") => Some("claude-sonnet-4-6"),
        ("anthropic", "haiku") => Some("claude-haiku-4-5-20251001"),
        ("anthropic", "best") => Some("claude-opus-4-7"),
        ("openai" | "azure_openai", "opus") => Some(DEFAULT_OPENAI_MODEL),
        ("openai" | "azure_openai", "sonnet") => Some("gpt-5.4-mini"),
        ("openai" | "azure_openai", "haiku") => Some("gpt-5.4-nano"),
        ("openai" | "azure_openai", "best") => Some(DEFAULT_OPENAI_MODEL),
        ("bedrock", "opus" | "best") => Some(DEFAULT_BEDROCK_MODEL),
        ("bedrock", "sonnet") => Some(DEFAULT_BEDROCK_MODEL),
        ("bedrock", "haiku") => Some(DEFAULT_BEDROCK_MODEL),
        ("google", "opus" | "best") => Some(DEFAULT_GOOGLE_MODEL),
        ("google", "sonnet") => Some("gemini-2.5-flash"),
        ("google", "haiku") => Some("gemini-2.5-flash-lite"),
        _ => None,
    }
}

/// Cloudflare Workers AI's OpenAI-compatible chat completions endpoint is
/// per-account. Returns the resolved base URL for an `account_id`, ready for
/// `/chat/completions` to be appended. Built on top of the canonical
/// [`DEFAULT_CLOUDFLARE_WORKERS_AI_BASE_URL`] template so callers that read
/// the template directly (e.g. config inspectors) and callers that resolve
/// it eagerly stay in sync.
pub fn cloudflare_workers_ai_base_url(account_id: &str) -> String {
    DEFAULT_CLOUDFLARE_WORKERS_AI_BASE_URL.replace("{account_id}", account_id)
}

/// Cloudflare AI Gateway proxies any OpenAI-compatible upstream behind a
/// per-(account, gateway) URL. The `compat` suffix is the gateway's
/// OpenAI-format compatibility surface; underneath it can route to Workers AI,
/// OpenAI, Anthropic, etc. depending on the gateway's configured upstream.
pub fn cloudflare_ai_gateway_base_url(account_id: &str, gateway_id: &str) -> String {
    DEFAULT_CLOUDFLARE_AI_GATEWAY_BASE_URL
        .replace("{account_id}", account_id)
        .replace("{gateway_id}", gateway_id)
}

pub const MODEL_SELECTION_VERSION: u32 = 1;
pub const DEFAULT_EXA_MCP_URL: &str = "https://mcp.exa.ai/mcp";
pub const DEFAULT_EXA_API_KEY_ENV: &str = "EXA_API_KEY";
pub const DEFAULT_PARALLEL_MCP_URL: &str = "https://search.parallel.ai/mcp";
pub const DEFAULT_PARALLEL_API_KEY_ENV: &str = "PARALLEL_API_KEY";
pub const DEFAULT_WEBSEARCH_PROVIDER: &str = "exa";
pub const DEFAULT_MAX_OUTPUT_TOKENS: Option<u32> = None;
pub const DEFAULT_TOOL_SPILL_THRESHOLD_BYTES: usize = 25_000;
pub const DEFAULT_TOOL_PREVIEW_BYTES: usize = 2_000;
pub const DEFAULT_MAX_TOOL_RESULT_BYTES_PER_ROUND: usize = 50_000;
pub const DEFAULT_TOOL_OUTPUT_RETENTION_DAYS: u64 = 7;
pub const DEFAULT_MAX_PARALLEL_TOOLS: usize = 16;
// Per-turn aggregate budgets across every tool the agent runs in a
// single turn. These defaults are sized so they never bind in realistic
// use; users who want strict cost caps can set tighter values in
// `squeezy.toml`. Kept finite (rather than `u64::MAX`) so the inspect
// output remains TOML-roundtrippable.
pub const DEFAULT_MAX_TOOL_CALLS_PER_TURN: u64 = 10_000;
pub const DEFAULT_MAX_TOOL_BYTES_READ_PER_TURN: u64 = 1_000_000_000;
pub const DEFAULT_MAX_SEARCH_FILES_PER_TURN: u64 = 1_000_000;
pub const DEFAULT_STREAM_IDLE_TIMEOUT_MS: u64 = 300_000;
pub const DEFAULT_PROVIDER_REQUEST_MAX_RETRIES: u8 = 4;
pub const DEFAULT_PROVIDER_STREAM_MAX_RETRIES: u8 = 5;
pub const DEFAULT_PROVIDER_STREAM_IDLE_TIMEOUT_MS: u64 = 300_000;
/// Default idle timeout (ms) for sockets parked in the shared HTTP
/// pool before reqwest evicts them. Matches reqwest's own default so
/// the centralized factory preserves pre-F08 per-provider behavior
/// when the user has not set a custom `[transport]` knob.
pub const DEFAULT_PROVIDER_POOL_IDLE_TIMEOUT_MS: u64 = 90_000;
/// Default cap on idle TCP connections kept per origin in the shared
/// HTTP pool. `u32::MAX` is effectively unbounded (mirrors reqwest's
/// `usize::MAX` default) so existing per-provider workloads keep
/// reusing as many warmed sockets as they did before the dispatcher
/// was centralized.
pub const DEFAULT_PROVIDER_POOL_MAX_IDLE_PER_HOST: u32 = u32::MAX;
/// Hard ceiling (ms) on any inter-retry sleep — including a
/// server-supplied `Retry-After` / `Retry-After-Ms` hint. Sized at one
/// minute so honest throttle hints (typically seconds) pass through
/// untouched while a malicious or buggy upstream can't park the agent
/// for hours by claiming a multi-day cooldown.
pub const DEFAULT_PROVIDER_MAX_RETRY_DELAY_MS: u64 = 60_000;
pub const DEFAULT_COST_WARN_PERCENT: u8 = 85;
// Per-turn model routing ("cheap-model fast path") defaults. The router
// inspects the user prompt before each turn's first LLM request; on a
// match it dispatches that turn to the provider's small-fast tier
// (`small_fast_model_for_provider`). The mid-turn escalation detector
// hands the same turn back to the parent model when the cheap model
// stalls. See `crates/squeezy-agent/src/turn_router.rs` for the
// classifier and the escalation signals; see chapter 11 of the
// cost-saving docs for the rationale.
pub const DEFAULT_ROUTING_AUTO_CHEAP: bool = true;
pub const DEFAULT_ROUTING_AUTO_CHEAP_LLM_JUDGE: bool = true;
pub const DEFAULT_ROUTING_CHEAP_ESCALATION_ERROR_THRESHOLD: u8 = 2;
pub const DEFAULT_ROUTING_ESCALATION_STICKY_TURNS: u8 = 3;
pub const DEFAULT_ROUTING_BYPASS_FOR_IMAGES: bool = true;
/// Char-budget gate for the heuristic prefilter. Prompts longer than
/// this skip the slam-dunk path and fall through to the borderline
/// judge (or `Parent`). Sized at ~400 tokens of English at 5 chars/tok.
pub const DEFAULT_ROUTING_HEURISTIC_MAX_CHARS: u32 = 2_000;
/// Char-budget gate for the LLM judge. Prompts longer than this skip the
/// judge call and route to `Parent` directly — long prompts almost
/// always carry the kind of nuance the cheap tier struggles with, and a
/// long judge call would erode the savings the router is trying to
/// produce. Sized at ~1500 tokens of English.
pub const DEFAULT_ROUTING_JUDGE_MAX_CHARS: u32 = 6_000;
// Per-subagent-invocation budgets, sized so they never bind in
// realistic use; the subagent's natural exit is the model emitting a
// final answer with no tool calls.
pub const DEFAULT_SUBAGENT_MAX_TOOL_CALLS_PER_CALL: u64 = 10_000;
pub const DEFAULT_SUBAGENT_MAX_TOOL_BYTES_READ_PER_CALL: u64 = 100_000_000;
pub const DEFAULT_SUBAGENT_MAX_SEARCH_FILES_PER_CALL: u64 = 50_000;
/// Maximum number of subagents that may be active at once for a single
/// parent Agent. The registry rejects further `start()` calls until an
/// in-flight subagent finishes (lease drops). Keeps fanout flat and
/// predictable rather than letting a model spawn an unbounded swarm.
pub const DEFAULT_SUBAGENT_MAX_CONCURRENT: usize = 20;
// Emergency belt on subagent model rounds. Plan/Delegate/Review
// subagents run full agent work, sized to match what real long-running
// agent sessions reach in practice. The cost broker, cancellation
// token, and per-tool-call truncations are the load-bearing safeguards;
// this is the last-resort belt.
pub const DEFAULT_SUBAGENT_MAX_MODEL_ROUNDS: usize = 1_000;
// Wall-clock ceiling for a single subagent run. None of the per-call
// budgets (tool calls, bytes, model rounds, summary tokens) measure elapsed
// time, so a slow model stream or a chain of slow tool calls can pin the
// parent indefinitely without ever tripping them. 300s sits well above the
// median Explore/Plan/Review run while still guaranteeing the parent's turn
// loop reclaims control on the order of minutes. Set to `0` in TOML or
// `SQUEEZY_SUBAGENT_MAX_RUNTIME_SECS=0` to disable.
pub const DEFAULT_SUBAGENT_MAX_RUNTIME_SECS: u64 = 900;
// Generous default sized for Plan/Delegate/Review summaries under a
// reasoning model: thinking tokens burn first, then the actual summary.
// 64K leaves room for both across every model we ship a preset for. The
// OpenAI Responses API surfaces an under-budget run as
// `response.incomplete: max_output_tokens` (a hard error in our SSE parser),
// so the failure mode of being too tight is loud, not silent. Used only
// as a fallback when the parent agent has no explicit max_output_tokens.
pub const DEFAULT_SUBAGENT_MAX_SUMMARY_TOKENS: u32 = 64_000;
/// Floor for the DocHelp subagent's output budget when the parent does not
/// cap `max_output_tokens`. DocHelp's "summary" is the user-visible answer
/// (not a synopsis of a tool-driven exploration), so it gets a much higher
/// floor than other subagent kinds.
pub const DEFAULT_DOC_HELP_MAX_OUTPUT_TOKENS: u32 = 32_000;
pub const DEFAULT_TICK_RATE_MS: u64 = 50;
pub const DEFAULT_TELEMETRY_ENDPOINT: &str =
    "https://squeezy-telemetry.esqueezy.workers.dev/v1/batch";
pub const DEFAULT_FEEDBACK_ENDPOINT: &str =
    "https://squeezy-telemetry.esqueezy.workers.dev/v1/feedback";
pub const DEFAULT_REPORT_ENDPOINT: &str =
    "https://squeezy-telemetry.esqueezy.workers.dev/v1/report";
pub const DEFAULT_FEEDBACK_MAX_BYTES: usize = 16 * 1024;
pub const DEFAULT_REPORT_MAX_BYTES: usize = 2 * 1024 * 1024;
pub const PROJECT_SETTINGS_FILE: &str = "squeezy.toml";
pub const DEFAULT_SQUEEZY_SKILLS_DIR: &str = ".squeezy/skills";
pub const DEFAULT_SESSION_LOG_RETENTION_DAYS: u64 = 30;
/// Days an archived session lives before the retention sweep permanently
/// deletes it. Live sessions hit `log_retention_days` first, then move to
/// `archived/<id>/`, where they linger for this long. Matches the live
/// default so an idle workspace keeps roughly 60 days of recoverable
/// history (30 live + 30 archived) without manual intervention.
pub const DEFAULT_SESSION_LOG_RETENTION_ARCHIVE_DAYS: u64 = 30;
pub const DEFAULT_SESSION_MAX_EVENT_BYTES: usize = 131_072;
pub const DEFAULT_SESSION_MAX_SESSION_BYTES: usize = 52_428_800;
pub const DEFAULT_CONTEXT_ATTACHMENT_MAX_BYTES: usize = 1_048_576;
// Absolute fallback for the per-turn compaction trigger when
// `model_context_window` is not set in `squeezy.toml`. Modern models
// run with 128k+ context windows; the percent-of-context path (~90%)
// is the right shape and takes over once the window is auto-derived
// from `model_info_for`. This fallback is only the safety net for the
// unknown-model case.
pub const DEFAULT_CONTEXT_COMPACTION_ESTIMATED_TOKENS: u64 = 60_000;
pub const DEFAULT_CONTEXT_COMPACTION_MIN_ITEMS: usize = 16;
pub const DEFAULT_CONTEXT_COMPACTION_RECENT_ITEMS: usize = 10;
pub const DEFAULT_CONTEXT_COMPACTION_MAX_SUMMARY_BYTES: usize = 12_000;
pub const DEFAULT_CONTEXT_REPO_DOC_MAX_BYTES: usize = 32_768;
pub const DEFAULT_CONTEXT_USER_MEMORY_MAX_BYTES: usize = 16_384;
/// Trigger mid-turn compaction once the provider-reported total token usage
/// reaches this fraction of `model_context_window` (out of 100).
pub const DEFAULT_CONTEXT_COMPACTION_THRESHOLD_PERCENT: u8 = 80;
/// Max output tokens to request when the model-assisted compaction strategy
/// is active.
pub const DEFAULT_CONTEXT_COMPACTION_MODEL_ASSISTED_MAX_OUTPUT_TOKENS: u32 = 1_500;
/// Timeout for a single model-assisted compaction round-trip. On expiry the
/// pipeline falls back to the extractive summary.
pub const DEFAULT_CONTEXT_COMPACTION_MODEL_ASSISTED_TIMEOUT_SECS: u64 = 30;
/// When strategy = LayeredFallback, model-assist only kicks in once the
/// dropped slice exceeds this many tokens; smaller slices stay extractive.
pub const DEFAULT_CONTEXT_COMPACTION_LAYERED_FALLBACK_EXTRACTIVE_THRESHOLD_TOKENS: u32 = 4_000;
/// Mid-tier micro-compaction fires below the full-compaction threshold so
/// the heavy tool-result bodies are reclaimed before the all-or-nothing
/// summary head replaces the older slice. 60% leaves a 20-percentage-point
/// band between micro and the 80% full-compaction default, which is the
/// span where local tool clearing has the highest leverage (one large
/// `read_file` or `shell` output dwarfs a few text messages).
pub const DEFAULT_CONTEXT_MICRO_COMPACTION_THRESHOLD_PERCENT: u8 = 60;
/// Keep this many newest compactable tool results verbatim during
/// micro-compaction; older results are rewritten to a placeholder. 5
/// is enough to resolve typical "what did the last read of foo.rs
/// show?" follow-ups without forcing a re-read.
pub const DEFAULT_CONTEXT_MICRO_COMPACTION_KEEP_RECENT: usize = 5;
pub const DEFAULT_AGENT_COMPAT_SKILLS_DIR: &str = ".agents/skills";
/// Tools whose full JSON schema is always sent up-front in every request,
/// independent of `[tools].lazy_schema_loading`.
///
/// These are the cheap-and-likely-needed-every-turn tools: bounded file
/// reads/writes, structured patching, search, shell, and graph-backed navigation. Heavyweight
/// or rarely-used tools (e.g. `verify`, `webfetch`, `websearch`) are
/// intentionally **not** in this list so they only cost prompt bytes once
/// the model explicitly attaches them via `load_tool_schema`.
///
/// `load_tool_schema` is not duplicated here on purpose: it is forced into the
/// request `tools` array by name in `squeezy_agent::request_tool_specs`, and
/// `squeezy_agent::tool_is_core_schema` treats it as always-core. Listing it
/// in two places risks future skew if one site is updated without the other.
///
/// `update_task_state` is intentionally omitted from model-visible schemas.
/// The runtime derives visible progress from turn/tool lifecycle events.
pub const DEFAULT_CORE_TOOL_NAMES: &[&str] = &[
    "glob",
    "grep",
    "read_file",
    "read_tool_output",
    "write_file",
    "apply_patch",
    "shell",
    "decl_search",
    "definition_search",
    "diff_context",
    "downstream_flow",
    "hierarchy",
    "plan_patch",
    "read_slice",
    "reference_search",
    "repo_map",
    "symbol_context",
    "upstream_flow",
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppConfig {
    pub provider: ProviderConfig,
    pub model: String,
    /// Cheap model id used for low-stakes background calls: compaction
    /// summary, AI reviewer classifier, auto-approver. `None` falls back to
    /// `small_fast_model_for_provider(provider)`, then to `model` if the
    /// provider has no curated cheap tier (e.g. Ollama). Configured via
    /// `[model].small_fast_model` in TOML or `SQUEEZY_SMALL_FAST_MODEL`.
    pub small_fast_model: Option<String>,
    pub profile: ModelProfile,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub instructions: String,
    pub max_output_tokens: Option<u32>,
    /// Forwarded as `tool_choice` to providers when tools are advertised.
    /// `None` omits the field; providers default to `auto`. Set to
    /// `"required"` to force a tool call every turn — needed for
    /// chat-completions models that ignore `auto` (Qwen via OpenRouter,
    /// smaller MoEs). Configured via `[model].tool_choice` in TOML or
    /// `SQUEEZY_TOOL_CHOICE` env var.
    pub tool_choice: Option<String>,
    pub stream_idle_timeout: Duration,
    pub tick_rate: Duration,
    pub workspace_root: PathBuf,
    pub permissions: PermissionPolicy,
    pub session_mode: SessionMode,
    #[serde(default)]
    pub session_resume_picker: SessionResumePicker,
    pub session_logs: SessionLogConfig,
    pub context_compaction: ContextCompactionConfig,
    pub subagents: SubagentConfig,
    pub store_responses: bool,
    pub exploration_compiler: bool,
    pub max_parallel_tools: usize,
    pub tool_spill_threshold_bytes: usize,
    pub tool_preview_bytes: usize,
    pub max_tool_result_bytes_per_round: usize,
    pub tool_output_retention_days: u64,
    pub exa_mcp_url: String,
    pub exa_api_key_env: String,
    pub parallel_mcp_url: String,
    pub parallel_api_key_env: String,
    /// Websearch backend identifier; expected values are "exa" or "parallel".
    /// Stored as a string so squeezy-core has no dependency on squeezy-tools.
    pub websearch_provider: String,
    pub max_tool_calls_per_turn: u64,
    pub max_tool_bytes_read_per_turn: u64,
    pub max_search_files_per_turn: u64,
    pub max_session_cost_usd_micros: Option<u64>,
    pub cost_warn_percent: u8,
    pub routing: RoutingConfig,
    pub telemetry: TelemetryConfig,
    pub feedback: FeedbackConfig,
    pub redaction: RedactionConfig,
    pub skills: SkillsConfig,
    pub graph: GraphConfig,
    pub cache: CacheConfig,
    pub tools: ToolSchemaConfig,
    pub checkpoints_enabled: bool,
    pub tui: TuiConfig,
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
    pub hardening: HardeningConfig,
    pub config_sources: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_warnings: Vec<ConfigWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigWarning {
    pub source: String,
    pub field: String,
}

impl AppConfig {
    pub fn from_env() -> Self {
        Self::from_env_vars(None, |name| env::var(name).ok())
    }

    pub fn from_env_and_settings() -> Result<Self> {
        Self::from_default_paths_and_env_with_provider_value(None, None)
    }

    pub fn from_env_and_settings_with_provider(provider: &str) -> Result<Self> {
        Self::from_default_paths_and_env_with_provider_value(Some(provider), None)
    }

    /// Like `from_env_and_settings`, but applies the named TOML profile
    /// (`[profiles.<name>]`) on top of the merged base settings before the
    /// env-var layer is applied. Errors if the profile is not configured.
    pub fn from_env_and_settings_with_profile(
        provider: Option<&str>,
        profile: Option<&str>,
    ) -> Result<Self> {
        Self::from_default_paths_and_env_with_provider_value(provider, profile)
    }

    pub fn from_settings_path_and_env(path: PathBuf) -> Result<Self> {
        let (settings, sources, warnings) = SettingsFile::load_optional_source(&path, "settings")?;
        Self::try_from_settings_and_env_vars_with_sources_and_warnings(
            settings,
            sources,
            warnings,
            None,
            |name| env::var(name).ok(),
        )
    }

    pub fn from_settings_path_and_env_with_provider(path: PathBuf, provider: &str) -> Result<Self> {
        let (settings, sources, warnings) = SettingsFile::load_optional_source(&path, "settings")?;
        Self::try_from_settings_and_env_vars_with_sources_and_warnings(
            settings,
            sources,
            warnings,
            Some(provider),
            |name| env::var(name).ok(),
        )
    }

    pub fn from_env_with_provider(provider: &str) -> Self {
        Self::from_env_vars(Some(provider), |name| env::var(name).ok())
    }

    fn from_env_vars(
        cli_provider: Option<&str>,
        mut var: impl FnMut(&str) -> Option<String>,
    ) -> Self {
        Self::try_from_settings_and_env_vars(SettingsFile::default(), cli_provider, &mut var)
            .unwrap_or_else(|error| {
                // Surfaces in real runs through tracing; tests have no subscriber
                // so they fall back silently the way they always did.
                tracing::warn!(
                    target: "squeezy_core::config",
                    %error,
                    "config resolution failed; falling back to built-in defaults",
                );
                Self::built_in_defaults()
            })
    }

    #[cfg(test)]
    fn from_settings_and_env_vars(
        settings: SettingsFile,
        mut var: impl FnMut(&str) -> Option<String>,
    ) -> Self {
        Self::try_from_settings_and_env_vars(settings, None, &mut var)
            .unwrap_or_else(|_| Self::built_in_defaults())
    }

    fn try_from_settings_and_env_vars(
        settings: SettingsFile,
        cli_provider: Option<&str>,
        var: impl FnMut(&str) -> Option<String>,
    ) -> Result<Self> {
        Self::try_from_settings_and_env_vars_with_sources(
            settings,
            vec!["defaults".to_string()],
            cli_provider,
            var,
        )
    }

    fn try_from_settings_and_env_vars_with_sources(
        settings: SettingsFile,
        sources: Vec<String>,
        cli_provider: Option<&str>,
        var: impl FnMut(&str) -> Option<String>,
    ) -> Result<Self> {
        Self::try_from_settings_and_env_vars_with_sources_and_warnings(
            settings,
            sources,
            Vec::new(),
            cli_provider,
            var,
        )
    }

    fn try_from_settings_and_env_vars_with_sources_and_warnings(
        settings: SettingsFile,
        mut sources: Vec<String>,
        config_warnings: Vec<ConfigWarning>,
        cli_provider: Option<&str>,
        mut var: impl FnMut(&str) -> Option<String>,
    ) -> Result<Self> {
        let workspace_root = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let mut env_used = false;
        let mut get_var = |name: &str| {
            let value = var(name);
            if value.is_some() {
                env_used = true;
            }
            value
        };

        let model_settings = settings.model_settings.clone().unwrap_or_default();
        let env_provider = get_var("SQUEEZY_PROVIDER");
        let provider_name = cli_provider
            .map(str::to_string)
            .or(env_provider)
            .or(model_settings.provider)
            .or(settings.provider.clone())
            .unwrap_or_else(|| "openai".to_string())
            .trim()
            .to_ascii_lowercase();
        let providers = settings.providers.unwrap_or_default();
        let provider = match provider_name.as_str() {
            "anthropic" | "claude" => ProviderConfig::Anthropic(AnthropicConfig {
                api_key_env: get_var("ANTHROPIC_API_KEY_ENV")
                    .or_else(|| provider_setting(&providers, "anthropic", "api_key_env"))
                    .unwrap_or_else(|| "SQUEEZY_ANTHROPIC_KEY".to_string()),
                api_key: provider_setting(&providers, "anthropic", "api_key"),
                base_url: get_var("ANTHROPIC_BASE_URL")
                    .or_else(|| provider_setting(&providers, "anthropic", "base_url"))
                    .unwrap_or_else(|| DEFAULT_ANTHROPIC_BASE_URL.to_string()),
                transport: provider_transport_settings(&providers, &["anthropic"]),
            }),
            "google" | "gemini" => ProviderConfig::Google(GoogleConfig {
                api_key_env: get_var("GOOGLE_API_KEY_ENV")
                    .or_else(|| provider_setting(&providers, "google", "api_key_env"))
                    .unwrap_or_else(|| "SQUEEZY_GOOGLE_KEY".to_string()),
                api_key: provider_setting(&providers, "google", "api_key"),
                base_url: get_var("GOOGLE_BASE_URL")
                    .or_else(|| provider_setting(&providers, "google", "base_url"))
                    .unwrap_or_else(|| DEFAULT_GOOGLE_BASE_URL.to_string()),
                transport: provider_transport_settings(&providers, &["google"]),
            }),
            "azure" | "azure-openai" | "azure_openai" => {
                // Entra ID opt-in is layered: either an explicit
                // `use_entra_id = true` in TOML, or the operator pre-populated
                // `AZURE_OPENAI_BEARER_TOKEN` and wants squeezy to honor it
                // without separately flipping the bool. The presence of a
                // bearer in the env is sufficient signal: callers that
                // truly want the api-key path leave that env var unset.
                let entra_bearer_token = get_var("AZURE_OPENAI_BEARER_TOKEN")
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty());
                let use_entra_id = provider_setting_bool_any(
                    &providers,
                    &["azure_openai", "azure"],
                    "use_entra_id",
                )
                .unwrap_or(false)
                    || entra_bearer_token.is_some();
                ProviderConfig::AzureOpenAi(AzureOpenAiConfig {
                    api_key_env: get_var("AZURE_OPENAI_API_KEY_ENV")
                        .or_else(|| provider_setting(&providers, "azure_openai", "api_key_env"))
                        .or_else(|| provider_setting(&providers, "azure", "api_key_env"))
                        .unwrap_or_else(|| "SQUEEZY_AZURE_OPENAI_KEY".to_string()),
                    api_key: provider_setting(&providers, "azure_openai", "api_key")
                        .or_else(|| provider_setting(&providers, "azure", "api_key")),
                    base_url: get_var("AZURE_OPENAI_BASE_URL")
                        .or_else(|| provider_setting(&providers, "azure_openai", "base_url"))
                        .or_else(|| provider_setting(&providers, "azure", "base_url"))
                        .unwrap_or_else(|| DEFAULT_AZURE_OPENAI_BASE_URL.to_string()),
                    api_version: get_var("AZURE_OPENAI_API_VERSION")
                        .or_else(|| provider_setting(&providers, "azure_openai", "api_version"))
                        .or_else(|| provider_setting(&providers, "azure", "api_version"))
                        .unwrap_or_else(|| DEFAULT_AZURE_OPENAI_API_VERSION.to_string()),
                    deployment_name_map: provider_setting_deployment_name_map(
                        &providers,
                        &["azure_openai", "azure"],
                    ),
                    extra_headers: provider_setting_headers_any(
                        &providers,
                        &["azure_openai", "azure"],
                    )
                    .unwrap_or_default(),
                    use_entra_id,
                    entra_bearer_token,
                    transport: provider_transport_settings(&providers, &["azure_openai", "azure"]),
                })
            }
            "bedrock" | "amazon-bedrock" | "amazon_bedrock" => {
                ProviderConfig::Bedrock(BedrockConfig {
                    region: get_var("AWS_REGION")
                        .or_else(|| get_var("AWS_DEFAULT_REGION"))
                        .or_else(|| provider_setting(&providers, "bedrock", "region"))
                        .unwrap_or_else(|| DEFAULT_BEDROCK_REGION.to_string()),
                    base_url: get_var("BEDROCK_BASE_URL")
                        .or_else(|| provider_setting(&providers, "bedrock", "base_url")),
                    // Pick up `AWS_BEARER_TOKEN_BEDROCK` exactly like
                    // boto3 / aws-sdk-js do; an empty string is treated
                    // as "unset" so a shell that exports the var but
                    // leaves it blank falls through to the default
                    // credential chain instead of failing with
                    // "empty bearer token".
                    bearer_token: get_var("AWS_BEARER_TOKEN_BEDROCK")
                        .map(|value| value.trim().to_string())
                        .filter(|value| !value.is_empty()),
                    request_metadata: provider_setting_request_metadata(&providers, "bedrock")
                        .unwrap_or_default(),
                    transport: provider_transport_settings(&providers, &["bedrock"]),
                })
            }
            "ollama" | "local" => ProviderConfig::Ollama(OllamaConfig {
                base_url: get_var("OLLAMA_BASE_URL")
                    .or_else(|| provider_setting(&providers, "ollama", "base_url"))
                    .unwrap_or_else(|| DEFAULT_OLLAMA_BASE_URL.to_string()),
                route_style: get_var("OLLAMA_ROUTE_STYLE")
                    .or_else(|| provider_setting(&providers, "ollama", "route_style"))
                    .as_deref()
                    .and_then(OllamaRoute::parse)
                    .unwrap_or_default(),
                transport: provider_transport_settings(&providers, &["ollama"]),
            }),
            "openai" => ProviderConfig::OpenAi(OpenAiConfig {
                api_key_env: get_var("OPENAI_API_KEY_ENV")
                    .or_else(|| provider_setting(&providers, "openai", "api_key_env"))
                    .unwrap_or_else(|| "SQUEEZY_OPENAI_KEY".to_string()),
                api_key: provider_setting(&providers, "openai", "api_key"),
                base_url: get_var("OPENAI_BASE_URL")
                    .or_else(|| provider_setting(&providers, "openai", "base_url"))
                    .unwrap_or_else(|| DEFAULT_OPENAI_BASE_URL.to_string()),
                // OPENAI_ORG_ID is the canonical env name in OpenAI's own
                // SDKs; OPENAI_ORGANIZATION is the long-form alias they
                // accept too. Honor both so users porting from other
                // tooling don't get surprised.
                organization: get_var("OPENAI_ORG_ID")
                    .or_else(|| get_var("OPENAI_ORGANIZATION"))
                    .or_else(|| provider_setting(&providers, "openai", "organization"))
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty()),
                project: get_var("OPENAI_PROJECT_ID")
                    .or_else(|| get_var("OPENAI_PROJECT"))
                    .or_else(|| provider_setting(&providers, "openai", "project"))
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty()),
                service_tier: get_var("OPENAI_SERVICE_TIER")
                    .or_else(|| provider_setting(&providers, "openai", "service_tier"))
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty()),
                transport: provider_transport_settings(&providers, &["openai"]),
            }),
            "openai-codex" | "openai_codex" | "chatgpt" => {
                ProviderConfig::OpenAiCodex(OpenAiCodexConfig {
                    base_url: get_var("OPENAI_CODEX_BASE_URL")
                        .or_else(|| provider_setting(&providers, "openai_codex", "base_url"))
                        .unwrap_or_else(|| DEFAULT_OPENAI_CODEX_BASE_URL.to_string()),
                    originator: get_var("OPENAI_CODEX_ORIGINATOR")
                        .or_else(|| provider_setting(&providers, "openai_codex", "originator"))
                        .unwrap_or_else(|| DEFAULT_OPENAI_CODEX_ORIGINATOR.to_string()),
                    transport: provider_transport_settings(&providers, &["openai_codex"]),
                })
            }
            "faux" | "mock" => ProviderConfig::Faux(FauxConfig {
                script: get_var("SQUEEZY_FAUX_SCRIPT")
                    .or_else(|| provider_setting(&providers, "faux", "script")),
                name: None,
                transport: provider_transport_settings(&providers, &["faux"]),
            }),
            other if OpenAiCompatiblePreset::parse(other).is_some() => {
                let preset =
                    OpenAiCompatiblePreset::parse(other).expect("guarded by match condition");
                build_openai_compatible_config(preset, &providers, &mut get_var)?
            }
            unknown => {
                return Err(SqueezyError::Config(format!(
                    "model.provider: unknown provider {unknown:?}"
                )));
            }
        };
        validate_provider_base_urls(&provider)?;
        let default_model = match &provider {
            ProviderConfig::OpenAi(_) => provider_setting(&providers, "openai", "default_model")
                .unwrap_or_else(|| DEFAULT_OPENAI_MODEL.to_string()),
            ProviderConfig::Anthropic(_) => {
                provider_setting(&providers, "anthropic", "default_model")
                    .unwrap_or_else(|| DEFAULT_ANTHROPIC_MODEL.to_string())
            }
            ProviderConfig::Google(_) => provider_setting(&providers, "google", "default_model")
                .unwrap_or_else(|| DEFAULT_GOOGLE_MODEL.to_string()),
            ProviderConfig::AzureOpenAi(_) => {
                provider_setting(&providers, "azure_openai", "default_model")
                    .or_else(|| provider_setting(&providers, "azure", "default_model"))
                    .unwrap_or_else(|| DEFAULT_AZURE_OPENAI_MODEL.to_string())
            }
            ProviderConfig::Bedrock(_) => provider_setting(&providers, "bedrock", "default_model")
                .unwrap_or_else(|| DEFAULT_BEDROCK_MODEL.to_string()),
            ProviderConfig::Ollama(_) => provider_setting(&providers, "ollama", "default_model")
                .unwrap_or_else(|| DEFAULT_OLLAMA_MODEL.to_string()),
            ProviderConfig::OpenAiCodex(_) => {
                provider_setting(&providers, "openai_codex", "default_model")
                    .unwrap_or_else(|| DEFAULT_OPENAI_CODEX_MODEL.to_string())
            }
            ProviderConfig::OpenAiCompatible(config) => {
                provider_setting(&providers, config.preset.as_str(), "default_model")
                    .unwrap_or_else(|| config.preset.default_model().to_string())
            }
            ProviderConfig::Faux(_) => provider_setting(&providers, "faux", "default_model")
                .unwrap_or_else(|| DEFAULT_FAUX_MODEL.to_string()),
        };
        let profile = get_var("SQUEEZY_PROFILE")
            .or(model_settings.profile)
            .or(settings.profile)
            .as_deref()
            .and_then(ModelProfile::parse)
            .unwrap_or_default();
        let raw_model = get_var("SQUEEZY_MODEL")
            .or(model_settings.model)
            .or(settings.model)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(default_model);
        let small_fast_model = get_var("SQUEEZY_SMALL_FAST_MODEL")
            .or(model_settings.small_fast_model.clone())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let provider_slug = match &provider {
            ProviderConfig::OpenAi(_) => "openai",
            ProviderConfig::Anthropic(_) => "anthropic",
            ProviderConfig::Google(_) => "google",
            ProviderConfig::AzureOpenAi(_) => "azure_openai",
            ProviderConfig::Bedrock(_) => "bedrock",
            ProviderConfig::Ollama(_) => "ollama",
            ProviderConfig::OpenAiCodex(_) => "openai_codex",
            ProviderConfig::OpenAiCompatible(_) => "",
            ProviderConfig::Faux(_) => "faux",
        };
        let model = resolve_model_alias(provider_slug, &raw_model)
            .map(str::to_string)
            .unwrap_or(raw_model);
        let reasoning_effort = model_settings.reasoning_effort;
        let max_output_tokens = get_var("SQUEEZY_MAX_OUTPUT_TOKENS")
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|value| *value > 0)
            .or(model_settings.max_output_tokens)
            .or(DEFAULT_MAX_OUTPUT_TOKENS);
        let tool_choice = get_var("SQUEEZY_TOOL_CHOICE")
            .map(|raw| raw.trim().to_string())
            .filter(|value| !value.is_empty())
            .or(model_settings.tool_choice.clone());
        let provider_timeout_keys = provider_settings_keys(&provider);
        let stream_idle_timeout_ms = parse_u64(
            get_var("SQUEEZY_STREAM_IDLE_TIMEOUT_MS")
                .or_else(|| {
                    model_settings
                        .stream_idle_timeout_ms
                        .map(|value| value.to_string())
                })
                .or_else(|| {
                    provider_u64_setting_any(
                        &providers,
                        provider_timeout_keys,
                        "stream_idle_timeout_ms",
                    )
                }),
            DEFAULT_STREAM_IDLE_TIMEOUT_MS,
        );
        let web = settings.web.unwrap_or_default();
        let exa_mcp_url = get_var("SQUEEZY_EXA_MCP_URL")
            .or(web.exa_mcp_url)
            .unwrap_or_else(|| DEFAULT_EXA_MCP_URL.to_string());
        let exa_api_key_env = get_var("SQUEEZY_EXA_API_KEY_ENV")
            .or(web.exa_api_key_env)
            .unwrap_or_else(|| DEFAULT_EXA_API_KEY_ENV.to_string());
        let parallel_mcp_url = get_var("SQUEEZY_PARALLEL_MCP_URL")
            .or(web.parallel_mcp_url)
            .unwrap_or_else(|| DEFAULT_PARALLEL_MCP_URL.to_string());
        let parallel_api_key_env = get_var("SQUEEZY_PARALLEL_API_KEY_ENV")
            .or(web.parallel_api_key_env)
            .unwrap_or_else(|| DEFAULT_PARALLEL_API_KEY_ENV.to_string());
        let websearch_provider = get_var("SQUEEZY_WEBSEARCH_PROVIDER")
            .or(web.websearch_provider)
            .unwrap_or_else(|| DEFAULT_WEBSEARCH_PROVIDER.to_string());
        let requested_store_responses = get_var("SQUEEZY_STORE_RESPONSES")
            .as_deref()
            .map(parse_enabled_bool)
            .unwrap_or(model_settings.store_responses.unwrap_or(false));
        let store_responses = requested_store_responses
            && matches!(
                provider,
                ProviderConfig::OpenAi(_) | ProviderConfig::AzureOpenAi(_)
            );
        let agent_settings = settings.agent.unwrap_or_default();
        // The exploration compiler defaults to on, and the documented env-var
        // override is `SQUEEZY_EXPLORATION_COMPILER=off|false|...`. Treating
        // the variable as a disable-only override keeps the documented values
        // working without silently flipping the default off on typos or empty
        // strings, matching how `SQUEEZY_TELEMETRY` and `SQUEEZY_FEEDBACK`
        // handle their own default-on flags.
        let settings_exploration_compiler = agent_settings.exploration_compiler.unwrap_or(true);
        let exploration_compiler_var = get_var("SQUEEZY_EXPLORATION_COMPILER");
        let exploration_compiler = if parse_disabled_bool(exploration_compiler_var.as_deref()) {
            false
        } else {
            settings_exploration_compiler
        };
        let budgets = settings.budgets.unwrap_or_default();
        let max_parallel_tools = get_var("SQUEEZY_MAX_PARALLEL_TOOLS")
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .or(budgets.max_parallel_tools)
            .unwrap_or(DEFAULT_MAX_PARALLEL_TOOLS);
        let tool_spill_threshold_bytes = parse_usize(
            get_var("SQUEEZY_TOOL_SPILL_THRESHOLD_BYTES"),
            budgets
                .tool_spill_threshold_bytes
                .unwrap_or(DEFAULT_TOOL_SPILL_THRESHOLD_BYTES),
        );
        let tool_preview_bytes = parse_usize(
            get_var("SQUEEZY_TOOL_PREVIEW_BYTES"),
            budgets
                .tool_preview_bytes
                .unwrap_or(DEFAULT_TOOL_PREVIEW_BYTES),
        );
        let max_tool_result_bytes_per_round = parse_usize(
            get_var("SQUEEZY_MAX_TOOL_RESULT_BYTES_PER_ROUND"),
            budgets
                .max_tool_result_bytes_per_round
                .unwrap_or(DEFAULT_MAX_TOOL_RESULT_BYTES_PER_ROUND),
        );
        let tool_output_retention_days = get_var("SQUEEZY_TOOL_OUTPUT_RETENTION_DAYS")
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .or(budgets.tool_output_retention_days)
            .unwrap_or(DEFAULT_TOOL_OUTPUT_RETENTION_DAYS);
        let max_tool_calls_per_turn = parse_u64(
            get_var("SQUEEZY_MAX_TOOL_CALLS_PER_TURN"),
            budgets
                .max_tool_calls_per_turn
                .unwrap_or(DEFAULT_MAX_TOOL_CALLS_PER_TURN),
        );
        let max_tool_bytes_read_per_turn = parse_u64(
            get_var("SQUEEZY_MAX_TOOL_BYTES_READ_PER_TURN"),
            budgets
                .max_tool_bytes_read_per_turn
                .unwrap_or(DEFAULT_MAX_TOOL_BYTES_READ_PER_TURN),
        );
        let max_search_files_per_turn = parse_u64(
            get_var("SQUEEZY_MAX_SEARCH_FILES_PER_TURN"),
            budgets
                .max_search_files_per_turn
                .unwrap_or(DEFAULT_MAX_SEARCH_FILES_PER_TURN),
        );
        let max_session_cost_usd_micros = get_var("SQUEEZY_MAX_SESSION_COST_USD_MICROS")
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .or(budgets.max_session_cost_usd_micros);
        let cost_warn_percent = get_var("SQUEEZY_COST_WARN_PERCENT")
            .and_then(|value| value.parse::<u8>().ok())
            .filter(|value| (1..=100).contains(value))
            .or(budgets.cost_warn_percent)
            .unwrap_or(DEFAULT_COST_WARN_PERCENT);
        let routing = RoutingConfig::from_settings_and_env(
            settings.routing.unwrap_or_default(),
            &mut get_var,
        );
        let telemetry = TelemetryConfig::from_settings_and_env(
            settings.telemetry.unwrap_or_default(),
            &mut get_var,
        );
        let feedback = FeedbackConfig::from_settings_and_env(
            settings.feedback.unwrap_or_default(),
            &mut get_var,
        );
        let redaction = RedactionConfig::from_settings(settings.redaction.unwrap_or_default())?;
        let mcp_servers = settings.mcp.map(|mcp| mcp.servers).unwrap_or_default();
        let mut permission_settings = settings.permissions.unwrap_or_default();
        // Insert MCP-derived rules *before* the user's explicit
        // `[[permissions.rules]]`. Permission matching is "last rule wins",
        // so this keeps any deliberate user deny/allow as the final word
        // and prevents an MCP server's own permission block from silently
        // overriding admin policy.
        let mut combined_rules = mcp_permission_rules(&mcp_servers);
        combined_rules.append(&mut permission_settings.rules);
        permission_settings.rules = combined_rules;
        let permissions = PermissionPolicy::from_settings_and_env(
            permission_settings,
            &sources.join(","),
            &workspace_root,
            &mut get_var,
        )?;
        let mut session_settings = settings.session.unwrap_or_default();
        let session_mode = parse_session_mode(
            get_var("SQUEEZY_SESSION_MODE"),
            session_settings.mode.unwrap_or_default(),
        );
        // `SQUEEZY_SESSION_DIR` overrides `[session].log_dir` so operators can
        // redirect session traces (CI runners, ephemeral sandboxes, multi-user
        // hosts) without rewriting settings.toml. Whitespace-only values are
        // treated as unset so a stray `export SQUEEZY_SESSION_DIR=` cannot
        // silently clear a configured directory. The CLI `--session-dir`
        // overlay in `squeezy-cli` mutates `AppConfig.session_logs.log_dir`
        // directly on top of this resolved value, giving the final order
        // flag > env > config > default.
        if let Some(raw) = get_var("SQUEEZY_SESSION_DIR") {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                session_settings.log_dir = Some(PathBuf::from(trimmed));
            }
        }
        let mut skills = SkillsConfig::from_settings_and_env_vars(
            settings.skills.unwrap_or_default(),
            &mut get_var,
        );
        let graph = GraphConfig::from_settings(settings.graph.unwrap_or_default());
        let cache = CacheConfig::from_settings(settings.cache.unwrap_or_default());
        let tool_settings = settings.tools.unwrap_or_default();
        let checkpoints_enabled = get_var("SQUEEZY_CHECKPOINTS_ENABLED")
            .as_deref()
            .map(parse_enabled_bool)
            .unwrap_or(tool_settings.checkpoints_enabled.unwrap_or(false));
        let tools = ToolSchemaConfig::from_settings(tool_settings)?;
        let session_resume_picker = session_settings.resume_picker.unwrap_or_default();
        let session_logs = SessionLogConfig::from_settings(&session_settings);
        let context_compaction = ContextCompactionConfig::from_settings_and_env(
            settings.context.unwrap_or_default(),
            &mut get_var,
        );
        // Skills' ContextPercent budget mode reads the same window value as
        // mid-turn compaction; resolve it here instead of duplicating the
        // env/file precedence logic.
        skills.model_context_window = context_compaction.model_context_window;
        let subagents = SubagentConfig::from_settings_and_env(
            settings.subagents.unwrap_or_default(),
            &mut get_var,
        );
        let tui = TuiConfig::from_settings(settings.tui.unwrap_or_default());
        if env_used {
            sources.push("env".to_string());
        }
        if cli_provider.is_some() && !sources.iter().any(|source| source == "cli") {
            sources.push("cli".to_string());
        }
        Ok(Self {
            provider,
            model,
            small_fast_model,
            profile,
            reasoning_effort,
            instructions: DEFAULT_INSTRUCTIONS.to_string(),
            max_output_tokens,
            tool_choice,
            stream_idle_timeout: Duration::from_millis(stream_idle_timeout_ms),
            tick_rate: Duration::from_millis(tui.tick_rate_ms),
            workspace_root,
            permissions,
            session_mode,
            session_resume_picker,
            session_logs,
            context_compaction,
            subagents,
            store_responses,
            exploration_compiler,
            max_parallel_tools,
            tool_spill_threshold_bytes,
            tool_preview_bytes,
            max_tool_result_bytes_per_round,
            tool_output_retention_days,
            exa_mcp_url,
            exa_api_key_env,
            parallel_mcp_url,
            parallel_api_key_env,
            websearch_provider,
            max_tool_calls_per_turn,
            max_tool_bytes_read_per_turn,
            max_search_files_per_turn,
            max_session_cost_usd_micros,
            cost_warn_percent,
            routing,
            telemetry,
            feedback,
            redaction,
            skills,
            graph,
            cache,
            tools,
            checkpoints_enabled,
            tui,
            mcp_servers,
            hardening: HardeningConfig::from_settings(settings.hardening.unwrap_or_default()),
            config_sources: sources,
            config_warnings,
        })
    }

    fn from_default_paths_and_env_with_provider_value(
        provider: Option<&str>,
        profile: Option<&str>,
    ) -> Result<Self> {
        let (mut settings, mut sources, warnings) = load_default_settings_sources()?;
        if let Some(name) = profile {
            settings.apply_profile(name)?;
            sources.push(format!("profile:{name}"));
        }
        Self::try_from_settings_and_env_vars_with_sources_and_warnings(
            settings,
            sources,
            warnings,
            provider,
            |name| env::var(name).ok(),
        )
    }

    fn built_in_defaults() -> Self {
        Self::try_from_settings_and_env_vars(SettingsFile::default(), None, |_| None)
            .expect("built-in config defaults are valid")
    }

    /// Resolves the small-fast-model id for background calls (compaction
    /// summary, classifier, auto-approver). Returns the user-configured value
    /// when set, then the provider built-in cheap default, then `None` when
    /// the provider has no curated cheap tier — callers fall back to the
    /// main model in that case.
    pub fn resolved_small_fast_model(&self) -> Option<String> {
        if let Some(model) = &self.small_fast_model {
            return Some(model.clone());
        }
        small_fast_model_for_provider(provider_kind(&self.provider)).map(str::to_string)
    }

    /// Returns `config_sources` with file paths reduced to short labels
    /// (`"user"`, `"project"`, `"repo"`) for display in narrow status lines. Full
    /// paths remain available on `config_sources` and via `config inspect`.
    pub fn config_source_labels(&self) -> Vec<&str> {
        self.config_sources
            .iter()
            .map(|source| match source.split_once(':') {
                Some((label, _)) => label,
                None => source.as_str(),
            })
            .collect()
    }

    /// Returns a TOML-shaped report of the effective configuration with
    /// sensitive values redacted. The output is valid TOML and the same
    /// document can be parsed back by `SettingsFile::from_toml_str`
    /// (note: `[graph]` and `[mcp.servers.*]` sections currently round-trip
    /// into the typed model but no consumer reads them yet).
    pub fn inspect_redacted(&self) -> String {
        let mut output = String::new();
        output.push_str("# effective Squeezy config\n");
        // sources is a debug artifact, emitted as a comment so the document
        // round-trips through SettingsFile::from_toml_str without choking on
        // a key that does not belong in user-authored settings.
        output.push_str(&format!(
            "# sources = {}\n\n",
            toml_string_array(&self.config_sources)
        ));

        output.push_str("[model]\n");
        output.push_str(&format!(
            "provider = {}\n",
            toml_string(provider_kind(&self.provider))
        ));
        output.push_str(&format!("model = {}\n", toml_string(&self.model)));
        if let Some(small_fast) = &self.small_fast_model {
            output.push_str(&format!("small_fast_model = {}\n", toml_string(small_fast)));
        } else {
            output.push_str(
                "# small_fast_model = unset  # uses per-provider built-in cheap default\n",
            );
        }
        output.push_str(&format!(
            "profile = {}\n",
            toml_string(self.profile.as_str())
        ));
        if let Some(reasoning_effort) = self.reasoning_effort {
            output.push_str(&format!(
                "reasoning_effort = {}\n",
                toml_string(reasoning_effort.as_str())
            ));
        }
        if let Some(max_output_tokens) = self.max_output_tokens {
            output.push_str(&format!("max_output_tokens = {max_output_tokens}\n"));
        } else {
            output.push_str(
                "# max_output_tokens = unset  # no Squeezy cap; provider/model limit applies\n",
            );
        }
        output.push_str(&format!(
            "stream_idle_timeout_ms = {}\n",
            self.stream_idle_timeout.as_millis()
        ));
        output.push_str(&format!("store_responses = {}\n\n", self.store_responses));

        output.push_str("[agent]\n");
        output.push_str(&format!(
            "exploration_compiler = {}\n\n",
            self.exploration_compiler
        ));

        output.push_str("[session]\n");
        output.push_str(&format!(
            "mode = {}\n",
            toml_string(self.session_mode.as_str())
        ));
        output.push_str(&format!(
            "resume_picker = {}\n",
            toml_string(self.session_resume_picker.as_str())
        ));
        if let Some(log_dir) = &self.session_logs.log_dir {
            output.push_str(&format!(
                "log_dir = {}\n",
                toml_string(&log_dir.display().to_string())
            ));
        }
        output.push_str(&format!(
            "log_retention_days = {}\n",
            self.session_logs.log_retention_days
        ));
        output.push_str(&format!(
            "log_retention_archive_days = {}\n",
            self.session_logs.log_retention_archive_days
        ));
        output.push_str(&format!(
            "max_event_bytes = {}\n",
            self.session_logs.max_event_bytes
        ));
        output.push_str(&format!(
            "max_session_bytes = {}\n\n",
            self.session_logs.max_session_bytes
        ));

        output.push_str("[context]\n");
        output.push_str(&format!(
            "compaction_enabled = {}\n",
            self.context_compaction.enabled
        ));
        output.push_str(&format!(
            "compaction_estimated_tokens = {}\n",
            self.context_compaction.estimated_tokens
        ));
        output.push_str(&format!(
            "compaction_min_items = {}\n",
            self.context_compaction.min_items
        ));
        output.push_str(&format!(
            "compaction_recent_items = {}\n",
            self.context_compaction.recent_items
        ));
        output.push_str(&format!(
            "compaction_max_summary_bytes = {}\n",
            self.context_compaction.max_summary_bytes
        ));
        output.push_str(&format!(
            "repo_doc_max_bytes = {}\n",
            self.context_compaction.repo_doc_max_bytes
        ));
        output.push_str(&format!(
            "user_memory_max_bytes = {}\n",
            self.context_compaction.user_memory_max_bytes
        ));
        output.push_str(&format!(
            "enabled_mid_turn = {}\n",
            self.context_compaction.enabled_mid_turn
        ));
        if let Some(window) = self.context_compaction.model_context_window {
            output.push_str(&format!("model_context_window = {}\n", window));
        }
        output.push_str(&format!(
            "threshold_percent = {}\n",
            self.context_compaction.threshold_percent
        ));
        output.push_str(&format!(
            "strategy = {}\n",
            toml_string(self.context_compaction.strategy.as_str())
        ));
        if let Some(model) = &self.context_compaction.model_assisted_model {
            output.push_str(&format!("model_assisted_model = {}\n", toml_string(model)));
        }
        output.push_str(&format!(
            "model_assisted_max_output_tokens = {}\n",
            self.context_compaction.model_assisted_max_output_tokens
        ));
        output.push_str(&format!(
            "model_assisted_timeout_secs = {}\n",
            self.context_compaction.model_assisted_timeout_secs
        ));
        output.push_str(&format!(
            "layered_fallback_extractive_threshold_tokens = {}\n",
            self.context_compaction
                .layered_fallback_extractive_threshold_tokens
        ));
        output.push_str(&format!(
            "micro_compaction_enabled = {}\n",
            self.context_compaction.micro_compaction_enabled
        ));
        output.push_str(&format!(
            "micro_compaction_threshold_percent = {}\n",
            self.context_compaction.micro_compaction_threshold_percent
        ));
        output.push_str(&format!(
            "micro_compaction_keep_recent = {}\n\n",
            self.context_compaction.micro_compaction_keep_recent
        ));

        output.push_str("[subagents]\n");
        output.push_str(&format!("enabled = {}\n", self.subagents.enabled));
        output.push_str(&format!(
            "explore_enabled = {}\n",
            self.subagents.explore_enabled
        ));
        if let Some(model) = &self.subagents.explore_model {
            output.push_str(&format!("explore_model = {}\n", toml_string(model)));
        }
        output.push_str(&format!(
            "max_concurrent = {}\n",
            self.subagents.max_concurrent
        ));
        output.push_str(&format!(
            "max_tool_calls_per_call = {}\n",
            self.subagents.max_tool_calls_per_call
        ));
        output.push_str(&format!(
            "max_tool_bytes_read_per_call = {}\n",
            self.subagents.max_tool_bytes_read_per_call
        ));
        output.push_str(&format!(
            "max_search_files_per_call = {}\n",
            self.subagents.max_search_files_per_call
        ));
        output.push_str(&format!(
            "max_model_rounds = {}\n",
            self.subagents.max_model_rounds
        ));
        output.push_str(&format!(
            "max_summary_tokens = {}\n",
            self.subagents.max_summary_tokens
        ));
        match self.subagents.max_runtime_secs {
            Some(secs) => output.push_str(&format!("max_runtime_secs = {secs}\n")),
            None => output.push_str("max_runtime_secs = 0  # disabled; no wall-clock cap\n"),
        }
        output.push_str(&format!(
            "include_transcript = {}\n\n",
            self.subagents.include_transcript
        ));

        output.push_str("[budgets]\n");
        output.push_str(&format!(
            "max_parallel_tools = {}\n",
            self.max_parallel_tools
        ));
        output.push_str(&format!(
            "max_tool_calls_per_turn = {}\n",
            self.max_tool_calls_per_turn
        ));
        output.push_str(&format!(
            "max_tool_bytes_read_per_turn = {}\n",
            self.max_tool_bytes_read_per_turn
        ));
        output.push_str(&format!(
            "max_search_files_per_turn = {}\n",
            self.max_search_files_per_turn
        ));
        output.push_str(&format!(
            "max_tool_result_bytes_per_round = {}\n\n",
            self.max_tool_result_bytes_per_round
        ));
        if let Some(max_session_cost_usd_micros) = self.max_session_cost_usd_micros {
            output.push_str(&format!(
                "max_session_cost_usd_micros = {max_session_cost_usd_micros}\n"
            ));
        } else {
            output.push_str("# max_session_cost_usd_micros = unset\n");
        }
        output.push_str(&format!(
            "cost_warn_percent = {}\n\n",
            self.cost_warn_percent
        ));

        output.push_str("[routing]\n");
        output.push_str(&format!("auto_cheap = {}\n", self.routing.auto_cheap));
        output.push_str(&format!(
            "auto_cheap_llm_judge = {}\n",
            self.routing.auto_cheap_llm_judge
        ));
        if self.routing.cheap_escalation_tool_calls == 0 {
            output.push_str(
                "# cheap_escalation_tool_calls = 0  # derives at runtime as max_tool_calls_per_turn / 4\n",
            );
        } else {
            output.push_str(&format!(
                "cheap_escalation_tool_calls = {}\n",
                self.routing.cheap_escalation_tool_calls
            ));
        }
        output.push_str(&format!(
            "cheap_escalation_error_threshold = {}\n",
            self.routing.cheap_escalation_error_threshold
        ));
        output.push_str(&format!(
            "escalation_sticky_turns = {}\n",
            self.routing.escalation_sticky_turns
        ));
        output.push_str(&format!(
            "bypass_for_images = {}\n",
            self.routing.bypass_for_images
        ));
        output.push_str(&format!(
            "heuristic_max_chars = {}\n",
            self.routing.heuristic_max_chars
        ));
        output.push_str(&format!(
            "judge_max_chars = {}\n",
            self.routing.judge_max_chars
        ));
        if self.routing.extra_heuristic_verbs.is_empty() {
            output.push_str("# extra_heuristic_verbs = []  # user-extended verb whitelist\n\n");
        } else {
            output.push_str(&format!(
                "extra_heuristic_verbs = {}\n\n",
                toml_string_array(&self.routing.extra_heuristic_verbs)
            ));
        }

        output.push_str("[permissions]\n");
        output.push_str(&format!(
            "mode = {}\n",
            toml_string(self.permissions.mode.as_str())
        ));
        output.push_str(&format!(
            "read = {}\n",
            toml_string(self.permissions.read.as_str())
        ));
        output.push_str(&format!(
            "search = {}\n",
            toml_string(self.permissions.search.as_str())
        ));
        output.push_str(&format!(
            "edit = {}\n",
            toml_string(self.permissions.edit.as_str())
        ));
        output.push_str(&format!(
            "shell = {}\n",
            toml_string(self.permissions.shell.as_str())
        ));
        output.push_str(&format!(
            "ignored_search = {}\n",
            toml_string(self.permissions.ignored_search.as_str())
        ));
        output.push_str(&format!(
            "web = {}\n",
            toml_string(self.permissions.web.as_str())
        ));
        output.push_str(&format!(
            "mcp = {}\n",
            toml_string(self.permissions.mcp.as_str())
        ));
        output.push_str(&format!(
            "git = {}\n",
            toml_string(self.permissions.git.as_str())
        ));
        output.push_str(&format!(
            "compiler = {}\n",
            toml_string(self.permissions.compiler.as_str())
        ));
        output.push_str(&format!(
            "destructive = {}\n",
            toml_string(self.permissions.destructive.as_str())
        ));
        output.push_str(&format!(
            "shell_classifier = {}\n\n",
            self.permissions.shell_classifier
        ));
        output.push_str("[permissions.ai_reviewer]\n");
        output.push_str(&format!(
            "enabled = {}\n",
            self.permissions.ai_reviewer.enabled
        ));
        if let Some(model) = &self.permissions.ai_reviewer.model {
            output.push_str(&format!("model = {}\n", toml_string(model)));
        }
        output.push_str(&format!(
            "allow_capabilities = {}\n",
            toml_string_array(
                &self
                    .permissions
                    .ai_reviewer
                    .allow_capabilities
                    .iter()
                    .map(|capability| capability.as_str().to_string())
                    .collect::<Vec<_>>()
            )
        ));
        if let Some(policy_file) = &self.permissions.ai_reviewer.policy_file {
            output.push_str(&format!(
                "policy_file = {}\n",
                toml_string(&policy_file.display().to_string())
            ));
        }
        output.push_str(&format!(
            "max_transcript_tokens = {}\n",
            self.permissions.ai_reviewer.max_transcript_tokens
        ));
        output.push_str(&format!(
            "timeout_secs = {}\n\n",
            self.permissions.ai_reviewer.timeout_secs
        ));
        output.push_str("[permissions.shell_sandbox]\n");
        output.push_str(&format!(
            "mode = {}\n",
            toml_string(self.permissions.shell_sandbox.mode.as_str())
        ));
        output.push_str(&format!(
            "network = {}\n",
            toml_string(self.permissions.shell_sandbox.network.as_str())
        ));
        output.push_str(&format!(
            "audit = {}\n",
            self.permissions.shell_sandbox.audit
        ));
        output.push_str(&format!(
            "kill_grace_ms = {}\n",
            self.permissions.shell_sandbox.kill_grace_ms
        ));
        output.push_str(&format!(
            "env_allowlist = {}\n",
            toml_string_array(&self.permissions.shell_sandbox.env_allowlist)
        ));
        output.push_str(&format!(
            "read_roots = {}\n",
            toml_path_array(&self.permissions.shell_sandbox.read_roots)
        ));
        output.push_str(&format!(
            "write_roots = {}\n",
            toml_path_array(&self.permissions.shell_sandbox.write_roots)
        ));
        output.push_str(&format!(
            "protected_metadata_names = {}\n",
            toml_string_array(&self.permissions.shell_sandbox.protected_metadata_names)
        ));
        output.push_str(&format!(
            "sensitive_path_patterns = {}\n",
            toml_string_array(&self.permissions.shell_sandbox.sensitive_path_patterns)
        ));
        // The list above is the EFFECTIVE list (built-in floor unioned with
        // user additions). Round-tripping must not re-union, otherwise an
        // inspected config would diverge from the running config.
        output.push_str("replace_sensitive_path_patterns = true\n\n");
        for rule in self
            .permissions
            .rules
            .iter()
            .filter(|rule| rule.source != PermissionRuleSource::Builtin)
        {
            output.push_str("[[permissions.rules]]\n");
            output.push_str(&format!("capability = {}\n", toml_string(&rule.capability)));
            output.push_str(&format!("target = {}\n", toml_string(&rule.target)));
            output.push_str(&format!("action = {}\n", toml_string(rule.action.as_str())));
            output.push_str(&format!("source = {}\n", toml_string(rule.source.as_str())));
            if let Some(reason) = &rule.reason {
                output.push_str(&format!("reason = {}\n", toml_string(reason)));
            }
            output.push('\n');
        }

        output.push_str("[hardening]\n");
        output.push_str(&format!(
            "disable_core_dumps = {}\n",
            self.hardening.disable_core_dumps
        ));
        output.push_str(&format!(
            "deny_debug_attach = {}\n\n",
            self.hardening.deny_debug_attach
        ));

        output.push_str("[telemetry]\n");
        output.push_str(&format!("enabled = {}\n", self.telemetry.enabled));
        output.push_str(&format!(
            "endpoint = {}\n\n",
            toml_string(&self.telemetry.endpoint)
        ));

        output.push_str("[feedback]\n");
        output.push_str(&format!("enabled = {}\n", self.feedback.enabled));
        output.push_str(&format!(
            "feedback_endpoint = {}\n",
            toml_string(&self.feedback.feedback_endpoint)
        ));
        output.push_str(&format!(
            "report_endpoint = {}\n",
            toml_string(&self.feedback.report_endpoint)
        ));
        output.push_str(&format!(
            "max_feedback_bytes = {}\n",
            self.feedback.max_feedback_bytes
        ));
        output.push_str(&format!(
            "max_report_bytes = {}\n\n",
            self.feedback.max_report_bytes
        ));

        output.push_str("[redaction]\n");
        if self.redaction.custom_patterns.is_empty() {
            output.push_str("custom_patterns = []\n\n");
        } else {
            // Emit a TOML comment so the document still round-trips through
            // `SettingsFile::from_toml_str`, but do not echo the literal
            // patterns. A previous version emitted
            // `custom_patterns = ["<redacted>"]`, which was itself a valid
            // (no-op) regex if pasted back into a settings file.
            output.push_str(&format!(
                "# {} custom redaction pattern(s) hidden in inspect output\n",
                self.redaction.custom_patterns.len(),
            ));
            output.push_str("custom_patterns = []\n\n");
        }

        output.push_str("[web]\n");
        output.push_str(&format!(
            "websearch_provider = {}\n",
            toml_string(&self.websearch_provider)
        ));
        output.push_str(&format!(
            "exa_mcp_url = {}\n",
            toml_string(&self.exa_mcp_url)
        ));
        output.push_str("exa_api_key_env = \"<redacted>\"\n");
        output.push_str(&format!(
            "parallel_mcp_url = {}\n",
            toml_string(&self.parallel_mcp_url)
        ));
        output.push_str("parallel_api_key_env = \"<redacted>\"\n\n");

        output.push_str("[skills]\n");
        output.push_str(&format!(
            "user_dir = {}\n",
            toml_string(&self.skills.user_dir.display().to_string())
        ));
        output.push_str(&format!(
            "compat_user_dir = {}\n",
            toml_string(&self.skills.compat_user_dir.display().to_string())
        ));
        output.push_str(&format!(
            "active_budget_chars = {}\n",
            self.skills.active_budget_chars
        ));
        output.push_str(&format!(
            "active_body_cap_chars = {}\n",
            self.skills.active_body_cap_chars
        ));
        output.push_str(&format!(
            "preamble_enabled = {}\n",
            self.skills.preamble_enabled
        ));
        output.push_str(&format!(
            "preamble_budget_chars = {}\n",
            self.skills.preamble_budget_chars
        ));
        output.push_str(&format!("inline = {}\n", self.skills.inline));
        // The mode tables follow the same inline-table shape that
        // `from_table` accepts, so the inspect output round-trips when
        // pasted back into a settings file.
        emit_skills_budget_mode(
            &mut output,
            "active_budget_mode",
            self.skills.active_budget_mode,
        );
        emit_skills_budget_mode(
            &mut output,
            "preamble_budget_mode",
            self.skills.preamble_budget_mode,
        );
        if self.skills.config.is_empty() {
            output.push('\n');
        } else {
            output.push('\n');
            for entry in &self.skills.config {
                output.push_str("[[skills.config]]\n");
                if let Some(name) = &entry.name {
                    output.push_str(&format!("name = {}\n", toml_string(name)));
                }
                if let Some(path) = &entry.path {
                    output.push_str(&format!(
                        "path = {}\n",
                        toml_string(&path.display().to_string())
                    ));
                }
                output.push_str(&format!("enabled = {}\n\n", entry.enabled));
            }
        }

        output.push_str("[graph]\n");
        output.push_str(&format!(
            "languages = {}\n",
            toml_string_array(&self.graph.languages)
        ));
        output.push_str(&format!("max_file_bytes = {}\n", self.graph.max_file_bytes));
        output.push_str(&format!("include_hidden = {}\n", self.graph.include_hidden));
        output.push_str(&format!(
            "require_indexing_signal = {}\n\n",
            self.graph.require_indexing_signal
        ));
        output.push_str(&format!(
            "include = {}\n",
            toml_string_array(&self.graph.include)
        ));
        output.push_str(&format!(
            "exclude = {}\n",
            toml_string_array(&self.graph.exclude)
        ));
        output.push_str(&format!(
            "include_classes = {}\n",
            toml_string_array(&self.graph.include_classes)
        ));
        output.push_str(&format!(
            "exclude_classes = {}\n\n",
            toml_string_array(&self.graph.exclude_classes)
        ));

        output.push_str("[cache]\n");
        if let Some(root) = &self.cache.root {
            output.push_str(&format!(
                "root = {}\n",
                toml_string(&root.display().to_string())
            ));
        }
        if let Some(tool_outputs) = &self.cache.tool_outputs {
            output.push_str(&format!(
                "tool_outputs = {}\n",
                toml_string(&tool_outputs.display().to_string())
            ));
        }
        output.push('\n');

        output.push_str("[tools]\n");
        output.push_str(&format!(
            "checkpoints_enabled = {}\n",
            self.checkpoints_enabled
        ));
        output.push_str(&format!(
            "lazy_schema_loading = {}\n",
            self.tools.lazy_schema_loading
        ));
        output.push_str(&format!("core = {}\n", toml_string_array(&self.tools.core)));
        output.push_str(&format!(
            "discoverable = {}\n\n",
            toml_string_array(&self.tools.discoverable)
        ));

        output.push_str("[tui]\n");
        output.push_str(&format!("tick_rate_ms = {}\n", self.tui.tick_rate_ms));
        output.push_str(&format!(
            "status_verbosity = {}\n",
            toml_string(self.tui.status_verbosity.as_str())
        ));
        output.push_str(&format!(
            "response_verbosity = {}\n",
            toml_string(self.tui.response_verbosity.as_str())
        ));
        output.push_str(&format!(
            "tool_output_verbosity = {}\n",
            toml_string(self.tui.tool_output_verbosity.as_str())
        ));
        output.push_str(&format!(
            "transcript_default = {}\n",
            toml_string(self.tui.transcript_default.as_str())
        ));
        output.push_str(&format!(
            "alternate_screen = {}\n",
            toml_string(self.tui.alternate_screen.as_str())
        ));
        output.push_str(&format!(
            "synchronized_output = {}\n",
            toml_string(self.tui.synchronized_output.as_str())
        ));
        output.push_str(&format!("theme = {}\n", toml_string(&self.tui.theme)));
        output.push_str(&format!(
            "show_reasoning_usage = {}\n",
            self.tui.show_reasoning_usage
        ));
        output.push_str(&format!(
            "persist_prompt_history = {}\n\n",
            self.tui.persist_prompt_history
        ));
        for (name, theme) in &self.tui.themes {
            if theme.colors.is_empty() {
                continue;
            }
            output.push_str(&format!(
                "[tui.themes.{}.colors]\n",
                toml_bare_or_quoted_key(name)
            ));
            for (token, rgb) in &theme.colors {
                output.push_str(&format!(
                    "{} = [{}, {}, {}]\n",
                    toml_bare_or_quoted_key(token),
                    rgb[0],
                    rgb[1],
                    rgb[2]
                ));
            }
            output.push('\n');
        }

        for (name, server) in &self.mcp_servers {
            output.push_str(&format!(
                "[mcp.servers.{}]\n",
                toml_bare_or_quoted_key(name)
            ));
            output.push_str(&format!("enabled = {}\n", server.enabled));
            output.push_str(&format!(
                "transport = {}\n",
                toml_string(server.transport.as_str())
            ));
            if let Some(command) = &server.command {
                output.push_str(&format!("command = {}\n", toml_string(command)));
            }
            output.push_str(&format!("args = {}\n", toml_string_array(&server.args)));
            if let Some(url) = &server.url {
                output.push_str(&format!("url = {}\n", toml_string(url)));
            }
            if let Some(timeout_ms) = server.timeout_ms {
                output.push_str(&format!("timeout_ms = {timeout_ms}\n"));
            }
            if let Some(discovery_timeout_ms) = server.discovery_timeout_ms {
                output.push_str(&format!("discovery_timeout_ms = {discovery_timeout_ms}\n"));
            }
            if let Some(tool_call_timeout_ms) = server.tool_call_timeout_ms {
                output.push_str(&format!("tool_call_timeout_ms = {tool_call_timeout_ms}\n"));
            }
            if let Some(enabled_tools) = &server.enabled_tools {
                output.push_str(&format!(
                    "enabled_tools = {}\n",
                    toml_string_array(enabled_tools)
                ));
            }
            if !server.disabled_tools.is_empty() {
                output.push_str(&format!(
                    "disabled_tools = {}\n",
                    toml_string_array(&server.disabled_tools)
                ));
            }
            if !server.env.is_empty() {
                let entries = server
                    .env
                    .keys()
                    .map(|key| {
                        format!(
                            "{} = {}",
                            toml_bare_or_quoted_key(key),
                            toml_string("<redacted>")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                output.push_str(&format!("env = {{ {entries} }}\n"));
            }
            if let Some(default) = server.permissions.default {
                output.push('\n');
                output.push_str(&format!(
                    "[mcp.servers.{}.permissions]\n",
                    toml_bare_or_quoted_key(name)
                ));
                output.push_str(&format!("default = {}\n", toml_string(default.as_str())));
            }
            for rule in &server.permissions.rules {
                output.push('\n');
                output.push_str(&format!(
                    "[[mcp.servers.{}.permissions.rules]]\n",
                    toml_bare_or_quoted_key(name)
                ));
                let target = rule
                    .target
                    .strip_prefix(&format!("{name}/"))
                    .unwrap_or(&rule.target);
                output.push_str(&format!("target = {}\n", toml_string(target)));
                output.push_str(&format!("action = {}\n", toml_string(rule.action.as_str())));
                output.push_str(&format!("source = {}\n", toml_string(rule.source.as_str())));
                if let Some(reason) = &rule.reason {
                    output.push_str(&format!("reason = {}\n", toml_string(reason)));
                }
            }
            output.push('\n');
        }
        output
    }
}

fn provider_kind(provider: &ProviderConfig) -> &'static str {
    match provider {
        ProviderConfig::OpenAi(_) => "openai",
        ProviderConfig::Anthropic(_) => "anthropic",
        ProviderConfig::Google(_) => "google",
        ProviderConfig::AzureOpenAi(_) => "azure_openai",
        ProviderConfig::Bedrock(_) => "bedrock",
        ProviderConfig::Ollama(_) => "ollama",
        ProviderConfig::OpenAiCodex(_) => "openai_codex",
        ProviderConfig::OpenAiCompatible(config) => config.preset.as_str(),
        ProviderConfig::Faux(_) => "faux",
    }
}

/// Escape `value` as a TOML basic string. Public so persistence helpers in
/// downstream crates (e.g. permission rule writing) can share the same
/// escaping rules used by `config inspect`.
pub fn escape_toml_basic_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn toml_string(value: &str) -> String {
    escape_toml_basic_string(value)
}

fn toml_string_array<S: AsRef<str>>(values: &[S]) -> String {
    let mut out = String::from("[");
    for (i, value) in values.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&toml_string(value.as_ref()));
    }
    out.push(']');
    out
}

fn toml_path_array(values: &[PathBuf]) -> String {
    let values = values
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    toml_string_array(&values)
}

fn emit_skills_budget_mode(output: &mut String, key: &str, mode: SkillsBudgetMode) {
    match mode {
        SkillsBudgetMode::Chars { chars } => {
            output.push_str(&format!("{key} = {{ chars = {chars} }}\n"));
        }
        SkillsBudgetMode::ContextPercent { percent } => {
            // `{:?}` on f32 always emits a decimal point, keeping the TOML
            // parser on the float branch instead of misreading e.g. `2`
            // as an integer next round-trip.
            output.push_str(&format!(
                "{key} = {{ context_percent = {:?} }}\n",
                percent as f64
            ));
        }
    }
}

fn toml_bare_or_quoted_key(key: &str) -> String {
    if !key.is_empty()
        && key
            .chars()
            .all(|c| matches!(c, 'A'..='Z' | 'a'..='z' | '0'..='9' | '_' | '-'))
    {
        key.to_string()
    } else {
        toml_string(key)
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self::from_env()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderConfig {
    OpenAi(OpenAiConfig),
    Anthropic(AnthropicConfig),
    Google(GoogleConfig),
    AzureOpenAi(AzureOpenAiConfig),
    Bedrock(BedrockConfig),
    Ollama(OllamaConfig),
    OpenAiCompatible(OpenAiCompatibleConfig),
    /// ChatGPT Plus/Pro subscription. The wire protocol is the OpenAI
    /// Responses API; auth is an OAuth access token persisted under
    /// `~/.squeezy/auth/openai-codex.json` rather than an env-var API
    /// key. The credential is never carried inline in the TOML — the
    /// settings only describe the endpoint and originator.
    OpenAiCodex(OpenAiCodexConfig),
    /// Deterministic in-process faux provider for tests and the eval
    /// harness. The wire protocol is local: each `stream_response` call
    /// pops the next scripted response from an internal queue and replays
    /// it as a synthetic event stream. No outbound HTTP. See
    /// `squeezy-llm`'s `FauxProvider` for the runtime behaviour and
    /// script format.
    Faux(FauxConfig),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenAiCompatibleConfig {
    pub preset: OpenAiCompatiblePreset,
    pub api_key_env: String,
    /// Inline plaintext API key resolved from the user/local TOML layer.
    /// `None` keeps the legacy env/keychain resolution. Never serialized
    /// in plain text — emits `"<redacted>"` for accidental dumps.
    #[serde(serialize_with = "redact_secret_opt")]
    pub api_key: Option<String>,
    pub base_url: String,
    pub extra_headers: BTreeMap<String, String>,
    pub transport: ProviderTransportConfig,
    /// Cloudflare account id. Populated for the Workers AI / AI Gateway
    /// presets so the LLM client can substitute the `{account_id}`
    /// placeholder in `base_url` before requests fire. `None` for every
    /// non-Cloudflare preset. `serde(default)` keeps older configs (and
    /// hand-rolled provider configs that predate this field) deserializing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// Cloudflare AI Gateway id. Populated for the AI Gateway preset so
    /// `{gateway_id}` in `base_url` resolves before requests fire. `None`
    /// for the Workers AI preset and every non-Cloudflare preset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_id: Option<String>,
}

/// Named presets for the OpenAI-compatible (Chat Completions) provider. Each
/// preset carries enough defaults that the user can wire a provider with just
/// an API key. `Custom` is for any other OpenAI-compatible endpoint (e.g.
/// self-hosted LiteLLM, Cohere) and requires the caller to supply `base_url`
/// and `api_key_env` explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiCompatiblePreset {
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
    // Serde's snake_case derivation chops between each upper/lower transition
    // (`LMStudio` -> `l_m_studio`), which would diverge from the TOML section
    // name. Pin the wire format explicitly to keep round-trip stable.
    #[serde(rename = "lmstudio")]
    LMStudio,
    #[serde(rename = "vllm")]
    VLlm,
    #[serde(rename = "llamacpp")]
    LlamaCpp,
    CloudflareWorkersAi,
    CloudflareAiGateway,
    Custom,
}

impl OpenAiCompatiblePreset {
    /// Kebab/snake-case identifier used in TOML provider section names, CLI
    /// `--provider` values, and the model registry.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenRouter => "openrouter",
            Self::Vercel => "vercel",
            Self::PortKey => "portkey",
            Self::Groq => "groq",
            Self::XAi => "xai",
            Self::DeepSeek => "deepseek",
            Self::Vertex => "vertex",
            Self::Mistral => "mistral",
            Self::Together => "together",
            Self::Fireworks => "fireworks",
            Self::Cerebras => "cerebras",
            Self::DeepInfra => "deepinfra",
            Self::Baseten => "baseten",
            Self::LMStudio => "lmstudio",
            Self::VLlm => "vllm",
            Self::LlamaCpp => "llamacpp",
            Self::CloudflareWorkersAi => "cloudflare_workers_ai",
            Self::CloudflareAiGateway => "cloudflare_ai_gateway",
            Self::Custom => "openai_compatible",
        }
    }

    /// Human-readable label for the startup picker and `--list-providers`.
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::OpenRouter => "OpenRouter",
            Self::Vercel => "Vercel AI Gateway",
            Self::PortKey => "PortKey",
            Self::Groq => "Groq",
            Self::XAi => "xAI",
            Self::DeepSeek => "DeepSeek",
            Self::Vertex => "Google Vertex AI",
            Self::Mistral => "Mistral La Plateforme",
            Self::Together => "Together AI",
            Self::Fireworks => "Fireworks AI",
            Self::Cerebras => "Cerebras",
            Self::DeepInfra => "DeepInfra",
            Self::Baseten => "Baseten",
            Self::LMStudio => "LM Studio",
            Self::VLlm => "vLLM",
            Self::LlamaCpp => "llama.cpp server",
            Self::CloudflareWorkersAi => "Cloudflare Workers AI",
            Self::CloudflareAiGateway => "Cloudflare AI Gateway",
            Self::Custom => "OpenAI-compatible (custom)",
        }
    }

    /// `true` when curated models exist in the registry and a dedicated costly
    /// integration test ships in `crates/squeezy-llm/tests/`. Light presets
    /// return `false` and fall back to generic context-window estimates.
    pub const fn is_full_tier(self) -> bool {
        matches!(
            self,
            Self::OpenRouter
                | Self::Vercel
                | Self::PortKey
                | Self::Groq
                | Self::XAi
                | Self::DeepSeek
                | Self::Vertex
        )
    }

    pub const fn default_base_url(self) -> &'static str {
        match self {
            Self::OpenRouter => DEFAULT_OPENROUTER_BASE_URL,
            Self::Vercel => DEFAULT_VERCEL_AI_BASE_URL,
            Self::PortKey => DEFAULT_PORTKEY_BASE_URL,
            Self::Groq => DEFAULT_GROQ_BASE_URL,
            Self::XAi => DEFAULT_XAI_BASE_URL,
            Self::DeepSeek => DEFAULT_DEEPSEEK_BASE_URL,
            // Vertex's base URL is per-project and per-region. The caller
            // must template it from `vertex_project` + `vertex_location`
            // (see `vertex_base_url`); presetting a static URL here would
            // hard-code one project.
            Self::Vertex => "",
            Self::Mistral => DEFAULT_MISTRAL_BASE_URL,
            Self::Together => DEFAULT_TOGETHER_BASE_URL,
            Self::Fireworks => DEFAULT_FIREWORKS_BASE_URL,
            Self::Cerebras => DEFAULT_CEREBRAS_BASE_URL,
            Self::DeepInfra => DEFAULT_DEEPINFRA_BASE_URL,
            Self::Baseten => DEFAULT_BASETEN_BASE_URL,
            Self::LMStudio => DEFAULT_LMSTUDIO_BASE_URL,
            Self::VLlm => DEFAULT_VLLM_BASE_URL,
            Self::LlamaCpp => DEFAULT_LLAMACPP_BASE_URL,
            // Cloudflare's base URLs are per-account (and per-gateway for the
            // gateway preset). The default templates carry `{account_id}`
            // and `{gateway_id}` placeholders that the LLM client substitutes
            // out of `OpenAiCompatibleConfig.account_id` / `.gateway_id`
            // before requests fire (see `substitute_url_placeholders` in
            // `squeezy-llm::compatible`). Users who override `base_url`
            // can keep the same placeholder syntax to route through a
            // custom reverse proxy without re-implementing the substitution.
            Self::CloudflareWorkersAi => DEFAULT_CLOUDFLARE_WORKERS_AI_BASE_URL,
            Self::CloudflareAiGateway => DEFAULT_CLOUDFLARE_AI_GATEWAY_BASE_URL,
            Self::Custom => "",
        }
    }

    pub const fn default_api_key_env(self) -> &'static str {
        match self {
            Self::OpenRouter => "OPENROUTER_API_KEY",
            Self::Vercel => "AI_GATEWAY_API_KEY",
            Self::PortKey => "PORTKEY_API_KEY",
            Self::Groq => "GROQ_API_KEY",
            Self::XAi => "XAI_API_KEY",
            Self::DeepSeek => "DEEPSEEK_API_KEY",
            // Vertex's "key" is an OAuth2 access token (~1 hour TTL). Users
            // either set this env var to a token they refresh themselves
            // (e.g. via `gcloud auth print-access-token`) or wire in a
            // service-account JSON helper.
            Self::Vertex => "VERTEX_ACCESS_TOKEN",
            Self::Mistral => "MISTRAL_API_KEY",
            Self::Together => "TOGETHER_API_KEY",
            Self::Fireworks => "FIREWORKS_API_KEY",
            Self::Cerebras => "CEREBRAS_API_KEY",
            Self::DeepInfra => "DEEPINFRA_API_KEY",
            Self::Baseten => "BASETEN_API_KEY",
            // Local self-hosted servers usually do not authenticate, but the env
            // var slot lets users layer a bearer token via a reverse proxy.
            Self::LMStudio => "LMSTUDIO_API_KEY",
            Self::VLlm => "VLLM_API_KEY",
            Self::LlamaCpp => "LLAMACPP_API_KEY",
            // Cloudflare uses one API token (CLOUDFLARE_API_KEY) for the
            // direct Workers AI endpoint, and the same token for the
            // upstream bearer when routing through AI Gateway. The optional
            // gateway-level token (`cf-aig-authorization`) is configured
            // separately via `CF_AIG_TOKEN` and injected as an extra header.
            Self::CloudflareWorkersAi => "CLOUDFLARE_API_KEY",
            Self::CloudflareAiGateway => "CLOUDFLARE_API_KEY",
            Self::Custom => "",
        }
    }

    pub const fn default_model(self) -> &'static str {
        match self {
            Self::OpenRouter => DEFAULT_OPENROUTER_MODEL,
            Self::Vercel => DEFAULT_VERCEL_AI_MODEL,
            Self::PortKey => DEFAULT_PORTKEY_MODEL,
            Self::Groq => DEFAULT_GROQ_MODEL,
            Self::XAi => DEFAULT_XAI_MODEL,
            Self::DeepSeek => DEFAULT_DEEPSEEK_MODEL,
            Self::Vertex => DEFAULT_VERTEX_MODEL,
            Self::Mistral => DEFAULT_MISTRAL_MODEL,
            Self::Together => DEFAULT_TOGETHER_MODEL,
            Self::Fireworks => DEFAULT_FIREWORKS_MODEL,
            Self::Cerebras => DEFAULT_CEREBRAS_MODEL,
            Self::DeepInfra => DEFAULT_DEEPINFRA_MODEL,
            Self::Baseten => DEFAULT_BASETEN_MODEL,
            // Local servers expose whatever checkpoint the operator loaded; we
            // cannot guess a default, so the user supplies `model = …`.
            Self::LMStudio => "",
            Self::VLlm => "",
            Self::LlamaCpp => "",
            Self::CloudflareWorkersAi => DEFAULT_CLOUDFLARE_WORKERS_AI_MODEL,
            Self::CloudflareAiGateway => DEFAULT_CLOUDFLARE_AI_GATEWAY_MODEL,
            Self::Custom => "",
        }
    }

    /// Aliases accepted from CLI `--provider`, env `SQUEEZY_PROVIDER`, and TOML
    /// `model.provider`. The canonical name (`as_str`) is always accepted.
    pub fn parse(value: &str) -> Option<Self> {
        let normalised = value.trim().to_ascii_lowercase().replace('-', "_");
        match normalised.as_str() {
            "openrouter" | "open_router" => Some(Self::OpenRouter),
            "vercel" | "vercel_ai" | "vercel_ai_gateway" => Some(Self::Vercel),
            "portkey" | "port_key" => Some(Self::PortKey),
            "groq" => Some(Self::Groq),
            "xai" | "x_ai" | "grok" => Some(Self::XAi),
            "deepseek" | "deep_seek" => Some(Self::DeepSeek),
            "vertex" | "vertex_ai" | "google_vertex" | "google_vertex_ai" => Some(Self::Vertex),
            "mistral" | "mistral_ai" => Some(Self::Mistral),
            "together" | "together_ai" => Some(Self::Together),
            "fireworks" | "fireworks_ai" => Some(Self::Fireworks),
            "cerebras" => Some(Self::Cerebras),
            "deepinfra" | "deep_infra" => Some(Self::DeepInfra),
            "baseten" => Some(Self::Baseten),
            "lmstudio" | "lm_studio" => Some(Self::LMStudio),
            "vllm" => Some(Self::VLlm),
            "llamacpp" | "llama_cpp" | "llama_cpp_server" => Some(Self::LlamaCpp),
            "cloudflare_workers_ai" | "cloudflare_workersai" | "workers_ai" | "cf_workers_ai" => {
                Some(Self::CloudflareWorkersAi)
            }
            "cloudflare_ai_gateway" | "cloudflare_gateway" | "cf_ai_gateway" | "ai_gateway" => {
                Some(Self::CloudflareAiGateway)
            }
            "openai_compatible" | "custom" => Some(Self::Custom),
            _ => None,
        }
    }

    /// Every preset that ships with `cargo run -p squeezy -- --list-providers`.
    /// Used by the CLI to enumerate options without hard-coding the list.
    pub fn all() -> [Self; 19] {
        [
            Self::OpenRouter,
            Self::Vercel,
            Self::PortKey,
            Self::Groq,
            Self::XAi,
            Self::DeepSeek,
            Self::Vertex,
            Self::Mistral,
            Self::Together,
            Self::Fireworks,
            Self::Cerebras,
            Self::DeepInfra,
            Self::Baseten,
            Self::LMStudio,
            Self::VLlm,
            Self::LlamaCpp,
            Self::CloudflareWorkersAi,
            Self::CloudflareAiGateway,
            Self::Custom,
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenAiConfig {
    pub api_key_env: String,
    #[serde(serialize_with = "redact_secret_opt")]
    pub api_key: Option<String>,
    pub base_url: String,
    /// Pay-As-You-Go org slug forwarded as the `OpenAI-Organization`
    /// header so spend attributes against the right billing org when
    /// the API key has access to multiple. `None` keeps the legacy
    /// behavior of letting OpenAI pick the user's default org.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    /// Project id forwarded as `OpenAI-Project` so multi-project orgs
    /// attribute usage to the right project rather than the org's
    /// fallback project. Reads `OPENAI_PROJECT_ID` env / `project` TOML.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Service tier forwarded as the `service_tier` body field on
    /// `/responses` and chat-completions calls. Accepts `"auto"`,
    /// `"default"`, `"flex"`, `"priority"`, and `"scale"`; the
    /// provider passes the string through so newly-added tiers do
    /// not require a client release. `None` lets OpenAI pick.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    pub transport: ProviderTransportConfig,
}

/// ChatGPT Plus/Pro subscription provider settings. The OAuth token
/// itself lives outside the TOML (under `~/.squeezy/auth/openai-codex.json`
/// with `chmod 600`); only the endpoint and the originator label are
/// configurable here. `base_url` accepts user overrides for testing
/// against a captive backend but defaults to the live ChatGPT Codex
/// endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenAiCodexConfig {
    pub base_url: String,
    pub originator: String,
    pub transport: ProviderTransportConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnthropicConfig {
    pub api_key_env: String,
    #[serde(serialize_with = "redact_secret_opt")]
    pub api_key: Option<String>,
    pub base_url: String,
    pub transport: ProviderTransportConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoogleConfig {
    pub api_key_env: String,
    #[serde(serialize_with = "redact_secret_opt")]
    pub api_key: Option<String>,
    pub base_url: String,
    pub transport: ProviderTransportConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AzureOpenAiConfig {
    pub api_key_env: String,
    #[serde(serialize_with = "redact_secret_opt")]
    pub api_key: Option<String>,
    pub base_url: String,
    pub api_version: String,
    /// Maps logical model ids the caller uses in `[model]` (e.g. `gpt-4o`)
    /// to the Azure-deployment name the resource actually exposes
    /// (e.g. `my-deployment-gpt-4o`). When the body's `model` field is
    /// built, the provider substitutes the mapped value so users can
    /// keep stable model ids in config even when the Azure deployment
    /// is renamed or differs between environments. An entry missing from
    /// the map is sent through verbatim, preserving the historical
    /// "deployment id is the model id" behavior.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub deployment_name_map: BTreeMap<String, String>,
    /// Operator-controlled HTTP headers forwarded on every Azure OpenAI
    /// request. Lets users wire `Apim-Subscription-Key` for API Management
    /// fronted deployments, `x-ms-client-request-id` for correlation,
    /// `x-ms-region` pinning, or an explicit `Authorization: Bearer …`
    /// that overrides the default `api-key` header (Entra ID / managed
    /// identity flows resolved out-of-band). User-supplied headers always
    /// win over the provider's defaults so an override is honored
    /// verbatim. Keyed via the standard `[providers.azure_openai.headers]`
    /// TOML table, matching the OpenAI-compatible preset shape.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra_headers: BTreeMap<String, String>,
    /// Switches the provider away from the default `api-key: …` header
    /// to `Authorization: Bearer …` so Microsoft Entra ID / managed
    /// identity flows can authenticate against Azure OpenAI. The actual
    /// bearer is sourced from `AZURE_OPENAI_BEARER_TOKEN` (an external
    /// refresher — `az account get-access-token`, IMDS, or a sidecar —
    /// keeps the env var current). Defaults to `false`, preserving the
    /// historical API-key behavior.
    #[serde(default)]
    pub use_entra_id: bool,
    /// Bearer token resolved from the environment when `use_entra_id`
    /// is set. Snapshot at config build time; treat as short-lived and
    /// arrange for the supplying process to refresh the env var before
    /// expiry. `None` means no token was available — the provider
    /// then surfaces an explicit error rather than silently falling
    /// back to the api-key path.
    #[serde(default, serialize_with = "redact_secret_opt")]
    pub entra_bearer_token: Option<String>,
    pub transport: ProviderTransportConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BedrockConfig {
    pub region: String,
    pub base_url: Option<String>,
    /// Optional short-lived bearer token sourced from
    /// `AWS_BEARER_TOKEN_BEDROCK`. When present the provider routes
    /// requests through the Bedrock HTTP bearer-auth scheme instead of
    /// the standard AWS SigV4 credential chain — matching the
    /// long-term "Amazon Bedrock API keys" feature that AWS's other
    /// SDKs (boto3, JS, Java) auto-detect from the same env var. Kept
    /// out of serialized config dumps via `redact_secret_opt` so it
    /// never lands in TOML logs alongside the rest of `[providers.bedrock]`.
    #[serde(default, serialize_with = "redact_secret_opt")]
    pub bearer_token: Option<String>,
    /// Operator-defined cost-allocation tags forwarded on every
    /// `ConverseStream` invocation so AWS can group invocations by
    /// team/env/project in CloudWatch + Cost Explorer.
    /// (F16pi-bedrock-request-metadata-tags.)
    #[serde(default)]
    pub request_metadata: BTreeMap<String, String>,
    pub transport: ProviderTransportConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OllamaConfig {
    pub base_url: String,
    /// Wire route for `POST <model>` traffic. `Native` keeps the proprietary
    /// `/api/chat` NDJSON path (default; exposes `keep_alive` + `num_ctx`);
    /// `OpenAiCompatible` rewrites the request to `/v1/chat/completions` SSE
    /// so users with portable tooling can pin Ollama to a uniform contract.
    #[serde(default)]
    pub route_style: OllamaRoute,
    pub transport: ProviderTransportConfig,
}

/// Configuration for the in-process faux provider used by tests and the
/// eval harness. The provider is wired through [`ProviderConfig::Faux`]
/// so eval / integration tests can target it with a `[providers.faux]`
/// TOML section instead of plumbing a bespoke mock through every entry
/// point.
///
/// Example settings:
///
/// ```toml
/// [model]
/// provider = "faux"
///
/// [providers.faux]
/// # Optional path to a TOML script file. When omitted the provider
/// # starts empty and callers must push responses programmatically.
/// script = "tests/fixtures/faux-script.toml"
/// # Optional override for the provider name reported by
/// # `LlmProvider::name` (defaults to "faux").
/// default_model = "faux-1"
/// ```
///
/// `script` is read by `squeezy-llm`'s `FauxProvider::from_config`; see
/// that crate for the script schema (a list of `[[turn]]` entries with
/// `text` / `thinking` / `tool_calls` / `error` / token-usage fields).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FauxConfig {
    /// Path to a TOML file describing the scripted responses. Resolved
    /// relative to the process working directory when the provider is
    /// constructed.
    #[serde(default)]
    pub script: Option<String>,
    /// Optional override for the provider name returned by
    /// `LlmProvider::name`. Falls back to `"faux"` when unset.
    #[serde(default)]
    pub name: Option<String>,
    /// Retry / timeout knobs are accepted for surface symmetry with the
    /// real providers but ignored by the in-process faux implementation.
    #[serde(default)]
    pub transport: ProviderTransportConfig,
}

/// Selects which HTTP route the Ollama provider uses to stream completions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OllamaRoute {
    /// Ollama's native NDJSON endpoint (`<base>/chat`). Preserves Ollama
    /// extensions like `keep_alive` and `num_ctx` for local hardware tuning.
    #[default]
    Native,
    /// OpenAI-compatible Chat Completions SSE endpoint (`<root>/v1/chat/completions`).
    /// Portable wire shape; loses Ollama-specific options.
    OpenAiCompatible,
}

impl OllamaRoute {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "native" | "api" | "default" => Some(Self::Native),
            "openai_compatible" | "openai" | "v1" | "compat" => Some(Self::OpenAiCompatible),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProviderTransportConfig {
    pub request_max_retries: u8,
    pub stream_max_retries: u8,
    pub stream_idle_timeout_ms: u64,
    /// Idle timeout (ms) for TCP connections sitting in the shared
    /// HTTP pool. `0` disables eviction entirely (connections live
    /// until the remote closes them). Controls pool eviction;
    /// per-event SSE idle gating stays governed by
    /// [`Self::stream_idle_timeout_ms`].
    pub pool_idle_timeout_ms: u64,
    /// Maximum idle TCP connections kept per origin in the shared
    /// HTTP pool. `u32::MAX` is effectively unbounded.
    pub pool_max_idle_per_host: u32,
    /// Upper bound (ms) on any inter-retry sleep. The retry path
    /// honors `Retry-After` / `Retry-After-Ms` hints from the
    /// upstream, but clamps the resulting delay to this value so a
    /// malicious or buggy header (e.g. `Retry-After: 999999`) can't
    /// hang the agent indefinitely.
    pub max_retry_delay_ms: u64,
}

impl Default for ProviderTransportConfig {
    fn default() -> Self {
        Self {
            request_max_retries: DEFAULT_PROVIDER_REQUEST_MAX_RETRIES,
            stream_max_retries: DEFAULT_PROVIDER_STREAM_MAX_RETRIES,
            stream_idle_timeout_ms: DEFAULT_PROVIDER_STREAM_IDLE_TIMEOUT_MS,
            pool_idle_timeout_ms: DEFAULT_PROVIDER_POOL_IDLE_TIMEOUT_MS,
            pool_max_idle_per_host: DEFAULT_PROVIDER_POOL_MAX_IDLE_PER_HOST,
            max_retry_delay_ms: DEFAULT_PROVIDER_MAX_RETRY_DELAY_MS,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelProfile {
    Cheap,
    #[default]
    Balanced,
    Strong,
}

impl ModelProfile {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "cheap" => Some(Self::Cheap),
            "balanced" | "default" => Some(Self::Balanced),
            "strong" => Some(Self::Strong),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cheap => "cheap",
            Self::Balanced => "balanced",
            Self::Strong => "strong",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
    #[serde(rename = "xhigh")]
    XHigh,
}

impl ReasoningEffort {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" | "x-high" | "x_high" => Some(Self::XHigh),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
        }
    }

    /// Anthropic-style thinking budget in tokens for this effort level.
    pub const fn thinking_budget_tokens(self) -> u32 {
        match self {
            Self::Low => 4_096,
            Self::Medium => 16_384,
            Self::High => 32_768,
            Self::XHigh => 60_000,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SettingsFile {
    pub provider: Option<String>,
    pub profile: Option<String>,
    pub model: Option<String>,
    pub model_settings: Option<ModelSettings>,
    pub providers: Option<BTreeMap<String, ProviderSettings>>,
    pub agent: Option<AgentSettings>,
    pub session: Option<SessionSettings>,
    pub context: Option<ContextCompactionSettings>,
    pub subagents: Option<SubagentSettings>,
    pub budgets: Option<BudgetSettings>,
    pub routing: Option<RoutingSettings>,
    pub permissions: Option<PermissionSettings>,
    pub telemetry: Option<TelemetrySettings>,
    pub feedback: Option<FeedbackSettings>,
    pub redaction: Option<RedactionSettings>,
    pub web: Option<WebSettings>,
    pub skills: Option<SkillsSettings>,
    pub graph: Option<GraphSettings>,
    pub cache: Option<CacheSettings>,
    pub tools: Option<ToolSchemaSettings>,
    pub tui: Option<TuiSettings>,
    pub mcp: Option<McpSettings>,
    pub hardening: Option<HardeningSettings>,
    /// Named configuration profiles. A `[profiles.<name>]` TOML section
    /// accepts the same leaves as the root settings file and is merged on
    /// top of the base settings when the user selects `<name>` via
    /// `--profile`. The `profiles` and `profile` leaves of the inner table
    /// are stripped so a profile cannot recursively select another profile.
    pub profiles: Option<BTreeMap<String, SettingsFile>>,
}

impl SettingsFile {
    pub fn load_optional(path: &Path) -> Result<Self> {
        Ok(Self::load_optional_source(path, "settings")?.0)
    }

    fn load_optional_source(
        path: &Path,
        label: &str,
    ) -> Result<(Self, Vec<String>, Vec<ConfigWarning>)> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Self::default(), vec!["defaults".to_string()], Vec::new()));
            }
            Err(error) => return Err(error.into()),
        };
        let source = format!("{label}:{}", path.display());
        let settings = Self::from_toml_str(&text, &source)?;
        let unknowns = take_unknown_fields();
        Ok((
            settings,
            vec!["defaults".to_string(), source.clone()],
            config_warnings_from_unknown_fields(&source, unknowns),
        ))
    }

    pub fn from_toml_str(text: &str, source: &str) -> Result<Self> {
        UNKNOWN_FIELDS.with(|cell| cell.borrow_mut().clear());
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        let table = toml::from_str::<toml::value::Table>(text)
            .map_err(|err| SqueezyError::Config(format!("{source}: {err}")))?;
        Self::from_toml_table(&table, source)
    }

    fn from_toml_table(table: &toml::value::Table, source: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "provider",
                "profile",
                "model",
                "providers",
                "agent",
                "session",
                "context",
                "subagents",
                "budgets",
                "routing",
                "permissions",
                "telemetry",
                "feedback",
                "redaction",
                "web",
                "skills",
                "graph",
                "cache",
                "tools",
                "tui",
                "mcp",
                "hardening",
                "profiles",
            ],
            source,
            "",
        )?;

        let mut settings = Self {
            provider: string_value(table, "provider", source, "provider")?,
            profile: string_value(table, "profile", source, "profile")?,
            ..Self::default()
        };
        if let Some(value) = table.get("model") {
            if let Some(model) = value.as_str() {
                settings.model = Some(model.to_string());
            } else if let Some(model_table) = value.as_table() {
                settings.model_settings =
                    Some(ModelSettings::from_table(model_table, source, "model")?);
            } else {
                return Err(type_error(source, "model", "string or table"));
            }
        }
        settings.providers = providers_settings(table, source)?;
        settings.agent = optional_table(table, "agent", source)?
            .map(|table| AgentSettings::from_table(table, source, "agent"))
            .transpose()?;
        settings.session = optional_table(table, "session", source)?
            .map(|table| SessionSettings::from_table(table, source, "session"))
            .transpose()?;
        settings.context = optional_table(table, "context", source)?
            .map(|table| ContextCompactionSettings::from_table(table, source, "context"))
            .transpose()?;
        settings.subagents = optional_table(table, "subagents", source)?
            .map(|table| SubagentSettings::from_table(table, source, "subagents"))
            .transpose()?;
        settings.budgets = optional_table(table, "budgets", source)?
            .map(|table| BudgetSettings::from_table(table, source, "budgets"))
            .transpose()?;
        settings.routing = optional_table(table, "routing", source)?
            .map(|table| RoutingSettings::from_table(table, source, "routing"))
            .transpose()?;
        settings.permissions = optional_table(table, "permissions", source)?
            .map(|table| PermissionSettings::from_table(table, source, "permissions"))
            .transpose()?;
        settings.telemetry = optional_table(table, "telemetry", source)?
            .map(|table| TelemetrySettings::from_table(table, source, "telemetry"))
            .transpose()?;
        settings.feedback = optional_table(table, "feedback", source)?
            .map(|table| FeedbackSettings::from_table(table, source, "feedback"))
            .transpose()?;
        settings.redaction = optional_table(table, "redaction", source)?
            .map(|table| RedactionSettings::from_table(table, source, "redaction"))
            .transpose()?;
        settings.web = optional_table(table, "web", source)?
            .map(|table| WebSettings::from_table(table, source, "web"))
            .transpose()?;
        settings.skills = optional_table(table, "skills", source)?
            .map(|table| SkillsSettings::from_table(table, source, "skills"))
            .transpose()?;
        settings.graph = optional_table(table, "graph", source)?
            .map(|table| GraphSettings::from_table(table, source, "graph"))
            .transpose()?;
        settings.cache = optional_table(table, "cache", source)?
            .map(|table| CacheSettings::from_table(table, source, "cache"))
            .transpose()?;
        settings.tools = optional_table(table, "tools", source)?
            .map(|table| ToolSchemaSettings::from_table(table, source, "tools"))
            .transpose()?;
        settings.tui = optional_table(table, "tui", source)?
            .map(|table| TuiSettings::from_table(table, source, "tui"))
            .transpose()?;
        settings.mcp = optional_table(table, "mcp", source)?
            .map(|table| McpSettings::from_table(table, source, "mcp"))
            .transpose()?;
        settings.hardening = optional_table(table, "hardening", source)?
            .map(|table| HardeningSettings::from_table(table, source, "hardening"))
            .transpose()?;
        settings.profiles = parse_profiles_map(table, source)?;
        Ok(settings)
    }

    /// Merges the named `[profiles.<name>]` section on top of the base
    /// settings. Returns an error if `name` isn't present in `profiles`.
    /// After application, the `profiles` map is cleared so downstream
    /// merging and `inspect` paths don't re-emit it.
    pub fn apply_profile(&mut self, name: &str) -> Result<()> {
        let Some(profile_map) = self.profiles.take() else {
            return Err(SqueezyError::Config(format!(
                "profile {name:?} not found; no `[profiles.*]` sections are configured"
            )));
        };
        let Some(profile) = profile_map.get(name).cloned() else {
            let mut available: Vec<&String> = profile_map.keys().collect();
            available.sort();
            return Err(SqueezyError::Config(format!(
                "profile {name:?} not found; available profiles: {available:?}"
            )));
        };
        self.merge(profile);
        Ok(())
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.provider, next.provider);
        replace_if_some(&mut self.profile, next.profile);
        replace_if_some(&mut self.model, next.model);
        merge_option(
            &mut self.model_settings,
            next.model_settings,
            ModelSettings::merge,
        );
        merge_provider_maps(&mut self.providers, next.providers);
        merge_option(&mut self.agent, next.agent, AgentSettings::merge);
        merge_option(&mut self.session, next.session, SessionSettings::merge);
        merge_option(
            &mut self.context,
            next.context,
            ContextCompactionSettings::merge,
        );
        merge_option(&mut self.subagents, next.subagents, SubagentSettings::merge);
        merge_option(&mut self.budgets, next.budgets, BudgetSettings::merge);
        merge_option(&mut self.routing, next.routing, RoutingSettings::merge);
        merge_option(
            &mut self.permissions,
            next.permissions,
            PermissionSettings::merge,
        );
        merge_option(
            &mut self.telemetry,
            next.telemetry,
            TelemetrySettings::merge,
        );
        merge_option(&mut self.feedback, next.feedback, FeedbackSettings::merge);
        merge_option(
            &mut self.redaction,
            next.redaction,
            RedactionSettings::merge,
        );
        merge_option(&mut self.web, next.web, WebSettings::merge);
        merge_option(&mut self.skills, next.skills, SkillsSettings::merge);
        merge_option(&mut self.graph, next.graph, GraphSettings::merge);
        merge_option(&mut self.cache, next.cache, CacheSettings::merge);
        merge_option(&mut self.tools, next.tools, ToolSchemaSettings::merge);
        merge_option(&mut self.tui, next.tui, TuiSettings::merge);
        merge_option(&mut self.mcp, next.mcp, McpSettings::merge);
        merge_option(
            &mut self.hardening,
            next.hardening,
            HardeningSettings::merge,
        );
        merge_profiles_maps(&mut self.profiles, next.profiles);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct HardeningSettings {
    pub disable_core_dumps: Option<bool>,
    pub deny_debug_attach: Option<bool>,
}

impl HardeningSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &["disable_core_dumps", "deny_debug_attach"],
            source,
            path,
        )?;
        Ok(Self {
            disable_core_dumps: bool_value(
                table,
                "disable_core_dumps",
                source,
                &field(path, "disable_core_dumps"),
            )?,
            deny_debug_attach: bool_value(
                table,
                "deny_debug_attach",
                source,
                &field(path, "deny_debug_attach"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.disable_core_dumps, next.disable_core_dumps);
        replace_if_some(&mut self.deny_debug_attach, next.deny_debug_attach);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HardeningConfig {
    pub disable_core_dumps: bool,
    pub deny_debug_attach: bool,
}

impl Default for HardeningConfig {
    fn default() -> Self {
        Self {
            disable_core_dumps: true,
            deny_debug_attach: true,
        }
    }
}

impl HardeningConfig {
    fn from_settings(settings: HardeningSettings) -> Self {
        Self {
            disable_core_dumps: settings.disable_core_dumps.unwrap_or(true),
            deny_debug_attach: settings.deny_debug_attach.unwrap_or(true),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSettings {
    pub api_key_env: Option<String>,
    /// Inline plaintext API key, persisted in the user/local TOML layer.
    /// Serde Serialize emits `"<redacted>"` so accidental
    /// `to_string` / inspect paths can't leak it; Deserialize parses the
    /// real value out of TOML normally.
    #[serde(serialize_with = "redact_secret_opt")]
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub default_model: Option<String>,
    pub api_version: Option<String>,
    pub region: Option<String>,
    pub preset: Option<String>,
    pub vertex_project: Option<String>,
    pub vertex_location: Option<String>,
    pub cloudflare_account_id: Option<String>,
    pub cloudflare_gateway_id: Option<String>,
    pub request_max_retries: Option<u8>,
    pub stream_max_retries: Option<u8>,
    pub stream_idle_timeout_ms: Option<u64>,
    pub headers: Option<BTreeMap<String, String>>,
    /// Bedrock only: operator-defined cost-allocation tags threaded into
    /// every ConverseStream request via `set_request_metadata` so AWS
    /// can group invocations by team/env/project. Non-Bedrock providers
    /// ignore this setting.
    pub request_metadata: Option<BTreeMap<String, String>>,
    /// Azure OpenAI only: logical model id → Azure-deployment name. Keeps
    /// callers' `[model]` ids stable even when the deployment is renamed
    /// or differs between environments. See
    /// [`AzureOpenAiConfig::deployment_name_map`] for the runtime
    /// substitution contract.
    pub deployment_name_map: Option<BTreeMap<String, String>>,
    /// Ollama-only: `"native"` (default) or `"openai_compatible"` to pin the
    /// provider to `/v1/chat/completions` SSE instead of `/api/chat` NDJSON.
    pub route_style: Option<String>,
    /// Faux-only: path to a TOML script file consumed by the in-process
    /// faux provider. Other providers ignore this field.
    pub script: Option<String>,
    /// Azure-only: opt in to the Entra ID / managed-identity bearer auth
    /// path. When `Some(true)` the provider emits `Authorization: Bearer`
    /// sourced from `AZURE_OPENAI_BEARER_TOKEN` instead of the default
    /// `api-key` header. Tri-state (`None`) so the higher-precedence
    /// layer can leave the value untouched during settings merge.
    pub use_entra_id: Option<bool>,
    /// OpenAI-only: PayG org slug forwarded as `OpenAI-Organization`.
    /// Ignored by every other provider.
    pub organization: Option<String>,
    /// OpenAI-only: project id forwarded as `OpenAI-Project`. Ignored by
    /// every other provider.
    pub project: Option<String>,
    /// OpenAI-only: `service_tier` body field (`flex`, `priority`,
    /// `default`, `auto`, `scale`). Pass-through string so new tiers
    /// land without a client release. Ignored by every other provider.
    pub service_tier: Option<String>,
}

impl ProviderSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "api_key_env",
                "api_key",
                "base_url",
                "default_model",
                "api_version",
                "region",
                "preset",
                "vertex_project",
                "vertex_location",
                "cloudflare_account_id",
                "cloudflare_gateway_id",
                "request_max_retries",
                "stream_max_retries",
                "stream_idle_timeout_ms",
                "headers",
                "request_metadata",
                "deployment_name_map",
                "route_style",
                "script",
                "use_entra_id",
                "organization",
                "project",
                "service_tier",
            ],
            source,
            path,
        )?;
        let headers = match table.get("headers") {
            None => None,
            Some(toml::Value::Table(table)) => {
                let mut map = BTreeMap::new();
                for (key, value) in table {
                    let header_path = field(path, &format!("headers.{key}"));
                    let toml::Value::String(value) = value else {
                        return Err(SqueezyError::Config(format!(
                            "{source}: {header_path} must map to string values"
                        )));
                    };
                    let resolved = resolve_shell_escape(value.clone(), source, &header_path)?;
                    map.insert(key.clone(), resolved);
                }
                Some(map)
            }
            Some(_) => {
                return Err(SqueezyError::Config(format!(
                    "{source}: {} must be a TOML table of string values",
                    field(path, "headers"),
                )));
            }
        };
        let deployment_name_map = match table.get("deployment_name_map") {
            None => None,
            Some(toml::Value::Table(table)) => {
                let mut map = BTreeMap::new();
                for (key, value) in table {
                    let entry_path = field(path, &format!("deployment_name_map.{key}"));
                    let toml::Value::String(value) = value else {
                        return Err(SqueezyError::Config(format!(
                            "{source}: {entry_path} must map to string deployment names"
                        )));
                    };
                    map.insert(key.clone(), value.clone());
                }
                Some(map)
            }
            Some(_) => {
                return Err(SqueezyError::Config(format!(
                    "{source}: {} must be a TOML table of string deployment names",
                    field(path, "deployment_name_map"),
                )));
            }
        };
        Ok(Self {
            api_key_env: string_value(table, "api_key_env", source, &field(path, "api_key_env"))?,
            api_key: string_value(table, "api_key", source, &field(path, "api_key"))?,
            base_url: string_value(table, "base_url", source, &field(path, "base_url"))?,
            default_model: string_value(
                table,
                "default_model",
                source,
                &field(path, "default_model"),
            )?,
            api_version: string_value(table, "api_version", source, &field(path, "api_version"))?,
            region: string_value(table, "region", source, &field(path, "region"))?,
            preset: string_value(table, "preset", source, &field(path, "preset"))?,
            vertex_project: string_value(
                table,
                "vertex_project",
                source,
                &field(path, "vertex_project"),
            )?,
            vertex_location: string_value(
                table,
                "vertex_location",
                source,
                &field(path, "vertex_location"),
            )?,
            cloudflare_account_id: string_value(
                table,
                "cloudflare_account_id",
                source,
                &field(path, "cloudflare_account_id"),
            )?,
            cloudflare_gateway_id: string_value(
                table,
                "cloudflare_gateway_id",
                source,
                &field(path, "cloudflare_gateway_id"),
            )?,
            request_max_retries: u8_nonnegative_value(
                table,
                "request_max_retries",
                source,
                &field(path, "request_max_retries"),
            )?,
            stream_max_retries: u8_nonnegative_value(
                table,
                "stream_max_retries",
                source,
                &field(path, "stream_max_retries"),
            )?,
            stream_idle_timeout_ms: u64_nonnegative_value(
                table,
                "stream_idle_timeout_ms",
                source,
                &field(path, "stream_idle_timeout_ms"),
            )?,
            headers,
            request_metadata: None,
            deployment_name_map,
            route_style: string_value(table, "route_style", source, &field(path, "route_style"))?,
            script: string_value(table, "script", source, &field(path, "script"))?,
            use_entra_id: bool_value(table, "use_entra_id", source, &field(path, "use_entra_id"))?,
            organization: string_value(
                table,
                "organization",
                source,
                &field(path, "organization"),
            )?,
            project: string_value(table, "project", source, &field(path, "project"))?,
            service_tier: string_value(
                table,
                "service_tier",
                source,
                &field(path, "service_tier"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.api_key_env, next.api_key_env);
        replace_if_some(&mut self.api_key, next.api_key);
        replace_if_some(&mut self.base_url, next.base_url);
        replace_if_some(&mut self.default_model, next.default_model);
        replace_if_some(&mut self.api_version, next.api_version);
        replace_if_some(&mut self.region, next.region);
        replace_if_some(&mut self.preset, next.preset);
        replace_if_some(&mut self.vertex_project, next.vertex_project);
        replace_if_some(&mut self.vertex_location, next.vertex_location);
        replace_if_some(&mut self.cloudflare_account_id, next.cloudflare_account_id);
        replace_if_some(&mut self.cloudflare_gateway_id, next.cloudflare_gateway_id);
        replace_if_some(&mut self.request_max_retries, next.request_max_retries);
        replace_if_some(&mut self.stream_max_retries, next.stream_max_retries);
        replace_if_some(
            &mut self.stream_idle_timeout_ms,
            next.stream_idle_timeout_ms,
        );
        replace_if_some(&mut self.headers, next.headers);
        replace_if_some(&mut self.route_style, next.route_style);
        replace_if_some(&mut self.script, next.script);
        replace_if_some(&mut self.use_entra_id, next.use_entra_id);
        replace_if_some(&mut self.organization, next.organization);
        replace_if_some(&mut self.project, next.project);
        replace_if_some(&mut self.service_tier, next.service_tier);
    }
}

/// Serde Serializer hook for `Option<String>` fields holding a secret.
/// `None` round-trips as null/absent; `Some(_)` emits the literal
/// `"<redacted>"` so any path that serializes the struct (debug dumps,
/// inspect output, accidental `to_string`) can't leak the plaintext.
fn redact_secret_opt<S>(
    value: &Option<String>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match value {
        Some(_) => serializer.serialize_some("<redacted>"),
        None => serializer.serialize_none(),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ModelSettings {
    pub provider: Option<String>,
    pub model: Option<String>,
    /// Cheap model id used for low-stakes background calls (compaction
    /// summary, AI reviewer classifier, auto-approver). When `None` the
    /// per-provider built-in (`small_fast_model_for_provider`) applies.
    pub small_fast_model: Option<String>,
    pub profile: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub max_output_tokens: Option<u32>,
    pub stream_idle_timeout_ms: Option<u64>,
    pub store_responses: Option<bool>,
    pub selection_version: Option<u32>,
    /// Forwarded to the provider as `tool_choice` whenever tools are
    /// advertised on the request. `None` omits the field (provider
    /// default — typically `auto`). Set to `"required"` for tool-shy
    /// models routed through chat-completions aggregators (Qwen via
    /// OpenRouter, smaller MoEs) that otherwise emit a chatty preamble
    /// and finish with `stop` without calling any tool. Other accepted
    /// values: `"auto"`, `"none"`.
    pub tool_choice: Option<String>,
}

impl ModelSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "provider",
                "model",
                "small_fast_model",
                "profile",
                "reasoning_effort",
                "max_output_tokens",
                "stream_idle_timeout_ms",
                "store_responses",
                "selection_version",
                "tool_choice",
            ],
            source,
            path,
        )?;
        let profile = string_value(table, "profile", source, &field(path, "profile"))?;
        if let Some(profile) = &profile
            && ModelProfile::parse(profile).is_none()
        {
            return Err(SqueezyError::Config(format!(
                "{source}: {} invalid profile {profile:?}; expected cheap, balanced, or strong",
                field(path, "profile")
            )));
        }
        let reasoning_effort = reasoning_effort_value(
            table,
            "reasoning_effort",
            source,
            &field(path, "reasoning_effort"),
        )?;
        Ok(Self {
            provider: string_value(table, "provider", source, &field(path, "provider"))?,
            model: string_value(table, "model", source, &field(path, "model"))?,
            small_fast_model: string_value(
                table,
                "small_fast_model",
                source,
                &field(path, "small_fast_model"),
            )?,
            profile,
            reasoning_effort,
            max_output_tokens: u32_value(
                table,
                "max_output_tokens",
                source,
                &field(path, "max_output_tokens"),
            )?,
            stream_idle_timeout_ms: u64_value(
                table,
                "stream_idle_timeout_ms",
                source,
                &field(path, "stream_idle_timeout_ms"),
            )?,
            store_responses: bool_value(
                table,
                "store_responses",
                source,
                &field(path, "store_responses"),
            )?,
            selection_version: u32_value(
                table,
                "selection_version",
                source,
                &field(path, "selection_version"),
            )?,
            tool_choice: tool_choice_value(
                table,
                "tool_choice",
                source,
                &field(path, "tool_choice"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.provider, next.provider);
        replace_if_some(&mut self.model, next.model);
        replace_if_some(&mut self.small_fast_model, next.small_fast_model);
        replace_if_some(&mut self.profile, next.profile);
        replace_if_some(&mut self.reasoning_effort, next.reasoning_effort);
        replace_if_some(&mut self.max_output_tokens, next.max_output_tokens);
        replace_if_some(
            &mut self.stream_idle_timeout_ms,
            next.stream_idle_timeout_ms,
        );
        replace_if_some(&mut self.store_responses, next.store_responses);
        replace_if_some(&mut self.selection_version, next.selection_version);
        replace_if_some(&mut self.tool_choice, next.tool_choice);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AgentSettings {
    pub exploration_compiler: Option<bool>,
}

impl AgentSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(table, &["exploration_compiler"], source, path)?;
        Ok(Self {
            exploration_compiler: bool_value(
                table,
                "exploration_compiler",
                source,
                &field(path, "exploration_compiler"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.exploration_compiler, next.exploration_compiler);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BudgetSettings {
    pub max_parallel_tools: Option<usize>,
    pub tool_spill_threshold_bytes: Option<usize>,
    pub tool_preview_bytes: Option<usize>,
    pub max_tool_result_bytes_per_round: Option<usize>,
    pub tool_output_retention_days: Option<u64>,
    pub max_tool_calls_per_turn: Option<u64>,
    pub max_tool_bytes_read_per_turn: Option<u64>,
    pub max_search_files_per_turn: Option<u64>,
    pub max_session_cost_usd_micros: Option<u64>,
    pub cost_warn_percent: Option<u8>,
}

impl BudgetSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "max_parallel_tools",
                "tool_spill_threshold_bytes",
                "tool_preview_bytes",
                "max_tool_result_bytes_per_round",
                "tool_output_retention_days",
                "max_tool_calls_per_turn",
                "max_tool_bytes_read_per_turn",
                "max_search_files_per_turn",
                "max_session_cost_usd_micros",
                "cost_warn_percent",
            ],
            source,
            path,
        )?;
        Ok(Self {
            max_parallel_tools: usize_value(
                table,
                "max_parallel_tools",
                source,
                &field(path, "max_parallel_tools"),
            )?,
            tool_spill_threshold_bytes: usize_value(
                table,
                "tool_spill_threshold_bytes",
                source,
                &field(path, "tool_spill_threshold_bytes"),
            )?,
            tool_preview_bytes: usize_value(
                table,
                "tool_preview_bytes",
                source,
                &field(path, "tool_preview_bytes"),
            )?,
            max_tool_result_bytes_per_round: usize_value(
                table,
                "max_tool_result_bytes_per_round",
                source,
                &field(path, "max_tool_result_bytes_per_round"),
            )?,
            tool_output_retention_days: u64_value(
                table,
                "tool_output_retention_days",
                source,
                &field(path, "tool_output_retention_days"),
            )?,
            max_tool_calls_per_turn: u64_value(
                table,
                "max_tool_calls_per_turn",
                source,
                &field(path, "max_tool_calls_per_turn"),
            )?,
            max_tool_bytes_read_per_turn: u64_value(
                table,
                "max_tool_bytes_read_per_turn",
                source,
                &field(path, "max_tool_bytes_read_per_turn"),
            )?,
            max_search_files_per_turn: u64_value(
                table,
                "max_search_files_per_turn",
                source,
                &field(path, "max_search_files_per_turn"),
            )?,
            max_session_cost_usd_micros: u64_value(
                table,
                "max_session_cost_usd_micros",
                source,
                &field(path, "max_session_cost_usd_micros"),
            )?,
            cost_warn_percent: percent_value(
                table,
                "cost_warn_percent",
                source,
                &field(path, "cost_warn_percent"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.max_parallel_tools, next.max_parallel_tools);
        replace_if_some(
            &mut self.tool_spill_threshold_bytes,
            next.tool_spill_threshold_bytes,
        );
        replace_if_some(&mut self.tool_preview_bytes, next.tool_preview_bytes);
        replace_if_some(
            &mut self.max_tool_result_bytes_per_round,
            next.max_tool_result_bytes_per_round,
        );
        replace_if_some(
            &mut self.tool_output_retention_days,
            next.tool_output_retention_days,
        );
        replace_if_some(
            &mut self.max_tool_calls_per_turn,
            next.max_tool_calls_per_turn,
        );
        replace_if_some(
            &mut self.max_tool_bytes_read_per_turn,
            next.max_tool_bytes_read_per_turn,
        );
        replace_if_some(
            &mut self.max_search_files_per_turn,
            next.max_search_files_per_turn,
        );
        replace_if_some(
            &mut self.max_session_cost_usd_micros,
            next.max_session_cost_usd_micros,
        );
        replace_if_some(&mut self.cost_warn_percent, next.cost_warn_percent);
    }
}

/// Per-turn model routing config. Resolved from `[routing]` in TOML and
/// the matching `SQUEEZY_ROUTING_*` env vars; the agent crate's
/// `turn_router` module reads these knobs to decide whether to dispatch
/// the current turn on the cheap tier and when to hand back to the
/// parent model after a false positive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingConfig {
    pub auto_cheap: bool,
    pub auto_cheap_llm_judge: bool,
    /// Hard ceiling on tool calls a cheap-routed turn may issue before
    /// the escalation detector hands back to the parent model. `0`
    /// (default) means "derive at runtime as `max_tool_calls_per_turn /
    /// 4`". Resolves via `RoutingConfig::resolved_cheap_escalation_tool_calls`.
    pub cheap_escalation_tool_calls: u64,
    pub cheap_escalation_error_threshold: u8,
    pub escalation_sticky_turns: u8,
    pub bypass_for_images: bool,
    pub heuristic_max_chars: u32,
    pub judge_max_chars: u32,
    /// User-extended heuristic verb whitelist. The built-in whitelist
    /// is deliberately narrow because false positives bypass the LLM
    /// judge — adding an entry here widens the heuristic surface but
    /// the matched prompt still has to clear the same ambiguity-marker,
    /// compound-connector, word-count, and sentence-count guards as a
    /// built-in match. Empty by default. Configured via
    /// `[routing].extra_heuristic_verbs = ["deploy", "tail"]` in TOML
    /// or `SQUEEZY_ROUTING_EXTRA_HEURISTIC_VERBS=deploy,tail` env.
    pub extra_heuristic_verbs: Vec<String>,
}

impl RoutingConfig {
    fn from_settings_and_env(
        settings: RoutingSettings,
        get_var: &mut impl FnMut(&str) -> Option<String>,
    ) -> Self {
        Self {
            auto_cheap: get_var("SQUEEZY_ROUTING_AUTO_CHEAP")
                .as_deref()
                .map(parse_enabled_bool)
                .unwrap_or(settings.auto_cheap.unwrap_or(DEFAULT_ROUTING_AUTO_CHEAP)),
            auto_cheap_llm_judge: get_var("SQUEEZY_ROUTING_AUTO_CHEAP_LLM_JUDGE")
                .as_deref()
                .map(parse_enabled_bool)
                .unwrap_or(
                    settings
                        .auto_cheap_llm_judge
                        .unwrap_or(DEFAULT_ROUTING_AUTO_CHEAP_LLM_JUDGE),
                ),
            cheap_escalation_tool_calls: parse_u64(
                get_var("SQUEEZY_ROUTING_CHEAP_ESCALATION_TOOL_CALLS"),
                settings.cheap_escalation_tool_calls.unwrap_or(0),
            ),
            cheap_escalation_error_threshold: get_var(
                "SQUEEZY_ROUTING_CHEAP_ESCALATION_ERROR_THRESHOLD",
            )
            .as_deref()
            .and_then(|raw| raw.parse::<u8>().ok())
            .or(settings.cheap_escalation_error_threshold)
            .unwrap_or(DEFAULT_ROUTING_CHEAP_ESCALATION_ERROR_THRESHOLD),
            escalation_sticky_turns: get_var("SQUEEZY_ROUTING_ESCALATION_STICKY_TURNS")
                .as_deref()
                .and_then(|raw| raw.parse::<u8>().ok())
                .or(settings.escalation_sticky_turns)
                .unwrap_or(DEFAULT_ROUTING_ESCALATION_STICKY_TURNS),
            bypass_for_images: get_var("SQUEEZY_ROUTING_BYPASS_FOR_IMAGES")
                .as_deref()
                .map(parse_enabled_bool)
                .unwrap_or(
                    settings
                        .bypass_for_images
                        .unwrap_or(DEFAULT_ROUTING_BYPASS_FOR_IMAGES),
                ),
            heuristic_max_chars: get_var("SQUEEZY_ROUTING_HEURISTIC_MAX_CHARS")
                .as_deref()
                .and_then(|raw| raw.parse::<u32>().ok())
                .or(settings.heuristic_max_chars)
                .unwrap_or(DEFAULT_ROUTING_HEURISTIC_MAX_CHARS),
            judge_max_chars: get_var("SQUEEZY_ROUTING_JUDGE_MAX_CHARS")
                .as_deref()
                .and_then(|raw| raw.parse::<u32>().ok())
                .or(settings.judge_max_chars)
                .unwrap_or(DEFAULT_ROUTING_JUDGE_MAX_CHARS),
            extra_heuristic_verbs: get_var("SQUEEZY_ROUTING_EXTRA_HEURISTIC_VERBS")
                .map(|raw| {
                    raw.split(',')
                        .map(|verb| verb.trim().to_string())
                        .filter(|verb| !verb.is_empty())
                        .collect()
                })
                .or(settings.extra_heuristic_verbs)
                .unwrap_or_default(),
        }
    }

    /// Resolves the in-turn escalation ceiling for tool calls. When the
    /// user leaves `cheap_escalation_tool_calls` at the `0` sentinel we
    /// derive a value from the parent's `max_tool_calls_per_turn` rather
    /// than hard-coding a number, so the threshold scales with the
    /// user's existing budget choices.
    pub fn resolved_cheap_escalation_tool_calls(&self, max_tool_calls_per_turn: u64) -> u64 {
        if self.cheap_escalation_tool_calls > 0 {
            self.cheap_escalation_tool_calls
        } else {
            (max_tool_calls_per_turn / 4).max(1)
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RoutingSettings {
    pub auto_cheap: Option<bool>,
    pub auto_cheap_llm_judge: Option<bool>,
    pub cheap_escalation_tool_calls: Option<u64>,
    pub cheap_escalation_error_threshold: Option<u8>,
    pub escalation_sticky_turns: Option<u8>,
    pub bypass_for_images: Option<bool>,
    pub heuristic_max_chars: Option<u32>,
    pub judge_max_chars: Option<u32>,
    pub extra_heuristic_verbs: Option<Vec<String>>,
}

impl RoutingSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "auto_cheap",
                "auto_cheap_llm_judge",
                "cheap_escalation_tool_calls",
                "cheap_escalation_error_threshold",
                "escalation_sticky_turns",
                "bypass_for_images",
                "heuristic_max_chars",
                "judge_max_chars",
                "extra_heuristic_verbs",
            ],
            source,
            path,
        )?;
        Ok(Self {
            auto_cheap: bool_value(table, "auto_cheap", source, &field(path, "auto_cheap"))?,
            auto_cheap_llm_judge: bool_value(
                table,
                "auto_cheap_llm_judge",
                source,
                &field(path, "auto_cheap_llm_judge"),
            )?,
            cheap_escalation_tool_calls: u64_value(
                table,
                "cheap_escalation_tool_calls",
                source,
                &field(path, "cheap_escalation_tool_calls"),
            )?,
            cheap_escalation_error_threshold: u8_value(
                table,
                "cheap_escalation_error_threshold",
                source,
                &field(path, "cheap_escalation_error_threshold"),
            )?,
            escalation_sticky_turns: u8_value(
                table,
                "escalation_sticky_turns",
                source,
                &field(path, "escalation_sticky_turns"),
            )?,
            bypass_for_images: bool_value(
                table,
                "bypass_for_images",
                source,
                &field(path, "bypass_for_images"),
            )?,
            heuristic_max_chars: u32_value(
                table,
                "heuristic_max_chars",
                source,
                &field(path, "heuristic_max_chars"),
            )?,
            judge_max_chars: u32_value(
                table,
                "judge_max_chars",
                source,
                &field(path, "judge_max_chars"),
            )?,
            extra_heuristic_verbs: string_array_value(
                table,
                "extra_heuristic_verbs",
                source,
                &field(path, "extra_heuristic_verbs"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.auto_cheap, next.auto_cheap);
        replace_if_some(&mut self.auto_cheap_llm_judge, next.auto_cheap_llm_judge);
        replace_if_some(
            &mut self.cheap_escalation_tool_calls,
            next.cheap_escalation_tool_calls,
        );
        replace_if_some(
            &mut self.cheap_escalation_error_threshold,
            next.cheap_escalation_error_threshold,
        );
        replace_if_some(
            &mut self.escalation_sticky_turns,
            next.escalation_sticky_turns,
        );
        replace_if_some(&mut self.bypass_for_images, next.bypass_for_images);
        replace_if_some(&mut self.heuristic_max_chars, next.heuristic_max_chars);
        replace_if_some(&mut self.judge_max_chars, next.judge_max_chars);
        merge_string_lists(&mut self.extra_heuristic_verbs, next.extra_heuristic_verbs);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSchemaConfig {
    pub lazy_schema_loading: bool,
    pub core: Vec<String>,
    pub discoverable: Vec<String>,
}

impl Default for ToolSchemaConfig {
    fn default() -> Self {
        Self {
            lazy_schema_loading: true,
            core: DEFAULT_CORE_TOOL_NAMES
                .iter()
                .map(|name| (*name).to_string())
                .collect(),
            discoverable: Vec::new(),
        }
    }
}

impl ToolSchemaConfig {
    pub fn from_settings(settings: ToolSchemaSettings) -> Result<Self> {
        let defaults = Self::default();
        if let (Some(core), Some(discoverable)) = (&settings.core, &settings.discoverable) {
            reject_tool_schema_overlap(core, discoverable)?;
        }
        let mut core = defaults.core;
        if let Some(additional_core) = settings.core {
            for tool in additional_core {
                if !core.contains(&tool) {
                    core.push(tool);
                }
            }
        }
        let discoverable = settings.discoverable.unwrap_or(defaults.discoverable);
        core.retain(|tool| !discoverable.contains(tool));
        Ok(Self {
            lazy_schema_loading: settings
                .lazy_schema_loading
                .unwrap_or(defaults.lazy_schema_loading),
            core,
            discoverable,
        })
    }

    pub fn core_contains(&self, name: &str) -> bool {
        self.core.iter().any(|tool| tool == name)
    }

    pub fn discoverable_contains(&self, name: &str) -> bool {
        self.discoverable.iter().any(|tool| tool == name)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ToolSchemaSettings {
    pub checkpoints_enabled: Option<bool>,
    pub lazy_schema_loading: Option<bool>,
    pub core: Option<Vec<String>>,
    pub discoverable: Option<Vec<String>>,
}

impl ToolSchemaSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "checkpoints_enabled",
                "lazy_schema_loading",
                "core",
                "discoverable",
            ],
            source,
            path,
        )?;
        Ok(Self {
            checkpoints_enabled: bool_value(
                table,
                "checkpoints_enabled",
                source,
                &field(path, "checkpoints_enabled"),
            )?,
            lazy_schema_loading: bool_value(
                table,
                "lazy_schema_loading",
                source,
                &field(path, "lazy_schema_loading"),
            )?,
            core: string_array_value(table, "core", source, &field(path, "core"))?,
            discoverable: string_array_value(
                table,
                "discoverable",
                source,
                &field(path, "discoverable"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.checkpoints_enabled, next.checkpoints_enabled);
        replace_if_some(&mut self.lazy_schema_loading, next.lazy_schema_loading);
        merge_string_lists(&mut self.core, next.core);
        merge_string_lists(&mut self.discoverable, next.discoverable);
    }
}

fn reject_tool_schema_overlap(core: &[String], discoverable: &[String]) -> Result<()> {
    let core = core.iter().collect::<BTreeSet<_>>();
    let overlap = discoverable
        .iter()
        .filter(|name| core.contains(name))
        .cloned()
        .collect::<Vec<_>>();
    if overlap.is_empty() {
        return Ok(());
    }
    Err(SqueezyError::Config(format!(
        "[tools] core and discoverable overlap: {}",
        overlap.join(", ")
    )))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionMode {
    Allow,
    Ask,
    Deny,
}

impl PermissionMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "allow" | "allowed" => Some(Self::Allow),
            "ask" | "prompt" | "confirm" => Some(Self::Ask),
            "deny" | "denied" | "refuse" => Some(Self::Deny),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
        }
    }
}

pub type PermissionAction = PermissionMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionPolicyMode {
    Default,
    AutoReview,
    FullAccess,
    Custom,
}

impl PermissionPolicyMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "default" | "workspace" | "workspace_write" | "workspace-write" => Some(Self::Default),
            "auto_review" | "auto-review" | "autoreview" | "auto" => Some(Self::AutoReview),
            "full_access" | "full-access" | "danger_full_access" | "danger-full-access" => {
                Some(Self::FullAccess)
            }
            "custom" | "granular" => Some(Self::Custom),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AutoReview => "auto_review",
            Self::FullAccess => "full_access",
            Self::Custom => "custom",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    Plan,
    #[default]
    Build,
}

impl SessionMode {
    /// Parse the two canonical session-mode names. The accepted values are
    /// only `plan` and `build` (case-insensitive, surrounding whitespace
    /// ignored) so that the user-visible vocabulary stays in sync with
    /// `as_str`, error messages, and config docs. Anything else returns
    /// `None` so configuration loaders can surface a precise error.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "plan" => Some(Self::Plan),
            "build" => Some(Self::Build),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Build => "build",
        }
    }

    /// Compact wire form for lock-free storage in an `AtomicU8`. `from_u8`
    /// rejects unknown discriminants and the caller decides on a safe
    /// default; see `Agent::session_mode` for the in-process use.
    pub const fn to_u8(self) -> u8 {
        match self {
            Self::Plan => 0,
            Self::Build => 1,
        }
    }

    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Plan),
            1 => Some(Self::Build),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionResumePicker {
    #[default]
    Ask,
    Never,
}

impl SessionResumePicker {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "ask" | "always" | "on" | "true" => Some(Self::Ask),
            "never" | "off" | "false" => Some(Self::Never),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Never => "never",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SessionSettings {
    pub mode: Option<SessionMode>,
    pub resume_picker: Option<SessionResumePicker>,
    pub log_dir: Option<PathBuf>,
    pub log_retention_days: Option<u64>,
    pub log_retention_archive_days: Option<u64>,
    pub max_event_bytes: Option<usize>,
    pub max_session_bytes: Option<usize>,
}

impl SessionSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "mode",
                "resume_picker",
                "log_dir",
                "log_retention_days",
                "log_retention_archive_days",
                "max_event_bytes",
                "max_session_bytes",
            ],
            source,
            path,
        )?;
        let mode = match table.get("mode") {
            Some(value) => {
                let value = value
                    .as_str()
                    .ok_or_else(|| type_error(source, &field(path, "mode"), "string"))?;
                Some(parse_session_mode_value(
                    value,
                    source,
                    &field(path, "mode"),
                )?)
            }
            None => None,
        };
        Ok(Self {
            mode,
            resume_picker: session_resume_picker_value(
                table,
                "resume_picker",
                source,
                &field(path, "resume_picker"),
            )?,
            log_dir: path_value(table, "log_dir", source, &field(path, "log_dir"))?,
            log_retention_days: u64_value(
                table,
                "log_retention_days",
                source,
                &field(path, "log_retention_days"),
            )?,
            log_retention_archive_days: u64_value(
                table,
                "log_retention_archive_days",
                source,
                &field(path, "log_retention_archive_days"),
            )?,
            max_event_bytes: usize_value(
                table,
                "max_event_bytes",
                source,
                &field(path, "max_event_bytes"),
            )?,
            max_session_bytes: usize_value(
                table,
                "max_session_bytes",
                source,
                &field(path, "max_session_bytes"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.mode, next.mode);
        replace_if_some(&mut self.resume_picker, next.resume_picker);
        replace_if_some(&mut self.log_dir, next.log_dir);
        replace_if_some(&mut self.log_retention_days, next.log_retention_days);
        replace_if_some(
            &mut self.log_retention_archive_days,
            next.log_retention_archive_days,
        );
        replace_if_some(&mut self.max_event_bytes, next.max_event_bytes);
        replace_if_some(&mut self.max_session_bytes, next.max_session_bytes);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLogConfig {
    pub log_dir: Option<PathBuf>,
    pub log_retention_days: u64,
    /// Days an archived session lingers before the retention sweep
    /// permanently deletes it. Setting this to `0` disables the archive
    /// sweep; archived sessions are then retained until removed by hand.
    #[serde(default = "default_log_retention_archive_days")]
    pub log_retention_archive_days: u64,
    pub max_event_bytes: usize,
    pub max_session_bytes: usize,
}

fn default_log_retention_archive_days() -> u64 {
    DEFAULT_SESSION_LOG_RETENTION_ARCHIVE_DAYS
}

impl SessionLogConfig {
    fn from_settings(settings: &SessionSettings) -> Self {
        Self {
            log_dir: settings.log_dir.clone(),
            log_retention_days: settings
                .log_retention_days
                .filter(|value| *value > 0)
                .unwrap_or(DEFAULT_SESSION_LOG_RETENTION_DAYS),
            // `0` is a valid explicit value here: it disables the archive
            // sweep and lets archived sessions accumulate until the user
            // removes them by hand. Anything else falls back to the
            // built-in default rather than silently being treated as zero.
            log_retention_archive_days: settings
                .log_retention_archive_days
                .unwrap_or(DEFAULT_SESSION_LOG_RETENTION_ARCHIVE_DAYS),
            max_event_bytes: settings
                .max_event_bytes
                .filter(|value| *value > 0)
                .unwrap_or(DEFAULT_SESSION_MAX_EVENT_BYTES),
            max_session_bytes: settings
                .max_session_bytes
                .filter(|value| *value > 0)
                .unwrap_or(DEFAULT_SESSION_MAX_SESSION_BYTES),
        }
    }
}

impl Default for SessionLogConfig {
    fn default() -> Self {
        Self {
            log_dir: None,
            log_retention_days: DEFAULT_SESSION_LOG_RETENTION_DAYS,
            log_retention_archive_days: DEFAULT_SESSION_LOG_RETENTION_ARCHIVE_DAYS,
            max_event_bytes: DEFAULT_SESSION_MAX_EVENT_BYTES,
            max_session_bytes: DEFAULT_SESSION_MAX_SESSION_BYTES,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCompactionSettings {
    pub compaction_enabled: Option<bool>,
    pub compaction_estimated_tokens: Option<u64>,
    pub compaction_min_items: Option<usize>,
    pub compaction_recent_items: Option<usize>,
    pub compaction_max_summary_bytes: Option<usize>,
    pub repo_doc_max_bytes: Option<usize>,
    pub user_memory_max_bytes: Option<usize>,
    pub enabled_mid_turn: Option<bool>,
    pub model_context_window: Option<u64>,
    pub threshold_percent: Option<u8>,
    pub strategy: Option<CompactionStrategy>,
    pub model_assisted_model: Option<String>,
    pub model_assisted_max_output_tokens: Option<u32>,
    pub model_assisted_timeout_secs: Option<u64>,
    pub layered_fallback_extractive_threshold_tokens: Option<u32>,
    pub micro_compaction_enabled: Option<bool>,
    pub micro_compaction_threshold_percent: Option<u8>,
    pub micro_compaction_keep_recent: Option<usize>,
}

impl ContextCompactionSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "compaction_enabled",
                "compaction_estimated_tokens",
                "compaction_min_items",
                "compaction_recent_items",
                "compaction_max_summary_bytes",
                "repo_doc_max_bytes",
                "user_memory_max_bytes",
                "enabled_mid_turn",
                "model_context_window",
                "threshold_percent",
                "strategy",
                "model_assisted_model",
                "model_assisted_max_output_tokens",
                "model_assisted_timeout_secs",
                "layered_fallback_extractive_threshold_tokens",
                "micro_compaction_enabled",
                "micro_compaction_threshold_percent",
                "micro_compaction_keep_recent",
            ],
            source,
            path,
        )?;
        Ok(Self {
            compaction_enabled: bool_value(
                table,
                "compaction_enabled",
                source,
                &field(path, "compaction_enabled"),
            )?,
            compaction_estimated_tokens: u64_value(
                table,
                "compaction_estimated_tokens",
                source,
                &field(path, "compaction_estimated_tokens"),
            )?,
            compaction_min_items: usize_value(
                table,
                "compaction_min_items",
                source,
                &field(path, "compaction_min_items"),
            )?,
            compaction_recent_items: usize_value(
                table,
                "compaction_recent_items",
                source,
                &field(path, "compaction_recent_items"),
            )?,
            compaction_max_summary_bytes: usize_value(
                table,
                "compaction_max_summary_bytes",
                source,
                &field(path, "compaction_max_summary_bytes"),
            )?,
            repo_doc_max_bytes: usize_value(
                table,
                "repo_doc_max_bytes",
                source,
                &field(path, "repo_doc_max_bytes"),
            )?,
            user_memory_max_bytes: usize_value(
                table,
                "user_memory_max_bytes",
                source,
                &field(path, "user_memory_max_bytes"),
            )?,
            enabled_mid_turn: bool_value(
                table,
                "enabled_mid_turn",
                source,
                &field(path, "enabled_mid_turn"),
            )?,
            model_context_window: u64_value(
                table,
                "model_context_window",
                source,
                &field(path, "model_context_window"),
            )?,
            threshold_percent: u8_value(
                table,
                "threshold_percent",
                source,
                &field(path, "threshold_percent"),
            )?,
            strategy: {
                let raw = string_value(table, "strategy", source, &field(path, "strategy"))?;
                match raw {
                    None => None,
                    Some(value) => Some(CompactionStrategy::parse(&value).ok_or_else(|| {
                        SqueezyError::Config(format!(
                            "{source}: {}: expected one of extractive | model_assisted | layered_fallback",
                            field(path, "strategy")
                        ))
                    })?),
                }
            },
            model_assisted_model: string_value(
                table,
                "model_assisted_model",
                source,
                &field(path, "model_assisted_model"),
            )?,
            model_assisted_max_output_tokens: u32_value(
                table,
                "model_assisted_max_output_tokens",
                source,
                &field(path, "model_assisted_max_output_tokens"),
            )?,
            model_assisted_timeout_secs: u64_value(
                table,
                "model_assisted_timeout_secs",
                source,
                &field(path, "model_assisted_timeout_secs"),
            )?,
            layered_fallback_extractive_threshold_tokens: u32_value(
                table,
                "layered_fallback_extractive_threshold_tokens",
                source,
                &field(path, "layered_fallback_extractive_threshold_tokens"),
            )?,
            micro_compaction_enabled: bool_value(
                table,
                "micro_compaction_enabled",
                source,
                &field(path, "micro_compaction_enabled"),
            )?,
            micro_compaction_threshold_percent: u8_value(
                table,
                "micro_compaction_threshold_percent",
                source,
                &field(path, "micro_compaction_threshold_percent"),
            )?,
            micro_compaction_keep_recent: usize_value(
                table,
                "micro_compaction_keep_recent",
                source,
                &field(path, "micro_compaction_keep_recent"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.compaction_enabled, next.compaction_enabled);
        replace_if_some(
            &mut self.compaction_estimated_tokens,
            next.compaction_estimated_tokens,
        );
        replace_if_some(&mut self.compaction_min_items, next.compaction_min_items);
        replace_if_some(
            &mut self.compaction_recent_items,
            next.compaction_recent_items,
        );
        replace_if_some(
            &mut self.compaction_max_summary_bytes,
            next.compaction_max_summary_bytes,
        );
        replace_if_some(&mut self.repo_doc_max_bytes, next.repo_doc_max_bytes);
        replace_if_some(&mut self.user_memory_max_bytes, next.user_memory_max_bytes);
        replace_if_some(&mut self.enabled_mid_turn, next.enabled_mid_turn);
        replace_if_some(&mut self.model_context_window, next.model_context_window);
        replace_if_some(&mut self.threshold_percent, next.threshold_percent);
        replace_if_some(&mut self.strategy, next.strategy);
        replace_if_some(&mut self.model_assisted_model, next.model_assisted_model);
        replace_if_some(
            &mut self.model_assisted_max_output_tokens,
            next.model_assisted_max_output_tokens,
        );
        replace_if_some(
            &mut self.model_assisted_timeout_secs,
            next.model_assisted_timeout_secs,
        );
        replace_if_some(
            &mut self.layered_fallback_extractive_threshold_tokens,
            next.layered_fallback_extractive_threshold_tokens,
        );
        replace_if_some(
            &mut self.micro_compaction_enabled,
            next.micro_compaction_enabled,
        );
        replace_if_some(
            &mut self.micro_compaction_threshold_percent,
            next.micro_compaction_threshold_percent,
        );
        replace_if_some(
            &mut self.micro_compaction_keep_recent,
            next.micro_compaction_keep_recent,
        );
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentSettings {
    pub enabled: Option<bool>,
    pub explore_enabled: Option<bool>,
    pub explore_model: Option<String>,
    pub max_concurrent: Option<usize>,
    pub max_tool_calls_per_call: Option<u64>,
    pub max_tool_bytes_read_per_call: Option<u64>,
    pub max_search_files_per_call: Option<u64>,
    pub max_model_rounds: Option<usize>,
    pub max_summary_tokens: Option<u32>,
    pub max_runtime_secs: Option<u64>,
    pub include_transcript: Option<bool>,
}

impl SubagentSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "enabled",
                "explore_enabled",
                "explore_model",
                "max_concurrent",
                "max_tool_calls_per_call",
                "max_tool_bytes_read_per_call",
                "max_search_files_per_call",
                "max_model_rounds",
                "max_summary_tokens",
                "max_runtime_secs",
                "include_transcript",
            ],
            source,
            path,
        )?;
        Ok(Self {
            enabled: bool_value(table, "enabled", source, &field(path, "enabled"))?,
            explore_enabled: bool_value(
                table,
                "explore_enabled",
                source,
                &field(path, "explore_enabled"),
            )?,
            explore_model: string_value(
                table,
                "explore_model",
                source,
                &field(path, "explore_model"),
            )?,
            max_concurrent: usize_value(
                table,
                "max_concurrent",
                source,
                &field(path, "max_concurrent"),
            )?,
            max_tool_calls_per_call: u64_value(
                table,
                "max_tool_calls_per_call",
                source,
                &field(path, "max_tool_calls_per_call"),
            )?,
            max_tool_bytes_read_per_call: u64_value(
                table,
                "max_tool_bytes_read_per_call",
                source,
                &field(path, "max_tool_bytes_read_per_call"),
            )?,
            max_search_files_per_call: u64_value(
                table,
                "max_search_files_per_call",
                source,
                &field(path, "max_search_files_per_call"),
            )?,
            max_model_rounds: usize_value(
                table,
                "max_model_rounds",
                source,
                &field(path, "max_model_rounds"),
            )?,
            max_summary_tokens: u32_value(
                table,
                "max_summary_tokens",
                source,
                &field(path, "max_summary_tokens"),
            )?,
            max_runtime_secs: u64_value(
                table,
                "max_runtime_secs",
                source,
                &field(path, "max_runtime_secs"),
            )?,
            include_transcript: bool_value(
                table,
                "include_transcript",
                source,
                &field(path, "include_transcript"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.enabled, next.enabled);
        replace_if_some(&mut self.explore_enabled, next.explore_enabled);
        replace_if_some(&mut self.explore_model, next.explore_model);
        replace_if_some(&mut self.max_concurrent, next.max_concurrent);
        replace_if_some(
            &mut self.max_tool_calls_per_call,
            next.max_tool_calls_per_call,
        );
        replace_if_some(
            &mut self.max_tool_bytes_read_per_call,
            next.max_tool_bytes_read_per_call,
        );
        replace_if_some(
            &mut self.max_search_files_per_call,
            next.max_search_files_per_call,
        );
        replace_if_some(&mut self.max_model_rounds, next.max_model_rounds);
        replace_if_some(&mut self.max_summary_tokens, next.max_summary_tokens);
        replace_if_some(&mut self.max_runtime_secs, next.max_runtime_secs);
        replace_if_some(&mut self.include_transcript, next.include_transcript);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentConfig {
    pub enabled: bool,
    pub explore_enabled: bool,
    pub explore_model: Option<String>,
    pub max_concurrent: usize,
    pub max_tool_calls_per_call: u64,
    pub max_tool_bytes_read_per_call: u64,
    pub max_search_files_per_call: u64,
    pub max_model_rounds: usize,
    pub max_summary_tokens: u32,
    /// Wall-clock cap on a single subagent run. `None` (set via TOML
    /// `max_runtime_secs = 0` or `SQUEEZY_SUBAGENT_MAX_RUNTIME_SECS=0`)
    /// disables the timeout entirely; cancellation and round caps remain.
    pub max_runtime_secs: Option<u64>,
    /// When `true`, the structured subagent result returned to the parent
    /// carries a `transcript` field with the child's assistant + tool
    /// trace. Default `false` — the parent sees only the final fields
    /// (`summary`, `supporting_receipts`, `files_touched`), keeping the
    /// parent loop's context tight.
    pub include_transcript: bool,
}

impl SubagentConfig {
    fn from_settings_and_env(
        settings: SubagentSettings,
        get_var: &mut impl FnMut(&str) -> Option<String>,
    ) -> Self {
        Self {
            enabled: get_var("SQUEEZY_SUBAGENTS_ENABLED")
                .as_deref()
                .map(parse_enabled_bool)
                .unwrap_or(settings.enabled.unwrap_or(true)),
            explore_enabled: get_var("SQUEEZY_EXPLORE_SUBAGENT_ENABLED")
                .as_deref()
                .map(parse_enabled_bool)
                .unwrap_or(settings.explore_enabled.unwrap_or(true)),
            explore_model: get_var("SQUEEZY_EXPLORE_MODEL")
                .or(settings.explore_model)
                .filter(|value| !value.trim().is_empty()),
            max_concurrent: {
                let raw = parse_usize(
                    get_var("SQUEEZY_SUBAGENT_MAX_CONCURRENT"),
                    settings
                        .max_concurrent
                        .unwrap_or(DEFAULT_SUBAGENT_MAX_CONCURRENT),
                );
                raw.max(1)
            },
            max_tool_calls_per_call: parse_u64(
                get_var("SQUEEZY_SUBAGENT_MAX_TOOL_CALLS_PER_CALL"),
                settings
                    .max_tool_calls_per_call
                    .unwrap_or(DEFAULT_SUBAGENT_MAX_TOOL_CALLS_PER_CALL),
            ),
            max_tool_bytes_read_per_call: parse_u64(
                get_var("SQUEEZY_SUBAGENT_MAX_TOOL_BYTES_READ_PER_CALL"),
                settings
                    .max_tool_bytes_read_per_call
                    .unwrap_or(DEFAULT_SUBAGENT_MAX_TOOL_BYTES_READ_PER_CALL),
            ),
            max_search_files_per_call: parse_u64(
                get_var("SQUEEZY_SUBAGENT_MAX_SEARCH_FILES_PER_CALL"),
                settings
                    .max_search_files_per_call
                    .unwrap_or(DEFAULT_SUBAGENT_MAX_SEARCH_FILES_PER_CALL),
            ),
            max_model_rounds: parse_usize(
                get_var("SQUEEZY_SUBAGENT_MAX_MODEL_ROUNDS"),
                settings
                    .max_model_rounds
                    .unwrap_or(DEFAULT_SUBAGENT_MAX_MODEL_ROUNDS),
            ),
            max_summary_tokens: get_var("SQUEEZY_SUBAGENT_MAX_SUMMARY_TOKENS")
                .and_then(|value| value.parse::<u32>().ok())
                .filter(|value| *value > 0)
                .or(settings.max_summary_tokens)
                .unwrap_or(DEFAULT_SUBAGENT_MAX_SUMMARY_TOKENS),
            max_runtime_secs: {
                let raw = get_var("SQUEEZY_SUBAGENT_MAX_RUNTIME_SECS")
                    .and_then(|value| value.parse::<u64>().ok())
                    .or(settings.max_runtime_secs)
                    .unwrap_or(DEFAULT_SUBAGENT_MAX_RUNTIME_SECS);
                if raw == 0 { None } else { Some(raw) }
            },
            include_transcript: get_var("SQUEEZY_SUBAGENT_INCLUDE_TRANSCRIPT")
                .as_deref()
                .map(parse_enabled_bool)
                .unwrap_or(settings.include_transcript.unwrap_or(false)),
        }
    }
}

impl Default for SubagentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            explore_enabled: true,
            explore_model: None,
            max_concurrent: DEFAULT_SUBAGENT_MAX_CONCURRENT,
            max_tool_calls_per_call: DEFAULT_SUBAGENT_MAX_TOOL_CALLS_PER_CALL,
            max_tool_bytes_read_per_call: DEFAULT_SUBAGENT_MAX_TOOL_BYTES_READ_PER_CALL,
            max_search_files_per_call: DEFAULT_SUBAGENT_MAX_SEARCH_FILES_PER_CALL,
            max_model_rounds: DEFAULT_SUBAGENT_MAX_MODEL_ROUNDS,
            max_summary_tokens: DEFAULT_SUBAGENT_MAX_SUMMARY_TOKENS,
            max_runtime_secs: Some(DEFAULT_SUBAGENT_MAX_RUNTIME_SECS),
            include_transcript: false,
        }
    }
}

/// How the compaction summary is produced. Default is `Extractive`, which
/// preserves the historical deterministic / no-model-call behavior. The two
/// other variants opt into model-assisted summarization with strict
/// extractive fallback on error / timeout.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionStrategy {
    #[default]
    Extractive,
    ModelAssisted,
    LayeredFallback,
}

impl CompactionStrategy {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "extractive" => Some(Self::Extractive),
            "model_assisted" | "model-assisted" => Some(Self::ModelAssisted),
            "layered_fallback" | "layered-fallback" => Some(Self::LayeredFallback),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Extractive => "extractive",
            Self::ModelAssisted => "model_assisted",
            Self::LayeredFallback => "layered_fallback",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCompactionConfig {
    pub enabled: bool,
    pub estimated_tokens: u64,
    pub min_items: usize,
    pub recent_items: usize,
    pub max_summary_bytes: usize,
    /// Maximum bytes of concatenated AGENTS.md content stitched into the
    /// base instructions at session start. 0 disables ingestion.
    pub repo_doc_max_bytes: usize,
    /// Maximum bytes of `~/.squeezy/MEMORY.md` (or lowercase `memory.md`)
    /// stitched into the base instructions at session start. 0 disables
    /// ingestion. The static file is the only cross-session memory
    /// surface; see `docs/internal/MEMORY_SCOPE.md` for the deferred
    /// tool-mediated pipeline decision.
    pub user_memory_max_bytes: usize,
    /// When true, the turn loop re-checks token usage between LLM events and
    /// triggers compaction once usage crosses `threshold_percent` of
    /// `model_context_window`. Defaults to true; the trigger only fires
    /// when `model_context_window` is also set.
    pub enabled_mid_turn: bool,
    /// Configured token budget for the active model. When `None`, mid-turn
    /// compaction stays dormant and the post-turn auto trigger is the only
    /// path. Squeezy does not auto-detect this per-provider yet.
    pub model_context_window: Option<u64>,
    /// Fraction of `model_context_window` (0..=100) at which mid-turn
    /// compaction fires. Capped to 100 on read.
    pub threshold_percent: u8,
    /// Summary generation strategy. Default `Extractive` preserves current
    /// behavior; other variants opt-in to model-assisted summarization with
    /// extractive fallback.
    pub strategy: CompactionStrategy,
    /// Cheap model id used for model-assisted compaction. Required when
    /// `strategy != Extractive`; the path falls back to extractive if unset
    /// or if the provider rejects the model.
    pub model_assisted_model: Option<String>,
    pub model_assisted_max_output_tokens: u32,
    pub model_assisted_timeout_secs: u64,
    pub layered_fallback_extractive_threshold_tokens: u32,
    /// Master switch for the mid-tier "micro" compaction pass. When true
    /// and `enabled_mid_turn` is also true, the agent attempts to clear
    /// older `FunctionCallOutput` payloads in place before falling
    /// through to full compaction. Disabled callers go straight from
    /// no-op to full compaction.
    pub micro_compaction_enabled: bool,
    /// Token-window fraction (0..=100) at which micro-compaction fires.
    /// Should sit below `threshold_percent` so micro reclaims tool-output
    /// bytes before the full tier's all-or-nothing summary kicks in.
    pub micro_compaction_threshold_percent: u8,
    /// Keep this many newest compactable tool results verbatim; older
    /// results are rewritten to a structured placeholder.
    pub micro_compaction_keep_recent: usize,
}

impl ContextCompactionConfig {
    fn from_settings_and_env(
        settings: ContextCompactionSettings,
        get_var: &mut impl FnMut(&str) -> Option<String>,
    ) -> Self {
        Self {
            enabled: get_var("SQUEEZY_CONTEXT_COMPACTION_ENABLED")
                .as_deref()
                .map(parse_enabled_bool)
                .unwrap_or(settings.compaction_enabled.unwrap_or(true)),
            estimated_tokens: parse_u64(
                get_var("SQUEEZY_CONTEXT_COMPACTION_ESTIMATED_TOKENS"),
                settings
                    .compaction_estimated_tokens
                    .unwrap_or(DEFAULT_CONTEXT_COMPACTION_ESTIMATED_TOKENS),
            ),
            min_items: parse_usize(
                get_var("SQUEEZY_CONTEXT_COMPACTION_MIN_ITEMS"),
                settings
                    .compaction_min_items
                    .unwrap_or(DEFAULT_CONTEXT_COMPACTION_MIN_ITEMS),
            ),
            recent_items: parse_usize(
                get_var("SQUEEZY_CONTEXT_COMPACTION_RECENT_ITEMS"),
                settings
                    .compaction_recent_items
                    .unwrap_or(DEFAULT_CONTEXT_COMPACTION_RECENT_ITEMS),
            ),
            max_summary_bytes: parse_usize(
                get_var("SQUEEZY_CONTEXT_COMPACTION_MAX_SUMMARY_BYTES"),
                settings
                    .compaction_max_summary_bytes
                    .unwrap_or(DEFAULT_CONTEXT_COMPACTION_MAX_SUMMARY_BYTES),
            ),
            repo_doc_max_bytes: parse_usize(
                get_var("SQUEEZY_CONTEXT_REPO_DOC_MAX_BYTES"),
                settings
                    .repo_doc_max_bytes
                    .unwrap_or(DEFAULT_CONTEXT_REPO_DOC_MAX_BYTES),
            ),
            user_memory_max_bytes: parse_usize(
                get_var("SQUEEZY_CONTEXT_USER_MEMORY_MAX_BYTES"),
                settings
                    .user_memory_max_bytes
                    .unwrap_or(DEFAULT_CONTEXT_USER_MEMORY_MAX_BYTES),
            ),
            enabled_mid_turn: get_var("SQUEEZY_CONTEXT_COMPACTION_ENABLED_MID_TURN")
                .as_deref()
                .map(parse_enabled_bool)
                .unwrap_or(settings.enabled_mid_turn.unwrap_or(true)),
            model_context_window: get_var("SQUEEZY_CONTEXT_MODEL_CONTEXT_WINDOW")
                .as_deref()
                .and_then(|raw| raw.parse::<u64>().ok())
                .or(settings.model_context_window),
            threshold_percent: clamp_percent(
                get_var("SQUEEZY_CONTEXT_COMPACTION_THRESHOLD_PERCENT")
                    .as_deref()
                    .and_then(|raw| raw.parse::<u8>().ok())
                    .or(settings.threshold_percent)
                    .unwrap_or(DEFAULT_CONTEXT_COMPACTION_THRESHOLD_PERCENT),
            ),
            strategy: get_var("SQUEEZY_CONTEXT_COMPACTION_STRATEGY")
                .as_deref()
                .and_then(CompactionStrategy::parse)
                .or(settings.strategy)
                .unwrap_or_default(),
            model_assisted_model: get_var("SQUEEZY_CONTEXT_COMPACTION_MODEL_ASSISTED_MODEL")
                .or_else(|| settings.model_assisted_model.clone()),
            model_assisted_max_output_tokens: get_var(
                "SQUEEZY_CONTEXT_COMPACTION_MODEL_ASSISTED_MAX_OUTPUT_TOKENS",
            )
            .as_deref()
            .and_then(|raw| raw.parse::<u32>().ok())
            .or(settings.model_assisted_max_output_tokens)
            .unwrap_or(DEFAULT_CONTEXT_COMPACTION_MODEL_ASSISTED_MAX_OUTPUT_TOKENS),
            model_assisted_timeout_secs: get_var(
                "SQUEEZY_CONTEXT_COMPACTION_MODEL_ASSISTED_TIMEOUT_SECS",
            )
            .as_deref()
            .and_then(|raw| raw.parse::<u64>().ok())
            .or(settings.model_assisted_timeout_secs)
            .unwrap_or(DEFAULT_CONTEXT_COMPACTION_MODEL_ASSISTED_TIMEOUT_SECS),
            layered_fallback_extractive_threshold_tokens: get_var(
                "SQUEEZY_CONTEXT_COMPACTION_LAYERED_FALLBACK_THRESHOLD_TOKENS",
            )
            .as_deref()
            .and_then(|raw| raw.parse::<u32>().ok())
            .or(settings.layered_fallback_extractive_threshold_tokens)
            .unwrap_or(DEFAULT_CONTEXT_COMPACTION_LAYERED_FALLBACK_EXTRACTIVE_THRESHOLD_TOKENS),
            micro_compaction_enabled: get_var("SQUEEZY_CONTEXT_MICRO_COMPACTION_ENABLED")
                .as_deref()
                .map(parse_enabled_bool)
                .unwrap_or(settings.micro_compaction_enabled.unwrap_or(true)),
            micro_compaction_threshold_percent: clamp_percent(
                get_var("SQUEEZY_CONTEXT_MICRO_COMPACTION_THRESHOLD_PERCENT")
                    .as_deref()
                    .and_then(|raw| raw.parse::<u8>().ok())
                    .or(settings.micro_compaction_threshold_percent)
                    .unwrap_or(DEFAULT_CONTEXT_MICRO_COMPACTION_THRESHOLD_PERCENT),
            ),
            micro_compaction_keep_recent: parse_usize(
                get_var("SQUEEZY_CONTEXT_MICRO_COMPACTION_KEEP_RECENT"),
                settings
                    .micro_compaction_keep_recent
                    .unwrap_or(DEFAULT_CONTEXT_MICRO_COMPACTION_KEEP_RECENT),
            ),
        }
    }
}

fn clamp_percent(value: u8) -> u8 {
    value.min(100)
}

impl Default for ContextCompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            estimated_tokens: DEFAULT_CONTEXT_COMPACTION_ESTIMATED_TOKENS,
            min_items: DEFAULT_CONTEXT_COMPACTION_MIN_ITEMS,
            recent_items: DEFAULT_CONTEXT_COMPACTION_RECENT_ITEMS,
            max_summary_bytes: DEFAULT_CONTEXT_COMPACTION_MAX_SUMMARY_BYTES,
            repo_doc_max_bytes: DEFAULT_CONTEXT_REPO_DOC_MAX_BYTES,
            user_memory_max_bytes: DEFAULT_CONTEXT_USER_MEMORY_MAX_BYTES,
            enabled_mid_turn: true,
            model_context_window: None,
            threshold_percent: DEFAULT_CONTEXT_COMPACTION_THRESHOLD_PERCENT,
            strategy: CompactionStrategy::default(),
            model_assisted_model: None,
            model_assisted_max_output_tokens:
                DEFAULT_CONTEXT_COMPACTION_MODEL_ASSISTED_MAX_OUTPUT_TOKENS,
            model_assisted_timeout_secs: DEFAULT_CONTEXT_COMPACTION_MODEL_ASSISTED_TIMEOUT_SECS,
            layered_fallback_extractive_threshold_tokens:
                DEFAULT_CONTEXT_COMPACTION_LAYERED_FALLBACK_EXTRACTIVE_THRESHOLD_TOKENS,
            micro_compaction_enabled: true,
            micro_compaction_threshold_percent: DEFAULT_CONTEXT_MICRO_COMPACTION_THRESHOLD_PERCENT,
            micro_compaction_keep_recent: DEFAULT_CONTEXT_MICRO_COMPACTION_KEEP_RECENT,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PermissionCapability {
    Read,
    Search,
    Edit,
    Shell,
    Network,
    Mcp,
    Git,
    Compiler,
    Destructive,
}

impl PermissionCapability {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "read" => Some(Self::Read),
            "search" => Some(Self::Search),
            "edit" | "write" => Some(Self::Edit),
            "shell" | "bash" | "command" => Some(Self::Shell),
            "network" | "web" => Some(Self::Network),
            "mcp" => Some(Self::Mcp),
            "git" => Some(Self::Git),
            "compiler" | "verify" => Some(Self::Compiler),
            "destructive" | "dangerous" => Some(Self::Destructive),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Search => "search",
            Self::Edit => "edit",
            Self::Shell => "shell",
            Self::Network => "network",
            Self::Mcp => "mcp",
            Self::Git => "git",
            Self::Compiler => "compiler",
            Self::Destructive => "destructive",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionRisk {
    Low,
    Medium,
    High,
    Critical,
}

impl PermissionRisk {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionRuleSource {
    Builtin,
    User,
    Project,
    Session,
}

impl PermissionRuleSource {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "builtin" => Some(Self::Builtin),
            "user" => Some(Self::User),
            "project" => Some(Self::Project),
            "session" => Some(Self::Session),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::User => "user",
            Self::Project => "project",
            Self::Session => "session",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRule {
    pub capability: String,
    pub target: String,
    pub action: PermissionAction,
    pub source: PermissionRuleSource,
    pub reason: Option<String>,
    /// When `true` and `action == Deny`, the matching verdict suppresses the
    /// per-call narrative on the tool-result string sent back to the model
    /// (the audit JSONL still records the full reason). Only meaningful for
    /// `Deny` rules; loaders reject `silent = true` on `Allow`/`Ask` to keep
    /// the field's semantics narrow. The use case is boilerplate policy: an
    /// absolute deny rule like `rm -rf /` or writes to `.git/config` does not
    /// need to spell out `capability=...; target=...; risk=...` to the model
    /// on every retry — the model only needs to know the call is rejected and
    /// move on. Auditability stays in the JSONL log.
    #[serde(default, skip_serializing_if = "is_false")]
    pub silent: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl PermissionRule {
    pub fn new(
        capability: impl Into<String>,
        target: impl Into<String>,
        action: PermissionAction,
        source: PermissionRuleSource,
        reason: Option<String>,
    ) -> Self {
        Self {
            capability: capability.into(),
            target: target.into(),
            action,
            source,
            reason,
            silent: false,
        }
    }

    /// Mark this rule as silent. Only meaningful for `Deny` rules; the loader
    /// rejects `silent = true` on `Allow`/`Ask`, but this setter does not
    /// re-check because in-memory builders may compose pieces in either order.
    /// Callers responsible for upholding the invariant are the TOML loader and
    /// any future builders that synthesize rules from typed sources.
    pub fn with_silent(mut self, silent: bool) -> Self {
        self.silent = silent;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRequest {
    pub call_id: String,
    pub tool_name: String,
    pub capability: PermissionCapability,
    pub target: String,
    pub risk: PermissionRisk,
    pub summary: String,
    pub metadata: BTreeMap<String, String>,
    pub suggested_rules: Vec<PermissionRule>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionVerdict {
    pub action: PermissionAction,
    pub matched_rule: Option<PermissionRule>,
    pub reason: String,
    /// True when this verdict came from a `silent` deny rule. The agent uses
    /// this to send a minimal `"action denied by policy"` to the model in place
    /// of the structured `reason`, while the audit JSONL still receives the
    /// full reason via `log_permission_verdict`. Only set when `action` is
    /// `Deny`; other actions ignore the flag.
    pub silent: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PermissionSettings {
    pub mode: Option<PermissionPolicyMode>,
    pub read: Option<PermissionMode>,
    pub search: Option<PermissionMode>,
    pub edit: Option<PermissionMode>,
    pub shell: Option<PermissionMode>,
    pub ignored_search: Option<PermissionMode>,
    pub web: Option<PermissionMode>,
    pub mcp: Option<PermissionMode>,
    pub git: Option<PermissionMode>,
    pub compiler: Option<PermissionMode>,
    pub destructive: Option<PermissionMode>,
    pub shell_classifier: Option<bool>,
    pub custom: Option<PermissionCustomSettings>,
    pub ai_reviewer: Option<AiReviewerSettings>,
    pub shell_sandbox: Option<ShellSandboxSettings>,
    pub rules: Vec<PermissionRule>,
}

impl PermissionSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "mode",
                "read",
                "search",
                "edit",
                "shell",
                "ignored_search",
                "web",
                "mcp",
                "git",
                "compiler",
                "destructive",
                "shell_classifier",
                "custom",
                "ai_reviewer",
                "shell_sandbox",
                "rules",
            ],
            source,
            path,
        )?;
        let mode = string_value(table, "mode", source, &field(path, "mode"))?
            .map(|mode| {
                PermissionPolicyMode::parse(&mode).ok_or_else(|| {
                    SqueezyError::Config(format!(
                        "{source}: {path}.mode invalid value {mode:?}; expected default, auto_review, full_access, or custom"
                    ))
                })
            })
            .transpose()?;
        Ok(Self {
            mode,
            read: permission_value(table, "read", source, &field(path, "read"))?,
            search: permission_value(table, "search", source, &field(path, "search"))?,
            edit: permission_value(table, "edit", source, &field(path, "edit"))?,
            shell: permission_value(table, "shell", source, &field(path, "shell"))?,
            ignored_search: permission_value(
                table,
                "ignored_search",
                source,
                &field(path, "ignored_search"),
            )?,
            web: permission_value(table, "web", source, &field(path, "web"))?,
            mcp: permission_value(table, "mcp", source, &field(path, "mcp"))?,
            git: permission_value(table, "git", source, &field(path, "git"))?,
            compiler: permission_value(table, "compiler", source, &field(path, "compiler"))?,
            destructive: permission_value(
                table,
                "destructive",
                source,
                &field(path, "destructive"),
            )?,
            shell_classifier: bool_value(
                table,
                "shell_classifier",
                source,
                &field(path, "shell_classifier"),
            )?,
            custom: optional_table(table, "custom", source)?
                .map(|table| {
                    PermissionCustomSettings::from_table(table, source, &field(path, "custom"))
                })
                .transpose()?,
            ai_reviewer: optional_table(table, "ai_reviewer", source)?
                .map(|table| {
                    AiReviewerSettings::from_table(table, source, &field(path, "ai_reviewer"))
                })
                .transpose()?,
            shell_sandbox: optional_table(table, "shell_sandbox", source)?
                .map(|table| {
                    ShellSandboxSettings::from_table(table, source, &field(path, "shell_sandbox"))
                })
                .transpose()?,
            rules: permission_rules_value(table, source, &field(path, "rules"))?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.mode, next.mode);
        replace_if_some(&mut self.read, next.read);
        replace_if_some(&mut self.search, next.search);
        replace_if_some(&mut self.edit, next.edit);
        replace_if_some(&mut self.shell, next.shell);
        replace_if_some(&mut self.ignored_search, next.ignored_search);
        replace_if_some(&mut self.web, next.web);
        replace_if_some(&mut self.mcp, next.mcp);
        replace_if_some(&mut self.git, next.git);
        replace_if_some(&mut self.compiler, next.compiler);
        replace_if_some(&mut self.destructive, next.destructive);
        replace_if_some(&mut self.shell_classifier, next.shell_classifier);
        merge_option(
            &mut self.custom,
            next.custom,
            PermissionCustomSettings::merge,
        );
        merge_option(
            &mut self.ai_reviewer,
            next.ai_reviewer,
            AiReviewerSettings::merge,
        );
        merge_option(
            &mut self.shell_sandbox,
            next.shell_sandbox,
            ShellSandboxSettings::merge,
        );
        self.rules.extend(next.rules);
    }

    fn has_legacy_defaults(&self) -> bool {
        self.read.is_some()
            || self.search.is_some()
            || self.edit.is_some()
            || self.shell.is_some()
            || self.ignored_search.is_some()
            || self.web.is_some()
            || self.mcp.is_some()
            || self.git.is_some()
            || self.compiler.is_some()
            || self.destructive.is_some()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PermissionCustomSettings {
    pub read: Option<PermissionMode>,
    pub search: Option<PermissionMode>,
    pub edit: Option<PermissionMode>,
    pub shell: Option<PermissionMode>,
    pub ignored_search: Option<PermissionMode>,
    pub network: Option<PermissionMode>,
    pub mcp: Option<PermissionMode>,
    pub git: Option<PermissionMode>,
    pub compiler: Option<PermissionMode>,
    pub destructive: Option<PermissionMode>,
}

impl PermissionCustomSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "read",
                "search",
                "edit",
                "shell",
                "ignored_search",
                "network",
                "mcp",
                "git",
                "compiler",
                "destructive",
            ],
            source,
            path,
        )?;
        Ok(Self {
            read: permission_value(table, "read", source, &field(path, "read"))?,
            search: permission_value(table, "search", source, &field(path, "search"))?,
            edit: permission_value(table, "edit", source, &field(path, "edit"))?,
            shell: permission_value(table, "shell", source, &field(path, "shell"))?,
            ignored_search: permission_value(
                table,
                "ignored_search",
                source,
                &field(path, "ignored_search"),
            )?,
            network: permission_value(table, "network", source, &field(path, "network"))?,
            mcp: permission_value(table, "mcp", source, &field(path, "mcp"))?,
            git: permission_value(table, "git", source, &field(path, "git"))?,
            compiler: permission_value(table, "compiler", source, &field(path, "compiler"))?,
            destructive: permission_value(
                table,
                "destructive",
                source,
                &field(path, "destructive"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.read, next.read);
        replace_if_some(&mut self.search, next.search);
        replace_if_some(&mut self.edit, next.edit);
        replace_if_some(&mut self.shell, next.shell);
        replace_if_some(&mut self.ignored_search, next.ignored_search);
        replace_if_some(&mut self.network, next.network);
        replace_if_some(&mut self.mcp, next.mcp);
        replace_if_some(&mut self.git, next.git);
        replace_if_some(&mut self.compiler, next.compiler);
        replace_if_some(&mut self.destructive, next.destructive);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AiReviewerSettings {
    pub enabled: Option<bool>,
    pub model: Option<String>,
    pub allow_capabilities: Option<Vec<String>>,
    pub policy_file: Option<String>,
    pub timeout_secs: Option<u64>,
    pub max_transcript_tokens: Option<u64>,
}

impl AiReviewerSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "enabled",
                "model",
                "allow_capabilities",
                "policy_file",
                "timeout_secs",
                "max_transcript_tokens",
            ],
            source,
            path,
        )?;
        Ok(Self {
            enabled: bool_value(table, "enabled", source, &field(path, "enabled"))?,
            model: string_value(table, "model", source, &field(path, "model"))?,
            allow_capabilities: string_array_value(
                table,
                "allow_capabilities",
                source,
                &field(path, "allow_capabilities"),
            )?,
            policy_file: string_value(table, "policy_file", source, &field(path, "policy_file"))?,
            timeout_secs: u64_value(table, "timeout_secs", source, &field(path, "timeout_secs"))?,
            max_transcript_tokens: u64_value(
                table,
                "max_transcript_tokens",
                source,
                &field(path, "max_transcript_tokens"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.enabled, next.enabled);
        replace_if_some(&mut self.model, next.model);
        replace_if_some(&mut self.allow_capabilities, next.allow_capabilities);
        replace_if_some(&mut self.policy_file, next.policy_file);
        replace_if_some(&mut self.timeout_secs, next.timeout_secs);
        replace_if_some(&mut self.max_transcript_tokens, next.max_transcript_tokens);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiReviewerConfig {
    pub enabled: bool,
    pub model: Option<String>,
    pub allow_capabilities: Vec<PermissionCapability>,
    pub policy_file: Option<PathBuf>,
    pub timeout_secs: u64,
    /// Sliding-window transcript budget for the reviewer prompt. Keeps the most
    /// recent turns whole and compacts older entries into a single summary
    /// line so late-turn permission requests retain earlier intent context.
    pub max_transcript_tokens: usize,
}

pub const DEFAULT_AI_REVIEWER_MAX_TRANSCRIPT_TOKENS: usize = 4_000;
const MIN_AI_REVIEWER_MAX_TRANSCRIPT_TOKENS: u64 = 512;
const MAX_AI_REVIEWER_MAX_TRANSCRIPT_TOKENS: u64 = 32_000;

impl Default for AiReviewerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: None,
            allow_capabilities: vec![PermissionCapability::Read, PermissionCapability::Search],
            policy_file: None,
            timeout_secs: 15,
            max_transcript_tokens: DEFAULT_AI_REVIEWER_MAX_TRANSCRIPT_TOKENS,
        }
    }
}

impl AiReviewerConfig {
    fn from_settings(settings: Option<AiReviewerSettings>, source: &str) -> Result<Self> {
        let mut config = Self::default();
        let Some(settings) = settings else {
            return Ok(config);
        };
        if let Some(enabled) = settings.enabled {
            config.enabled = enabled;
        }
        if let Some(model) = settings.model {
            let model = model.trim();
            if !model.is_empty() {
                config.model = Some(model.to_string());
            }
        }
        if let Some(policy_file) = settings.policy_file {
            let policy_file = policy_file.trim();
            if !policy_file.is_empty() {
                config.policy_file = Some(expand_home_path(PathBuf::from(policy_file)));
            }
        }
        if let Some(timeout_secs) = settings.timeout_secs {
            if !(1..=120).contains(&timeout_secs) {
                return Err(SqueezyError::Config(format!(
                    "{source}: permissions.ai_reviewer.timeout_secs {timeout_secs} outside supported range 1..=120"
                )));
            }
            config.timeout_secs = timeout_secs;
        }
        if let Some(max_transcript_tokens) = settings.max_transcript_tokens {
            if !(MIN_AI_REVIEWER_MAX_TRANSCRIPT_TOKENS..=MAX_AI_REVIEWER_MAX_TRANSCRIPT_TOKENS)
                .contains(&max_transcript_tokens)
            {
                return Err(SqueezyError::Config(format!(
                    "{source}: permissions.ai_reviewer.max_transcript_tokens {max_transcript_tokens} outside supported range {MIN_AI_REVIEWER_MAX_TRANSCRIPT_TOKENS}..={MAX_AI_REVIEWER_MAX_TRANSCRIPT_TOKENS}"
                )));
            }
            config.max_transcript_tokens = max_transcript_tokens as usize;
        }
        if let Some(allow_capabilities) = settings.allow_capabilities {
            let mut parsed = Vec::new();
            for capability in allow_capabilities {
                let Some(capability) = PermissionCapability::parse(&capability) else {
                    return Err(SqueezyError::Config(format!(
                        "{source}: permissions.ai_reviewer.allow_capabilities contains invalid capability {capability:?}"
                    )));
                };
                if !parsed.contains(&capability) {
                    parsed.push(capability);
                }
            }
            config.allow_capabilities = parsed;
        }
        Ok(config)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ShellSandboxSettings {
    pub mode: Option<String>,
    pub network: Option<String>,
    pub audit: Option<bool>,
    pub kill_grace_ms: Option<u64>,
    pub env_allowlist: Option<Vec<String>>,
    pub read_roots: Option<Vec<String>>,
    pub write_roots: Option<Vec<String>>,
    pub protected_metadata_names: Option<Vec<String>>,
    pub sensitive_path_patterns: Option<Vec<String>>,
    /// When `true`, the user-provided `sensitive_path_patterns` REPLACE the
    /// built-in floor. The default behavior (`false` / unset) extends the
    /// floor so a config that lists a single project pattern still keeps
    /// the `.ssh/**`, `.aws/**`, `.netrc`, etc. denials.
    pub replace_sensitive_path_patterns: Option<bool>,
}

impl ShellSandboxSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "mode",
                "network",
                "audit",
                "kill_grace_ms",
                "env_allowlist",
                "read_roots",
                "write_roots",
                "protected_metadata_names",
                "sensitive_path_patterns",
                "replace_sensitive_path_patterns",
            ],
            source,
            path,
        )?;
        Ok(Self {
            mode: string_value(table, "mode", source, &field(path, "mode"))?,
            network: string_value(table, "network", source, &field(path, "network"))?,
            audit: bool_value(table, "audit", source, &field(path, "audit"))?,
            kill_grace_ms: u64_value(
                table,
                "kill_grace_ms",
                source,
                &field(path, "kill_grace_ms"),
            )?,
            env_allowlist: string_array_value(
                table,
                "env_allowlist",
                source,
                &field(path, "env_allowlist"),
            )?,
            read_roots: string_array_value(
                table,
                "read_roots",
                source,
                &field(path, "read_roots"),
            )?,
            write_roots: string_array_value(
                table,
                "write_roots",
                source,
                &field(path, "write_roots"),
            )?,
            protected_metadata_names: string_array_value(
                table,
                "protected_metadata_names",
                source,
                &field(path, "protected_metadata_names"),
            )?,
            sensitive_path_patterns: string_array_value(
                table,
                "sensitive_path_patterns",
                source,
                &field(path, "sensitive_path_patterns"),
            )?,
            replace_sensitive_path_patterns: bool_value(
                table,
                "replace_sensitive_path_patterns",
                source,
                &field(path, "replace_sensitive_path_patterns"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.mode, next.mode);
        replace_if_some(&mut self.network, next.network);
        replace_if_some(&mut self.audit, next.audit);
        replace_if_some(&mut self.kill_grace_ms, next.kill_grace_ms);
        replace_if_some(&mut self.env_allowlist, next.env_allowlist);
        merge_string_lists(&mut self.read_roots, next.read_roots);
        merge_string_lists(&mut self.write_roots, next.write_roots);
        replace_if_some(
            &mut self.protected_metadata_names,
            next.protected_metadata_names,
        );
        replace_if_some(
            &mut self.sensitive_path_patterns,
            next.sensitive_path_patterns,
        );
        replace_if_some(
            &mut self.replace_sensitive_path_patterns,
            next.replace_sensitive_path_patterns,
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShellSandboxMode {
    Required,
    BestEffort,
    Off,
    External,
}

impl ShellSandboxMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "required" => Some(Self::Required),
            "best_effort" | "best-effort" => Some(Self::BestEffort),
            "off" | "disabled" => Some(Self::Off),
            "external" | "external_sandbox" | "external-sandbox" => Some(Self::External),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Required => "required",
            Self::BestEffort => "best_effort",
            Self::Off => "off",
            Self::External => "external",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShellSandboxNetworkPolicy {
    DenyByDefault,
    AllowWhenApproved,
}

impl ShellSandboxNetworkPolicy {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "deny_by_default" | "deny-by-default" => Some(Self::DenyByDefault),
            "allow_when_approved" | "allow-when-approved" => Some(Self::AllowWhenApproved),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DenyByDefault => "deny_by_default",
            Self::AllowWhenApproved => "allow_when_approved",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellSandboxConfig {
    pub mode: ShellSandboxMode,
    pub network: ShellSandboxNetworkPolicy,
    pub audit: bool,
    pub kill_grace_ms: u64,
    pub env_allowlist: Vec<String>,
    pub read_roots: Vec<PathBuf>,
    pub write_roots: Vec<PathBuf>,
    pub protected_metadata_names: Vec<String>,
    pub sensitive_path_patterns: Vec<String>,
}

impl Default for ShellSandboxConfig {
    fn default() -> Self {
        Self {
            mode: ShellSandboxMode::BestEffort,
            network: ShellSandboxNetworkPolicy::DenyByDefault,
            audit: true,
            kill_grace_ms: 250,
            env_allowlist: default_shell_env_allowlist(),
            read_roots: Vec::new(),
            write_roots: Vec::new(),
            protected_metadata_names: default_protected_metadata_names(),
            sensitive_path_patterns: default_sensitive_path_patterns(),
        }
    }
}

const SHELL_SANDBOX_KILL_GRACE_MIN_MS: u64 = 10;
const SHELL_SANDBOX_KILL_GRACE_MAX_MS: u64 = 60_000;

impl ShellSandboxConfig {
    fn from_settings(
        settings: Option<ShellSandboxSettings>,
        source: &str,
        workspace_root: &Path,
    ) -> Result<Self> {
        let mut config = Self::default();
        let Some(settings) = settings else {
            return Ok(config);
        };
        if let Some(mode) = settings.mode {
            config.mode = ShellSandboxMode::parse(&mode).ok_or_else(|| {
                SqueezyError::Config(format!(
                    "{source}: permissions.shell_sandbox.mode invalid value {mode:?}; expected required, best_effort, off, or external"
                ))
            })?;
        }
        if let Some(network) = settings.network {
            config.network = ShellSandboxNetworkPolicy::parse(&network).ok_or_else(|| {
                SqueezyError::Config(format!(
                    "{source}: permissions.shell_sandbox.network invalid value {network:?}; expected deny_by_default or allow_when_approved"
                ))
            })?;
        }
        if let Some(audit) = settings.audit {
            config.audit = audit;
        }
        if let Some(kill_grace_ms) = settings.kill_grace_ms {
            if !(SHELL_SANDBOX_KILL_GRACE_MIN_MS..=SHELL_SANDBOX_KILL_GRACE_MAX_MS)
                .contains(&kill_grace_ms)
            {
                return Err(SqueezyError::Config(format!(
                    "{source}: permissions.shell_sandbox.kill_grace_ms {kill_grace_ms} \
                     outside supported range {SHELL_SANDBOX_KILL_GRACE_MIN_MS}..={SHELL_SANDBOX_KILL_GRACE_MAX_MS}"
                )));
            }
            config.kill_grace_ms = kill_grace_ms;
        }
        if let Some(env_allowlist) = settings.env_allowlist {
            for pattern in &env_allowlist {
                validate_env_allowlist_pattern(pattern, source)?;
            }
            if env_allowlist.is_empty() {
                tracing::warn!(
                    target: "squeezy::permissions",
                    source = %source,
                    "permissions.shell_sandbox.env_allowlist was set to an empty list; \
                     shell commands will run with an empty environment"
                );
            }
            config.env_allowlist = env_allowlist;
        }
        // sensitive_path_patterns uses UNION semantics: user-provided patterns
        // EXTEND the built-in floor (.ssh/**, .aws/**, .netrc, …) rather than
        // replacing it. The built-in floor cannot be silently disabled by
        // listing a single project-specific pattern. To explicitly disable
        // the floor, set `replace_sensitive_path_patterns = true`.
        if let Some(sensitive_path_patterns) = settings.sensitive_path_patterns {
            for pattern in &sensitive_path_patterns {
                validate_sensitive_path_pattern(pattern, source)?;
            }
            if settings.replace_sensitive_path_patterns.unwrap_or(false) {
                if sensitive_path_patterns.is_empty() {
                    tracing::warn!(
                        target: "squeezy::permissions",
                        source = %source,
                        "permissions.shell_sandbox.sensitive_path_patterns was replaced with an empty list; \
                         pre-spawn shell sensitive-path checks are now disabled"
                    );
                }
                config.sensitive_path_patterns = sensitive_path_patterns;
            } else {
                let mut merged = config.sensitive_path_patterns.clone();
                for pattern in sensitive_path_patterns {
                    if !merged.contains(&pattern) {
                        merged.push(pattern);
                    }
                }
                config.sensitive_path_patterns = merged;
            }
        }
        if let Some(read_roots) = settings.read_roots {
            config.read_roots = validate_shell_sandbox_roots(
                read_roots,
                "read_roots",
                source,
                workspace_root,
                &config.sensitive_path_patterns,
            )?;
        }
        if let Some(write_roots) = settings.write_roots {
            config.write_roots = validate_shell_sandbox_roots(
                write_roots,
                "write_roots",
                source,
                workspace_root,
                &config.sensitive_path_patterns,
            )?;
        }
        if let Some(protected_metadata_names) = settings.protected_metadata_names {
            config.protected_metadata_names =
                validate_protected_metadata_names(protected_metadata_names, source)?;
        }
        reject_duplicate_shell_roots(source, &config.read_roots, &config.write_roots)?;
        Ok(config)
    }
}

/// Valid env_allowlist patterns: exact names like `PATH`, or trailing-`*`
/// patterns like `LC_*`. We don't support `*FOO`, `FOO_*_BAR`, or any glob
/// containing characters the runtime matcher doesn't understand.
fn validate_env_allowlist_pattern(pattern: &str, source: &str) -> Result<()> {
    let trimmed = pattern.trim();
    if trimmed.is_empty() {
        return Err(SqueezyError::Config(format!(
            "{source}: permissions.shell_sandbox.env_allowlist contains empty pattern"
        )));
    }
    let star_count = trimmed.matches('*').count();
    if star_count > 1 || (star_count == 1 && !trimmed.ends_with('*')) {
        return Err(SqueezyError::Config(format!(
            "{source}: permissions.shell_sandbox.env_allowlist pattern {pattern:?} \
             only supports an exact name or a single trailing `*` (e.g. `LC_*`)"
        )));
    }
    Ok(())
}

/// Valid sensitive_path_patterns: a leading path segment optionally followed
/// by trailing wildcards (`/**`, `/*`, or `*`). We disallow patterns whose
/// runtime base (everything up to the first wildcard) would be empty after
/// `sensitive_pattern_base`, since they degrade to "match every command".
fn validate_sensitive_path_pattern(pattern: &str, source: &str) -> Result<()> {
    let trimmed = pattern.trim();
    if trimmed.is_empty() {
        return Err(SqueezyError::Config(format!(
            "{source}: permissions.shell_sandbox.sensitive_path_patterns contains empty pattern"
        )));
    }
    if trimmed == "*" || trimmed == "**" {
        return Err(SqueezyError::Config(format!(
            "{source}: permissions.shell_sandbox.sensitive_path_patterns pattern {pattern:?} \
             matches every command and is refused"
        )));
    }
    // Strip any leading `/` so we look at the same base the runtime does.
    let body = trimmed.trim_start_matches('/');
    let base_end = body.find(['*', '?']).unwrap_or(body.len());
    if base_end == 0 {
        return Err(SqueezyError::Config(format!(
            "{source}: permissions.shell_sandbox.sensitive_path_patterns pattern {pattern:?} \
             must include a literal path prefix before any wildcard"
        )));
    }
    Ok(())
}

fn validate_shell_sandbox_roots(
    roots: Vec<String>,
    key: &str,
    source: &str,
    workspace_root: &Path,
    sensitive_patterns: &[String],
) -> Result<Vec<PathBuf>> {
    let mut validated = Vec::new();
    for raw in roots {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(SqueezyError::Config(format!(
                "{source}: permissions.shell_sandbox.{key} contains empty path"
            )));
        }
        let path = expand_home_path(PathBuf::from(trimmed));
        if !path.is_absolute() {
            return Err(SqueezyError::Config(format!(
                "{source}: permissions.shell_sandbox.{key} path {trimmed:?} must be absolute"
            )));
        }
        let canonical = fs::canonicalize(&path).map_err(|err| {
            SqueezyError::Config(format!(
                "{source}: permissions.shell_sandbox.{key} path {} is not accessible: {err}",
                path.display()
            ))
        })?;
        if !canonical.is_dir() {
            return Err(SqueezyError::Config(format!(
                "{source}: permissions.shell_sandbox.{key} path {} is not a directory",
                canonical.display()
            )));
        }
        if let Some(sensitive) =
            shell_root_sensitive_overlap(&canonical, workspace_root, sensitive_patterns)
        {
            return Err(SqueezyError::Config(format!(
                "{source}: permissions.shell_sandbox.{key} path {} is inside sensitive path {}",
                canonical.display(),
                sensitive.display()
            )));
        }
        if validated.contains(&canonical) {
            return Err(SqueezyError::Config(format!(
                "{source}: permissions.shell_sandbox.{key} path {} duplicates another configured root",
                canonical.display()
            )));
        }
        validated.push(canonical);
    }
    validated.sort();
    Ok(validated)
}

fn reject_duplicate_shell_roots(
    source: &str,
    read_roots: &[PathBuf],
    write_roots: &[PathBuf],
) -> Result<()> {
    for read_root in read_roots {
        if write_roots.contains(read_root) {
            return Err(SqueezyError::Config(format!(
                "{source}: permissions.shell_sandbox root {} appears in both read_roots and write_roots; write_roots already imply read access",
                read_root.display()
            )));
        }
    }
    Ok(())
}

fn validate_protected_metadata_names(names: Vec<String>, source: &str) -> Result<Vec<String>> {
    let mut validated = Vec::new();
    for raw in names {
        let name = raw.trim();
        if name.is_empty() {
            return Err(SqueezyError::Config(format!(
                "{source}: permissions.shell_sandbox.protected_metadata_names contains empty name"
            )));
        }
        if name.contains('/') || name.contains('\\') || name == "." || name == ".." {
            return Err(SqueezyError::Config(format!(
                "{source}: permissions.shell_sandbox.protected_metadata_names name {raw:?} must be a single path segment"
            )));
        }
        let name = name.to_string();
        if !validated.contains(&name) {
            validated.push(name);
        }
    }
    if validated.is_empty() {
        tracing::warn!(
            target: "squeezy::permissions",
            source = %source,
            "permissions.shell_sandbox.protected_metadata_names is empty; metadata directory write protection is disabled"
        );
    }
    Ok(validated)
}

fn shell_root_sensitive_overlap(
    root: &Path,
    workspace_root: &Path,
    sensitive_patterns: &[String],
) -> Option<PathBuf> {
    let workspace_root =
        fs::canonicalize(workspace_root).unwrap_or_else(|_| workspace_root.to_path_buf());
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|home| fs::canonicalize(&home).ok().or(Some(home)));
    for pattern in sensitive_patterns {
        let base = sensitive_pattern_base(pattern);
        if base.is_empty() {
            continue;
        }
        let workspace_sensitive = workspace_root.join(&base);
        if root.starts_with(&workspace_sensitive) {
            return Some(workspace_sensitive);
        }
        if let Some(home) = &home {
            let home_sensitive = home.join(&base);
            if root.starts_with(&home_sensitive) {
                return Some(home_sensitive);
            }
        }
    }
    None
}

/// Returns the literal directory prefix of a sensitive-path glob pattern,
/// stripping any trailing wildcards (`*`, `/**`) and the leading `/`. Empty
/// output indicates that the pattern is purely a wildcard and should be
/// treated as having no enforceable prefix.
pub fn sensitive_pattern_base(pattern: &str) -> String {
    let trimmed = pattern
        .trim()
        .trim_end_matches('*')
        .trim_end_matches('/')
        .trim_end_matches("/**");
    trimmed.trim_start_matches('/').to_string()
}

fn default_shell_env_allowlist() -> Vec<String> {
    [
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "SHELL",
        "TERM",
        "LANG",
        "TMPDIR",
        "TEMP",
        "TMP",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "RUSTFLAGS",
        "RUST_BACKTRACE",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "NIX_SSL_CERT_FILE",
        "LC_*",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn default_sensitive_path_patterns() -> Vec<String> {
    [
        ".ssh/**",
        ".aws/**",
        ".config/gh/**",
        ".netrc",
        ".gnupg/**",
        ".kube/**",
        ".docker/config.json",
        ".cargo/credentials*",
        ".npmrc",
        ".pypirc",
        ".env*",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn default_protected_metadata_names() -> Vec<String> {
    [".git", ".squeezy", ".agents"]
        .into_iter()
        .map(str::to_string)
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PermissionScope {
    Read,
    Edit,
    Shell,
    IgnoredSearch,
    Web,
    /// External MCP tools. Treated as its own scope so the shell sandbox
    /// gating (network policy, plan-mode shell denial) does not accidentally
    /// extend to MCP calls without explicit opt-in.
    Mcp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionPolicy {
    pub mode: PermissionPolicyMode,
    pub read: PermissionMode,
    pub search: PermissionMode,
    pub edit: PermissionMode,
    pub shell: PermissionMode,
    pub ignored_search: PermissionMode,
    pub web: PermissionMode,
    pub mcp: PermissionMode,
    pub git: PermissionMode,
    pub compiler: PermissionMode,
    pub destructive: PermissionMode,
    pub shell_classifier: bool,
    pub ai_reviewer: AiReviewerConfig,
    pub shell_sandbox: ShellSandboxConfig,
    pub rules: Vec<PermissionRule>,
}

impl PermissionPolicy {
    pub fn from_env_vars(mut var: impl FnMut(&str) -> Option<String>) -> Self {
        Self::from_settings_and_env(
            PermissionSettings::default(),
            "defaults",
            &env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            &mut var,
        )
        .expect("built-in permission defaults are valid")
    }

    fn from_settings_and_env(
        settings: PermissionSettings,
        source: &str,
        workspace_root: &Path,
        mut var: impl FnMut(&str) -> Option<String>,
    ) -> Result<Self> {
        let legacy_defaults = settings.has_legacy_defaults();
        let custom_defaults = settings.custom.is_some();
        let legacy_compat = legacy_defaults && settings.mode.is_none();
        let mode = settings
            .mode
            .unwrap_or(if legacy_defaults || custom_defaults {
                PermissionPolicyMode::Custom
            } else {
                PermissionPolicyMode::Default
            });
        let ai_reviewer_settings = settings.ai_reviewer.clone();
        let shell_sandbox_settings = settings.shell_sandbox.clone();
        let shell_sandbox_network_configured = shell_sandbox_settings
            .as_ref()
            .is_some_and(|settings| settings.network.is_some());

        let mut policy = if legacy_compat {
            Self::legacy_compat_defaults()
        } else {
            Self::preset(mode)
        };
        policy.mode = mode;

        if mode == PermissionPolicyMode::Custom {
            policy.apply_legacy_defaults(&settings, legacy_compat);
            if let Some(custom) = &settings.custom {
                policy.apply_custom_defaults(custom);
            }
        }

        policy.read = parse_permission(var("SQUEEZY_READ_PERMISSION"), policy.read);
        policy.search = parse_permission(var("SQUEEZY_SEARCH_PERMISSION"), policy.search);
        policy.edit = parse_permission(var("SQUEEZY_EDIT_PERMISSION"), policy.edit);
        policy.shell = parse_permission(var("SQUEEZY_SHELL_PERMISSION"), policy.shell);
        policy.ignored_search = parse_permission(
            var("SQUEEZY_IGNORED_SEARCH_PERMISSION"),
            policy.ignored_search,
        );
        policy.web = parse_permission(var("SQUEEZY_WEB_PERMISSION"), policy.web);
        policy.mcp = parse_permission(var("SQUEEZY_MCP_PERMISSION"), policy.mcp);
        policy.git = parse_permission(var("SQUEEZY_GIT_PERMISSION"), policy.git);
        policy.compiler = parse_permission(var("SQUEEZY_COMPILER_PERMISSION"), policy.compiler);
        policy.destructive =
            parse_permission(var("SQUEEZY_DESTRUCTIVE_PERMISSION"), policy.destructive);
        policy.shell_classifier = parse_bool(
            var("SQUEEZY_SHELL_PERMISSION_CLASSIFIER"),
            settings.shell_classifier.unwrap_or(false),
        );

        policy.ai_reviewer = AiReviewerConfig::from_settings(ai_reviewer_settings, source)?;
        match mode {
            PermissionPolicyMode::AutoReview => {
                policy.ai_reviewer.enabled = true;
                policy.ai_reviewer.allow_capabilities = auto_review_allow_capabilities();
            }
            PermissionPolicyMode::FullAccess => {
                policy.ai_reviewer.enabled = false;
            }
            PermissionPolicyMode::Default | PermissionPolicyMode::Custom => {}
        }

        policy.shell_sandbox =
            ShellSandboxConfig::from_settings(shell_sandbox_settings, source, workspace_root)?;
        match mode {
            PermissionPolicyMode::Default | PermissionPolicyMode::AutoReview => {
                if !shell_sandbox_network_configured {
                    policy.shell_sandbox.network = ShellSandboxNetworkPolicy::AllowWhenApproved;
                }
            }
            PermissionPolicyMode::FullAccess => {
                policy.shell_sandbox.mode = ShellSandboxMode::Off;
                policy.shell_sandbox.network = ShellSandboxNetworkPolicy::AllowWhenApproved;
            }
            PermissionPolicyMode::Custom => {}
        }

        policy.rules = settings.rules;
        Ok(policy)
    }

    pub const fn mode_for(&self, scope: PermissionScope) -> PermissionMode {
        match scope {
            PermissionScope::Read => self.read,
            PermissionScope::Edit => self.edit,
            PermissionScope::Shell => self.shell,
            PermissionScope::IgnoredSearch => self.ignored_search,
            PermissionScope::Web => self.web,
            PermissionScope::Mcp => self.mcp,
        }
    }

    pub const fn mode_for_capability(&self, capability: PermissionCapability) -> PermissionMode {
        match capability {
            PermissionCapability::Read => self.read,
            PermissionCapability::Search => self.search,
            PermissionCapability::Edit => self.edit,
            PermissionCapability::Shell => self.shell,
            PermissionCapability::Network => self.web,
            PermissionCapability::Mcp => self.mcp,
            PermissionCapability::Git => self.git,
            PermissionCapability::Compiler => self.compiler,
            PermissionCapability::Destructive => self.destructive,
        }
    }

    pub fn apply_mode(&mut self, mode: PermissionPolicyMode) {
        let rules = std::mem::take(&mut self.rules);
        let shell_classifier = self.shell_classifier;
        let mut ai_reviewer = self.ai_reviewer.clone();
        let mut shell_sandbox = self.shell_sandbox.clone();
        let mut next = Self::preset(mode);
        next.rules = rules;
        next.shell_classifier = shell_classifier;
        ai_reviewer.enabled = next.ai_reviewer.enabled;
        ai_reviewer.allow_capabilities = next.ai_reviewer.allow_capabilities.clone();
        next.ai_reviewer = ai_reviewer;
        shell_sandbox.mode = next.shell_sandbox.mode;
        shell_sandbox.network = next.shell_sandbox.network;
        next.shell_sandbox = shell_sandbox;
        *self = next;
    }

    pub fn evaluate(&self, request: &PermissionRequest) -> PermissionVerdict {
        self.evaluate_with_extra(request, &[])
    }

    /// Like [`Self::evaluate`] but lets the caller layer additional rules on
    /// top of the configured ones. `extra` is treated as appended after
    /// `self.rules`, so the most recently added session rule wins over any
    /// rule from the on-disk config.
    pub fn evaluate_with_extra(
        &self,
        request: &PermissionRequest,
        extra: &[PermissionRule],
    ) -> PermissionVerdict {
        let matched_rule = self
            .rules
            .iter()
            .chain(extra.iter())
            .rev()
            .find(|rule| {
                wildcard_match(request.capability.as_str(), &rule.capability)
                    && wildcard_match(&request.target, &rule.target)
            })
            .cloned();
        if let Some(rule) = matched_rule {
            let (action, override_reason) =
                downgrade_unsafe_action(rule.action, request.capability, &rule.target);
            let reason = override_reason.unwrap_or_else(|| {
                rule.reason
                    .clone()
                    .unwrap_or_else(|| format!("matched {} permission rule", rule.source.as_str()))
            });
            // Silent is honored only when the resolved action is Deny: a
            // downgraded Allow that survives with silent=true would be outside
            // the documented policy shape (the loader already refuses silent
            // on non-Deny rules), and a Deny that was downgraded would also
            // drop silent.
            let silent = rule.silent && action == PermissionAction::Deny;
            return PermissionVerdict {
                action,
                reason,
                matched_rule: Some(rule),
                silent,
            };
        }
        let action = if path_request_targets_outside_workspace(request)
            && self.mode != PermissionPolicyMode::FullAccess
        {
            PermissionAction::Ask
        } else {
            self.mode_for_capability(request.capability)
        };
        PermissionVerdict {
            action,
            matched_rule: None,
            reason: format!(
                "default {} permission is {}",
                request.capability.as_str(),
                action.as_str()
            ),
            silent: false,
        }
    }
}

fn path_request_targets_outside_workspace(request: &PermissionRequest) -> bool {
    matches!(
        request.capability,
        PermissionCapability::Read | PermissionCapability::Edit
    ) && request
        .metadata
        .get("outside_workspace")
        .is_some_and(|value| value == "true")
}

/// Belt-and-suspenders safety: refuse to honor an Allow rule that targets the
/// `destructive` capability or whose `target` is functionally a "match
/// everything" wildcard. Returns the (possibly downgraded) action and an
/// explanatory reason when a downgrade happens.
fn downgrade_unsafe_action(
    action: PermissionAction,
    capability: PermissionCapability,
    target: &str,
) -> (PermissionAction, Option<String>) {
    if action == PermissionAction::Allow {
        if capability == PermissionCapability::Destructive {
            return (
                PermissionAction::Ask,
                Some(
                    "ignoring Allow rule on destructive capability; require explicit per-call approval"
                        .to_string(),
                ),
            );
        }
        if target_is_effectively_wildcard(target) {
            return (
                PermissionAction::Ask,
                Some(
                    "ignoring Allow rule with bare wildcard target; require a narrower target"
                        .to_string(),
                ),
            );
        }
    }
    (action, None)
}

/// True when a rule target is functionally identical to "match anything".
/// We refuse to load or persist Allow rules with such targets because they
/// undo the entire point of the permission system. The check is shared by
/// the on-disk load path (`permission_rules_value`), the session
/// persistence path (`install_persistent_rule`), and the runtime evaluator
/// (`downgrade_unsafe_action`) so the three layers cannot drift.
pub fn target_is_effectively_wildcard(target: &str) -> bool {
    let trimmed = target.trim();
    if trimmed.is_empty() {
        return true;
    }
    trimmed.chars().all(|ch| ch == '*' || ch.is_whitespace())
}

impl Default for PermissionPolicy {
    fn default() -> Self {
        Self::preset(PermissionPolicyMode::Default)
    }
}

impl PermissionPolicy {
    fn preset(mode: PermissionPolicyMode) -> Self {
        let mut shell_sandbox = ShellSandboxConfig {
            network: ShellSandboxNetworkPolicy::AllowWhenApproved,
            ..ShellSandboxConfig::default()
        };
        let mut ai_reviewer = AiReviewerConfig::default();
        let (read, search, edit, shell, ignored_search, web, mcp, git, compiler, destructive) =
            match mode {
                PermissionPolicyMode::Default
                | PermissionPolicyMode::AutoReview
                | PermissionPolicyMode::Custom => (
                    PermissionMode::Allow,
                    PermissionMode::Allow,
                    PermissionMode::Allow,
                    PermissionMode::Allow,
                    PermissionMode::Allow,
                    PermissionMode::Ask,
                    PermissionMode::Ask,
                    PermissionMode::Allow,
                    PermissionMode::Allow,
                    PermissionMode::Ask,
                ),
                PermissionPolicyMode::FullAccess => {
                    shell_sandbox.mode = ShellSandboxMode::Off;
                    (
                        PermissionMode::Allow,
                        PermissionMode::Allow,
                        PermissionMode::Allow,
                        PermissionMode::Allow,
                        PermissionMode::Allow,
                        PermissionMode::Allow,
                        PermissionMode::Allow,
                        PermissionMode::Allow,
                        PermissionMode::Allow,
                        PermissionMode::Allow,
                    )
                }
            };
        if mode == PermissionPolicyMode::AutoReview {
            ai_reviewer.enabled = true;
            ai_reviewer.allow_capabilities = auto_review_allow_capabilities();
        }
        Self {
            mode,
            read,
            search,
            edit,
            shell,
            ignored_search,
            web,
            mcp,
            git,
            compiler,
            destructive,
            shell_classifier: false,
            ai_reviewer,
            shell_sandbox,
            rules: Vec::new(),
        }
    }

    fn legacy_compat_defaults() -> Self {
        Self {
            mode: PermissionPolicyMode::Custom,
            read: PermissionMode::Allow,
            search: PermissionMode::Allow,
            edit: PermissionMode::Allow,
            shell: PermissionMode::Ask,
            ignored_search: PermissionMode::Allow,
            web: PermissionMode::Ask,
            mcp: PermissionMode::Ask,
            git: PermissionMode::Ask,
            compiler: PermissionMode::Ask,
            destructive: PermissionMode::Ask,
            shell_classifier: false,
            ai_reviewer: AiReviewerConfig::default(),
            shell_sandbox: ShellSandboxConfig::default(),
            rules: Vec::new(),
        }
    }

    fn apply_legacy_defaults(&mut self, settings: &PermissionSettings, fan_out_shell: bool) {
        replace_if_some_value(&mut self.read, settings.read);
        replace_if_some_value(&mut self.search, settings.search);
        replace_if_some_value(&mut self.edit, settings.edit);
        if let Some(shell) = settings.shell {
            self.shell = shell;
            if fan_out_shell {
                self.git = shell;
                self.compiler = shell;
                self.destructive = shell;
            }
        }
        replace_if_some_value(&mut self.ignored_search, settings.ignored_search);
        replace_if_some_value(&mut self.web, settings.web);
        replace_if_some_value(&mut self.mcp, settings.mcp);
        replace_if_some_value(&mut self.git, settings.git);
        replace_if_some_value(&mut self.compiler, settings.compiler);
        replace_if_some_value(&mut self.destructive, settings.destructive);
    }

    fn apply_custom_defaults(&mut self, custom: &PermissionCustomSettings) {
        replace_if_some_value(&mut self.read, custom.read);
        replace_if_some_value(&mut self.search, custom.search);
        replace_if_some_value(&mut self.edit, custom.edit);
        replace_if_some_value(&mut self.shell, custom.shell);
        replace_if_some_value(&mut self.ignored_search, custom.ignored_search);
        replace_if_some_value(&mut self.web, custom.network);
        replace_if_some_value(&mut self.mcp, custom.mcp);
        replace_if_some_value(&mut self.git, custom.git);
        replace_if_some_value(&mut self.compiler, custom.compiler);
        replace_if_some_value(&mut self.destructive, custom.destructive);
    }
}

fn auto_review_allow_capabilities() -> Vec<PermissionCapability> {
    vec![
        PermissionCapability::Read,
        PermissionCapability::Search,
        PermissionCapability::Network,
        PermissionCapability::Mcp,
    ]
}

fn parse_permission(value: Option<String>, default: PermissionMode) -> PermissionMode {
    value
        .as_deref()
        .and_then(PermissionMode::parse)
        .unwrap_or(default)
}

fn parse_session_mode(value: Option<String>, default: SessionMode) -> SessionMode {
    value
        .as_deref()
        .and_then(SessionMode::parse)
        .unwrap_or(default)
}

fn parse_session_mode_value(value: &str, source: &str, path: &str) -> Result<SessionMode> {
    SessionMode::parse(value).ok_or_else(|| {
        SqueezyError::Config(format!(
            "{source}: {path}: invalid session mode {value:?}; expected plan or build"
        ))
    })
}

fn parse_session_resume_picker_value(
    value: &str,
    source: &str,
    path: &str,
) -> Result<SessionResumePicker> {
    SessionResumePicker::parse(value).ok_or_else(|| {
        SqueezyError::Config(format!(
            "{source}: {path}: invalid resume picker value {value:?}; expected ask or never"
        ))
    })
}

fn parse_bool(value: Option<String>, default: bool) -> bool {
    value.as_deref().map_or(default, parse_enabled_bool)
}

fn parse_enabled_bool(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn parse_disabled_bool(value: Option<&str>) -> bool {
    matches!(
        value.map(str::trim).map(str::to_ascii_lowercase).as_deref(),
        Some("0" | "false" | "no" | "off" | "disabled")
    )
}

fn parse_usize(value: Option<String>, default: usize) -> usize {
    value
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn parse_u64(value: Option<String>, default: u64) -> u64 {
    value
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelemetryConfig {
    pub enabled: bool,
    pub endpoint: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TelemetrySettings {
    pub enabled: Option<bool>,
    pub endpoint: Option<String>,
}

impl TelemetrySettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(table, &["enabled", "endpoint"], source, path)?;
        Ok(Self {
            enabled: bool_value(table, "enabled", source, &field(path, "enabled"))?,
            endpoint: string_value(table, "endpoint", source, &field(path, "endpoint"))?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.enabled, next.enabled);
        replace_if_some(&mut self.endpoint, next.endpoint);
    }
}

impl TelemetryConfig {
    pub fn from_env_vars(mut var: impl FnMut(&str) -> Option<String>) -> Self {
        Self::from_settings_and_env(TelemetrySettings::default(), &mut var)
    }

    fn from_settings_and_env(
        settings: TelemetrySettings,
        mut var: impl FnMut(&str) -> Option<String>,
    ) -> Self {
        let disabled = parse_disabled_bool(var("SQUEEZY_TELEMETRY").as_deref());
        let endpoint = var("SQUEEZY_TELEMETRY_ENDPOINT")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .or(settings.endpoint)
            .unwrap_or_else(|| DEFAULT_TELEMETRY_ENDPOINT.to_string());
        Self {
            enabled: if disabled {
                false
            } else {
                settings.enabled.unwrap_or(true)
            },
            endpoint,
        }
    }
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            endpoint: DEFAULT_TELEMETRY_ENDPOINT.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackConfig {
    pub enabled: bool,
    pub feedback_endpoint: String,
    pub report_endpoint: String,
    pub max_feedback_bytes: usize,
    pub max_report_bytes: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct FeedbackSettings {
    pub enabled: Option<bool>,
    pub feedback_endpoint: Option<String>,
    pub report_endpoint: Option<String>,
    pub max_feedback_bytes: Option<usize>,
    pub max_report_bytes: Option<usize>,
}

impl FeedbackSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "enabled",
                "feedback_endpoint",
                "report_endpoint",
                "max_feedback_bytes",
                "max_report_bytes",
            ],
            source,
            path,
        )?;
        Ok(Self {
            enabled: bool_value(table, "enabled", source, &field(path, "enabled"))?,
            feedback_endpoint: string_value(
                table,
                "feedback_endpoint",
                source,
                &field(path, "feedback_endpoint"),
            )?,
            report_endpoint: string_value(
                table,
                "report_endpoint",
                source,
                &field(path, "report_endpoint"),
            )?,
            max_feedback_bytes: usize_value(
                table,
                "max_feedback_bytes",
                source,
                &field(path, "max_feedback_bytes"),
            )?,
            max_report_bytes: usize_value(
                table,
                "max_report_bytes",
                source,
                &field(path, "max_report_bytes"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.enabled, next.enabled);
        replace_if_some(&mut self.feedback_endpoint, next.feedback_endpoint);
        replace_if_some(&mut self.report_endpoint, next.report_endpoint);
        replace_if_some(&mut self.max_feedback_bytes, next.max_feedback_bytes);
        replace_if_some(&mut self.max_report_bytes, next.max_report_bytes);
    }
}

impl FeedbackConfig {
    fn from_settings_and_env(
        settings: FeedbackSettings,
        mut var: impl FnMut(&str) -> Option<String>,
    ) -> Self {
        let disabled = parse_disabled_bool(var("SQUEEZY_FEEDBACK").as_deref());
        let feedback_endpoint = var("SQUEEZY_FEEDBACK_ENDPOINT")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .or(settings.feedback_endpoint)
            .unwrap_or_else(|| DEFAULT_FEEDBACK_ENDPOINT.to_string());
        let report_endpoint = var("SQUEEZY_REPORT_ENDPOINT")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .or(settings.report_endpoint)
            .unwrap_or_else(|| DEFAULT_REPORT_ENDPOINT.to_string());
        Self {
            enabled: if disabled {
                false
            } else {
                settings.enabled.unwrap_or(true)
            },
            feedback_endpoint,
            report_endpoint,
            max_feedback_bytes: settings
                .max_feedback_bytes
                .filter(|value| *value > 0)
                .unwrap_or(DEFAULT_FEEDBACK_MAX_BYTES),
            max_report_bytes: settings
                .max_report_bytes
                .filter(|value| *value > 0)
                .unwrap_or(DEFAULT_REPORT_MAX_BYTES),
        }
    }
}

impl Default for FeedbackConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            feedback_endpoint: DEFAULT_FEEDBACK_ENDPOINT.to_string(),
            report_endpoint: DEFAULT_REPORT_ENDPOINT.to_string(),
            max_feedback_bytes: DEFAULT_FEEDBACK_MAX_BYTES,
            max_report_bytes: DEFAULT_REPORT_MAX_BYTES,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RedactionSettings {
    pub custom_patterns: Option<Vec<String>>,
}

impl RedactionSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(table, &["custom_patterns"], source, path)?;
        Ok(Self {
            custom_patterns: string_array_value(
                table,
                "custom_patterns",
                source,
                &field(path, "custom_patterns"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.custom_patterns, next.custom_patterns);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactionConfig {
    pub custom_patterns: Vec<String>,
}

impl RedactionConfig {
    fn from_settings(settings: RedactionSettings) -> Result<Self> {
        let config = Self {
            custom_patterns: settings.custom_patterns.unwrap_or_default(),
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        for (index, pattern) in self.custom_patterns.iter().enumerate() {
            Regex::new(pattern).map_err(|err| {
                SqueezyError::Config(format!(
                    "redaction.custom_patterns.{index}: invalid regex: {err}"
                ))
            })?;
        }
        Ok(())
    }

    pub fn redactor(&self) -> Result<Redactor> {
        Redactor::new(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactedText {
    pub text: String,
    pub redactions: u64,
}

impl RedactedText {
    pub fn unchanged(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            redactions: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Redactor {
    patterns: Vec<RedactionPattern>,
}

#[derive(Debug, Clone)]
struct RedactionPattern {
    kind: &'static str,
    regex: Regex,
}

impl Redactor {
    pub fn new(config: &RedactionConfig) -> Result<Self> {
        let mut patterns = Vec::new();
        for (kind, pattern) in DEFAULT_REDACTION_PATTERNS {
            patterns.push(RedactionPattern {
                kind,
                regex: Regex::new(pattern).map_err(|err| {
                    SqueezyError::Config(format!("built-in redaction pattern {kind}: {err}"))
                })?,
            });
        }
        for (index, pattern) in config.custom_patterns.iter().enumerate() {
            patterns.push(RedactionPattern {
                kind: "custom",
                regex: Regex::new(pattern).map_err(|err| {
                    SqueezyError::Config(format!(
                        "redaction.custom_patterns.{index}: invalid regex: {err}"
                    ))
                })?,
            });
        }
        Ok(Self { patterns })
    }

    pub fn redact(&self, text: &str) -> RedactedText {
        if text.is_empty() {
            return RedactedText::unchanged("");
        }

        // Track allocation lazily: keep `output` borrowed until a pattern
        // actually replaces something, then own the result. This keeps the
        // common no-match case allocation-free, which matters because the
        // redactor runs over every tool result, JSON arg, and model request.
        let mut output: Cow<'_, str> = Cow::Borrowed(text);
        let mut values = BTreeMap::<String, usize>::new();
        let mut redactions = 0u64;
        for pattern in &self.patterns {
            let next = pattern
                .regex
                .replace_all(output.as_ref(), |captures: &Captures<'_>| {
                    redactions += 1;
                    redact_capture(pattern.kind, captures, &mut values)
                });
            if let Cow::Owned(owned) = next {
                output = Cow::Owned(owned);
            }
        }
        match output {
            Cow::Borrowed(_) => RedactedText::unchanged(text),
            Cow::Owned(owned) => RedactedText {
                text: owned,
                redactions,
            },
        }
    }
}

/// Incrementally redacts a streaming text channel.
///
/// Emitting redacted token deltas naively is unsafe: a secret can be split
/// across two stream chunks, and a regex applied to either half misses it.
/// `StreamRedactor` keeps a tail buffer large enough to cover any realistic
/// single-line token plus a "hold" mode that suppresses output entirely
/// while a multi-line PEM block is open. Callers append text with [`push`]
/// (returning what is now safe to emit) and end with [`finish`] (returning
/// any remaining text after a final redaction pass).
///
/// [`push`]: StreamRedactor::push
/// [`finish`]: StreamRedactor::finish
#[derive(Debug)]
pub struct StreamRedactor {
    redactor: std::sync::Arc<Redactor>,
    buffer: String,
    redactions: u64,
    pem_open: bool,
}

/// Maximum number of bytes the stream redactor will keep buffered when no
/// multi-line pattern is open. Sized to comfortably exceed the longest
/// realistic single-line secret (long JWTs, bearer tokens, signed URLs).
const STREAM_TAIL_BYTES: usize = 1024;

const PEM_BEGIN: &str = "-----BEGIN";
const PEM_END: &str = "-----END";

impl StreamRedactor {
    pub fn new(redactor: std::sync::Arc<Redactor>) -> Self {
        Self {
            redactor,
            buffer: String::new(),
            redactions: 0,
            pem_open: false,
        }
    }

    /// Append `delta` to the internal buffer and return whatever portion is
    /// now safe to emit downstream. Returned text is fully redacted.
    pub fn push(&mut self, delta: &str) -> StreamChunk {
        if delta.is_empty() {
            return StreamChunk::empty();
        }
        self.buffer.push_str(delta);
        self.try_emit()
    }

    /// Flush any remaining buffered text after a final redaction pass.
    /// Returns the trailing redacted text and the total redactions seen
    /// since this redactor was created.
    pub fn finish(&mut self) -> StreamChunk {
        if self.buffer.is_empty() {
            return StreamChunk {
                text: String::new(),
                redactions: 0,
            };
        }
        let RedactedText { text, redactions } = self.redactor.redact(&self.buffer);
        self.redactions += redactions;
        self.buffer.clear();
        self.pem_open = false;
        StreamChunk { text, redactions }
    }

    pub fn total_redactions(&self) -> u64 {
        self.redactions
    }

    fn try_emit(&mut self) -> StreamChunk {
        // If we previously opened a PEM block, hold until we see END.
        if self.pem_open {
            if !self.buffer.contains(PEM_END) {
                return StreamChunk::empty();
            }
            self.pem_open = false;
        } else if let Some(begin) = self.buffer.find(PEM_BEGIN)
            && !self.buffer[begin..].contains(PEM_END)
        {
            self.pem_open = true;
            return StreamChunk::empty();
        }

        if self.buffer.len() <= STREAM_TAIL_BYTES {
            return StreamChunk::empty();
        }

        // Redaction markers are idempotent w.r.t. the built-in patterns, so
        // running the redactor over the whole buffer on each push is safe;
        // the previously-emitted prefix has been removed from `buffer`.
        let RedactedText { text, redactions } = self.redactor.redact(&self.buffer);
        self.redactions += redactions;

        if text.len() <= STREAM_TAIL_BYTES {
            self.buffer = text;
            return StreamChunk {
                text: String::new(),
                redactions,
            };
        }

        let mut emit_end = text.len() - STREAM_TAIL_BYTES;
        emit_end = floor_char_boundary(&text, emit_end);
        emit_end = avoid_marker_split(&text, emit_end);
        if emit_end == 0 {
            self.buffer = text;
            return StreamChunk {
                text: String::new(),
                redactions,
            };
        }
        let emitted = text[..emit_end].to_string();
        self.buffer = text[emit_end..].to_string();
        StreamChunk {
            text: emitted,
            redactions,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamChunk {
    pub text: String,
    pub redactions: u64,
}

impl StreamChunk {
    pub fn empty() -> Self {
        Self {
            text: String::new(),
            redactions: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    idx = idx.min(s.len());
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn avoid_marker_split(text: &str, idx: usize) -> usize {
    let prefix = &text[..idx];
    let Some(open) = prefix.rfind("<redacted:") else {
        return idx;
    };
    if prefix[open..].contains('>') {
        return idx;
    }
    floor_char_boundary(text, open)
}

impl Default for Redactor {
    fn default() -> Self {
        RedactionConfig::default()
            .redactor()
            .expect("built-in redaction patterns must compile")
    }
}

fn redact_capture(
    kind: &'static str,
    captures: &Captures<'_>,
    values: &mut BTreeMap<String, usize>,
) -> String {
    let Some(full) = captures.get(0) else {
        return "<redacted:unknown#0 bytes=0>".to_string();
    };
    let value = captures.name("value").unwrap_or(full);
    let replacement = redaction_marker(kind, value.as_str(), values);
    if value.start() == full.start() && value.end() == full.end() {
        replacement
    } else {
        let relative_start = value.start() - full.start();
        let relative_end = value.end() - full.start();
        let full_text = full.as_str();
        format!(
            "{}{}{}",
            &full_text[..relative_start],
            replacement,
            &full_text[relative_end..]
        )
    }
}

fn redaction_marker(
    kind: &'static str,
    value: &str,
    values: &mut BTreeMap<String, usize>,
) -> String {
    let next = values.len() + 1;
    let ordinal = *values.entry(value.to_string()).or_insert(next);
    format!("<redacted:{kind}#{ordinal} bytes={}>", value.len())
}

const DEFAULT_REDACTION_PATTERNS: &[(&str, &str)] = &[
    // Order matters: `secret_assignment` runs first and consumes the value
    // half of `KEY=...`-style strings, so the per-provider patterns below
    // typically only fire on bare tokens that appear without an assignment
    // prefix (for example pasted command output). Keep that contract in
    // mind when reordering.
    //
    // The captured value excludes common trailing punctuation (`)`, `]`,
    // `}`, `>`, plus separators) so that surrounding shape is preserved in
    // shell output like `KEY=foo)` or markdown like `KEY=foo]`.
    (
        "secret_assignment",
        r#"(?i)\b[A-Z0-9_]*(?:API|AUTH|BEARER|CREDENTIAL|KEY|PASSWORD|SECRET|TOKEN)[A-Z0-9_]*\s*=\s*["']?(?P<value>[^\s"',;`)\]}>]+)"#,
    ),
    (
        "url_query",
        r#"(?i)[?&](?:access_token|api-key|api_key|apikey|code|key|signature|sig|token|x-amz-credential|x-amz-security-token|x-amz-signature)=(?P<value>[^&#\s]+)"#,
    ),
    (
        "url_userinfo",
        r#"(?i)https?://(?P<value>[^/\s:@]+:[^/\s@]+)@"#,
    ),
    (
        "bearer_token",
        r#"(?i)\bBearer\s+(?P<value>[A-Za-z0-9._~+/=-]{16,})\b"#,
    ),
    ("anthropic_key", r#"\bsk-ant-[A-Za-z0-9_-]{20,}\b"#),
    ("openai_key", r#"\bsk-[A-Za-z0-9][A-Za-z0-9_-]{20,}\b"#),
    ("google_key", r#"\bAIza[0-9A-Za-z_-]{20,}\b"#),
    ("github_token", r#"\bgh[pousr]_[A-Za-z0-9_]{20,}\b"#),
    ("aws_access_key", r#"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"#),
    (
        "jwt",
        r#"\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\b"#,
    ),
    (
        "private_key",
        r#"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----"#,
    ),
];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct WebSettings {
    pub exa_mcp_url: Option<String>,
    pub exa_api_key_env: Option<String>,
    pub parallel_mcp_url: Option<String>,
    pub parallel_api_key_env: Option<String>,
    /// Pluggable websearch backend selector. Valid values: `exa`,
    /// `parallel`.
    pub websearch_provider: Option<String>,
}

impl WebSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "exa_mcp_url",
                "exa_api_key_env",
                "parallel_mcp_url",
                "parallel_api_key_env",
                "websearch_provider",
            ],
            source,
            path,
        )?;
        Ok(Self {
            exa_mcp_url: string_value(table, "exa_mcp_url", source, &field(path, "exa_mcp_url"))?,
            exa_api_key_env: string_value(
                table,
                "exa_api_key_env",
                source,
                &field(path, "exa_api_key_env"),
            )?,
            parallel_mcp_url: string_value(
                table,
                "parallel_mcp_url",
                source,
                &field(path, "parallel_mcp_url"),
            )?,
            parallel_api_key_env: string_value(
                table,
                "parallel_api_key_env",
                source,
                &field(path, "parallel_api_key_env"),
            )?,
            websearch_provider: string_value(
                table,
                "websearch_provider",
                source,
                &field(path, "websearch_provider"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.exa_mcp_url, next.exa_mcp_url);
        replace_if_some(&mut self.exa_api_key_env, next.exa_api_key_env);
        replace_if_some(&mut self.parallel_mcp_url, next.parallel_mcp_url);
        replace_if_some(&mut self.parallel_api_key_env, next.parallel_api_key_env);
        replace_if_some(&mut self.websearch_provider, next.websearch_provider);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SkillsSettings {
    pub user_dir: Option<PathBuf>,
    pub compat_user_dir: Option<PathBuf>,
    /// Additional filesystem roots scanned during skill discovery. Each
    /// entry is treated like a user-level skills directory: skills loaded
    /// from these roots use [`SkillSource::ExtraRoot`] precedence, which
    /// sits above the personal `user_dir` but below project-local skills
    /// so a workspace's `.squeezy/skills/` still wins on name collisions.
    /// Use this to point at a shared mount, network drive, or vendored
    /// git submodule without standing up a marketplace or plugin host.
    pub extra_roots: Vec<PathBuf>,
    pub active_budget_chars: Option<usize>,
    pub active_body_cap_chars: Option<usize>,
    pub preamble_enabled: Option<bool>,
    pub preamble_budget_chars: Option<usize>,
    pub active_budget_mode: Option<SkillsBudgetMode>,
    pub preamble_budget_mode: Option<SkillsBudgetMode>,
    /// When `Some(true)`, restore the legacy behavior of inlining each
    /// activated skill's full body into the system prompt. The default
    /// (`None` / `Some(false)`) emits metadata-only blocks so the model
    /// pays for the body only when it explicitly calls `load_skill`.
    pub inline: Option<bool>,
    pub config: Vec<SkillConfigEntry>,
}

impl SkillsSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "user_dir",
                "compat_user_dir",
                "extra_roots",
                "active_budget_chars",
                "active_body_cap_chars",
                "preamble_enabled",
                "preamble_budget_chars",
                "active_budget_mode",
                "preamble_budget_mode",
                "inline",
                "config",
            ],
            source,
            path,
        )?;
        Ok(Self {
            user_dir: path_value(table, "user_dir", source, &field(path, "user_dir"))?,
            compat_user_dir: path_value(
                table,
                "compat_user_dir",
                source,
                &field(path, "compat_user_dir"),
            )?,
            extra_roots: path_array_value(
                table,
                "extra_roots",
                source,
                &field(path, "extra_roots"),
            )?,
            active_budget_chars: usize_value(
                table,
                "active_budget_chars",
                source,
                &field(path, "active_budget_chars"),
            )?,
            active_body_cap_chars: usize_value(
                table,
                "active_body_cap_chars",
                source,
                &field(path, "active_body_cap_chars"),
            )?,
            preamble_enabled: bool_value(
                table,
                "preamble_enabled",
                source,
                &field(path, "preamble_enabled"),
            )?,
            preamble_budget_chars: usize_value(
                table,
                "preamble_budget_chars",
                source,
                &field(path, "preamble_budget_chars"),
            )?,
            active_budget_mode: skills_budget_mode_value(
                table,
                "active_budget_mode",
                source,
                &field(path, "active_budget_mode"),
            )?,
            preamble_budget_mode: skills_budget_mode_value(
                table,
                "preamble_budget_mode",
                source,
                &field(path, "preamble_budget_mode"),
            )?,
            inline: bool_value(table, "inline", source, &field(path, "inline"))?,
            config: skill_config_entries_value(table, source, &field(path, "config"))?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.user_dir, next.user_dir);
        replace_if_some(&mut self.compat_user_dir, next.compat_user_dir);
        self.extra_roots.extend(next.extra_roots);
        replace_if_some(&mut self.active_budget_chars, next.active_budget_chars);
        replace_if_some(&mut self.active_body_cap_chars, next.active_body_cap_chars);
        replace_if_some(&mut self.preamble_enabled, next.preamble_enabled);
        replace_if_some(&mut self.preamble_budget_chars, next.preamble_budget_chars);
        replace_if_some(&mut self.active_budget_mode, next.active_budget_mode);
        replace_if_some(&mut self.preamble_budget_mode, next.preamble_budget_mode);
        replace_if_some(&mut self.inline, next.inline);
        self.config.extend(next.config);
    }
}

pub const DEFAULT_SKILLS_ACTIVE_BUDGET_CHARS: usize = 4_000;
pub const DEFAULT_SKILLS_ACTIVE_BODY_CAP_CHARS: usize = 16_000;
pub const DEFAULT_SKILLS_PREAMBLE_ENABLED: bool = true;
pub const DEFAULT_SKILLS_PREAMBLE_BUDGET_CHARS: usize = 800;
/// Default for `[skills] inline`. The metadata-only default keeps skill
/// bodies out of the system prompt; users that want the legacy behavior
/// of inlining each activated skill's body can set `[skills] inline = true`.
pub const DEFAULT_SKILLS_INLINE: bool = false;
/// Default fraction of `model_context_window` (in percent) consumed by the
/// active and available-skills bundles when no explicit chars budget is set.
/// Matches the codex reference (`SKILL_METADATA_CONTEXT_WINDOW_PERCENT=2`).
pub const DEFAULT_SKILLS_BUDGET_CONTEXT_PERCENT: f32 = 2.0;
/// Conservative chars-per-token used when scaling a token-denominated context
/// window into a chars budget. Mirrors `CHARS_PER_TOKEN` in `ai_reviewer.rs`.
pub const SKILLS_CHARS_PER_TOKEN: u64 = 4;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillConfigEntry {
    pub name: Option<String>,
    pub path: Option<PathBuf>,
    pub enabled: bool,
}

/// Selects how a skills budget is computed at render time.
///
/// `Chars` is an absolute cap and ignores the context window. `ContextPercent`
/// scales the budget to a fraction of `model_context_window` (converted to
/// chars via [`SKILLS_CHARS_PER_TOKEN`]), so larger-context models get
/// proportionally more room for skill instructions.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SkillsBudgetMode {
    /// Absolute character cap, regardless of context window.
    Chars { chars: usize },
    /// Percentage of the model context window (0..=100), converted to chars
    /// via `SKILLS_CHARS_PER_TOKEN`.
    ContextPercent { percent: f32 },
}

impl Default for SkillsBudgetMode {
    fn default() -> Self {
        Self::ContextPercent {
            percent: DEFAULT_SKILLS_BUDGET_CONTEXT_PERCENT,
        }
    }
}

impl SkillsBudgetMode {
    /// Computes the effective character budget for this mode. `Chars(n)` is
    /// returned verbatim. `ContextPercent(p)` scales `model_context_window`
    /// (in tokens) to chars and applies `p%`; if the window is unknown, the
    /// caller's `fallback_chars` is used (typically the legacy
    /// `*_budget_chars` default).
    pub fn effective_chars(
        &self,
        model_context_window: Option<u64>,
        fallback_chars: usize,
    ) -> usize {
        match *self {
            Self::Chars { chars } => chars,
            Self::ContextPercent { percent } => {
                let Some(window) = model_context_window else {
                    return fallback_chars;
                };
                // Clamp to non-negative and bound the float math: at worst a
                // 200K-token window with percent=100 yields ~800k chars, well
                // under f32's safe integer range (~16M).
                let percent = percent.max(0.0);
                let chars_per_token = SKILLS_CHARS_PER_TOKEN as f32;
                let effective = (window as f32) * chars_per_token * percent / 100.0;
                effective.round().max(0.0) as usize
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillsConfig {
    pub user_dir: PathBuf,
    pub compat_user_dir: PathBuf,
    /// Additional filesystem roots walked during skill discovery. Their
    /// skills live at [`SkillSource::ExtraRoot`] precedence, above the
    /// personal `user_dir` and below project-local skills, so a workspace
    /// can still override a shared catalog by dropping a same-name skill
    /// in `.squeezy/skills/`. Non-existent entries are reported via
    /// `tracing::warn!` at discovery time and otherwise skipped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_roots: Vec<PathBuf>,
    pub active_budget_chars: usize,
    pub active_body_cap_chars: usize,
    pub preamble_enabled: bool,
    pub preamble_budget_chars: usize,
    /// Mode used to compute the active skills bundle budget at render time.
    /// Falls back to `Chars(active_budget_chars)` when only the legacy field
    /// is set in user settings.
    pub active_budget_mode: SkillsBudgetMode,
    /// Mode used to compute the available-skills preamble budget at render
    /// time. Falls back to `Chars(preamble_budget_chars)` when only the
    /// legacy field is set in user settings.
    pub preamble_budget_mode: SkillsBudgetMode,
    /// When `true`, the active-skills bundle inlines each skill's full
    /// body into the system prompt (the legacy behavior). The default
    /// (`false`) emits metadata-only blocks; the model fetches a body on
    /// demand via the `load_skill` tool.
    pub inline: bool,
    /// Token budget for the active model, copied from
    /// `context_compaction.model_context_window`. `None` keeps
    /// `ContextPercent` modes dormant and forces a fall-back to
    /// `*_budget_chars`.
    pub model_context_window: Option<u64>,
    pub config: Vec<SkillConfigEntry>,
}

impl SkillsConfig {
    pub fn from_env_vars(mut var: impl FnMut(&str) -> Option<String>) -> Self {
        Self::from_settings_and_env_vars(SkillsSettings::default(), &mut var)
    }

    fn from_settings_and_env_vars(
        settings: SkillsSettings,
        mut var: impl FnMut(&str) -> Option<String>,
    ) -> Self {
        // Legacy `*_budget_chars` keys keep working: if the user only set the
        // chars field (and not the new mode), treat it as a Chars-mode
        // override so behavior matches the pre-mode release exactly.
        let active_budget_chars = settings
            .active_budget_chars
            .unwrap_or(DEFAULT_SKILLS_ACTIVE_BUDGET_CHARS);
        let preamble_budget_chars = settings
            .preamble_budget_chars
            .unwrap_or(DEFAULT_SKILLS_PREAMBLE_BUDGET_CHARS);
        let active_budget_mode = settings
            .active_budget_mode
            .or_else(|| {
                settings
                    .active_budget_chars
                    .map(|chars| SkillsBudgetMode::Chars { chars })
            })
            .unwrap_or_default();
        let preamble_budget_mode = settings
            .preamble_budget_mode
            .or_else(|| {
                settings
                    .preamble_budget_chars
                    .map(|chars| SkillsBudgetMode::Chars { chars })
            })
            .unwrap_or_default();
        Self {
            user_dir: expand_home_path(
                var("SQUEEZY_SKILLS_USER_DIR")
                    .map(PathBuf::from)
                    .or(settings.user_dir)
                    .unwrap_or_else(default_squeezy_skills_dir),
            ),
            compat_user_dir: expand_home_path(
                var("SQUEEZY_SKILLS_COMPAT_USER_DIR")
                    .map(PathBuf::from)
                    .or(settings.compat_user_dir)
                    .unwrap_or_else(default_agent_compat_skills_dir),
            ),
            extra_roots: settings
                .extra_roots
                .into_iter()
                .map(expand_home_path)
                .collect(),
            active_budget_chars,
            active_body_cap_chars: settings
                .active_body_cap_chars
                .unwrap_or(DEFAULT_SKILLS_ACTIVE_BODY_CAP_CHARS),
            preamble_enabled: settings
                .preamble_enabled
                .unwrap_or(DEFAULT_SKILLS_PREAMBLE_ENABLED),
            preamble_budget_chars,
            active_budget_mode,
            preamble_budget_mode,
            inline: settings.inline.unwrap_or(DEFAULT_SKILLS_INLINE),
            model_context_window: None,
            config: settings
                .config
                .into_iter()
                .map(|entry| SkillConfigEntry {
                    name: entry.name,
                    path: entry.path.map(expand_home_path),
                    enabled: entry.enabled,
                })
                .collect(),
        }
    }

    /// Computes the active skills bundle budget for the current context
    /// window. Falls through to `active_budget_chars` when the mode is
    /// `ContextPercent` but no window is configured.
    pub fn active_budget_effective_chars(&self) -> usize {
        self.active_budget_mode
            .effective_chars(self.model_context_window, self.active_budget_chars)
    }

    /// Computes the available-skills preamble budget for the current context
    /// window. Falls through to `preamble_budget_chars` when the mode is
    /// `ContextPercent` but no window is configured.
    pub fn preamble_budget_effective_chars(&self) -> usize {
        self.preamble_budget_mode
            .effective_chars(self.model_context_window, self.preamble_budget_chars)
    }
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            user_dir: default_squeezy_skills_dir(),
            compat_user_dir: default_agent_compat_skills_dir(),
            extra_roots: Vec::new(),
            active_budget_chars: DEFAULT_SKILLS_ACTIVE_BUDGET_CHARS,
            active_body_cap_chars: DEFAULT_SKILLS_ACTIVE_BODY_CAP_CHARS,
            preamble_enabled: DEFAULT_SKILLS_PREAMBLE_ENABLED,
            preamble_budget_chars: DEFAULT_SKILLS_PREAMBLE_BUDGET_CHARS,
            active_budget_mode: SkillsBudgetMode::default(),
            preamble_budget_mode: SkillsBudgetMode::default(),
            inline: DEFAULT_SKILLS_INLINE,
            model_context_window: None,
            config: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphConfig {
    pub languages: Vec<String>,
    pub max_file_bytes: u64,
    pub include_hidden: bool,
    pub require_indexing_signal: bool,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub include_classes: Vec<String>,
    pub exclude_classes: Vec<String>,
}

impl GraphConfig {
    fn from_settings(settings: GraphSettings) -> Self {
        Self {
            languages: settings
                .languages
                .unwrap_or_else(|| vec!["rust".to_string(), "python".to_string()]),
            max_file_bytes: settings.max_file_bytes.unwrap_or(1_000_000),
            include_hidden: settings.include_hidden.unwrap_or(false),
            require_indexing_signal: settings.require_indexing_signal.unwrap_or(true),
            include: settings.include.unwrap_or_default(),
            exclude: settings.exclude.unwrap_or_default(),
            include_classes: settings.include_classes.unwrap_or_default(),
            exclude_classes: settings.exclude_classes.unwrap_or_default(),
        }
    }
}

impl Default for GraphConfig {
    fn default() -> Self {
        Self::from_settings(GraphSettings::default())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct GraphSettings {
    pub languages: Option<Vec<String>>,
    pub max_file_bytes: Option<u64>,
    pub include_hidden: Option<bool>,
    pub require_indexing_signal: Option<bool>,
    pub include: Option<Vec<String>>,
    pub exclude: Option<Vec<String>>,
    pub include_classes: Option<Vec<String>>,
    pub exclude_classes: Option<Vec<String>>,
}

impl GraphSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "languages",
                "max_file_bytes",
                "include_hidden",
                "require_indexing_signal",
                "include",
                "exclude",
                "include_classes",
                "exclude_classes",
            ],
            source,
            path,
        )?;
        Ok(Self {
            languages: string_array_value(table, "languages", source, &field(path, "languages"))?,
            max_file_bytes: u64_value(
                table,
                "max_file_bytes",
                source,
                &field(path, "max_file_bytes"),
            )?,
            include_hidden: bool_value(
                table,
                "include_hidden",
                source,
                &field(path, "include_hidden"),
            )?,
            require_indexing_signal: bool_value(
                table,
                "require_indexing_signal",
                source,
                &field(path, "require_indexing_signal"),
            )?,
            include: string_array_value(table, "include", source, &field(path, "include"))?,
            exclude: string_array_value(table, "exclude", source, &field(path, "exclude"))?,
            include_classes: string_array_value(
                table,
                "include_classes",
                source,
                &field(path, "include_classes"),
            )?,
            exclude_classes: string_array_value(
                table,
                "exclude_classes",
                source,
                &field(path, "exclude_classes"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.languages, next.languages);
        replace_if_some(&mut self.max_file_bytes, next.max_file_bytes);
        replace_if_some(&mut self.include_hidden, next.include_hidden);
        replace_if_some(
            &mut self.require_indexing_signal,
            next.require_indexing_signal,
        );
        replace_if_some(&mut self.include, next.include);
        replace_if_some(&mut self.exclude, next.exclude);
        replace_if_some(&mut self.include_classes, next.include_classes);
        replace_if_some(&mut self.exclude_classes, next.exclude_classes);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheConfig {
    pub root: Option<PathBuf>,
    pub tool_outputs: Option<PathBuf>,
}

impl CacheConfig {
    fn from_settings(settings: CacheSettings) -> Self {
        Self {
            root: settings.root,
            tool_outputs: settings.tool_outputs,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CacheSettings {
    pub root: Option<PathBuf>,
    pub tool_outputs: Option<PathBuf>,
}

impl CacheSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(table, &["root", "tool_outputs"], source, path)?;
        Ok(Self {
            root: path_value(table, "root", source, &field(path, "root"))?,
            tool_outputs: path_value(table, "tool_outputs", source, &field(path, "tool_outputs"))?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.root, next.root);
        replace_if_some(&mut self.tool_outputs, next.tool_outputs);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StatusVerbosity {
    Compact,
    Verbose,
}

impl StatusVerbosity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Verbose => "verbose",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseVerbosity {
    Concise,
    Normal,
    Verbose,
}

impl ResponseVerbosity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Concise => "concise",
            Self::Normal => "normal",
            Self::Verbose => "verbose",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutputVerbosity {
    Compact,
    Normal,
    Verbose,
}

impl ToolOutputVerbosity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Normal => "normal",
            Self::Verbose => "verbose",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellDiffInline {
    /// Render unified-diff output from shell commands in full, bypassing the
    /// collapsed-card head/tail preview cap. Default — a `git diff` card is
    /// only useful when every hunk is visible.
    Full,
    /// Keep shell-produced diffs on the same head/tail preview budget as
    /// other shell output. For users who run `git diff` against large files
    /// often enough that uncapped inline diffs overwhelm the transcript.
    Folded,
}

impl ShellDiffInline {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Folded => "folded",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptDefault {
    Compact,
    Expanded,
}

impl TranscriptDefault {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Expanded => "expanded",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TuiAlternateScreen {
    Auto,
    Never,
    Always,
}

impl TuiAlternateScreen {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Never => "never",
            Self::Always => "always",
        }
    }
}

/// Controls whether the TUI wraps each frame draw in DEC mode 2026
/// (Begin/End Synchronized Update). Capable terminals (kitty, WezTerm,
/// Ghostty, iTerm2, Alacritty) flip the entire frame atomically, which
/// eliminates the cell-by-cell tearing visible during fast streaming.
/// The sequences are spec'd to be silently ignored by terminals that
/// do not implement them, so emitting them is safe by default.
///
/// `Auto` enables wrapping when capability is signalled via environment
/// (`KITTY_WINDOW_ID`, `WEZTERM_PANE`, `GHOSTTY_RESOURCES_DIR`,
/// `ALACRITTY_LOG`, `TERM_PROGRAM` matching iTerm/WezTerm/Ghostty, or
/// `TERM` containing `kitty`/`alacritty`/`wezterm`/`ghostty`). `Always`
/// forces wrapping on regardless of detection (useful for terminals
/// that do not advertise themselves but honour the sequence). `Never`
/// disables wrapping entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TuiSynchronizedOutput {
    Auto,
    Always,
    Never,
}

impl TuiSynchronizedOutput {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Always => "always",
            Self::Never => "never",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "always" | "on" | "true" => Some(Self::Always),
            "never" | "off" | "false" => Some(Self::Never),
            _ => None,
        }
    }
}

pub const DEFAULT_TUI_THEME_NAME: &str = "default";

pub const BUILTIN_TUI_THEME_NAMES: &[&str] =
    &["default", "bright", "fun", "catppuccin", "high-contrast"];

pub const TUI_THEME_COLOR_TOKENS: &[&str] = &[
    "palette.accent",
    "palette.secondary",
    "palette.red",
    "palette.green",
    "palette.yellow",
    "palette.blue",
    "palette.magenta",
    "palette.cyan",
    "ui.background",
    "ui.foreground",
    "ui.border",
    "ui.muted",
    "ui.quiet",
    "ui.footer",
    "ui.surface",
    "ui.prompt_bg",
    "syntax.keyword",
    "syntax.string",
    "syntax.comment",
    "syntax.literal",
    "syntax.function",
    "syntax.type",
    "syntax.operator",
    "syntax.variable",
    "status.ok",
    "status.warn",
    "status.err",
    "status.info",
    "transcript.user",
    "transcript.assistant",
    "transcript.tool",
    "transcript.system",
    "diff.added",
    "diff.removed",
    "diff.added_bg",
    "diff.removed_bg",
    "diff.context",
    "diff.hunk",
    "effects.shimmer",
    "separator.primary",
    "inline.code",
    "inline.model",
    "path.hint",
];

pub type TuiRgb = [u8; 3];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TuiThemeSettings {
    pub colors: BTreeMap<String, TuiRgb>,
}

impl TuiThemeSettings {
    fn merge(&mut self, next: Self) {
        for (token, rgb) in next.colors {
            self.colors.insert(token, rgb);
        }
    }
}

pub fn normalize_tui_theme_name(value: &str) -> Option<String> {
    let normalized = value.trim().to_ascii_lowercase().replace('_', "-");
    let canonical = match normalized.as_str() {
        "system" | "auto" | "dark" => DEFAULT_TUI_THEME_NAME,
        "light" => "bright",
        "mauve" => "catppuccin",
        "highcontrast" | "hc" => "high-contrast",
        other if is_valid_tui_theme_name(other) => other,
        _ => return None,
    };
    Some(canonical.to_string())
}

pub fn is_builtin_tui_theme_name(value: &str) -> bool {
    BUILTIN_TUI_THEME_NAMES.contains(&value)
}

pub fn is_tui_theme_color_token(value: &str) -> bool {
    TUI_THEME_COLOR_TOKENS.contains(&value)
}

pub fn is_valid_tui_theme_name(value: &str) -> bool {
    let len = value.len();
    len > 0
        && len <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// Off-screen notification surface for turn-complete and approval-pending
/// events. Maps directly to bytes emitted to the controlling terminal:
/// `Bel` writes `\x07`, `Osc9` writes the iTerm-style OSC 9 desktop
/// notification escape, `Auto` picks OSC 9 when `$TERM_PROGRAM` matches a
/// known capable terminal and falls back to BEL otherwise. `Off` is the
/// default so a fresh install never makes noise the user did not ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationMethod {
    Off,
    Bel,
    Osc9,
    Auto,
}

impl NotificationMethod {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Bel => "bel",
            Self::Osc9 => "osc9",
            Self::Auto => "auto",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "false" => Some(Self::Off),
            "bel" | "bell" => Some(Self::Bel),
            "osc9" | "osc-9" | "osc_9" => Some(Self::Osc9),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TuiConfig {
    pub tick_rate_ms: u64,
    pub status_verbosity: StatusVerbosity,
    pub response_verbosity: ResponseVerbosity,
    pub tool_output_verbosity: ToolOutputVerbosity,
    pub transcript_default: TranscriptDefault,
    pub alternate_screen: TuiAlternateScreen,
    /// DEC 2026 synchronized-output policy. `Auto` flips on for known
    /// capable terminals; `Always` forces it on; `Never` disables it.
    /// See [`TuiSynchronizedOutput`] for the capability heuristic.
    pub synchronized_output: TuiSynchronizedOutput,
    pub show_reasoning_usage: bool,
    /// Render-time grouping of adjacent same-tool same-status calls into
    /// one card (e.g. three back-to-back `read_file` calls become "Read 3
    /// files"). Independent of the push-time retry coalescer. Default
    /// `true`; flip to `false` to keep every tool call on its own row.
    pub coalesce_tool_runs: bool,
    /// Ordered list of status-line item identifiers. `None` means
    /// "use the built-in default list"; an empty list means the user
    /// deliberately disabled the detail line.
    pub status_line: Option<Vec<String>>,
    /// Color status-line items with their accent palette.
    /// Defaults to `true`.
    pub status_line_use_colors: bool,
    /// Active named TUI theme. Builtins are `default`, `bright`, `fun`,
    /// `catppuccin`, and `high-contrast`; user settings may add more names.
    pub theme: String,
    /// User-defined or overridden theme colors, merged through the normal
    /// settings precedence chain.
    pub themes: BTreeMap<String, TuiThemeSettings>,
    /// Off-screen attention surface (OSC 9 desktop notification / BEL).
    /// Fires on turn-complete and approval-pending; default `Off`.
    pub desktop_notifications: NotificationMethod,
    /// Mirror the in-memory prompt-recall ring to a flat file on disk
    /// (default `~/.squeezy/prompt_history`, XDG-compatible). Off by
    /// default — history survives across sessions only when the user
    /// opts in, matching shell-history conventions and avoiding any
    /// surprise persisted plaintext for users who'd rather not have it.
    pub persist_prompt_history: bool,
    /// User-supplied key rebindings for the TUI composer / chat surface.
    /// Keyed by an action slug (e.g. `transcript_overlay`, `page_up`);
    /// the value is a key spec like `"Ctrl+t"` or `"PageUp"`. Unknown
    /// slugs and unparseable specs are surfaced by the TUI when
    /// `/keymap` is invoked.
    pub keymap: BTreeMap<String, String>,
    /// Whether `git diff`-style output from shell tools renders in full
    /// (default) or stays under the collapsed-card head/tail preview cap.
    pub shell_diff_inline: ShellDiffInline,
}

impl TuiConfig {
    fn from_settings(settings: TuiSettings) -> Self {
        Self {
            tick_rate_ms: settings.tick_rate_ms.unwrap_or(DEFAULT_TICK_RATE_MS),
            status_verbosity: settings
                .status_verbosity
                .unwrap_or(StatusVerbosity::Compact),
            response_verbosity: settings
                .response_verbosity
                .unwrap_or(ResponseVerbosity::Normal),
            tool_output_verbosity: settings
                .tool_output_verbosity
                .unwrap_or(ToolOutputVerbosity::Compact),
            transcript_default: settings
                .transcript_default
                .unwrap_or(TranscriptDefault::Compact),
            alternate_screen: settings
                .alternate_screen
                .unwrap_or(TuiAlternateScreen::Auto),
            synchronized_output: settings
                .synchronized_output
                .unwrap_or(TuiSynchronizedOutput::Auto),
            show_reasoning_usage: settings.show_reasoning_usage.unwrap_or(true),
            coalesce_tool_runs: settings.coalesce_tool_runs.unwrap_or(true),
            status_line: settings.status_line,
            status_line_use_colors: settings.status_line_use_colors.unwrap_or(true),
            theme: settings
                .theme
                .unwrap_or_else(|| DEFAULT_TUI_THEME_NAME.to_string()),
            themes: settings.themes.unwrap_or_default(),
            desktop_notifications: settings
                .desktop_notifications
                .unwrap_or(NotificationMethod::Off),
            persist_prompt_history: settings.persist_prompt_history.unwrap_or(false),
            keymap: settings.keymap.unwrap_or_default(),
            shell_diff_inline: settings.shell_diff_inline.unwrap_or(ShellDiffInline::Full),
        }
    }
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self::from_settings(TuiSettings::default())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TuiSettings {
    pub tick_rate_ms: Option<u64>,
    pub status_verbosity: Option<StatusVerbosity>,
    pub response_verbosity: Option<ResponseVerbosity>,
    pub tool_output_verbosity: Option<ToolOutputVerbosity>,
    pub transcript_default: Option<TranscriptDefault>,
    pub alternate_screen: Option<TuiAlternateScreen>,
    pub synchronized_output: Option<TuiSynchronizedOutput>,
    pub show_reasoning_usage: Option<bool>,
    pub coalesce_tool_runs: Option<bool>,
    pub status_line: Option<Vec<String>>,
    pub status_line_use_colors: Option<bool>,
    pub theme: Option<String>,
    pub themes: Option<BTreeMap<String, TuiThemeSettings>>,
    pub desktop_notifications: Option<NotificationMethod>,
    pub persist_prompt_history: Option<bool>,
    pub keymap: Option<BTreeMap<String, String>>,
    pub shell_diff_inline: Option<ShellDiffInline>,
}

impl TuiSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "tick_rate_ms",
                "status_verbosity",
                "response_verbosity",
                "tool_output_verbosity",
                "transcript_default",
                "alternate_screen",
                "synchronized_output",
                "show_reasoning_usage",
                "coalesce_tool_runs",
                "status_line",
                "status_line_use_colors",
                "theme",
                "themes",
                "desktop_notifications",
                "persist_prompt_history",
                "keymap",
                "shell_diff_inline",
            ],
            source,
            path,
        )?;
        Ok(Self {
            tick_rate_ms: u64_value(table, "tick_rate_ms", source, &field(path, "tick_rate_ms"))?,
            status_verbosity: status_verbosity_value(
                table,
                "status_verbosity",
                source,
                &field(path, "status_verbosity"),
            )?,
            response_verbosity: response_verbosity_value(
                table,
                "response_verbosity",
                source,
                &field(path, "response_verbosity"),
            )?,
            tool_output_verbosity: tool_output_verbosity_value(
                table,
                "tool_output_verbosity",
                source,
                &field(path, "tool_output_verbosity"),
            )?,
            transcript_default: transcript_default_value(
                table,
                "transcript_default",
                source,
                &field(path, "transcript_default"),
            )?,
            alternate_screen: tui_alternate_screen_value(
                table,
                "alternate_screen",
                source,
                &field(path, "alternate_screen"),
            )?,
            synchronized_output: tui_synchronized_output_value(
                table,
                "synchronized_output",
                source,
                &field(path, "synchronized_output"),
            )?,
            show_reasoning_usage: bool_value(
                table,
                "show_reasoning_usage",
                source,
                &field(path, "show_reasoning_usage"),
            )?,
            coalesce_tool_runs: bool_value(
                table,
                "coalesce_tool_runs",
                source,
                &field(path, "coalesce_tool_runs"),
            )?,
            status_line: string_array_value(
                table,
                "status_line",
                source,
                &field(path, "status_line"),
            )?,
            status_line_use_colors: bool_value(
                table,
                "status_line_use_colors",
                source,
                &field(path, "status_line_use_colors"),
            )?,
            theme: tui_theme_value(table, "theme", source, &field(path, "theme"))?,
            themes: tui_themes_value(table, "themes", source, &field(path, "themes"))?,
            desktop_notifications: notification_method_value(
                table,
                "desktop_notifications",
                source,
                &field(path, "desktop_notifications"),
            )?,
            persist_prompt_history: bool_value(
                table,
                "persist_prompt_history",
                source,
                &field(path, "persist_prompt_history"),
            )?,
            keymap: string_map_value(table, "keymap", source, &field(path, "keymap"))?,
            shell_diff_inline: shell_diff_inline_value(
                table,
                "shell_diff_inline",
                source,
                &field(path, "shell_diff_inline"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.tick_rate_ms, next.tick_rate_ms);
        replace_if_some(&mut self.status_verbosity, next.status_verbosity);
        replace_if_some(&mut self.response_verbosity, next.response_verbosity);
        replace_if_some(&mut self.tool_output_verbosity, next.tool_output_verbosity);
        replace_if_some(&mut self.transcript_default, next.transcript_default);
        replace_if_some(&mut self.alternate_screen, next.alternate_screen);
        replace_if_some(&mut self.synchronized_output, next.synchronized_output);
        replace_if_some(&mut self.show_reasoning_usage, next.show_reasoning_usage);
        replace_if_some(&mut self.status_line, next.status_line);
        replace_if_some(
            &mut self.status_line_use_colors,
            next.status_line_use_colors,
        );
        replace_if_some(&mut self.theme, next.theme);
        merge_option(&mut self.themes, next.themes, merge_tui_theme_maps);
        replace_if_some(&mut self.desktop_notifications, next.desktop_notifications);
        replace_if_some(
            &mut self.persist_prompt_history,
            next.persist_prompt_history,
        );
        replace_if_some(&mut self.keymap, next.keymap);
        replace_if_some(&mut self.shell_diff_inline, next.shell_diff_inline);
    }
}

pub fn default_settings_path() -> PathBuf {
    if let Some(custom) = env::var_os("SQUEEZY_SETTINGS_PATH") {
        return PathBuf::from(custom);
    }
    if let Some(home) = home_squeezy_subpath("settings.toml") {
        return home;
    }
    if let Some(config) = dirs::config_dir() {
        return config.join("squeezy").join("settings.toml");
    }
    PathBuf::from(".squeezy/settings.toml")
}

/// Path of the on-disk prompt-recall ring backing `[tui]
/// .persist_prompt_history`. Prefers `$HOME/.squeezy/prompt_history`
/// for parity with `default_settings_path`; falls back to
/// `dirs::data_dir()/squeezy/prompt_history` (XDG-compatible:
/// `$XDG_DATA_HOME/squeezy/prompt_history` on Linux,
/// `%APPDATA%\squeezy\prompt_history` on Windows). Overridable with
/// `SQUEEZY_PROMPT_HISTORY_PATH` for tests and power users who keep
/// their dotfiles elsewhere.
pub fn default_prompt_history_path() -> PathBuf {
    if let Some(custom) = env::var_os("SQUEEZY_PROMPT_HISTORY_PATH") {
        return PathBuf::from(custom);
    }
    if let Some(home) = home_squeezy_subpath("prompt_history") {
        return home;
    }
    if let Some(data) = dirs::data_dir() {
        return data.join("squeezy").join("prompt_history");
    }
    PathBuf::from(".squeezy/prompt_history")
}

pub fn default_projects_dir() -> PathBuf {
    if let Some(custom) = env::var_os("SQUEEZY_PROJECTS_DIR") {
        return PathBuf::from(custom);
    }
    if let Some(home) = home_squeezy_subpath("projects") {
        return home;
    }
    if let Some(config) = dirs::config_dir() {
        return config.join("squeezy").join("projects");
    }
    PathBuf::from(".squeezy/projects")
}

fn home_squeezy_subpath(name: &str) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(".squeezy").join(name))
    }
    #[cfg(not(unix))]
    {
        let _ = name;
        None
    }
}

pub fn repo_settings_id(root: impl AsRef<Path>) -> String {
    let root = root.as_ref();
    let canonical = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let display = canonical.display().to_string();
    let name = canonical
        .file_name()
        .and_then(|name| name.to_str())
        .map(sanitize_repo_settings_name)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "repo".to_string());
    format!("{name}-{:016x}", fnv1a64(display.as_bytes()))
}

pub fn per_repo_settings_path(root: impl AsRef<Path>) -> PathBuf {
    default_projects_dir()
        .join(repo_settings_id(root))
        .join("settings.toml")
}

fn sanitize_repo_settings_name(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

pub fn default_squeezy_skills_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(DEFAULT_SQUEEZY_SKILLS_DIR))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SQUEEZY_SKILLS_DIR))
}

pub fn default_agent_compat_skills_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(DEFAULT_AGENT_COMPAT_SKILLS_DIR))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_AGENT_COMPAT_SKILLS_DIR))
}

fn expand_home_path(path: PathBuf) -> PathBuf {
    let Some(path_str) = path.to_str() else {
        return path;
    };
    if path_str == "~" {
        return env::var_os("HOME").map(PathBuf::from).unwrap_or(path);
    }
    if let Some(rest) = path_str.strip_prefix("~/") {
        return env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(rest))
            .unwrap_or(path);
    }
    path
}

/// Walks up the directory tree from `start` looking for `squeezy.toml`.
///
/// The starting directory is canonicalized so that `..` segments do not
/// confuse the walk and so that running from inside a symlinked checkout
/// resolves to the real workspace root. Falling back to the original path
/// when canonicalization fails (for example on a path that does not yet
/// exist) keeps tests and bare invocations working.
pub fn find_project_settings_path(start: impl AsRef<Path>) -> Option<PathBuf> {
    let start = start.as_ref();
    let mut dir = if start.is_file() {
        start.parent()?.to_path_buf()
    } else {
        start.to_path_buf()
    };
    if let Ok(canonical) = fs::canonicalize(&dir) {
        dir = canonical;
    }
    loop {
        let candidate = dir.join(PROJECT_SETTINGS_FILE);
        if candidate.is_file() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

pub fn user_settings_template() -> &'static str {
    r#"# User-level Squeezy settings. Uncomment any key you want to override.
# Commented values are examples or defaults that apply when the key is absent.

[model]
# provider = "openai"          # openai | anthropic | google | azure_openai | bedrock | ollama
# profile = "balanced"         # cheap | balanced | strong
# model = "gpt-5.5"            # provider-specific model id; leave unset to use the provider default
# reasoning_effort = "medium"  # low | medium | high | xhigh; only sent to capable providers
# max_output_tokens = 64000    # optional output cap; unset means provider/model limit
# stream_idle_timeout_ms = 300000 # fail a stalled model stream after 5m idle
# store_responses = false      # only honored by openai/azure_openai
# selection_version = 1        # maintained by the startup provider/model selector

[agent]
# exploration_compiler = true  # graph-first planner for common navigation prompts

[session]
# mode = "build"              # build | plan
# resume_picker = "ask"       # ask | never
# log_dir = ".squeezy/sessions"
# log_retention_days = 30
# log_retention_archive_days = 30  # archived sessions deleted after this many days; 0 disables the archive sweep
# max_event_bytes = 65536
# max_session_bytes = 52428800

[context]
# compaction_enabled = true
# compaction_estimated_tokens = 60000
# compaction_min_items = 16
# compaction_recent_items = 6
# compaction_max_summary_bytes = 12000
# repo_doc_max_bytes = 16384    # cap on AGENTS.md content stitched into base instructions (0 disables)
# user_memory_max_bytes = 8192  # cap on ~/.squeezy/MEMORY.md content stitched into base instructions (0 disables)
# enabled_mid_turn = true                          # trigger compaction between LLM events when usage crosses the threshold
# model_context_window = 100000                    # token budget for the active model; mid-turn trigger is dormant until set
# threshold_percent = 80                           # fraction (0-100) of the window that arms the mid-turn trigger
# strategy = "extractive"                          # extractive | model_assisted | layered_fallback
# model_assisted_model = "gpt-5-nano"              # cheap model used when strategy != "extractive"
# model_assisted_max_output_tokens = 500
# model_assisted_timeout_secs = 30
# layered_fallback_extractive_threshold_tokens = 4000

[subagents]
# enabled = true
# explore_enabled = true
# explore_model = "gpt-5-nano" # optional cheap model override for the current provider
# max_concurrent = 4           # maximum parallel subagents per parent agent
# max_tool_calls_per_call = 24
# max_tool_bytes_read_per_call = 8388608
# max_search_files_per_call = 2000
# max_model_rounds = 1000
# max_summary_tokens = 64000

# [providers.openai]
# api_key_env = "OPENAI_API_KEY"
# base_url = "https://api.openai.com/v1"
# default_model = "gpt-5.5"
# stream_idle_timeout_ms = 300000

# [providers.anthropic]
# api_key_env = "ANTHROPIC_API_KEY"
# base_url = "https://api.anthropic.com/v1"
# default_model = "claude-sonnet-4-6"
# stream_idle_timeout_ms = 300000

[permissions]
# mode = "default"               # default | auto_review | full_access | custom
# default mode allows workspace read/edit/search plus local shell/git/compiler;
# web, MCP, destructive actions, and outside-workspace paths still ask.
# Top-level capability keys below are legacy aliases. Prefer [permissions.custom]
# when mode = "custom".
# read = "allow"
# search = "allow"
# edit = "allow"
# shell = "allow"
# ignored_search = "allow"
# web = "ask"
# mcp = "ask"
# git = "allow"
# compiler = "allow"
# destructive = "ask"
# shell_classifier = false       # narrow LLM fallback for ambiguous shell commands (extra LLM call)
#
# [permissions.custom]           # used when permissions.mode = "custom"
# read = "allow"
# search = "allow"
# edit = "allow"
# shell = "allow"
# ignored_search = "allow"
# network = "ask"
# mcp = "ask"
# git = "allow"
# compiler = "allow"
# destructive = "ask"

# [permissions.ai_reviewer]
# enabled = false
# model = "gpt-5-mini"          # optional reviewer model override
# allow_capabilities = ["read", "search", "network", "mcp"]
# auto_review mode forces enabled=true and this allow_capabilities set.
# policy_file = ""              # optional local approval policy override
# timeout_secs = 15
# max_transcript_tokens = 4000  # sliding-window budget: keeps recent turns whole + summary of older
#
# Rule targets use prefix-tagged strings so different scopes don't collide.
# Known prefixes:
#   path:<rel-path>      - edit/write rules
#   domain:<host>        - network rules
#   search:<provider>    - web search rules
#   workspace:*          - read/search rules limited to workspace files
#   ignored:*            - read/search rules that include git-ignored files
#   tool:<name>          - catch-all per-tool rule
#   <cmd-prefix>:*       - shell/git/compiler rules (e.g. "cargo test:*", "rm:*")
# Allow rules on the `destructive` capability are refused at load time; keep
# them at `ask` or `deny`.
#
# [[permissions.rules]]
# capability = "network"
# target = "domain:docs.rs"
# action = "allow"
# source = "user"
#
# [[permissions.rules]]
# capability = "shell"
# target = "cargo test:*"
# action = "allow"
# source = "user"
#
# [[permissions.rules]]
# capability = "network"
# target = "shell:curl:*"
# action = "ask"
# source = "project"

# [permissions.shell_sandbox]
# mode = "best_effort"              # best_effort | required | off | external
# default/auto_review set network = "allow_when_approved" unless explicitly configured.
# network = "allow_when_approved"   # deny_by_default | allow_when_approved
# audit = true
# kill_grace_ms = 250
# env_allowlist = ["PATH", "HOME", "USER", "LOGNAME", "SHELL", "TERM", "LANG", "TMPDIR", "TEMP", "TMP", "CARGO_HOME", "RUSTUP_HOME", "RUSTFLAGS", "RUST_BACKTRACE", "SSL_CERT_FILE", "SSL_CERT_DIR", "NIX_SSL_CERT_FILE", "LC_*"]
# read_roots = []                  # extra absolute directories shell may read
# write_roots = []                 # extra absolute directories shell may read/write
# protected_metadata_names = [".git", ".squeezy", ".agents"]
# sensitive_path_patterns = [".ssh/**", ".aws/**", ".config/gh/**", ".netrc", ".gnupg/**", ".kube/**", ".docker/config.json", ".cargo/credentials*", ".npmrc", ".pypirc", ".env*"]

[hardening]
# disable_core_dumps = true
# deny_debug_attach = true

[telemetry]
# enabled = true

[feedback]
# enabled = true
# feedback_endpoint = "https://squeezy-telemetry.esqueezy.workers.dev/v1/feedback"
# report_endpoint = "https://squeezy-telemetry.esqueezy.workers.dev/v1/report"
# max_feedback_bytes = 16384
# max_report_bytes = 2097152

# [redaction]
# custom_patterns = []

# [web]
# websearch_provider = "exa"          # "exa" or "parallel"
# exa_mcp_url = "https://mcp.exa.ai/mcp"
# exa_api_key_env = "EXA_API_KEY"
# parallel_mcp_url = "https://search.parallel.ai/mcp"
# parallel_api_key_env = "PARALLEL_API_KEY"

# [skills]
# user_dir = "~/.squeezy/skills"
# compat_user_dir = "~/.agents/skills"
# active_budget_chars = 4000          # legacy absolute cap; used only when active_budget_mode is unset
# active_body_cap_chars = 16000
# preamble_enabled = true
# preamble_budget_chars = 800         # legacy absolute cap; used only when preamble_budget_mode is unset
# active_budget_mode = { context_percent = 2.0 }   # default; scales with [context].model_context_window
# preamble_budget_mode = { context_percent = 2.0 } # alternative: active_budget_mode = { chars = 4000 }
# inline = false                      # default; emit only metadata for active skills and let the model call load_skill on demand
#
# [[skills.config]]
# name = "example-skill"
# enabled = false

# [tools]
# checkpoints_enabled = false
# lazy_schema_loading = true
# `update_task_state` and `load_tool_schema` are always-core control tools
# and do not need to appear in `core`. See `DEFAULT_CORE_TOOL_NAMES` in
# `squeezy_core` for the authoritative default list.
# core = ["glob", "grep", "read_file", "read_tool_output", "write_file", "apply_patch", "shell", "decl_search", "definition_search", "diff_context", "downstream_flow", "hierarchy", "plan_patch", "read_slice", "reference_search", "repo_map", "symbol_context", "upstream_flow"]
# discoverable = []

[tui]
# tick_rate_ms = 50
# status_verbosity = "compact"   # compact | verbose
# response_verbosity = "normal"  # concise | normal | verbose
# tool_output_verbosity = "compact" # compact | normal | verbose
# transcript_default = "compact" # compact | expanded
# alternate_screen = "auto"     # auto | always | never
# synchronized_output = "auto"  # auto | always | never (DEC 2026 atomic redraw)
# show_reasoning_usage = true
# persist_prompt_history = false  # mirror Up/Down prompt history to ~/.squeezy/prompt_history (XDG-compatible)

# [mcp.servers.docs]
# enabled = true
# transport = "stdio"       # stdio | http | sse
# command = "docs-mcp"
# args = []
# enabled_tools = ["lookup"]
# disabled_tools = []
#
# [mcp.servers.docs.permissions]
# default = "ask"
"#
}

pub fn project_settings_template() -> &'static str {
    r#"# Project-level Squeezy settings (committed alongside the project).
# Uncomment any key to override the built-in defaults shown after `=`.

[budgets]
# max_parallel_tools = 8
# max_tool_calls_per_turn = 64
# max_tool_bytes_read_per_turn = 20000000
# max_search_files_per_turn = 50000
# max_tool_result_bytes_per_round = 50000
# max_session_cost_usd_micros = 5000000
# cost_warn_percent = 85

[agent]
# exploration_compiler = true  # graph-first planner for common navigation prompts

[session]
# mode = "build"              # build | plan
# resume_picker = "ask"       # ask | never
# log_dir = ".squeezy/sessions"
# log_retention_days = 30
# log_retention_archive_days = 30  # archived sessions deleted after this many days; 0 disables the archive sweep
# max_event_bytes = 65536
# max_session_bytes = 52428800

[context]
# compaction_enabled = true
# compaction_estimated_tokens = 60000
# compaction_min_items = 16
# compaction_recent_items = 6
# compaction_max_summary_bytes = 12000
# repo_doc_max_bytes = 16384    # cap on AGENTS.md content stitched into base instructions (0 disables)
# user_memory_max_bytes = 8192  # cap on ~/.squeezy/MEMORY.md content stitched into base instructions (0 disables)
# enabled_mid_turn = true                          # trigger compaction between LLM events when usage crosses the threshold
# model_context_window = 100000                    # token budget for the active model; mid-turn trigger is dormant until set
# threshold_percent = 80                           # fraction (0-100) of the window that arms the mid-turn trigger
# strategy = "extractive"                          # extractive | model_assisted | layered_fallback
# model_assisted_model = "gpt-5-nano"              # cheap model used when strategy != "extractive"
# model_assisted_max_output_tokens = 500
# model_assisted_timeout_secs = 30
# layered_fallback_extractive_threshold_tokens = 4000

[subagents]
# enabled = true
# explore_enabled = true
# explore_model = "gpt-5-nano" # optional cheap model override for the current provider
# max_concurrent = 4           # maximum parallel subagents per parent agent
# max_tool_calls_per_call = 24
# max_tool_bytes_read_per_call = 8388608
# max_search_files_per_call = 2000
# max_model_rounds = 1000
# max_summary_tokens = 64000

# [redaction]
# Add project-specific Rust regex patterns for secrets Squeezy should redact
# everywhere they appear in tool output, model requests, and UI surfaces.
# custom_patterns = []

[permissions]
# mode = "default"               # default | auto_review | full_access | custom
# default mode allows workspace read/edit/search plus local shell/git/compiler;
# web, MCP, destructive actions, and outside-workspace paths still ask.
# Use [permissions.custom] for granular custom-mode defaults.
#
# [permissions.custom]
# read = "allow"
# search = "allow"
# edit = "allow"
# shell = "allow"
# ignored_search = "allow"
# network = "ask"
# mcp = "ask"
# git = "allow"
# compiler = "allow"
# destructive = "ask"
#
# [permissions.ai_reviewer]
# enabled = false
# allow_capabilities = ["read", "search", "network", "mcp"]
# auto_review mode forces enabled=true and this allow_capabilities set.
#
# [[permissions.rules]]
# capability = "compiler"
# target = "cargo test:*"
# action = "allow"
# source = "project"
#
# [permissions.shell_sandbox]
# default/auto_review set network = "allow_when_approved" unless explicitly configured.
# network = "allow_when_approved"
# read_roots = []                  # shared absolute read-only shell roots
# write_roots = []                 # shared absolute read/write shell roots
# protected_metadata_names = [".git", ".squeezy", ".agents"]

[hardening]
# disable_core_dumps = true
# deny_debug_attach = true

# `[graph]` controls workspace indexing. `[mcp.servers.*]` configures
# external MCP tools that are discovered before each agent turn.

# [graph]
# languages = ["rust", "python"]
# max_file_bytes = 1000000
# include_hidden = false
# require_indexing_signal = true
# include = ["vendor/allowed/**"]
# exclude = ["fixtures/generated/**"]
# include_classes = ["lockfile"]
# exclude_classes = ["generated"]

[cache]
# Relative paths are resolved against the project root (the directory
# containing this squeezy.toml).
# tool_outputs = ".squeezy/tool_outputs"

# [tools]
# checkpoints_enabled = false
# lazy_schema_loading = true
# `update_task_state` and `load_tool_schema` are always-core control tools
# and do not need to appear in `core`. See `DEFAULT_CORE_TOOL_NAMES` in
# `squeezy_core` for the authoritative default list.
# core = ["glob", "grep", "read_file", "read_tool_output", "write_file", "apply_patch", "shell", "decl_search", "definition_search", "diff_context", "downstream_flow", "hierarchy", "plan_patch", "read_slice", "reference_search", "repo_map", "symbol_context", "upstream_flow"]
# discoverable = []

[tui]
# tick_rate_ms = 50
# status_verbosity = "compact"   # compact | verbose
# response_verbosity = "normal"  # concise | normal | verbose
# tool_output_verbosity = "compact" # compact | normal | verbose
# transcript_default = "compact" # compact | expanded
# alternate_screen = "auto"     # auto | always | never
# synchronized_output = "auto"  # auto | always | never (DEC 2026 atomic redraw)
# show_reasoning_usage = true

# [mcp.servers.docs]
# enabled = true
# transport = "stdio"       # stdio | http | sse
# command = "docs-mcp"
# args = []
# enabled_tools = ["lookup"]
# disabled_tools = []
#
# [mcp.servers.docs.permissions]
# default = "ask"
"#
}

fn load_default_settings_sources() -> Result<(SettingsFile, Vec<String>, Vec<ConfigWarning>)> {
    let user_path = default_settings_path();
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let project_path = find_project_settings_path(&cwd);
    let repo_root = project_path
        .as_deref()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or(cwd);
    let repo_path = per_repo_settings_path(repo_root);
    load_settings_from_paths(
        Some(user_path.as_path()),
        project_path.as_deref(),
        Some(repo_path.as_path()),
    )
}

/// A single tier's settings file as both its parsed form and its raw
/// `toml_edit` document. The document is what the writer mutates so saves
/// preserve user-authored comments and formatting. The path is what the UI
/// shows when the user asks "where does this value live?"
#[derive(Debug, Clone)]
pub struct TierSource {
    pub path: PathBuf,
    pub doc: toml_edit::DocumentMut,
}

impl TierSource {
    /// Whether this tier explicitly sets the leaf at `path`. Walks the parent
    /// tables and reports `true` only when the final segment is present.
    pub fn contains_path(&self, path: &[&str]) -> bool {
        if path.is_empty() {
            return false;
        }
        let (leaf, parents) = path.split_last().unwrap();
        let mut current = self.doc.as_table();
        for seg in parents {
            match current.get(seg) {
                Some(toml_edit::Item::Table(t)) => current = t,
                _ => return false,
            }
        }
        current.contains_key(leaf)
    }
}

/// The three tier files plus the effective merged config. Used by the config
/// screen to compute per-leaf inheritance badges.
///
/// Field naming intentionally mirrors the internal load order
/// (`user → project → repo`). User-facing labels in the TUI map differently:
/// `project` = the committed `./squeezy.toml` ("Repo" in the screen) and
/// `repo` = the per-machine `~/.squeezy/projects/<hash>/settings.toml`
/// ("Local" in the screen).
#[derive(Debug, Clone)]
pub struct SeparatedSources {
    pub user: Option<TierSource>,
    pub project: Option<TierSource>,
    pub repo: Option<TierSource>,
    pub user_path_default: PathBuf,
    pub project_path_default: PathBuf,
    pub repo_path_default: PathBuf,
}

/// Loads each tier separately so the UI can compute inheritance per leaf.
/// Reads each file independently (no merging here).
pub fn load_separated_settings_sources() -> Result<SeparatedSources> {
    let user_path = default_settings_path();
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let project_path = find_project_settings_path(&cwd);
    let repo_root = project_path
        .as_deref()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cwd.clone());
    let repo_path = per_repo_settings_path(&repo_root);

    let user = load_tier_source(&user_path)?;
    let project = match project_path.as_ref() {
        Some(p) => load_tier_source(p)?,
        None => None,
    };
    let repo = load_tier_source(&repo_path)?;
    let project_path_default =
        project_path.unwrap_or_else(|| repo_root.join(PROJECT_SETTINGS_FILE));
    Ok(SeparatedSources {
        user,
        project,
        repo,
        user_path_default: user_path,
        project_path_default,
        repo_path_default: repo_path,
    })
}

fn load_tier_source(path: &Path) -> Result<Option<TierSource>> {
    match fs::read_to_string(path) {
        Ok(text) => {
            let doc = text.parse::<toml_edit::DocumentMut>().map_err(|err| {
                SqueezyError::Config(format!("toml_edit parse {}: {err}", path.display()))
            })?;
            Ok(Some(TierSource {
                path: path.to_path_buf(),
                doc,
            }))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

/// Resolves which tier owns a field, using env > repo > project > user > default
/// precedence. Env wins because env-var overrides are applied after the merged
/// settings in `from_settings_and_env_vars`; repo wins next because it's the
/// last tier merged in `load_settings_from_paths`.
pub fn resolve_field_source(
    sources: &SeparatedSources,
    field: &config_schema::FieldMeta,
) -> config_schema::FieldSource {
    if let Some(var_name) = field.env_override
        && std::env::var(var_name).is_ok()
    {
        return config_schema::FieldSource::Env;
    }
    let path = field.toml_path;
    if let Some(repo) = &sources.repo
        && repo.contains_path(path)
    {
        return config_schema::FieldSource::Repo;
    }
    if let Some(project) = &sources.project
        && project.contains_path(path)
    {
        return config_schema::FieldSource::Project;
    }
    if let Some(user) = &sources.user
        && user.contains_path(path)
    {
        return config_schema::FieldSource::User;
    }
    config_schema::FieldSource::Default
}

fn load_settings_from_paths(
    user_path: Option<&Path>,
    project_path: Option<&Path>,
    repo_path: Option<&Path>,
) -> Result<(SettingsFile, Vec<String>, Vec<ConfigWarning>)> {
    let mut settings = SettingsFile::default();
    let mut sources = vec!["defaults".to_string()];
    let mut warnings = Vec::new();
    for (path, label) in [
        (user_path, "user"),
        (project_path, "project"),
        (repo_path, "repo"),
    ] {
        let Some(path) = path else { continue };
        if !path.is_file() {
            continue;
        }
        let source = format!("{label}:{}", path.display());
        let parsed = SettingsFile::from_toml_str(&fs::read_to_string(path)?, &source)?;
        let unknowns = take_unknown_fields();
        settings.merge(parsed);
        sources.push(source.clone());
        warnings.extend(config_warnings_from_unknown_fields(&source, unknowns));
    }
    Ok((settings, sources, warnings))
}

fn provider_setting(
    providers: &BTreeMap<String, ProviderSettings>,
    provider: &str,
    key: &str,
) -> Option<String> {
    let settings = providers.get(provider)?;
    let value = match key {
        "api_key_env" => settings.api_key_env.as_ref(),
        "api_key" => settings.api_key.as_ref(),
        "base_url" => settings.base_url.as_ref(),
        "default_model" => settings.default_model.as_ref(),
        "api_version" => settings.api_version.as_ref(),
        "region" => settings.region.as_ref(),
        "preset" => settings.preset.as_ref(),
        "vertex_project" => settings.vertex_project.as_ref(),
        "vertex_location" => settings.vertex_location.as_ref(),
        "route_style" => settings.route_style.as_ref(),
        "cloudflare_account_id" => settings.cloudflare_account_id.as_ref(),
        "cloudflare_gateway_id" => settings.cloudflare_gateway_id.as_ref(),
        "script" => settings.script.as_ref(),
        "organization" => settings.organization.as_ref(),
        "project" => settings.project.as_ref(),
        "service_tier" => settings.service_tier.as_ref(),
        _ => None,
    }?;
    Some(value.clone())
}

fn provider_setting_headers(
    providers: &BTreeMap<String, ProviderSettings>,
    provider: &str,
) -> Option<BTreeMap<String, String>> {
    providers.get(provider)?.headers.clone()
}

/// Resolve a `[providers.<section>.headers]` table from the first section
/// in `sections` that defines a non-empty map. Lets providers that accept
/// multiple TOML section aliases (e.g. Azure's `azure_openai` / `azure`)
/// share one header table without duplicating the lookup at every call
/// site.
fn provider_setting_headers_any(
    providers: &BTreeMap<String, ProviderSettings>,
    sections: &[&str],
) -> Option<BTreeMap<String, String>> {
    for section in sections {
        if let Some(headers) = providers
            .get(*section)
            .and_then(|settings| settings.headers.as_ref())
            && !headers.is_empty()
        {
            return Some(headers.clone());
        }
    }
    None
}

/// Resolve a typed boolean setting (currently `use_entra_id`) from the
/// first section in `sections` that defines it. Returns the first
/// non-`None` value so the higher-precedence alias wins when both
/// sections set the flag.
fn provider_setting_bool_any(
    providers: &BTreeMap<String, ProviderSettings>,
    sections: &[&str],
    key: &str,
) -> Option<bool> {
    for section in sections {
        let Some(settings) = providers.get(*section) else {
            continue;
        };
        let value = match key {
            "use_entra_id" => settings.use_entra_id,
            _ => None,
        };
        if value.is_some() {
            return value;
        }
    }
    None
}

/// Resolve the Azure `deployment_name_map` from the first section in
/// `sections` that defines it. Empty (`{}`) is treated as "not set" so a
/// downstream config layer can still override; a missing field returns the
/// empty map so the runtime path defaults to passthrough.
fn provider_setting_deployment_name_map(
    providers: &BTreeMap<String, ProviderSettings>,
    sections: &[&str],
) -> BTreeMap<String, String> {
    for section in sections {
        if let Some(map) = providers
            .get(*section)
            .and_then(|settings| settings.deployment_name_map.as_ref())
            && !map.is_empty()
        {
            return map.clone();
        }
    }
    BTreeMap::new()
}

fn provider_setting_request_metadata(
    providers: &BTreeMap<String, ProviderSettings>,
    provider: &str,
) -> Option<BTreeMap<String, String>> {
    providers.get(provider)?.request_metadata.clone()
}

fn validate_provider_base_urls(provider: &ProviderConfig) -> Result<()> {
    match provider {
        ProviderConfig::OpenAi(cfg) => check_base_url_scheme(&cfg.base_url, "openai"),
        ProviderConfig::Anthropic(cfg) => check_base_url_scheme(&cfg.base_url, "anthropic"),
        ProviderConfig::Google(cfg) => check_base_url_scheme(&cfg.base_url, "google"),
        ProviderConfig::AzureOpenAi(cfg) => check_base_url_scheme(&cfg.base_url, "azure_openai"),
        ProviderConfig::Ollama(cfg) => check_base_url_scheme(&cfg.base_url, "ollama"),
        ProviderConfig::OpenAiCodex(cfg) => check_base_url_scheme(&cfg.base_url, "openai_codex"),
        ProviderConfig::OpenAiCompatible(cfg) => {
            check_base_url_scheme(&cfg.base_url, cfg.preset.as_str())
        }
        ProviderConfig::Bedrock(cfg) => match &cfg.base_url {
            Some(url) => check_base_url_scheme(url, "bedrock"),
            None => Ok(()),
        },
        // The faux provider runs entirely in-process; no base URL to
        // validate.
        ProviderConfig::Faux(_) => Ok(()),
    }
}

/// Refuses an HTTP `base_url` unless the host is a loopback identifier
/// (`localhost`, `127.0.0.0/8`, or `[::1]`). Anything else (LAN IPs, public
/// hostnames) must use HTTPS so a misconfigured config file cannot silently
/// exfiltrate API keys + prompt content to an attacker-controlled origin.
fn check_base_url_scheme(base_url: &str, section: &str) -> Result<()> {
    let trimmed = base_url.trim();
    let Some(rest) = trimmed.strip_prefix("http://") else {
        // Empty, https://, or any non-http scheme: the existing emptiness +
        // reachability checks elsewhere handle these. We only police http.
        return Ok(());
    };
    let host = rest
        .split('/')
        .next()
        .unwrap_or("")
        .rsplit('@')
        .next()
        .unwrap_or("");
    let host_only = host
        .strip_prefix('[')
        .and_then(|s| s.split_once(']'))
        .map(|(h, _)| h)
        .unwrap_or_else(|| host.split(':').next().unwrap_or(""));
    if is_loopback_host(host_only) {
        return Ok(());
    }
    Err(SqueezyError::Config(format!(
        "providers.{section}.base_url must use https:// for non-loopback hosts (got {trimmed:?}); \
         API keys and prompt content would otherwise transit in cleartext"
    )))
}

fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") || host == "::1" {
        return true;
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return ip.is_loopback();
    }
    false
}

fn build_openai_compatible_config(
    preset: OpenAiCompatiblePreset,
    providers: &BTreeMap<String, ProviderSettings>,
    get_var: &mut dyn FnMut(&str) -> Option<String>,
) -> Result<ProviderConfig> {
    let section = preset.as_str();
    let api_key_env = provider_setting(providers, section, "api_key_env")
        .or_else(|| {
            let candidate = preset.default_api_key_env();
            if candidate.is_empty() {
                None
            } else {
                Some(candidate.to_string())
            }
        })
        .ok_or_else(|| {
            SqueezyError::Config(format!(
                "providers.{section}.api_key_env is required for the {} preset",
                preset.display_name()
            ))
        })?;
    let base_url_override = get_var(&format!("{}_BASE_URL", section.to_ascii_uppercase()))
        .or_else(|| provider_setting(providers, section, "base_url"));
    let base_url = match (preset, base_url_override) {
        (_, Some(url)) => url,
        (OpenAiCompatiblePreset::Vertex, None) => {
            let project = get_var("VERTEX_PROJECT")
                .or_else(|| get_var("GOOGLE_CLOUD_PROJECT"))
                .or_else(|| provider_setting(providers, section, "vertex_project"))
                .ok_or_else(|| {
                    SqueezyError::Config(
                        "providers.vertex.vertex_project (or VERTEX_PROJECT / GOOGLE_CLOUD_PROJECT) is required for the Vertex AI preset"
                            .to_string(),
                    )
                })?;
            let location = get_var("VERTEX_LOCATION")
                .or_else(|| provider_setting(providers, section, "vertex_location"))
                .unwrap_or_else(|| DEFAULT_VERTEX_LOCATION.to_string());
            vertex_base_url(project.trim(), location.trim())
        }
        (_, None) => preset.default_base_url().to_string(),
    };
    if base_url.trim().is_empty() {
        return Err(SqueezyError::Config(format!(
            "providers.{section}.base_url is required for the {} preset",
            preset.display_name()
        )));
    }
    // Cloudflare presets carry `{account_id}` (and `{gateway_id}` for the
    // AI Gateway preset) placeholders in their default `base_url`
    // template; the LLM client substitutes them out of these fields
    // right before requests fire. Required values are validated up
    // front so misconfigurations surface at config-build time rather
    // than producing a confusing 404 against a literal `{account_id}`
    // URL. Aliases (`CLOUDFLARE_ACCOUNT_ID` env, `cloudflare_account_id`
    // TOML field) match the historical settings shape so existing
    // configs keep working.
    let (account_id, gateway_id) = match preset {
        OpenAiCompatiblePreset::CloudflareWorkersAi => {
            let account_id = get_var("CLOUDFLARE_ACCOUNT_ID")
                .or_else(|| provider_setting(providers, section, "cloudflare_account_id"))
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    SqueezyError::Config(
                        "providers.cloudflare_workers_ai.cloudflare_account_id (or CLOUDFLARE_ACCOUNT_ID) is required for the Cloudflare Workers AI preset"
                            .to_string(),
                    )
                })?;
            (Some(account_id), None)
        }
        OpenAiCompatiblePreset::CloudflareAiGateway => {
            let account_id = get_var("CLOUDFLARE_ACCOUNT_ID")
                .or_else(|| provider_setting(providers, section, "cloudflare_account_id"))
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    SqueezyError::Config(
                        "providers.cloudflare_ai_gateway.cloudflare_account_id (or CLOUDFLARE_ACCOUNT_ID) is required for the Cloudflare AI Gateway preset"
                            .to_string(),
                    )
                })?;
            let gateway_id = get_var("CLOUDFLARE_AI_GATEWAY_ID")
                .or_else(|| provider_setting(providers, section, "cloudflare_gateway_id"))
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| DEFAULT_CLOUDFLARE_AI_GATEWAY_ID.to_string());
            (Some(account_id), Some(gateway_id))
        }
        _ => (None, None),
    };
    let mut extra_headers = provider_setting_headers(providers, section).unwrap_or_default();
    // AI Gateway dual-auth: the upstream provider's API key flows in as the
    // standard `Authorization: Bearer …` (resolved by the provider), and an
    // optional gateway-level token flows in as `cf-aig-authorization`. Honor
    // `CF_AIG_TOKEN` as a convenience env so the gateway token does not have
    // to be pasted into a TOML `headers` table; user-supplied headers always
    // win to keep manual overrides possible.
    if preset == OpenAiCompatiblePreset::CloudflareAiGateway {
        let has_gateway_header = extra_headers
            .keys()
            .any(|key| key.eq_ignore_ascii_case("cf-aig-authorization"));
        if !has_gateway_header && let Some(token) = get_var("CF_AIG_TOKEN") {
            let trimmed = token.trim();
            if !trimmed.is_empty() {
                extra_headers.insert(
                    "cf-aig-authorization".to_string(),
                    format!("Bearer {trimmed}"),
                );
            }
        }
    }
    let transport = provider_transport_settings(providers, &[section]);
    let api_key = provider_setting(providers, section, "api_key");
    Ok(ProviderConfig::OpenAiCompatible(OpenAiCompatibleConfig {
        preset,
        api_key_env,
        api_key,
        base_url,
        extra_headers,
        transport,
        account_id,
        gateway_id,
    }))
}

fn provider_settings_keys(provider: &ProviderConfig) -> &'static [&'static str] {
    match provider {
        ProviderConfig::OpenAi(_) => &["openai"],
        ProviderConfig::Anthropic(_) => &["anthropic"],
        ProviderConfig::Google(_) => &["google"],
        ProviderConfig::AzureOpenAi(_) => &["azure_openai", "azure"],
        ProviderConfig::Bedrock(_) => &["bedrock"],
        ProviderConfig::Ollama(_) => &["ollama"],
        ProviderConfig::OpenAiCodex(_) => &["openai_codex"],
        ProviderConfig::OpenAiCompatible(config) => match config.preset {
            OpenAiCompatiblePreset::OpenRouter => &["openrouter"],
            OpenAiCompatiblePreset::Vercel => &["vercel"],
            OpenAiCompatiblePreset::PortKey => &["portkey"],
            OpenAiCompatiblePreset::Groq => &["groq"],
            OpenAiCompatiblePreset::XAi => &["xai"],
            OpenAiCompatiblePreset::DeepSeek => &["deepseek"],
            OpenAiCompatiblePreset::Vertex => &["vertex"],
            OpenAiCompatiblePreset::Mistral => &["mistral"],
            OpenAiCompatiblePreset::Together => &["together"],
            OpenAiCompatiblePreset::Fireworks => &["fireworks"],
            OpenAiCompatiblePreset::Cerebras => &["cerebras"],
            OpenAiCompatiblePreset::DeepInfra => &["deepinfra"],
            OpenAiCompatiblePreset::Baseten => &["baseten"],
            OpenAiCompatiblePreset::LMStudio => &["lmstudio"],
            OpenAiCompatiblePreset::VLlm => &["vllm"],
            OpenAiCompatiblePreset::LlamaCpp => &["llamacpp"],
            OpenAiCompatiblePreset::CloudflareWorkersAi => &["cloudflare_workers_ai"],
            OpenAiCompatiblePreset::CloudflareAiGateway => &["cloudflare_ai_gateway"],
            OpenAiCompatiblePreset::Custom => &["openai_compatible"],
        },
        ProviderConfig::Faux(_) => &["faux"],
    }
}

fn provider_u64_setting_any(
    providers: &BTreeMap<String, ProviderSettings>,
    provider_keys: &[&str],
    key: &str,
) -> Option<String> {
    provider_keys.iter().find_map(|provider| {
        let settings = providers.get(*provider)?;
        let value = match key {
            "stream_idle_timeout_ms" => settings.stream_idle_timeout_ms,
            _ => None,
        }?;
        Some(value.to_string())
    })
}

fn provider_transport_settings(
    providers: &BTreeMap<String, ProviderSettings>,
    names: &[&str],
) -> ProviderTransportConfig {
    let mut transport = ProviderTransportConfig::default();
    for name in names {
        let Some(settings) = providers.get(*name) else {
            continue;
        };
        if let Some(value) = settings.request_max_retries {
            transport.request_max_retries = value;
        }
        if let Some(value) = settings.stream_max_retries {
            transport.stream_max_retries = value;
        }
        if let Some(value) = settings.stream_idle_timeout_ms {
            transport.stream_idle_timeout_ms = value;
        }
    }
    transport
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpSettings {
    pub servers: BTreeMap<String, McpServerConfig>,
}

impl McpSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(table, &["servers"], source, path)?;
        let Some(servers) = optional_table(table, "servers", source)? else {
            return Ok(Self::default());
        };
        let mut result = BTreeMap::new();
        for (name, value) in servers {
            let server_table = value.as_table().ok_or_else(|| {
                type_error(source, &field(&field(path, "servers"), name), "table")
            })?;
            result.insert(
                name.clone(),
                McpServerConfig::from_table(
                    name,
                    server_table,
                    source,
                    &field(&field(path, "servers"), name),
                )?,
            );
        }
        Ok(Self { servers: result })
    }

    fn merge(&mut self, next: Self) {
        for (name, server) in next.servers {
            self.servers
                .entry(name)
                .and_modify(|existing| existing.merge(server.clone()))
                .or_insert(server);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpTransport {
    Stdio,
    Sse,
    Http,
}

impl McpTransport {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Sse => "sse",
            Self::Http => "http",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub enabled: bool,
    pub transport: McpTransport,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub url: Option<String>,
    pub timeout_ms: Option<u64>,
    /// Timeout applied to MCP tool discovery (`tools/list`, plus the implicit
    /// session bring-up on the first call). Falls back to `timeout_ms` when
    /// unset. Useful for servers that are slow to initialize but fast per
    /// call, or vice-versa.
    pub discovery_timeout_ms: Option<u64>,
    /// Timeout applied to MCP tool invocations and follow-on requests
    /// (`tools/call`, `resources/list`, `resources/read`). Falls back to
    /// `timeout_ms` when unset.
    pub tool_call_timeout_ms: Option<u64>,
    pub enabled_tools: Option<Vec<String>>,
    pub disabled_tools: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub permissions: McpPermissionConfig,
    /// Name of the environment variable holding the bearer token for HTTP/SSE
    /// transports. Resolved at session start; missing env vars are skipped.
    pub bearer_token_env_var: Option<String>,
    /// Static HTTP headers attached to every request on HTTP/SSE transports.
    pub http_headers: BTreeMap<String, String>,
    /// HTTP headers whose values are read from environment variables at session
    /// start. Map key is the header name, value is the env var name. On
    /// conflict with `http_headers`, the env-sourced value wins.
    pub env_http_headers: BTreeMap<String, String>,
}

impl McpServerConfig {
    fn from_table(
        name: &str,
        table: &toml::value::Table,
        source: &str,
        path: &str,
    ) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "enabled",
                "transport",
                "command",
                "args",
                "url",
                "timeout_ms",
                "discovery_timeout_ms",
                "tool_call_timeout_ms",
                "enabled_tools",
                "disabled_tools",
                "env",
                "permissions",
                "bearer_token_env_var",
                "http_headers",
                "env_http_headers",
            ],
            source,
            path,
        )?;
        let transport = mcp_transport_value(table, "transport", source, &field(path, "transport"))?
            .unwrap_or(McpTransport::Stdio);
        let env = string_map_value(table, "env", source, &field(path, "env"))?.unwrap_or_default();
        let http_headers =
            string_map_value(table, "http_headers", source, &field(path, "http_headers"))?
                .unwrap_or_default();
        let env_http_headers = string_map_value(
            table,
            "env_http_headers",
            source,
            &field(path, "env_http_headers"),
        )?
        .unwrap_or_default();
        let permissions = optional_table(table, "permissions", source)?
            .map(|table| {
                McpPermissionConfig::from_table(name, table, source, &field(path, "permissions"))
            })
            .transpose()?
            .unwrap_or_default();
        Ok(Self {
            enabled: bool_value(table, "enabled", source, &field(path, "enabled"))?.unwrap_or(true),
            transport,
            command: string_value(table, "command", source, &field(path, "command"))?,
            args: string_array_value(table, "args", source, &field(path, "args"))?
                .unwrap_or_default(),
            url: string_value(table, "url", source, &field(path, "url"))?,
            timeout_ms: u64_value(table, "timeout_ms", source, &field(path, "timeout_ms"))?,
            discovery_timeout_ms: u64_value(
                table,
                "discovery_timeout_ms",
                source,
                &field(path, "discovery_timeout_ms"),
            )?,
            tool_call_timeout_ms: u64_value(
                table,
                "tool_call_timeout_ms",
                source,
                &field(path, "tool_call_timeout_ms"),
            )?,
            enabled_tools: string_array_value(
                table,
                "enabled_tools",
                source,
                &field(path, "enabled_tools"),
            )?,
            disabled_tools: string_array_value(
                table,
                "disabled_tools",
                source,
                &field(path, "disabled_tools"),
            )?
            .unwrap_or_default(),
            env,
            permissions,
            bearer_token_env_var: string_value(
                table,
                "bearer_token_env_var",
                source,
                &field(path, "bearer_token_env_var"),
            )?,
            http_headers,
            env_http_headers,
        })
    }

    fn merge(&mut self, next: Self) {
        self.enabled = next.enabled;
        self.transport = next.transport;
        replace_if_some(&mut self.command, next.command);
        if !next.args.is_empty() {
            self.args = next.args;
        }
        replace_if_some(&mut self.url, next.url);
        replace_if_some(&mut self.timeout_ms, next.timeout_ms);
        replace_if_some(&mut self.discovery_timeout_ms, next.discovery_timeout_ms);
        replace_if_some(&mut self.tool_call_timeout_ms, next.tool_call_timeout_ms);
        replace_if_some(&mut self.enabled_tools, next.enabled_tools);
        if !next.disabled_tools.is_empty() {
            self.disabled_tools = next.disabled_tools;
        }
        if !next.env.is_empty() {
            self.env.extend(next.env);
        }
        self.permissions.merge(next.permissions);
        replace_if_some(&mut self.bearer_token_env_var, next.bearer_token_env_var);
        if !next.http_headers.is_empty() {
            self.http_headers.extend(next.http_headers);
        }
        if !next.env_http_headers.is_empty() {
            self.env_http_headers.extend(next.env_http_headers);
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpPermissionConfig {
    pub default: Option<PermissionMode>,
    #[serde(default, skip)]
    pub default_source: Option<PermissionRuleSource>,
    pub rules: Vec<PermissionRule>,
}

impl McpPermissionConfig {
    fn from_table(
        server_name: &str,
        table: &toml::value::Table,
        source: &str,
        path: &str,
    ) -> Result<Self> {
        reject_unknown_keys(table, &["default", "rules"], source, path)?;
        let default = permission_value(table, "default", source, &field(path, "default"))?;
        let default_source = default.map(|_| default_permission_rule_source(source));
        let rules = mcp_permission_rules_value(server_name, table, source, &field(path, "rules"))?;
        Ok(Self {
            default,
            default_source,
            rules,
        })
    }

    fn merge(&mut self, next: Self) {
        if next.default.is_some() {
            self.default = next.default;
            self.default_source = next.default_source;
        }
        self.rules.extend(next.rules);
    }
}

fn mcp_permission_rules(servers: &BTreeMap<String, McpServerConfig>) -> Vec<PermissionRule> {
    let mut rules = Vec::new();
    for (server_name, server) in servers {
        if let Some(default) = server.permissions.default {
            rules.push(PermissionRule::new(
                "mcp",
                format!("{server_name}/*"),
                default,
                server
                    .permissions
                    .default_source
                    .unwrap_or(PermissionRuleSource::Project),
                Some(format!("default MCP policy for server {server_name}")),
            ));
        }
        rules.extend(server.permissions.rules.clone());
    }
    rules
}

fn providers_settings(
    table: &toml::value::Table,
    source: &str,
) -> Result<Option<BTreeMap<String, ProviderSettings>>> {
    let Some(providers) = optional_table(table, "providers", source)? else {
        return Ok(None);
    };
    let mut result = BTreeMap::new();
    for (name, value) in providers {
        let provider_table = value
            .as_table()
            .ok_or_else(|| type_error(source, &field("providers", name), "table"))?;
        result.insert(
            name.clone(),
            ProviderSettings::from_table(provider_table, source, &field("providers", name))?,
        );
    }
    Ok(Some(result))
}

fn parse_profiles_map(
    table: &toml::value::Table,
    source: &str,
) -> Result<Option<BTreeMap<String, SettingsFile>>> {
    let Some(profiles) = optional_table(table, "profiles", source)? else {
        return Ok(None);
    };
    let mut result = BTreeMap::new();
    for (name, value) in profiles {
        let inner = value
            .as_table()
            .ok_or_else(|| type_error(source, &field("profiles", name), "table"))?;
        let mut parsed =
            SettingsFile::from_toml_table(inner, &format!("{source}.profiles.{name}"))?;
        // Profiles are leaves: an inner `[profiles.<name>.profiles]` or
        // `profile = …` selector inside a named profile would invite
        // surprising recursion, so drop them silently here. The TOML parser
        // has already rejected truly-unknown keys via reject_unknown_keys.
        parsed.profiles = None;
        parsed.profile = None;
        result.insert(name.clone(), parsed);
    }
    Ok(Some(result))
}

thread_local! {
    /// Dotted paths of unknown fields seen during the most recent
    /// `SettingsFile::from_toml_str` call. The file loader clears this
    /// before parsing and drains it afterwards so the app can warn the
    /// user without rejecting or rewriting the settings file.
    static UNKNOWN_FIELDS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

fn reject_unknown_keys(
    table: &toml::value::Table,
    allowed: &[&str],
    source: &str,
    path: &str,
) -> Result<()> {
    // Pre-1.0 the schema is still moving; silently ignoring renamed or
    // removed fields lets users keep their old settings.toml around while
    // we iterate. The loader records `UNKNOWN_FIELDS` so startup can show
    // a warning in the transcript.
    for key in table.keys() {
        if !allowed.iter().any(|allowed| key == allowed) {
            let field_path = field(path, key);
            tracing::warn!(source, field = %field_path, "ignoring unknown config field");
            UNKNOWN_FIELDS.with(|cell| cell.borrow_mut().push(field_path));
        }
    }
    Ok(())
}

fn take_unknown_fields() -> Vec<String> {
    UNKNOWN_FIELDS.with(|cell| std::mem::take(&mut *cell.borrow_mut()))
}

fn config_warnings_from_unknown_fields(source: &str, fields: Vec<String>) -> Vec<ConfigWarning> {
    fields
        .into_iter()
        .map(|field| ConfigWarning {
            source: source.to_string(),
            field,
        })
        .collect()
}

fn optional_table<'a>(
    table: &'a toml::value::Table,
    key: &str,
    source: &str,
) -> Result<Option<&'a toml::value::Table>> {
    match table.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_table()
            .map(Some)
            .ok_or_else(|| type_error(source, key, "table")),
    }
}

// SECURITY: `resolve_shell_escape` executes arbitrary shell commands as the
// invoking user, at config-load time. Any TOML string whose first byte is `!`
// (after any quoting the TOML parser already stripped) is passed verbatim to
// `/bin/sh -c` on Unix and `cmd.exe /C` on Windows. A malicious or hijacked
// `settings.toml` therefore has the same blast radius as a malicious shell
// rc-file: it runs before sandboxing, the agent loop, or the permission
// engine. This is intentional so users can wire in credential helpers like
// `!op read op://…` and `!gcloud auth …` without writing keys to disk, but it
// means settings files must be treated as code, not data. See
// `docs/internal/CONFIG_SHELL_ESCAPES.md` for the full guardrail note.
fn resolve_shell_escape(value: String, source: &str, path: &str) -> Result<String> {
    // Only the literal `!`-prefix form triggers execution; strings that merely
    // contain `!` anywhere else are passed through unchanged, e.g.
    // `prompt = "hello!"` or `regex = "[a-z]!\\d+"`.
    if !value.starts_with('!') {
        return Ok(value);
    }
    let command = &value[1..];
    if command.trim().is_empty() {
        return Err(SqueezyError::Config(format!(
            "{source}: {path}: shell escape `!` is empty; expected `!<command>`"
        )));
    }

    #[cfg(windows)]
    let mut cmd = {
        let mut c = process::Command::new("cmd");
        c.args(["/C", command]);
        c
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut c = process::Command::new("/bin/sh");
        c.args(["-c", command]);
        c
    };

    let output = cmd.output().map_err(|err| {
        SqueezyError::Config(format!(
            "{source}: {path}: failed to spawn shell escape `!{command}`: {err}"
        ))
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        let code_part = output
            .status
            .code()
            .map(|c| format!("exit code {c}"))
            .unwrap_or_else(|| "no exit code (signal)".to_string());
        let stderr_part = if stderr.is_empty() {
            String::new()
        } else {
            format!(": {stderr}")
        };
        return Err(SqueezyError::Config(format!(
            "{source}: {path}: shell escape `!{command}` failed with {code_part}{stderr_part}"
        )));
    }

    let stdout = String::from_utf8(output.stdout).map_err(|err| {
        SqueezyError::Config(format!(
            "{source}: {path}: shell escape `!{command}` produced non-UTF-8 stdout: {err}"
        ))
    })?;

    // Trim a single trailing newline (the common shell-echo case) plus any
    // additional trailing whitespace, so credential helpers that emit
    // `secret\n` still round-trip cleanly.
    Ok(stdout.trim_end().to_string())
}

fn string_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<String>> {
    match table.get(key) {
        None => Ok(None),
        Some(value) => {
            let raw = value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| type_error(source, path, "string"))?;
            resolve_shell_escape(raw, source, path).map(Some)
        }
    }
}

fn session_resume_picker_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<SessionResumePicker>> {
    let Some(value) = table.get(key) else {
        return Ok(None);
    };
    let value = value
        .as_str()
        .ok_or_else(|| type_error(source, path, "string"))?;
    parse_session_resume_picker_value(value, source, path).map(Some)
}

fn bool_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<bool>> {
    match table.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_bool()
            .map(Some)
            .ok_or_else(|| type_error(source, path, "boolean")),
    }
}

fn usize_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<usize>> {
    match table.get(key) {
        None => Ok(None),
        Some(value) => {
            let integer = positive_integer(value, source, path)?;
            usize::try_from(integer)
                .map(Some)
                .map_err(|_| SqueezyError::Config(format!("{source}: {path}: value is too large")))
        }
    }
}

fn u8_value(table: &toml::value::Table, key: &str, source: &str, path: &str) -> Result<Option<u8>> {
    match table.get(key) {
        None => Ok(None),
        Some(value) => {
            let integer = positive_integer(value, source, path)?;
            u8::try_from(integer)
                .map(Some)
                .map_err(|_| SqueezyError::Config(format!("{source}: {path}: value is too large")))
        }
    }
}

fn u32_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<u32>> {
    match table.get(key) {
        None => Ok(None),
        Some(value) => {
            let integer = positive_integer(value, source, path)?;
            u32::try_from(integer)
                .map(Some)
                .map_err(|_| SqueezyError::Config(format!("{source}: {path}: value is too large")))
        }
    }
}

fn u8_nonnegative_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<u8>> {
    match table.get(key) {
        None => Ok(None),
        Some(value) => {
            let integer = non_negative_integer(value, source, path)?;
            u8::try_from(integer)
                .map(Some)
                .map_err(|_| SqueezyError::Config(format!("{source}: {path}: value is too large")))
        }
    }
}

fn u64_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<u64>> {
    match table.get(key) {
        None => Ok(None),
        Some(value) => Ok(Some(positive_integer(value, source, path)?)),
    }
}

fn u64_nonnegative_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<u64>> {
    match table.get(key) {
        None => Ok(None),
        Some(value) => Ok(Some(non_negative_integer(value, source, path)?)),
    }
}

fn percent_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<u8>> {
    let Some(value) = u8_nonnegative_value(table, key, source, path)? else {
        return Ok(None);
    };
    if (1..=100).contains(&value) {
        Ok(Some(value))
    } else {
        Err(SqueezyError::Config(format!(
            "{source}: {path}: expected an integer from 1 to 100"
        )))
    }
}

fn positive_integer(value: &toml::Value, source: &str, path: &str) -> Result<u64> {
    let Some(integer) = value.as_integer() else {
        return Err(type_error(source, path, "positive integer"));
    };
    if integer <= 0 {
        return Err(SqueezyError::Config(format!(
            "{source}: {path}: expected a positive integer"
        )));
    }
    u64::try_from(integer)
        .map_err(|_| SqueezyError::Config(format!("{source}: {path}: expected a positive integer")))
}

fn non_negative_integer(value: &toml::Value, source: &str, path: &str) -> Result<u64> {
    let Some(integer) = value.as_integer() else {
        return Err(type_error(source, path, "non-negative integer"));
    };
    if integer < 0 {
        return Err(SqueezyError::Config(format!(
            "{source}: {path}: expected a non-negative integer"
        )));
    }
    u64::try_from(integer).map_err(|_| {
        SqueezyError::Config(format!("{source}: {path}: expected a non-negative integer"))
    })
}

fn path_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<PathBuf>> {
    Ok(string_value(table, key, source, path)?.map(PathBuf::from))
}

fn path_array_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Vec<PathBuf>> {
    Ok(string_array_value(table, key, source, path)?
        .map(|values| values.into_iter().map(PathBuf::from).collect())
        .unwrap_or_default())
}

fn skills_budget_mode_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<SkillsBudgetMode>> {
    let Some(value) = table.get(key) else {
        return Ok(None);
    };
    let entry = value
        .as_table()
        .ok_or_else(|| type_error(source, path, "table"))?;
    reject_unknown_keys(entry, &["chars", "context_percent"], source, path)?;
    let chars = usize_value(entry, "chars", source, &field(path, "chars"))?;
    let context_percent = match entry.get("context_percent") {
        None => None,
        Some(value) => {
            let raw = value
                .as_float()
                .or_else(|| value.as_integer().map(|integer| integer as f64))
                .ok_or_else(|| type_error(source, &field(path, "context_percent"), "number"))?;
            if !raw.is_finite() || raw < 0.0 {
                return Err(SqueezyError::Config(format!(
                    "{source}: {}: expected a non-negative number",
                    field(path, "context_percent")
                )));
            }
            Some(raw as f32)
        }
    };
    match (chars, context_percent) {
        (Some(_), Some(_)) => Err(SqueezyError::Config(format!(
            "{source}: {path}: set exactly one of `chars` or `context_percent`",
        ))),
        (None, None) => Err(SqueezyError::Config(format!(
            "{source}: {path}: set either `chars` or `context_percent`",
        ))),
        (Some(chars), None) => Ok(Some(SkillsBudgetMode::Chars { chars })),
        (None, Some(percent)) => Ok(Some(SkillsBudgetMode::ContextPercent { percent })),
    }
}

fn skill_config_entries_value(
    table: &toml::value::Table,
    source: &str,
    path: &str,
) -> Result<Vec<SkillConfigEntry>> {
    let Some(value) = table.get("config") else {
        return Ok(Vec::new());
    };
    let Some(values) = value.as_array() else {
        return Err(type_error(source, path, "array of tables"));
    };
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let entry_path = format!("{path}.{index}");
            let entry = value
                .as_table()
                .ok_or_else(|| type_error(source, &entry_path, "table"))?;
            reject_unknown_keys(entry, &["name", "path", "enabled"], source, &entry_path)?;
            let enabled = bool_value(entry, "enabled", source, &field(&entry_path, "enabled"))?
                .ok_or_else(|| {
                    SqueezyError::Config(format!(
                        "{source}: {}: missing field",
                        field(&entry_path, "enabled")
                    ))
                })?;
            Ok(SkillConfigEntry {
                name: string_value(entry, "name", source, &field(&entry_path, "name"))?,
                path: path_value(entry, "path", source, &field(&entry_path, "path"))?,
                enabled,
            })
        })
        .collect()
}

fn string_array_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<Vec<String>>> {
    let Some(value) = table.get(key) else {
        return Ok(None);
    };
    let Some(values) = value.as_array() else {
        return Err(type_error(source, path, "array of strings"));
    };
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let element_path = format!("{path}.{index}");
            let raw = value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| type_error(source, &element_path, "string"))?;
            resolve_shell_escape(raw, source, &element_path)
        })
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

fn string_map_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<BTreeMap<String, String>>> {
    let Some(value) = table.get(key) else {
        return Ok(None);
    };
    let Some(values) = value.as_table() else {
        return Err(type_error(source, path, "table of strings"));
    };
    values
        .iter()
        .map(|(key, value)| {
            let entry_path = field(path, key);
            let raw = value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| type_error(source, &entry_path, "string"))?;
            let resolved = resolve_shell_escape(raw, source, &entry_path)?;
            Ok((key.clone(), resolved))
        })
        .collect::<Result<BTreeMap<_, _>>>()
        .map(Some)
}

fn permission_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<PermissionMode>> {
    let Some(value) = string_value(table, key, source, path)? else {
        return Ok(None);
    };
    PermissionMode::parse(&value).map(Some).ok_or_else(|| {
        SqueezyError::Config(format!(
            "{source}: {path}: invalid permission mode {value:?}; expected allow, ask, or deny"
        ))
    })
}

fn permission_rules_value(
    table: &toml::value::Table,
    source: &str,
    path: &str,
) -> Result<Vec<PermissionRule>> {
    let Some(value) = table.get("rules") else {
        return Ok(Vec::new());
    };
    let rules = value
        .as_array()
        .ok_or_else(|| type_error(source, path, "array of tables"))?;
    rules
        .iter()
        .enumerate()
        .map(|value| {
            let rule_path = format!("{path}[{}]", value.0);
            let table = value
                .1
                .as_table()
                .ok_or_else(|| type_error(source, &rule_path, "table"))?;
            reject_unknown_keys(
                table,
                &["capability", "target", "action", "source", "reason", "silent"],
                source,
                &rule_path,
            )?;
            let capability = required_string_value(
                table,
                "capability",
                source,
                &field(&rule_path, "capability"),
            )?;
            if PermissionCapability::parse(&capability).is_none() && !capability.contains('*') {
                return Err(SqueezyError::Config(format!(
                    "{source}: {} invalid permission capability {capability:?}",
                    field(&rule_path, "capability")
                )));
            }
            let target =
                required_string_value(table, "target", source, &field(&rule_path, "target"))?;
            let action = permission_value(table, "action", source, &field(&rule_path, "action"))?
                .ok_or_else(|| {
                SqueezyError::Config(format!(
                    "{source}: {} missing required permission action",
                    field(&rule_path, "action")
                ))
            })?;
            if action == PermissionAction::Allow {
                if PermissionCapability::parse(&capability)
                    == Some(PermissionCapability::Destructive)
                {
                    return Err(SqueezyError::Config(format!(
                        "{source}: {rule_path}: refuse to load Allow rule on destructive capability; \
                         destructive actions must be approved per call or via a broader shell scope"
                    )));
                }
                if target_is_effectively_wildcard(&target) {
                    return Err(SqueezyError::Config(format!(
                        "{source}: {rule_path}: refuse to load Allow rule with bare wildcard target {target:?}; \
                         narrow the target to a specific path, host, or command prefix"
                    )));
                }
            }
            let source_value = string_value(table, "source", source, &field(&rule_path, "source"))?
                .as_deref()
                .and_then(PermissionRuleSource::parse)
                .unwrap_or_else(|| default_permission_rule_source(source));
            let reason = string_value(table, "reason", source, &field(&rule_path, "reason"))?;
            let silent =
                bool_value(table, "silent", source, &field(&rule_path, "silent"))?.unwrap_or(false);
            if silent && action != PermissionAction::Deny {
                return Err(SqueezyError::Config(format!(
                    "{source}: {rule_path}: silent = true is only valid on Deny rules; \
                     remove `silent` or set `action = \"deny\"`"
                )));
            }
            Ok(PermissionRule::new(capability, target, action, source_value, reason)
                .with_silent(silent))
        })
        .collect()
}

fn mcp_permission_rules_value(
    server_name: &str,
    table: &toml::value::Table,
    source: &str,
    path: &str,
) -> Result<Vec<PermissionRule>> {
    let Some(value) = table.get("rules") else {
        return Ok(Vec::new());
    };
    let rules = value
        .as_array()
        .ok_or_else(|| type_error(source, path, "array of tables"))?;
    rules
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let rule_path = format!("{path}[{index}]");
            let table = value
                .as_table()
                .ok_or_else(|| type_error(source, &rule_path, "table"))?;
            reject_unknown_keys(
                table,
                &["target", "action", "source", "reason", "silent"],
                source,
                &rule_path,
            )?;
            let target =
                required_string_value(table, "target", source, &field(&rule_path, "target"))?;
            let target = if target.starts_with(&format!("{server_name}/")) {
                target
            } else {
                format!("{server_name}/{target}")
            };
            let action = permission_value(table, "action", source, &field(&rule_path, "action"))?
                .ok_or_else(|| {
                    SqueezyError::Config(format!(
                        "{source}: {} missing required permission action",
                        field(&rule_path, "action")
                    ))
                })?;
            if action == PermissionAction::Allow && target_is_effectively_wildcard(&target) {
                return Err(SqueezyError::Config(format!(
                    "{source}: {rule_path}: refuse to load Allow rule with bare wildcard target {target:?}; \
                     narrow the target to a specific MCP server/tool"
                )));
            }
            let source_value = string_value(table, "source", source, &field(&rule_path, "source"))?
                .as_deref()
                .and_then(PermissionRuleSource::parse)
                .unwrap_or_else(|| default_permission_rule_source(source));
            let reason = string_value(table, "reason", source, &field(&rule_path, "reason"))?;
            let silent =
                bool_value(table, "silent", source, &field(&rule_path, "silent"))?.unwrap_or(false);
            if silent && action != PermissionAction::Deny {
                return Err(SqueezyError::Config(format!(
                    "{source}: {rule_path}: silent = true is only valid on Deny rules; \
                     remove `silent` or set `action = \"deny\"`"
                )));
            }
            Ok(PermissionRule::new("mcp", target, action, source_value, reason)
                .with_silent(silent))
        })
        .collect()
}

fn required_string_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<String> {
    string_value(table, key, source, path)?.ok_or_else(|| {
        SqueezyError::Config(format!("{source}: {path}: missing required string value"))
    })
}

fn default_permission_rule_source(source: &str) -> PermissionRuleSource {
    if source.starts_with("user:") {
        PermissionRuleSource::User
    } else {
        PermissionRuleSource::Project
    }
}

/// Minimal glob matcher for permission rule targets and capabilities.
///
/// Supports any number of `*` wildcards. Each `*` matches any (possibly empty)
/// run of characters; the prefix before the first `*` must anchor to the start
/// of `value` and the suffix after the last `*` must anchor to the end.
pub(crate) fn wildcard_match(value: &str, pattern: &str) -> bool {
    let value = value.trim();
    let pattern = pattern.trim();
    if pattern == value {
        return true;
    }
    if !pattern.contains('*') {
        return false;
    }
    let segments: Vec<&str> = pattern.split('*').collect();
    let first = segments[0];
    let last = segments[segments.len() - 1];
    if !value.starts_with(first) || !value.ends_with(last) {
        return false;
    }
    if first.len() + last.len() > value.len() {
        return false;
    }
    let mut cursor = first.len();
    let end = value.len() - last.len();
    for segment in &segments[1..segments.len().saturating_sub(1)] {
        if segment.is_empty() {
            continue;
        }
        let Some(idx) = value
            .get(cursor..end)
            .and_then(|window| window.find(segment))
        else {
            return false;
        };
        cursor += idx + segment.len();
    }
    true
}

fn status_verbosity_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<StatusVerbosity>> {
    let Some(value) = string_value(table, key, source, path)? else {
        return Ok(None);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "compact" => Ok(Some(StatusVerbosity::Compact)),
        "verbose" => Ok(Some(StatusVerbosity::Verbose)),
        _ => Err(SqueezyError::Config(format!(
            "{source}: {path}: invalid status verbosity {value:?}; expected compact or verbose"
        ))),
    }
}

fn response_verbosity_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<ResponseVerbosity>> {
    let Some(value) = string_value(table, key, source, path)? else {
        return Ok(None);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "concise" => Ok(Some(ResponseVerbosity::Concise)),
        "normal" => Ok(Some(ResponseVerbosity::Normal)),
        "verbose" => Ok(Some(ResponseVerbosity::Verbose)),
        _ => Err(SqueezyError::Config(format!(
            "{source}: {path}: invalid response verbosity {value:?}; expected concise, normal, or verbose"
        ))),
    }
}

fn tool_output_verbosity_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<ToolOutputVerbosity>> {
    let Some(value) = string_value(table, key, source, path)? else {
        return Ok(None);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "compact" => Ok(Some(ToolOutputVerbosity::Compact)),
        "normal" => Ok(Some(ToolOutputVerbosity::Normal)),
        "verbose" => Ok(Some(ToolOutputVerbosity::Verbose)),
        _ => Err(SqueezyError::Config(format!(
            "{source}: {path}: invalid tool output verbosity {value:?}; expected compact, normal, or verbose"
        ))),
    }
}

fn transcript_default_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<TranscriptDefault>> {
    let Some(value) = string_value(table, key, source, path)? else {
        return Ok(None);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "compact" => Ok(Some(TranscriptDefault::Compact)),
        "expanded" => Ok(Some(TranscriptDefault::Expanded)),
        _ => Err(SqueezyError::Config(format!(
            "{source}: {path}: invalid transcript default {value:?}; expected compact or expanded"
        ))),
    }
}

fn shell_diff_inline_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<ShellDiffInline>> {
    let Some(value) = string_value(table, key, source, path)? else {
        return Ok(None);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "full" => Ok(Some(ShellDiffInline::Full)),
        "folded" => Ok(Some(ShellDiffInline::Folded)),
        _ => Err(SqueezyError::Config(format!(
            "{source}: {path}: invalid shell diff inline {value:?}; expected full or folded"
        ))),
    }
}

fn tui_alternate_screen_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<TuiAlternateScreen>> {
    let Some(value) = string_value(table, key, source, path)? else {
        return Ok(None);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "auto" => Ok(Some(TuiAlternateScreen::Auto)),
        "never" => Ok(Some(TuiAlternateScreen::Never)),
        "always" => Ok(Some(TuiAlternateScreen::Always)),
        _ => Err(SqueezyError::Config(format!(
            "{source}: {path}: invalid TUI alternate screen {value:?}; expected auto, never, or always"
        ))),
    }
}

fn tui_synchronized_output_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<TuiSynchronizedOutput>> {
    let Some(value) = string_value(table, key, source, path)? else {
        return Ok(None);
    };
    TuiSynchronizedOutput::parse(&value).map(Some).ok_or_else(|| {
        SqueezyError::Config(format!(
            "{source}: {path}: invalid TUI synchronized output {value:?}; expected auto, always, or never"
        ))
    })
}

fn tui_theme_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<String>> {
    let Some(value) = string_value(table, key, source, path)? else {
        return Ok(None);
    };
    normalize_tui_theme_name(&value).map(Some).ok_or_else(|| {
        SqueezyError::Config(format!(
            "{source}: {path}: invalid TUI theme {value:?}; expected a theme slug"
        ))
    })
}

fn tui_themes_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<BTreeMap<String, TuiThemeSettings>>> {
    let Some(value) = table.get(key) else {
        return Ok(None);
    };
    let themes = value
        .as_table()
        .ok_or_else(|| type_error(source, path, "table"))?;
    let mut out = BTreeMap::new();
    for (raw_name, value) in themes {
        let name = normalize_tui_theme_name(raw_name).ok_or_else(|| {
            SqueezyError::Config(format!(
                "{source}: {path}.{raw_name}: invalid TUI theme name"
            ))
        })?;
        let theme_table = value
            .as_table()
            .ok_or_else(|| type_error(source, &field(path, raw_name), "table"))?;
        reject_unknown_keys(theme_table, &["colors"], source, &field(path, raw_name))?;
        let Some(colors_value) = theme_table.get("colors") else {
            out.insert(name, TuiThemeSettings::default());
            continue;
        };
        let colors_table = colors_value
            .as_table()
            .ok_or_else(|| type_error(source, &field(&field(path, raw_name), "colors"), "table"))?;
        let mut colors = BTreeMap::new();
        collect_tui_theme_colors(
            colors_table,
            None,
            source,
            &field(&field(path, raw_name), "colors"),
            &mut colors,
        )?;
        out.insert(name, TuiThemeSettings { colors });
    }
    Ok(Some(out))
}

fn collect_tui_theme_colors(
    table: &toml::value::Table,
    prefix: Option<&str>,
    source: &str,
    path: &str,
    out: &mut BTreeMap<String, TuiRgb>,
) -> Result<()> {
    for (key, color_value) in table {
        let token = match prefix {
            Some(prefix) => format!("{prefix}.{key}"),
            None => key.to_string(),
        };
        let token_path = field(path, key);
        if let Some(child) = color_value.as_table() {
            collect_tui_theme_colors(child, Some(&token), source, &token_path, out)?;
            continue;
        }
        if !is_tui_theme_color_token(&token) {
            return Err(SqueezyError::Config(format!(
                "{source}: {token_path}: unknown TUI theme color token {token:?}"
            )));
        }
        out.insert(token, rgb_array_value(color_value, source, &token_path)?);
    }
    Ok(())
}

fn rgb_array_value(value: &toml::Value, source: &str, path: &str) -> Result<TuiRgb> {
    let array = value
        .as_array()
        .ok_or_else(|| type_error(source, path, "RGB array"))?;
    if array.len() != 3 {
        return Err(SqueezyError::Config(format!(
            "{source}: {path}: expected RGB array with exactly 3 integers"
        )));
    }
    let mut rgb = [0u8; 3];
    for (idx, item) in array.iter().enumerate() {
        let Some(value) = item.as_integer() else {
            return Err(type_error(source, path, "RGB integer"));
        };
        let Ok(channel) = u8::try_from(value) else {
            return Err(SqueezyError::Config(format!(
                "{source}: {path}: RGB channel {value} is outside 0..=255"
            )));
        };
        rgb[idx] = channel;
    }
    Ok(rgb)
}

fn notification_method_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<NotificationMethod>> {
    let Some(value) = string_value(table, key, source, path)? else {
        return Ok(None);
    };
    NotificationMethod::parse(&value).map(Some).ok_or_else(|| {
        SqueezyError::Config(format!(
            "{source}: {path}: invalid notification method {value:?}; expected off, bel, osc9, or auto"
        ))
    })
}

fn reasoning_effort_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<ReasoningEffort>> {
    let Some(value) = string_value(table, key, source, path)? else {
        return Ok(None);
    };
    ReasoningEffort::parse(&value).ok_or_else(|| {
        SqueezyError::Config(format!(
            "{source}: {path}: invalid reasoning effort {value:?}; expected low, medium, high, or xhigh"
        ))
    }).map(Some)
}

fn tool_choice_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<String>> {
    let Some(value) = string_value(table, key, source, path)? else {
        return Ok(None);
    };
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "auto" | "required" | "none" => Ok(Some(normalized)),
        _ => Err(SqueezyError::Config(format!(
            "{source}: {path}: invalid tool_choice {value:?}; expected auto, required, or none"
        ))),
    }
}

fn mcp_transport_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<McpTransport>> {
    let Some(value) = string_value(table, key, source, path)? else {
        return Ok(None);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "stdio" | "local" => Ok(Some(McpTransport::Stdio)),
        "sse" => Ok(Some(McpTransport::Sse)),
        "http" | "remote" => Ok(Some(McpTransport::Http)),
        _ => Err(SqueezyError::Config(format!(
            "{source}: {path}: invalid MCP transport {value:?}; expected stdio, sse, or http"
        ))),
    }
}

fn type_error(source: &str, path: &str, expected: &str) -> SqueezyError {
    SqueezyError::Config(format!("{source}: {path}: expected {expected}"))
}

fn field(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_string()
    } else {
        format!("{prefix}.{key}")
    }
}

fn replace_if_some<T>(target: &mut Option<T>, next: Option<T>) {
    if next.is_some() {
        *target = next;
    }
}

fn replace_if_some_value<T: Copy>(target: &mut T, next: Option<T>) {
    if let Some(next) = next {
        *target = next;
    }
}

fn merge_string_lists(target: &mut Option<Vec<String>>, next: Option<Vec<String>>) {
    let Some(next) = next else {
        return;
    };
    match target {
        Some(existing) => {
            for value in next {
                if !existing.contains(&value) {
                    existing.push(value);
                }
            }
        }
        None => *target = Some(next),
    }
}

fn merge_option<T>(target: &mut Option<T>, next: Option<T>, merge: impl FnOnce(&mut T, T)) {
    let Some(next) = next else {
        return;
    };
    match target {
        Some(existing) => merge(existing, next),
        None => *target = Some(next),
    }
}

fn merge_provider_maps(
    target: &mut Option<BTreeMap<String, ProviderSettings>>,
    next: Option<BTreeMap<String, ProviderSettings>>,
) {
    let Some(next) = next else {
        return;
    };
    let target = target.get_or_insert_with(BTreeMap::new);
    for (name, provider) in next {
        target
            .entry(name)
            .and_modify(|existing| existing.merge(provider.clone()))
            .or_insert(provider);
    }
}

fn merge_tui_theme_maps(
    target: &mut BTreeMap<String, TuiThemeSettings>,
    next: BTreeMap<String, TuiThemeSettings>,
) {
    for (name, theme) in next {
        target
            .entry(name)
            .and_modify(|existing| existing.merge(theme.clone()))
            .or_insert(theme);
    }
}

fn merge_profiles_maps(
    target: &mut Option<BTreeMap<String, SettingsFile>>,
    next: Option<BTreeMap<String, SettingsFile>>,
) {
    let Some(next) = next else {
        return;
    };
    let target = target.get_or_insert_with(BTreeMap::new);
    for (name, profile) in next {
        target
            .entry(name)
            .and_modify(|existing| existing.merge(profile.clone()))
            .or_insert(profile);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TurnId(u64);

impl TurnId {
    /// Sentinel value for events that originate outside any specific turn
    /// (e.g. manual `/compact` invoked between turns). Subscribers that
    /// key off `turn_id` can match on this to recognise out-of-turn events
    /// without conflating them with a real turn-0.
    pub const INVALID: Self = Self(u64::MAX);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for TurnId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "turn-{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningKind {
    Summary,
    Text,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnthropicThinkingKind {
    Thinking,
    Redacted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnthropicThinkingBlock {
    pub kind: AnthropicThinkingKind,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case")]
pub enum ReasoningPayload {
    OpenAi {
        item_id: String,
        summary: Vec<String>,
        encrypted_content: Option<String>,
    },
    Anthropic {
        blocks: Vec<AnthropicThinkingBlock>,
    },
    Google {
        summary: Vec<String>,
        thought_signature: Option<String>,
    },
}

impl ReasoningPayload {
    pub fn provider_name(&self) -> &'static str {
        match self {
            ReasoningPayload::OpenAi { .. } => "openai",
            ReasoningPayload::Anthropic { .. } => "anthropic",
            ReasoningPayload::Google { .. } => "google",
        }
    }

    pub fn display_text(&self) -> String {
        match self {
            ReasoningPayload::OpenAi { summary, .. } => summary.join("\n\n"),
            ReasoningPayload::Anthropic { blocks } => blocks
                .iter()
                .map(|block| match block.kind {
                    AnthropicThinkingKind::Thinking => block.text.clone(),
                    AnthropicThinkingKind::Redacted => "[redacted reasoning]".to_string(),
                })
                .collect::<Vec<_>>()
                .join("\n\n"),
            ReasoningPayload::Google { summary, .. } => summary.join("\n\n"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningSnapshot {
    pub display_text: String,
    pub payload: ReasoningPayload,
}

impl ReasoningSnapshot {
    pub fn from_payload(payload: ReasoningPayload) -> Self {
        let display_text = payload.display_text();
        Self {
            display_text,
            payload,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptItem {
    pub role: Role,
    pub content: String,
    /// Boxed to keep `TranscriptItem` small: it sits inside `AgentEvent`
    /// variants and the unboxed snapshot is large enough to trip clippy's
    /// `large_enum_variant` threshold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Box<ReasoningSnapshot>>,
    /// Set on assistant messages whose turn was cancelled mid-stream. The
    /// `content` is the partial text that was streamed before the user
    /// pressed Esc; downstream renderers append a `(cancelled)` marker so
    /// the next turn (and `/diff`/`/undo`) can reference the cut-off work.
    #[serde(default, skip_serializing_if = "is_false")]
    pub cancelled: bool,
}

impl TranscriptItem {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            reasoning: None,
            cancelled: false,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            reasoning: None,
            cancelled: false,
        }
    }

    pub fn assistant_with_reasoning(
        content: impl Into<String>,
        reasoning: Option<ReasoningSnapshot>,
    ) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            reasoning: reasoning.map(Box::new),
            cancelled: false,
        }
    }

    /// Builds an assistant transcript item from a turn that was cancelled
    /// mid-stream. The `content` is whatever streamed before the cancel —
    /// possibly empty if the model never started producing text.
    pub fn assistant_cancelled(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            reasoning: None,
            cancelled: true,
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            reasoning: None,
            cancelled: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextAttachmentSource {
    Paste,
    File,
}

impl ContextAttachmentSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Paste => "paste",
            Self::File => "file",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextAttachmentKind {
    Log,
    StackTrace,
    Config,
    Text,
    UnsupportedBinary,
    /// Image-shaped payload (label extension or non-canonical magic
    /// bytes) that cannot be routed to a vision model — typically
    /// HEIC/BMP/TIFF or a `.png` label whose body is empty/garbled.
    /// The bytes are dropped and the attachment is marked
    /// [`ContextAttachmentStatus::Unsupported`].
    UnsupportedImage,
    /// Vision-routable image payload (PNG/JPEG/GIF/WEBP confirmed by
    /// magic bytes). The raw bytes are retained on
    /// [`ContextAttachment::image_data_base64`] so the agent can emit a
    /// matching `LlmInputItem::Image` per turn when the active model
    /// advertises vision capability.
    Image,
}

impl ContextAttachmentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Log => "log",
            Self::StackTrace => "stack_trace",
            Self::Config => "config",
            Self::Text => "text",
            Self::UnsupportedBinary => "unsupported_binary",
            Self::UnsupportedImage => "unsupported_image",
            Self::Image => "image",
        }
    }

    pub fn is_supported_text(self) -> bool {
        !matches!(
            self,
            Self::UnsupportedBinary | Self::UnsupportedImage | Self::Image
        )
    }

    /// `true` for the vision-routable [`Self::Image`] kind. Used at
    /// request-build time to fan a single attachment out into a
    /// `LlmInputItem::Image` so the bytes reach the provider verbatim
    /// instead of being squashed into the user text reference block.
    pub fn is_routable_image(self) -> bool {
        matches!(self, Self::Image)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextAttachmentStatus {
    Attached,
    Removed,
    Unsupported,
}

impl ContextAttachmentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Attached => "attached",
            Self::Removed => "removed",
            Self::Unsupported => "unsupported",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextAttachment {
    pub id: String,
    pub source: ContextAttachmentSource,
    pub kind: ContextAttachmentKind,
    pub status: ContextAttachmentStatus,
    pub label: String,
    pub path: Option<String>,
    pub original_sha256: String,
    pub redacted_sha256: Option<String>,
    pub original_bytes: usize,
    pub stored_bytes: usize,
    pub preview_bytes: usize,
    pub redactions: u64,
    pub preview: String,
    pub truncated: bool,
    /// MIME type for vision-routable image attachments
    /// (`image/{png,jpeg,gif,webp}`). `None` for text/log/binary kinds
    /// and for label-only `UnsupportedImage` payloads whose magic
    /// bytes did not match a canonical format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_media_type: Option<String>,
    /// Base64-encoded original image bytes for vision-routable image
    /// attachments. Stored alongside the JSON checkpoint so resume
    /// rehydrates an `LlmInputItem::Image` without re-reading the
    /// source. `None` outside the [`ContextAttachmentKind::Image`]
    /// kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_data_base64: Option<String>,
}

impl ContextAttachment {
    pub fn is_active(&self) -> bool {
        self.status == ContextAttachmentStatus::Attached
    }

    pub fn reference(&self) -> String {
        format!("attachment://{}", self.id)
    }
}

pub fn detect_context_attachment_kind(
    label: Option<&str>,
    bytes: &[u8],
    text: Option<&str>,
) -> ContextAttachmentKind {
    // Magic-byte-detected images route to the vision pipeline; the
    // squeezy-llm providers re-encode the bytes inline for the
    // provider's native wire format. Label-only "image-looking"
    // payloads (extension matched but magic bytes didn't) stay
    // `UnsupportedImage` so we don't ship garbled bytes to a model
    // that can't decode them.
    if detect_image_mime(bytes).is_some() {
        return ContextAttachmentKind::Image;
    }
    if looks_like_image(label, bytes) {
        return ContextAttachmentKind::UnsupportedImage;
    }
    let Some(text) = text else {
        return ContextAttachmentKind::UnsupportedBinary;
    };
    if looks_like_binary(bytes) {
        return ContextAttachmentKind::UnsupportedBinary;
    }
    if looks_like_stack_trace(text) {
        return ContextAttachmentKind::StackTrace;
    }
    if looks_like_log(text) {
        return ContextAttachmentKind::Log;
    }
    if looks_like_config(label, text) {
        return ContextAttachmentKind::Config;
    }
    ContextAttachmentKind::Text
}

/// Detect the canonical image MIME type from a byte prefix using
/// magic numbers. Supports PNG, JPEG, GIF (87a/89a), and WEBP
/// (RIFF / WEBP container) — the set of formats the upstream vision
/// providers (Anthropic / OpenAI / Google / Bedrock / Bedrock
/// Claude) all accept for inline image content blocks. Returns
/// `None` when the prefix does not match a known image format so the
/// caller can fall back to label-only detection or to the
/// `UnsupportedImage` path.
///
/// Mirrors [`squeezy_llm::infer_image_mime`] but lives in
/// `squeezy-core` so the attachment detection layer (which is
/// upstream of `squeezy-llm`) can short-circuit on magic bytes
/// without depending on the LLM crate.
pub fn detect_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

pub fn context_attachment_preview(text: &str, max_bytes: usize) -> (String, bool) {
    truncate_utf8(text, max_bytes)
}

pub fn context_attachment_storage_text(text: &str, max_bytes: usize) -> (String, bool) {
    truncate_utf8(text, max_bytes)
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextEstimate {
    pub bytes: usize,
    pub estimated_tokens: u64,
    pub items: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextCompactionTrigger {
    #[default]
    Auto,
    Manual,
}

impl ContextCompactionTrigger {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Manual => "manual",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPin {
    pub id: String,
    pub label: String,
    pub summary: String,
    pub source: String,
    pub created_unix_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCompactionRecord {
    pub generation: u64,
    pub trigger: ContextCompactionTrigger,
    pub compacted_at_ms: u64,
    pub before: ContextEstimate,
    pub after: ContextEstimate,
    pub dropped_items: usize,
    pub summary_bytes: usize,
    /// Stable id of the pre-compaction snapshot persisted in
    /// `compaction_checkpoints`. Populated when the agent had a `SqueezyStore`
    /// handle at compaction time; `None` for sessions without persistence or
    /// when the checkpoint write itself failed (non-fatal).
    #[serde(default)]
    pub replacement_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCompactionState {
    pub generation: u64,
    pub summary: Option<String>,
    pub pinned: Vec<ContextPin>,
    pub last: Option<ContextCompactionRecord>,
    #[serde(default)]
    pub history: Vec<ContextCompactionRecord>,
}

fn truncate_utf8(text: &str, max_bytes: usize) -> (String, bool) {
    if max_bytes == 0 {
        return (String::new(), !text.is_empty());
    }
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

fn looks_like_image(label: Option<&str>, bytes: &[u8]) -> bool {
    let lower_label = label.unwrap_or_default().to_ascii_lowercase();
    if matches!(
        lower_label.rsplit('.').next(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "heic")
    ) {
        return true;
    }
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(b"\xff\xd8\xff")
        || bytes.starts_with(b"GIF87a")
        || bytes.starts_with(b"GIF89a")
        || bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP")
}

fn looks_like_binary(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    let sample = &bytes[..bytes.len().min(4096)];
    if sample.contains(&0) {
        return true;
    }
    let control = sample
        .iter()
        .filter(|byte| {
            let byte = **byte;
            byte < 0x09 || (byte > 0x0d && byte < 0x20)
        })
        .count();
    control.saturating_mul(100) > sample.len().saturating_mul(10)
}

fn looks_like_stack_trace(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if lower.contains("traceback (most recent call last)")
        || lower.contains("stack backtrace:")
        || lower.contains("caused by:")
        || lower.contains("thread '")
        || lower.contains("panic")
        || lower.contains("exception in thread")
    {
        return true;
    }
    let stackish_lines = text
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("at ")
                || trimmed.starts_with("File \"")
                || trimmed.starts_with("from ")
                || trimmed.starts_with("error[E")
                || trimmed.starts_with("#")
        })
        .take(3)
        .count();
    stackish_lines >= 2
}

fn looks_like_log(text: &str) -> bool {
    let mut logish = 0usize;
    let mut lines = 0usize;
    for line in text.lines().take(20) {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        lines += 1;
        let lower = trimmed.to_ascii_lowercase();
        if lower.starts_with("error")
            || lower.starts_with("warn")
            || lower.starts_with("info")
            || lower.starts_with("debug")
            || lower.starts_with("trace")
            || lower.contains(" error ")
            || lower.contains(" warn ")
            || lower.contains(" failed")
            || starts_with_timestamp(trimmed)
        {
            logish += 1;
        }
    }
    lines >= 2 && logish >= 2
}

fn starts_with_timestamp(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() >= 10
        && bytes[0..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[7] == b'-'
        && bytes[8..10].iter().all(u8::is_ascii_digit)
    {
        return true;
    }
    bytes.len() >= 8
        && bytes[0..2].iter().all(u8::is_ascii_digit)
        && bytes[2] == b':'
        && bytes[3..5].iter().all(u8::is_ascii_digit)
        && bytes[5] == b':'
        && bytes[6..8].iter().all(u8::is_ascii_digit)
}

fn looks_like_config(label: Option<&str>, text: &str) -> bool {
    let lower_label = label.unwrap_or_default().to_ascii_lowercase();
    if matches!(
        lower_label.rsplit('.').next(),
        Some(
            "toml"
                | "yaml"
                | "yml"
                | "json"
                | "jsonl"
                | "env"
                | "ini"
                | "properties"
                | "conf"
                | "config"
        )
    ) {
        return true;
    }
    let mut configish = 0usize;
    let mut lines = 0usize;
    for line in text.lines().take(20) {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with("//") {
            continue;
        }
        lines += 1;
        if trimmed.starts_with('{')
            || trimmed.starts_with('[')
            || trimmed.contains('=')
            || trimmed.contains(": ")
        {
            configish += 1;
        }
    }
    lines > 0 && configish.saturating_mul(100) >= lines.saturating_mul(60)
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostSnapshot {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub reasoning_output_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub cache_write_input_tokens: Option<u64>,
    pub estimated_usd_micros: Option<u64>,
}

/// Per-`SubagentKind` rollup attached to [`TurnMetrics`] /
/// [`SessionMetrics`]. Each bucket carries the same counters as the
/// aggregate `subagent_*` fields but scoped to a single kind so the
/// operator can answer "is Explore burning the bulk of subagent tokens?"
/// and tune `explore_model` accordingly.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentKindMetrics {
    #[serde(default)]
    pub delegate: SubagentKindBucket,
    #[serde(default)]
    pub explore: SubagentKindBucket,
    #[serde(default)]
    pub plan: SubagentKindBucket,
    #[serde(default)]
    pub review: SubagentKindBucket,
}

impl SubagentKindMetrics {
    pub fn merge(&mut self, other: &SubagentKindMetrics) {
        self.delegate.merge(&other.delegate);
        self.explore.merge(&other.explore);
        self.plan.merge(&other.plan);
        self.review.merge(&other.review);
    }

    /// Mutable handle to the bucket for `kind`. Returns `None` for kinds
    /// outside the four audited buckets (delegate/explore/plan/review)
    /// so callers can ignore intra-agent helper kinds like `doc_help`
    /// without polluting the rollup.
    pub fn bucket_mut(&mut self, kind: &str) -> Option<&mut SubagentKindBucket> {
        match kind {
            "delegate" => Some(&mut self.delegate),
            "explore" => Some(&mut self.explore),
            "plan" => Some(&mut self.plan),
            "review" => Some(&mut self.review),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentKindBucket {
    pub calls: u64,
    pub failures: u64,
    pub tool_calls: u64,
    pub bytes_read: u64,
    pub provider: CostSnapshot,
}

impl SubagentKindBucket {
    pub fn merge(&mut self, other: &SubagentKindBucket) {
        self.calls += other.calls;
        self.failures += other.failures;
        self.tool_calls += other.tool_calls;
        self.bytes_read += other.bytes_read;
        merge_cost_snapshot(&mut self.provider, &other.provider);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMetrics {
    pub turns: u64,
    pub tool_calls: u64,
    pub tool_successes: u64,
    pub tool_errors: u64,
    pub tool_denials: u64,
    pub tool_cancellations: u64,
    pub files_scanned: u64,
    pub bytes_read: u64,
    pub matches_returned: u64,
    pub model_output_bytes: u64,
    pub receipt_stub_hits: u64,
    pub negative_receipt_hits: u64,
    pub spill_writes: u64,
    pub spill_reads: u64,
    pub budget_denials: u64,
    pub planner_turns: u64,
    pub planner_tool_calls: u64,
    pub planner_refusals: u64,
    pub subagent_calls: u64,
    pub subagent_failures: u64,
    pub subagent_tool_calls: u64,
    pub subagent_budget_denials: u64,
    pub subagent_files_scanned: u64,
    pub subagent_bytes_read: u64,
    pub subagent_model_output_bytes: u64,
    pub redactions: u64,
    pub provider: CostSnapshot,
    pub subagent_provider: CostSnapshot,
    #[serde(default)]
    pub subagent_by_kind: SubagentKindMetrics,
    /// Cumulative USD micros spent on cheap-tier routing-judge calls
    /// across the session.
    #[serde(default)]
    pub routing_judge_usd_micros: u64,
    /// Number of turns dispatched to the cheap tier instead of the
    /// parent model.
    #[serde(default)]
    pub routed_to_cheap_turns: u64,
    /// Number of cheap-routed turns that escalated back to the parent
    /// model mid-turn.
    #[serde(default)]
    pub escalated_to_parent_turns: u64,
    /// Cumulative estimated savings versus running the same turns on
    /// the parent model.
    #[serde(default)]
    pub routing_estimated_savings_usd_micros: u64,
}

impl SessionMetrics {
    pub fn merge_turn(&mut self, turn: &TurnMetrics) {
        self.turns += 1;
        self.tool_calls += turn.tool_calls;
        self.tool_successes += turn.tool_successes;
        self.tool_errors += turn.tool_errors;
        self.tool_denials += turn.tool_denials;
        self.tool_cancellations += turn.tool_cancellations;
        self.files_scanned += turn.files_scanned;
        self.bytes_read += turn.bytes_read;
        self.matches_returned += turn.matches_returned;
        self.model_output_bytes += turn.model_output_bytes;
        self.receipt_stub_hits += turn.receipt_stub_hits;
        self.negative_receipt_hits += turn.negative_receipt_hits;
        self.spill_writes += turn.spill_writes;
        self.spill_reads += turn.spill_reads;
        self.budget_denials += turn.budget_denials;
        self.planner_turns += turn.planner_turns;
        self.planner_tool_calls += turn.planner_tool_calls;
        self.planner_refusals += turn.planner_refusals;
        self.subagent_calls += turn.subagent_calls;
        self.subagent_failures += turn.subagent_failures;
        self.subagent_tool_calls += turn.subagent_tool_calls;
        self.subagent_budget_denials += turn.subagent_budget_denials;
        self.subagent_files_scanned += turn.subagent_files_scanned;
        self.subagent_bytes_read += turn.subagent_bytes_read;
        self.subagent_model_output_bytes += turn.subagent_model_output_bytes;
        self.redactions += turn.redactions;
        merge_cost_snapshot(&mut self.provider, &turn.provider);
        merge_cost_snapshot(&mut self.subagent_provider, &turn.subagent_provider);
        self.subagent_by_kind.merge(&turn.subagent_by_kind);
        self.routing_judge_usd_micros = self
            .routing_judge_usd_micros
            .saturating_add(turn.routing_judge_usd_micros);
        if turn.routed_to_cheap {
            self.routed_to_cheap_turns = self.routed_to_cheap_turns.saturating_add(1);
        }
        if turn.escalated_to_parent {
            self.escalated_to_parent_turns = self.escalated_to_parent_turns.saturating_add(1);
        }
        self.routing_estimated_savings_usd_micros = self
            .routing_estimated_savings_usd_micros
            .saturating_add(turn.routing_estimated_savings_usd_micros);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnMetrics {
    pub tool_calls: u64,
    pub tool_successes: u64,
    pub tool_errors: u64,
    pub tool_denials: u64,
    pub tool_cancellations: u64,
    pub files_scanned: u64,
    pub bytes_read: u64,
    pub matches_returned: u64,
    pub model_output_bytes: u64,
    pub receipt_stub_hits: u64,
    pub negative_receipt_hits: u64,
    pub spill_writes: u64,
    pub spill_reads: u64,
    pub budget_denials: u64,
    pub planner_turns: u64,
    pub planner_tool_calls: u64,
    pub planner_refusals: u64,
    pub subagent_calls: u64,
    pub subagent_failures: u64,
    pub subagent_tool_calls: u64,
    pub subagent_budget_denials: u64,
    pub subagent_files_scanned: u64,
    pub subagent_bytes_read: u64,
    pub subagent_model_output_bytes: u64,
    pub redactions: u64,
    pub provider: CostSnapshot,
    pub subagent_provider: CostSnapshot,
    #[serde(default)]
    pub subagent_by_kind: SubagentKindMetrics,
    /// USD micros spent on the borderline-classification call
    /// dispatched by the cheap-model fast path. Zero on turns where the
    /// heuristic fired (no judge call) or routing was disabled.
    #[serde(default)]
    pub routing_judge_usd_micros: u64,
    /// True when the turn's first LLM round dispatched on the cheap
    /// tier rather than the user's configured parent model.
    #[serde(default)]
    pub routed_to_cheap: bool,
    /// True when a cheap-routed turn ran into an escalation signal and
    /// switched back to the parent model mid-turn.
    #[serde(default)]
    pub escalated_to_parent: bool,
    /// Estimated savings versus running the same turn on the parent
    /// model — computed by re-pricing the provider-reported token
    /// counts at the parent's per-Mtok rate and subtracting the actual
    /// cheap-tier bill. Zero when the turn was not routed or when the
    /// model registry has no pricing for either side.
    #[serde(default)]
    pub routing_estimated_savings_usd_micros: u64,
}

impl TurnMetrics {
    pub fn record_provider(&mut self, cost: &CostSnapshot) {
        merge_cost_snapshot(&mut self.provider, cost);
    }

    /// Roll up the subagent's own [`TurnMetrics`] into the parent turn. The
    /// subagent's tool / I/O / provider counters are attributed to
    /// `subagent_*` so the parent's tool / I/O / provider numbers stay scoped
    /// to the parent agent's own work, while `redactions` is a session-wide
    /// safety counter and is merged into the parent total instead of dropped.
    pub fn merge_subagent_tool_metrics(&mut self, metrics: &TurnMetrics) {
        self.subagent_tool_calls += metrics.tool_calls;
        self.subagent_budget_denials += metrics.budget_denials;
        self.subagent_files_scanned += metrics.files_scanned;
        self.subagent_bytes_read += metrics.bytes_read;
        self.subagent_model_output_bytes += metrics.model_output_bytes;
        self.redactions += metrics.redactions;
        merge_cost_snapshot(&mut self.subagent_provider, &metrics.provider);
    }
}

fn merge_cost_snapshot(total: &mut CostSnapshot, next: &CostSnapshot) {
    total.input_tokens = add_optional_u64(total.input_tokens, next.input_tokens);
    total.output_tokens = add_optional_u64(total.output_tokens, next.output_tokens);
    total.cached_input_tokens =
        add_optional_u64(total.cached_input_tokens, next.cached_input_tokens);
    total.cache_write_input_tokens = add_optional_u64(
        total.cache_write_input_tokens,
        next.cache_write_input_tokens,
    );
    total.estimated_usd_micros =
        add_optional_u64(total.estimated_usd_micros, next.estimated_usd_micros);
}

fn add_optional_u64(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    [left, right].into_iter().flatten().reduce(|a, b| a + b)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContentHash(pub String);

impl ContentHash {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileId(pub String);

impl FileId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SymbolId(pub String);

impl SymbolId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourcePoint {
    pub line: u32,
    pub column: u32,
}

impl SourcePoint {
    pub const fn new(line: u32, column: u32) -> Self {
        Self { line, column }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourceSpan {
    pub start_byte: u32,
    pub end_byte: u32,
    pub start: SourcePoint,
    pub end: SourcePoint,
}

impl SourceSpan {
    pub const fn new(start_byte: u32, end_byte: u32, start: SourcePoint, end: SourcePoint) -> Self {
        Self {
            start_byte,
            end_byte,
            start,
            end,
        }
    }

    pub const fn contains_byte(self, byte: u32) -> bool {
        self.start_byte <= byte && byte <= self.end_byte
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LanguageKind {
    C,
    CSharp,
    Cpp,
    Dart,
    Go,
    Java,
    JavaScript,
    Jsx,
    Kotlin,
    Php,
    Python,
    Ruby,
    Rust,
    Scala,
    Swift,
    TypeScript,
    Tsx,
    Unsupported,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LanguageFamily {
    Rust,
    Python,
    Java,
    CSharp,
    Go,
    CFamily,
    JsTs,
    Ruby,
    Php,
    Kotlin,
    Swift,
    Scala,
    Dart,
}

impl LanguageFamily {
    pub const ALL: [Self; 13] = [
        Self::Rust,
        Self::Python,
        Self::Java,
        Self::CSharp,
        Self::Go,
        Self::CFamily,
        Self::JsTs,
        Self::Ruby,
        Self::Php,
        Self::Kotlin,
        Self::Swift,
        Self::Scala,
        Self::Dart,
    ];

    pub const fn all() -> &'static [Self] {
        &Self::ALL
    }

    pub const fn id(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::Java => "java",
            Self::CSharp => "csharp",
            Self::Go => "go",
            Self::CFamily => "c-family",
            Self::JsTs => "js-ts",
            Self::Ruby => "ruby",
            Self::Php => "php",
            Self::Kotlin => "kotlin",
            Self::Swift => "swift",
            Self::Scala => "scala",
            Self::Dart => "dart",
        }
    }

    /// Human-readable label suitable for prose (tool descriptions, docs).
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Rust => "Rust",
            Self::Python => "Python",
            Self::Java => "Java",
            Self::CSharp => "C#",
            Self::Go => "Go",
            Self::CFamily => "C/C++",
            Self::JsTs => "JavaScript/TypeScript",
            Self::Ruby => "Ruby",
            Self::Php => "PHP",
            Self::Kotlin => "Kotlin",
            Self::Swift => "Swift",
            Self::Scala => "Scala",
            Self::Dart => "Dart",
        }
    }

    pub const fn of(kind: LanguageKind) -> Option<Self> {
        match kind {
            LanguageKind::Rust => Some(Self::Rust),
            LanguageKind::Python => Some(Self::Python),
            LanguageKind::Java => Some(Self::Java),
            LanguageKind::CSharp => Some(Self::CSharp),
            LanguageKind::Go => Some(Self::Go),
            LanguageKind::C | LanguageKind::Cpp => Some(Self::CFamily),
            LanguageKind::JavaScript
            | LanguageKind::Jsx
            | LanguageKind::TypeScript
            | LanguageKind::Tsx => Some(Self::JsTs),
            LanguageKind::Ruby => Some(Self::Ruby),
            LanguageKind::Php => Some(Self::Php),
            LanguageKind::Kotlin => Some(Self::Kotlin),
            LanguageKind::Swift => Some(Self::Swift),
            LanguageKind::Scala => Some(Self::Scala),
            LanguageKind::Dart => Some(Self::Dart),
            LanguageKind::Unsupported | LanguageKind::Unknown => None,
        }
    }

    pub const fn kinds(self) -> &'static [LanguageKind] {
        match self {
            Self::Rust => &[LanguageKind::Rust],
            Self::Python => &[LanguageKind::Python],
            Self::Java => &[LanguageKind::Java],
            Self::CSharp => &[LanguageKind::CSharp],
            Self::Go => &[LanguageKind::Go],
            Self::CFamily => &[LanguageKind::C, LanguageKind::Cpp],
            Self::JsTs => &[
                LanguageKind::JavaScript,
                LanguageKind::Jsx,
                LanguageKind::TypeScript,
                LanguageKind::Tsx,
            ],
            Self::Ruby => &[LanguageKind::Ruby],
            Self::Php => &[LanguageKind::Php],
            Self::Kotlin => &[LanguageKind::Kotlin],
            Self::Swift => &[LanguageKind::Swift],
            Self::Scala => &[LanguageKind::Scala],
            Self::Dart => &[LanguageKind::Dart],
        }
    }

    pub const fn file_extensions(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &["rs"],
            Self::Python => &["py"],
            Self::Java => &["java"],
            Self::CSharp => &["cs", "csx"],
            Self::Go => &["go"],
            Self::CFamily => &["c", "h", "cc", "cpp", "cxx", "hh", "hpp", "hxx"],
            Self::JsTs => &["cjs", "cts", "js", "jsx", "mjs", "mts", "ts", "tsx"],
            Self::Ruby => &["rb"],
            Self::Php => &["php"],
            Self::Kotlin => &["kt", "kts"],
            Self::Swift => &["swift"],
            Self::Scala => &["scala", "sc"],
            Self::Dart => &["dart"],
        }
    }
}

impl LanguageKind {
    pub const fn family(self) -> Option<LanguageFamily> {
        LanguageFamily::of(self)
    }

    pub const fn from_extension(extension: &str) -> Self {
        match extension.as_bytes() {
            b"c" => Self::C,
            b"cc" | b"cpp" | b"cxx" | b"hh" | b"hpp" | b"hxx" => Self::Cpp,
            b"h" => Self::Cpp,
            b"cs" | b"csx" => Self::CSharp,
            b"cjs" | b"js" | b"mjs" => Self::JavaScript,
            b"cts" | b"mts" | b"ts" => Self::TypeScript,
            b"dart" => Self::Dart,
            b"go" => Self::Go,
            b"java" => Self::Java,
            b"jsx" => Self::Jsx,
            b"kt" | b"kts" => Self::Kotlin,
            b"php" => Self::Php,
            b"py" => Self::Python,
            b"rb" => Self::Ruby,
            b"rs" => Self::Rust,
            b"scala" | b"sc" => Self::Scala,
            b"swift" => Self::Swift,
            b"tsx" => Self::Tsx,
            _ => Self::Unsupported,
        }
    }

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::C => "C",
            Self::CSharp => "C#",
            Self::Cpp => "C++",
            Self::Dart => "Dart",
            Self::Go => "Go",
            Self::Java => "Java",
            Self::JavaScript => "JavaScript",
            Self::Jsx => "JSX",
            Self::Kotlin => "Kotlin",
            Self::Php => "PHP",
            Self::Python => "Python",
            Self::Ruby => "Ruby",
            Self::Rust => "Rust",
            Self::Scala => "Scala",
            Self::Swift => "Swift",
            Self::TypeScript => "TypeScript",
            Self::Tsx => "TSX",
            Self::Unsupported => "unsupported",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OracleId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SymbolKind {
    Class,
    Crate,
    File,
    Interface,
    Module,
    Struct,
    Enum,
    Union,
    Trait,
    Impl,
    Function,
    Method,
    Const,
    Static,
    TypeAlias,
    Field,
    Variant,
    Macro,
    Test,
    Unknown,
}

/// Stable kind tag attached to every `GraphEdge`. New variants append at the
/// end so serialized graphs stay forward-compatible: callers that ignore
/// unknown kinds keep working when a deployment lands a kind they haven't
/// seen yet. Inheritance-style kinds (`Extends`, `Implements`, `UsesTrait`)
/// participate in ancestor walks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EdgeKind {
    Contains,
    Imports,
    Reexports,
    Calls,
    References,
    Extends,
    PartialOf,
    Implements,
    InherentImpl,
    TraitImpl,
    TestOf,
    DefinesMacro,
    InvokesMacro,
    Conditional,
    /// PHP trait inclusion (`use TraitA;` inside a class/trait body). Modelled
    /// alongside `Extends`/`Implements` so ancestor-style queries can walk a
    /// class up through its included traits the same way they walk parents
    /// and implemented interfaces.
    UsesTrait,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Confidence {
    ExactSyntax,
    ImportResolved,
    Heuristic,
    CandidateSet,
    External,
    MacroOpaque,
    ConditionalUnknown,
    Unsupported,
    Stale,
    Partial,
}

impl Confidence {
    /// Every variant in declaration order. Use this for iteration when
    /// building distributions or summarising packets.
    pub const ALL: [Self; 10] = [
        Self::ExactSyntax,
        Self::ImportResolved,
        Self::Heuristic,
        Self::CandidateSet,
        Self::External,
        Self::MacroOpaque,
        Self::ConditionalUnknown,
        Self::Unsupported,
        Self::Stale,
        Self::Partial,
    ];

    /// Stable snake_case identifier suitable for JSON map keys.
    pub const fn id(self) -> &'static str {
        match self {
            Self::ExactSyntax => "exact_syntax",
            Self::ImportResolved => "import_resolved",
            Self::Heuristic => "heuristic",
            Self::CandidateSet => "candidate_set",
            Self::External => "external",
            Self::MacroOpaque => "macro_opaque",
            Self::ConditionalUnknown => "conditional_unknown",
            Self::Unsupported => "unsupported",
            Self::Stale => "stale",
            Self::Partial => "partial",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Freshness {
    Fresh,
    Stale,
    Partial,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    pub source: String,
    pub reason: String,
}

impl Provenance {
    pub fn new(source: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            reason: reason.into(),
        }
    }
}

#[derive(Debug, Error)]
pub enum SqueezyError {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("provider is not configured: {0}")]
    ProviderNotConfigured(String),
    #[error("provider request failed: {0}")]
    ProviderRequest(String),
    #[error("provider stream failed: {0}")]
    ProviderStream(String),
    #[error("terminal error: {0}")]
    Terminal(String),
    #[error("agent error: {0}")]
    Agent(String),
    #[error("workspace error: {0}")]
    Workspace(String),
    #[error("parse error: {0}")]
    Parse(String),
    #[error("graph error: {0}")]
    Graph(String),
    #[error("tool error: {0}")]
    Tool(String),
    #[error("permission denied: {0}")]
    Permission(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, SqueezyError>;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStateStatus {
    #[default]
    Running,
    Blocked,
    Completed,
    Cancelled,
    Failed,
}

impl TaskStateStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Blocked => "blocked",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStepStatus {
    #[default]
    Pending,
    Active,
    Completed,
    Blocked,
    Skipped,
}

impl TaskStepStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Completed => "completed",
            Self::Blocked => "blocked",
            Self::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskVerificationState {
    #[default]
    NotStarted,
    Running,
    Passed,
    Failed,
    Skipped,
}

impl TaskVerificationState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotStarted => "not_started",
            Self::Running => "running",
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskStateStep {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub status: TaskStepStatus,
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskStateSnapshot {
    #[serde(default)]
    pub task: String,
    #[serde(default)]
    pub status: TaskStateStatus,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub steps: Vec<TaskStateStep>,
    #[serde(default)]
    pub blocker: Option<String>,
    #[serde(default)]
    pub next_action: Option<String>,
    #[serde(default)]
    pub verification: TaskVerificationState,
    #[serde(default)]
    pub recent_changes: Vec<String>,
    #[serde(default)]
    pub replan_reason: Option<String>,
}

impl TaskStateSnapshot {
    pub fn starting(task: impl Into<String>) -> Self {
        Self {
            task: task.into(),
            status: TaskStateStatus::Running,
            steps: vec![TaskStateStep {
                title: "Start turn".to_string(),
                status: TaskStepStatus::Active,
                detail: Some("Preparing the first model request".to_string()),
            }],
            next_action: Some("wait for agent task-state update".to_string()),
            ..Self::default()
        }
        .normalized()
    }

    pub fn terminal_from(
        latest: Option<&Self>,
        fallback_task: impl Into<String>,
        status: TaskStateStatus,
        summary: Option<String>,
    ) -> Self {
        let mut snapshot = latest
            .cloned()
            .unwrap_or_else(|| Self::starting(fallback_task));
        snapshot.status = status;
        snapshot.summary = summary.or(snapshot.summary);
        if matches!(
            status,
            TaskStateStatus::Completed | TaskStateStatus::Cancelled | TaskStateStatus::Failed
        ) {
            snapshot.next_action = None;
        }
        snapshot.normalized()
    }

    pub fn active_step_title(&self) -> Option<&str> {
        self.steps
            .iter()
            .find(|step| {
                matches!(
                    step.status,
                    TaskStepStatus::Active | TaskStepStatus::Blocked
                )
            })
            .map(|step| step.title.as_str())
    }

    pub fn compact_summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.task.is_empty() {
            parts.push(self.task.clone());
        }
        parts.push(format!("status={}", self.status.as_str()));
        if let Some(step) = self.active_step_title()
            && !step.is_empty()
        {
            parts.push(format!("active={step}"));
        }
        if let Some(blocker) = &self.blocker {
            parts.push(format!("blocker={blocker}"));
        }
        if let Some(next_action) = &self.next_action {
            parts.push(format!("next={next_action}"));
        }
        parts.push(format!("verification={}", self.verification.as_str()));
        parts.join(" | ")
    }

    pub fn normalized(mut self) -> Self {
        self.task = normalize_task_text(self.task, 500);
        self.summary = normalize_optional_task_text(self.summary, 500);
        self.blocker = normalize_optional_task_text(self.blocker, 500);
        self.next_action = normalize_optional_task_text(self.next_action, 500);
        self.replan_reason = normalize_optional_task_text(self.replan_reason, 500);
        self.steps = self
            .steps
            .into_iter()
            .take(20)
            .map(|mut step| {
                step.title = normalize_task_text(step.title, 200);
                step.detail = normalize_optional_task_text(step.detail, 300);
                step
            })
            .collect();
        self.recent_changes = self
            .recent_changes
            .into_iter()
            .filter_map(|change| normalize_optional_task_text(Some(change), 300))
            .take(20)
            .collect();
        if self.blocker.is_some() && self.status == TaskStateStatus::Running {
            self.status = TaskStateStatus::Blocked;
        }
        self
    }
}

fn normalize_optional_task_text(value: Option<String>, limit: usize) -> Option<String> {
    value.and_then(|text| {
        let text = normalize_task_text(text, limit);
        (!text.is_empty()).then_some(text)
    })
}

fn normalize_task_text(text: String, limit: usize) -> String {
    let mut output = text.trim().replace('\n', " ");
    if output.chars().count() > limit {
        output = output.chars().take(limit.saturating_sub(3)).collect();
        output.push_str("...");
    }
    output
}

pub const DEFAULT_INSTRUCTIONS: &str = "You are Squeezy, a cost-aware coding agent. Keep responses concise, explicit, and grounded in workspace evidence. Prefer semantic graph tools such as repo_map, definition_search, symbol_context, reference_search, and read_slice before grep/read_file on supported code. Use websearch for web discovery and webfetch for retrieving a specific URL when web tools are available. Treat websearch and webfetch results as remote documentation evidence, cite source URLs from their citation metadata when relying on them, and keep remote docs distinct from local code or graph facts. Do not invent URLs. If a tool call is denied, do not retry the same call. Do not issue duplicate tool calls — if you need the same result you already have, refer to the earlier output instead of re-running the call. For simple existence checks (e.g. \"does function X exist?\") or simple definition questions (e.g. \"which file defines X?\"), a single grep or definition_search is usually enough; stop once graph evidence directly answers the user instead of adding repo_map, grep, or relationship tools. Before a batch of two or more related tool calls, emit a brief preamble (1–2 sentences, roughly 8–12 words) saying what you are about to do — for example: \"Looking up Error in src/lib.rs, then tracing its constructors.\" Logically group related tools under one preamble; if a turn covers two unrelated topics, emit one preamble per group. Skip the preamble for a single tool call or a trivial answer.";

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
