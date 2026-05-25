//! Declarative metadata for every editable field in `AppConfig`.
//!
//! `CONFIG_SECTIONS` is the single source of truth shared by the TUI config
//! screen and the TOML writer: both walk the same list, so the screen cannot
//! show a field the writer doesn't know how to persist (and vice versa).
//!
//! New sections are added by appending a `ConfigSectionMeta` entry below.

use std::time::Duration;

use crate::{
    AppConfig, DEFAULT_ANTHROPIC_MODEL, DEFAULT_AZURE_OPENAI_MODEL, DEFAULT_BEDROCK_MODEL,
    DEFAULT_GOOGLE_MODEL, DEFAULT_OLLAMA_MODEL, DEFAULT_OPENAI_MODEL, DEFAULT_TELEMETRY_ENDPOINT,
    PermissionMode, ProviderConfig, ReasoningEffort, ResponseVerbosity, StatusVerbosity,
    ToolOutputVerbosity, TranscriptDefault, TuiAlternateScreen,
};

/// When a save takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyTier {
    /// Applies immediately to the running process. Consumed per-render or
    /// per-tool-call: verbosity, permissions, theme bits.
    Immediate,
    /// Applies on the next user prompt. The in-flight turn (if any) finishes
    /// on the old config. The agent's pending swap is drained at the top of
    /// `start_turn`: model, provider, MCP servers, anything baked into the
    /// LLM client.
    NextPrompt,
    /// Cannot be swapped mid-process. The screen writes the TOML but surfaces
    /// a "restart required" notification: log dirs, graph indexer, alternate
    /// screen mode.
    Restart,
}

impl ApplyTier {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Immediate => "immediate",
            Self::NextPrompt => "next prompt",
            Self::Restart => "restart required",
        }
    }
}

/// Where an effective value came from, used to render the inheritance badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldSource {
    Default,
    User,
    Project,
    Repo,
    Env,
}

impl FieldSource {
    pub const fn badge(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::User => "user",
            Self::Project => "project",
            Self::Repo => "repo",
            Self::Env => "env",
        }
    }
}

/// The editor shape for a field. Drives which widget the UI renders.
#[derive(Debug, Clone, Copy)]
pub enum FieldKind {
    Bool,
    Integer {
        min: i64,
        max: i64,
        suffix: Option<&'static str>,
    },
    OptionalInteger {
        min: i64,
        max: i64,
        suffix: Option<&'static str>,
    },
    Enum {
        options: &'static [&'static str],
    },
    OptionalEnum {
        options: &'static [&'static str],
    },
    String {
        multiline: bool,
    },
    /// `<name>_ms` u64 in TOML, rendered as a duration.
    DurationMs,
}

/// Concrete value carried through reads, writes, and editor commits.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldValue {
    Bool(bool),
    Integer(i64),
    OptionalInteger(Option<i64>),
    Enum(&'static str),
    OptionalEnum(Option<&'static str>),
    String(String),
    Duration(Duration),
    Unset,
}

impl FieldValue {
    pub fn as_display(&self) -> String {
        match self {
            Self::Bool(v) => v.to_string(),
            Self::Integer(v) => v.to_string(),
            Self::OptionalInteger(Some(v)) => v.to_string(),
            Self::OptionalInteger(None) => "—".to_string(),
            Self::Enum(v) => (*v).to_string(),
            Self::OptionalEnum(Some(v)) => (*v).to_string(),
            Self::OptionalEnum(None) => "—".to_string(),
            Self::String(s) => s.clone(),
            Self::Duration(d) => format!("{} ms", d.as_millis()),
            Self::Unset => "—".to_string(),
        }
    }
}

/// Ordered TOML path. e.g. `["model", "provider"]` or `["tui", "tick_rate_ms"]`.
pub type SettingsPath = &'static [&'static str];

/// Identity for a section, used by the slash router (`/model` → `Models`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SectionId {
    Models,
    Permissions,
    Verbosity,
    Limits,
    Telemetry,
}

impl SectionId {
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Models => "models",
            Self::Permissions => "permissions",
            Self::Verbosity => "verbosity",
            Self::Limits => "limits",
            Self::Telemetry => "telemetry",
        }
    }
}

/// Metadata for one field. Getters/setters operate on a borrowed `AppConfig`.
pub struct FieldMeta {
    pub label: &'static str,
    pub toml_path: SettingsPath,
    pub kind: FieldKind,
    pub tier: ApplyTier,
    pub get: fn(&AppConfig) -> FieldValue,
    pub set: fn(&mut AppConfig, FieldValue) -> Result<(), &'static str>,
    pub default_display: &'static str,
    pub help: &'static str,
}

