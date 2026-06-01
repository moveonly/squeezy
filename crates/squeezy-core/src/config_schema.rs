//! Declarative metadata for every editable field in `AppConfig`.
//!
//! `CONFIG_SECTIONS` is the single source of truth shared by the TUI config
//! screen and the TOML writer: both walk the same list, so the screen cannot
//! show a field the writer doesn't know how to persist (and vice versa).
//!
//! New sections are added by appending a `ConfigSectionMeta` entry below.

use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use crate::{
    AppConfig, DEFAULT_ANTHROPIC_MODEL, DEFAULT_AZURE_OPENAI_MODEL, DEFAULT_BEDROCK_MODEL,
    DEFAULT_COST_WARN_PERCENT, DEFAULT_EXA_API_KEY_ENV, DEFAULT_EXA_MCP_URL,
    DEFAULT_FEEDBACK_ENDPOINT, DEFAULT_FEEDBACK_MAX_BYTES, DEFAULT_GOOGLE_MODEL,
    DEFAULT_MAX_PARALLEL_TOOLS, DEFAULT_MAX_SEARCH_FILES_PER_TURN,
    DEFAULT_MAX_TOOL_BYTES_READ_PER_TURN, DEFAULT_MAX_TOOL_CALLS_PER_TURN, DEFAULT_OLLAMA_MODEL,
    DEFAULT_OPENAI_CODEX_MODEL, DEFAULT_OPENAI_MODEL, DEFAULT_PARALLEL_API_KEY_ENV,
    DEFAULT_PARALLEL_MCP_URL, DEFAULT_REPORT_ENDPOINT, DEFAULT_REPORT_MAX_BYTES,
    DEFAULT_SESSION_LOG_RETENTION_ARCHIVE_DAYS, DEFAULT_SESSION_LOG_RETENTION_DAYS,
    DEFAULT_SESSION_MAX_EVENT_BYTES, DEFAULT_SESSION_MAX_SESSION_BYTES,
    DEFAULT_STREAM_IDLE_TIMEOUT_MS, DEFAULT_SUBAGENT_MAX_MODEL_ROUNDS,
    DEFAULT_SUBAGENT_MAX_SEARCH_FILES_PER_CALL, DEFAULT_SUBAGENT_MAX_SUMMARY_TOKENS,
    DEFAULT_SUBAGENT_MAX_TOOL_BYTES_READ_PER_CALL, DEFAULT_SUBAGENT_MAX_TOOL_CALLS_PER_CALL,
    DEFAULT_TELEMETRY_ENDPOINT, DEFAULT_TICK_RATE_MS, DEFAULT_TUI_THEME_NAME,
    DEFAULT_WEBSEARCH_PROVIDER, OpenAiCompatiblePreset, PermissionMode, PermissionPolicyMode,
    ProviderConfig, ReasoningEffort, ResponseVerbosity, SessionMode, SessionResumePicker,
    StatusVerbosity, ToolOutputVerbosity, TranscriptDefault, TuiAlternateScreen,
    TuiSynchronizedOutput, normalize_tui_theme_name,
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
    /// Singleton kind used only by the `Providers` section to indicate the
    /// six per-provider sub-tabs along the right pane.
    ProviderSubTabs,
    /// Multi-row editor: each row is itself a `FieldMeta` schema. `Keyed`
    /// rows are addressed by name (`[mcp.servers.<name>]`); `Ordered` rows
    /// are positional (`[[permissions.rules]]`).
    TableArray {
        kind: TableArrayKind,
    },
}

#[derive(Clone, Copy)]
pub enum TableArrayKind {
    Keyed { item_fields: &'static [FieldMeta] },
    Ordered { item_fields: &'static [FieldMeta] },
}

impl std::fmt::Debug for TableArrayKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Keyed { item_fields } => write!(f, "Keyed {{ {} fields }}", item_fields.len()),
            Self::Ordered { item_fields } => {
                write!(f, "Ordered {{ {} fields }}", item_fields.len())
            }
        }
    }
}

