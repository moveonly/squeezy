//! Declarative metadata for every editable field in `AppConfig`.
//!
//! `CONFIG_SECTIONS` is the single source of truth shared by the TUI config
//! screen and the TOML writer: both walk the same list, so the screen cannot
//! show a field the writer doesn't know how to persist (and vice versa).
//!
//! New sections are added by appending a `ConfigSectionMeta` entry below.

use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use crate::{
    AppConfig, CacheDurability, CompactionStrategy, DEFAULT_ANTHROPIC_MODEL,
    DEFAULT_AZURE_OPENAI_MODEL, DEFAULT_BEDROCK_MODEL,
    DEFAULT_CONTEXT_COMPACTION_LAYERED_FALLBACK_EXTRACTIVE_THRESHOLD_TOKENS,
    DEFAULT_CONTEXT_COMPACTION_MAX_SUMMARY_BYTES, DEFAULT_CONTEXT_COMPACTION_MIN_ITEMS,
    DEFAULT_CONTEXT_COMPACTION_MODEL_ASSISTED_MAX_OUTPUT_TOKENS,
    DEFAULT_CONTEXT_COMPACTION_MODEL_ASSISTED_TIMEOUT_SECS,
    DEFAULT_CONTEXT_COMPACTION_RECENT_ITEMS, DEFAULT_CONTEXT_FALLBACK_WINDOW_TOKENS,
    DEFAULT_CONTEXT_MICRO_COMPACTION_KEEP_RECENT, DEFAULT_CONTEXT_REPO_DOC_MAX_BYTES,
    DEFAULT_CONTEXT_TRIM_AT_PERCENT, DEFAULT_CONTEXT_USER_MEMORY_MAX_BYTES,
    DEFAULT_CONTEXT_WARN_AT_PERCENT, DEFAULT_COST_WARN_PERCENT, DEFAULT_EXA_API_KEY_ENV,
    DEFAULT_EXA_MCP_URL, DEFAULT_FEEDBACK_ENDPOINT, DEFAULT_FEEDBACK_MAX_BYTES,
    DEFAULT_GITHUB_COPILOT_MODEL, DEFAULT_GOOGLE_MODEL, DEFAULT_MAX_PARALLEL_TOOLS,
    DEFAULT_MAX_SEARCH_FILES_PER_TURN, DEFAULT_MAX_TOOL_BYTES_READ_PER_TURN,
    DEFAULT_MAX_TOOL_CALLS_PER_TURN, DEFAULT_OLLAMA_MODEL, DEFAULT_OPENAI_CODEX_MODEL,
    DEFAULT_OPENAI_MODEL, DEFAULT_PARALLEL_API_KEY_ENV, DEFAULT_PARALLEL_MCP_URL,
    DEFAULT_REPORT_ENDPOINT, DEFAULT_REPORT_MAX_BYTES, DEFAULT_SESSION_LOG_RETENTION_ARCHIVE_DAYS,
    DEFAULT_SESSION_LOG_RETENTION_DAYS, DEFAULT_SESSION_MAX_EVENT_BYTES,
    DEFAULT_SESSION_MAX_SESSION_BYTES, DEFAULT_STREAM_IDLE_TIMEOUT_MS,
    DEFAULT_SUBAGENT_MAX_CONCURRENT, DEFAULT_SUBAGENT_MAX_MODEL_ROUNDS,
    DEFAULT_SUBAGENT_MAX_SEARCH_FILES_PER_CALL, DEFAULT_SUBAGENT_MAX_SUMMARY_TOKENS,
    DEFAULT_SUBAGENT_MAX_TOOL_BYTES_READ_PER_CALL, DEFAULT_SUBAGENT_MAX_TOOL_CALLS_PER_CALL,
    DEFAULT_TELEMETRY_ENDPOINT, DEFAULT_TICK_RATE_MS, DEFAULT_TUI_SPINNER_NAME,
    DEFAULT_TUI_THEME_NAME, DEFAULT_WEBSEARCH_PROVIDER, NotificationMethod, OpenAiCompatiblePreset,
    PermissionMode, PermissionPolicyMode, ProviderConfig, ReasoningEffort, ResponseVerbosity,
    SessionMode, SessionResumePicker, StatusVerbosity, ToolOutputVerbosity, TranscriptDefault,
    TuiSynchronizedOutput, normalize_tui_spinner_name, normalize_tui_theme_name,
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
    /// User-facing badge label. The screen exposes three scopes:
    ///   User → ~/.squeezy/settings.toml
    ///   Repo  → ./squeezy.toml          (internal tier name: `project`)
    ///   Local → ~/.squeezy/projects/<hash>/settings.toml
    ///                                   (internal tier name: `repo`)
    pub const fn badge(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::User => "user",
            Self::Project => "repo",
            Self::Repo => "local",
            Self::Env => "env",
        }
    }
}

/// The editor shape for a field. Drives which widget the UI renders.
#[derive(Clone, Copy)]
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
    OptionalFloat {
        min: f64,
        max: f64,
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
    /// Editable list of strings (e.g. `graph.languages`).
    StringList {
        min: usize,
        max: usize,
    },
    /// Filesystem path. `must_exist` validation deferred to the editor.
    Path {
        must_exist: bool,
        dir_only: bool,
    },
    /// API-key style secret. Never read into `AppConfig` directly; the
    /// TUI handles entry through its dedicated secret-editor path which
    /// writes `[providers.<name>] api_key` to the active scope's TOML.
    Secret {
        env_var: &'static str,
    },
    /// Read-only informational row. The `get` fn returns a `String` rendered
    /// verbatim; there is no editor and saves never touch it. Used to surface
    /// context like the active provider in the Routing section.
    Info,
}

impl std::fmt::Debug for FieldKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bool => write!(f, "Bool"),
            Self::Info => write!(f, "Info"),
            Self::Integer { min, max, suffix } => f
                .debug_struct("Integer")
                .field("min", min)
                .field("max", max)
                .field("suffix", suffix)
                .finish(),
            Self::OptionalInteger { min, max, suffix } => f
                .debug_struct("OptionalInteger")
                .field("min", min)
                .field("max", max)
                .field("suffix", suffix)
                .finish(),
            Self::OptionalFloat { min, max } => f
                .debug_struct("OptionalFloat")
                .field("min", min)
                .field("max", max)
                .finish(),
            Self::Enum { options } => f.debug_struct("Enum").field("options", options).finish(),
            Self::OptionalEnum { options } => f
                .debug_struct("OptionalEnum")
                .field("options", options)
                .finish(),
            Self::String { multiline } => f
                .debug_struct("String")
                .field("multiline", multiline)
                .finish(),
            Self::DurationMs => write!(f, "DurationMs"),
            Self::StringList { min, max } => f
                .debug_struct("StringList")
                .field("min", min)
                .field("max", max)
                .finish(),
            Self::Path {
                must_exist,
                dir_only,
            } => f
                .debug_struct("Path")
                .field("must_exist", must_exist)
                .field("dir_only", dir_only)
                .finish(),
            Self::Secret { env_var } => f.debug_struct("Secret").field("env_var", env_var).finish(),
        }
    }
}

/// Concrete value carried through reads, writes, and editor commits.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldValue {
    Bool(bool),
    Integer(i64),
    OptionalInteger(Option<i64>),
    OptionalFloat(Option<f64>),
    Enum(&'static str),
    OptionalEnum(Option<&'static str>),
    String(String),
    Duration(Duration),
    Unset,
    StringList(Vec<String>),
    Path(PathBuf),
    /// Placeholder — secrets never carry the plaintext through `FieldValue`.
    /// The screen mask-renders this as `••••` and routes editing through
    /// the dedicated secret-entry flow which writes inline `api_key` to
    /// the active scope's TOML.
    Secret,
    /// Selected sub-tab index (read-only convenience for a sub-tabbed row).
    SubTabs(usize),
    /// Keyed table array (e.g. `[mcp.servers.<name>]`).
    TableArrayKeyed(BTreeMap<String, BTreeMap<String, FieldValue>>),
    /// Positional table array (e.g. `[[permissions.rules]]`).
    TableArrayOrdered(Vec<BTreeMap<String, FieldValue>>),
}

impl FieldValue {
    pub fn as_display(&self) -> String {
        match self {
            Self::Bool(v) => v.to_string(),
            Self::Integer(v) => v.to_string(),
            Self::OptionalInteger(Some(v)) => v.to_string(),
            Self::OptionalInteger(None) => "—".to_string(),
            Self::OptionalFloat(Some(v)) => format_config_float(*v),
            Self::OptionalFloat(None) => "—".to_string(),
            Self::Enum(v) => (*v).to_string(),
            Self::OptionalEnum(Some(v)) => (*v).to_string(),
            Self::OptionalEnum(None) => "—".to_string(),
            Self::String(s) => {
                if s.is_empty() {
                    "—".to_string()
                } else {
                    s.clone()
                }
            }
            Self::Duration(d) => format!("{} ms", d.as_millis()),
            Self::Unset => "—".to_string(),
            Self::StringList(items) => {
                if items.is_empty() {
                    "—".to_string()
                } else {
                    items.join(", ")
                }
            }
            Self::Path(p) => p.display().to_string(),
            Self::Secret => "••••".to_string(),
            Self::SubTabs(_) => String::new(),
            Self::TableArrayKeyed(map) => format!("{} entries", map.len()),
            Self::TableArrayOrdered(rows) => format!("{} rows", rows.len()),
        }
    }
}

fn format_config_float(value: f64) -> String {
    let mut formatted = format!("{value:.6}");
    while formatted.contains('.') && formatted.ends_with('0') {
        formatted.pop();
    }
    if formatted.ends_with('.') {
        formatted.push('0');
    }
    formatted
}

/// Ordered TOML path. e.g. `["model", "provider"]` or `["tui", "tick_rate_ms"]`.
pub type SettingsPath = &'static [&'static str];

/// Identity for a section, used by the slash router (`/model` → `Models`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SectionId {
    Models,
    Permissions,
    Themes,
    Verbosity,
    Limits,
    Telemetry,
    Routing,
    Session,
    Modes,
    Context,
    Subagents,
    Graph,
    Cache,
    Feedback,
    Redaction,
    Web,
    Tools,
    McpServers,
    /// Synthetic section that hosts tier-wide reset actions ("delete the
    /// user file", "delete the repo file", "delete the local file"). Has
    /// no `FieldMeta` entries — the TUI renders an action list and runs
    /// each action against `SeparatedSources` directly.
    Reset,
}

impl SectionId {
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Models => "models",
            Self::Permissions => "permissions",
            Self::Themes => "themes",
            Self::Verbosity => "verbosity",
            Self::Limits => "limits",
            Self::Telemetry => "telemetry",
            Self::Routing => "routing",
            Self::Session => "session",
            Self::Modes => "modes",
            Self::Context => "context",
            Self::Subagents => "subagents",
            Self::Graph => "graph",
            Self::Cache => "cache",
            Self::Feedback => "feedback",
            Self::Redaction => "redaction",
            Self::Web => "web",
            Self::Tools => "tools",
            Self::McpServers => "mcp-servers",
            Self::Reset => "reset",
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
    /// Programmatic default — invoked by `Ctrl+R` reset. Must mirror what
    /// `AppConfig::from_env()` would yield with no overrides set.
    pub default: fn() -> FieldValue,
    pub help: &'static str,
    /// `SQUEEZY_*` env var that, when set, shadows this field at runtime.
    /// Displayed as the `[env]` badge and disables in-screen editing.
    pub env_override: Option<&'static str>,
    /// `true` for API-key style fields: rendered as `••••`, edits route
    /// through the secret-entry flow which writes inline `api_key` to the
    /// active scope's TOML.
    pub secret: bool,
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
    "openai_codex",
    "github_copilot",
    "openrouter",
    "vercel",
    "portkey",
    "groq",
    "xai",
    "deepseek",
    "vertex",
    "mistral",
    "together",
    "fireworks",
    "cerebras",
    "deepinfra",
    "baseten",
    "lmstudio",
    "vllm",
    "llamacpp",
    "cloudflare_workers_ai",
    "cloudflare_ai_gateway",
    "openai_compatible",
];

pub const COMPACTION_STRATEGY_OPTIONS: &[&str] =
    &["extractive", "model_assisted", "layered_fallback"];
pub const PROFILE_OPTIONS: &[&str] = &["cheap", "balanced", "strong"];
pub const CACHE_ISOLATION_OPTIONS: &[&str] = &["switch", "subagent", "auto"];
pub const REASONING_EFFORT_OPTIONS: &[&str] = &["low", "medium", "high", "xhigh"];
pub const SESSION_MODE_OPTIONS: &[&str] = &["build", "plan"];
pub const SESSION_RESUME_PICKER_OPTIONS: &[&str] = &["ask", "never"];
pub const CACHE_DURABILITY_OPTIONS: &[&str] = &["fast", "turn", "strict"];
pub const STATUS_VERBOSITY_OPTIONS: &[&str] = &["compact", "verbose"];
pub const RESPONSE_VERBOSITY_OPTIONS: &[&str] = &["concise", "normal", "verbose"];
pub const TOOL_OUTPUT_VERBOSITY_OPTIONS: &[&str] = &["compact", "normal", "verbose"];
pub const TRANSCRIPT_DEFAULT_OPTIONS: &[&str] = &["compact", "expanded"];
pub const DESKTOP_NOTIFICATIONS_OPTIONS: &[&str] = &["off", "bel", "osc9", "auto"];
pub const SYNCHRONIZED_OUTPUT_OPTIONS: &[&str] = &["auto", "always", "never"];
pub const PERMISSION_POLICY_MODE_OPTIONS: &[&str] =
    &["default", "auto_review", "full_access", "custom"];