pub struct ConfigSectionMeta {
    pub id: SectionId,
    pub label: &'static str,
    pub description: &'static str,
    pub fields: &'static [FieldMeta],
}

pub const PROVIDER_OPTIONS: &[&str] = &[
    "openai",
    "anthropic",
    "google",
    "azure_openai",
    "bedrock",
    "ollama",
];

pub const PROFILE_OPTIONS: &[&str] = &["cheap", "balanced", "strong"];
pub const REASONING_EFFORT_OPTIONS: &[&str] = &["low", "medium", "high", "xhigh"];
pub const SESSION_MODE_OPTIONS: &[&str] = &["build", "plan"];
pub const STATUS_VERBOSITY_OPTIONS: &[&str] = &["compact", "verbose"];
pub const RESPONSE_VERBOSITY_OPTIONS: &[&str] = &["concise", "normal", "verbose"];
pub const TOOL_OUTPUT_VERBOSITY_OPTIONS: &[&str] = &["compact", "normal", "verbose"];
pub const TRANSCRIPT_DEFAULT_OPTIONS: &[&str] = &["compact", "expanded"];
pub const ALTERNATE_SCREEN_OPTIONS: &[&str] = &["auto", "never", "always"];
pub const PERMISSION_MODE_OPTIONS: &[&str] = &["allow", "ask", "deny"];

