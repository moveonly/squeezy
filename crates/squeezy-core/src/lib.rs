use std::{
    borrow::Cow,
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    env, fmt, fs,
    path::{Path, PathBuf},
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
pub const DEFAULT_ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com/v1";
pub const DEFAULT_ANTHROPIC_MODEL: &str = "claude-opus-4-7";
pub const DEFAULT_GOOGLE_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
pub const DEFAULT_GOOGLE_MODEL: &str = "gemini-2.5-pro";
pub const DEFAULT_AZURE_OPENAI_BASE_URL: &str = "";
pub const DEFAULT_AZURE_OPENAI_API_VERSION: &str = "v1";
pub const DEFAULT_AZURE_OPENAI_MODEL: &str = DEFAULT_OPENAI_MODEL;
pub const DEFAULT_BEDROCK_REGION: &str = "us-east-1";
pub const DEFAULT_BEDROCK_MODEL: &str = "anthropic.claude-haiku-4-5-20251001-v1:0";
pub const DEFAULT_OLLAMA_BASE_URL: &str = "http://localhost:11434/api";
pub const DEFAULT_OLLAMA_MODEL: &str = "qwen3-coder";

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

/// Vertex AI's OpenAI-compatible chat completions endpoint lives behind a
/// regional URL that names the project. Returns the resolved base URL for a
/// `(project, location)` pair, ready for `/chat/completions` to be appended.
pub fn vertex_base_url(project: &str, location: &str) -> String {
    format!(
        "https://{location}-aiplatform.googleapis.com/v1/projects/{project}/locations/{location}/endpoints/openapi"
    )
}

pub const MODEL_SELECTION_VERSION: u32 = 1;
pub const DEFAULT_EXA_MCP_URL: &str = "https://mcp.exa.ai/mcp";
pub const DEFAULT_EXA_API_KEY_ENV: &str = "EXA_API_KEY";
pub const DEFAULT_MAX_OUTPUT_TOKENS: Option<u32> = None;
pub const DEFAULT_TOOL_SPILL_THRESHOLD_BYTES: usize = 25_000;
pub const DEFAULT_TOOL_PREVIEW_BYTES: usize = 2_000;
pub const DEFAULT_MAX_TOOL_RESULT_BYTES_PER_ROUND: usize = 50_000;
pub const DEFAULT_TOOL_OUTPUT_RETENTION_DAYS: u64 = 7;
pub const DEFAULT_MAX_PARALLEL_TOOLS: usize = 8;
// Per-turn aggregate budgets. None of the three peer agents (codex,
// CC, opencode) bound aggregate tool calls, bytes read, or
// files enumerated across a turn — every cap they have is per single
// tool invocation. These defaults are sized so they never bind in
// realistic use; users who want strict cost caps can set tighter
// values in `squeezy.toml`. Kept finite (rather than `u64::MAX`) so
// the inspect output remains TOML-roundtrippable.
pub const DEFAULT_MAX_TOOL_CALLS_PER_TURN: u64 = 10_000;
pub const DEFAULT_MAX_TOOL_BYTES_READ_PER_TURN: u64 = 1_000_000_000;
pub const DEFAULT_MAX_SEARCH_FILES_PER_TURN: u64 = 1_000_000;
pub const DEFAULT_STREAM_IDLE_TIMEOUT_MS: u64 = 300_000;
pub const DEFAULT_PROVIDER_REQUEST_MAX_RETRIES: u8 = 4;
pub const DEFAULT_PROVIDER_STREAM_MAX_RETRIES: u8 = 5;
pub const DEFAULT_PROVIDER_STREAM_IDLE_TIMEOUT_MS: u64 = 300_000;
pub const DEFAULT_COST_WARN_PERCENT: u8 = 85;
// Per-subagent-invocation budgets. No peer agent has any equivalent —
// codex, CC, and opencode bound work per single tool call,
// not per subagent run. Sized so they never bind in realistic use;
// the subagent's natural exit is the model emitting a final answer
// with no tool calls.
pub const DEFAULT_SUBAGENT_MAX_TOOL_CALLS_PER_CALL: u64 = 10_000;
pub const DEFAULT_SUBAGENT_MAX_TOOL_BYTES_READ_PER_CALL: u64 = 100_000_000;
pub const DEFAULT_SUBAGENT_MAX_SEARCH_FILES_PER_CALL: u64 = 50_000;
// Emergency belt on subagent model rounds — matches CC's
// `forkSubagent.maxTurns = 200`, the only concrete cap any peer
// sets on a full subagent. Above this the cost broker, cancellation
// token, and per-tool-call truncations should already have caught
// any runaway.
pub const DEFAULT_SUBAGENT_MAX_MODEL_ROUNDS: usize = 200;
// Generous default that absorbs reasoning-model overhead. The previous 1_200
// silently broke any subagent run under a reasoning model with effort >= medium:
// reasoning alone burns several thousand tokens before the model can emit a
// single character of summary, which the OpenAI Responses API surfaces as
// `response.incomplete: max_output_tokens` (a hard error in our SSE parser).
// 16K leaves room for reasoning + a real summary across every model we ship a
// preset for.
pub const DEFAULT_SUBAGENT_MAX_SUMMARY_TOKENS: u32 = 32_000;
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
pub const DEFAULT_SESSION_MAX_EVENT_BYTES: usize = 65_536;
pub const DEFAULT_SESSION_MAX_SESSION_BYTES: usize = 52_428_800;
pub const DEFAULT_CONTEXT_ATTACHMENT_MAX_BYTES: usize = 1_048_576;
// Absolute fallback for the per-turn compaction trigger when
// `model_context_window` is not set in `squeezy.toml`. Modern models
// run with 128k+ context windows; the percent-of-context path (which
// peer agents use, ~90%) is the right shape and should take over once
// the window is auto-derived from `model_info_for`. This fallback is
// only the safety net for the unknown-model case.
pub const DEFAULT_CONTEXT_COMPACTION_ESTIMATED_TOKENS: u64 = 60_000;
pub const DEFAULT_CONTEXT_COMPACTION_MIN_ITEMS: usize = 16;
pub const DEFAULT_CONTEXT_COMPACTION_RECENT_ITEMS: usize = 6;
pub const DEFAULT_CONTEXT_COMPACTION_MAX_SUMMARY_BYTES: usize = 12_000;
pub const DEFAULT_CONTEXT_REPO_DOC_MAX_BYTES: usize = 16_384;
pub const DEFAULT_CONTEXT_USER_MEMORY_MAX_BYTES: usize = 8_192;
/// Trigger mid-turn compaction once the provider-reported total token usage
/// reaches this fraction of `model_context_window` (out of 100).
pub const DEFAULT_CONTEXT_COMPACTION_THRESHOLD_PERCENT: u8 = 80;
/// Max output tokens to request when the model-assisted compaction strategy
/// is active.
pub const DEFAULT_CONTEXT_COMPACTION_MODEL_ASSISTED_MAX_OUTPUT_TOKENS: u32 = 500;
/// Timeout for a single model-assisted compaction round-trip. On expiry the
/// pipeline falls back to the extractive summary.
pub const DEFAULT_CONTEXT_COMPACTION_MODEL_ASSISTED_TIMEOUT_SECS: u64 = 30;
/// When strategy = LayeredFallback, model-assist only kicks in once the
/// dropped slice exceeds this many tokens; smaller slices stay extractive.
pub const DEFAULT_CONTEXT_COMPACTION_LAYERED_FALLBACK_EXTRACTIVE_THRESHOLD_TOKENS: u32 = 4_000;
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppConfig {
    pub provider: ProviderConfig,
    pub model: String,
    pub profile: ModelProfile,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub instructions: String,
    pub max_output_tokens: Option<u32>,
    pub stream_idle_timeout: Duration,
    pub tick_rate: Duration,
    pub workspace_root: PathBuf,
    pub permissions: PermissionPolicy,
    pub session_mode: SessionMode,
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
    pub max_tool_calls_per_turn: u64,
    pub max_tool_bytes_read_per_turn: u64,
    pub max_search_files_per_turn: u64,
    pub max_session_cost_usd_micros: Option<u64>,
    pub cost_warn_percent: u8,
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
}

impl AppConfig {
    pub fn from_env() -> Self {
        Self::from_env_vars(None, |name| env::var(name).ok())
    }

    pub fn from_env_and_settings() -> Result<Self> {
        Self::from_default_paths_and_env_with_provider_value(None)
    }

    pub fn from_env_and_settings_with_provider(provider: &str) -> Result<Self> {
        Self::from_default_paths_and_env_with_provider_value(Some(provider))
    }

    pub fn from_settings_path_and_env(path: PathBuf) -> Result<Self> {
        let (settings, sources) = SettingsFile::load_optional_source(&path, "settings")?;
        Self::try_from_settings_and_env_vars_with_sources(settings, sources, None, |name| {
            env::var(name).ok()
        })
    }

    pub fn from_settings_path_and_env_with_provider(path: PathBuf, provider: &str) -> Result<Self> {
        let (settings, sources) = SettingsFile::load_optional_source(&path, "settings")?;
        Self::try_from_settings_and_env_vars_with_sources(
            settings,
            sources,
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
        mut sources: Vec<String>,
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
                base_url: get_var("ANTHROPIC_BASE_URL")
                    .or_else(|| provider_setting(&providers, "anthropic", "base_url"))
                    .unwrap_or_else(|| DEFAULT_ANTHROPIC_BASE_URL.to_string()),
                transport: provider_transport_settings(&providers, &["anthropic"]),
            }),
            "google" | "gemini" => ProviderConfig::Google(GoogleConfig {
                api_key_env: get_var("GOOGLE_API_KEY_ENV")
                    .or_else(|| provider_setting(&providers, "google", "api_key_env"))
                    .unwrap_or_else(|| "SQUEEZY_GOOGLE_KEY".to_string()),
                base_url: get_var("GOOGLE_BASE_URL")
                    .or_else(|| provider_setting(&providers, "google", "base_url"))
                    .unwrap_or_else(|| DEFAULT_GOOGLE_BASE_URL.to_string()),
                transport: provider_transport_settings(&providers, &["google"]),
            }),
            "azure" | "azure-openai" | "azure_openai" => {
                ProviderConfig::AzureOpenAi(AzureOpenAiConfig {
                    api_key_env: get_var("AZURE_OPENAI_API_KEY_ENV")
                        .or_else(|| provider_setting(&providers, "azure_openai", "api_key_env"))
                        .or_else(|| provider_setting(&providers, "azure", "api_key_env"))
                        .unwrap_or_else(|| "SQUEEZY_AZURE_OPENAI_KEY".to_string()),
                    base_url: get_var("AZURE_OPENAI_BASE_URL")
                        .or_else(|| provider_setting(&providers, "azure_openai", "base_url"))
                        .or_else(|| provider_setting(&providers, "azure", "base_url"))
                        .unwrap_or_else(|| DEFAULT_AZURE_OPENAI_BASE_URL.to_string()),
                    api_version: get_var("AZURE_OPENAI_API_VERSION")
                        .or_else(|| provider_setting(&providers, "azure_openai", "api_version"))
                        .or_else(|| provider_setting(&providers, "azure", "api_version"))
                        .unwrap_or_else(|| DEFAULT_AZURE_OPENAI_API_VERSION.to_string()),
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
                    transport: provider_transport_settings(&providers, &["bedrock"]),
                })
            }
            "ollama" | "local" => ProviderConfig::Ollama(OllamaConfig {
                base_url: get_var("OLLAMA_BASE_URL")
                    .or_else(|| provider_setting(&providers, "ollama", "base_url"))
                    .unwrap_or_else(|| DEFAULT_OLLAMA_BASE_URL.to_string()),
                transport: provider_transport_settings(&providers, &["ollama"]),
            }),
            "openai" => ProviderConfig::OpenAi(OpenAiConfig {
                api_key_env: get_var("OPENAI_API_KEY_ENV")
                    .or_else(|| provider_setting(&providers, "openai", "api_key_env"))
                    .unwrap_or_else(|| "SQUEEZY_OPENAI_KEY".to_string()),
                base_url: get_var("OPENAI_BASE_URL")
                    .or_else(|| provider_setting(&providers, "openai", "base_url"))
                    .unwrap_or_else(|| DEFAULT_OPENAI_BASE_URL.to_string()),
                transport: provider_transport_settings(&providers, &["openai"]),
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
            ProviderConfig::OpenAiCompatible(config) => {
                provider_setting(&providers, config.preset.as_str(), "default_model")
                    .unwrap_or_else(|| config.preset.default_model().to_string())
            }
        };
        let profile = get_var("SQUEEZY_PROFILE")
            .or(model_settings.profile)
            .or(settings.profile)
            .as_deref()
            .and_then(ModelProfile::parse)
            .unwrap_or_default();
        let model = get_var("SQUEEZY_MODEL")
            .or(model_settings.model)
            .or(settings.model)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(default_model);
        let reasoning_effort = model_settings.reasoning_effort;
        let max_output_tokens = get_var("SQUEEZY_MAX_OUTPUT_TOKENS")
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|value| *value > 0)
            .or(model_settings.max_output_tokens)
            .or(DEFAULT_MAX_OUTPUT_TOKENS);
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
        let session_settings = settings.session.unwrap_or_default();
        let session_mode = parse_session_mode(
            get_var("SQUEEZY_SESSION_MODE"),
            session_settings.mode.unwrap_or_default(),
        );
        let skills = SkillsConfig::from_settings_and_env_vars(
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
        let session_logs = SessionLogConfig::from_settings(&session_settings);
        let context_compaction = ContextCompactionConfig::from_settings_and_env(
            settings.context.unwrap_or_default(),
            &mut get_var,
        );
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
            profile,
            reasoning_effort,
            instructions: DEFAULT_INSTRUCTIONS.to_string(),
            max_output_tokens,
            stream_idle_timeout: Duration::from_millis(stream_idle_timeout_ms),
            tick_rate: Duration::from_millis(tui.tick_rate_ms),
            workspace_root,
            permissions,
            session_mode,
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
            max_tool_calls_per_turn,
            max_tool_bytes_read_per_turn,
            max_search_files_per_turn,
            max_session_cost_usd_micros,
            cost_warn_percent,
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
        })
    }

    fn from_default_paths_and_env_with_provider_value(provider: Option<&str>) -> Result<Self> {
        let (settings, sources) = load_default_settings_sources()?;
        Self::try_from_settings_and_env_vars_with_sources(settings, sources, provider, |name| {
            env::var(name).ok()
        })
    }

    fn built_in_defaults() -> Self {
        Self::try_from_settings_and_env_vars(SettingsFile::default(), None, |_| None)
            .expect("built-in config defaults are valid")
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
            "layered_fallback_extractive_threshold_tokens = {}\n\n",
            self.context_compaction
                .layered_fallback_extractive_threshold_tokens
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
            "max_summary_tokens = {}\n\n",
            self.subagents.max_summary_tokens
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

        output.push_str("[permissions]\n");
        output.push_str(&format!(
            "read = {}\n",
            toml_string(self.permissions.read.as_str())
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
            "exa_mcp_url = {}\n",
            toml_string(&self.exa_mcp_url)
        ));
        output.push_str("exa_api_key_env = \"<redacted>\"\n\n");

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
            "show_reasoning_usage = {}\n\n",
            self.tui.show_reasoning_usage
        ));

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
        ProviderConfig::OpenAiCompatible(config) => config.preset.as_str(),
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenAiCompatibleConfig {
    pub preset: OpenAiCompatiblePreset,
    pub api_key_env: String,
    pub base_url: String,
    pub extra_headers: BTreeMap<String, String>,
    pub transport: ProviderTransportConfig,
}