pub const PERMISSION_MODE_OPTIONS: &[&str] = &["allow", "ask", "deny"];
pub const THEME_OPTIONS: &[&str] = crate::BUILTIN_TUI_THEME_NAMES;

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
                default: || FieldValue::Enum("openai"),
                help: "Which LLM provider to use. Switching also resets the model to that provider's default unless you set one explicitly.",
                env_override: Some("SQUEEZY_PROVIDER"),
                secret: false,
            },
            FieldMeta {
                label: "model",
                toml_path: &["model", "model"],
                kind: FieldKind::String { multiline: false },
                tier: ApplyTier::NextPrompt,
                get: get_model,
                set: set_model,
                default_display: DEFAULT_OPENAI_MODEL,
                default: || FieldValue::String(String::new()),
                help: "Provider-specific model identifier.",
                env_override: Some("SQUEEZY_MODEL"),
                secret: false,
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
                default: || FieldValue::Enum("balanced"),
                help: "Default cost/capability profile when model is unset.",
                env_override: Some("SQUEEZY_PROFILE"),
                secret: false,
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
                default: || FieldValue::OptionalEnum(None),
                help: "Reasoning effort hint. Only meaningful for reasoning-capable models.",
                env_override: Some("SQUEEZY_REASONING_EFFORT"),
                secret: false,
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
                default: || FieldValue::OptionalInteger(None),
                help: "Cap on output tokens per request. Unset means provider default.",
                env_override: Some("SQUEEZY_MAX_OUTPUT_TOKENS"),
                secret: false,
            },
            FieldMeta {
                label: "context_window",
                toml_path: &["model_limits", "*", "context_window"],
                kind: FieldKind::OptionalInteger {
                    min: 1,
                    max: 100_000_000,
                    suffix: Some("tok"),
                },
                tier: ApplyTier::NextPrompt,
                get: get_model_context_window,
                set: set_model_context_window,
                default_display: "auto",
                default: || FieldValue::OptionalInteger(None),
                help: "Context window for THIS model. auto = resolve from override → provider \
                       live → curated catalog → models.dev → fallback. A number pins the window \
                       used by /context and compaction. Stored per provider:model in \
                       [model_limits], so switching models keeps the right value.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "temperature",
                toml_path: &["model", "temperature"],
                kind: FieldKind::OptionalFloat { min: 0.0, max: 2.0 },
                tier: ApplyTier::NextPrompt,
                get: get_temperature,
                set: set_temperature,
                default_display: "—",
                default: || FieldValue::OptionalFloat(None),
                help: "Sampling randomness. Lower is more deterministic; unset leaves the provider/model default.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "top_p",
                toml_path: &["model", "top_p"],
                kind: FieldKind::OptionalFloat { min: 0.0, max: 1.0 },
                tier: ApplyTier::NextPrompt,
                get: get_top_p,
                set: set_top_p,
                default_display: "—",
                default: || FieldValue::OptionalFloat(None),
                help: "Nucleus-sampling cutoff. Lower narrows token choices; unset leaves the provider/model default.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "seed",
                toml_path: &["model", "seed"],
                kind: FieldKind::OptionalInteger {
                    min: 0,
                    max: i64::MAX,
                    suffix: None,
                },
                tier: ApplyTier::NextPrompt,
                get: get_seed,
                set: set_seed,
                default_display: "—",
                default: || FieldValue::OptionalInteger(None),
                help: "Deterministic sampling seed where supported. Unset leaves provider/model default randomness.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "stop",
                toml_path: &["model", "stop"],
                kind: FieldKind::StringList { min: 0, max: 32 },
                tier: ApplyTier::NextPrompt,
                get: get_stop_sequences,
                set: set_stop_sequences,
                default_display: "—",
                default: || FieldValue::StringList(Vec::new()),
                help: "Stop generation when any listed string appears. Empty/unset leaves the provider/model default.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "frequency_penalty",
                toml_path: &["model", "frequency_penalty"],
                kind: FieldKind::OptionalFloat {
                    min: -2.0,
                    max: 2.0,
                },
                tier: ApplyTier::NextPrompt,
                get: get_frequency_penalty,
                set: set_frequency_penalty,
                default_display: "—",
                default: || FieldValue::OptionalFloat(None),
                help: "Reduce repeated wording where supported. Unset leaves the provider/model default.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "presence_penalty",
                toml_path: &["model", "presence_penalty"],
                kind: FieldKind::OptionalFloat {
                    min: -2.0,
                    max: 2.0,
                },
                tier: ApplyTier::NextPrompt,
                get: get_presence_penalty,
                set: set_presence_penalty,
                default_display: "—",
                default: || FieldValue::OptionalFloat(None),
                help: "Encourage new topics where supported. Unset leaves the provider/model default.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "stream_idle_timeout",
                toml_path: &["model", "stream_idle_timeout_ms"],
                kind: FieldKind::DurationMs,
                tier: ApplyTier::NextPrompt,
                get: get_stream_idle_timeout,
                set: set_stream_idle_timeout,
                default_display: "300000 ms",
                default: || {
                    FieldValue::Duration(Duration::from_millis(DEFAULT_STREAM_IDLE_TIMEOUT_MS))
                },
                help: "Abort if no streaming bytes arrive for this duration.",
                env_override: Some("SQUEEZY_STREAM_IDLE_TIMEOUT_MS"),
                secret: false,
            },
            FieldMeta {
                label: "store_responses",
                toml_path: &["model", "store_responses"],
                kind: FieldKind::Bool,
                tier: ApplyTier::NextPrompt,
                get: get_store_responses,
                set: set_store_responses,
                default_display: "false",
                default: || FieldValue::Bool(false),
                help: "(OpenAI/Azure only) Persist responses on the provider side for retrieval.",
                env_override: Some("SQUEEZY_STORE_RESPONSES"),
                secret: false,
            },
            FieldMeta {
                label: "context_1m",
                toml_path: &["model", "context_1m"],
                kind: FieldKind::Bool,
                tier: ApplyTier::NextPrompt,
                get: get_context_1m,
                set: set_context_1m,
                default_display: "false",
                default: || FieldValue::Bool(false),
                help: "(Anthropic/Bedrock only) Opt into the 1M-token context window (context-1m beta). Raises the cap from 200K but bills long prompts at a premium per-token rate — leave off unless you need it.",
                env_override: Some("SQUEEZY_CONTEXT_1M"),
                secret: false,
            },
            FieldMeta {
                label: "extended_thinking",
                toml_path: &["model", "extended_thinking"],
                kind: FieldKind::Bool,
                tier: ApplyTier::NextPrompt,
                get: get_extended_thinking,
                set: set_extended_thinking,
                default_display: "false",
                default: || FieldValue::Bool(false),
                help: "(Anthropic/Bedrock only) Opt into interleaved/extended thinking (interleaved-thinking beta), letting the model reason between tool calls. Spends extra thinking tokens and adds latency.",
                env_override: Some("SQUEEZY_EXTENDED_THINKING"),
                secret: false,
            },
            FieldMeta {
                label: "ollama_keep_alive",
                toml_path: &["providers", "ollama", "keep_alive"],
                kind: FieldKind::String { multiline: false },
                tier: ApplyTier::NextPrompt,
                get: get_ollama_keep_alive,
                set: set_ollama_keep_alive,
                default_display: "5m (server default)",
                default: || FieldValue::String(String::new()),
                help: "(Ollama only) How long the server keeps the model loaded between turns. Accepts duration strings (\"5m\", \"24h\"), integer seconds, \"0\" to evict immediately, or \"-1\" to pin indefinitely. Empty = server default (5 minutes). Also via OLLAMA_KEEP_ALIVE env var.",
                env_override: Some("OLLAMA_KEEP_ALIVE"),
                secret: false,
            },
        ],
    },
    ConfigSectionMeta {
        id: SectionId::Routing,
        label: "Routing",
        description: "Auto-route easy turns to a cheaper model to cut cost",
        fields: &[
            FieldMeta {
                label: "provider",
                toml_path: &["routing", "_provider_info"],
                kind: FieldKind::Info,
                tier: ApplyTier::Immediate,
                get: get_routing_provider_info,
                set: set_noop,
                default_display: "",
                default: || FieldValue::String(String::new()),
                help: "Routing is per-provider — the model fields below apply to the ACTIVE provider (switch it in Models). The toggles above are global. Other providers keep their own saved settings.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "enabled",
                toml_path: &["routing", "enabled"],
                kind: FieldKind::Bool,
                tier: ApplyTier::NextPrompt,
                get: get_routing_enabled,
                set: set_routing_enabled,
                default_display: "true",
                default: || FieldValue::Bool(crate::DEFAULT_ROUTING_ENABLED),
                help: "Master switch: route easy turns to the cheaper model; harder turns stay on the main model. Same as `/router on|off`. Global.",
                env_override: Some("SQUEEZY_ROUTING_ENABLED"),
                secret: false,
            },
            FieldMeta {
                label: "heuristic",
                toml_path: &["routing", "heuristic"],
                kind: FieldKind::Bool,
                tier: ApplyTier::NextPrompt,
                get: get_routing_heuristic,
                set: set_routing_heuristic,
                default_display: "true",
                default: || FieldValue::Bool(crate::DEFAULT_ROUTING_HEURISTIC),
                help: "Static fast-path: instantly route obvious mechanical commands (e.g. 'run cargo test') with no judge call. Global.",
                env_override: Some("SQUEEZY_ROUTING_HEURISTIC"),
                secret: false,
            },
            FieldMeta {
                label: "llm_judge",
                toml_path: &["routing", "llm_judge"],
                kind: FieldKind::Bool,
                tier: ApplyTier::NextPrompt,
                get: get_routing_llm_judge,
                set: set_routing_llm_judge,
                default_display: "true",
                default: || FieldValue::Bool(crate::DEFAULT_ROUTING_LLM_JUDGE),
                help: "For non-obvious turns, ask the judge model whether to route cheap. Global.",
                env_override: Some("SQUEEZY_ROUTING_LLM_JUDGE"),
                secret: false,
            },
            FieldMeta {
                label: "cache_isolation",
                toml_path: &["routing", "cache_isolation"],
                kind: FieldKind::Enum {
                    options: CACHE_ISOLATION_OPTIONS,
                },
                tier: ApplyTier::NextPrompt,
                get: get_routing_cache_isolation,
                set: set_routing_cache_isolation,
                default_display: "auto",
                default: || FieldValue::Enum(crate::DEFAULT_ROUTING_CACHE_ISOLATION.as_str()),
                help: "How a cheap-routed turn avoids cold-writing the parent's prompt cache. switch = swap the main loop's model (classic); subagent = run the cheap work in a scoped subagent so the main loop stays on the parent (cache warm); auto = subagent only when the prefix is large enough to pay for it.",
                env_override: Some("SQUEEZY_ROUTING_CACHE_ISOLATION"),
                secret: false,
            },
            FieldMeta {
                label: "auto_prefix_token_threshold",
                toml_path: &["routing", "auto_prefix_token_threshold"],
                kind: FieldKind::Integer {
                    min: 0,
                    max: 2_000_000,
                    suffix: Some("tok"),
                },
                tier: ApplyTier::NextPrompt,
                get: get_routing_auto_prefix_token_threshold,
                set: set_routing_auto_prefix_token_threshold,
                default_display: "8000 tok",
                default: || {
                    FieldValue::Integer(crate::DEFAULT_ROUTING_AUTO_PREFIX_TOKEN_THRESHOLD as i64)
                },
                help: "Prefix size (estimated tokens) above which `cache_isolation = auto` isolates a cheap turn into a subagent.",
                env_override: Some("SQUEEZY_ROUTING_AUTO_PREFIX_TOKEN_THRESHOLD"),
                secret: false,
            },
            FieldMeta {
                label: "tier_effort",
                toml_path: &["routing", "tier_effort"],
                kind: FieldKind::Bool,
                tier: ApplyTier::NextPrompt,
                get: get_routing_tier_effort,
                set: set_routing_tier_effort,
                default_display: "true",
                default: || FieldValue::Bool(crate::DEFAULT_ROUTING_TIER_EFFORT),
                help: "Run each routed rung at its own reasoning effort (cheap shallow, flagship deep) — effort is a cost lever orthogonal to model. A user /effort pin always wins; the parent rung keeps its provider default unless effort_strong is set. Global.",
                env_override: Some("SQUEEZY_ROUTING_TIER_EFFORT"),
                secret: false,
            },
            FieldMeta {
                label: "effort_weak",
                toml_path: &["routing", "effort_weak"],
                kind: FieldKind::OptionalEnum {
                    options: REASONING_EFFORT_OPTIONS,
                },
                tier: ApplyTier::NextPrompt,
                get: get_routing_effort_weak,
                set: set_routing_effort_weak,
                default_display: "low",
                default: || FieldValue::OptionalEnum(None),
                help: "Reasoning effort for the weak rung when tier_effort is on (unset = low).",
                env_override: Some("SQUEEZY_ROUTING_EFFORT_WEAK"),
                secret: false,
            },
            FieldMeta {
                label: "effort_medium",
                toml_path: &["routing", "effort_medium"],
                kind: FieldKind::OptionalEnum {
                    options: REASONING_EFFORT_OPTIONS,
                },
                tier: ApplyTier::NextPrompt,
                get: get_routing_effort_medium,
                set: set_routing_effort_medium,
                default_display: "medium",
                default: || FieldValue::OptionalEnum(None),
                help: "Reasoning effort for the mid rung when tier_effort is on (unset = medium).",
                env_override: Some("SQUEEZY_ROUTING_EFFORT_MEDIUM"),
                secret: false,
            },
            FieldMeta {
                label: "effort_strong",
                toml_path: &["routing", "effort_strong"],
                kind: FieldKind::OptionalEnum {
                    options: REASONING_EFFORT_OPTIONS,
                },
                tier: ApplyTier::NextPrompt,
                get: get_routing_effort_strong,
                set: set_routing_effort_strong,
                default_display: "provider default",
                default: || FieldValue::OptionalEnum(None),
                help: "Reasoning effort for the parent rung when tier_effort is on. Unset = the provider/model default (so enabling tier_effort never silently deepens un-rerouted turns); set it to opt into a deeper flagship.",
                env_override: Some("SQUEEZY_ROUTING_EFFORT_STRONG"),
                secret: false,
            },
            FieldMeta {
                label: "judge_effort",
                toml_path: &["routing", "judge_effort"],
                kind: FieldKind::Bool,
                tier: ApplyTier::NextPrompt,
                get: get_routing_judge_effort,
                set: set_routing_judge_effort,
                default_display: "false",
                default: || FieldValue::Bool(crate::DEFAULT_ROUTING_JUDGE_EFFORT),
                help: "Let the LLM judge estimate per-task reasoning effort (overriding the tier→effort map for that turn), so two turns on the same rung can run at different depths. Off by default; needs llm_judge. A user /effort pin still wins.",
                env_override: Some("SQUEEZY_ROUTING_JUDGE_EFFORT"),
                secret: false,
            },
            FieldMeta {
                label: "cheap_model",
                toml_path: &["providers", "*", "cheap_model"],
                kind: FieldKind::String { multiline: false },
                tier: ApplyTier::NextPrompt,
                get: get_provider_cheap_model,
                set: set_provider_cheap_model,
                default_display: "auto",
                default: || FieldValue::String(String::new()),
                help: "Route TO (weak rung): the cheap model easy turns are sent to, for the active provider. Shows the model in effect; clear it to inherit the per-provider default mini tier (e.g. openai gpt-5.4-mini, google gemini-3.5-flash).",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "medium_model",
                toml_path: &["providers", "*", "medium_model"],
                kind: FieldKind::String { multiline: false },
                tier: ApplyTier::NextPrompt,
                get: get_provider_medium_model,
                set: set_provider_medium_model,
                default_display: "auto",
                default: || FieldValue::String(String::new()),
                help: "Mid rung of the Auto ladder: moderate turns route here, and a weak turn escalates here before the parent. Empty = the per-provider Sonnet-class default (e.g. anthropic claude-sonnet-4-6); deduped away when it equals the cheap or parent model. Accepts aliases like 'sonnet'.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "judge_model",
                toml_path: &["providers", "*", "judge_model"],
                kind: FieldKind::String { multiline: false },
                tier: ApplyTier::NextPrompt,
                get: get_provider_judge_model,
                set: set_provider_judge_model,
                default_display: "auto",
                default: || FieldValue::String(String::new()),
                help: "Cheap/fast model that classifies turns cheap-vs-parent, for the active provider. Must be cheap — a mini tier judges better than nano. Empty = per-provider mini default. Accepts aliases like 'haiku'.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "expensive_models",
                toml_path: &["providers", "*", "expensive_models"],
                kind: FieldKind::String { multiline: false },
                tier: ApplyTier::NextPrompt,
                get: get_provider_expensive_models,
                set: set_provider_expensive_models,
                default_display: "auto",
                default: || FieldValue::String(String::new()),
                help: "Route FROM: one regex; the parent model is rerouted when it matches. Default uses a negative lookahead to skip this provider's cheap tiers and reroute every flagship — e.g. (?i)^(?!.*(nano|mini)).* — so it's forward-compatible. Set your own regex (e.g. opus|gpt-5 to restrict, or a lookahead to exclude), or clear it (shown as 'any') to reroute every model.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "judge_prompt",
                toml_path: &["providers", "*", "judge_prompt"],
                kind: FieldKind::String { multiline: true },
                tier: ApplyTier::NextPrompt,
                get: get_provider_judge_prompt,
                set: set_provider_judge_prompt,
                default_display: "built-in",
                default: || FieldValue::String(String::new()),
                help: "Judge instructions for the active provider. Press Enter to open the full editor. Shows the built-in per-provider prompt unless you override it.",
                env_override: None,
                secret: false,
            },
        ],
    },
    ConfigSectionMeta {
        id: SectionId::Permissions,
        label: "Permissions",
        description: "Mode first, with granular defaults in Custom",
        fields: &[
            FieldMeta {
                label: "mode",
                toml_path: &["permissions", "mode"],
                kind: FieldKind::Enum {
                    options: PERMISSION_POLICY_MODE_OPTIONS,
                },
                tier: ApplyTier::Immediate,
                get: get_perm_mode,
                set: set_perm_mode,
                default_display: "default",
                default: || FieldValue::Enum("default"),
                help: "Permission preset (shipped default: Default — human prompts, no LLM reviewer). Default allows workspace read/edit/shell/git/compiler and asks on web/mcp/destructive. Auto-review (opt-in) instead routes edit/shell/git/compiler/web/mcp through the reviewer, which auto-approves the safe ones (see reviewer_model / reviewer_policy). Full Access removes workspace/network prompts; Custom exposes each capability.",
                env_override: None,
                secret: false,
            },
            // Reviewer rows are shown under Auto-review (and Custom). They sit
            // immediately after `mode` so the Permissions section's visible
            // rows stay a contiguous prefix — see `permissions_visible_rows`
            // in the TUI config screen.
            FieldMeta {
                label: "reviewer_model",
                toml_path: &["permissions", "ai_reviewer", "model"],
                kind: FieldKind::String { multiline: false },
                tier: ApplyTier::NextPrompt,
                get: get_ai_reviewer_model,
                set: set_ai_reviewer_model,
                default_display: "small-fast",
                default: || FieldValue::String(String::new()),
                help: "Model the Auto-review reviewer uses to judge permission prompts. Empty uses the provider's small/fast tier (cheap). Distinct from the turn-routing judge_model.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "reviewer_policy",
                toml_path: &["permissions", "ai_reviewer", "policy_file"],
                kind: FieldKind::String { multiline: false },
                tier: ApplyTier::NextPrompt,
                get: get_ai_reviewer_policy_file,
                set: set_ai_reviewer_policy_file,
                default_display: "built-in",
                default: || FieldValue::String(String::new()),
                help: "Path to a Markdown file that replaces the built-in Auto-review judging policy (APPROVAL_POLICY.md). Empty uses the built-in policy.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "reviewer_policy_extra",
                toml_path: &["permissions", "ai_reviewer", "policy"],
                kind: FieldKind::String { multiline: true },
                tier: ApplyTier::NextPrompt,
                get: get_ai_reviewer_policy_text,
                set: set_ai_reviewer_policy_text,
                default_display: "none",
                help: "Extra judging instructions appended to the base Auto-review policy (the built-in one, or reviewer_policy if set). Press Enter to edit. Tightens or extends the policy without replacing it.",
                default: || FieldValue::String(String::new()),
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "reviewer_capabilities",
                toml_path: &["permissions", "ai_reviewer", "allow_capabilities"],
                kind: FieldKind::StringList { min: 0, max: 9 },
                tier: ApplyTier::NextPrompt,
                get: get_ai_reviewer_capabilities,
                set: set_ai_reviewer_capabilities,
                default_display: "read, search, network, mcp, edit, shell, git, compiler",
                default: || FieldValue::StringList(Vec::new()),
                help: "Capabilities the Auto-review reviewer may auto-approve (read, search, edit, shell, git, compiler, network, mcp). Anything else always reaches a human; destructive is never auto-approved, and high-risk network/mcp and out-of-workspace writes always escalate. Empty = review-only.",
                env_override: None,
                secret: false,
            },
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
                default: || FieldValue::Enum("allow"),
                help: "Default for file reads.",
                env_override: Some("SQUEEZY_READ_PERMISSION"),
                secret: false,
            },
            FieldMeta {
                label: "search",
                toml_path: &["permissions", "search"],
                kind: FieldKind::Enum {
                    options: PERMISSION_MODE_OPTIONS,
                },
                tier: ApplyTier::Immediate,
                get: get_perm_search,
                set: set_perm_search,
                default_display: "allow",
                default: || FieldValue::Enum("allow"),
                help: "Default for ordinary workspace search/navigation tools.",
                env_override: Some("SQUEEZY_SEARCH_PERMISSION"),
                secret: false,
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
                default_display: "allow",
                default: || FieldValue::Enum("allow"),
                help: "Default for file edits and writes.",
                env_override: Some("SQUEEZY_EDIT_PERMISSION"),
                secret: false,
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
                default_display: "allow",
                default: || FieldValue::Enum("allow"),
                help: "Default for shell command execution.",
                env_override: Some("SQUEEZY_SHELL_PERMISSION"),
                secret: false,
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
                default_display: "allow",
                default: || FieldValue::Enum("allow"),
                help: "Default for searches that escape .gitignore boundaries.",
                env_override: Some("SQUEEZY_IGNORED_SEARCH_PERMISSION"),
                secret: false,
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
                default: || FieldValue::Enum("ask"),
                help: "Default for web fetches and searches.",
                env_override: Some("SQUEEZY_WEB_PERMISSION"),
                secret: false,
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
                default: || FieldValue::Enum("ask"),
                help: "Default for MCP tool invocations.",
                env_override: Some("SQUEEZY_MCP_PERMISSION"),
                secret: false,
            },
            FieldMeta {
                label: "git",
                toml_path: &["permissions", "git"],
                kind: FieldKind::Enum {
                    options: PERMISSION_MODE_OPTIONS,
                },
                tier: ApplyTier::Immediate,
                get: get_perm_git,
                set: set_perm_git,
                default_display: "allow",
                default: || FieldValue::Enum("allow"),
                help: "Default for non-destructive git command families.",
                env_override: Some("SQUEEZY_GIT_PERMISSION"),
                secret: false,
            },
            FieldMeta {
                label: "compiler",
                toml_path: &["permissions", "compiler"],
                kind: FieldKind::Enum {
                    options: PERMISSION_MODE_OPTIONS,
                },
                tier: ApplyTier::Immediate,
                get: get_perm_compiler,
                set: set_perm_compiler,
                default_display: "allow",
                default: || FieldValue::Enum("allow"),
                help: "Default for compiler/build/test verification command families.",
                env_override: Some("SQUEEZY_COMPILER_PERMISSION"),
                secret: false,
            },
            FieldMeta {
                label: "destructive",
                toml_path: &["permissions", "destructive"],
                kind: FieldKind::Enum {
                    options: PERMISSION_MODE_OPTIONS,
                },
                tier: ApplyTier::Immediate,
                get: get_perm_destructive,
                set: set_perm_destructive,
                default_display: "ask",
                default: || FieldValue::Enum("ask"),
                help: "Default for destructive commands such as remove/reset-style operations.",
                env_override: Some("SQUEEZY_DESTRUCTIVE_PERMISSION"),
                secret: false,
            },
        ],
    },
    ConfigSectionMeta {
        id: SectionId::Themes,
        label: "Themes",
        description: "Choose, create, and edit RGB colors",
        fields: &[],
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
                default: || FieldValue::Enum("normal"),
                help: "How chatty the assistant's prose answers are.",
                env_override: None,
                secret: false,
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
                default: || FieldValue::Enum("compact"),
                help: "How much tool output is shown inline.",
                env_override: None,
                secret: false,
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
                default: || FieldValue::Enum("compact"),
                help: "How much detail the bottom status bar shows.",
                env_override: None,
                secret: false,
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
                default: || FieldValue::Enum("compact"),
                help: "Whether new transcript entries start collapsed or expanded.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "desktop_notifications",
                toml_path: &["tui", "desktop_notifications"],
                kind: FieldKind::Enum {
                    options: DESKTOP_NOTIFICATIONS_OPTIONS,
                },
                tier: ApplyTier::Immediate,
                get: get_desktop_notifications,
                set: set_desktop_notifications,
                default_display: "off",
                default: || FieldValue::Enum("off"),
                help: "Bell/desktop notification when a turn finishes or needs approval.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "show_reasoning_usage",
                toml_path: &["tui", "show_reasoning_usage"],
                kind: FieldKind::Bool,
                tier: ApplyTier::Immediate,
                get: get_show_reasoning_usage,
                set: set_show_reasoning_usage,
                default_display: "true",
                default: || FieldValue::Bool(true),
                help: "Show reasoning-token usage alongside completion tokens.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "coalesce_tool_runs",
                toml_path: &["tui", "coalesce_tool_runs"],
                kind: FieldKind::Bool,
                tier: ApplyTier::Immediate,
                get: get_coalesce_tool_runs,
                set: set_coalesce_tool_runs,
                default_display: "true",
                default: || FieldValue::Bool(true),
                help: "Group consecutive same-tool calls (e.g. a fan-out of `read_file`s) into one card.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "persist_prompt_history",
                toml_path: &["tui", "persist_prompt_history"],
                kind: FieldKind::Bool,
                tier: ApplyTier::Restart,
                get: get_persist_prompt_history,
                set: set_persist_prompt_history,
                default_display: "false",
                default: || FieldValue::Bool(false),
                help: "Mirror the prompt-recall ring (Up/Down at the composer) to `~/.squeezy/prompt_history` so prior prompts survive across sessions.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "copy_on_select",
                toml_path: &["tui", "copy_on_select"],
                kind: FieldKind::Bool,
                tier: ApplyTier::Immediate,
                get: get_copy_on_select,
                set: set_copy_on_select,
                default_display: "true",
                default: || FieldValue::Bool(true),
                help: "Copy app-level mouse selections to the clipboard automatically on release — drag, then paste.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "first_run_hints",
                toml_path: &["tui", "first_run_hints"],
                kind: FieldKind::Bool,
                tier: ApplyTier::Restart,
                get: get_first_run_hints,
                set: set_first_run_hints,
                default_display: "true",
                default: || FieldValue::Bool(true),
                help: "Show the gentle first-run hints (command-palette chord, hover peek, turn jump). Each shows at most once and fades the instant you use or dismiss it; the seen-set persists across sessions. Off silences them entirely.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "synchronized_output",
                toml_path: &["tui", "synchronized_output"],
                kind: FieldKind::Enum {
                    options: SYNCHRONIZED_OUTPUT_OPTIONS,
                },
                tier: ApplyTier::Restart,
                get: get_synchronized_output,
                set: set_synchronized_output,
                default_display: "auto",
                default: || FieldValue::Enum("auto"),
                help: "Wrap each frame in DEC 2026 Begin/End Synchronized Update so capable terminals (kitty, WezTerm, Ghostty, iTerm2, Alacritty) flip the cell grid atomically and eliminate streaming tearing. `auto` enables when the terminal advertises support; the sequences are silently ignored elsewhere.",
                env_override: None,
                secret: false,
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
                default: || FieldValue::Integer(DEFAULT_TICK_RATE_MS as i64),
                help: "Frame interval for animations.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "status_line",
                toml_path: &["tui", "status_line"],
                kind: FieldKind::StringList { min: 0, max: 32 },
                tier: ApplyTier::Immediate,
                get: get_status_line,
                set: set_status_line,
                default_display: "",
                default: || FieldValue::StringList(Vec::new()),
                help: "Ordered status-bar items. Configure via /statusline.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "status_line_use_colors",
                toml_path: &["tui", "status_line_use_colors"],
                kind: FieldKind::Bool,
                tier: ApplyTier::Immediate,
                get: get_status_line_use_colors,
                set: set_status_line_use_colors,
                default_display: "true",
                default: || FieldValue::Bool(true),
                help: "Color status-bar items using their accent palette.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "zen",
                toml_path: &["tui", "zen"],
                kind: FieldKind::Bool,
                tier: ApplyTier::Immediate,
                get: get_zen,
                set: set_zen,
                default_display: "false",
                default: || FieldValue::Bool(false),
                help: "Zen Mode: low-noise layout that hides secondary chrome. Toggle live with F10.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "theme",
                toml_path: &["tui", "theme"],
                kind: FieldKind::String { multiline: false },
                tier: ApplyTier::Immediate,
                get: get_theme,
                set: set_theme,
                default_display: "default",
                default: || FieldValue::String(DEFAULT_TUI_THEME_NAME.to_string()),
                help: "Active named theme. Configure colors in the Themes section or via /theme.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "spinner",
                toml_path: &["tui", "spinner"],
                kind: FieldKind::Enum {
                    options: crate::BUILTIN_TUI_SPINNER_NAMES,
                },
                tier: ApplyTier::Immediate,
                get: get_spinner,
                set: set_spinner,
                default_display: "scintillate",
                default: || FieldValue::Enum(DEFAULT_TUI_SPINNER_NAME),
                help: "Working-status spinner shape: twinkle, scintillate, or drift.",
                env_override: None,
                secret: false,
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
                default: || FieldValue::Integer(DEFAULT_MAX_PARALLEL_TOOLS as i64),
                help: "Maximum tool calls executed concurrently per turn.",
                env_override: None,
                secret: false,
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
                default: || FieldValue::Integer(DEFAULT_MAX_TOOL_CALLS_PER_TURN as i64),
                help: "Stop the turn after this many tool calls.",
                env_override: None,
                secret: false,
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
                default: || FieldValue::Integer(DEFAULT_MAX_TOOL_BYTES_READ_PER_TURN as i64),
                help: "Aggregate read budget across all tools per turn.",
                env_override: None,
                secret: false,
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
                default: || FieldValue::Integer(DEFAULT_MAX_SEARCH_FILES_PER_TURN as i64),
                help: "Files scanned across all search tools per turn.",
                env_override: None,
                secret: false,
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
                default: || FieldValue::Integer(DEFAULT_COST_WARN_PERCENT as i64),
                help: "Warn when session cost crosses this percentage of the cap.",
                env_override: None,
                secret: false,
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
                default: || FieldValue::OptionalInteger(None),
                help: "Hard cap on session cost in micro-dollars. Unset means no cap.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "max_round_input_tokens",
                toml_path: &["budgets", "max_round_input_tokens"],
                kind: FieldKind::OptionalInteger {
                    min: 1,
                    max: 100_000_000,
                    suffix: Some("tok"),
                },
                tier: ApplyTier::Immediate,
                get: get_max_round_input_tokens,
                set: set_max_round_input_tokens,
                default_display: "—",
                default: || FieldValue::OptionalInteger(None),
                help: "Pre-flight ceiling on estimated input tokens per LLM round. \
                       Over it the agent compacts first, then gates. Unset means off.",
                env_override: None,
                secret: false,
            },
        ],
    },
    ConfigSectionMeta {
        id: SectionId::Context,
        label: "Context & Compaction",
        description: "When and how the conversation is compacted before the model window",
        fields: &[
            FieldMeta {
                label: "triggers",
                toml_path: &["context", "_trigger_info"],
                kind: FieldKind::Info,
                tier: ApplyTier::Immediate,
                get: get_context_trigger_info,
                set: set_noop,
                default_display: "",
                default: || FieldValue::String(String::new()),
                help: "Resolved window and the token points each tier fires at, for the \
                       active model. Percentages below are of the window.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "compaction_enabled",
                toml_path: &["context", "compaction_enabled"],
                kind: FieldKind::Bool,
                tier: ApplyTier::NextPrompt,
                get: get_context_compaction_enabled,
                set: set_context_compaction_enabled,
                default_display: "true",
                default: || FieldValue::Bool(true),
                help: "Master switch for post-turn (between-turn) compaction.",
                env_override: Some("SQUEEZY_CONTEXT_COMPACTION_ENABLED"),
                secret: false,
            },
            FieldMeta {
                label: "fallback_window_tokens",
                toml_path: &["context", "fallback_window_tokens"],
                kind: FieldKind::Integer {
                    min: 1,
                    max: 100_000_000,
                    suffix: Some("tok"),
                },
                tier: ApplyTier::NextPrompt,
                get: get_context_fallback_window,
                set: set_context_fallback_window,
                default_display: "128000 tok",
                default: || FieldValue::Integer(DEFAULT_CONTEXT_FALLBACK_WINDOW_TOKENS as i64),
                help: "Window assumed for the percent thresholds when the model's real window \
                       is unknown. Only a fallback; set model_context_window otherwise.",
                env_override: Some("SQUEEZY_CONTEXT_FALLBACK_WINDOW_TOKENS"),
                secret: false,
            },
            FieldMeta {
                label: "max_context_tokens",
                toml_path: &["context", "max_context_tokens"],
                kind: FieldKind::OptionalInteger {
                    min: 1,
                    max: 100_000_000,
                    suffix: Some("tok"),
                },
                tier: ApplyTier::NextPrompt,
                get: get_context_max_context_tokens,
                set: set_context_max_context_tokens,
                default_display: "—",
                default: || FieldValue::OptionalInteger(None),
                help: "Optional hard cap on the summarize threshold, independent of the window. \
                       Unset → thresholds scale with the window; set it to keep requests small.",
                env_override: Some("SQUEEZY_CONTEXT_MAX_CONTEXT_TOKENS"),
                secret: false,
            },
            FieldMeta {
                label: "compaction_min_items",
                toml_path: &["context", "compaction_min_items"],
                kind: FieldKind::Integer {
                    min: 1,
                    max: 100_000,
                    suffix: None,
                },
                tier: ApplyTier::NextPrompt,
                get: get_context_min_items,
                set: set_context_min_items,
                default_display: "16",
                default: || FieldValue::Integer(DEFAULT_CONTEXT_COMPACTION_MIN_ITEMS as i64),
                help: "Post-turn also needs at least this many items (AND-ed with the token \
                       floor), unless the conversation is already near the window.",
                env_override: Some("SQUEEZY_CONTEXT_COMPACTION_MIN_ITEMS"),
                secret: false,
            },
            FieldMeta {
                label: "compaction_recent_items",
                toml_path: &["context", "compaction_recent_items"],
                kind: FieldKind::Integer {
                    min: 1,
                    max: 100_000,
                    suffix: None,
                },
                tier: ApplyTier::NextPrompt,
                get: get_context_recent_items,
                set: set_context_recent_items,
                default_display: "10",
                default: || FieldValue::Integer(DEFAULT_CONTEXT_COMPACTION_RECENT_ITEMS as i64),
                help: "Newest items kept verbatim through a full compaction.",
                env_override: Some("SQUEEZY_CONTEXT_COMPACTION_RECENT_ITEMS"),
                secret: false,
            },
            FieldMeta {
                label: "compaction_max_summary_bytes",
                toml_path: &["context", "compaction_max_summary_bytes"],
                kind: FieldKind::Integer {
                    min: 256,
                    max: 10_000_000,
                    suffix: Some("bytes"),
                },
                tier: ApplyTier::NextPrompt,
                get: get_context_max_summary_bytes,
                set: set_context_max_summary_bytes,
                default_display: "12000 bytes",
                default: || {
                    FieldValue::Integer(DEFAULT_CONTEXT_COMPACTION_MAX_SUMMARY_BYTES as i64)
                },
                help: "Cap on the generated extractive summary size.",
                env_override: Some("SQUEEZY_CONTEXT_COMPACTION_MAX_SUMMARY_BYTES"),
                secret: false,
            },
            FieldMeta {
                label: "enabled_mid_turn",
                toml_path: &["context", "enabled_mid_turn"],
                kind: FieldKind::Bool,
                tier: ApplyTier::NextPrompt,
                get: get_context_enabled_mid_turn,
                set: set_context_enabled_mid_turn,
                default_display: "true",
                default: || FieldValue::Bool(true),
                help: "Run the trim pass between LLM events within a turn. Summarize never \
                       runs mid-turn; it waits for the turn boundary or forced overflow.",
                env_override: Some("SQUEEZY_CONTEXT_COMPACTION_ENABLED_MID_TURN"),
                secret: false,
            },
            FieldMeta {
                label: "model_context_window",
                toml_path: &["context", "model_context_window"],
                kind: FieldKind::OptionalInteger {
                    min: 1,
                    max: 100_000_000,
                    suffix: Some("tok"),
                },
                tier: ApplyTier::NextPrompt,
                get: get_context_model_window,
                set: set_context_model_window,
                default_display: "—",
                default: || FieldValue::OptionalInteger(None),
                help: "Global fallback model token budget; all percent knobs are % of this. The \
                       per-model [Models].context_window override takes precedence. Unset → \
                       auto-derived by the limit resolver, else fallback_window_tokens.",
                env_override: Some("SQUEEZY_CONTEXT_MODEL_CONTEXT_WINDOW"),
                secret: false,
            },
            FieldMeta {
                label: "effective_context_window_percent",
                toml_path: &["context", "effective_context_window_percent"],
                kind: FieldKind::OptionalInteger {
                    min: 1,
                    max: 100,
                    suffix: Some("%"),
                },
                tier: ApplyTier::NextPrompt,
                get: get_context_effective_percent,
                set: set_context_effective_percent,
                default_display: "— (95)",
                default: || FieldValue::OptionalInteger(None),
                help: "Percent of the raw window treated as usable (rest is headroom); the \
                       summarize tier folds at this usable budget. Unset uses the curated \
                       model's value, else 95.",
                env_override: Some("SQUEEZY_CONTEXT_EFFECTIVE_CONTEXT_WINDOW_PERCENT"),
                secret: false,
            },
            FieldMeta {
                label: "baseline_reserve_tokens",
                toml_path: &["context", "baseline_reserve_tokens"],
                kind: FieldKind::OptionalInteger {
                    min: 0,
                    max: 10_000_000,
                    suffix: Some("tok"),
                },
                tier: ApplyTier::NextPrompt,
                get: get_context_baseline_reserve,
                set: set_context_baseline_reserve,
                default_display: "— (12000)",
                default: || FieldValue::OptionalInteger(None),
                help: "Flat token reserve carved off the effective window for system framing. \
                       Unset uses the built-in 12000.",
                env_override: Some("SQUEEZY_CONTEXT_BASELINE_RESERVE_TOKENS"),
                secret: false,
            },
            FieldMeta {
                label: "warn_at_percent",
                toml_path: &["context", "warn_at_percent"],
                kind: FieldKind::Integer {
                    min: 0,
                    max: 100,
                    suffix: Some("%"),
                },
                tier: ApplyTier::NextPrompt,
                get: get_context_warn_at_percent,
                set: set_context_warn_at_percent,
                default_display: "85 %",
                default: || FieldValue::Integer(DEFAULT_CONTEXT_WARN_AT_PERCENT as i64),
                help: "% of the effective window at which the pre-summarize /pin nudge fires. \
                       Sits below the summarize point so you can pin before any lossy summarize.",
                env_override: Some("SQUEEZY_CONTEXT_WARN_AT_PERCENT"),
                secret: false,
            },
            FieldMeta {
                label: "micro_compaction_enabled",
                toml_path: &["context", "micro_compaction_enabled"],
                kind: FieldKind::Bool,
                tier: ApplyTier::NextPrompt,
                get: get_context_micro_enabled,
                set: set_context_micro_enabled,
                default_display: "true",
                default: || FieldValue::Bool(true),
                help: "Master switch for the trim tier that clears older tool-output bodies \
                       in place (runs both mid-turn and as a post-turn pre-pass).",
                env_override: Some("SQUEEZY_CONTEXT_MICRO_COMPACTION_ENABLED"),
                secret: false,
            },
            FieldMeta {
                label: "trim_at_percent",
                toml_path: &["context", "trim_at_percent"],
                kind: FieldKind::Integer {
                    min: 0,
                    max: 100,
                    suffix: Some("%"),
                },
                tier: ApplyTier::NextPrompt,
                get: get_context_trim_at_percent,
                set: set_context_trim_at_percent,
                default_display: "40 %",
                default: || FieldValue::Integer(DEFAULT_CONTEXT_TRIM_AT_PERCENT as i64),
                help: "% of window at which old tool output is trimmed in place. Low by design \
                       (cheap, structure-preserving), so it runs well before summarize.",
                env_override: Some("SQUEEZY_CONTEXT_TRIM_AT_PERCENT"),
                secret: false,
            },
            FieldMeta {
                label: "micro_compaction_keep_recent",
                toml_path: &["context", "micro_compaction_keep_recent"],
                kind: FieldKind::Integer {
                    min: 0,
                    max: 10_000,
                    suffix: None,
                },
                tier: ApplyTier::NextPrompt,
                get: get_context_micro_keep_recent,
                set: set_context_micro_keep_recent,
                default_display: "5",
                default: || {
                    FieldValue::Integer(DEFAULT_CONTEXT_MICRO_COMPACTION_KEEP_RECENT as i64)
                },
                help: "Newest tool outputs the micro pass keeps verbatim.",
                env_override: Some("SQUEEZY_CONTEXT_MICRO_COMPACTION_KEEP_RECENT"),
                secret: false,
            },
            FieldMeta {
                label: "strategy",
                toml_path: &["context", "strategy"],
                kind: FieldKind::Enum {
                    options: COMPACTION_STRATEGY_OPTIONS,
                },
                tier: ApplyTier::NextPrompt,
                get: get_context_strategy,
                set: set_context_strategy,
                default_display: "extractive",
                default: || FieldValue::Enum(CompactionStrategy::Extractive.as_str()),
                help: "Summary strategy. extractive = deterministic, no model call. \
                       model_assisted / layered_fallback rewrite via a cheap model with \
                       extractive fallback. See docs/internal/cost-saving/02-conversation-compaction.md.",
                env_override: Some("SQUEEZY_CONTEXT_COMPACTION_STRATEGY"),
                secret: false,
            },
            FieldMeta {
                label: "model_assisted_model",
                toml_path: &["context", "model_assisted_model"],
                kind: FieldKind::String { multiline: false },
                tier: ApplyTier::NextPrompt,
                get: get_context_model_assisted_model,
                set: set_context_model_assisted_model,
                default_display: "—",
                default: || FieldValue::String(String::new()),
                help: "Cheap model for non-extractive strategies. Empty → resolved \
                       small/fast model; falls back to extractive if neither resolves.",
                env_override: Some("SQUEEZY_CONTEXT_COMPACTION_MODEL_ASSISTED_MODEL"),
                secret: false,
            },
            FieldMeta {
                label: "model_assisted_max_output_tokens",
                toml_path: &["context", "model_assisted_max_output_tokens"],
                kind: FieldKind::Integer {
                    min: 1,
                    max: 1_000_000,
                    suffix: Some("tok"),
                },
                tier: ApplyTier::NextPrompt,
                get: get_context_model_assisted_max_output_tokens,
                set: set_context_model_assisted_max_output_tokens,
                default_display: "1500 tok",
                default: || {
                    FieldValue::Integer(
                        DEFAULT_CONTEXT_COMPACTION_MODEL_ASSISTED_MAX_OUTPUT_TOKENS as i64,
                    )
                },
                help: "Output cap per model-assisted summary call.",
                env_override: Some("SQUEEZY_CONTEXT_COMPACTION_MODEL_ASSISTED_MAX_OUTPUT_TOKENS"),
                secret: false,
            },
            FieldMeta {
                label: "model_assisted_timeout_secs",
                toml_path: &["context", "model_assisted_timeout_secs"],
                kind: FieldKind::Integer {
                    min: 1,
                    max: 3_600,
                    suffix: Some("s"),
                },
                tier: ApplyTier::NextPrompt,
                get: get_context_model_assisted_timeout_secs,
                set: set_context_model_assisted_timeout_secs,
                default_display: "30 s",
                default: || {
                    FieldValue::Integer(
                        DEFAULT_CONTEXT_COMPACTION_MODEL_ASSISTED_TIMEOUT_SECS as i64,
                    )
                },
                help: "Timeout before a model-assisted summary falls back to extractive.",
                env_override: Some("SQUEEZY_CONTEXT_COMPACTION_MODEL_ASSISTED_TIMEOUT_SECS"),
                secret: false,
            },
            FieldMeta {
                label: "layered_fallback_extractive_threshold_tokens",
                toml_path: &["context", "layered_fallback_extractive_threshold_tokens"],
                kind: FieldKind::Integer {
                    min: 0,
                    max: 100_000_000,
                    suffix: Some("tok"),
                },
                tier: ApplyTier::NextPrompt,
                get: get_context_layered_fallback_threshold,
                set: set_context_layered_fallback_threshold,
                default_display: "4000 tok",
                default: || {
                    FieldValue::Integer(
                        DEFAULT_CONTEXT_COMPACTION_LAYERED_FALLBACK_EXTRACTIVE_THRESHOLD_TOKENS
                            as i64,
                    )
                },
                help: "In layered_fallback, only call the model when the dropped slice \
                       exceeds this many tokens.",
                env_override: Some("SQUEEZY_CONTEXT_COMPACTION_LAYERED_FALLBACK_THRESHOLD_TOKENS"),
                secret: false,
            },
            FieldMeta {
                label: "repo_doc_max_bytes",
                toml_path: &["context", "repo_doc_max_bytes"],
                kind: FieldKind::Integer {
                    min: 0,
                    max: 100_000_000,
                    suffix: Some("bytes"),
                },
                tier: ApplyTier::NextPrompt,
                get: get_context_repo_doc_max_bytes,
                set: set_context_repo_doc_max_bytes,
                default_display: "32768 bytes",
                default: || FieldValue::Integer(DEFAULT_CONTEXT_REPO_DOC_MAX_BYTES as i64),
                help: "Cap on AGENTS.md content stitched into base instructions (0 disables).",
                env_override: Some("SQUEEZY_CONTEXT_REPO_DOC_MAX_BYTES"),
                secret: false,
            },
            FieldMeta {
                label: "user_memory_max_bytes",
                toml_path: &["context", "user_memory_max_bytes"],
                kind: FieldKind::Integer {
                    min: 0,
                    max: 100_000_000,
                    suffix: Some("bytes"),
                },
                tier: ApplyTier::NextPrompt,
                get: get_context_user_memory_max_bytes,
                set: set_context_user_memory_max_bytes,
                default_display: "16384 bytes",
                default: || FieldValue::Integer(DEFAULT_CONTEXT_USER_MEMORY_MAX_BYTES as i64),
                help: "Cap on ~/.squeezy/MEMORY.md stitched into base instructions (0 disables).",
                env_override: Some("SQUEEZY_CONTEXT_USER_MEMORY_MAX_BYTES"),
                secret: false,
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
                default: || FieldValue::Bool(true),
                help: "Send anonymous usage events.",
                env_override: Some("SQUEEZY_TELEMETRY"),
                secret: false,
            },
            FieldMeta {
                label: "endpoint",
                toml_path: &["telemetry", "endpoint"],
                kind: FieldKind::String { multiline: false },
                tier: ApplyTier::NextPrompt,
                get: get_telemetry_endpoint,
                set: set_telemetry_endpoint,
                default_display: DEFAULT_TELEMETRY_ENDPOINT,
                default: || FieldValue::String(DEFAULT_TELEMETRY_ENDPOINT.to_string()),
                help: "Where telemetry events are POSTed.",
                env_override: Some("SQUEEZY_TELEMETRY_ENDPOINT"),
                secret: false,
            },
        ],
    },
    ConfigSectionMeta {
        id: SectionId::Modes,
        label: "Modes",
        description: "Session mode and high-level agent behavior",
        fields: &[
            FieldMeta {
                label: "session_mode",
                toml_path: &["session", "mode"],
                kind: FieldKind::Enum {
                    options: SESSION_MODE_OPTIONS,
                },
                tier: ApplyTier::Immediate,
                get: get_session_mode,
                set: set_session_mode,
                default_display: "build",
                default: || FieldValue::Enum("build"),
                help: "Build mode runs tools freely; Plan mode allows non-mutating exploration and emits a structured plan.",
                env_override: Some("SQUEEZY_SESSION_MODE"),
                secret: false,
            },
            FieldMeta {
                label: "resume_picker",
                toml_path: &["session", "resume_picker"],
                kind: FieldKind::Enum {
                    options: SESSION_RESUME_PICKER_OPTIONS,
                },
                tier: ApplyTier::Restart,
                get: get_session_resume_picker,
                set: set_session_resume_picker,
                default_display: "ask",
                default: || FieldValue::Enum("ask"),
                help: "Ask to resume a recent session at startup, or always start fresh.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "exploration_graph",
                toml_path: &["agent", "exploration_graph"],
                kind: FieldKind::Bool,
                tier: ApplyTier::NextPrompt,
                get: get_exploration_graph,
                set: set_exploration_graph,
                default_display: "true",
                default: || FieldValue::Bool(true),
                help: "Use graph-first exploration before LLM tool dispatch.",
                env_override: Some("SQUEEZY_EXPLORATION_GRAPH"),
                secret: false,
            },
        ],
    },
    ConfigSectionMeta {
        id: SectionId::Session,
        label: "Session Logs",
        description: "Where session traces are written and how long they live",
        fields: &[
            FieldMeta {
                label: "log_dir",
                toml_path: &["session", "log_dir"],
                kind: FieldKind::Path {
                    must_exist: false,
                    dir_only: true,
                },
                tier: ApplyTier::Restart,
                get: get_session_log_dir,
                set: set_session_log_dir,
                default_display: ".squeezy/sessions",
                default: || FieldValue::Path(std::path::PathBuf::from(".squeezy/sessions")),
                help: "Directory where session event logs are written.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "log_retention_days",
                toml_path: &["session", "log_retention_days"],
                kind: FieldKind::Integer {
                    min: 1,
                    max: 3650,
                    suffix: Some("days"),
                },
                tier: ApplyTier::Restart,
                get: get_session_log_retention_days,
                set: set_session_log_retention_days,
                default_display: "30 days",
                default: || FieldValue::Integer(DEFAULT_SESSION_LOG_RETENTION_DAYS as i64),
                help: "Live session logs older than this are archived at startup.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "log_retention_archive_days",
                toml_path: &["session", "log_retention_archive_days"],
                kind: FieldKind::Integer {
                    min: 0,
                    max: 3650,
                    suffix: Some("days"),
                },
                tier: ApplyTier::Restart,
                get: get_session_log_retention_archive_days,
                set: set_session_log_retention_archive_days,
                default_display: "30 days",
                default: || FieldValue::Integer(DEFAULT_SESSION_LOG_RETENTION_ARCHIVE_DAYS as i64),
                help: "Archived sessions older than this are permanently deleted; 0 disables the archive sweep.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "max_event_bytes",
                toml_path: &["session", "max_event_bytes"],
                kind: FieldKind::Integer {
                    min: 4096,
                    max: 16_000_000,
                    suffix: Some("bytes"),
                },
                tier: ApplyTier::Restart,
                get: get_session_max_event_bytes,
                set: set_session_max_event_bytes,
                default_display: "65536 bytes",
                default: || FieldValue::Integer(DEFAULT_SESSION_MAX_EVENT_BYTES as i64),
                help: "Cap on individual session event size before truncation.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "max_session_bytes",
                toml_path: &["session", "max_session_bytes"],
                kind: FieldKind::Integer {
                    min: 1_048_576,
                    max: 1_000_000_000,
                    suffix: Some("bytes"),
                },
                tier: ApplyTier::Restart,
                get: get_session_max_session_bytes,
                set: set_session_max_session_bytes,
                default_display: "52428800 bytes",
                default: || FieldValue::Integer(DEFAULT_SESSION_MAX_SESSION_BYTES as i64),
                help: "Cap on total session log size; rolls over when exceeded.",
                env_override: None,
                secret: false,
            },
        ],
    },
    ConfigSectionMeta {
        id: SectionId::Subagents,
        label: "Subagents",
        description: "Per-subagent budgets and toggles",
        fields: &[
            FieldMeta {
                label: "enabled",
                toml_path: &["subagents", "enabled"],
                kind: FieldKind::Bool,
                tier: ApplyTier::NextPrompt,
                get: get_subagents_enabled,
                set: set_subagents_enabled,
                default_display: "true",
                default: || FieldValue::Bool(true),
                help: "Allow delegate / explore / delegate_plan / delegate_review / delegate_chain subagent dispatch, and the doc-help /help fallback.",
                env_override: Some("SQUEEZY_SUBAGENTS_ENABLED"),
                secret: false,
            },
            FieldMeta {
                label: "explore_enabled",
                toml_path: &["subagents", "explore_enabled"],
                kind: FieldKind::Bool,
                tier: ApplyTier::NextPrompt,
                get: get_subagent_explore_enabled,
                set: set_subagent_explore_enabled,
                default_display: "true",
                default: || FieldValue::Bool(true),
                help: "Allow the Explore subagent variant.",
                env_override: Some("SQUEEZY_EXPLORE_SUBAGENT_ENABLED"),
                secret: false,
            },
            FieldMeta {
                label: "explore_model",
                toml_path: &["subagents", "explore_model"],
                kind: FieldKind::String { multiline: false },
                tier: ApplyTier::NextPrompt,
                get: get_subagent_explore_model,
                set: set_subagent_explore_model,
                default_display: "—",
                default: || FieldValue::String(String::new()),
                help: "Override model id for the Explore subagent. Empty inherits the main model.",
                env_override: Some("SQUEEZY_EXPLORE_MODEL"),
                secret: false,
            },
            FieldMeta {
                label: "help_strict_local",
                toml_path: &["subagents", "help_strict_local"],
                kind: FieldKind::Bool,
                tier: ApplyTier::NextPrompt,
                get: get_subagent_help_strict_local,
                set: set_subagent_help_strict_local,
                default_display: "false",
                default: || FieldValue::Bool(false),
                help: "Answer /help fully locally — never call the DocHelp model subagent.",
                env_override: Some("SQUEEZY_HELP_STRICT_LOCAL"),
                secret: false,
            },
            FieldMeta {
                label: "max_concurrent",
                toml_path: &["subagents", "max_concurrent"],
                kind: FieldKind::Integer {
                    min: 1,
                    max: 256,
                    suffix: None,
                },
                tier: ApplyTier::NextPrompt,
                get: get_subagent_max_concurrent,
                set: set_subagent_max_concurrent,
                default_display: "20",
                default: || FieldValue::Integer(DEFAULT_SUBAGENT_MAX_CONCURRENT as i64),
                help: "Maximum number of subagents that may run concurrently per parent agent turn. The `/config` UI accepts 1–256; values set via TOML or `SQUEEZY_SUBAGENT_MAX_CONCURRENT` are not capped at the runtime but are clamped to ≥1.",
                env_override: Some("SQUEEZY_SUBAGENT_MAX_CONCURRENT"),
                secret: false,
            },
            FieldMeta {
                label: "max_tool_calls_per_call",
                toml_path: &["subagents", "max_tool_calls_per_call"],
                kind: FieldKind::Integer {
                    min: 1,
                    max: 100_000,
                    suffix: None,
                },
                tier: ApplyTier::NextPrompt,
                get: get_subagent_max_tool_calls,
                set: set_subagent_max_tool_calls,
                default_display: "10000",
                default: || FieldValue::Integer(DEFAULT_SUBAGENT_MAX_TOOL_CALLS_PER_CALL as i64),
                help: "Cap on tool calls within one subagent invocation.",
                env_override: Some("SQUEEZY_SUBAGENT_MAX_TOOL_CALLS_PER_CALL"),
                secret: false,
            },
            FieldMeta {
                label: "max_tool_bytes_read_per_call",
                toml_path: &["subagents", "max_tool_bytes_read_per_call"],
                kind: FieldKind::Integer {
                    min: 65_536,
                    max: 1_000_000_000,
                    suffix: Some("bytes"),
                },
                tier: ApplyTier::NextPrompt,
                get: get_subagent_max_bytes,
                set: set_subagent_max_bytes,
                default_display: "1000000000 bytes",
                default: || {
                    FieldValue::Integer(DEFAULT_SUBAGENT_MAX_TOOL_BYTES_READ_PER_CALL as i64)
                },
                help: "Cap on bytes a subagent can read across all its tool calls.",
                env_override: Some("SQUEEZY_SUBAGENT_MAX_TOOL_BYTES_READ_PER_CALL"),
                secret: false,
            },
            FieldMeta {
                label: "max_search_files_per_call",
                toml_path: &["subagents", "max_search_files_per_call"],
                kind: FieldKind::Integer {
                    min: 100,
                    max: 1_000_000,
                    suffix: Some("files"),
                },
                tier: ApplyTier::NextPrompt,
                get: get_subagent_max_files,
                set: set_subagent_max_files,
                default_display: "1000000 files",
                default: || FieldValue::Integer(DEFAULT_SUBAGENT_MAX_SEARCH_FILES_PER_CALL as i64),
                help: "Cap on files scanned by a subagent's search tools.",
                env_override: Some("SQUEEZY_SUBAGENT_MAX_SEARCH_FILES_PER_CALL"),
                secret: false,
            },
            FieldMeta {
                label: "max_model_rounds",
                toml_path: &["subagents", "max_model_rounds"],
                kind: FieldKind::Integer {
                    min: 1,
                    max: 5_000,
                    suffix: None,
                },
                tier: ApplyTier::NextPrompt,
                get: get_subagent_max_rounds,
                set: set_subagent_max_rounds,
                default_display: "1000",
                default: || FieldValue::Integer(DEFAULT_SUBAGENT_MAX_MODEL_ROUNDS as i64),
                help: "Cap on model-call rounds inside a single subagent invocation.",
                env_override: Some("SQUEEZY_SUBAGENT_MAX_MODEL_ROUNDS"),
                secret: false,
            },
            FieldMeta {
                label: "max_summary_tokens",
                toml_path: &["subagents", "max_summary_tokens"],
                kind: FieldKind::Integer {
                    min: 100,
                    max: 256_000,
                    suffix: Some("tokens"),
                },
                tier: ApplyTier::NextPrompt,
                get: get_subagent_max_summary,
                set: set_subagent_max_summary,
                default_display: "64000 tokens",
                default: || FieldValue::Integer(DEFAULT_SUBAGENT_MAX_SUMMARY_TOKENS as i64),
                help: "Cap on the subagent's summary length back to the main agent.",
                env_override: Some("SQUEEZY_SUBAGENT_MAX_SUMMARY_TOKENS"),
                secret: false,
            },
        ],
    },
    ConfigSectionMeta {
        id: SectionId::Graph,
        label: "Graph",
        description: "Semantic graph indexer (per-language)",
        fields: &[
            FieldMeta {
                label: "languages",
                toml_path: &["graph", "languages"],
                kind: FieldKind::StringList { min: 0, max: 32 },
                tier: ApplyTier::Restart,
                get: get_graph_languages,
                set: set_graph_languages,
                default_display: "rust, python",
                default: || FieldValue::StringList(vec!["rust".to_string(), "python".to_string()]),
                help: "Languages indexed by the semantic graph at startup.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "max_file_bytes",
                toml_path: &["graph", "max_file_bytes"],
                kind: FieldKind::Integer {
                    min: 1024,
                    max: 100_000_000,
                    suffix: Some("bytes"),
                },
                tier: ApplyTier::Restart,
                get: get_graph_max_file_bytes,
                set: set_graph_max_file_bytes,
                default_display: "1000000 bytes",
                default: || FieldValue::Integer(1_000_000),
                help: "Files larger than this are skipped by the indexer.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "include_hidden",
                toml_path: &["graph", "include_hidden"],
                kind: FieldKind::Bool,
                tier: ApplyTier::Restart,
                get: get_graph_include_hidden,
                set: set_graph_include_hidden,
                default_display: "false",
                default: || FieldValue::Bool(false),
                help: "Include dotfiles in the graph index.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "require_indexing_signal",
                toml_path: &["graph", "require_indexing_signal"],
                kind: FieldKind::Bool,
                tier: ApplyTier::Restart,
                get: get_graph_require_signal,
                set: set_graph_require_signal,
                default_display: "true",
                default: || FieldValue::Bool(true),
                help: "Skip files without an explicit indexing hint (improves indexing cost).",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "include",
                toml_path: &["graph", "include"],
                kind: FieldKind::StringList { min: 0, max: 256 },
                tier: ApplyTier::Restart,
                get: get_graph_include,
                set: set_graph_include,
                default_display: "—",
                default: || FieldValue::StringList(Vec::new()),
                help: "Globs to force-include for indexing.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "exclude",
                toml_path: &["graph", "exclude"],
                kind: FieldKind::StringList { min: 0, max: 256 },
                tier: ApplyTier::Restart,
                get: get_graph_exclude,
                set: set_graph_exclude,
                default_display: "—",
                default: || FieldValue::StringList(Vec::new()),
                help: "Globs to exclude from indexing.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "include_classes",
                toml_path: &["graph", "include_classes"],
                kind: FieldKind::StringList { min: 0, max: 32 },
                tier: ApplyTier::Restart,
                get: get_graph_include_classes,
                set: set_graph_include_classes,
                default_display: "—",
                default: || FieldValue::StringList(Vec::new()),
                help: "Exclusion classes to force-include in the graph index.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "exclude_classes",
                toml_path: &["graph", "exclude_classes"],
                kind: FieldKind::StringList { min: 0, max: 32 },
                tier: ApplyTier::Restart,
                get: get_graph_exclude_classes,
                set: set_graph_exclude_classes,
                default_display: "—",
                default: || FieldValue::StringList(Vec::new()),
                help: "Exclusion classes to force-exclude from the graph index.",
                env_override: None,
                secret: false,
            },
        ],
    },
    ConfigSectionMeta {
        id: SectionId::Cache,
        label: "Cache",
        description: "On-disk caches for tool output and the persistent state store",
        fields: &[
            FieldMeta {
                label: "root",
                toml_path: &["cache", "root"],
                kind: FieldKind::Path {
                    must_exist: false,
                    dir_only: true,
                },
                tier: ApplyTier::Restart,
                get: get_cache_root,
                set: set_cache_root,
                default_display: ".squeezy/cache",
                default: || FieldValue::Path(std::path::PathBuf::from(".squeezy/cache")),
                help: "Workspace root for the persistent state store and tool-output cache.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "tool_outputs",
                toml_path: &["cache", "tool_outputs"],
                kind: FieldKind::Path {
                    must_exist: false,
                    dir_only: true,
                },
                tier: ApplyTier::Restart,
                get: get_cache_tool_outputs,
                set: set_cache_tool_outputs,
                default_display: "<inherits cache.root>",
                default: || FieldValue::Path(std::path::PathBuf::from("")),
                help: "Directory for spilled tool-output blobs. Empty inherits cache.root.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "durability",
                toml_path: &["cache", "durability"],
                kind: FieldKind::Enum {
                    options: CACHE_DURABILITY_OPTIONS,
                },
                tier: ApplyTier::Restart,
                get: get_cache_durability,
                set: set_cache_durability,
                default_display: "fast",
                default: || FieldValue::Enum("fast"),
                help: "Session JSONL durability: fast avoids fsync, turn syncs on explicit session flushes, strict syncs each durable append.",
                env_override: None,
                secret: false,
            },
        ],
    },
    ConfigSectionMeta {
        id: SectionId::Feedback,
        label: "Feedback",
        description: "Endpoints and size caps for /feedback and /report",
        fields: &[
            FieldMeta {
                label: "enabled",
                toml_path: &["feedback", "enabled"],
                kind: FieldKind::Bool,
                tier: ApplyTier::Immediate,
                get: get_feedback_enabled,
                set: set_feedback_enabled,
                default_display: "true",
                default: || FieldValue::Bool(true),
                help: "Allow the /feedback and /report commands to upload.",
                env_override: Some("SQUEEZY_FEEDBACK_ENABLED"),
                secret: false,
            },
            FieldMeta {
                label: "feedback_endpoint",
                toml_path: &["feedback", "feedback_endpoint"],
                kind: FieldKind::String { multiline: false },
                tier: ApplyTier::NextPrompt,
                get: get_feedback_endpoint,
                set: set_feedback_endpoint,
                default_display: DEFAULT_FEEDBACK_ENDPOINT,
                default: || FieldValue::String(DEFAULT_FEEDBACK_ENDPOINT.to_string()),
                help: "Where /feedback POSTs.",
                env_override: Some("SQUEEZY_FEEDBACK_ENDPOINT"),
                secret: false,
            },
            FieldMeta {
                label: "report_endpoint",
                toml_path: &["feedback", "report_endpoint"],
                kind: FieldKind::String { multiline: false },
                tier: ApplyTier::NextPrompt,
                get: get_report_endpoint,
                set: set_report_endpoint,
                default_display: DEFAULT_REPORT_ENDPOINT,
                default: || FieldValue::String(DEFAULT_REPORT_ENDPOINT.to_string()),
                help: "Where /report POSTs.",
                env_override: Some("SQUEEZY_REPORT_ENDPOINT"),
                secret: false,
            },
            FieldMeta {
                label: "max_feedback_bytes",
                toml_path: &["feedback", "max_feedback_bytes"],
                kind: FieldKind::Integer {
                    min: 1024,
                    max: 10_000_000,
                    suffix: Some("bytes"),
                },
                tier: ApplyTier::Immediate,
                get: get_feedback_max_bytes,
                set: set_feedback_max_bytes,
                default_display: "16384 bytes",
                default: || FieldValue::Integer(DEFAULT_FEEDBACK_MAX_BYTES as i64),
                help: "Cap on /feedback payload size.",
                env_override: None,
                secret: false,
            },
            FieldMeta {
                label: "max_report_bytes",
                toml_path: &["feedback", "max_report_bytes"],
                kind: FieldKind::Integer {
                    min: 1024,
                    max: 50_000_000,
                    suffix: Some("bytes"),
                },
                tier: ApplyTier::Immediate,
                get: get_report_max_bytes,
                set: set_report_max_bytes,
                default_display: "2097152 bytes",
                default: || FieldValue::Integer(DEFAULT_REPORT_MAX_BYTES as i64),
                help: "Cap on /report payload size.",
                env_override: None,
                secret: false,
            },
        ],
    },
    ConfigSectionMeta {
        id: SectionId::Redaction,
        label: "Redaction",
        description: "Custom secret-masking regex patterns",
        fields: &[FieldMeta {
            label: "custom_patterns",
            toml_path: &["redaction", "custom_patterns"],
            kind: FieldKind::StringList { min: 0, max: 256 },
            tier: ApplyTier::Immediate,
            get: get_redaction_custom_patterns,
            set: set_redaction_custom_patterns,
            default_display: "—",
            default: || FieldValue::StringList(Vec::new()),
            help: "Additional regex patterns to redact from transcripts and logs.",
            env_override: None,
            secret: false,
        }],
    },
    ConfigSectionMeta {
        id: SectionId::Web,
        label: "Web",
        description: "Pluggable websearch backend and fetch",
        fields: &[
            FieldMeta {
                label: "websearch_provider",
                toml_path: &["web", "websearch_provider"],
                kind: FieldKind::String { multiline: false },
                tier: ApplyTier::NextPrompt,
                get: get_websearch_provider,
                set: set_websearch_provider,
                default_display: DEFAULT_WEBSEARCH_PROVIDER,
                default: || FieldValue::String(DEFAULT_WEBSEARCH_PROVIDER.to_string()),
                help: "Websearch backend: \"exa\" or \"parallel\".",
                env_override: Some("SQUEEZY_WEBSEARCH_PROVIDER"),
                secret: false,
            },
            FieldMeta {
                label: "exa_mcp_url",
                toml_path: &["web", "exa_mcp_url"],
                kind: FieldKind::String { multiline: false },
                tier: ApplyTier::NextPrompt,
                get: get_exa_mcp_url,
                set: set_exa_mcp_url,
                default_display: DEFAULT_EXA_MCP_URL,
                default: || FieldValue::String(DEFAULT_EXA_MCP_URL.to_string()),
                help: "Endpoint for the Exa MCP web tool.",
                env_override: Some("SQUEEZY_EXA_MCP_URL"),
                secret: false,
            },
            FieldMeta {
                label: "exa_api_key_env",
                toml_path: &["web", "exa_api_key_env"],
                kind: FieldKind::String { multiline: false },
                tier: ApplyTier::NextPrompt,
                get: get_exa_api_key_env,
                set: set_exa_api_key_env,
                default_display: DEFAULT_EXA_API_KEY_ENV,
                default: || FieldValue::String(DEFAULT_EXA_API_KEY_ENV.to_string()),
                help: "Env var that holds the Exa API key. Use `squeezy auth set exa` to write the value.",
                env_override: Some("SQUEEZY_EXA_API_KEY_ENV"),
                secret: false,
            },
            FieldMeta {
                label: "parallel_mcp_url",
                toml_path: &["web", "parallel_mcp_url"],
                kind: FieldKind::String { multiline: false },
                tier: ApplyTier::NextPrompt,
                get: get_parallel_mcp_url,
                set: set_parallel_mcp_url,
                default_display: DEFAULT_PARALLEL_MCP_URL,
                default: || FieldValue::String(DEFAULT_PARALLEL_MCP_URL.to_string()),
                help: "Endpoint for the Parallel Search MCP web tool.",
                env_override: Some("SQUEEZY_PARALLEL_MCP_URL"),
                secret: false,
            },
            FieldMeta {
                label: "parallel_api_key_env",
                toml_path: &["web", "parallel_api_key_env"],
                kind: FieldKind::String { multiline: false },
                tier: ApplyTier::NextPrompt,
                get: get_parallel_api_key_env,
                set: set_parallel_api_key_env,
                default_display: DEFAULT_PARALLEL_API_KEY_ENV,
                default: || FieldValue::String(DEFAULT_PARALLEL_API_KEY_ENV.to_string()),
                help: "Env var that holds the Parallel Search API key.",
                env_override: Some("SQUEEZY_PARALLEL_API_KEY_ENV"),
                secret: false,
            },
        ],
    },
    ConfigSectionMeta {
        id: SectionId::Tools,
        label: "Tools",
        description: "Local tool behavior such as the checkpoint/undo safety net",
        fields: &[FieldMeta {
            label: "checkpoints_enabled",
            toml_path: &["tools", "checkpoints_enabled"],
            kind: FieldKind::Bool,
            tier: ApplyTier::Restart,
            get: get_checkpoints_enabled,
            set: set_checkpoints_enabled,
            default_display: "false",
            default: || FieldValue::Bool(false),
            help: "Enable local git-snapshot checkpoints with /undo and /revert (off by default).",
            env_override: Some("SQUEEZY_CHECKPOINTS_ENABLED"),
            secret: false,
        }],
    },
    ConfigSectionMeta {
        id: SectionId::McpServers,
        label: "MCP Servers",
        // Render path: the `/mcp` page renders one row per configured
        // server with live status, and handles toggle/restart/add/
        // remove via dedicated key bindings rather than the generic
        // field editor. Fields stay empty here so the regular field
        // navigator skips the section without rendering a stub row.
        description: "Status, enable, disable, restart, add or remove configured MCP servers \
                      (lives behind `/mcp`).",
        fields: &[],
    },
    ConfigSectionMeta {
        id: SectionId::Reset,
        label: "Reset",
        description: "Delete a tier's settings file. Inherited values from \
                      other tiers then take over — no other tab is touched.",
        fields: &[],
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
        DEFAULT_ANTHROPIC_MODEL, DEFAULT_AZURE_OPENAI_API_VERSION, DEFAULT_AZURE_OPENAI_BASE_URL,
        DEFAULT_AZURE_OPENAI_MODEL, DEFAULT_BEDROCK_MODEL, DEFAULT_BEDROCK_REGION,
        DEFAULT_GITHUB_COPILOT_MODEL, DEFAULT_GOOGLE_BASE_URL, DEFAULT_GOOGLE_MODEL,
        DEFAULT_OLLAMA_BASE_URL, DEFAULT_OLLAMA_MODEL, DEFAULT_OPENAI_BASE_URL,
        DEFAULT_OPENAI_CODEX_BASE_URL, DEFAULT_OPENAI_CODEX_MODEL, DEFAULT_OPENAI_CODEX_ORIGINATOR,
        DEFAULT_OPENAI_MODEL, GitHubCopilotConfig, GoogleConfig, OllamaConfig, OpenAiCodexConfig,
        OpenAiCompatibleConfig, OpenAiCompatiblePreset, OpenAiConfig, ProviderTransportConfig,
    };
    let transport = ProviderTransportConfig::default();
    let (provider, default_model) = match s {
        "openai" => (
            ProviderConfig::OpenAi(OpenAiConfig {
                api_key_env: "SQUEEZY_OPENAI_KEY".to_string(),
                api_key: None,
                base_url: DEFAULT_OPENAI_BASE_URL.to_string(),
                organization: None,
                project: None,
                service_tier: None,
                transport,
            }),
            DEFAULT_OPENAI_MODEL,
        ),
        "openai-codex" | "openai_codex" | "chatgpt" => (
            ProviderConfig::OpenAiCodex(OpenAiCodexConfig {
                base_url: DEFAULT_OPENAI_CODEX_BASE_URL.to_string(),
                originator: DEFAULT_OPENAI_CODEX_ORIGINATOR.to_string(),
                transport,
            }),
            DEFAULT_OPENAI_CODEX_MODEL,
        ),
        "github-copilot" | "github_copilot" | "copilot" => (
            ProviderConfig::GitHubCopilot(GitHubCopilotConfig { transport }),
            DEFAULT_GITHUB_COPILOT_MODEL,
        ),
        "anthropic" => (
            ProviderConfig::Anthropic(AnthropicConfig {
                api_key_env: "SQUEEZY_ANTHROPIC_KEY".to_string(),
                api_key: None,
                base_url: DEFAULT_ANTHROPIC_BASE_URL.to_string(),
                transport,
            }),
            DEFAULT_ANTHROPIC_MODEL,
        ),
        "google" => (
            ProviderConfig::Google(GoogleConfig {
                api_key_env: "SQUEEZY_GOOGLE_KEY".to_string(),
                api_key: None,
                base_url: DEFAULT_GOOGLE_BASE_URL.to_string(),
                transport,
            }),
            DEFAULT_GOOGLE_MODEL,
        ),
        "azure_openai" => (
            ProviderConfig::AzureOpenAi(AzureOpenAiConfig {
                api_key_env: "SQUEEZY_AZURE_OPENAI_KEY".to_string(),
                api_key: None,
                base_url: DEFAULT_AZURE_OPENAI_BASE_URL.to_string(),
                api_version: DEFAULT_AZURE_OPENAI_API_VERSION.to_string(),
                deployment_name_map: BTreeMap::new(),
                extra_headers: BTreeMap::new(),
                use_entra_id: false,
                entra_bearer_token: None,
                transport,
            }),
            DEFAULT_AZURE_OPENAI_MODEL,
        ),
        "bedrock" => (
            ProviderConfig::Bedrock(BedrockConfig {
                region: DEFAULT_BEDROCK_REGION.to_string(),
                base_url: None,
                bearer_token: None,
                request_metadata: BTreeMap::new(),
                transport,
            }),
            DEFAULT_BEDROCK_MODEL,
        ),
        "ollama" => (
            ProviderConfig::Ollama(OllamaConfig {
                base_url: DEFAULT_OLLAMA_BASE_URL.to_string(),
                route_style: Default::default(),
                transport,
                ..Default::default()
            }),
            DEFAULT_OLLAMA_MODEL,
        ),
        other => {
            let preset = OpenAiCompatiblePreset::parse(other).ok_or("unknown provider")?;
            let default_model = preset.default_model();
            (
                ProviderConfig::OpenAiCompatible(OpenAiCompatibleConfig {
                    preset,
                    api_key_env: preset.default_api_key_env().to_string(),
                    api_key: None,
                    base_url: preset.default_base_url().to_string(),
                    extra_headers: BTreeMap::new(),
                    transport,
                    account_id: None,
                    gateway_id: None,
                    deployment_id: None,
                    cf_ai_gateway: None,
                    use_oauth: false,
                }),
                default_model,
            )
        }
    };
    cfg.provider = provider;
    cfg.model = default_model.to_string();
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
        ProviderConfig::OpenAiCodex(_) => "openai_codex",
        ProviderConfig::GitHubCopilot(_) => "github_copilot",
        ProviderConfig::OpenAiCompatible(config) => config.preset.as_str(),
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
        "openai_codex" | "openai-codex" | "chatgpt" => DEFAULT_OPENAI_CODEX_MODEL,
        "github_copilot" | "github-copilot" | "copilot" => DEFAULT_GITHUB_COPILOT_MODEL,
        other => match OpenAiCompatiblePreset::parse(other) {
            Some(preset) => preset.default_model(),
            None => DEFAULT_OPENAI_MODEL,
        },
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