pub const CONFIG_SECTIONS: &[ConfigSectionMeta] = &[
    ConfigSectionMeta {
        id: SectionId::Models,
        label: "Models",
        description: "Provider and model selection",
        fields: &[
            FieldMeta {
                label: "provider",
                toml_path: &["model", "provider"],
                kind: FieldKind::Enum {
                    options: PROVIDER_OPTIONS,
                },
                tier: ApplyTier::NextPrompt,
                get: get_provider,
                set: set_provider,
                default_display: "openai",
                help: "Which LLM provider to use. Switching also resets the model to that provider's default unless you set one explicitly.",
            },
            FieldMeta {
                label: "model",
                toml_path: &["model", "model"],
                kind: FieldKind::String { multiline: false },
                tier: ApplyTier::NextPrompt,
                get: get_model,
                set: set_model,
                default_display: DEFAULT_OPENAI_MODEL,
                help: "Provider-specific model identifier.",
            },
            FieldMeta {
                label: "profile",
                toml_path: &["model", "profile"],
                kind: FieldKind::Enum {
                    options: PROFILE_OPTIONS,
                },
                tier: ApplyTier::NextPrompt,
                get: get_profile,
                set: set_profile,
                default_display: "balanced",
                help: "Default cost/capability profile when model is unset.",
            },
            FieldMeta {
                label: "reasoning_effort",
                toml_path: &["model", "reasoning_effort"],
                kind: FieldKind::OptionalEnum {
                    options: REASONING_EFFORT_OPTIONS,
                },
                tier: ApplyTier::NextPrompt,
                get: get_reasoning_effort,
                set: set_reasoning_effort,
                default_display: "—",
                help: "Reasoning effort hint. Only meaningful for reasoning-capable models.",
            },
            FieldMeta {
                label: "max_output_tokens",
                toml_path: &["model", "max_output_tokens"],
                kind: FieldKind::OptionalInteger {
                    min: 1,
                    max: 1_000_000,
                    suffix: Some("tokens"),
                },
                tier: ApplyTier::NextPrompt,
                get: get_max_output_tokens,
                set: set_max_output_tokens,
                default_display: "—",
                help: "Cap on output tokens per request. Unset means provider default.",
            },
            FieldMeta {
                label: "stream_idle_timeout",
                toml_path: &["model", "stream_idle_timeout_ms"],
                kind: FieldKind::DurationMs,
                tier: ApplyTier::NextPrompt,
                get: get_stream_idle_timeout,
                set: set_stream_idle_timeout,
                default_display: "300000 ms",
                help: "Abort if no streaming bytes arrive for this duration.",
            },
            FieldMeta {
                label: "store_responses",
                toml_path: &["model", "store_responses"],
                kind: FieldKind::Bool,
                tier: ApplyTier::NextPrompt,
                get: get_store_responses,
                set: set_store_responses,
                default_display: "false",
                help: "(OpenAI/Azure only) Persist responses on the provider side for retrieval.",
            },
        ],
    },
    ConfigSectionMeta {
        id: SectionId::Permissions,
        label: "Permissions",
        description: "Default action for each capability",
        fields: &[
            FieldMeta {
                label: "read",
                toml_path: &["permissions", "read"],
                kind: FieldKind::Enum {
                    options: PERMISSION_MODE_OPTIONS,
                },
                tier: ApplyTier::Immediate,
                get: get_perm_read,
                set: set_perm_read,
                default_display: "allow",
                help: "Default for file reads.",
            },
            FieldMeta {
                label: "edit",
                toml_path: &["permissions", "edit"],
                kind: FieldKind::Enum {
                    options: PERMISSION_MODE_OPTIONS,
                },
                tier: ApplyTier::Immediate,
                get: get_perm_edit,
                set: set_perm_edit,
                default_display: "ask",
                help: "Default for file edits and writes.",
            },
            FieldMeta {
                label: "shell",
                toml_path: &["permissions", "shell"],
                kind: FieldKind::Enum {
                    options: PERMISSION_MODE_OPTIONS,
                },
                tier: ApplyTier::Immediate,
                get: get_perm_shell,
                set: set_perm_shell,
                default_display: "ask",
                help: "Default for shell command execution.",
            },
            FieldMeta {
                label: "ignored_search",
                toml_path: &["permissions", "ignored_search"],
                kind: FieldKind::Enum {
                    options: PERMISSION_MODE_OPTIONS,
                },
                tier: ApplyTier::Immediate,
                get: get_perm_ignored_search,
                set: set_perm_ignored_search,
                default_display: "ask",
                help: "Default for searches that escape .gitignore boundaries.",
            },
            FieldMeta {
                label: "web",
                toml_path: &["permissions", "web"],
                kind: FieldKind::Enum {
                    options: PERMISSION_MODE_OPTIONS,
                },
                tier: ApplyTier::Immediate,
                get: get_perm_web,
                set: set_perm_web,
                default_display: "ask",
                help: "Default for web fetches and searches.",
            },
            FieldMeta {
                label: "mcp",
                toml_path: &["permissions", "mcp"],
                kind: FieldKind::Enum {
                    options: PERMISSION_MODE_OPTIONS,
                },
                tier: ApplyTier::Immediate,
                get: get_perm_mcp,
                set: set_perm_mcp,
                default_display: "ask",
                help: "Default for MCP tool invocations.",
            },
        ],
    },
    ConfigSectionMeta {
        id: SectionId::Verbosity,
        label: "Verbosity & TUI",
        description: "Terminal UI output detail and behavior",
        fields: &[
            FieldMeta {
                label: "response_verbosity",
                toml_path: &["tui", "response_verbosity"],
                kind: FieldKind::Enum {
                    options: RESPONSE_VERBOSITY_OPTIONS,
                },
                tier: ApplyTier::Immediate,
                get: get_response_verbosity,
                set: set_response_verbosity,
                default_display: "normal",
                help: "How chatty the assistant's prose answers are.",
            },
            FieldMeta {
                label: "tool_output_verbosity",
                toml_path: &["tui", "tool_output_verbosity"],
                kind: FieldKind::Enum {
                    options: TOOL_OUTPUT_VERBOSITY_OPTIONS,
                },
                tier: ApplyTier::Immediate,
                get: get_tool_output_verbosity,
                set: set_tool_output_verbosity,
                default_display: "compact",
                help: "How much tool output is shown inline.",
            },
            FieldMeta {
                label: "status_verbosity",
                toml_path: &["tui", "status_verbosity"],
                kind: FieldKind::Enum {
                    options: STATUS_VERBOSITY_OPTIONS,
                },
                tier: ApplyTier::Immediate,
                get: get_status_verbosity,
                set: set_status_verbosity,
                default_display: "compact",
                help: "How much detail the bottom status bar shows.",
            },
            FieldMeta {
                label: "transcript_default",
                toml_path: &["tui", "transcript_default"],
                kind: FieldKind::Enum {
                    options: TRANSCRIPT_DEFAULT_OPTIONS,
                },
                tier: ApplyTier::Immediate,
                get: get_transcript_default,
                set: set_transcript_default,
                default_display: "compact",
                help: "Whether new transcript entries start collapsed or expanded.",
            },
            FieldMeta {
                label: "show_reasoning_usage",
                toml_path: &["tui", "show_reasoning_usage"],
                kind: FieldKind::Bool,
                tier: ApplyTier::Immediate,
                get: get_show_reasoning_usage,
                set: set_show_reasoning_usage,
                default_display: "true",
                help: "Show reasoning-token usage alongside completion tokens.",
            },
            FieldMeta {
                label: "alternate_screen",
                toml_path: &["tui", "alternate_screen"],
                kind: FieldKind::Enum {
                    options: ALTERNATE_SCREEN_OPTIONS,
                },
                tier: ApplyTier::Restart,
                get: get_alternate_screen,
                set: set_alternate_screen,
                default_display: "auto",
                help: "Whether to take over the terminal screen on launch.",
            },
            FieldMeta {
                label: "tick_rate",
                toml_path: &["tui", "tick_rate_ms"],
                kind: FieldKind::Integer {
                    min: 10,
                    max: 1000,
                    suffix: Some("ms"),
                },
                tier: ApplyTier::Restart,
                get: get_tick_rate,
                set: set_tick_rate,
                default_display: "50 ms",
                help: "Frame interval for animations.",
            },
        ],
    },
    ConfigSectionMeta {
        id: SectionId::Limits,
        label: "Limits & Costs",
        description: "Per-turn and per-session budgets",
        fields: &[
            FieldMeta {
                label: "max_parallel_tools",
                toml_path: &["budgets", "max_parallel_tools"],
                kind: FieldKind::Integer {
                    min: 1,
                    max: 64,
                    suffix: None,
                },
                tier: ApplyTier::NextPrompt,
                get: get_max_parallel_tools,
                set: set_max_parallel_tools,
                default_display: "8",
                help: "Maximum tool calls executed concurrently per turn.",
            },
            FieldMeta {
                label: "max_tool_calls_per_turn",
                toml_path: &["budgets", "max_tool_calls_per_turn"],
                kind: FieldKind::Integer {
                    min: 1,
                    max: 4096,
                    suffix: None,
                },
                tier: ApplyTier::Immediate,
                get: get_max_tool_calls_per_turn,
                set: set_max_tool_calls_per_turn,
                default_display: "64",
                help: "Stop the turn after this many tool calls.",
            },
            FieldMeta {
                label: "max_tool_bytes_read_per_turn",
                toml_path: &["budgets", "max_tool_bytes_read_per_turn"],
                kind: FieldKind::Integer {
                    min: 1024,
                    max: 1_000_000_000,
                    suffix: Some("bytes"),
                },
                tier: ApplyTier::Immediate,
                get: get_max_tool_bytes_read_per_turn,
                set: set_max_tool_bytes_read_per_turn,
                default_display: "20000000 bytes",
                help: "Aggregate read budget across all tools per turn.",
            },
            FieldMeta {
                label: "max_search_files_per_turn",
                toml_path: &["budgets", "max_search_files_per_turn"],
                kind: FieldKind::Integer {
                    min: 100,
                    max: 10_000_000,
                    suffix: Some("files"),
                },
                tier: ApplyTier::Immediate,
                get: get_max_search_files_per_turn,
                set: set_max_search_files_per_turn,
                default_display: "50000 files",
                help: "Files scanned across all search tools per turn.",
            },
            FieldMeta {
                label: "cost_warn_percent",
                toml_path: &["budgets", "cost_warn_percent"],
                kind: FieldKind::Integer {
                    min: 1,
                    max: 100,
                    suffix: Some("%"),
                },
                tier: ApplyTier::Immediate,
                get: get_cost_warn_percent,
                set: set_cost_warn_percent,
                default_display: "85 %",
                help: "Warn when session cost crosses this percentage of the cap.",
            },
            FieldMeta {
                label: "max_session_cost_usd_micros",
                toml_path: &["budgets", "max_session_cost_usd_micros"],
                kind: FieldKind::OptionalInteger {
                    min: 1,
                    max: 1_000_000_000_000,
                    suffix: Some("μUSD"),
                },
                tier: ApplyTier::Immediate,
                get: get_max_session_cost_usd_micros,
                set: set_max_session_cost_usd_micros,
                default_display: "—",
                help: "Hard cap on session cost in micro-dollars. Unset means no cap.",
            },
        ],
    },
    ConfigSectionMeta {
        id: SectionId::Telemetry,
        label: "Telemetry",
        description: "Anonymous usage reporting",
        fields: &[
            FieldMeta {
                label: "enabled",
                toml_path: &["telemetry", "enabled"],
                kind: FieldKind::Bool,
                tier: ApplyTier::Immediate,
                get: get_telemetry_enabled,
                set: set_telemetry_enabled,
                default_display: "true",
                help: "Send anonymous usage events.",
            },
            FieldMeta {
                label: "endpoint",
                toml_path: &["telemetry", "endpoint"],
                kind: FieldKind::String { multiline: false },
                tier: ApplyTier::NextPrompt,
                get: get_telemetry_endpoint,
                set: set_telemetry_endpoint,
                default_display: DEFAULT_TELEMETRY_ENDPOINT,
                help: "Where telemetry events are POSTed.",
            },
        ],
    },
];

