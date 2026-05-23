use std::{
    collections::BTreeMap,
    env, fmt, fs,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_OPENAI_MODEL: &str = "gpt-5-nano";
pub const DEFAULT_ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com/v1";
pub const DEFAULT_ANTHROPIC_MODEL: &str = "claude-3-5-haiku-20241022";
pub const DEFAULT_GOOGLE_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
pub const DEFAULT_GOOGLE_MODEL: &str = "gemini-2.5-flash-lite";
pub const DEFAULT_AZURE_OPENAI_BASE_URL: &str = "";
pub const DEFAULT_AZURE_OPENAI_API_VERSION: &str = "v1";
pub const DEFAULT_AZURE_OPENAI_MODEL: &str = DEFAULT_OPENAI_MODEL;
pub const DEFAULT_BEDROCK_REGION: &str = "us-east-1";
pub const DEFAULT_BEDROCK_MODEL: &str = "anthropic.claude-3-5-haiku-20241022-v1:0";
pub const DEFAULT_OLLAMA_BASE_URL: &str = "http://localhost:11434/api";
pub const DEFAULT_OLLAMA_MODEL: &str = "qwen3";
pub const DEFAULT_EXA_MCP_URL: &str = "https://mcp.exa.ai/mcp";
pub const DEFAULT_EXA_API_KEY_ENV: &str = "EXA_API_KEY";
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 128;
pub const DEFAULT_TOOL_SPILL_THRESHOLD_BYTES: usize = 25_000;
pub const DEFAULT_TOOL_PREVIEW_BYTES: usize = 2_000;
pub const DEFAULT_MAX_TOOL_RESULT_BYTES_PER_ROUND: usize = 50_000;
pub const DEFAULT_TOOL_OUTPUT_RETENTION_DAYS: u64 = 7;
pub const DEFAULT_MAX_TOOL_CALLS_PER_TURN: u64 = 64;
pub const DEFAULT_MAX_TOOL_BYTES_READ_PER_TURN: u64 = 20_000_000;
pub const DEFAULT_MAX_SEARCH_FILES_PER_TURN: u64 = 50_000;
pub const DEFAULT_TELEMETRY_ENDPOINT: &str = "https://telemetry.squeezy.dev/v1/batch";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppConfig {
    pub provider: ProviderConfig,
    pub model: String,
    pub profile: ModelProfile,
    pub instructions: String,
    pub max_output_tokens: Option<u32>,
    pub tick_rate: Duration,
    pub workspace_root: PathBuf,
    pub permissions: PermissionPolicy,
    pub store_responses: bool,
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
    pub telemetry: TelemetryConfig,
}

impl AppConfig {
    pub fn from_env() -> Self {
        Self::from_env_vars(|name| env::var(name).ok())
    }

    pub fn from_env_and_settings() -> Result<Self> {
        Self::from_settings_path_and_env(default_settings_path())
    }

    pub fn from_env_and_settings_with_provider(provider: &str) -> Result<Self> {
        Self::from_settings_path_and_env_with_provider(default_settings_path(), provider)
    }

    pub fn from_settings_path_and_env(path: PathBuf) -> Result<Self> {
        let settings = SettingsFile::load_optional(&path)?;
        Ok(Self::from_settings_and_env_vars(settings, |name| {
            env::var(name).ok()
        }))
    }

    pub fn from_settings_path_and_env_with_provider(path: PathBuf, provider: &str) -> Result<Self> {
        let settings = SettingsFile::load_optional(&path)?;
        Ok(Self::from_settings_and_env_vars(settings, |name| {
            if name == "SQUEEZY_PROVIDER" {
                Some(provider.to_string())
            } else {
                env::var(name).ok()
            }
        }))
    }

    pub fn from_env_with_provider(provider: &str) -> Self {
        Self::from_env_vars(|name| {
            if name == "SQUEEZY_PROVIDER" {
                Some(provider.to_string())
            } else {
                env::var(name).ok()
            }
        })
    }

    fn from_env_vars(mut var: impl FnMut(&str) -> Option<String>) -> Self {
        Self::from_settings_and_env_vars(SettingsFile::default(), &mut var)
    }

    fn from_settings_and_env_vars(
        settings: SettingsFile,
        mut var: impl FnMut(&str) -> Option<String>,
    ) -> Self {
        let provider_name = var("SQUEEZY_PROVIDER")
            .or(settings.provider.clone())
            .unwrap_or_else(|| "openai".to_string())
            .trim()
            .to_ascii_lowercase();
        let providers = settings.providers.unwrap_or_default();
        let provider = match provider_name.as_str() {
            "anthropic" | "claude" => ProviderConfig::Anthropic(AnthropicConfig {
                api_key_env: var("ANTHROPIC_API_KEY_ENV")
                    .or_else(|| provider_setting(&providers, "anthropic", "api_key_env"))
                    .unwrap_or_else(|| "ANTHROPIC_API_KEY".to_string()),
                base_url: var("ANTHROPIC_BASE_URL")
                    .or_else(|| provider_setting(&providers, "anthropic", "base_url"))
                    .unwrap_or_else(|| DEFAULT_ANTHROPIC_BASE_URL.to_string()),
            }),
            "google" | "gemini" => ProviderConfig::Google(GoogleConfig {
                api_key_env: var("GOOGLE_API_KEY_ENV")
                    .or_else(|| provider_setting(&providers, "google", "api_key_env"))
                    .unwrap_or_else(|| "GEMINI_API_KEY".to_string()),
                base_url: var("GOOGLE_BASE_URL")
                    .or_else(|| provider_setting(&providers, "google", "base_url"))
                    .unwrap_or_else(|| DEFAULT_GOOGLE_BASE_URL.to_string()),
            }),
            "azure" | "azure-openai" | "azure_openai" => {
                ProviderConfig::AzureOpenAi(AzureOpenAiConfig {
                    api_key_env: var("AZURE_OPENAI_API_KEY_ENV")
                        .or_else(|| provider_setting(&providers, "azure_openai", "api_key_env"))
                        .or_else(|| provider_setting(&providers, "azure", "api_key_env"))
                        .unwrap_or_else(|| "AZURE_OPENAI_API_KEY".to_string()),
                    base_url: var("AZURE_OPENAI_BASE_URL")
                        .or_else(|| provider_setting(&providers, "azure_openai", "base_url"))
                        .or_else(|| provider_setting(&providers, "azure", "base_url"))
                        .unwrap_or_else(|| DEFAULT_AZURE_OPENAI_BASE_URL.to_string()),
                    api_version: var("AZURE_OPENAI_API_VERSION")
                        .or_else(|| provider_setting(&providers, "azure_openai", "api_version"))
                        .or_else(|| provider_setting(&providers, "azure", "api_version"))
                        .unwrap_or_else(|| DEFAULT_AZURE_OPENAI_API_VERSION.to_string()),
                })
            }
            "bedrock" | "amazon-bedrock" | "amazon_bedrock" => {
                ProviderConfig::Bedrock(BedrockConfig {
                    region: var("AWS_REGION")
                        .or_else(|| var("AWS_DEFAULT_REGION"))
                        .or_else(|| provider_setting(&providers, "bedrock", "region"))
                        .unwrap_or_else(|| DEFAULT_BEDROCK_REGION.to_string()),
                    base_url: var("BEDROCK_BASE_URL")
                        .or_else(|| provider_setting(&providers, "bedrock", "base_url")),
                })
            }
            "ollama" | "local" => ProviderConfig::Ollama(OllamaConfig {
                base_url: var("OLLAMA_BASE_URL")
                    .or_else(|| provider_setting(&providers, "ollama", "base_url"))
                    .unwrap_or_else(|| DEFAULT_OLLAMA_BASE_URL.to_string()),
            }),
            _ => ProviderConfig::OpenAi(OpenAiConfig {
                api_key_env: var("OPENAI_API_KEY_ENV")
                    .or_else(|| provider_setting(&providers, "openai", "api_key_env"))
                    .unwrap_or_else(|| "OPENAI_API_KEY".to_string()),
                base_url: var("OPENAI_BASE_URL")
                    .or_else(|| provider_setting(&providers, "openai", "base_url"))
                    .unwrap_or_else(|| DEFAULT_OPENAI_BASE_URL.to_string()),
            }),
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
        };
        let profile = var("SQUEEZY_PROFILE")
            .or(settings.profile)
            .as_deref()
            .and_then(ModelProfile::parse)
            .unwrap_or_default();
        let model = var("SQUEEZY_MODEL")
            .or(settings.model)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(default_model);
        let exa_mcp_url =
            var("SQUEEZY_EXA_MCP_URL").unwrap_or_else(|| DEFAULT_EXA_MCP_URL.to_string());
        let exa_api_key_env =
            var("SQUEEZY_EXA_API_KEY_ENV").unwrap_or_else(|| DEFAULT_EXA_API_KEY_ENV.to_string());
        let requested_store_responses = parse_bool(var("SQUEEZY_STORE_RESPONSES").as_deref());
        let store_responses = requested_store_responses
            && matches!(
                provider,
                ProviderConfig::OpenAi(_) | ProviderConfig::AzureOpenAi(_)
            );
        let max_parallel_tools = var("SQUEEZY_MAX_PARALLEL_TOOLS")
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(8);
        let tool_spill_threshold_bytes = parse_usize(
            var("SQUEEZY_TOOL_SPILL_THRESHOLD_BYTES"),
            DEFAULT_TOOL_SPILL_THRESHOLD_BYTES,
        );
        let tool_preview_bytes = parse_usize(
            var("SQUEEZY_TOOL_PREVIEW_BYTES"),
            DEFAULT_TOOL_PREVIEW_BYTES,
        );
        let max_tool_result_bytes_per_round = parse_usize(
            var("SQUEEZY_MAX_TOOL_RESULT_BYTES_PER_ROUND"),
            DEFAULT_MAX_TOOL_RESULT_BYTES_PER_ROUND,
        );
        let tool_output_retention_days = var("SQUEEZY_TOOL_OUTPUT_RETENTION_DAYS")
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_TOOL_OUTPUT_RETENTION_DAYS);
        let max_tool_calls_per_turn = parse_u64(
            var("SQUEEZY_MAX_TOOL_CALLS_PER_TURN"),
            DEFAULT_MAX_TOOL_CALLS_PER_TURN,
        );
        let max_tool_bytes_read_per_turn = parse_u64(
            var("SQUEEZY_MAX_TOOL_BYTES_READ_PER_TURN"),
            DEFAULT_MAX_TOOL_BYTES_READ_PER_TURN,
        );
        let max_search_files_per_turn = parse_u64(
            var("SQUEEZY_MAX_SEARCH_FILES_PER_TURN"),
            DEFAULT_MAX_SEARCH_FILES_PER_TURN,
        );
        let telemetry = TelemetryConfig::from_env_vars(&mut var);
        Self {
            provider,
            model,
            profile,
            instructions: DEFAULT_INSTRUCTIONS.to_string(),
            max_output_tokens: Some(DEFAULT_MAX_OUTPUT_TOKENS),
            tick_rate: Duration::from_millis(50),
            workspace_root: env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            permissions: PermissionPolicy::from_env_vars(var),
            store_responses,
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
            telemetry,
        }
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenAiConfig {
    pub api_key_env: String,
    pub base_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnthropicConfig {
    pub api_key_env: String,
    pub base_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoogleConfig {
    pub api_key_env: String,
    pub base_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AzureOpenAiConfig {
    pub api_key_env: String,
    pub base_url: String,
    pub api_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BedrockConfig {
    pub region: String,
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OllamaConfig {
    pub base_url: String,
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
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsFile {
    pub provider: Option<String>,
    pub profile: Option<String>,
    pub model: Option<String>,
    pub providers: Option<BTreeMap<String, ProviderSettings>>,
}

impl SettingsFile {
    pub fn load_optional(path: &Path) -> Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error.into()),
        };
        toml::from_str(&text)
            .map_err(|err| SqueezyError::Config(format!("{}: {err}", path.display())))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSettings {
    pub api_key_env: Option<String>,
    pub base_url: Option<String>,
    pub default_model: Option<String>,
    pub api_version: Option<String>,
    pub region: Option<String>,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PermissionScope {
    Read,
    Edit,
    Shell,
    IgnoredSearch,
    Web,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionPolicy {
    pub read: PermissionMode,
    pub edit: PermissionMode,
    pub shell: PermissionMode,
    pub ignored_search: PermissionMode,
    pub web: PermissionMode,
}

impl PermissionPolicy {
    pub fn from_env_vars(mut var: impl FnMut(&str) -> Option<String>) -> Self {
        Self {
            read: parse_permission(var("SQUEEZY_READ_PERMISSION"), PermissionMode::Allow),
            edit: parse_permission(var("SQUEEZY_EDIT_PERMISSION"), PermissionMode::Ask),
            shell: parse_permission(var("SQUEEZY_SHELL_PERMISSION"), PermissionMode::Ask),
            ignored_search: parse_permission(
                var("SQUEEZY_IGNORED_SEARCH_PERMISSION"),
                PermissionMode::Allow,
            ),
            web: parse_permission(var("SQUEEZY_WEB_PERMISSION"), PermissionMode::Ask),
        }
    }

    pub const fn mode_for(&self, scope: PermissionScope) -> PermissionMode {
        match scope {
            PermissionScope::Read => self.read,
            PermissionScope::Edit => self.edit,
            PermissionScope::Shell => self.shell,
            PermissionScope::IgnoredSearch => self.ignored_search,
            PermissionScope::Web => self.web,
        }
    }
}

impl Default for PermissionPolicy {
    fn default() -> Self {
        Self {
            read: PermissionMode::Allow,
            edit: PermissionMode::Ask,
            shell: PermissionMode::Ask,
            ignored_search: PermissionMode::Allow,
            web: PermissionMode::Ask,
        }
    }
}

fn parse_permission(value: Option<String>, default: PermissionMode) -> PermissionMode {
    value
        .as_deref()
        .and_then(PermissionMode::parse)
        .unwrap_or(default)
}

fn parse_bool(value: Option<&str>) -> bool {
    matches!(
        value.map(str::trim).map(str::to_ascii_lowercase).as_deref(),
        Some("1" | "true" | "yes" | "on")
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

impl TelemetryConfig {
    pub fn from_env_vars(mut var: impl FnMut(&str) -> Option<String>) -> Self {
        let disabled = parse_disabled_bool(var("SQUEEZY_TELEMETRY").as_deref());
        let endpoint = var("SQUEEZY_TELEMETRY_ENDPOINT")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_TELEMETRY_ENDPOINT.to_string());
        Self {
            enabled: !disabled,
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

pub fn default_settings_path() -> PathBuf {
    env::var_os("SQUEEZY_SETTINGS_PATH")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".squeezy/settings.toml"))
        })
        .unwrap_or_else(|| PathBuf::from(".squeezy/settings.toml"))
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
        _ => None,
    }?;
    Some(value.clone())
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptItem {
    pub role: Role,
    pub content: String,
}

impl TranscriptItem {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostSnapshot {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
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
    pub provider: CostSnapshot,
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
        merge_cost_snapshot(&mut self.provider, &turn.provider);
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
    pub provider: CostSnapshot,
}

impl TurnMetrics {
    pub fn record_provider(&mut self, cost: &CostSnapshot) {
        merge_cost_snapshot(&mut self.provider, cost);
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
    match (left, right) {
        (Some(left), Some(right)) => Some(left + right),
        (Some(left), None) => Some(left),
        (None, Some(right)) => Some(right),
        (None, None) => None,
    }
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
    Rust,
    Unsupported,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SymbolKind {
    Crate,
    File,
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

pub const DEFAULT_INSTRUCTIONS: &str = "You are Squeezy, a cost-aware coding agent. Keep responses concise, explicit, and grounded in local evidence. Use websearch for web discovery and webfetch for retrieving a specific URL when web tools are available. Do not invent URLs. If a tool call is denied, do not retry the same call.";

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