fn get_model_context_window(cfg: &AppConfig) -> FieldValue {
    FieldValue::OptionalInteger(
        cfg.model_limits
            .get(&cfg.model_limit_key())
            .and_then(|entry| entry.context_window)
            .map(|window| window as i64),
    )
}
fn set_model_context_window(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let key = cfg.model_limit_key();
    match value {
        FieldValue::OptionalInteger(None) | FieldValue::Unset => {
            cfg.model_limits.remove(&key);
        }
        FieldValue::OptionalInteger(Some(v)) | FieldValue::Integer(v) => {
            if v < 1 {
                return Err("must be >= 1");
            }
            cfg.model_limits.entry(key).or_default().context_window = Some(v as u64);
        }
        _ => return Err("expects integer"),
    }
    Ok(())
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

fn get_temperature(cfg: &AppConfig) -> FieldValue {
    FieldValue::OptionalFloat(cfg.temperature.map(f64::from))
}
fn set_temperature(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    cfg.temperature = optional_f32_range(value, 0.0, 2.0, "temperature")?;
    Ok(())
}

fn get_top_p(cfg: &AppConfig) -> FieldValue {
    FieldValue::OptionalFloat(cfg.top_p.map(f64::from))
}
fn set_top_p(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    cfg.top_p = optional_f32_range(value, 0.0, 1.0, "top_p")?;
    Ok(())
}

fn get_seed(cfg: &AppConfig) -> FieldValue {
    FieldValue::OptionalInteger(cfg.seed.and_then(|seed| i64::try_from(seed).ok()))
}
fn set_seed(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    cfg.seed = match value {
        FieldValue::OptionalInteger(None) | FieldValue::Unset => None,
        FieldValue::OptionalInteger(Some(v)) | FieldValue::Integer(v) => {
            if v < 0 {
                return Err("seed must be >= 0");
            }
            Some(v as u64)
        }
        _ => return Err("seed expects integer"),
    };
    Ok(())
}

fn get_stop_sequences(cfg: &AppConfig) -> FieldValue {
    FieldValue::StringList(cfg.stop.clone())
}
fn set_stop_sequences(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    cfg.stop = match value {
        FieldValue::StringList(items) => items,
        FieldValue::Unset => Vec::new(),
        _ => return Err("stop expects string list"),
    };
    Ok(())
}

fn get_frequency_penalty(cfg: &AppConfig) -> FieldValue {
    FieldValue::OptionalFloat(cfg.frequency_penalty.map(f64::from))
}
fn set_frequency_penalty(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    cfg.frequency_penalty = optional_f32_range(value, -2.0, 2.0, "frequency_penalty")?;
    Ok(())
}

fn get_presence_penalty(cfg: &AppConfig) -> FieldValue {
    FieldValue::OptionalFloat(cfg.presence_penalty.map(f64::from))
}
fn set_presence_penalty(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    cfg.presence_penalty = optional_f32_range(value, -2.0, 2.0, "presence_penalty")?;
    Ok(())
}

fn optional_f32_range(
    value: FieldValue,
    min: f64,
    max: f64,
    label: &'static str,
) -> Result<Option<f32>, &'static str> {
    let value = match value {
        FieldValue::OptionalFloat(None) | FieldValue::Unset => return Ok(None),
        FieldValue::OptionalFloat(Some(v)) => v,
        _ => return Err("sampling option expects number"),
    };
    if !value.is_finite() || value < min || value > max {
        return Err(match label {
            "temperature" => "temperature out of range",
            "top_p" => "top_p out of range",
            "frequency_penalty" => "frequency_penalty out of range",
            "presence_penalty" => "presence_penalty out of range",
            _ => "value out of range",
        });
    }
    Ok(Some(value as f32))
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

fn get_context_1m(cfg: &AppConfig) -> FieldValue {
    FieldValue::Bool(cfg.context_1m)
}
fn set_context_1m(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::Bool(v) => {
            cfg.context_1m = v;
            Ok(())
        }
        _ => Err("context_1m expects bool"),
    }
}