// ─── getters / setters ────────────────────────────────────────────────────────

fn get_provider(cfg: &AppConfig) -> FieldValue {
    FieldValue::Enum(provider_to_str(&cfg.provider))
}

fn set_provider(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let s = match value {
        FieldValue::Enum(s) => s,
        _ => return Err("provider expects enum"),
    };
    use crate::{
        AnthropicConfig, AzureOpenAiConfig, BedrockConfig, DEFAULT_ANTHROPIC_BASE_URL,
        DEFAULT_AZURE_OPENAI_API_VERSION, DEFAULT_AZURE_OPENAI_BASE_URL, DEFAULT_BEDROCK_REGION,
        DEFAULT_GOOGLE_BASE_URL, DEFAULT_OLLAMA_BASE_URL, DEFAULT_OPENAI_BASE_URL, GoogleConfig,
        OllamaConfig, OpenAiConfig, ProviderTransportConfig,
    };
    let transport = ProviderTransportConfig::default();
    cfg.provider = match s {
        "openai" => ProviderConfig::OpenAi(OpenAiConfig {
            api_key_env: "OPENAI_API_KEY".to_string(),
            base_url: DEFAULT_OPENAI_BASE_URL.to_string(),
            transport,
        }),
        "anthropic" => ProviderConfig::Anthropic(AnthropicConfig {
            api_key_env: "ANTHROPIC_API_KEY".to_string(),
            base_url: DEFAULT_ANTHROPIC_BASE_URL.to_string(),
            transport,
        }),
        "google" => ProviderConfig::Google(GoogleConfig {
            api_key_env: "GOOGLE_API_KEY".to_string(),
            base_url: DEFAULT_GOOGLE_BASE_URL.to_string(),
            transport,
        }),
        "azure_openai" => ProviderConfig::AzureOpenAi(AzureOpenAiConfig {
            api_key_env: "AZURE_OPENAI_API_KEY".to_string(),
            base_url: DEFAULT_AZURE_OPENAI_BASE_URL.to_string(),
            api_version: DEFAULT_AZURE_OPENAI_API_VERSION.to_string(),
            transport,
        }),
        "bedrock" => ProviderConfig::Bedrock(BedrockConfig {
            region: DEFAULT_BEDROCK_REGION.to_string(),
            base_url: None,
            transport,
        }),
        "ollama" => ProviderConfig::Ollama(OllamaConfig {
            base_url: DEFAULT_OLLAMA_BASE_URL.to_string(),
            transport,
        }),
        _ => return Err("unknown provider"),
    };
    cfg.model = default_model_for(s).to_string();
    Ok(())
}

