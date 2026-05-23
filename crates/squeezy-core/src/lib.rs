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
pub const DEFAULT_MAX_PARALLEL_TOOLS: usize = 8;
pub const DEFAULT_MAX_TOOL_CALLS_PER_TURN: u64 = 64;
pub const DEFAULT_MAX_TOOL_BYTES_READ_PER_TURN: u64 = 20_000_000;
pub const DEFAULT_MAX_SEARCH_FILES_PER_TURN: u64 = 50_000;
pub const DEFAULT_TICK_RATE_MS: u64 = 50;
pub const DEFAULT_TELEMETRY_ENDPOINT: &str =
    "https://squeezy-telemetry.esqueezy.workers.dev/v1/batch";
pub const PROJECT_SETTINGS_FILE: &str = "squeezy.toml";

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
    pub graph: GraphConfig,
    pub cache: CacheConfig,
    pub tui: TuiConfig,
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
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
                    .unwrap_or_else(|| "ANTHROPIC_API_KEY".to_string()),
                base_url: get_var("ANTHROPIC_BASE_URL")
                    .or_else(|| provider_setting(&providers, "anthropic", "base_url"))
                    .unwrap_or_else(|| DEFAULT_ANTHROPIC_BASE_URL.to_string()),
            }),
            "google" | "gemini" => ProviderConfig::Google(GoogleConfig {
                api_key_env: get_var("GOOGLE_API_KEY_ENV")
                    .or_else(|| provider_setting(&providers, "google", "api_key_env"))
                    .unwrap_or_else(|| "GEMINI_API_KEY".to_string()),
                base_url: get_var("GOOGLE_BASE_URL")
                    .or_else(|| provider_setting(&providers, "google", "base_url"))
                    .unwrap_or_else(|| DEFAULT_GOOGLE_BASE_URL.to_string()),
            }),
            "azure" | "azure-openai" | "azure_openai" => {
                ProviderConfig::AzureOpenAi(AzureOpenAiConfig {
                    api_key_env: get_var("AZURE_OPENAI_API_KEY_ENV")
                        .or_else(|| provider_setting(&providers, "azure_openai", "api_key_env"))
                        .or_else(|| provider_setting(&providers, "azure", "api_key_env"))
                        .unwrap_or_else(|| "AZURE_OPENAI_API_KEY".to_string()),
                    base_url: get_var("AZURE_OPENAI_BASE_URL")
                        .or_else(|| provider_setting(&providers, "azure_openai", "base_url"))
                        .or_else(|| provider_setting(&providers, "azure", "base_url"))
                        .unwrap_or_else(|| DEFAULT_AZURE_OPENAI_BASE_URL.to_string()),
                    api_version: get_var("AZURE_OPENAI_API_VERSION")
                        .or_else(|| provider_setting(&providers, "azure_openai", "api_version"))
                        .or_else(|| provider_setting(&providers, "azure", "api_version"))
                        .unwrap_or_else(|| DEFAULT_AZURE_OPENAI_API_VERSION.to_string()),
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
                })
            }
            "ollama" | "local" => ProviderConfig::Ollama(OllamaConfig {
                base_url: get_var("OLLAMA_BASE_URL")
                    .or_else(|| provider_setting(&providers, "ollama", "base_url"))
                    .unwrap_or_else(|| DEFAULT_OLLAMA_BASE_URL.to_string()),
            }),
            "openai" => ProviderConfig::OpenAi(OpenAiConfig {
                api_key_env: get_var("OPENAI_API_KEY_ENV")
                    .or_else(|| provider_setting(&providers, "openai", "api_key_env"))
                    .unwrap_or_else(|| "OPENAI_API_KEY".to_string()),
                base_url: get_var("OPENAI_BASE_URL")
                    .or_else(|| provider_setting(&providers, "openai", "base_url"))
                    .unwrap_or_else(|| DEFAULT_OPENAI_BASE_URL.to_string()),
            }),
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
        let max_output_tokens = get_var("SQUEEZY_MAX_OUTPUT_TOKENS")
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|value| *value > 0)
            .or(model_settings.max_output_tokens)
            .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
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
        let telemetry = TelemetryConfig::from_settings_and_env(
            settings.telemetry.unwrap_or_default(),
            &mut get_var,
        );
        let permissions = PermissionPolicy::from_settings_and_env(
            settings.permissions.unwrap_or_default(),
            &mut get_var,
        );
        let graph = GraphConfig::from_settings(settings.graph.unwrap_or_default());
        let cache = CacheConfig::from_settings(settings.cache.unwrap_or_default());
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
            instructions: DEFAULT_INSTRUCTIONS.to_string(),
            max_output_tokens: Some(max_output_tokens),
            tick_rate: Duration::from_millis(tui.tick_rate_ms),
            workspace_root: env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            permissions,
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
            graph,
            cache,
            tui,
            mcp_servers: settings.mcp.map(|mcp| mcp.servers).unwrap_or_default(),
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
    /// (`"user"`, `"project"`) for display in narrow status lines. Full
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
    /// (note: `[graph]`, `[tui].status_verbosity`, and `[mcp.servers.*]`
    /// sections currently round-trip into the typed model but no consumer
    /// reads them yet).
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
        output.push_str(&format!(
            "max_output_tokens = {}\n",
            self.max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS)
        ));
        output.push_str(&format!("store_responses = {}\n\n", self.store_responses));

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
            "web = {}\n\n",
            toml_string(self.permissions.web.as_str())
        ));

        output.push_str("[telemetry]\n");
        output.push_str(&format!("enabled = {}\n", self.telemetry.enabled));
        output.push_str(&format!(
            "endpoint = {}\n\n",
            toml_string(&self.telemetry.endpoint)
        ));

        output.push_str("[web]\n");
        output.push_str(&format!(
            "exa_mcp_url = {}\n",
            toml_string(&self.exa_mcp_url)
        ));
        output.push_str("exa_api_key_env = \"<redacted>\"\n\n");

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

        output.push_str("[tui]\n");
        output.push_str(&format!("tick_rate_ms = {}\n", self.tui.tick_rate_ms));
        output.push_str(&format!(
            "status_verbosity = {}\n\n",
            toml_string(self.tui.status_verbosity.as_str())
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
            if !server.env.is_empty() {
                output.push_str("env = \"<redacted>\"\n");
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
    }
}

fn toml_string(value: &str) -> String {
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

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cheap => "cheap",
            Self::Balanced => "balanced",
            Self::Strong => "strong",
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
    pub budgets: Option<BudgetSettings>,
    pub permissions: Option<PermissionSettings>,
    pub telemetry: Option<TelemetrySettings>,
    pub web: Option<WebSettings>,
    pub graph: Option<GraphSettings>,
    pub cache: Option<CacheSettings>,
    pub tui: Option<TuiSettings>,
    pub mcp: Option<McpSettings>,
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
        Ok((
            Self::from_toml_str(&text, &path.display().to_string())?,
            vec![
                "defaults".to_string(),
                format!("{label}:{}", path.display()),
            ],
        ))
    }

    pub fn from_toml_str(text: &str, source: &str) -> Result<Self> {
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        let value = text
            .parse::<toml::Value>()
            .map_err(|err| SqueezyError::Config(format!("{source}: {err}")))?;
        Self::from_toml_value(value, source)
    }

    fn from_toml_value(value: toml::Value, source: &str) -> Result<Self> {
        let table = value.as_table().ok_or_else(|| {
            SqueezyError::Config(format!("{source}: settings root must be a TOML table"))
        })?;
        reject_unknown_keys(
            table,
            &[
                "provider",
                "profile",
                "model",
                "providers",
                "budgets",
                "permissions",
                "telemetry",
                "web",
                "graph",
                "cache",
                "tui",
                "mcp",
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
        settings.budgets = optional_table(table, "budgets", source)?
            .map(|table| BudgetSettings::from_table(table, source, "budgets"))
            .transpose()?;
        settings.permissions = optional_table(table, "permissions", source)?
            .map(|table| PermissionSettings::from_table(table, source, "permissions"))
            .transpose()?;
        settings.telemetry = optional_table(table, "telemetry", source)?
            .map(|table| TelemetrySettings::from_table(table, source, "telemetry"))
            .transpose()?;
        settings.web = optional_table(table, "web", source)?
            .map(|table| WebSettings::from_table(table, source, "web"))
            .transpose()?;
        settings.graph = optional_table(table, "graph", source)?
            .map(|table| GraphSettings::from_table(table, source, "graph"))
            .transpose()?;
        settings.cache = optional_table(table, "cache", source)?
            .map(|table| CacheSettings::from_table(table, source, "cache"))
            .transpose()?;
        settings.tui = optional_table(table, "tui", source)?
            .map(|table| TuiSettings::from_table(table, source, "tui"))
            .transpose()?;
        settings.mcp = optional_table(table, "mcp", source)?
            .map(|table| McpSettings::from_table(table, source, "mcp"))
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
        merge_option(&mut self.web, next.web, WebSettings::merge);
        merge_option(&mut self.graph, next.graph, GraphSettings::merge);
        merge_option(&mut self.cache, next.cache, CacheSettings::merge);
        merge_option(&mut self.tui, next.tui, TuiSettings::merge);
        merge_option(&mut self.mcp, next.mcp, McpSettings::merge);
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
            ],
            source,
            path,
        )?;
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
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.api_key_env, next.api_key_env);
        replace_if_some(&mut self.base_url, next.base_url);
        replace_if_some(&mut self.default_model, next.default_model);
        replace_if_some(&mut self.api_version, next.api_version);
        replace_if_some(&mut self.region, next.region);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ModelSettings {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub profile: Option<String>,
    pub max_output_tokens: Option<u32>,
    pub store_responses: Option<bool>,
}

impl ModelSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "provider",
                "model",
                "profile",
                "max_output_tokens",
                "store_responses",
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
        Ok(Self {
            provider: string_value(table, "provider", source, &field(path, "provider"))?,
            model: string_value(table, "model", source, &field(path, "model"))?,
            profile,
            max_output_tokens: u32_value(
                table,
                "max_output_tokens",
                source,
                &field(path, "max_output_tokens"),
            )?,
            store_responses: bool_value(
                table,
                "store_responses",
                source,
                &field(path, "store_responses"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.provider, next.provider);
        replace_if_some(&mut self.model, next.model);
        replace_if_some(&mut self.profile, next.profile);
        replace_if_some(&mut self.max_output_tokens, next.max_output_tokens);
        replace_if_some(&mut self.store_responses, next.store_responses);
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
    }
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PermissionSettings {
    pub read: Option<PermissionMode>,
    pub edit: Option<PermissionMode>,
    pub shell: Option<PermissionMode>,
    pub ignored_search: Option<PermissionMode>,
    pub web: Option<PermissionMode>,
}

impl PermissionSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &["read", "edit", "shell", "ignored_search", "web"],
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
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.read, next.read);
        replace_if_some(&mut self.edit, next.edit);
        replace_if_some(&mut self.shell, next.shell);
        replace_if_some(&mut self.ignored_search, next.ignored_search);
        replace_if_some(&mut self.web, next.web);
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
        Self::from_settings_and_env(PermissionSettings::default(), &mut var)
    }

    fn from_settings_and_env(
        settings: PermissionSettings,
        mut var: impl FnMut(&str) -> Option<String>,
    ) -> Self {
        Self {
            read: parse_permission(
                var("SQUEEZY_READ_PERMISSION"),
                settings.read.unwrap_or(PermissionMode::Allow),
            ),
            edit: parse_permission(
                var("SQUEEZY_EDIT_PERMISSION"),
                settings.edit.unwrap_or(PermissionMode::Ask),
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphConfig {
    pub languages: Vec<String>,
    pub max_file_bytes: u64,
    pub include_hidden: bool,
    pub require_indexing_signal: bool,
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
pub struct TuiConfig {
    pub tick_rate_ms: u64,
    pub status_verbosity: StatusVerbosity,
}

impl TuiConfig {
    fn from_settings(settings: TuiSettings) -> Self {
        Self {
            tick_rate_ms: settings.tick_rate_ms.unwrap_or(DEFAULT_TICK_RATE_MS),
            status_verbosity: settings
                .status_verbosity
                .unwrap_or(StatusVerbosity::Compact),
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
}

impl TuiSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(table, &["tick_rate_ms", "status_verbosity"], source, path)?;
        Ok(Self {
            tick_rate_ms: u64_value(table, "tick_rate_ms", source, &field(path, "tick_rate_ms"))?,
            status_verbosity: status_verbosity_value(
                table,
                "status_verbosity",
                source,
                &field(path, "status_verbosity"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.tick_rate_ms, next.tick_rate_ms);
        replace_if_some(&mut self.status_verbosity, next.status_verbosity);
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
# Values shown after `=` are the built-in defaults that apply when the
# key is absent or commented out.

[model]
# provider = "openai"          # openai | anthropic | google | azure_openai | bedrock | ollama
# profile = "balanced"         # cheap | balanced | strong
# model = "gpt-5-nano"         # provider-specific model id; leave unset to use the provider default
# max_output_tokens = 128
# store_responses = false      # only honored by openai/azure_openai

# [providers.openai]
# api_key_env = "OPENAI_API_KEY"
# base_url = "https://api.openai.com/v1"
# default_model = "gpt-5-nano"

# [providers.anthropic]
# api_key_env = "ANTHROPIC_API_KEY"
# base_url = "https://api.anthropic.com/v1"
# default_model = "claude-3-5-haiku-20241022"

[permissions]
# read = "allow"
# edit = "ask"
# shell = "ask"
# ignored_search = "allow"
# web = "ask"

[telemetry]
# enabled = true

# [web]
# exa_mcp_url = "https://mcp.exa.ai/mcp"
# exa_api_key_env = "EXA_API_KEY"
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

# `[graph]`, `[tui].status_verbosity`, and `[mcp.servers.*]` are parsed and
# round-trip through `squeezy config inspect` but no runtime consumer reads
# them yet; treat them as v0 reservations.

# [graph]
# languages = ["rust", "python"]
# max_file_bytes = 1000000
# include_hidden = false
# require_indexing_signal = true

[cache]
# Relative paths are resolved against the project root (the directory
# containing this squeezy.toml).
# tool_outputs = ".squeezy/tool_outputs"

[tui]
# tick_rate_ms = 50
# status_verbosity = "compact"   # not wired yet
"#
}

fn load_default_settings_sources() -> Result<(SettingsFile, Vec<String>)> {
    let user_path = default_settings_path();
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let project_path = find_project_settings_path(cwd);
    load_settings_from_paths(Some(user_path.as_path()), project_path.as_deref())
}

fn load_settings_from_paths(
    user_path: Option<&Path>,
    project_path: Option<&Path>,
) -> Result<(SettingsFile, Vec<String>)> {
    let mut settings = SettingsFile::default();
    let mut sources = vec!["defaults".to_string()];
    if let Some(user_path) = user_path
        && user_path.is_file()
    {
        let user = SettingsFile::from_toml_str(
            &fs::read_to_string(user_path)?,
            &user_path.display().to_string(),
        )?;
        settings.merge(user);
        sources.push(format!("user:{}", user_path.display()));
    }
    if let Some(project_path) = project_path
        && project_path.is_file()
    {
        let project = SettingsFile::from_toml_str(
            &fs::read_to_string(project_path)?,
            &project_path.display().to_string(),
        )?;
        settings.merge(project);
        sources.push(format!("project:{}", project_path.display()));
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
        _ => None,
    }?;
    Some(value.clone())
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
    pub env: BTreeMap<String, String>,
}

impl McpServerConfig {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "enabled",
                "transport",
                "command",
                "args",
                "url",
                "timeout_ms",
                "env",
            ],
            source,
            path,
        )?;
        let transport = mcp_transport_value(table, "transport", source, &field(path, "transport"))?
            .unwrap_or(McpTransport::Stdio);
        let env = string_map_value(table, "env", source, &field(path, "env"))?.unwrap_or_default();
        Ok(Self {
            enabled: bool_value(table, "enabled", source, &field(path, "enabled"))?.unwrap_or(true),
            transport,
            command: string_value(table, "command", source, &field(path, "command"))?,
            args: string_array_value(table, "args", source, &field(path, "args"))?
                .unwrap_or_default(),
            url: string_value(table, "url", source, &field(path, "url"))?,
            timeout_ms: u64_value(table, "timeout_ms", source, &field(path, "timeout_ms"))?,
            env,
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
        if !next.env.is_empty() {
            self.env.extend(next.env);
        }
    }
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

fn reject_unknown_keys(
    table: &toml::value::Table,
    allowed: &[&str],
    source: &str,
    path: &str,
) -> Result<()> {
    for key in table.keys() {
        if !allowed.iter().any(|allowed| key == allowed) {
            let field_path = field(path, key);
            return Err(SqueezyError::Config(format!(
                "{source}: {field_path}: unknown field"
            )));
        }
    }
    Ok(())
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

fn path_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<PathBuf>> {
    Ok(string_value(table, key, source, path)?.map(PathBuf::from))
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
pub enum SymbolKind {
    Class,
    Crate,
    File,
    Module,
    Interface,
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