impl std::fmt::Debug for FieldKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bool => write!(f, "Bool"),
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
            Self::ProviderSubTabs => write!(f, "ProviderSubTabs"),
            Self::TableArray { kind } => f.debug_struct("TableArray").field("kind", kind).finish(),
        }
    }
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
    StringList(Vec<String>),
    Path(PathBuf),
    /// Placeholder — secrets never carry the plaintext through `FieldValue`.
    /// The screen mask-renders this as `••••` and routes editing through
    /// the dedicated secret-entry flow which writes inline `api_key` to
    /// the active scope's TOML.
    Secret,
    /// Selected sub-tab index (read-only convenience for `ProviderSubTabs`).
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
    Providers,
    Session,
    Modes,
    Context,
    Subagents,
    Skills,
    Graph,
    Cache,
    Tools,
    Feedback,
    Redaction,
    Web,
    McpServers,
    ShellSandbox,
    PermissionRules,
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
            Self::Providers => "providers",
            Self::Session => "session",
            Self::Modes => "modes",
            Self::Context => "context",
            Self::Subagents => "subagents",
            Self::Skills => "skills",
            Self::Graph => "graph",
            Self::Cache => "cache",
            Self::Tools => "tools",
            Self::Feedback => "feedback",
            Self::Redaction => "redaction",
            Self::Web => "web",
            Self::McpServers => "mcp-servers",
            Self::ShellSandbox => "shell-sandbox",
            Self::PermissionRules => "permission-rules",
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
    "lmstudio",
    "vllm",
    "llamacpp",
    "openai_compatible",
];