fn provider_to_str(p: &ProviderConfig) -> &'static str {
    match p {
        ProviderConfig::OpenAi(_) => "openai",
        ProviderConfig::Anthropic(_) => "anthropic",
        ProviderConfig::Google(_) => "google",
        ProviderConfig::AzureOpenAi(_) => "azure_openai",
        ProviderConfig::Bedrock(_) => "bedrock",
        ProviderConfig::Ollama(_) => "ollama",
    }
}

pub fn default_model_for(provider: &str) -> &'static str {
    match provider {
        "openai" => DEFAULT_OPENAI_MODEL,
        "anthropic" => DEFAULT_ANTHROPIC_MODEL,
        "google" => DEFAULT_GOOGLE_MODEL,
        "azure_openai" => DEFAULT_AZURE_OPENAI_MODEL,
        "bedrock" => DEFAULT_BEDROCK_MODEL,
        "ollama" => DEFAULT_OLLAMA_MODEL,
        _ => DEFAULT_OPENAI_MODEL,
    }
}

fn get_model(cfg: &AppConfig) -> FieldValue {
    FieldValue::String(cfg.model.clone())
}
fn set_model(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::String(s) if !s.trim().is_empty() => {
            cfg.model = s;
            Ok(())
        }
        FieldValue::String(_) => Err("model cannot be empty"),
        _ => Err("model expects string"),
    }
}

fn get_profile(cfg: &AppConfig) -> FieldValue {
    FieldValue::Enum(cfg.profile.as_str())
}
fn set_profile(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    use crate::ModelProfile;
    let s = match value {
        FieldValue::Enum(s) => s,
        _ => return Err("profile expects enum"),
    };
    cfg.profile = ModelProfile::parse(s).ok_or("invalid profile")?;
    Ok(())
}