fn get_extended_thinking(cfg: &AppConfig) -> FieldValue {
    FieldValue::Bool(cfg.extended_thinking)
}
fn set_extended_thinking(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::Bool(v) => {
            cfg.extended_thinking = v;
            Ok(())
        }
        _ => Err("extended_thinking expects bool"),
    }
}

// Permissions

fn get_perm_mode(cfg: &AppConfig) -> FieldValue {
    FieldValue::Enum(cfg.permissions.mode.as_str())
}
fn set_perm_mode(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let s = match value {
        FieldValue::Enum(s) => s,
        _ => return Err("permission mode expects enum"),
    };
    let mode = PermissionPolicyMode::parse(s).ok_or("invalid permission policy mode")?;
    cfg.permissions.apply_mode(mode);
    Ok(())
}

fn get_ai_reviewer_model(cfg: &AppConfig) -> FieldValue {
    // Show the model that will actually run: the explicit override, else the
    // provider's resolved small/fast tier, else the main model — matching the
    // reviewer's own resolution. Clearing the override reverts the row to the
    // resolved default rather than a blank cell.
    let resolved = cfg
        .permissions
        .ai_reviewer
        .model
        .clone()
        .or_else(|| cfg.resolved_small_fast_model())
        .unwrap_or_else(|| cfg.model.clone());
    FieldValue::String(resolved)
}
// Unlike the per-capability setters, this does not force mode = Custom: the
// reviewer model is meaningful under the Auto-review preset.
fn set_ai_reviewer_model(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let s = match value {
        FieldValue::String(s) => s,
        _ => return Err("reviewer model expects string"),
    };
    let trimmed = s.trim();
    cfg.permissions.ai_reviewer.model = if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    };
    Ok(())
}