pub const PROFILE_OPTIONS: &[&str] = &["cheap", "balanced", "strong"];
pub const REASONING_EFFORT_OPTIONS: &[&str] = &["low", "medium", "high", "xhigh"];
pub const SESSION_MODE_OPTIONS: &[&str] = &["build", "plan"];
pub const SESSION_RESUME_PICKER_OPTIONS: &[&str] = &["ask", "never"];
pub const STATUS_VERBOSITY_OPTIONS: &[&str] = &["compact", "verbose"];
pub const RESPONSE_VERBOSITY_OPTIONS: &[&str] = &["concise", "normal", "verbose"];
pub const TOOL_OUTPUT_VERBOSITY_OPTIONS: &[&str] = &["compact", "normal", "verbose"];
pub const TRANSCRIPT_DEFAULT_OPTIONS: &[&str] = &["compact", "expanded"];
pub const ALTERNATE_SCREEN_OPTIONS: &[&str] = &["auto", "never", "always"];
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
                help: "Permission preset. Default allows workspace read/edit/local commands, Auto-review routes eligible approvals through the reviewer, Full Access removes workspace/network prompts, Custom exposes each capability.",
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
                label: "alternate_screen",
                toml_path: &["tui", "alternate_screen"],
                kind: FieldKind::Enum {
                    options: ALTERNATE_SCREEN_OPTIONS,
                },
                tier: ApplyTier::Restart,
                get: get_alternate_screen,
                set: set_alternate_screen,
                default_display: "auto",
                default: || FieldValue::Enum("auto"),
                help: "Whether to take over the terminal screen on launch.",
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
                help: "Build mode runs tools freely; Plan mode is read-only and emits a structured plan.",
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
                label: "exploration_compiler",
                toml_path: &["agent", "exploration_compiler"],
                kind: FieldKind::Bool,
                tier: ApplyTier::NextPrompt,
                get: get_exploration_compiler,
                set: set_exploration_compiler,
                default_display: "true",
                default: || FieldValue::Bool(true),
                help: "Use the graph-first exploration compiler before LLM tool dispatch.",
                env_override: Some("SQUEEZY_EXPLORATION_COMPILER"),
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
                help: "Allow delegate_plan / delegate_review subagent dispatch.",
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
                label: "max_tool_calls_per_call",
                toml_path: &["subagents", "max_tool_calls_per_call"],
                kind: FieldKind::Integer {
                    min: 1,
                    max: 256,
                    suffix: None,
                },
                tier: ApplyTier::NextPrompt,
                get: get_subagent_max_tool_calls,
                set: set_subagent_max_tool_calls,
                default_display: "24",
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
                default_display: "8388608 bytes",
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
                default_display: "2000 files",
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
        DEFAULT_AZURE_OPENAI_API_VERSION, DEFAULT_AZURE_OPENAI_BASE_URL, DEFAULT_BEDROCK_REGION,
        DEFAULT_GOOGLE_BASE_URL, DEFAULT_OLLAMA_BASE_URL, DEFAULT_OPENAI_BASE_URL,
        DEFAULT_OPENAI_CODEX_BASE_URL, DEFAULT_OPENAI_CODEX_ORIGINATOR, FauxConfig, GoogleConfig,
        OllamaConfig, OpenAiCodexConfig, OpenAiCompatibleConfig, OpenAiCompatiblePreset,
        OpenAiConfig, ProviderTransportConfig,
    };
    let transport = ProviderTransportConfig::default();
    cfg.provider = match s {
        "openai" => ProviderConfig::OpenAi(OpenAiConfig {
            api_key_env: "SQUEEZY_OPENAI_KEY".to_string(),
            api_key: None,
            base_url: DEFAULT_OPENAI_BASE_URL.to_string(),
            organization: None,
            project: None,
            service_tier: None,
            transport,
        }),
        "openai-codex" | "openai_codex" | "chatgpt" => {
            ProviderConfig::OpenAiCodex(OpenAiCodexConfig {
                base_url: DEFAULT_OPENAI_CODEX_BASE_URL.to_string(),
                originator: DEFAULT_OPENAI_CODEX_ORIGINATOR.to_string(),
                transport,
            })
        }
        "anthropic" => ProviderConfig::Anthropic(AnthropicConfig {
            api_key_env: "SQUEEZY_ANTHROPIC_KEY".to_string(),
            api_key: None,
            base_url: DEFAULT_ANTHROPIC_BASE_URL.to_string(),
            transport,
        }),
        "google" => ProviderConfig::Google(GoogleConfig {
            api_key_env: "SQUEEZY_GOOGLE_KEY".to_string(),
            api_key: None,
            base_url: DEFAULT_GOOGLE_BASE_URL.to_string(),
            transport,
        }),
        "azure_openai" => ProviderConfig::AzureOpenAi(AzureOpenAiConfig {
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
        "bedrock" => ProviderConfig::Bedrock(BedrockConfig {
            region: DEFAULT_BEDROCK_REGION.to_string(),
            base_url: None,
            bearer_token: None,
            request_metadata: BTreeMap::new(),
            transport,
        }),
        "ollama" => ProviderConfig::Ollama(OllamaConfig {
            base_url: DEFAULT_OLLAMA_BASE_URL.to_string(),
            route_style: Default::default(),
            transport,
        }),
        "faux" | "mock" => ProviderConfig::Faux(FauxConfig {
            script: None,
            name: None,
            transport,
        }),
        other => {
            let preset = OpenAiCompatiblePreset::parse(other).ok_or("unknown provider")?;
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
            })
        }
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
        ProviderConfig::OpenAiCodex(_) => "openai_codex",
        ProviderConfig::OpenAiCompatible(config) => config.preset.as_str(),
        ProviderConfig::Faux(_) => "faux",
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
        "faux" | "mock" => crate::DEFAULT_FAUX_MODEL,
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

fn get_exploration_compiler(cfg: &AppConfig) -> FieldValue {
    FieldValue::Bool(cfg.exploration_compiler)
}
fn set_exploration_compiler(cfg: &mut AppConfig, value: FieldValue) -> Result<(), &'static str> {
    match value {
        FieldValue::Bool(v) => {
            cfg.exploration_compiler = v;
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
        custom_patterns: items.clone(),
    };
    next.validate().map_err(|_| "invalid regex pattern")?;
    cfg.redaction.custom_patterns = items;
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
            let trimmed = s.trim().to_ascii_lowercase();
            if trimmed == "exa" || trimmed == "parallel" {
                cfg.websearch_provider = trimmed;
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
    let lower = slug.trim().to_ascii_lowercase();
    CONFIG_SECTIONS
        .iter()
        .find(|s| s.id.slug() == lower)
        .map(|s| s.id)
}

#[cfg(test)]
#[path = "config_schema_tests.rs"]
mod tests;