fn get_reasoning_effort(cfg: &AppConfig) -> FieldValue {
    FieldValue::OptionalEnum(cfg.reasoning_effort.map(|r| r.as_str()))
}
fn set_reasoning_effort(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    cfg.reasoning_effort = match value {
        FieldValue::OptionalEnum(None) | FieldValue::Unset => None,
        FieldValue::OptionalEnum(Some(s)) | FieldValue::Enum(s) => {
            Some(ReasoningEffort::parse(s).ok_or("invalid reasoning_effort")?)
        }
        _ => return Err("reasoning_effort expects enum"),
    };
    Ok(())
}

fn get_max_output_tokens(cfg: &AppConfig) -> FieldValue {
    FieldValue::OptionalInteger(cfg.max_output_tokens.map(|v| v as i64))
}
fn set_max_output_tokens(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    cfg.max_output_tokens = match value {
        FieldValue::OptionalInteger(None) | FieldValue::Unset => None,
        FieldValue::OptionalInteger(Some(v)) | FieldValue::Integer(v) => {
            if v < 1 {
                return Err("must be >= 1");
            }
            Some(v as u32)
        }
        _ => return Err("max_output_tokens expects integer"),
    };
    Ok(())
}

fn get_stream_idle_timeout(cfg: &AppConfig) -> FieldValue {
    FieldValue::Duration(cfg.stream_idle_timeout)
}
fn set_stream_idle_timeout(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    cfg.stream_idle_timeout = match value {
        FieldValue::Duration(d) => d,
        FieldValue::Integer(v) if v >= 0 => Duration::from_millis(v as u64),
        _ => return Err("stream_idle_timeout expects duration in ms"),
    };
    Ok(())
}

fn get_store_responses(cfg: &AppConfig) -> FieldValue {
    FieldValue::Bool(cfg.store_responses)
}
fn set_store_responses(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::Bool(v) => {
            cfg.store_responses = v;
            Ok(())
        }
        _ => Err("store_responses expects bool"),
    }
}

// Permissions

fn get_perm_read(cfg: &AppConfig) -> FieldValue {
    FieldValue::Enum(cfg.permissions.read.as_str())
}
fn set_perm_read(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    set_perm(value, &mut cfg.permissions.read)
}
fn get_perm_edit(cfg: &AppConfig) -> FieldValue {
    FieldValue::Enum(cfg.permissions.edit.as_str())
}
fn set_perm_edit(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    set_perm(value, &mut cfg.permissions.edit)
}
fn get_perm_shell(cfg: &AppConfig) -> FieldValue {
    FieldValue::Enum(cfg.permissions.shell.as_str())
}
fn set_perm_shell(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    set_perm(value, &mut cfg.permissions.shell)
}
fn get_perm_ignored_search(cfg: &AppConfig) -> FieldValue {
    FieldValue::Enum(cfg.permissions.ignored_search.as_str())
}
fn set_perm_ignored_search(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    set_perm(value, &mut cfg.permissions.ignored_search)
}
fn get_perm_web(cfg: &AppConfig) -> FieldValue {
    FieldValue::Enum(cfg.permissions.web.as_str())
}
fn set_perm_web(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    set_perm(value, &mut cfg.permissions.web)
}
fn get_perm_mcp(cfg: &AppConfig) -> FieldValue {
    FieldValue::Enum(cfg.permissions.mcp.as_str())
}
fn set_perm_mcp(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    set_perm(value, &mut cfg.permissions.mcp)
}

fn set_perm(value: FieldValue, slot: &mut PermissionMode) -> Result<(), &'static str> {
    let s = match value {
        FieldValue::Enum(s) => s,
        _ => return Err("permission expects enum"),
    };
    *slot = PermissionMode::parse(s).ok_or("invalid permission mode")?;
    Ok(())
}

// TUI / verbosity

fn get_response_verbosity(cfg: &AppConfig) -> FieldValue {
    FieldValue::Enum(cfg.tui.response_verbosity.as_str())
}
fn set_response_verbosity(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let s = match value {
        FieldValue::Enum(s) => s,
        _ => return Err("expects enum"),
    };
    cfg.tui.response_verbosity = match s {
        "concise" => ResponseVerbosity::Concise,
        "normal" => ResponseVerbosity::Normal,
        "verbose" => ResponseVerbosity::Verbose,
        _ => return Err("invalid response_verbosity"),
    };
    Ok(())
}