fn get_ai_reviewer_policy_file(cfg: &AppConfig) -> FieldValue {
    FieldValue::String(
        cfg.permissions
            .ai_reviewer
            .policy_file
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
    )
}
fn set_ai_reviewer_policy_file(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let s = match value {
        FieldValue::String(s) => s,
        _ => return Err("reviewer policy file expects string"),
    };
    let trimmed = s.trim();
    cfg.permissions.ai_reviewer.policy_file = if trimmed.is_empty() {
        None
    } else {
        Some(std::path::PathBuf::from(trimmed))
    };
    Ok(())
}

fn get_ai_reviewer_policy_text(cfg: &AppConfig) -> FieldValue {
    FieldValue::String(
        cfg.permissions
            .ai_reviewer
            .policy
            .clone()
            .unwrap_or_default(),
    )
}
fn set_ai_reviewer_policy_text(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let s = match value {
        FieldValue::String(s) => s,
        _ => return Err("reviewer policy expects string"),
    };
    let trimmed = s.trim();
    cfg.permissions.ai_reviewer.policy = if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    };
    Ok(())
}

fn get_ai_reviewer_capabilities(cfg: &AppConfig) -> FieldValue {
    FieldValue::StringList(
        cfg.permissions
            .ai_reviewer
            .allow_capabilities
            .iter()
            .map(|capability| capability.as_str().to_string())
            .collect(),
    )
}
fn set_ai_reviewer_capabilities(
    cfg: &mut AppConfig,
    value: FieldValue,
) -> Result<(), &'static str> {
    let FieldValue::StringList(items) = value else {
        return Err("reviewer capabilities expects string list");
    };
    let mut caps = Vec::new();
    for item in items {
        let capability =
            crate::PermissionCapability::parse(item.trim()).ok_or("invalid reviewer capability")?;
        if !caps.contains(&capability) {
            caps.push(capability);
        }
    }
    cfg.permissions.ai_reviewer.allow_capabilities = caps;
    Ok(())
}
fn get_perm_read(cfg: &AppConfig) -> FieldValue {
    FieldValue::Enum(cfg.permissions.read.as_str())
}
fn set_perm_read(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    set_perm(value, &mut cfg.permissions.read)?;
    cfg.permissions.mode = PermissionPolicyMode::Custom;
    Ok(())
}
fn get_perm_search(cfg: &AppConfig) -> FieldValue {
    FieldValue::Enum(cfg.permissions.search.as_str())
}
fn set_perm_search(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    set_perm(value, &mut cfg.permissions.search)?;
    cfg.permissions.mode = PermissionPolicyMode::Custom;
    Ok(())
}
fn get_perm_edit(cfg: &AppConfig) -> FieldValue {
    FieldValue::Enum(cfg.permissions.edit.as_str())
}
fn set_perm_edit(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    set_perm(value, &mut cfg.permissions.edit)?;
    cfg.permissions.mode = PermissionPolicyMode::Custom;
    Ok(())
}
fn get_perm_shell(cfg: &AppConfig) -> FieldValue {
    FieldValue::Enum(cfg.permissions.shell.as_str())
}
fn set_perm_shell(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    set_perm(value, &mut cfg.permissions.shell)?;
    cfg.permissions.mode = PermissionPolicyMode::Custom;
    Ok(())
}
fn get_perm_ignored_search(cfg: &AppConfig) -> FieldValue {
    FieldValue::Enum(cfg.permissions.ignored_search.as_str())
}
fn set_perm_ignored_search(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    set_perm(value, &mut cfg.permissions.ignored_search)?;
    cfg.permissions.mode = PermissionPolicyMode::Custom;
    Ok(())
}
fn get_perm_web(cfg: &AppConfig) -> FieldValue {
    FieldValue::Enum(cfg.permissions.web.as_str())
}
fn set_perm_web(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    set_perm(value, &mut cfg.permissions.web)?;
    cfg.permissions.mode = PermissionPolicyMode::Custom;
    Ok(())
}
fn get_perm_mcp(cfg: &AppConfig) -> FieldValue {
    FieldValue::Enum(cfg.permissions.mcp.as_str())
}
fn set_perm_mcp(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    set_perm(value, &mut cfg.permissions.mcp)?;
    cfg.permissions.mode = PermissionPolicyMode::Custom;
    Ok(())
}
fn get_perm_git(cfg: &AppConfig) -> FieldValue {
    FieldValue::Enum(cfg.permissions.git.as_str())
}
fn set_perm_git(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    set_perm(value, &mut cfg.permissions.git)?;
    cfg.permissions.mode = PermissionPolicyMode::Custom;
    Ok(())
}
fn get_perm_compiler(cfg: &AppConfig) -> FieldValue {
    FieldValue::Enum(cfg.permissions.compiler.as_str())
}
fn set_perm_compiler(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    set_perm(value, &mut cfg.permissions.compiler)?;
    cfg.permissions.mode = PermissionPolicyMode::Custom;
    Ok(())
}
fn get_perm_destructive(cfg: &AppConfig) -> FieldValue {
    FieldValue::Enum(cfg.permissions.destructive.as_str())
}
fn set_perm_destructive(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    set_perm(value, &mut cfg.permissions.destructive)?;
    cfg.permissions.mode = PermissionPolicyMode::Custom;
    Ok(())
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

fn get_status_line(cfg: &AppConfig) -> FieldValue {
    FieldValue::StringList(cfg.tui.status_line.clone().unwrap_or_default())
}
fn set_status_line(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::StringList(items) => {
            // Empty list = "unset" so the renderer falls back to the built-in
            // default. Anything else is persisted as-is; the TUI validates
            // identifiers when constructing the picker state.
            cfg.tui.status_line = if items.is_empty() { None } else { Some(items) };
            Ok(())
        }
        _ => Err("expects string list"),
    }
}