/// Named presets for the OpenAI-compatible (Chat Completions) provider. Each
/// preset carries enough defaults that the user can wire a provider with just
/// an API key. `Custom` is for any other OpenAI-compatible endpoint (e.g.
/// self-hosted LiteLLM, Cloudflare Workers AI, Cohere) and requires the
/// caller to supply `base_url` and `api_key_env` explicitly.
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
            "openai_compatible" | "custom" => Some(Self::Custom),
            _ => None,
        }
    }

    /// Every preset that ships with `cargo run -p squeezy -- --list-providers`.
    /// Used by the CLI to enumerate options without hard-coding the list.
    pub fn all() -> [Self; 12] {
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
            Self::Custom,
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenAiConfig {
    pub api_key_env: String,
    pub base_url: String,
    pub transport: ProviderTransportConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnthropicConfig {
    pub api_key_env: String,
    pub base_url: String,
    pub transport: ProviderTransportConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoogleConfig {
    pub api_key_env: String,
    pub base_url: String,
    pub transport: ProviderTransportConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AzureOpenAiConfig {
    pub api_key_env: String,
    pub base_url: String,
    pub api_version: String,
    pub transport: ProviderTransportConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BedrockConfig {
    pub region: String,
    pub base_url: Option<String>,
    pub transport: ProviderTransportConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OllamaConfig {
    pub base_url: String,
    pub transport: ProviderTransportConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderTransportConfig {
    pub request_max_retries: u8,
    pub stream_max_retries: u8,
    pub stream_idle_timeout_ms: u64,
}

impl Default for ProviderTransportConfig {
    fn default() -> Self {
        Self {
            request_max_retries: DEFAULT_PROVIDER_REQUEST_MAX_RETRIES,
            stream_max_retries: DEFAULT_PROVIDER_STREAM_MAX_RETRIES,
            stream_idle_timeout_ms: DEFAULT_PROVIDER_STREAM_IDLE_TIMEOUT_MS,
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
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
}

impl SettingsFile {
    pub fn load_optional(path: &Path) -> Result<Self> {
        Ok(Self::load_optional_source(path, "settings")?.0)
    }

    fn load_optional_source(path: &Path, label: &str) -> Result<(Self, Vec<String>)> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Self::default(), vec!["defaults".to_string()]));
            }
            Err(error) => return Err(error.into()),
        };
        let settings = Self::from_toml_str(&text, &format!("{label}:{}", path.display()))?;
        let unknowns = take_unknown_fields();
        if !unknowns.is_empty()
            && let Err(error) = strip_unknown_fields_from_file(path, &unknowns)
        {
            tracing::warn!(
                path = %path.display(),
                ?error,
                "failed to strip unknown fields from settings.toml"
            );
        }
        Ok((
            settings,
            vec![
                "defaults".to_string(),
                format!("{label}:{}", path.display()),
            ],
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
        Ok(settings)
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
    pub base_url: Option<String>,
    pub default_model: Option<String>,
    pub api_version: Option<String>,
    pub region: Option<String>,
    pub preset: Option<String>,
    pub vertex_project: Option<String>,
    pub vertex_location: Option<String>,
    pub request_max_retries: Option<u8>,
    pub stream_max_retries: Option<u8>,
    pub stream_idle_timeout_ms: Option<u64>,
    pub headers: Option<BTreeMap<String, String>>,
}

impl ProviderSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "api_key_env",
                "base_url",
                "default_model",
                "api_version",
                "region",
                "preset",
                "vertex_project",
                "vertex_location",
                "request_max_retries",
                "stream_max_retries",
                "stream_idle_timeout_ms",
                "headers",
            ],
            source,
            path,
        )?;
        let headers = match table.get("headers") {
            None => None,
            Some(toml::Value::Table(table)) => {
                let mut map = BTreeMap::new();
                for (key, value) in table {
                    let toml::Value::String(value) = value else {
                        return Err(SqueezyError::Config(format!(
                            "{source}: {} must map to string values",
                            field(path, &format!("headers.{key}")),
                        )));
                    };
                    map.insert(key.clone(), value.clone());
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
        Ok(Self {
            api_key_env: string_value(table, "api_key_env", source, &field(path, "api_key_env"))?,
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
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.api_key_env, next.api_key_env);
        replace_if_some(&mut self.base_url, next.base_url);
        replace_if_some(&mut self.default_model, next.default_model);
        replace_if_some(&mut self.api_version, next.api_version);
        replace_if_some(&mut self.region, next.region);
        replace_if_some(&mut self.preset, next.preset);
        replace_if_some(&mut self.vertex_project, next.vertex_project);
        replace_if_some(&mut self.vertex_location, next.vertex_location);
        replace_if_some(&mut self.request_max_retries, next.request_max_retries);
        replace_if_some(&mut self.stream_max_retries, next.stream_max_retries);
        replace_if_some(
            &mut self.stream_idle_timeout_ms,
            next.stream_idle_timeout_ms,
        );
        replace_if_some(&mut self.headers, next.headers);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ModelSettings {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub profile: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub max_output_tokens: Option<u32>,
    pub stream_idle_timeout_ms: Option<u64>,
    pub store_responses: Option<bool>,
    pub selection_version: Option<u32>,
}

impl ModelSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "provider",
                "model",
                "profile",
                "reasoning_effort",
                "max_output_tokens",
                "stream_idle_timeout_ms",
                "store_responses",
                "selection_version",
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
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.provider, next.provider);
        replace_if_some(&mut self.model, next.model);
        replace_if_some(&mut self.profile, next.profile);
        replace_if_some(&mut self.reasoning_effort, next.reasoning_effort);
        replace_if_some(&mut self.max_output_tokens, next.max_output_tokens);
        replace_if_some(
            &mut self.stream_idle_timeout_ms,
            next.stream_idle_timeout_ms,
        );
        replace_if_some(&mut self.store_responses, next.store_responses);
        replace_if_some(&mut self.selection_version, next.selection_version);
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SessionSettings {
    pub mode: Option<SessionMode>,
    pub log_dir: Option<PathBuf>,
    pub log_retention_days: Option<u64>,
    pub max_event_bytes: Option<usize>,
    pub max_session_bytes: Option<usize>,
}

impl SessionSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "mode",
                "log_dir",
                "log_retention_days",
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
            log_dir: path_value(table, "log_dir", source, &field(path, "log_dir"))?,
            log_retention_days: u64_value(
                table,
                "log_retention_days",
                source,
                &field(path, "log_retention_days"),
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
        replace_if_some(&mut self.log_dir, next.log_dir);
        replace_if_some(&mut self.log_retention_days, next.log_retention_days);
        replace_if_some(&mut self.max_event_bytes, next.max_event_bytes);
        replace_if_some(&mut self.max_session_bytes, next.max_session_bytes);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLogConfig {
    pub log_dir: Option<PathBuf>,
    pub log_retention_days: u64,
    pub max_event_bytes: usize,
    pub max_session_bytes: usize,
}

impl SessionLogConfig {
    fn from_settings(settings: &SessionSettings) -> Self {
        Self {
            log_dir: settings.log_dir.clone(),
            log_retention_days: settings
                .log_retention_days
                .filter(|value| *value > 0)
                .unwrap_or(DEFAULT_SESSION_LOG_RETENTION_DAYS),
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
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentSettings {
    pub enabled: Option<bool>,
    pub explore_enabled: Option<bool>,
    pub explore_model: Option<String>,
    pub max_tool_calls_per_call: Option<u64>,
    pub max_tool_bytes_read_per_call: Option<u64>,
    pub max_search_files_per_call: Option<u64>,
    pub max_model_rounds: Option<usize>,
    pub max_summary_tokens: Option<u32>,
}

impl SubagentSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "enabled",
                "explore_enabled",
                "explore_model",
                "max_tool_calls_per_call",
                "max_tool_bytes_read_per_call",
                "max_search_files_per_call",
                "max_model_rounds",
                "max_summary_tokens",
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
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.enabled, next.enabled);
        replace_if_some(&mut self.explore_enabled, next.explore_enabled);
        replace_if_some(&mut self.explore_model, next.explore_model);
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
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentConfig {
    pub enabled: bool,
    pub explore_enabled: bool,
    pub explore_model: Option<String>,
    pub max_tool_calls_per_call: u64,
    pub max_tool_bytes_read_per_call: u64,
    pub max_search_files_per_call: u64,
    pub max_model_rounds: usize,
    pub max_summary_tokens: u32,
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
        }
    }
}

impl Default for SubagentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            explore_enabled: true,
            explore_model: None,
            max_tool_calls_per_call: DEFAULT_SUBAGENT_MAX_TOOL_CALLS_PER_CALL,
            max_tool_bytes_read_per_call: DEFAULT_SUBAGENT_MAX_TOOL_BYTES_READ_PER_CALL,
            max_search_files_per_call: DEFAULT_SUBAGENT_MAX_SEARCH_FILES_PER_CALL,
            max_model_rounds: DEFAULT_SUBAGENT_MAX_MODEL_ROUNDS,
            max_summary_tokens: DEFAULT_SUBAGENT_MAX_SUMMARY_TOKENS,
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
    /// Maximum bytes of `~/.squeezy/memory.md` stitched into the base
    /// instructions at session start. 0 disables ingestion.
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
        }
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
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PermissionSettings {
    pub read: Option<PermissionMode>,
    pub edit: Option<PermissionMode>,
    pub shell: Option<PermissionMode>,
    pub ignored_search: Option<PermissionMode>,
    pub web: Option<PermissionMode>,
    pub mcp: Option<PermissionMode>,
    pub shell_classifier: Option<bool>,
    pub ai_reviewer: Option<AiReviewerSettings>,
    pub shell_sandbox: Option<ShellSandboxSettings>,
    pub rules: Vec<PermissionRule>,
}

impl PermissionSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "read",
                "edit",
                "shell",
                "ignored_search",
                "web",
                "mcp",
                "shell_classifier",
                "ai_reviewer",
                "shell_sandbox",
                "rules",
            ],
            source,
            path,
        )?;
        Ok(Self {
            read: permission_value(table, "read", source, &field(path, "read"))?,
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
            shell_classifier: bool_value(
                table,
                "shell_classifier",
                source,
                &field(path, "shell_classifier"),
            )?,
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
        replace_if_some(&mut self.read, next.read);
        replace_if_some(&mut self.edit, next.edit);
        replace_if_some(&mut self.shell, next.shell);
        replace_if_some(&mut self.ignored_search, next.ignored_search);
        replace_if_some(&mut self.web, next.web);
        replace_if_some(&mut self.mcp, next.mcp);
        replace_if_some(&mut self.shell_classifier, next.shell_classifier);
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
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AiReviewerSettings {
    pub enabled: Option<bool>,
    pub model: Option<String>,
    pub allow_capabilities: Option<Vec<String>>,
    pub policy_file: Option<String>,
    pub timeout_secs: Option<u64>,
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
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.enabled, next.enabled);
        replace_if_some(&mut self.model, next.model);
        replace_if_some(&mut self.allow_capabilities, next.allow_capabilities);
        replace_if_some(&mut self.policy_file, next.policy_file);
        replace_if_some(&mut self.timeout_secs, next.timeout_secs);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiReviewerConfig {
    pub enabled: bool,
    pub model: Option<String>,
    pub allow_capabilities: Vec<PermissionCapability>,
    pub policy_file: Option<PathBuf>,
    pub timeout_secs: u64,
}

impl Default for AiReviewerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: None,
            allow_capabilities: vec![PermissionCapability::Read, PermissionCapability::Search],
            policy_file: None,
            timeout_secs: 15,
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
    pub read: PermissionMode,
    pub edit: PermissionMode,
    pub shell: PermissionMode,
    pub ignored_search: PermissionMode,
    pub web: PermissionMode,
    pub mcp: PermissionMode,
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
        Ok(Self {
            read: parse_permission(
                var("SQUEEZY_READ_PERMISSION"),
                settings.read.unwrap_or(PermissionMode::Allow),
            ),
            edit: parse_permission(
                var("SQUEEZY_EDIT_PERMISSION"),
                settings.edit.unwrap_or(PermissionMode::Allow),
            ),
            shell: parse_permission(
                var("SQUEEZY_SHELL_PERMISSION"),
                settings.shell.unwrap_or(PermissionMode::Ask),
            ),
            ignored_search: parse_permission(
                var("SQUEEZY_IGNORED_SEARCH_PERMISSION"),
                settings.ignored_search.unwrap_or(PermissionMode::Allow),
            ),
            web: parse_permission(
                var("SQUEEZY_WEB_PERMISSION"),
                settings.web.unwrap_or(PermissionMode::Ask),
            ),
            mcp: parse_permission(
                var("SQUEEZY_MCP_PERMISSION"),
                settings.mcp.unwrap_or(PermissionMode::Ask),
            ),
            shell_classifier: parse_bool(
                var("SQUEEZY_SHELL_PERMISSION_CLASSIFIER"),
                settings.shell_classifier.unwrap_or(false),
            ),
            ai_reviewer: AiReviewerConfig::from_settings(settings.ai_reviewer, source)?,
            shell_sandbox: ShellSandboxConfig::from_settings(
                settings.shell_sandbox,
                source,
                workspace_root,
            )?,
            rules: settings.rules,
        })
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
            return PermissionVerdict {
                action,
                reason,
                matched_rule: Some(rule),
            };
        }
        let action = self.mode_for(legacy_scope_for_capability(request.capability));
        PermissionVerdict {
            action,
            matched_rule: None,
            reason: format!(
                "default {} permission is {}",
                request.capability.as_str(),
                action.as_str()
            ),
        }
    }
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
        Self {
            read: PermissionMode::Allow,
            edit: PermissionMode::Allow,
            shell: PermissionMode::Ask,
            ignored_search: PermissionMode::Allow,
            web: PermissionMode::Ask,
            mcp: PermissionMode::Ask,
            shell_classifier: false,
            ai_reviewer: AiReviewerConfig::default(),
            shell_sandbox: ShellSandboxConfig::default(),
            rules: Vec::new(),
        }
    }
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

fn parse_bool(value: Option<String>, default: bool) -> bool {
    value.as_deref().map_or(default, parse_enabled_bool)
}

fn legacy_scope_for_capability(capability: PermissionCapability) -> PermissionScope {
    match capability {
        PermissionCapability::Read => PermissionScope::Read,
        PermissionCapability::Search => PermissionScope::Read,
        PermissionCapability::Edit => PermissionScope::Edit,
        PermissionCapability::Shell => PermissionScope::Shell,
        PermissionCapability::Network => PermissionScope::Web,
        PermissionCapability::Mcp => PermissionScope::Mcp,
        PermissionCapability::Git => PermissionScope::Shell,
        PermissionCapability::Compiler => PermissionScope::Shell,
        PermissionCapability::Destructive => PermissionScope::Shell,
    }
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
}

impl WebSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(table, &["exa_mcp_url", "exa_api_key_env"], source, path)?;
        Ok(Self {
            exa_mcp_url: string_value(table, "exa_mcp_url", source, &field(path, "exa_mcp_url"))?,
            exa_api_key_env: string_value(
                table,
                "exa_api_key_env",
                source,
                &field(path, "exa_api_key_env"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.exa_mcp_url, next.exa_mcp_url);
        replace_if_some(&mut self.exa_api_key_env, next.exa_api_key_env);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SkillsSettings {
    pub user_dir: Option<PathBuf>,
    pub compat_user_dir: Option<PathBuf>,
    pub active_budget_chars: Option<usize>,
    pub active_body_cap_chars: Option<usize>,
    pub preamble_enabled: Option<bool>,
    pub preamble_budget_chars: Option<usize>,
    pub config: Vec<SkillConfigEntry>,
}

impl SkillsSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "user_dir",
                "compat_user_dir",
                "active_budget_chars",
                "active_body_cap_chars",
                "preamble_enabled",
                "preamble_budget_chars",
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
            config: skill_config_entries_value(table, source, &field(path, "config"))?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.user_dir, next.user_dir);
        replace_if_some(&mut self.compat_user_dir, next.compat_user_dir);
        replace_if_some(&mut self.active_budget_chars, next.active_budget_chars);
        replace_if_some(&mut self.active_body_cap_chars, next.active_body_cap_chars);
        replace_if_some(&mut self.preamble_enabled, next.preamble_enabled);
        replace_if_some(&mut self.preamble_budget_chars, next.preamble_budget_chars);
        self.config.extend(next.config);
    }
}

pub const DEFAULT_SKILLS_ACTIVE_BUDGET_CHARS: usize = 4_000;
pub const DEFAULT_SKILLS_ACTIVE_BODY_CAP_CHARS: usize = 16_000;
pub const DEFAULT_SKILLS_PREAMBLE_ENABLED: bool = true;
pub const DEFAULT_SKILLS_PREAMBLE_BUDGET_CHARS: usize = 800;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillConfigEntry {
    pub name: Option<String>,
    pub path: Option<PathBuf>,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillsConfig {
    pub user_dir: PathBuf,
    pub compat_user_dir: PathBuf,
    pub active_budget_chars: usize,
    pub active_body_cap_chars: usize,
    pub preamble_enabled: bool,
    pub preamble_budget_chars: usize,
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
            active_budget_chars: settings
                .active_budget_chars
                .unwrap_or(DEFAULT_SKILLS_ACTIVE_BUDGET_CHARS),
            active_body_cap_chars: settings
                .active_body_cap_chars
                .unwrap_or(DEFAULT_SKILLS_ACTIVE_BODY_CAP_CHARS),
            preamble_enabled: settings
                .preamble_enabled
                .unwrap_or(DEFAULT_SKILLS_PREAMBLE_ENABLED),
            preamble_budget_chars: settings
                .preamble_budget_chars
                .unwrap_or(DEFAULT_SKILLS_PREAMBLE_BUDGET_CHARS),
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
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            user_dir: default_squeezy_skills_dir(),
            compat_user_dir: default_agent_compat_skills_dir(),
            active_budget_chars: DEFAULT_SKILLS_ACTIVE_BUDGET_CHARS,
            active_body_cap_chars: DEFAULT_SKILLS_ACTIVE_BODY_CAP_CHARS,
            preamble_enabled: DEFAULT_SKILLS_PREAMBLE_ENABLED,
            preamble_budget_chars: DEFAULT_SKILLS_PREAMBLE_BUDGET_CHARS,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TuiConfig {
    pub tick_rate_ms: u64,
    pub status_verbosity: StatusVerbosity,
    pub response_verbosity: ResponseVerbosity,
    pub tool_output_verbosity: ToolOutputVerbosity,
    pub transcript_default: TranscriptDefault,
    pub alternate_screen: TuiAlternateScreen,
    pub show_reasoning_usage: bool,
    /// Ordered list of status-line item identifiers. `None` means
    /// "use the built-in default list"; an empty list means the user
    /// deliberately disabled the detail line.
    pub status_line: Option<Vec<String>>,
    /// Color status-line items with their accent palette.
    /// Defaults to `true`.
    pub status_line_use_colors: bool,
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
            show_reasoning_usage: settings.show_reasoning_usage.unwrap_or(true),
            status_line: settings.status_line,
            status_line_use_colors: settings.status_line_use_colors.unwrap_or(true),
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
    pub show_reasoning_usage: Option<bool>,
    pub status_line: Option<Vec<String>>,
    pub status_line_use_colors: Option<bool>,
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
                "show_reasoning_usage",
                "status_line",
                "status_line_use_colors",
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
            show_reasoning_usage: bool_value(
                table,
                "show_reasoning_usage",
                source,
                &field(path, "show_reasoning_usage"),
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
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.tick_rate_ms, next.tick_rate_ms);
        replace_if_some(&mut self.status_verbosity, next.status_verbosity);
        replace_if_some(&mut self.response_verbosity, next.response_verbosity);
        replace_if_some(&mut self.tool_output_verbosity, next.tool_output_verbosity);
        replace_if_some(&mut self.transcript_default, next.transcript_default);
        replace_if_some(&mut self.alternate_screen, next.alternate_screen);
        replace_if_some(&mut self.show_reasoning_usage, next.show_reasoning_usage);
        replace_if_some(&mut self.status_line, next.status_line);
        replace_if_some(
            &mut self.status_line_use_colors,
            next.status_line_use_colors,
        );
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
# log_dir = ".squeezy/sessions"
# log_retention_days = 30
# max_event_bytes = 65536
# max_session_bytes = 52428800

[context]
# compaction_enabled = true
# compaction_estimated_tokens = 60000
# compaction_min_items = 16
# compaction_recent_items = 6
# compaction_max_summary_bytes = 12000
# repo_doc_max_bytes = 16384    # cap on AGENTS.md content stitched into base instructions (0 disables)
# user_memory_max_bytes = 8192  # cap on ~/.squeezy/memory.md content stitched into base instructions (0 disables)
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
# max_tool_calls_per_call = 24
# max_tool_bytes_read_per_call = 8388608
# max_search_files_per_call = 2000
# max_model_rounds = 4
# max_summary_tokens = 16000

# [providers.openai]
# api_key_env = "OPENAI_API_KEY"
# base_url = "https://api.openai.com/v1"
# default_model = "gpt-5.5"
# stream_idle_timeout_ms = 300000

# [providers.anthropic]
# api_key_env = "ANTHROPIC_API_KEY"
# base_url = "https://api.anthropic.com/v1"
# default_model = "claude-opus-4-7"
# stream_idle_timeout_ms = 300000

[permissions]
# read = "allow"
# edit = "allow"
# shell = "ask"
# ignored_search = "allow"
# web = "ask"
# mcp = "ask"
# shell_classifier = false       # narrow LLM fallback for ambiguous shell commands (extra LLM call)

# [permissions.ai_reviewer]
# enabled = false
# model = "gpt-5-mini"          # optional reviewer model override
# allow_capabilities = ["read", "search"]
# policy_file = ""              # optional local approval policy override
# timeout_secs = 15
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
# network = "deny_by_default"       # deny_by_default | allow_when_approved
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
# exa_mcp_url = "https://mcp.exa.ai/mcp"
# exa_api_key_env = "EXA_API_KEY"

# [skills]
# user_dir = "~/.squeezy/skills"
# compat_user_dir = "~/.agents/skills"
# active_budget_chars = 4000
# active_body_cap_chars = 16000
# preamble_enabled = true
# preamble_budget_chars = 800
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
# log_dir = ".squeezy/sessions"
# log_retention_days = 30
# max_event_bytes = 65536
# max_session_bytes = 52428800

[context]
# compaction_enabled = true
# compaction_estimated_tokens = 60000
# compaction_min_items = 16
# compaction_recent_items = 6
# compaction_max_summary_bytes = 12000
# repo_doc_max_bytes = 16384    # cap on AGENTS.md content stitched into base instructions (0 disables)
# user_memory_max_bytes = 8192  # cap on ~/.squeezy/memory.md content stitched into base instructions (0 disables)
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
# max_tool_calls_per_call = 24
# max_tool_bytes_read_per_call = 8388608
# max_search_files_per_call = 2000
# max_model_rounds = 4
# max_summary_tokens = 16000

# [redaction]
# Add project-specific Rust regex patterns for secrets Squeezy should redact
# everywhere they appear in tool output, model requests, and UI surfaces.
# custom_patterns = []

[permissions]
# read = "allow"
# edit = "allow"
# shell = "ask"
# ignored_search = "allow"
# web = "ask"
# mcp = "ask"
#
# [permissions.ai_reviewer]
# enabled = false
# allow_capabilities = ["read", "search"]
#
# [[permissions.rules]]
# capability = "compiler"
# target = "cargo test:*"
# action = "allow"
# source = "project"
#
# [permissions.shell_sandbox]
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

fn load_default_settings_sources() -> Result<(SettingsFile, Vec<String>)> {
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
) -> Result<(SettingsFile, Vec<String>)> {
    let mut settings = SettingsFile::default();
    let mut sources = vec!["defaults".to_string()];
    for (path, label) in [
        (user_path, "user"),
        (project_path, "project"),
        (repo_path, "repo"),
    ] {
        let Some(path) = path else { continue };
        if !path.is_file() {
            continue;
        }
        let parsed = SettingsFile::from_toml_str(
            &fs::read_to_string(path)?,
            &format!("{label}:{}", path.display()),
        )?;
        let unknowns = take_unknown_fields();
        if !unknowns.is_empty()
            && let Err(error) = strip_unknown_fields_from_file(path, &unknowns)
        {
            tracing::warn!(
                path = %path.display(),
                ?error,
                "failed to strip unknown fields from settings.toml"
            );
        }
        settings.merge(parsed);
        sources.push(format!("{label}:{}", path.display()));
    }
    Ok((settings, sources))
}

fn provider_setting(
    providers: &BTreeMap<String, ProviderSettings>,
    provider: &str,
    key: &str,
) -> Option<String> {
    let settings = providers.get(provider)?;
    let value = match key {
        "api_key_env" => settings.api_key_env.as_ref(),
        "base_url" => settings.base_url.as_ref(),
        "default_model" => settings.default_model.as_ref(),
        "api_version" => settings.api_version.as_ref(),
        "region" => settings.region.as_ref(),
        "preset" => settings.preset.as_ref(),
        "vertex_project" => settings.vertex_project.as_ref(),
        "vertex_location" => settings.vertex_location.as_ref(),
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
    let extra_headers = provider_setting_headers(providers, section).unwrap_or_default();
    let transport = provider_transport_settings(providers, &[section]);
    Ok(ProviderConfig::OpenAiCompatible(OpenAiCompatibleConfig {
        preset,
        api_key_env,
        base_url,
        extra_headers,
        transport,
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
            OpenAiCompatiblePreset::Custom => &["openai_compatible"],
        },
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

thread_local! {
    /// Dotted paths of unknown fields seen during the most recent
    /// `SettingsFile::from_toml_str` call. The file loader clears this
    /// before parsing and drains it afterwards to rewrite the source
    /// without the dead keys.
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
    // we iterate. The loader uses `UNKNOWN_FIELDS` to rewrite the source
    // file without the dead keys after each load.
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

fn strip_unknown_fields_from_file(path: &Path, dotted_paths: &[String]) -> std::io::Result<()> {
    let text = fs::read_to_string(path)?;
    let mut doc: toml_edit::DocumentMut = match text.parse() {
        Ok(doc) => doc,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                ?error,
                "could not re-parse settings.toml for cleanup; leaving file untouched"
            );
            return Ok(());
        }
    };
    let mut changed = false;
    for dotted in dotted_paths {
        if remove_dotted_path(doc.as_table_mut(), dotted) {
            changed = true;
        }
    }
    if changed {
        fs::write(path, doc.to_string())?;
    }
    Ok(())
}

fn remove_dotted_path(root: &mut toml_edit::Table, dotted: &str) -> bool {
    let mut parts = dotted.split('.').collect::<Vec<_>>();
    let Some(last) = parts.pop() else {
        return false;
    };
    let mut current: &mut toml_edit::Table = root;
    for segment in &parts {
        let next = current
            .get_mut(segment)
            .and_then(|item| item.as_table_mut());
        match next {
            Some(table) => current = table,
            None => return false,
        }
    }
    current.remove(last).is_some()
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

fn string_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<String>> {
    match table.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_str()
            .map(str::to_string)
            .map(Some)
            .ok_or_else(|| type_error(source, path, "string")),
    }
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
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| type_error(source, &format!("{path}.{index}"), "string"))
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
            value
                .as_str()
                .map(|value| (key.clone(), value.to_string()))
                .ok_or_else(|| type_error(source, &field(path, key), "string"))
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
                &["capability", "target", "action", "source", "reason"],
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
            Ok(PermissionRule::new(
                capability,
                target,
                action,
                source_value,
                reason,
            ))
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
                &["target", "action", "source", "reason"],
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
            Ok(PermissionRule::new(
                "mcp",
                target,
                action,
                source_value,
                reason,
            ))
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TurnId(u64);

impl TurnId {
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
}

impl TranscriptItem {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            reasoning: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            reasoning: None,
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
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            reasoning: None,
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
    UnsupportedImage,
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
        }
    }

    pub fn is_supported_text(self) -> bool {
        !matches!(self, Self::UnsupportedBinary | Self::UnsupportedImage)
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
    Go,
    Java,
    JavaScript,
    Jsx,
    Python,
    Rust,
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
}

impl LanguageFamily {
    pub const ALL: [Self; 7] = [
        Self::Rust,
        Self::Python,
        Self::Java,
        Self::CSharp,
        Self::Go,
        Self::CFamily,
        Self::JsTs,
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
            b"go" => Self::Go,
            b"java" => Self::Java,
            b"jsx" => Self::Jsx,
            b"py" => Self::Python,
            b"rs" => Self::Rust,
            b"tsx" => Self::Tsx,
            _ => Self::Unsupported,
        }
    }

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::C => "C",
            Self::CSharp => "C#",
            Self::Cpp => "C++",
            Self::Go => "Go",
            Self::Java => "Java",
            Self::JavaScript => "JavaScript",
            Self::Jsx => "JSX",
            Self::Python => "Python",
            Self::Rust => "Rust",
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

pub const DEFAULT_INSTRUCTIONS: &str = "You are Squeezy, a cost-aware coding agent. Keep responses concise, explicit, and grounded in workspace evidence. Prefer semantic graph tools such as repo_map, definition_search, symbol_context, reference_search, and read_slice before grep/read_file on supported code. Use websearch for web discovery and webfetch for retrieving a specific URL when web tools are available. Treat websearch and webfetch results as remote documentation evidence, cite source URLs from their citation metadata when relying on them, and keep remote docs distinct from local code or graph facts. Do not invent URLs. If a tool call is denied, do not retry the same call. Do not issue duplicate tool calls — if you need the same result you already have, refer to the earlier output instead of re-running the call. For simple existence checks (e.g. \"does function X exist?\"), a single grep or definition_search is usually enough. Before a batch of two or more related tool calls, emit a brief preamble (1–2 sentences, roughly 8–12 words) saying what you are about to do — for example: \"Looking up Error in src/lib.rs, then tracing its constructors.\" Logically group related tools under one preamble; if a turn covers two unrelated topics, emit one preamble per group. Skip the preamble for a single tool call or a trivial answer.";

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