fn get_tool_output_verbosity(cfg: &AppConfig) -> FieldValue {
    FieldValue::Enum(cfg.tui.tool_output_verbosity.as_str())
}
fn set_tool_output_verbosity(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let s = match value {
        FieldValue::Enum(s) => s,
        _ => return Err("expects enum"),
    };
    cfg.tui.tool_output_verbosity = match s {
        "compact" => ToolOutputVerbosity::Compact,
        "normal" => ToolOutputVerbosity::Normal,
        "verbose" => ToolOutputVerbosity::Verbose,
        _ => return Err("invalid tool_output_verbosity"),
    };
    Ok(())
}

fn get_status_verbosity(cfg: &AppConfig) -> FieldValue {
    FieldValue::Enum(cfg.tui.status_verbosity.as_str())
}
fn set_status_verbosity(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let s = match value {
        FieldValue::Enum(s) => s,
        _ => return Err("expects enum"),
    };
    cfg.tui.status_verbosity = match s {
        "compact" => StatusVerbosity::Compact,
        "verbose" => StatusVerbosity::Verbose,
        _ => return Err("invalid status_verbosity"),
    };
    Ok(())
}

fn get_transcript_default(cfg: &AppConfig) -> FieldValue {
    FieldValue::Enum(cfg.tui.transcript_default.as_str())
}
fn set_transcript_default(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let s = match value {
        FieldValue::Enum(s) => s,
        _ => return Err("expects enum"),
    };
    cfg.tui.transcript_default = match s {
        "compact" => TranscriptDefault::Compact,
        "expanded" => TranscriptDefault::Expanded,
        _ => return Err("invalid transcript_default"),
    };
    Ok(())
}

fn get_show_reasoning_usage(cfg: &AppConfig) -> FieldValue {
    FieldValue::Bool(cfg.tui.show_reasoning_usage)
}
fn set_show_reasoning_usage(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::Bool(v) => {
            cfg.tui.show_reasoning_usage = v;
            Ok(())
        }
        _ => Err("expects bool"),
    }
}

fn get_alternate_screen(cfg: &AppConfig) -> FieldValue {
    FieldValue::Enum(cfg.tui.alternate_screen.as_str())
}
fn set_alternate_screen(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let s = match value {
        FieldValue::Enum(s) => s,
        _ => return Err("expects enum"),
    };
    cfg.tui.alternate_screen = match s {
        "auto" => TuiAlternateScreen::Auto,
        "never" => TuiAlternateScreen::Never,
        "always" => TuiAlternateScreen::Always,
        _ => return Err("invalid alternate_screen"),
    };
    Ok(())
}

fn get_tick_rate(cfg: &AppConfig) -> FieldValue {
    FieldValue::Integer(cfg.tui.tick_rate_ms as i64)
}
fn set_tick_rate(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let v = match value {
        FieldValue::Integer(v) => v,
        _ => return Err("expects integer"),
    };
    if !(10..=1000).contains(&v) {
        return Err("tick_rate_ms must be 10..=1000");
    }
    cfg.tui.tick_rate_ms = v as u64;
    Ok(())
}

// Limits

fn get_max_parallel_tools(cfg: &AppConfig) -> FieldValue {
    FieldValue::Integer(cfg.max_parallel_tools as i64)
}
fn set_max_parallel_tools(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let v = match value {
        FieldValue::Integer(v) => v,
        _ => return Err("expects integer"),
    };
    if v < 1 {
        return Err("must be >= 1");
    }
    cfg.max_parallel_tools = v as usize;
    Ok(())
}

fn get_max_tool_calls_per_turn(cfg: &AppConfig) -> FieldValue {
    FieldValue::Integer(cfg.max_tool_calls_per_turn as i64)
}
fn set_max_tool_calls_per_turn(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let v = match value {
        FieldValue::Integer(v) => v,
        _ => return Err("expects integer"),
    };
    if v < 1 {
        return Err("must be >= 1");
    }
    cfg.max_tool_calls_per_turn = v as u64;
    Ok(())
}

fn get_max_tool_bytes_read_per_turn(cfg: &AppConfig) -> FieldValue {
    FieldValue::Integer(cfg.max_tool_bytes_read_per_turn as i64)
}
fn set_max_tool_bytes_read_per_turn(
    cfg: &mut AppConfig,
    value: FieldValue,
) -> Result<(), &'static str> {
    let v = match value {
        FieldValue::Integer(v) => v,
        _ => return Err("expects integer"),
    };
    if v < 1024 {
        return Err("must be >= 1024");
    }
    cfg.max_tool_bytes_read_per_turn = v as u64;
    Ok(())
}