fn get_status_line_use_colors(cfg: &AppConfig) -> FieldValue {
    FieldValue::Bool(cfg.tui.status_line_use_colors)
}
fn set_status_line_use_colors(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::Bool(v) => {
            cfg.tui.status_line_use_colors = v;
            Ok(())
        }
        _ => Err("expects bool"),
    }
}

fn get_zen(cfg: &AppConfig) -> FieldValue {
    FieldValue::Bool(cfg.tui.zen)
}
fn set_zen(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::Bool(v) => {
            cfg.tui.zen = v;
            Ok(())
        }
        _ => Err("expects bool"),
    }
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

fn get_desktop_notifications(cfg: &AppConfig) -> FieldValue {
    FieldValue::Enum(cfg.tui.desktop_notifications.as_str())
}
fn set_desktop_notifications(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let s = match value {
        FieldValue::Enum(s) => s,
        _ => return Err("expects enum"),
    };
    cfg.tui.desktop_notifications =
        NotificationMethod::parse(s).ok_or("invalid desktop_notifications")?;
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

fn get_coalesce_tool_runs(cfg: &AppConfig) -> FieldValue {
    FieldValue::Bool(cfg.tui.coalesce_tool_runs)
}
fn set_coalesce_tool_runs(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::Bool(v) => {
            cfg.tui.coalesce_tool_runs = v;
            Ok(())
        }
        _ => Err("expects bool"),
    }
}

fn get_persist_prompt_history(cfg: &AppConfig) -> FieldValue {
    FieldValue::Bool(cfg.tui.persist_prompt_history)
}
fn set_persist_prompt_history(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::Bool(v) => {
            cfg.tui.persist_prompt_history = v;
            Ok(())
        }
        _ => Err("expects bool"),
    }
}

fn get_copy_on_select(cfg: &AppConfig) -> FieldValue {
    FieldValue::Bool(cfg.tui.copy_on_select)
}
fn set_copy_on_select(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::Bool(v) => {
            cfg.tui.copy_on_select = v;
            Ok(())
        }
        _ => Err("expects bool"),
    }
}

fn get_first_run_hints(cfg: &AppConfig) -> FieldValue {
    FieldValue::Bool(cfg.tui.first_run_hints)
}
fn set_first_run_hints(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::Bool(v) => {
            cfg.tui.first_run_hints = v;
            Ok(())
        }
        _ => Err("expects bool"),
    }
}

fn get_synchronized_output(cfg: &AppConfig) -> FieldValue {
    FieldValue::Enum(cfg.tui.synchronized_output.as_str())
}
fn set_synchronized_output(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let s = match value {
        FieldValue::Enum(s) => s,
        _ => return Err("expects enum"),
    };
    cfg.tui.synchronized_output =
        TuiSynchronizedOutput::parse(s).ok_or("invalid synchronized_output")?;
    Ok(())
}

fn get_theme(cfg: &AppConfig) -> FieldValue {
    FieldValue::String(cfg.tui.theme.clone())
}
fn set_theme(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let s = match value {
        FieldValue::String(s) => s,
        FieldValue::Enum(s) => s.to_string(),
        _ => return Err("expects theme name"),
    };
    cfg.tui.theme = normalize_tui_theme_name(&s).ok_or("invalid theme")?;
    Ok(())
}

fn get_spinner(cfg: &AppConfig) -> FieldValue {
    let name = crate::BUILTIN_TUI_SPINNER_NAMES
        .iter()
        .copied()
        .find(|s| *s == cfg.tui.spinner)
        .unwrap_or(DEFAULT_TUI_SPINNER_NAME);
    FieldValue::Enum(name)
}
fn set_spinner(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let s = match value {
        FieldValue::String(s) => s,
        FieldValue::Enum(s) => s.to_string(),
        _ => return Err("expects spinner name"),
    };
    cfg.tui.spinner = normalize_tui_spinner_name(&s).ok_or("invalid spinner")?;
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

fn get_max_round_input_tokens(cfg: &AppConfig) -> FieldValue {
    FieldValue::OptionalInteger(cfg.max_round_input_tokens.map(|v| v as i64))
}
fn set_max_round_input_tokens(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    cfg.max_round_input_tokens = match value {
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

fn get_routing_enabled(cfg: &AppConfig) -> FieldValue {
    FieldValue::Bool(cfg.routing.enabled)
}
fn set_routing_enabled(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::Bool(v) => {
            cfg.routing.enabled = v;
            Ok(())
        }
        _ => Err("expects bool"),
    }
}

fn get_routing_heuristic(cfg: &AppConfig) -> FieldValue {
    FieldValue::Bool(cfg.routing.heuristic)
}
fn set_routing_heuristic(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::Bool(v) => {
            cfg.routing.heuristic = v;
            Ok(())
        }
        _ => Err("expects bool"),
    }
}

fn get_routing_cache_isolation(cfg: &AppConfig) -> FieldValue {
    FieldValue::Enum(cfg.routing.cache_isolation.as_str())
}
fn set_routing_cache_isolation(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let s = match value {
        FieldValue::Enum(s) | FieldValue::OptionalEnum(Some(s)) => s,
        _ => return Err("expects enum"),
    };
    cfg.routing.cache_isolation =
        crate::CacheIsolation::parse(s).ok_or("invalid cache_isolation")?;
    Ok(())
}
fn get_routing_auto_prefix_token_threshold(cfg: &AppConfig) -> FieldValue {
    FieldValue::Integer(cfg.routing.auto_prefix_token_threshold as i64)
}
fn set_routing_auto_prefix_token_threshold(
    cfg: &mut AppConfig,
    value: FieldValue,
) -> Result<(), &'static str> {
    match value {
        FieldValue::Integer(v) if v >= 0 => {
            cfg.routing.auto_prefix_token_threshold = v as u64;
            Ok(())
        }
        FieldValue::Integer(_) => Err("must be >= 0"),
        _ => Err("expects integer"),
    }
}

fn get_routing_llm_judge(cfg: &AppConfig) -> FieldValue {
    FieldValue::Bool(cfg.routing.llm_judge)
}
fn set_routing_llm_judge(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::Bool(v) => {
            cfg.routing.llm_judge = v;
            Ok(())
        }
        _ => Err("expects bool"),
    }
}

fn get_routing_tier_effort(cfg: &AppConfig) -> FieldValue {
    FieldValue::Bool(cfg.routing.tier_effort)
}
fn set_routing_tier_effort(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::Bool(v) => {
            cfg.routing.tier_effort = v;
            Ok(())
        }
        _ => Err("expects bool"),
    }
}

fn get_routing_judge_effort(cfg: &AppConfig) -> FieldValue {
    FieldValue::Bool(cfg.routing.judge_effort)
}
fn set_routing_judge_effort(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::Bool(v) => {
            cfg.routing.judge_effort = v;
            Ok(())
        }
        _ => Err("expects bool"),
    }
}

fn parse_optional_effort(value: FieldValue) -> Result<Option<ReasoningEffort>, &'static str> {
    match value {
        FieldValue::OptionalEnum(None) | FieldValue::Unset => Ok(None),
        FieldValue::OptionalEnum(Some(s)) | FieldValue::Enum(s) => Ok(Some(
            ReasoningEffort::parse(s).ok_or("invalid reasoning effort")?,
        )),
        _ => Err("expects enum"),
    }
}
fn get_routing_effort_weak(cfg: &AppConfig) -> FieldValue {
    FieldValue::OptionalEnum(cfg.routing.effort_weak.map(|e| e.as_str()))
}
fn set_routing_effort_weak(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    cfg.routing.effort_weak = parse_optional_effort(value)?;
    Ok(())
}
fn get_routing_effort_medium(cfg: &AppConfig) -> FieldValue {
    FieldValue::OptionalEnum(cfg.routing.effort_medium.map(|e| e.as_str()))
}
fn set_routing_effort_medium(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    cfg.routing.effort_medium = parse_optional_effort(value)?;
    Ok(())
}
fn get_routing_effort_strong(cfg: &AppConfig) -> FieldValue {
    FieldValue::OptionalEnum(cfg.routing.effort_strong.map(|e| e.as_str()))
}
fn set_routing_effort_strong(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    cfg.routing.effort_strong = parse_optional_effort(value)?;
    Ok(())
}

// Per-provider routing model fields. They read/write `cfg.providers[<active
// provider>]` so switching the active provider (in Models) instantly shows that
// provider's own settings; routing never crosses providers. Persistence routes
// through a `[providers.<slug>]` table write (see config_screen save).

/// Canonical slug of the active provider — the key into `cfg.providers`.
fn active_routing_provider(cfg: &AppConfig) -> &'static str {
    crate::provider_kind(&cfg.provider)
}

fn set_noop(_cfg: &mut AppConfig, _value: FieldValue) -> Result<(), &'static str> {
    Ok(())
}

// ---- Context & Compaction ----

/// Read-only summary of the resolved window and where each tier fires, computed
/// from the live `ContextCompactionConfig` helpers so it always matches runtime.
fn get_context_trigger_info(cfg: &AppConfig) -> FieldValue {
    let cc = &cfg.context_compaction;
    let window = match cc.model_context_window {
        Some(w) if w > 0 => format!("window {w} tok"),
        _ => format!("window {} tok (fallback)", cc.fallback_window_tokens),
    };
    let trim = if cc.micro_compaction_enabled {
        format!("trim @{} ({}%)", cc.trim_threshold(), cc.trim_at_percent)
    } else {
        "trim off".to_string()
    };
    let warn = format!("warn @{} ({}%)", cc.warn_threshold(), cc.warn_at_percent);
    let summarize = if cc.enabled {
        format!("summarize @{} (effective window)", cc.summarize_threshold())
    } else {
        "summarize off".to_string()
    };
    let cap = match cc.max_context_tokens {
        Some(c) if c > 0 => format!("  ·  cap {c} tok"),
        _ => String::new(),
    };
    FieldValue::String(format!(
        "{window}  ·  {trim}  ·  {warn}  ·  {summarize}{cap}"
    ))
}

fn ctx_integer(value: FieldValue, min: i64) -> Result<i64, &'static str> {
    match value {
        FieldValue::Integer(v) | FieldValue::OptionalInteger(Some(v)) if v >= min => Ok(v),
        FieldValue::Integer(_) | FieldValue::OptionalInteger(Some(_)) => Err("value below minimum"),
        _ => Err("expects integer"),
    }
}

fn ctx_percent(value: FieldValue) -> Result<u8, &'static str> {
    match value {
        FieldValue::Integer(v) if (0..=100).contains(&v) => Ok(v as u8),
        FieldValue::Integer(_) => Err("must be 0..=100"),
        _ => Err("expects integer"),
    }
}

fn ctx_bool(value: FieldValue) -> Result<bool, &'static str> {
    match value {
        FieldValue::Bool(v) => Ok(v),
        _ => Err("expects bool"),
    }
}

fn get_context_compaction_enabled(cfg: &AppConfig) -> FieldValue {
    FieldValue::Bool(cfg.context_compaction.enabled)
}
fn set_context_compaction_enabled(
    cfg: &mut AppConfig,
    value: FieldValue,
) -> Result<(), &'static str> {
    cfg.context_compaction.enabled = ctx_bool(value)?;
    Ok(())
}

fn get_context_fallback_window(cfg: &AppConfig) -> FieldValue {
    FieldValue::Integer(cfg.context_compaction.fallback_window_tokens as i64)
}
fn set_context_fallback_window(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    cfg.context_compaction.fallback_window_tokens = ctx_integer(value, 1)? as u64;
    Ok(())
}

fn get_context_max_context_tokens(cfg: &AppConfig) -> FieldValue {
    FieldValue::OptionalInteger(cfg.context_compaction.max_context_tokens.map(|v| v as i64))
}
fn set_context_max_context_tokens(
    cfg: &mut AppConfig,
    value: FieldValue,
) -> Result<(), &'static str> {
    cfg.context_compaction.max_context_tokens = match value {
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

fn get_context_min_items(cfg: &AppConfig) -> FieldValue {
    FieldValue::Integer(cfg.context_compaction.min_items as i64)
}
fn set_context_min_items(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    cfg.context_compaction.min_items = ctx_integer(value, 1)? as usize;
    Ok(())
}

fn get_context_recent_items(cfg: &AppConfig) -> FieldValue {
    FieldValue::Integer(cfg.context_compaction.recent_items as i64)
}
fn set_context_recent_items(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    cfg.context_compaction.recent_items = ctx_integer(value, 1)? as usize;
    Ok(())
}

fn get_context_max_summary_bytes(cfg: &AppConfig) -> FieldValue {
    FieldValue::Integer(cfg.context_compaction.max_summary_bytes as i64)
}
fn set_context_max_summary_bytes(
    cfg: &mut AppConfig,
    value: FieldValue,
) -> Result<(), &'static str> {
    cfg.context_compaction.max_summary_bytes = ctx_integer(value, 256)? as usize;
    Ok(())
}

fn get_context_enabled_mid_turn(cfg: &AppConfig) -> FieldValue {
    FieldValue::Bool(cfg.context_compaction.enabled_mid_turn)
}
fn set_context_enabled_mid_turn(
    cfg: &mut AppConfig,
    value: FieldValue,
) -> Result<(), &'static str> {
    cfg.context_compaction.enabled_mid_turn = ctx_bool(value)?;
    Ok(())
}

fn get_context_model_window(cfg: &AppConfig) -> FieldValue {
    FieldValue::OptionalInteger(
        cfg.context_compaction
            .model_context_window
            .map(|v| v as i64),
    )
}
fn set_context_model_window(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    cfg.context_compaction.model_context_window = match value {
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

fn get_context_effective_percent(cfg: &AppConfig) -> FieldValue {
    FieldValue::OptionalInteger(
        cfg.context_compaction
            .effective_context_window_percent
            .map(|v| v as i64),
    )
}
fn set_context_effective_percent(
    cfg: &mut AppConfig,
    value: FieldValue,
) -> Result<(), &'static str> {
    cfg.context_compaction.effective_context_window_percent = match value {
        FieldValue::OptionalInteger(None) | FieldValue::Unset => None,
        FieldValue::OptionalInteger(Some(v)) | FieldValue::Integer(v) => {
            if !(1..=100).contains(&v) {
                return Err("must be 1..=100");
            }
            Some(v as u8)
        }
        _ => return Err("expects integer"),
    };
    Ok(())
}

fn get_context_baseline_reserve(cfg: &AppConfig) -> FieldValue {
    FieldValue::OptionalInteger(
        cfg.context_compaction
            .baseline_reserve_tokens
            .map(|v| v as i64),
    )
}
fn set_context_baseline_reserve(
    cfg: &mut AppConfig,
    value: FieldValue,
) -> Result<(), &'static str> {
    cfg.context_compaction.baseline_reserve_tokens = match value {
        FieldValue::OptionalInteger(None) | FieldValue::Unset => None,
        FieldValue::OptionalInteger(Some(v)) | FieldValue::Integer(v) => {
            if v < 0 {
                return Err("must be >= 0");
            }
            Some(v as u64)
        }
        _ => return Err("expects integer"),
    };
    Ok(())
}

fn get_context_warn_at_percent(cfg: &AppConfig) -> FieldValue {
    FieldValue::Integer(cfg.context_compaction.warn_at_percent as i64)
}
fn set_context_warn_at_percent(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    cfg.context_compaction.warn_at_percent = ctx_percent(value)?;
    Ok(())
}

fn get_context_micro_enabled(cfg: &AppConfig) -> FieldValue {
    FieldValue::Bool(cfg.context_compaction.micro_compaction_enabled)
}
fn set_context_micro_enabled(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    cfg.context_compaction.micro_compaction_enabled = ctx_bool(value)?;
    Ok(())
}

fn get_context_trim_at_percent(cfg: &AppConfig) -> FieldValue {
    FieldValue::Integer(cfg.context_compaction.trim_at_percent as i64)
}
fn set_context_trim_at_percent(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    cfg.context_compaction.trim_at_percent = ctx_percent(value)?;
    Ok(())
}

fn get_context_micro_keep_recent(cfg: &AppConfig) -> FieldValue {
    FieldValue::Integer(cfg.context_compaction.micro_compaction_keep_recent as i64)
}
fn set_context_micro_keep_recent(
    cfg: &mut AppConfig,
    value: FieldValue,
) -> Result<(), &'static str> {
    cfg.context_compaction.micro_compaction_keep_recent = ctx_integer(value, 0)? as usize;
    Ok(())
}

fn get_context_strategy(cfg: &AppConfig) -> FieldValue {
    FieldValue::Enum(cfg.context_compaction.strategy.as_str())
}
fn set_context_strategy(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let s = match value {
        FieldValue::Enum(s) => s,
        _ => return Err("strategy expects enum"),
    };
    cfg.context_compaction.strategy = CompactionStrategy::parse(s).ok_or("unknown strategy")?;
    Ok(())
}

fn get_context_model_assisted_model(cfg: &AppConfig) -> FieldValue {
    FieldValue::String(
        cfg.context_compaction
            .model_assisted_model
            .clone()
            .unwrap_or_default(),
    )
}
fn set_context_model_assisted_model(
    cfg: &mut AppConfig,
    value: FieldValue,
) -> Result<(), &'static str> {
    let s = match value {
        FieldValue::String(s) => s,
        _ => return Err("expects string"),
    };
    let trimmed = s.trim();
    cfg.context_compaction.model_assisted_model = if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    };
    Ok(())
}

fn get_context_model_assisted_max_output_tokens(cfg: &AppConfig) -> FieldValue {
    FieldValue::Integer(cfg.context_compaction.model_assisted_max_output_tokens as i64)
}
fn set_context_model_assisted_max_output_tokens(
    cfg: &mut AppConfig,
    value: FieldValue,
) -> Result<(), &'static str> {
    cfg.context_compaction.model_assisted_max_output_tokens = ctx_integer(value, 1)? as u32;
    Ok(())
}

fn get_context_model_assisted_timeout_secs(cfg: &AppConfig) -> FieldValue {
    FieldValue::Integer(cfg.context_compaction.model_assisted_timeout_secs as i64)
}
fn set_context_model_assisted_timeout_secs(
    cfg: &mut AppConfig,
    value: FieldValue,
) -> Result<(), &'static str> {
    cfg.context_compaction.model_assisted_timeout_secs = ctx_integer(value, 1)? as u64;
    Ok(())
}

fn get_context_layered_fallback_threshold(cfg: &AppConfig) -> FieldValue {
    FieldValue::Integer(
        cfg.context_compaction
            .layered_fallback_extractive_threshold_tokens as i64,
    )
}
fn set_context_layered_fallback_threshold(
    cfg: &mut AppConfig,
    value: FieldValue,
) -> Result<(), &'static str> {
    cfg.context_compaction
        .layered_fallback_extractive_threshold_tokens = ctx_integer(value, 0)? as u32;
    Ok(())
}

fn get_context_repo_doc_max_bytes(cfg: &AppConfig) -> FieldValue {
    FieldValue::Integer(cfg.context_compaction.repo_doc_max_bytes as i64)
}
fn set_context_repo_doc_max_bytes(
    cfg: &mut AppConfig,
    value: FieldValue,
) -> Result<(), &'static str> {
    cfg.context_compaction.repo_doc_max_bytes = ctx_integer(value, 0)? as usize;
    Ok(())
}

fn get_context_user_memory_max_bytes(cfg: &AppConfig) -> FieldValue {
    FieldValue::Integer(cfg.context_compaction.user_memory_max_bytes as i64)
}
fn set_context_user_memory_max_bytes(
    cfg: &mut AppConfig,
    value: FieldValue,
) -> Result<(), &'static str> {
    cfg.context_compaction.user_memory_max_bytes = ctx_integer(value, 0)? as usize;
    Ok(())
}

fn get_routing_provider_info(cfg: &AppConfig) -> FieldValue {
    let slug = active_routing_provider(cfg);
    let cheap = resolved_cheap_model(cfg, slug);
    let headline = cfg.model.trim();
    // The cheap target and the reroute filter are shown in the rows below, so
    // the banner only carries the pinned-provider note plus a flag when routing
    // can't fire at all: no distinct cheaper model, or the headline is itself
    // excluded by the filter (a cheap tier).
    let inactive = if cheap.trim().is_empty() || cheap == headline {
        Some("no cheaper model — set cheap_model")
    } else if !crate::parent_is_reroute_eligible(
        headline,
        &crate::resolved_reroute_filter(cfg, slug),
    ) {
        Some("current model excluded by expensive_models")
    } else {
        None
    };
    match inactive {
        Some(reason) => FieldValue::String(format!(
            "{slug}  ·  routing inactive — {reason}  ·  pinned (change provider in Models)"
        )),
        None => FieldValue::String(format!("{slug}  ·  pinned (change provider in Models)")),
    }
}

// Resolution helpers. `resolved_*` is what's actually used for the active
// provider (per-provider override → legacy global → built-in). `default_*` is
// the same chain WITHOUT the per-provider override, so a setter can store
// `None` when the user keeps the inherited value (the field stays "default"
// rather than pinning a redundant override).

fn resolved_cheap_model(cfg: &AppConfig, slug: &str) -> String {
    cfg.providers
        .get(slug)
        .and_then(|p| p.cheap_model.clone())
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| default_cheap_model(cfg, slug))
}
fn default_cheap_model(cfg: &AppConfig, slug: &str) -> String {
    // The reroute target defaults to the provider's mini tier (not nano): a
    // notch above the cheapest model judges and handles easy turns far more
    // reliably. `small_fast_model` (used for titles/summaries) stays a legacy
    // global override. Mirrors `cheap_model_for` in squeezy-agent exactly,
    // including the single-model `ollama` fall-through, so the config display
    // never disagrees with what the router actually uses.
    cfg.small_fast_model
        .clone()
        .or_else(|| crate::judge_model_for_provider(slug).map(str::to_string))
        .or_else(|| (slug == "ollama").then(|| crate::DEFAULT_OLLAMA_MODEL.to_string()))
        .unwrap_or_default()
}
fn resolved_medium_model(cfg: &AppConfig, slug: &str) -> String {
    cfg.providers
        .get(slug)
        .and_then(|p| p.medium_model.clone())
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| default_medium_model(slug))
}
fn default_medium_model(slug: &str) -> String {
    // The ladder's mid rung defaults to the provider's Sonnet-class model
    // (`medium_model_for_provider`); empty for providers with no distinct middle
    // tier, where the ladder then collapses to cheap↔parent.
    crate::medium_model_for_provider(slug)
        .map(str::to_string)
        .unwrap_or_default()
}
fn resolved_judge_model(cfg: &AppConfig, slug: &str) -> String {
    cfg.providers
        .get(slug)
        .and_then(|p| p.judge_model.clone())
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| default_judge_model(cfg, slug))
}
fn default_judge_model(cfg: &AppConfig, slug: &str) -> String {
    cfg.routing
        .judge_model
        .clone()
        .or_else(|| crate::judge_model_for_provider(slug).map(str::to_string))
        .unwrap_or_else(|| resolved_cheap_model(cfg, slug))
}
fn resolved_judge_prompt(cfg: &AppConfig, slug: &str) -> String {
    cfg.providers
        .get(slug)
        .and_then(|p| p.judge_prompt.clone())
        .filter(|p| !p.trim().is_empty())
        .unwrap_or_else(|| default_judge_prompt_for(cfg, slug))
}
fn default_judge_prompt_for(cfg: &AppConfig, slug: &str) -> String {
    cfg.routing
        .judge_prompt
        .clone()
        .unwrap_or_else(|| crate::default_judge_prompt(slug).to_string())
}

fn get_provider_cheap_model(cfg: &AppConfig) -> FieldValue {
    FieldValue::String(resolved_cheap_model(cfg, active_routing_provider(cfg)))
}
fn set_provider_cheap_model(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let s = match value {
        FieldValue::String(s) => s,
        _ => return Err("expects string"),
    };
    let slug = active_routing_provider(cfg);
    let keep = s.trim().is_empty() || s == default_cheap_model(cfg, slug);
    let slug = slug.to_string();
    cfg.providers.entry(slug).or_default().cheap_model = (!keep).then_some(s);
    Ok(())
}

fn get_provider_medium_model(cfg: &AppConfig) -> FieldValue {
    FieldValue::String(resolved_medium_model(cfg, active_routing_provider(cfg)))
}
fn set_provider_medium_model(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let s = match value {
        FieldValue::String(s) => s,
        _ => return Err("expects string"),
    };
    let slug = active_routing_provider(cfg);
    let keep = s.trim().is_empty() || s == default_medium_model(slug);
    let slug = slug.to_string();
    cfg.providers.entry(slug).or_default().medium_model = (!keep).then_some(s);
    Ok(())
}

fn get_provider_judge_model(cfg: &AppConfig) -> FieldValue {
    FieldValue::String(resolved_judge_model(cfg, active_routing_provider(cfg)))
}
fn set_provider_judge_model(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let s = match value {
        FieldValue::String(s) => s,
        _ => return Err("expects string"),
    };
    let slug = active_routing_provider(cfg);
    let keep = s.trim().is_empty() || s == default_judge_model(cfg, slug);
    let slug = slug.to_string();
    cfg.providers.entry(slug).or_default().judge_model = (!keep).then_some(s);
    Ok(())
}

fn get_provider_judge_prompt(cfg: &AppConfig) -> FieldValue {
    FieldValue::String(resolved_judge_prompt(cfg, active_routing_provider(cfg)))
}
fn set_provider_judge_prompt(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let s = match value {
        FieldValue::String(s) => s,
        _ => return Err("expects string"),
    };
    let slug = active_routing_provider(cfg);
    let keep = s.trim().is_empty() || s == default_judge_prompt_for(cfg, slug);
    let slug = slug.to_string();
    cfg.providers.entry(slug).or_default().judge_prompt = (!keep).then_some(s);
    Ok(())
}

fn get_provider_expensive_models(cfg: &AppConfig) -> FieldValue {
    // The reroute filter in effect: one case-insensitive regex, a leading `!`
    // excludes. The per-provider default (e.g. `!nano|mini`) reroutes every
    // flagship while skipping already-cheap tiers. An explicit empty string
    // ("reroute any") renders as "any" in the field pane.
    FieldValue::String(crate::resolved_reroute_filter(
        cfg,
        active_routing_provider(cfg),
    ))
}
fn set_provider_expensive_models(
    cfg: &mut AppConfig,
    value: FieldValue,
) -> Result<(), &'static str> {
    let filter = match value {
        FieldValue::String(s) => s,
        _ => return Err("expects string"),
    };
    let slug = active_routing_provider(cfg);
    // Store `None` (inherit) only when the value equals what would resolve
    // anyway — the non-empty global filter, else the built-in per-provider
    // default. Everything else (including an explicit empty "reroute any") is
    // persisted verbatim, so the user's choice round-trips without surprise.
    let inherited = if cfg.routing.expensive_models.is_empty() {
        crate::default_reroute_filter(slug).to_string()
    } else {
        cfg.routing.expensive_models.clone()
    };
    let keep = filter == inherited;
    let slug = slug.to_string();
    cfg.providers.entry(slug).or_default().expensive_models = (!keep).then_some(filter);
    Ok(())
}

fn get_ollama_keep_alive(cfg: &AppConfig) -> FieldValue {
    FieldValue::String(
        cfg.providers
            .get("ollama")
            .and_then(|p| p.keep_alive.clone())
            .unwrap_or_default(),
    )
}
fn set_ollama_keep_alive(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let s = match value {
        FieldValue::String(s) => s,
        _ => return Err("expects string"),
    };
    cfg.providers
        .entry("ollama".to_string())
        .or_default()
        .keep_alive = if s.trim().is_empty() { None } else { Some(s) };
    Ok(())
}

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