fn get_max_search_files_per_turn(cfg: &AppConfig) -> FieldValue {
    FieldValue::Integer(cfg.max_search_files_per_turn as i64)
}
fn set_max_search_files_per_turn(
    cfg: &mut AppConfig,
    value: FieldValue,
) -> Result<(), &'static str> {
    let v = match value {
        FieldValue::Integer(v) => v,
        _ => return Err("expects integer"),
    };
    if v < 100 {
        return Err("must be >= 100");
    }
    cfg.max_search_files_per_turn = v as u64;
    Ok(())
}

fn get_cost_warn_percent(cfg: &AppConfig) -> FieldValue {
    FieldValue::Integer(cfg.cost_warn_percent as i64)
}
fn set_cost_warn_percent(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let v = match value {
        FieldValue::Integer(v) => v,
        _ => return Err("expects integer"),
    };
    if !(1..=100).contains(&v) {
        return Err("must be 1..=100");
    }
    cfg.cost_warn_percent = v as u8;
    Ok(())
}

fn get_max_session_cost_usd_micros(cfg: &AppConfig) -> FieldValue {
    FieldValue::OptionalInteger(cfg.max_session_cost_usd_micros.map(|v| v as i64))
}
fn set_max_session_cost_usd_micros(
    cfg: &mut AppConfig,
    value: FieldValue,
) -> Result<(), &'static str> {
    cfg.max_session_cost_usd_micros = match value {
        FieldValue::OptionalInteger(None) | FieldValue::Unset => None,
        FieldValue::OptionalInteger(Some(v)) | FieldValue::Integer(v) => {
            if v < 1 {
                return Err("must be >= 1");
            }
            Some(v as u64)
        }
        _ => return Err("expects integer"),
    };
    Ok(())
}

// Telemetry

fn get_telemetry_enabled(cfg: &AppConfig) -> FieldValue {
    FieldValue::Bool(cfg.telemetry.enabled)
}
fn set_telemetry_enabled(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::Bool(v) => {
            cfg.telemetry.enabled = v;
            Ok(())
        }
        _ => Err("expects bool"),
    }
}

fn get_telemetry_endpoint(cfg: &AppConfig) -> FieldValue {
    FieldValue::String(cfg.telemetry.endpoint.clone())
}
fn set_telemetry_endpoint(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::String(s) if !s.trim().is_empty() => {
            cfg.telemetry.endpoint = s;
            Ok(())
        }
        FieldValue::String(_) => Err("endpoint cannot be empty"),
        _ => Err("expects string"),
    }
}

/// Look up a section by id, returning `None` if not registered.
pub fn section(id: SectionId) -> Option<&'static ConfigSectionMeta> {
    CONFIG_SECTIONS.iter().find(|s| s.id == id)
}

/// Parse a section slug to its `SectionId` (case-insensitive). Useful for
/// `/config <section>` arguments.
pub fn section_from_slug(slug: &str) -> Option<SectionId> {
    let lower = slug.trim().to_ascii_lowercase();
    CONFIG_SECTIONS
        .iter()
        .find(|s| s.id.slug() == lower)
        .map(|s| s.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_section_has_at_least_one_field() {
        for section in CONFIG_SECTIONS {
            assert!(
                !section.fields.is_empty(),
                "section {} has no fields",
                section.label
            );
        }
    }

    #[test]
    fn section_lookup_is_consistent() {
        for s in CONFIG_SECTIONS {
            assert_eq!(section(s.id).map(|m| m.id), Some(s.id));
            assert_eq!(section_from_slug(s.id.slug()), Some(s.id));
        }
    }

    #[test]
    fn provider_setter_swaps_default_model() {
        let mut cfg = AppConfig::from_env();
        let original = cfg.model.clone();
        (CONFIG_SECTIONS[0].fields[0].set)(&mut cfg, FieldValue::Enum("anthropic")).unwrap();
        assert_eq!(cfg.model, DEFAULT_ANTHROPIC_MODEL);
        // and back
        (CONFIG_SECTIONS[0].fields[0].set)(&mut cfg, FieldValue::Enum("openai")).unwrap();
        assert_eq!(cfg.model, DEFAULT_OPENAI_MODEL);
        let _ = original;
    }

    #[test]
    fn permission_round_trip() {
        let mut cfg = AppConfig::from_env();
        let perms = section(SectionId::Permissions).unwrap();
        for f in perms.fields {
            for option in PERMISSION_MODE_OPTIONS {
                (f.set)(&mut cfg, FieldValue::Enum(option)).unwrap();
                match (f.get)(&cfg) {
                    FieldValue::Enum(v) => assert_eq!(v, *option, "{}", f.label),
                    other => panic!("unexpected: {:?}", other),
                }
            }
        }
    }
}