// ─── Modes ────────────────────────────────────────────────────────────────────

fn get_session_mode(cfg: &AppConfig) -> FieldValue {
    FieldValue::Enum(cfg.session_mode.as_str())
}
fn set_session_mode(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let s = match value {
        FieldValue::Enum(s) => s,
        _ => return Err("session_mode expects enum"),
    };
    cfg.session_mode = SessionMode::parse(s).ok_or("invalid session_mode")?;
    Ok(())
}

fn get_session_resume_picker(cfg: &AppConfig) -> FieldValue {
    FieldValue::Enum(cfg.session_resume_picker.as_str())
}
fn set_session_resume_picker(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let s = match value {
        FieldValue::Enum(s) => s,
        _ => return Err("resume_picker expects enum"),
    };
    cfg.session_resume_picker = SessionResumePicker::parse(s).ok_or("invalid resume_picker")?;
    Ok(())
}

fn get_exploration_graph(cfg: &AppConfig) -> FieldValue {
    FieldValue::Bool(cfg.exploration_graph)
}
fn set_exploration_graph(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::Bool(v) => {
            cfg.exploration_graph = v;
            Ok(())
        }
        _ => Err("expects bool"),
    }
}

// ─── Tools ────────────────────────────────────────────────────────────────────

fn get_checkpoints_enabled(cfg: &AppConfig) -> FieldValue {
    FieldValue::Bool(cfg.checkpoints_enabled)
}
fn set_checkpoints_enabled(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::Bool(v) => {
            cfg.checkpoints_enabled = v;
            Ok(())
        }
        _ => Err("expects bool"),
    }
}

// ─── Session Logs ─────────────────────────────────────────────────────────────

fn get_session_log_dir(cfg: &AppConfig) -> FieldValue {
    FieldValue::Path(
        cfg.session_logs
            .log_dir
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from(".squeezy/sessions")),
    )
}
fn set_session_log_dir(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let p = match value {
        FieldValue::Path(p) => p,
        FieldValue::String(s) => std::path::PathBuf::from(s),
        _ => return Err("expects path"),
    };
    cfg.session_logs.log_dir = if p.as_os_str().is_empty() {
        None
    } else {
        Some(p)
    };
    Ok(())
}

fn get_session_log_retention_days(cfg: &AppConfig) -> FieldValue {
    FieldValue::Integer(cfg.session_logs.log_retention_days as i64)
}
fn set_session_log_retention_days(
    cfg: &mut AppConfig,
    value: FieldValue,
) -> Result<(), &'static str> {
    let v = match value {
        FieldValue::Integer(v) => v,
        _ => return Err("expects integer"),
    };
    if v < 1 {
        return Err("must be >= 1");
    }
    cfg.session_logs.log_retention_days = v as u64;
    Ok(())
}

fn get_session_log_retention_archive_days(cfg: &AppConfig) -> FieldValue {
    FieldValue::Integer(cfg.session_logs.log_retention_archive_days as i64)
}
fn set_session_log_retention_archive_days(
    cfg: &mut AppConfig,
    value: FieldValue,
) -> Result<(), &'static str> {
    let v = match value {
        FieldValue::Integer(v) => v,
        _ => return Err("expects integer"),
    };
    if v < 0 {
        return Err("must be >= 0");
    }
    cfg.session_logs.log_retention_archive_days = v as u64;
    Ok(())
}

fn get_session_max_event_bytes(cfg: &AppConfig) -> FieldValue {
    FieldValue::Integer(cfg.session_logs.max_event_bytes as i64)
}
fn set_session_max_event_bytes(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let v = match value {
        FieldValue::Integer(v) => v,
        _ => return Err("expects integer"),
    };
    if v < 4096 {
        return Err("must be >= 4096");
    }
    cfg.session_logs.max_event_bytes = v as usize;
    Ok(())
}

fn get_session_max_session_bytes(cfg: &AppConfig) -> FieldValue {
    FieldValue::Integer(cfg.session_logs.max_session_bytes as i64)
}
fn set_session_max_session_bytes(
    cfg: &mut AppConfig,
    value: FieldValue,
) -> Result<(), &'static str> {
    let v = match value {
        FieldValue::Integer(v) => v,
        _ => return Err("expects integer"),
    };
    if v < 1_048_576 {
        return Err("must be >= 1 MiB");
    }
    cfg.session_logs.max_session_bytes = v as usize;
    Ok(())
}

// ─── Subagents ────────────────────────────────────────────────────────────────

fn get_subagents_enabled(cfg: &AppConfig) -> FieldValue {
    FieldValue::Bool(cfg.subagents.enabled)
}
fn set_subagents_enabled(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::Bool(v) => {
            cfg.subagents.enabled = v;
            Ok(())
        }
        _ => Err("expects bool"),
    }
}

fn get_subagent_explore_enabled(cfg: &AppConfig) -> FieldValue {
    FieldValue::Bool(cfg.subagents.explore_enabled)
}
fn set_subagent_explore_enabled(
    cfg: &mut AppConfig,
    value: FieldValue,
) -> Result<(), &'static str> {
    match value {
        FieldValue::Bool(v) => {
            cfg.subagents.explore_enabled = v;
            Ok(())
        }
        _ => Err("expects bool"),
    }
}

fn get_subagent_help_strict_local(cfg: &AppConfig) -> FieldValue {
    FieldValue::Bool(cfg.subagents.help_strict_local)
}
fn set_subagent_help_strict_local(
    cfg: &mut AppConfig,
    value: FieldValue,
) -> Result<(), &'static str> {
    match value {
        FieldValue::Bool(v) => {
            cfg.subagents.help_strict_local = v;
            Ok(())
        }
        _ => Err("expects bool"),
    }
}

fn get_subagent_explore_model(cfg: &AppConfig) -> FieldValue {
    FieldValue::String(cfg.subagents.explore_model.clone().unwrap_or_default())
}
fn set_subagent_explore_model(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let s = match value {
        FieldValue::String(s) => s,
        _ => return Err("expects string"),
    };
    cfg.subagents.explore_model = if s.trim().is_empty() { None } else { Some(s) };
    Ok(())
}

fn get_subagent_max_concurrent(cfg: &AppConfig) -> FieldValue {
    FieldValue::Integer(cfg.subagents.max_concurrent as i64)
}
fn set_subagent_max_concurrent(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let v = match value {
        FieldValue::Integer(v) => v,
        _ => return Err("expects integer"),
    };
    if v < 1 {
        return Err("max_concurrent must be at least 1");
    }
    if v > 256 {
        return Err("max_concurrent must be at most 256");
    }
    cfg.subagents.max_concurrent = v as usize;
    Ok(())
}

fn get_subagent_max_tool_calls(cfg: &AppConfig) -> FieldValue {
    FieldValue::Integer(cfg.subagents.max_tool_calls_per_call as i64)
}
fn set_subagent_max_tool_calls(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let v = match value {
        FieldValue::Integer(v) => v,
        _ => return Err("expects integer"),
    };
    if v < 1 {
        return Err("must be >= 1");
    }
    cfg.subagents.max_tool_calls_per_call = v as u64;
    Ok(())
}

fn get_subagent_max_bytes(cfg: &AppConfig) -> FieldValue {
    FieldValue::Integer(cfg.subagents.max_tool_bytes_read_per_call as i64)
}
fn set_subagent_max_bytes(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let v = match value {
        FieldValue::Integer(v) => v,
        _ => return Err("expects integer"),
    };
    if v < 65_536 {
        return Err("must be >= 64 KiB");
    }
    cfg.subagents.max_tool_bytes_read_per_call = v as u64;
    Ok(())
}

fn get_subagent_max_files(cfg: &AppConfig) -> FieldValue {
    FieldValue::Integer(cfg.subagents.max_search_files_per_call as i64)
}
fn set_subagent_max_files(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let v = match value {
        FieldValue::Integer(v) => v,
        _ => return Err("expects integer"),
    };
    if v < 100 {
        return Err("must be >= 100");
    }
    cfg.subagents.max_search_files_per_call = v as u64;
    Ok(())
}

fn get_subagent_max_rounds(cfg: &AppConfig) -> FieldValue {
    FieldValue::Integer(cfg.subagents.max_model_rounds as i64)
}
fn set_subagent_max_rounds(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let v = match value {
        FieldValue::Integer(v) => v,
        _ => return Err("expects integer"),
    };
    if v < 1 {
        return Err("must be >= 1");
    }
    cfg.subagents.max_model_rounds = v as usize;
    Ok(())
}

fn get_subagent_max_summary(cfg: &AppConfig) -> FieldValue {
    FieldValue::Integer(cfg.subagents.max_summary_tokens as i64)
}
fn set_subagent_max_summary(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let v = match value {
        FieldValue::Integer(v) => v,
        _ => return Err("expects integer"),
    };
    if v < 100 {
        return Err("must be >= 100");
    }
    cfg.subagents.max_summary_tokens = v as u32;
    Ok(())
}

// ─── Graph ────────────────────────────────────────────────────────────────────

fn get_graph_languages(cfg: &AppConfig) -> FieldValue {
    FieldValue::StringList(cfg.graph.languages.clone())
}
fn set_graph_languages(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::StringList(items) => {
            cfg.graph.languages = items;
            Ok(())
        }
        _ => Err("expects string list"),
    }
}

fn get_graph_max_file_bytes(cfg: &AppConfig) -> FieldValue {
    FieldValue::Integer(cfg.graph.max_file_bytes as i64)
}
fn set_graph_max_file_bytes(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let v = match value {
        FieldValue::Integer(v) => v,
        _ => return Err("expects integer"),
    };
    if v < 1024 {
        return Err("must be >= 1024");
    }
    cfg.graph.max_file_bytes = v as u64;
    Ok(())
}

fn get_graph_include_hidden(cfg: &AppConfig) -> FieldValue {
    FieldValue::Bool(cfg.graph.include_hidden)
}
fn set_graph_include_hidden(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::Bool(v) => {
            cfg.graph.include_hidden = v;
            Ok(())
        }
        _ => Err("expects bool"),
    }
}

fn get_graph_require_signal(cfg: &AppConfig) -> FieldValue {
    FieldValue::Bool(cfg.graph.require_indexing_signal)
}
fn set_graph_require_signal(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::Bool(v) => {
            cfg.graph.require_indexing_signal = v;
            Ok(())
        }
        _ => Err("expects bool"),
    }
}

fn get_graph_include(cfg: &AppConfig) -> FieldValue {
    FieldValue::StringList(cfg.graph.include.clone())
}
fn set_graph_include(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::StringList(items) => {
            cfg.graph.include = items;
            Ok(())
        }
        _ => Err("expects string list"),
    }
}

fn get_graph_exclude(cfg: &AppConfig) -> FieldValue {
    FieldValue::StringList(cfg.graph.exclude.clone())
}
fn set_graph_exclude(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::StringList(items) => {
            cfg.graph.exclude = items;
            Ok(())
        }
        _ => Err("expects string list"),
    }
}

fn get_graph_include_classes(cfg: &AppConfig) -> FieldValue {
    FieldValue::StringList(cfg.graph.include_classes.clone())
}
fn set_graph_include_classes(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::StringList(items) => {
            cfg.graph.include_classes = items;
            Ok(())
        }
        _ => Err("expects string list"),
    }
}

fn get_graph_exclude_classes(cfg: &AppConfig) -> FieldValue {
    FieldValue::StringList(cfg.graph.exclude_classes.clone())
}
fn set_graph_exclude_classes(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::StringList(items) => {
            cfg.graph.exclude_classes = items;
            Ok(())
        }
        _ => Err("expects string list"),
    }
}

// ─── Cache ────────────────────────────────────────────────────────────────────

fn get_cache_root(cfg: &AppConfig) -> FieldValue {
    FieldValue::Path(cfg.cache.root.clone().unwrap_or_default())
}
fn set_cache_root(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let p = match value {
        FieldValue::Path(p) => p,
        FieldValue::String(s) => std::path::PathBuf::from(s),
        _ => return Err("expects path"),
    };
    cfg.cache.root = if p.as_os_str().is_empty() {
        None
    } else {
        Some(p)
    };
    Ok(())
}

fn get_cache_tool_outputs(cfg: &AppConfig) -> FieldValue {
    FieldValue::Path(cfg.cache.tool_outputs.clone().unwrap_or_default())
}
fn set_cache_tool_outputs(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let p = match value {
        FieldValue::Path(p) => p,
        FieldValue::String(s) => std::path::PathBuf::from(s),
        _ => return Err("expects path"),
    };
    cfg.cache.tool_outputs = if p.as_os_str().is_empty() {
        None
    } else {
        Some(p)
    };
    Ok(())
}

fn get_cache_durability(cfg: &AppConfig) -> FieldValue {
    FieldValue::Enum(cfg.cache.durability.as_str())
}
fn set_cache_durability(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let raw = match value {
        FieldValue::Enum(s) => s,
        FieldValue::String(s) => {
            cfg.cache.durability = CacheDurability::parse(&s).ok_or("unknown durability")?;
            return Ok(());
        }
        _ => return Err("expects enum"),
    };
    cfg.cache.durability = CacheDurability::parse(raw).ok_or("unknown durability")?;
    Ok(())
}

// ─── Feedback ─────────────────────────────────────────────────────────────────

fn get_feedback_enabled(cfg: &AppConfig) -> FieldValue {
    FieldValue::Bool(cfg.feedback.enabled)
}
fn set_feedback_enabled(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::Bool(v) => {
            cfg.feedback.enabled = v;
            Ok(())
        }
        _ => Err("expects bool"),
    }
}

fn get_feedback_endpoint(cfg: &AppConfig) -> FieldValue {
    FieldValue::String(cfg.feedback.feedback_endpoint.clone())
}
fn set_feedback_endpoint(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::String(s) if !s.trim().is_empty() => {
            cfg.feedback.feedback_endpoint = s;
            Ok(())
        }
        FieldValue::String(_) => Err("endpoint cannot be empty"),
        _ => Err("expects string"),
    }
}

fn get_report_endpoint(cfg: &AppConfig) -> FieldValue {
    FieldValue::String(cfg.feedback.report_endpoint.clone())
}
fn set_report_endpoint(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::String(s) if !s.trim().is_empty() => {
            cfg.feedback.report_endpoint = s;
            Ok(())
        }
        FieldValue::String(_) => Err("endpoint cannot be empty"),
        _ => Err("expects string"),
    }
}

fn get_feedback_max_bytes(cfg: &AppConfig) -> FieldValue {
    FieldValue::Integer(cfg.feedback.max_feedback_bytes as i64)
}
fn set_feedback_max_bytes(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let v = match value {
        FieldValue::Integer(v) => v,
        _ => return Err("expects integer"),
    };
    if v < 1024 {
        return Err("must be >= 1024");
    }
    cfg.feedback.max_feedback_bytes = v as usize;
    Ok(())
}

fn get_report_max_bytes(cfg: &AppConfig) -> FieldValue {
    FieldValue::Integer(cfg.feedback.max_report_bytes as i64)
}
fn set_report_max_bytes(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    let v = match value {
        FieldValue::Integer(v) => v,
        _ => return Err("expects integer"),
    };
    if v < 1024 {
        return Err("must be >= 1024");
    }
    cfg.feedback.max_report_bytes = v as usize;
    Ok(())
}

// ─── Redaction ────────────────────────────────────────────────────────────────

fn get_redaction_custom_patterns(cfg: &AppConfig) -> FieldValue {
    FieldValue::StringList(cfg.redaction.custom_patterns.clone())
}
fn set_redaction_custom_patterns(
    cfg: &mut AppConfig,
    value: FieldValue,
) -> Result<(), &'static str> {
    let items = match value {
        FieldValue::StringList(items) => items,
        _ => return Err("expects string list"),
    };
    // Validate each pattern compiles; let RedactionConfig::validate gate.
    let next = crate::RedactionConfig {
        custom_patterns: items,
    };
    next.validate().map_err(|_| "invalid regex pattern")?;
    cfg.redaction = next;
    Ok(())
}

// ─── Web ──────────────────────────────────────────────────────────────────────

fn get_exa_mcp_url(cfg: &AppConfig) -> FieldValue {
    FieldValue::String(cfg.exa_mcp_url.clone())
}
fn set_exa_mcp_url(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::String(s) if !s.trim().is_empty() => {
            cfg.exa_mcp_url = s;
            Ok(())
        }
        FieldValue::String(_) => Err("exa_mcp_url cannot be empty"),
        _ => Err("expects string"),
    }
}

fn get_exa_api_key_env(cfg: &AppConfig) -> FieldValue {
    FieldValue::String(cfg.exa_api_key_env.clone())
}
fn set_exa_api_key_env(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::String(s) if !s.trim().is_empty() => {
            cfg.exa_api_key_env = s;
            Ok(())
        }
        FieldValue::String(_) => Err("env var name cannot be empty"),
        _ => Err("expects string"),
    }
}

fn get_parallel_mcp_url(cfg: &AppConfig) -> FieldValue {
    FieldValue::String(cfg.parallel_mcp_url.clone())
}
fn set_parallel_mcp_url(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::String(s) if !s.trim().is_empty() => {
            cfg.parallel_mcp_url = s;
            Ok(())
        }
        FieldValue::String(_) => Err("parallel_mcp_url cannot be empty"),
        _ => Err("expects string"),
    }
}

fn get_parallel_api_key_env(cfg: &AppConfig) -> FieldValue {
    FieldValue::String(cfg.parallel_api_key_env.clone())
}
fn set_parallel_api_key_env(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::String(s) if !s.trim().is_empty() => {
            cfg.parallel_api_key_env = s;
            Ok(())
        }
        FieldValue::String(_) => Err("env var name cannot be empty"),
        _ => Err("expects string"),
    }
}

fn get_websearch_provider(cfg: &AppConfig) -> FieldValue {
    FieldValue::String(cfg.websearch_provider.clone())
}
fn set_websearch_provider(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::String(s) => {
            let trimmed = s.trim();
            if trimmed.eq_ignore_ascii_case("exa") {
                cfg.websearch_provider = "exa".to_string();
                Ok(())
            } else if trimmed.eq_ignore_ascii_case("parallel") {
                cfg.websearch_provider = "parallel".to_string();
                Ok(())
            } else {
                Err("websearch_provider must be \"exa\" or \"parallel\"")
            }
        }
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
    let slug = slug.trim();
    CONFIG_SECTIONS
        .iter()
        .find(|s| s.id.slug().eq_ignore_ascii_case(slug))
        .map(|s| s.id)
}

#[cfg(test)]
#[path = "config_schema_tests.rs"]
mod tests;
