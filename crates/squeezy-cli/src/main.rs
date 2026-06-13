use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    fmt::Write as _,
    fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Serialize;
use sha2::{Digest, Sha256};
use squeezy_agent::{
    Agent, AgentEvent, RequestUserInputResponse, SessionReplayReport, ToolApprovalDecision,
};
use squeezy_core::settings_writer::write_settings_atomic;
use squeezy_core::{
    AppConfig, CostSnapshot, DEFAULT_OLLAMA_BASE_URL, MODEL_SELECTION_VERSION, McpServerConfig,
    McpTransport, ModelProfile, OpenAiCompatiblePreset, PROJECT_SETTINGS_FILE, PermissionMode,
    ReasoningEffort, SessionMode, SettingsFile, SqueezyError, default_settings_path,
    find_project_settings_path, local_settings_template, per_repo_settings_path,
    project_settings_template, user_settings_template,
};
use squeezy_llm::{
    LlmProvider, ModelInfo, PROVIDERS, UnavailableProvider, capabilities_for,
    fetch_ollama_model_names, models_for_provider, provider_from_config,
};

mod auth;
mod config_browse;
mod doctor;
mod print_mode;
mod providers;
mod update;
use auth::handle_auth_command;
use config_browse::handle_browse_command;
use doctor::DoctorArgs;
use providers::{ProvidersCommand, handle_providers_command};
use squeezy_core::GraphConfig;
use squeezy_parse::smoke_all_languages;
use squeezy_store::{
    BugReportOptions, CleanupMode, RepoProfileLoad, STALE_RUNNING_SESSION_THRESHOLD_MS,
    SemanticSupport, SessionEvent, SessionMetadata, SessionQuery, SessionReplayTape, SessionStatus,
    SessionStore, default_bug_report_path, ensure_repo_profile, parse_bug_report_section,
    refresh_repo_profile,
};
use squeezy_telemetry::{
    FeedbackClient, ReportUpload, TelemetryClient, TelemetryEvent, prepare_feedback,
};
use squeezy_tools::{
    McpClientRegistry, McpElicitationResponse, McpServerStatus, McpStaleOutcome, ToolCall,
    ToolResult, ToolStatus,
};
use squeezy_workspace::{CrawlOptions, IndexingPolicy, WorkspaceCrawler};
use tokio_util::sync::CancellationToken;
use toml_edit::{DocumentMut, Item, Table, Value as TomlValue};

/// Output framing for `--prompt`. `Default` matches the historical
/// human-readable text-delta stream; `Json` emits one
/// `serde_json`-serialized `LlmEvent` per line so callers can pipe to `jq`
/// or capture the per-event cost surface programmatically. The line schema
/// follows the `LlmEvent` enum tag/data shape declared in
/// `crates/squeezy-llm/src/lib.rs` (`type` + `data`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lowercase")]
enum PromptFormat {
    Default,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "kebab-case")]
enum PromptPermissionMode {
    /// Allow each permission request once. This preserves the historical
    /// non-interactive behavior and keeps CI prompts from hanging.
    #[value(name = "auto-approve-ask")]
    AutoApprove,
    /// Deny each permission request and let the agent continue with the
    /// denied tool result.
    #[value(name = "deny-ask")]
    Deny,
    /// Deny the request and make the CLI command fail immediately.
    #[value(name = "fail-on-ask")]
    Fail,
}

#[derive(Debug, Parser)]
#[command(
    name = "squeezy",
    version,
    about = "Cost-aware coding agent TUI",
    disable_help_subcommand = true
)]
struct Cli {
    /// Provider id. `SQUEEZY_PROVIDER` is also honored, but goes through the
    /// env source layer so it is tagged correctly by `config inspect`.
    #[arg(long, help = "Provider id (openai, anthropic, google, ...)")]
    provider: Option<String>,
    #[arg(long, help = "Model id; overrides settings and SQUEEZY_MODEL")]
    model: Option<String>,
    #[arg(
        long,
        help = "Named TOML profile; merges `[profiles.<name>]` on top of settings"
    )]
    profile: Option<String>,
    #[arg(
        long = "model-profile",
        help = "Model tier: cheap, balanced, or strong"
    )]
    model_profile: Option<String>,
    #[arg(long, help = "Max output tokens; overrides SQUEEZY_MAX_OUTPUT_TOKENS")]
    max_output_tokens: Option<u32>,
    #[arg(long, help = "Start session mode: plan or build")]
    mode: Option<String>,
    #[arg(
        long = "session-dir",
        value_name = "PATH",
        help = "Directory for session traces; overrides [session].log_dir and SQUEEZY_SESSION_DIR"
    )]
    session_dir: Option<PathBuf>,
    #[arg(long, help = "List configured built-in providers")]
    list_providers: bool,
    #[arg(long, help = "List built-in model metadata")]
    list_models: bool,
    #[arg(
        long,
        help = "Run a non-interactive prompt and print streamed text. Repeat --prompt to queue prompts sequentially; use --prompt @path to expand a utf-8 file's contents, and --prompt - to consume piped stdin. Piped stdin is also read automatically and prepended to the first prompt when --prompt - is absent."
    )]
    prompt: Vec<String>,
    #[arg(
        long,
        value_name = "FORMAT",
        help = "Non-interactive output format for --prompt: 'default' (text deltas) or 'json' (one JSON LlmEvent per line; type + data, see squeezy-llm). Experimental; schema may change.",
        default_value = "default"
    )]
    format: PromptFormat,
    #[arg(
        long = "prompt-permission-mode",
        value_name = "MODE",
        value_enum,
        default_value_t = PromptPermissionMode::AutoApprove,
        help = "Permission behavior for non-interactive --prompt runs: auto-approve-ask (default; approve each request once), deny-ask (deny but continue), fail-on-ask (deny and exit non-zero)"
    )]
    prompt_permission_mode: PromptPermissionMode,
    #[arg(
        long = "health",
        hide = true,
        // `--health` short-circuits the dispatch table and runs doctor
        // with `DoctorArgs::default()`. Pairing it with one of the
        // top-level print-mode / list flags would silently drop the
        // other flag's behavior; reject those combinations at parse
        // time so the deviation is loud instead of mysterious. The
        // subcommand-with-`--health` combination is rejected at
        // runtime in `run()` because the subcommand group does not
        // expose a conflict-friendly arg id in clap derive. Use
        // `squeezy doctor --json` (and other doctor flags) for non-
        // default behavior; `--health` is a compatibility alias only.
        conflicts_with_all = ["prompt", "list_providers", "list_models"],
        help = "Hidden compatibility alias for `squeezy doctor` (no extra flags accepted; use `squeezy doctor` for --json/--probe/--only)"
    )]
    health: bool,
    #[arg(
        long,
        help = "Ignore saved provider/model defaults and run startup selection again"
    )]
    no_default: bool,
    #[arg(
        long = "resume",
        help = "Open the resume picker to choose a recent session (this project, or Tab for any project) instead of starting fresh"
    )]
    resume: bool,
    #[arg(
        long = "no-resume-picker",
        help = "Force a fresh session even when --resume is passed"
    )]
    no_resume_picker: bool,
    #[arg(
        long = "continue",
        conflicts_with = "session",
        help = "Resume the most recent resumable session for the current directory; falls back to a fresh session if none exists"
    )]
    continue_session: bool,
    #[arg(
        long = "session",
        value_name = "ID",
        help = "Resume an explicit session id"
    )]
    session: Option<String>,
    #[arg(
        long = "force-cross-project",
        help = "Bypass the cross-project confirmation when resuming a session whose recorded cwd differs from the current one"
    )]
    force_cross_project: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = "Inspect or initialize Squeezy configuration")]
    Config {
        #[command(subcommand)]
        command: Option<ConfigCommand>,
    },
    #[command(about = "Inspect or refresh the local generated repo profile")]
    Repo {
        #[command(subcommand)]
        command: RepoCommand,
    },
    #[command(about = "List, inspect, resume, export, or clean up local sessions")]
    Sessions {
        #[command(subcommand)]
        command: SessionsCommand,
    },
    #[command(about = "Submit short redacted feedback to Squeezy maintainers")]
    Feedback(FeedbackArgs),
    #[command(about = "Manage configured MCP servers")]
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
    #[command(about = "Inspect, enable/disable, or validate discovered Squeezy skills")]
    Skills {
        #[command(subcommand)]
        command: SkillsCommand,
    },
    #[command(about = "Ask the running Squeezy shell session for an in-flight permission decision")]
    Ask(AskArgs),
    #[command(about = "Manage provider API keys stored inline in the settings TOML")]
    Auth {
        #[command(subcommand)]
        command: auth::AuthCommand,
    },
    #[command(about = "Diagnose configuration, providers, session store, and sandbox availability")]
    Doctor(DoctorArgs),
    #[command(
        about = "Refresh the cached live model catalog from one or more OpenAI-compatible providers"
    )]
    RefreshModels(RefreshModelsArgs),
    #[command(about = "List and inspect the built-in provider registry")]
    Providers {
        #[command(subcommand)]
        command: ProvidersCommand,
    },
    #[command(
        about = "Show a local Squeezy help topic from bundled docs (same corpus as /help in the TUI)"
    )]
    Help {
        /// Topic name (e.g. skills, providers, config). Omit to list all topics.
        topic: Option<String>,
    },
    #[command(about = "Parser diagnostics and smoke testing")]
    Parse {
        #[command(subcommand)]
        command: ParseCommand,
    },
}

#[derive(Debug, Args)]
struct RefreshModelsArgs {
    /// Preset to refresh. Repeat for multiple; defaults to every preset whose
    /// API-key env var is currently set.
    #[arg(
        long = "provider",
        help = "Preset name (e.g. openrouter, groq, vertex)"
    )]
    providers: Vec<String>,
    #[arg(long, help = "Print the refreshed catalog as JSON")]
    json: bool,
}

#[derive(Debug, Args)]
struct AskArgs {
    #[arg(long, help = "Shell command or capability that needs approval")]
    command: String,
    #[arg(long, help = "Why the running shell step needs this approval")]
    justification: String,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    #[command(
        about = "List discoverable resources (skills, providers, sessions, prompt templates)"
    )]
    Browse(ConfigBrowseArgs),
    #[command(about = "Print the effective merged configuration with secrets redacted")]
    Inspect,
    #[command(about = "Create a default user or project settings file")]
    Init {
        #[command(flatten)]
        scope: InitScope,
        #[arg(long, help = "Overwrite an existing file")]
        force: bool,
        #[arg(
            long = "with-bundled-skills",
            help = "After writing settings, install the in-binary bundled sample skills under the user skills directory (--user only)"
        )]
        with_bundled_skills: bool,
    },
    #[command(
        about = "Validate the active settings files for unknown fields",
        long_about = "Validate the active settings files for unknown fields.\n\
                      By default (without --strict), unknown fields are warnings.\n\
                      With --strict, any unknown field is treated as an error."
    )]
    Validate {
        #[arg(
            long,
            help = "Treat unknown config fields as errors instead of warnings"
        )]
        strict: bool,
    },
    #[command(
        about = "Show the winning tier and shadowed values for a config field",
        long_about = "Show which tier owns a config field and whether lower tiers are \
                      shadowed.\n\
                      Example: squeezy config explain model.provider"
    )]
    Explain {
        /// Dotted TOML path, e.g. `model.provider` or `tui.tick_rate_ms`.
        field: String,
    },
    #[command(
        about = "Emit the config schema as JSON for external tooling",
        long_about = "Print the CONFIG_SECTIONS schema as a JSON array. Each entry \
                      describes a section with its fields, TOML paths, editor kinds, \
                      apply tiers, env overrides, and default display values."
    )]
    Schema,
}

#[derive(Debug, Args, Default)]
pub(crate) struct ConfigBrowseArgs {
    #[arg(long, help = "Emit machine-readable JSON instead of the human listing")]
    pub(crate) json: bool,
}

#[derive(Debug, Args)]
#[group(required = true, multiple = false)]
struct InitScope {
    #[arg(long, help = "Write the user-level settings file")]
    user: bool,
    #[arg(long, help = "Write the project-level settings file")]
    project: bool,
    #[arg(
        long,
        help = "Write the per-machine repo-local settings file (~/.squeezy/projects/<hash>/settings.toml)"
    )]
    local: bool,
}

#[derive(Debug, Subcommand)]
enum McpCommand {
    #[command(about = "List configured MCP servers")]
    List {
        #[arg(long)]
        json: bool,
        /// Probe each enabled server with a live handshake and report
        /// ready/stale/failed/cancelled status together with the advertised
        /// tool count (same checks as `doctor --probe`).
        #[arg(long)]
        probe: bool,
    },
    #[command(
        about = "Test one configured MCP server with the session initialize/tool-discovery handshake"
    )]
    Test {
        name: String,
        #[arg(long, help = "Emit machine-readable JSON instead of a status row")]
        json: bool,
    },
    #[command(about = "Add an MCP server to user or project settings")]
    Add(Box<McpAddArgs>),
    #[command(about = "Enable a configured MCP server")]
    Enable(McpNameScope),
    #[command(about = "Disable a configured MCP server")]
    Disable(McpNameScope),
    #[command(about = "Remove a configured MCP server")]
    Remove(McpNameScope),
}

#[derive(Debug, Args)]
struct McpAddArgs {
    name: String,
    #[command(flatten)]
    scope: McpConfigScope,
    #[arg(long, help = "Transport: stdio, http, or sse")]
    transport: String,
    #[arg(
        long,
        help = "Command for stdio MCP servers. \
                On Windows use the full launcher name, e.g. \
                `npx.cmd`, `cmd.exe`, or `powershell.exe`"
    )]
    command: Option<String>,
    #[arg(long = "arg", help = "Command argument; repeat for multiple args")]
    args: Vec<String>,
    #[arg(long, help = "URL for http or sse MCP servers")]
    url: Option<String>,
    #[arg(long, help = "Timeout in milliseconds")]
    timeout_ms: Option<u64>,
    #[arg(
        long,
        help = "Discovery-phase timeout in milliseconds (overrides --timeout-ms)"
    )]
    discovery_timeout_ms: Option<u64>,
    #[arg(
        long,
        help = "Tool-call timeout in milliseconds (overrides --timeout-ms)"
    )]
    tool_call_timeout_ms: Option<u64>,
    #[arg(long = "env", help = "Environment entry in KEY=VALUE form")]
    env: Vec<String>,
    #[arg(
        long,
        help = "Working directory for stdio MCP server processes. \
                Useful on Windows when relative config paths, Node scripts, \
                or Python virtualenvs must resolve from a specific project root"
    )]
    cwd: Option<String>,
    #[arg(
        long,
        help = "Name of the environment variable holding the bearer token \
                for http/sse transports (e.g. MY_API_KEY)"
    )]
    bearer_token_env_var: Option<String>,
    #[arg(
        long = "http-header",
        help = "Static HTTP header in HEADER=VALUE form; repeat for multiple headers"
    )]
    http_headers: Vec<String>,
    #[arg(
        long = "env-http-header",
        help = "HTTP header whose value is read from an env var in HEADER=ENV_VAR form; \
                repeat for multiple headers"
    )]
    env_http_headers: Vec<String>,
    #[arg(long, help = "Per-server default permission: allow, ask, or deny")]
    permission_default: Option<String>,
    #[arg(
        long = "enabled-tool",
        help = "Allow-list a specific tool name; repeat for multiple (all tools enabled when unset)"
    )]
    enabled_tools: Vec<String>,
    #[arg(
        long = "disabled-tool",
        help = "Block a specific tool name; repeat for multiple"
    )]
    disabled_tools: Vec<String>,
}

#[derive(Debug, Args)]
struct McpNameScope {
    name: String,
    #[command(flatten)]
    scope: McpConfigScope,
}

#[derive(Debug, Args)]
#[group(required = true, multiple = false)]
struct McpConfigScope {
    #[arg(long, help = "Edit the user-level settings file")]
    user: bool,
    #[arg(long, help = "Edit the project-level settings file")]
    project: bool,
}

#[derive(Debug, Subcommand)]
enum SkillsCommand {
    #[command(about = "List discovered skills, including disabled ones")]
    List {
        #[arg(long)]
        json: bool,
    },
    #[command(
        about = "Enable a discovered skill via [[skills.config]] (selector is name XOR path)"
    )]
    Enable(SkillsSelectorScope),
    #[command(about = "Disable a discovered skill via [[skills.config]]")]
    Disable(SkillsSelectorScope),
    #[command(about = "Validate frontmatter/manifest of every discovered skill")]
    Validate {
        #[arg(long)]
        json: bool,
    },
    #[command(
        about = "Install the in-binary bundled sample skills under the user skills directory"
    )]
    Install {
        #[arg(long, help = "Overwrite an existing target directory")]
        force: bool,
    },
    #[command(
        about = "Print the resolved skill discovery roots (compat-user, user, XDG, extra, project)"
    )]
    Paths {
        #[arg(long, help = "Emit machine-readable JSON")]
        json: bool,
    },
    #[command(
        about = "Show full metadata, triggers, config state, and optional body preview for a skill"
    )]
    Show {
        /// Skill name to inspect.
        name: String,
        #[arg(long, help = "Include the first 400 characters of the skill body")]
        preview: bool,
    },
}

#[derive(Debug, Args)]
struct SkillsSelectorScope {
    #[command(flatten)]
    selector: SkillsSelector,
    #[command(flatten)]
    scope: SkillsConfigScope,
}

#[derive(Debug, Args)]
#[group(required = true, multiple = false)]
struct SkillsSelector {
    #[arg(long, help = "Match by skill name (mutually exclusive with --path)")]
    name: Option<String>,
    #[arg(
        long,
        help = "Match by skill directory path (mutually exclusive with --name)"
    )]
    path: Option<PathBuf>,
}

#[derive(Debug, Args)]
#[group(required = true, multiple = false)]
struct SkillsConfigScope {
    #[arg(long, help = "Edit the user-level settings file")]
    user: bool,
    #[arg(long, help = "Edit the project-level settings file")]
    project: bool,
}

#[derive(Debug, Subcommand)]
enum RepoCommand {
    #[command(about = "Print the stored or freshly computed repo profile")]
    Inspect {
        #[arg(long, help = "Emit machine-readable JSON instead of human text")]
        json: bool,
    },
    #[command(about = "Recompute and persist the generated local repo profile")]
    Refresh,
    #[command(about = "Print suggested project config settings for manual adoption")]
    Recommendations,
    #[command(
        about = "Report file counts, extension inventory, and C/C++ header classification \
                 confidence for the workspace"
    )]
    Languages {
        #[arg(long, help = "Emit machine-readable JSON")]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ParseCommand {
    #[command(
        about = "Initialize every registered parser grammar and parse a built-in fixture per \
                 language; exits non-zero if any grammar fails to load"
    )]
    Smoke {
        #[arg(long, help = "Emit machine-readable JSON")]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum SessionsCommand {
    #[command(about = "List local sessions")]
    List(SessionListArgs),
    #[command(about = "Show a local session summary")]
    Show {
        id: String,
        #[arg(long, help = "Emit machine-readable JSON instead of key=value lines")]
        json: bool,
    },
    #[command(about = "Resume a local session in the TUI")]
    Resume {
        id: String,
        /// Bypass the cross-project confirmation prompt when the recorded
        /// `metadata.cwd` differs from the current working directory.
        /// Equivalent to typing `y` at the prompt; useful for scripted
        /// callers that intentionally resume sessions across checkouts.
        #[arg(long = "force-cross-project")]
        force_cross_project: bool,
    },
    #[command(about = "Fork a local session into a new child and resume it in the TUI")]
    Fork { id: String },
    #[command(about = "Replay a recorded local session deterministically")]
    Replay {
        id: String,
        #[arg(long, help = "Print replay report as JSON")]
        json: bool,
    },
    #[command(
        about = "Export a redacted local session bundle (JSON by default, --html for shareable HTML)"
    )]
    Export(SessionExportArgs),
    #[command(about = "Preview, save, or send a redacted bug-report archive")]
    Report(SessionReportArgs),
    #[command(
        about = "Soft-archive expired sessions or explicit ids (default), or --purge to hard-delete"
    )]
    Cleanup {
        #[arg(long = "id")]
        ids: Vec<String>,
        /// Explicitly soft-archive — the default — kept as a flag so
        /// scripts can be self-documenting alongside `--purge`.
        #[arg(long, conflicts_with = "purge")]
        archive: bool,
        /// Hard-delete instead of soft-archiving. Live sessions skip the
        /// `archived/` tree entirely; archived sessions named in `--id`
        /// are removed without waiting for archive retention.
        #[arg(long)]
        purge: bool,
    },
    #[command(about = "Soft-archive a session so it survives retention sweeps")]
    Archive { id: String },
    #[command(about = "Restore a previously archived session into the live root")]
    Unarchive { id: String },
}

#[derive(Debug, Args)]
struct SessionListArgs {
    #[arg(long, help = "Unix timestamp in milliseconds")]
    since: Option<u64>,
    #[arg(long, help = "Unix timestamp in milliseconds")]
    until: Option<u64>,
    #[arg(long)]
    cwd: Option<String>,
    #[arg(long)]
    repo: Option<String>,
    #[arg(long)]
    branch: Option<String>,
    #[arg(long)]
    provider: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(
        long,
        help = "running, archived, completed, cancelled, failed, or truncated"
    )]
    status: Option<String>,
    #[arg(long)]
    query: Option<String>,
    #[arg(long, help = "Include archived sessions (excluded by default)")]
    include_archived: bool,
    #[arg(
        long,
        help = "Emit machine-readable JSON instead of tab-separated rows"
    )]
    json: bool,
}

#[derive(Debug, Args)]
struct FeedbackArgs {
    #[arg(value_name = "TEXT", num_args = 0.., trailing_var_arg = true)]
    message: Vec<String>,
    #[arg(long, help = "Print the redacted payload without sending")]
    preview: bool,
    #[arg(long, help = "Send without an interactive confirmation prompt")]
    yes: bool,
}

#[derive(Debug, Args)]
struct SessionExportArgs {
    /// Session id to export. The session must exist either under the live
    /// session root or in the `archived/` tree.
    id: String,
    /// Render a self-contained HTML document (inline CSS, no external
    /// resources) instead of the default JSON bundle. The output is
    /// safe to share over email or attach to a bug report.
    #[arg(long)]
    html: bool,
    /// Optional output path. When omitted JSON goes to stdout; HTML
    /// goes to `squeezy-session-<id>.html` in the current directory.
    #[arg(long)]
    output: Option<PathBuf>,
    /// Color theme baked into the exported HTML. Defaults to dark.
    /// Ignored unless `--html` is set.
    #[arg(long, value_enum, default_value_t = HtmlThemeArg::Dark)]
    theme: HtmlThemeArg,
    /// Drop tool calls and tool outputs from the export. Useful when
    /// the conversation is the interesting part and the tool output
    /// would otherwise dominate the document. Ignored unless `--html`
    /// is set.
    #[arg(long = "no-tool-outputs", default_value_t = false)]
    no_tool_outputs: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "lowercase")]
enum HtmlThemeArg {
    Dark,
    Light,
}

impl HtmlThemeArg {
    fn to_theme(self) -> squeezy_agent::ExportTheme {
        match self {
            Self::Dark => squeezy_agent::ExportTheme::Dark,
            Self::Light => squeezy_agent::ExportTheme::Light,
        }
    }
}

#[derive(Debug, Args)]
struct SessionReportArgs {
    id: String,
    #[arg(long, help = "Write archive to this path")]
    output: Option<PathBuf>,
    #[arg(long, help = "Print the redacted report manifest preview")]
    preview: bool,
    #[arg(long, help = "Upload the archive to the configured feedback endpoint")]
    send: bool,
    #[arg(long, help = "Send without an interactive confirmation prompt")]
    yes: bool,
    #[arg(long = "exclude", help = "Exclude a report section")]
    exclude: Vec<String>,
}

/// Worker-thread stack for the multi-threaded runtime.
///
/// The delegate/subagent path nests several `Box::pin`ned async layers
/// (parent tool loop → subagent dispatch → subagent round loop → child
/// tool fan-out), and when a batch fans out across subagents these poll
/// trees stack up on a single worker thread. Tokio's 2 MiB default
/// overflows that combined depth on Windows (smaller default guard
/// pages), so we provision a generous worker stack. 16 MiB matches the
/// worker-stack size Codex provisions for the same agent workload.
const WORKER_THREAD_STACK_SIZE: usize = 16 * 1024 * 1024;

fn main() -> squeezy_core::Result<()> {
    // `block_on` drives the root future on the calling thread, and `run()`
    // performs deep synchronous work for some commands (notably `doctor`).
    // The OS main thread defaults to a ~1 MiB stack on Windows (vs 8 MiB on
    // Unix), which that depth overflows. Run the runtime on a dedicated thread
    // sized like the worker threads so the root future has the same generous
    // stack on every platform. Panics are re-raised so abort/backtrace
    // behavior is unchanged.
    let worker = std::thread::Builder::new()
        .name("squeezy-main".to_string())
        .stack_size(WORKER_THREAD_STACK_SIZE)
        .spawn(run_blocking)
        .map_err(|err| {
            SqueezyError::Config(format!("failed to spawn main runtime thread: {err}"))
        })?;
    match worker.join() {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

fn run_blocking() -> squeezy_core::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(WORKER_THREAD_STACK_SIZE)
        .build()
        .map_err(|err| SqueezyError::Config(format!("failed to build async runtime: {err}")))?
        .block_on(run())
}

async fn run() -> squeezy_core::Result<()> {
    squeezy_core::startup_trace::init();
    squeezy_core::startup_trace::mark("main_start");
    squeezy_core::pre_main_hardening(squeezy_core::HardeningConfig::default());
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    squeezy_core::startup_trace::mark("cli_parsed");
    let stdin_is_tty = print_mode::stdin_is_tty();
    let prompt_mode_active = !cli.prompt.is_empty() || !stdin_is_tty;
    if cli.format == PromptFormat::Json && !prompt_mode_active {
        return Err(SqueezyError::Config(
            "--format json requires --prompt or piped stdin; interactive sessions and subcommands only emit human-formatted output"
                .to_string(),
        ));
    }
    if cli.health {
        // Clap's `conflicts_with_all` on `--health` already rejects
        // `--prompt`, `--list-providers`, and `--list-models`, but the
        // subcommand group is not addressable from `conflicts_with*`
        // in the derive macro, so we surface that conflict here. The
        // failure mode we are preventing is `squeezy --health doctor
        // --probe` silently running plain doctor and dropping the
        // subcommand's flags.
        if cli.command.is_some() {
            return Err(SqueezyError::Config(
                "--health is a compatibility alias for `squeezy doctor` and cannot be combined with a subcommand; \
                 drop --health, or run `squeezy doctor` directly to pass --json/--probe/--only"
                    .to_string(),
            ));
        }
        let report = doctor::run(&DoctorArgs::default()).await?;
        report.print();
        let code = report.exit_code;
        if code != 0 {
            std::process::exit(code);
        }
        return Ok(());
    }
    match &cli.command {
        Some(Command::Config { command }) => {
            return handle_config_command(command.as_ref(), &cli);
        }
        Some(Command::Repo { command }) => return handle_repo_command(command, &cli),
        Some(Command::Sessions { command }) => {
            return handle_sessions_command(command, &cli).await;
        }
        Some(Command::Feedback(args)) => return handle_feedback_command(args, &cli).await,
        Some(Command::Mcp { command }) => return handle_mcp_command(command, &cli).await,
        Some(Command::Skills { command }) => return handle_skills_command(command, &cli),
        Some(Command::Ask(args)) => return handle_ask_command(args).await,
        Some(Command::Auth { command }) => return handle_auth_command(command).await,
        Some(Command::Doctor(args)) => {
            let report = doctor::run(args).await?;
            if let Ok(config) = config_from_cli(&cli) {
                let _ = tokio::time::timeout(
                    Duration::from_millis(250),
                    TelemetryClient::retry_pending_from_config(&config),
                )
                .await;
            }
            report.print();
            let code = report.exit_code;
            if code != 0 {
                std::process::exit(code);
            }
            return Ok(());
        }
        Some(Command::RefreshModels(args)) => {
            return handle_refresh_models(args).await;
        }
        Some(Command::Providers { command }) => return handle_providers_command(command),
        Some(Command::Help { topic }) => return handle_help_command(topic.as_deref(), &cli),
        Some(Command::Parse { command }) => return handle_parse_command(command),
        None => {}
    }

    let mut config = config_from_cli(&cli)?;
    squeezy_core::startup_trace::mark("config_loaded");

    if cli.list_providers {
        for provider in PROVIDERS {
            println!("{provider}");
        }
        return Ok(());
    }

    if cli.list_models {
        let provider = cli.provider.as_deref();
        for model in PROVIDERS
            .iter()
            .copied()
            .filter(|candidate| provider.is_none_or(|provider| provider == *candidate))
            .flat_map(models_for_provider)
        {
            let context_window = model
                .limits
                .map(|limits| limits.context_window_tokens.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let max_output = model
                .limits
                .map(|limits| limits.max_output_tokens.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            println!(
                "{}\t{}\t{:?}\tstreaming={} tools={} json={} vision={} state={} reasoning_tokens={} reasoning_effort={} verbosity={} context_window={} max_output={} tokenizer={} lifecycle={}",
                model.provider,
                model.id,
                model.profile,
                model.capabilities.streaming,
                model.capabilities.tool_calling,
                model.capabilities.json_mode,
                model.capabilities.vision,
                model.capabilities.response_state,
                model.capabilities.reasoning_tokens,
                model.capabilities.reasoning_effort,
                model.capabilities.text_verbosity,
                context_window,
                max_output,
                model.tokenizer.as_str(),
                model.lifecycle.as_str(),
            );
        }
        return Ok(());
    }

    let mut startup_setup_question_count = None;
    let mut startup_open_config_section = None;
    let startup_trailing_question_count =
        usize::from(startup_resume_question_available(&cli, &config));
    let mut startup_terminal = None;
    if should_run_startup_model_selector(&cli, &config)? {
        let mut terminal = squeezy_tui::enter_startup_terminal(&config)?;
        let Some(selection) = run_startup_model_selector(
            &config,
            startup_trailing_question_count,
            Some(&mut terminal),
        )
        .await?
        else {
            return Ok(());
        };
        startup_setup_question_count = Some(selection.question_count);
        startup_open_config_section = selection.open_config_section;
        startup_terminal = Some(terminal);
        config = config_from_cli(&cli)?;
    }

    squeezy_core::startup_trace::mark("model_selector_done");

    let onboarding = prepare_repo_profile(&mut config);
    squeezy_core::startup_trace::mark("repo_profile_done");

    show_telemetry_notice_once(&config);
    let telemetry = TelemetryClient::from_config(&config);
    telemetry.spawn(TelemetryEvent::app_started(&config));
    squeezy_core::startup_trace::mark("telemetry_spawned");

    // Resolve `--session <input>` against the on-disk session store
    // before any downstream code sees it. The user can pass:
    //   * the opaque `sess_<16hex>` handle printed by `squeezy
    //     sessions list` (the documented surface),
    //   * a short unique raw-id prefix (the same way `squeezy sessions
    //     resume abc12` works), or
    //   * the full raw id.
    // Ambiguous and unknown values fail fast with a clear message
    // instead of being forwarded into a generic "session not found"
    // later.
    let session_store = SessionStore::open(&config);
    let resolved_session_id: Option<String> = match cli.session.as_deref() {
        Some(id) => Some(
            resolve_session_input(&session_store, id)
                .map_err(|err| SqueezyError::Tool(format!("--session: {err}")))?,
        ),
        None => None,
    };
    let resume_flag = if cli.continue_session {
        ResumeFlag::Continue
    } else if let Some(id) = resolved_session_id.as_deref() {
        ResumeFlag::Explicit(id)
    } else {
        ResumeFlag::None
    };
    let resume_resolution = if matches!(resume_flag, ResumeFlag::None) {
        ResumeResolution {
            session_id: None,
            note: None,
        }
    } else {
        let sessions = session_store
            .list(&SessionQuery::default())
            .unwrap_or_default();
        let cwd_str = config.workspace_root.display().to_string();
        resolve_resume_session(resume_flag, &sessions, &cwd_str)
    };
    if let Some(note) = &resume_resolution.note {
        eprintln!("{note}");
    }
    // F07: gate cross-project resumes behind a y/N prompt so that
    // `squeezy --session <id>` (or `--continue` falling back to an
    // arbitrary id) cannot silently drag a session into an unrelated
    // checkout. `--continue` already filters by cwd equality, so this
    // is effectively a no-op for that path — the lookup is only on the
    // explicit `--session <id>` flow in practice.
    let resume_session_id_opt = if let Some(id) = resume_resolution.session_id.as_deref() {
        if !confirm_cross_project_resume_stdio(&session_store, id, cli.force_cross_project)? {
            println!("resume cancelled");
            flush_telemetry_best_effort(&telemetry).await;
            return Ok(());
        }
        Some(id.to_string())
    } else {
        None
    };
    squeezy_core::startup_trace::mark("session_resolved");
    if prompt_mode_active {
        let provider = provider_from_app_config(&config);
        let prompts = print_mode::resolve_prompt_inputs(
            &cli.prompt,
            stdin_is_tty,
            print_mode::read_stdin_to_string,
            print_mode::read_prompt_file,
        )?;
        if prompts.is_empty() {
            return Err(SqueezyError::Config(
                "no prompts to send: piped stdin was empty and no --prompt was supplied"
                    .to_string(),
            ));
        }
        // Recognise the `!!` exclude-from-context prefix on each resolved
        // prompt before we hand them to the agent loop. Print mode feeds
        // those prompts back through the same local-shell turn path as the
        // TUI so the command executes while staying out of the LLM-facing
        // conversation.
        let typed_prompts: Vec<print_mode::PromptInput> = prompts
            .into_iter()
            .map(print_mode::classify_prompt)
            .collect();
        // Non-interactive prompt mode has no TUI to seed the summary into,
        // so surface it on stderr before the streamed completion lands on
        // stdout. The TUI path skips this print because it shows the same
        // summary in the transcript's system row.
        if let Some(summary) = &onboarding.visible_summary {
            eprintln!("{summary}");
        }
        let result = run_prompts(
            config,
            provider,
            typed_prompts,
            cli.format,
            cli.prompt_permission_mode,
            resume_resolution.session_id,
            telemetry.clone(),
        )
        .await;
        flush_telemetry_best_effort(&telemetry).await;
        return result;
    }

    // Never block first paint on network I/O. Startup may show a cached update
    // banner, while live update probing remains available through `doctor`.
    let update_banner = update::cached_banner_for_startup();
    let resume_session_id = resume_session_id_opt;
    // The resume picker is opt-in: bare `squeezy` starts a fresh session
    // immediately. `--resume` brings up the picker; `--continue` / `--session`
    // resolve a target directly and skip the picker as before.
    let skip_resume_picker = !cli.resume || cli.no_resume_picker || resume_session_id.is_some();
    let mut onboarding = onboarding;
    squeezy_core::startup_trace::mark("update_banner_done");
    loop {
        let provider = provider_from_app_config(&config);
        squeezy_core::startup_trace::mark("provider_built");
        let startup_profile = squeezy_tui::StartupProfile {
            onboarding_summary: onboarding.visible_summary.clone(),
            languages: onboarding.language_summary.clone(),
            skip_resume_picker,
            update_banner: update_banner.clone(),
            resume_session_id: resume_session_id.clone(),
            setup_question_count: startup_setup_question_count,
            open_config_section: startup_open_config_section,
        };
        let run_result = if let Some(terminal) = startup_terminal.take() {
            squeezy_tui::run_with_startup_profile_in_terminal_and_telemetry(
                terminal,
                config.clone(),
                provider,
                startup_profile,
                telemetry.clone(),
            )
            .await?
        } else {
            squeezy_tui::StartupRunResult {
                outcome: squeezy_tui::run_with_startup_profile_and_telemetry(
                    config.clone(),
                    provider,
                    startup_profile,
                    telemetry.clone(),
                )
                .await?,
                terminal: None,
            }
        };
        let outcome = run_result.outcome;
        match outcome {
            squeezy_tui::StartupRunOutcome::Finished => {
                flush_telemetry_best_effort(&telemetry).await;
                return Ok(());
            }
            squeezy_tui::StartupRunOutcome::BackToSetup
                if startup_setup_question_count.is_some() =>
            {
                let mut terminal = match run_result.terminal {
                    Some(terminal) => terminal,
                    None => squeezy_tui::enter_startup_terminal(&config)?,
                };
                let Some(selection) = run_startup_model_selector(
                    &config,
                    startup_trailing_question_count,
                    Some(&mut terminal),
                )
                .await?
                else {
                    flush_telemetry_best_effort(&telemetry).await;
                    return Ok(());
                };
                startup_setup_question_count = Some(selection.question_count);
                startup_open_config_section = selection.open_config_section;
                startup_terminal = Some(terminal);
                config = config_from_cli(&cli)?;
                onboarding = prepare_repo_profile(&mut config);
            }
            squeezy_tui::StartupRunOutcome::BackToSetup => {
                flush_telemetry_best_effort(&telemetry).await;
                return Ok(());
            }
        }
    }
}

async fn flush_telemetry_best_effort(telemetry: &TelemetryClient) {
    let _ = tokio::time::timeout(Duration::from_millis(250), telemetry.flush()).await;
}

fn config_from_cli(cli: &Cli) -> squeezy_core::Result<AppConfig> {
    let mut config = config_from_cli_provider(cli.provider.as_deref(), cli.profile.as_deref())?;
    let mut cli_used = false;
    if let Some(model) = &cli.model {
        cli_used = true;
        config.model = model.clone();
    }
    if let Some(raw) = cli.model_profile.as_deref() {
        let profile = ModelProfile::parse(raw).ok_or_else(|| {
            SqueezyError::Config(format!(
                "cli: --model-profile: invalid value {raw:?}; expected cheap, balanced, or strong"
            ))
        })?;
        cli_used = true;
        config.profile = profile;
    }
    if let Some(max_output_tokens) = cli.max_output_tokens {
        cli_used = true;
        config.max_output_tokens = Some(max_output_tokens);
    }
    if let Some(mode) = &cli.mode {
        cli_used = true;
        config.session_mode = SessionMode::parse(mode).ok_or_else(|| {
            SqueezyError::Config(format!(
                "cli: --mode: invalid session mode {mode:?}; expected plan or build"
            ))
        })?;
    }
    if let Some(session_dir) = &cli.session_dir {
        cli_used = true;
        config.session_logs.log_dir = Some(session_dir.clone());
    }
    if cli_used && !config.config_sources.iter().any(|source| source == "cli") {
        config.config_sources.push("cli".to_string());
    }
    Ok(config)
}

fn handle_config_command(command: Option<&ConfigCommand>, cli: &Cli) -> squeezy_core::Result<()> {
    match command {
        // `squeezy config` with no subcommand lands on the resource picker.
        None => {
            let config = config_from_cli(cli)?;
            handle_browse_command(&config, &ConfigBrowseArgs::default())
        }
        Some(ConfigCommand::Browse(args)) => {
            let config = config_from_cli(cli)?;
            handle_browse_command(&config, args)
        }
        Some(ConfigCommand::Inspect) => {
            let config = config_from_cli(cli)?;
            // Warn when any settings file contains shell-escape values (= "!command").
            // These execute at config-load time before sandboxing or permission policy.
            // We scan raw TOML rather than the resolved AppConfig because by the time
            // the config is built, shell-escape values have already been executed and
            // replaced with their output.
            if let Ok(sources) = squeezy_core::load_separated_settings_sources() {
                let tier_paths: [(&str, Option<&std::path::Path>); 3] = [
                    ("user", sources.user.as_ref().map(|t| t.path.as_path())),
                    ("repo", sources.project.as_ref().map(|t| t.path.as_path())),
                    ("local", sources.repo.as_ref().map(|t| t.path.as_path())),
                ];
                for (label, path) in tier_paths {
                    if let Some(p) = path {
                        for escaped_line in scan_file_for_shell_escapes(p) {
                            eprintln!(
                                "warning: shell-escape value in {label} config ({}): {escaped_line}",
                                p.display()
                            );
                        }
                    }
                }
            }
            print!("{}", config.inspect_redacted());
            Ok(())
        }
        Some(ConfigCommand::Init {
            scope,
            force,
            with_bundled_skills,
        }) => {
            let (path, template) = if scope.user {
                (default_settings_path(), user_settings_template())
            } else if scope.local {
                // Mirror the logic in load_default_settings_sources: walk up to
                // find squeezy.toml and use its parent as the canonical repo root.
                // Using raw CWD would hash a different path than the runtime uses
                // when the user is in a subdirectory of the repo.
                let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                let repo_root = find_project_settings_path(&cwd)
                    .as_deref()
                    .and_then(|p| p.parent())
                    .map(|p| p.to_path_buf())
                    .unwrap_or(cwd);
                (
                    per_repo_settings_path(&repo_root),
                    local_settings_template(),
                )
            } else {
                let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                (project_init_target(cwd), project_settings_template())
            };
            // Validate flag compatibility before any filesystem writes so a
            // bad combination never overwrites an existing file.
            if *with_bundled_skills && !scope.user {
                return Err(SqueezyError::Config(
                    "--with-bundled-skills is only supported under --user".to_string(),
                ));
            }
            if path.exists() && !*force {
                return Err(SqueezyError::Config(format!(
                    "{} already exists; pass --force to overwrite",
                    path.display()
                )));
            }
            // Use the hardened atomic writer so the settings file gets
            // 0o600 permissions (and 0o700 parent dirs) even when it
            // contains inline provider keys or MCP credentials.
            write_settings_atomic(&path, template.as_bytes())?;
            println!("wrote {}", path.display());
            // On Windows, print a one-line PowerShell command for opening the
            // settings file so users don't have to navigate to %APPDATA%
            // manually. Use a single-quoted PowerShell literal so `$`, `` ` ``,
            // and `"` in the path are not reinterpreted; doubling embedded
            // single quotes is the documented PowerShell escape for a
            // single-quoted literal string.
            #[cfg(target_os = "windows")]
            if scope.user {
                let powershell_literal = path.display().to_string().replace('\'', "''");
                println!("  Open (PowerShell): Invoke-Item '{}'", powershell_literal);
            }
            if *with_bundled_skills {
                // scope.user is already verified above.
                let config = config_from_cli(cli)?;
                let target = &config.skills.user_dir;
                let written = squeezy_skills::install_bundled_skills(target).map_err(|err| {
                    SqueezyError::Config(format!(
                        "failed to install bundled skills under {}: {err}",
                        target.display()
                    ))
                })?;
                if written.is_empty() {
                    println!("bundled skills already present under {}", target.display());
                } else {
                    println!(
                        "installed {} bundled skill(s) under {}: {}",
                        written.len(),
                        target.display(),
                        written.join(", ")
                    );
                }
            }
            Ok(())
        }
        Some(ConfigCommand::Validate { strict }) => {
            let config = config_from_cli(cli)?;
            let unknown_field_warnings: Vec<&squeezy_core::ConfigWarning> = config
                .config_warnings
                .iter()
                .filter(|w| {
                    // Unknown field warnings have a dotted TOML path as their
                    // `field`; provider/model compat warnings contain prose.
                    !w.field.contains(' ')
                })
                .collect();
            if unknown_field_warnings.is_empty() {
                println!("config OK — no unknown fields found");
                return Ok(());
            }
            for w in &unknown_field_warnings {
                let prefix = if *strict { "error" } else { "warning" };
                eprintln!(
                    "{prefix}: unknown config field `{}` in {}",
                    w.field, w.source
                );
            }
            if *strict {
                return Err(SqueezyError::Config(format!(
                    "{} unknown config field(s) found (--strict mode)",
                    unknown_field_warnings.len()
                )));
            }
            Ok(())
        }
        Some(ConfigCommand::Explain { field: field_path }) => {
            use squeezy_core::{config_schema::FieldSource, load_separated_settings_sources};
            let parts_owned = split_config_field_path(field_path).map_err(|reason| {
                SqueezyError::Config(format!(
                    "could not parse config field {field_path:?}: {reason}. \
                     Quote keys that contain `.`, e.g. \
                     `model_limits.\"openai:gpt-5.5\".context_window`."
                ))
            })?;
            let parts: Vec<&str> = parts_owned.iter().map(String::as_str).collect();
            let Some(field_meta) = find_config_field_for_path(&parts) else {
                return Err(SqueezyError::Config(format!(
                    "unknown config field {field_path:?}; \
                     use `squeezy config schema` to list all fields. \
                     If a key contains `.` (e.g. a model id), quote it: \
                     `model_limits.\"openai:gpt-5.5\".context_window`."
                )));
            };
            let config = config_from_cli(cli)?;
            let effective_value = explain_effective_value(&config, field_meta, &parts);
            let sources = load_separated_settings_sources()
                .map_err(|e| SqueezyError::Config(format!("failed to load settings tiers: {e}")))?;
            let winning_source = resolve_explain_field_source(&sources, field_meta, &parts);
            let source_path = match winning_source {
                FieldSource::Env => field_meta
                    .env_override
                    .map(|v| format!("${v}"))
                    .unwrap_or_else(|| "env".to_string()),
                FieldSource::Repo => sources
                    .repo
                    .as_ref()
                    .map(|t| t.path.display().to_string())
                    .unwrap_or_else(|| "local tier".to_string()),
                FieldSource::Project => sources
                    .project
                    .as_ref()
                    .map(|t| t.path.display().to_string())
                    .unwrap_or_else(|| "project tier".to_string()),
                FieldSource::User => sources
                    .user
                    .as_ref()
                    .map(|t| t.path.display().to_string())
                    .unwrap_or_else(|| "user tier".to_string()),
                FieldSource::Default => "(built-in default)".to_string(),
            };
            println!("field:   {field_path}");
            println!("value:   {effective_value}");
            println!("source:  {} ({})", winning_source.badge(), source_path);
            if let Some(env_var) = field_meta.env_override {
                if winning_source != FieldSource::Env {
                    println!("env:     ${env_var} (not set — would override if set)");
                } else {
                    println!("env:     ${env_var} (active)");
                }
            }
            println!("apply:   {}", field_meta.tier.label());
            // Show which lower tiers also define this field (they are shadowed).
            let tier_entries: [(FieldSource, &str, Option<&squeezy_core::TierSource>); 3] = [
                (FieldSource::Repo, "local", sources.repo.as_ref()),
                (FieldSource::Project, "repo", sources.project.as_ref()),
                (FieldSource::User, "user", sources.user.as_ref()),
            ];
            let mut printed_header = false;
            for (src, label, tier) in tier_entries {
                if src == winning_source {
                    continue;
                }
                if tier.is_some_and(|t| t.contains_path(&parts)) {
                    if !printed_header {
                        println!("shadowed: (highest precedence first)");
                        printed_header = true;
                    }
                    let path_str = tier
                        .map(|t| t.path.display().to_string())
                        .unwrap_or_default();
                    println!("  {label}: {path_str}");
                }
            }
            Ok(())
        }
        Some(ConfigCommand::Schema) => {
            use squeezy_core::config_schema::CONFIG_SECTIONS;
            let sections: Vec<serde_json::Value> = CONFIG_SECTIONS
                .iter()
                .map(|section| {
                    let fields: Vec<serde_json::Value> = section
                        .fields
                        .iter()
                        .map(|f| {
                            let kind_json = field_kind_to_json(&f.kind);
                            serde_json::json!({
                                "label": f.label,
                                "toml_path": f.toml_path,
                                "kind": kind_json,
                                "apply_tier": f.tier.label(),
                                "default_display": f.default_display,
                                "help": f.help,
                                "env_override": f.env_override,
                                "secret": f.secret,
                            })
                        })
                        .collect();
                    serde_json::json!({
                        "id": section.id.slug(),
                        "label": section.label,
                        "description": section.description,
                        "fields": fields,
                    })
                })
                .collect();
            let json = serde_json::to_string_pretty(&sections)
                .map_err(|e| SqueezyError::Config(format!("schema serialization failed: {e}")))?;
            println!("{json}");
            Ok(())
        }
    }
}

fn find_config_field_for_path(
    requested_path: &[&str],
) -> Option<&'static squeezy_core::config_schema::FieldMeta> {
    use squeezy_core::config_schema::CONFIG_SECTIONS;

    CONFIG_SECTIONS
        .iter()
        .flat_map(|s| s.fields.iter())
        .find(|field| config_field_path_matches(field.toml_path, requested_path))
}

/// Splits a user-supplied dotted TOML key path into segments, honouring
/// TOML basic-string (`"..."`) and literal-string (`'...'`) quoting on
/// individual keys. Naïve `split('.')` breaks any path with a key that
/// contains a `.` — model identifiers like `gpt-5.5`, `claude-3.5-sonnet`,
/// or `gemini-2.5-pro` are the dominant case for `model_limits.<id>.<field>`
/// lookups, so the splitter has to match TOML's tokenisation rather than
/// raw byte-level dots.
///
/// Examples:
///
/// - `model.provider`
///   → `["model", "provider"]`
/// - `model_limits."openai:gpt-5.5".context_window`
///   → `["model_limits", "openai:gpt-5.5", "context_window"]`
/// - `providers.'weird.alias'.cheap_model`
///   → `["providers", "weird.alias", "cheap_model"]`
///
/// Returns a structured error string describing where the parser bailed
/// (unterminated quote, stray characters after a closing quote, empty
/// segment) so the caller can surface a hint rather than an opaque
/// `unknown field` message.
fn split_config_field_path(path: &str) -> Result<Vec<String>, String> {
    if path.is_empty() {
        return Err("empty config field path".to_string());
    }

    let mut segments: Vec<String> = Vec::new();
    let mut current = String::new();
    // True after an unquoted `.`: parser is waiting for the next segment.
    // Falls back to false as soon as a character is pushed onto `current`
    // or a closing quote produces a segment. A trailing `.` therefore ends
    // the loop with this flag still set, which is an error.
    let mut expects_segment = false;
    let mut chars = path.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '"' | '\'' => {
                if !current.is_empty() {
                    return Err(format!(
                        "unexpected quote {ch} inside bare key segment {current:?}"
                    ));
                }
                let quote = ch;
                let mut quoted = String::new();
                let mut closed = false;
                for c in chars.by_ref() {
                    if c == quote {
                        closed = true;
                        break;
                    }
                    quoted.push(c);
                }
                if !closed {
                    return Err(format!("unterminated quoted segment starting with {quote}"));
                }
                current.push_str(&quoted);
                match chars.peek().copied() {
                    Some('.') => {
                        chars.next();
                        segments.push(std::mem::take(&mut current));
                        expects_segment = true;
                    }
                    Some(other) => {
                        return Err(format!(
                            "unexpected character {other:?} after closing quote {quote}"
                        ));
                    }
                    None => {
                        segments.push(std::mem::take(&mut current));
                        expects_segment = false;
                    }
                }
            }
            '.' => {
                if current.is_empty() {
                    return Err("empty key segment".to_string());
                }
                segments.push(std::mem::take(&mut current));
                expects_segment = true;
            }
            _ => {
                current.push(ch);
                expects_segment = false;
            }
        }
    }

    if !current.is_empty() {
        segments.push(current);
        expects_segment = false;
    }

    if expects_segment {
        return Err("trailing `.` without a final key segment".to_string());
    }

    if segments.is_empty() || segments.iter().any(String::is_empty) {
        return Err("empty key segment".to_string());
    }

    Ok(segments)
}

fn config_field_path_matches(schema_path: &[&str], requested_path: &[&str]) -> bool {
    schema_path.len() == requested_path.len()
        && schema_path
            .iter()
            .zip(requested_path.iter())
            .all(|(schema, requested)| *schema == "*" || schema == requested)
}

fn resolve_explain_field_source(
    sources: &squeezy_core::SeparatedSources,
    field: &squeezy_core::config_schema::FieldMeta,
    requested_path: &[&str],
) -> squeezy_core::config_schema::FieldSource {
    use squeezy_core::config_schema::FieldSource;

    if let Some(var_name) = field.env_override
        && std::env::var(var_name).is_ok()
    {
        return FieldSource::Env;
    }
    if let Some(repo) = &sources.repo
        && repo.contains_path(requested_path)
    {
        return FieldSource::Repo;
    }
    if let Some(project) = &sources.project
        && project.contains_path(requested_path)
    {
        return FieldSource::Project;
    }
    if let Some(user) = &sources.user
        && user.contains_path(requested_path)
    {
        return FieldSource::User;
    }
    FieldSource::Default
}

/// Display wrapper for the value rendered by `config explain`. Construction
/// runs the redaction gate, so anything that flows from here into a sink (e.g.
/// `println!`) has provably been screened against `FieldKind::Secret`. The
/// newtype also gives the CodeQL `Cleartext logging of sensitive information`
/// analyzer a structural sanitizer boundary it can recognise — prior to this,
/// the analyzer followed `Option::as_ref` and `preset.default_api_key_env()`
/// flows through `FieldMeta::get` callbacks straight into the explain
/// `println!`, which surfaced as a high-severity alert at the print site even
/// though `FieldValue::Secret::as_display()` already redacts to `"••••"`.
#[derive(Debug, Clone)]
struct RedactedDisplay(String);

impl RedactedDisplay {
    fn safe(text: String) -> Self {
        Self(text)
    }

    fn redacted() -> Self {
        Self(REDACTED_VALUE_DISPLAY.to_string())
    }
}

impl std::fmt::Display for RedactedDisplay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl PartialEq<&str> for RedactedDisplay {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

/// `FieldValue::Secret::as_display()` uses this same sentinel — kept in lock-
/// step so a cosmetic change flips both sites at once.
const REDACTED_VALUE_DISPLAY: &str = "••••";

/// Sentinel rendered when a wildcard field cannot be resolved to a concrete
/// value. Mirrors `FieldValue::as_display()`'s "not set" sentinel so a future
/// cosmetic change (`"—"` → `"(none)"`) flips both sites at once.
const EMPTY_FIELD_DISPLAY: &str = "—";

fn explain_effective_value(
    config: &AppConfig,
    field: &squeezy_core::config_schema::FieldMeta,
    requested_path: &[&str],
) -> RedactedDisplay {
    use squeezy_core::config_schema::FieldKind;

    // SECURITY: secret fields must never leak through `config explain`.
    // `FieldValue::Secret::as_display()` already renders "••••", but routing
    // the check through this explicit branch keeps the redaction contract
    // visible at the call site and lets static analysis prove the printed
    // value of a secret field is the constant sentinel, independent of any
    // `FieldMeta::get` callback or provider-config field.
    if field.secret || matches!(field.kind, FieldKind::Secret { .. }) {
        return RedactedDisplay::redacted();
    }

    let text = concrete_explain_value(config, field.toml_path, requested_path)
        .unwrap_or_else(|| (field.get)(config).as_display());
    RedactedDisplay::safe(text)
}

fn concrete_explain_value(
    config: &AppConfig,
    schema_path: &[&str],
    requested_path: &[&str],
) -> Option<String> {
    match (schema_path, requested_path) {
        (["providers", "*", key], ["providers", provider, _]) => {
            provider_explain_value(config, provider, key)
        }
        (["model_limits", "*", "context_window"], ["model_limits", model_key, _]) => Some(
            config
                .model_limits
                .get(*model_key)
                .and_then(|entry| entry.context_window)
                .map(|value| value.to_string())
                .unwrap_or_else(|| "auto".to_string()),
        ),
        _ => None,
    }
}

fn provider_explain_value(config: &AppConfig, provider: &str, key: &str) -> Option<String> {
    match key {
        "cheap_model" => Some(
            provider_cheap_model(config, provider)
                .unwrap_or_else(|| EMPTY_FIELD_DISPLAY.to_string()),
        ),
        "judge_model" => Some(provider_judge_model(config, provider)),
        "judge_prompt" => Some(provider_judge_prompt(config, provider)),
        "expensive_models" => Some(squeezy_core::resolved_reroute_filter(config, provider)),
        _ => None,
    }
}

fn provider_cheap_model(config: &AppConfig, provider: &str) -> Option<String> {
    let model = config
        .providers
        .get(provider)
        .and_then(|p| p.cheap_model.clone())
        .filter(|model| !model.trim().is_empty())
        .or_else(|| config.small_fast_model.clone())
        .or_else(|| squeezy_core::judge_model_for_provider(provider).map(str::to_string))
        .or_else(|| {
            (provider == "ollama").then(|| squeezy_core::DEFAULT_OLLAMA_MODEL.to_string())
        })?;
    Some(resolve_model_alias_for_display(provider, model))
}

fn provider_judge_model(config: &AppConfig, provider: &str) -> String {
    if let Some(model) = config
        .providers
        .get(provider)
        .and_then(|p| p.judge_model.clone())
        .filter(|model| !model.trim().is_empty())
        .or_else(|| config.routing.judge_model.clone())
    {
        return resolve_model_alias_for_display(provider, model);
    }
    squeezy_core::judge_model_for_provider(provider)
        .map(str::to_string)
        .or_else(|| provider_cheap_model(config, provider))
        .unwrap_or_else(|| EMPTY_FIELD_DISPLAY.to_string())
}

fn provider_judge_prompt(config: &AppConfig, provider: &str) -> String {
    config
        .providers
        .get(provider)
        .and_then(|p| p.judge_prompt.clone())
        .filter(|prompt| !prompt.trim().is_empty())
        .or_else(|| config.routing.judge_prompt.clone())
        .unwrap_or_else(|| squeezy_core::default_judge_prompt(provider).to_string())
}

fn resolve_model_alias_for_display(provider: &str, model: String) -> String {
    squeezy_core::resolve_model_alias(provider, &model)
        .unwrap_or(&model)
        .to_string()
}

/// Reads `path` and returns each non-comment line that has a string value
/// beginning with `!` (shell-escape syntax).  Returns an empty vec if the
/// file cannot be read.
fn scan_file_for_shell_escapes(path: &std::path::Path) -> Vec<String> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|line| {
            let t = line.trim();
            !t.starts_with('#') && t.contains('=')
        })
        .filter_map(|line| {
            let eq_pos = line.find('=')?;
            let val = line[eq_pos + 1..].trim();
            // Strip one layer of surrounding quotes and check for !.
            let inner = val
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .or_else(|| val.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
                .unwrap_or(val);
            if inner.starts_with('!') {
                Some(line.trim().to_string())
            } else {
                None
            }
        })
        .collect()
}

fn field_kind_to_json(kind: &squeezy_core::config_schema::FieldKind) -> serde_json::Value {
    use squeezy_core::config_schema::FieldKind;
    match kind {
        FieldKind::Bool => serde_json::json!({"type": "bool"}),
        FieldKind::Integer { min, max, suffix } => serde_json::json!({
            "type": "integer", "min": min, "max": max, "suffix": suffix
        }),
        FieldKind::OptionalInteger { min, max, suffix } => serde_json::json!({
            "type": "optional_integer", "min": min, "max": max, "suffix": suffix
        }),
        FieldKind::OptionalFloat { min, max } => serde_json::json!({
            "type": "optional_float", "min": min, "max": max
        }),
        FieldKind::Enum { options } => serde_json::json!({
            "type": "enum", "options": options
        }),
        FieldKind::OptionalEnum { options } => serde_json::json!({
            "type": "optional_enum", "options": options
        }),
        FieldKind::String { multiline } => serde_json::json!({
            "type": "string", "multiline": multiline
        }),
        FieldKind::DurationMs => serde_json::json!({"type": "duration_ms"}),
        FieldKind::StringList { min, max } => serde_json::json!({
            "type": "string_list", "min": min, "max": max
        }),
        FieldKind::Path {
            must_exist,
            dir_only,
        } => serde_json::json!({
            "type": "path", "must_exist": must_exist, "dir_only": dir_only
        }),
        FieldKind::Secret { env_var } => serde_json::json!({
            "type": "secret", "env_var": env_var
        }),
        FieldKind::Info => serde_json::json!({"type": "info"}),
        FieldKind::ProviderSubTabs => serde_json::json!({"type": "provider_sub_tabs"}),
        FieldKind::TableArray { kind } => {
            use squeezy_core::config_schema::TableArrayKind;
            match kind {
                TableArrayKind::Keyed { .. } => serde_json::json!({"type": "table_array_keyed"}),
                TableArrayKind::Ordered { .. } => {
                    serde_json::json!({"type": "table_array_ordered"})
                }
            }
        }
    }
}

async fn handle_mcp_command(command: &McpCommand, cli: &Cli) -> squeezy_core::Result<()> {
    match command {
        McpCommand::List { json, probe } => {
            let config = config_from_cli(cli)?;
            // Run a live handshake probe when requested.  The result gives
            // us a per-server ready/stale/failed/cancelled signal and the
            // live tool count — the same information `doctor --probe` reports
            // but scoped to MCP servers only.
            let live_status = if *probe {
                let registry = McpClientRegistry::new(config.mcp_servers.clone());
                let outcome = registry.refresh_tools(CancellationToken::new()).await;
                registry.shutdown().await;
                Some(outcome.status.per_server)
            } else {
                None
            };
            if *json {
                let servers = config
                    .mcp_servers
                    .iter()
                    .map(|(name, server)| {
                        let mut entry = serde_json::json!({
                            "name": name,
                            "enabled": server.enabled,
                            "transport": server.transport.as_str(),
                            "command": server.command,
                            "args": server.args,
                            "url": server.url,
                            "cwd": server.cwd,
                            "timeout_ms": server.timeout_ms,
                            "env": server.env.keys().collect::<Vec<_>>(),
                            "bearer_token_env_var": server.bearer_token_env_var,
                            "http_headers": server.http_headers.keys().collect::<Vec<_>>(),
                            "env_http_headers": server.env_http_headers.keys().collect::<Vec<_>>(),
                            "permission_default": server.permissions.default.map(|value| value.as_str()),
                            "permission_rules": server.permissions.rules.len(),
                        });
                        if let Some(status_map) = &live_status
                            && let Some(status) = status_map.get(name)
                        {
                            entry["probe"] = mcp_status_probe_json(status);
                        }
                        entry
                    })
                    .collect::<Vec<_>>();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&servers).unwrap_or_default()
                );
            } else if config.mcp_servers.is_empty() {
                println!("No MCP servers configured.");
            } else {
                let mut rows: Vec<Vec<String>> = Vec::with_capacity(config.mcp_servers.len() + 1);
                let mut header = vec![
                    "NAME".to_string(),
                    "STATE".to_string(),
                    "TRANSPORT".to_string(),
                    "ENDPOINT".to_string(),
                    "AUTH/ENV".to_string(),
                ];
                if live_status.is_some() {
                    header.push("PROBE".to_string());
                }
                let col_count = header.len();
                rows.push(header);
                for (name, server) in &config.mcp_servers {
                    let state = if server.enabled {
                        "enabled"
                    } else {
                        "disabled"
                    };
                    // For stdio: show command basename + arg count.
                    // For http/sse: show URL host+path (never the token value).
                    let endpoint = match server.transport {
                        squeezy_core::McpTransport::Stdio => {
                            let cmd = server
                                .command
                                .as_deref()
                                .map(|c| {
                                    std::path::Path::new(c)
                                        .file_name()
                                        .and_then(|n| n.to_str())
                                        .unwrap_or(c)
                                        .to_string()
                                })
                                .unwrap_or_else(|| "-".to_string());
                            if server.args.is_empty() {
                                cmd
                            } else {
                                format!(
                                    "{} ({} arg{})",
                                    cmd,
                                    server.args.len(),
                                    if server.args.len() == 1 { "" } else { "s" }
                                )
                            }
                        }
                        squeezy_core::McpTransport::Http | squeezy_core::McpTransport::Sse => {
                            // Strip scheme to keep the column compact while
                            // retaining host, port, and path for identification.
                            server
                                .url
                                .as_deref()
                                .map(|u| {
                                    // Strip leading scheme (http://, https://, etc.)
                                    u.find("://")
                                        .map(|pos| &u[pos + 3..])
                                        .unwrap_or(u)
                                        .to_string()
                                })
                                .unwrap_or_else(|| "-".to_string())
                        }
                    };
                    // Auth/env column: bearer env var name and count of env keys.
                    let mut auth_parts: Vec<String> = Vec::new();
                    if let Some(env_var) = &server.bearer_token_env_var {
                        auth_parts.push(format!("bearer=${env_var}"));
                    }
                    if !server.env.is_empty() {
                        let keys: Vec<&str> = server.env.keys().map(String::as_str).collect();
                        auth_parts.push(format!("env:[{}]", keys.join(",")));
                    }
                    if !server.env_http_headers.is_empty() {
                        auth_parts.push(format!("{} env-header(s)", server.env_http_headers.len()));
                    }
                    let auth_col = if auth_parts.is_empty() {
                        "-".to_string()
                    } else {
                        auth_parts.join(", ")
                    };
                    let mut row = vec![
                        name.clone(),
                        state.to_string(),
                        server.transport.as_str().to_string(),
                        endpoint,
                        auth_col,
                    ];
                    if let Some(status_map) = &live_status {
                        let probe_str = status_map
                            .get(name)
                            .map(mcp_status_probe_str)
                            .unwrap_or_else(|| {
                                // Disabled servers are not probed; distinguish
                                // from servers that simply failed to report.
                                if server.enabled {
                                    "no result".to_string()
                                } else {
                                    "disabled".to_string()
                                }
                            });
                        row.push(probe_str);
                    }
                    rows.push(row);
                }
                let widths = (0..col_count)
                    .map(|col| {
                        rows.iter()
                            .filter_map(|row| row.get(col))
                            .map(|s| s.len())
                            .max()
                            .unwrap_or(0)
                    })
                    .collect::<Vec<_>>();
                for row in &rows {
                    let mut line = String::new();
                    for (i, cell) in row.iter().enumerate() {
                        if i > 0 {
                            line.push_str("  ");
                        }
                        if i + 1 < col_count {
                            let _ = write!(line, "{:<width$}", cell, width = widths[i]);
                        } else {
                            line.push_str(cell);
                        }
                    }
                    println!("{line}");
                }
            }
            Ok(())
        }
        McpCommand::Test { name, json } => {
            let config = config_from_cli(cli)?;
            let Some(server) = config.mcp_servers.get(name).cloned() else {
                return Err(SqueezyError::Config(format!(
                    "MCP server {name:?} is not configured"
                )));
            };
            // `mcp test` is an explicit operator action ("probe this
            // server now"), so we honor it even when `enabled = false`
            // in settings — running `squeezy mcp disable foo &&
            // squeezy mcp test foo` should still report whether `foo`
            // would handshake. The probe is one-shot and uses a
            // throwaway registry, so this never persists `enabled =
            // true`; we just surface the override in both human and
            // JSON output so the operator is not surprised when the
            // probe succeeds against a `disabled` server.
            let enabled_in_config = server.enabled;
            let server = McpServerConfig {
                enabled: true,
                ..server
            };
            let mut servers = BTreeMap::new();
            servers.insert(name.clone(), server);
            const MCP_TEST_TIMEOUT_SECS: u64 = 30;
            let registry = McpClientRegistry::new(servers);
            let timed_out = tokio::time::timeout(
                Duration::from_secs(MCP_TEST_TIMEOUT_SECS),
                registry.refresh_tools(CancellationToken::new()),
            )
            .await;
            registry.shutdown().await;
            let (status_label, detail, tools_count) = match timed_out {
                Err(_) => (
                    "warn",
                    format!("handshake timed out after {MCP_TEST_TIMEOUT_SECS}s"),
                    None,
                ),
                Ok(outcome) => {
                    let status = outcome.status.per_server.get(name);
                    match status {
                        Some(McpServerStatus::Ready { tools_count, .. }) => (
                            "ok",
                            format!("handshake ok; {tools_count} tools advertised"),
                            Some(*tools_count),
                        ),
                        Some(McpServerStatus::Stale {
                            tools_count,
                            outcome,
                        }) => (
                            "warn",
                            format!(
                                "handshake stale; serving {tools_count} cached tools after {}",
                                mcp_stale_outcome_detail(outcome)
                            ),
                            Some(*tools_count),
                        ),
                        Some(McpServerStatus::Failed { error }) => {
                            ("fail", format!("handshake failed: {error}"), None)
                        }
                        Some(McpServerStatus::Cancelled) => (
                            "warn",
                            "handshake timed out or was cancelled".to_string(),
                            None,
                        ),
                        Some(McpServerStatus::Starting) => {
                            ("warn", "handshake did not complete".to_string(), None)
                        }
                        None => (
                            "warn",
                            "server did not produce a probe status".to_string(),
                            None,
                        ),
                    }
                }
            };
            let detail_with_note = if enabled_in_config {
                detail.clone()
            } else {
                format!("(server was disabled in config; re-enabled for probe) {detail}")
            };
            if *json {
                let body = serde_json::json!({
                    "name": name,
                    "status": status_label,
                    "detail": &detail_with_note,
                    "tools_count": tools_count,
                    "enabled_in_config": enabled_in_config,
                });
                // Emit the JSON body before any potential error return
                // below so machine consumers always receive a parseable
                // payload, even on `fail`. The non-zero exit still
                // signals failure for shell-level callers.
                println!(
                    "{}",
                    serde_json::to_string_pretty(&body).map_err(|err| {
                        SqueezyError::Parse(format!("failed to serialize MCP test: {err}"))
                    })?
                );
            } else {
                println!("[{status_label}] mcp:{name}  {detail_with_note}");
            }
            if status_label == "fail" {
                return Err(SqueezyError::Tool(detail_with_note));
            }
            Ok(())
        }
        McpCommand::Add(args) => {
            let config = config_from_cli(cli)?;
            update_mcp_settings(&config, &args.scope, |servers| {
                validate_mcp_name(&args.name)?;
                if servers.contains_key(&args.name) {
                    return Err(SqueezyError::Config(format!(
                        "MCP server {:?} already exists in selected settings file",
                        args.name
                    )));
                }
                let transport = parse_mcp_transport(&args.transport)?;
                match transport {
                    McpTransport::Stdio if args.command.as_deref().unwrap_or("").is_empty() => {
                        return Err(SqueezyError::Config(
                            "stdio MCP servers require --command".to_string(),
                        ));
                    }
                    McpTransport::Http | McpTransport::Sse
                        if args.url.as_deref().unwrap_or("").is_empty() =>
                    {
                        return Err(SqueezyError::Config(
                            "http and sse MCP servers require --url".to_string(),
                        ));
                    }
                    McpTransport::Http | McpTransport::Sse => {
                        // Warn when the URL contains bare `localhost`. On
                        // Windows, IPv4/IPv6 loopback resolution can differ from
                        // macOS/Linux depending on the system hosts file and
                        // firewall configuration. Prefer explicit `127.0.0.1` or
                        // `[::1]` for predictable behavior.
                        if let Some(url) = args.url.as_deref()
                            && let Some(host_part) = url.find("://").map(|p| &url[p + 3..])
                        {
                            let host_end = host_part.find('/').unwrap_or(host_part.len());
                            let host = &host_part[..host_end];
                            let bare_host = host.rfind(':').map(|p| &host[..p]).unwrap_or(host);
                            if bare_host.eq_ignore_ascii_case("localhost") {
                                eprintln!(
                                    "warning: URL uses `localhost` which may resolve \
                                     differently on Windows (IPv4 vs IPv6 loopback). \
                                     Consider using `127.0.0.1` or `[::1]` explicitly."
                                );
                            }
                        }
                    }
                    _ => {}
                }
                // Reject HTTP-only fields when the transport is stdio; the runtime
                // silently ignores them, which would mislead the user.
                if matches!(transport, McpTransport::Stdio) {
                    if args.bearer_token_env_var.is_some() {
                        return Err(SqueezyError::Config(
                            "--bearer-token-env-var is only valid for http and sse transports"
                                .to_string(),
                        ));
                    }
                    if !args.http_headers.is_empty() {
                        return Err(SqueezyError::Config(
                            "--http-header is only valid for http and sse transports".to_string(),
                        ));
                    }
                    if !args.env_http_headers.is_empty() {
                        return Err(SqueezyError::Config(
                            "--env-http-header is only valid for http and sse transports"
                                .to_string(),
                        ));
                    }
                }
                let mut server = Table::new();
                server.insert("enabled", Item::Value(TomlValue::from(true)));
                server.insert(
                    "transport",
                    Item::Value(TomlValue::from(transport.as_str())),
                );
                if let Some(command) = &args.command {
                    server.insert("command", Item::Value(TomlValue::from(command.as_str())));
                }
                if !args.args.is_empty() {
                    let mut array = toml_edit::Array::default();
                    for arg in &args.args {
                        array.push(arg.as_str());
                    }
                    server.insert("args", Item::Value(TomlValue::Array(array)));
                }
                if let Some(url) = &args.url {
                    server.insert("url", Item::Value(TomlValue::from(url.as_str())));
                }
                if let Some(timeout_ms) = args.timeout_ms {
                    server.insert(
                        "timeout_ms",
                        Item::Value(TomlValue::from(timeout_ms as i64)),
                    );
                }
                if let Some(ms) = args.discovery_timeout_ms {
                    server.insert(
                        "discovery_timeout_ms",
                        Item::Value(TomlValue::from(ms as i64)),
                    );
                }
                if let Some(ms) = args.tool_call_timeout_ms {
                    server.insert(
                        "tool_call_timeout_ms",
                        Item::Value(TomlValue::from(ms as i64)),
                    );
                }
                if !args.env.is_empty() {
                    let mut env = toml_edit::InlineTable::default();
                    for entry in &args.env {
                        let (key, value) = parse_env_entry(entry)?;
                        env.insert(key, TomlValue::from(value));
                    }
                    server.insert("env", Item::Value(TomlValue::InlineTable(env)));
                }
                if let Some(cwd) = &args.cwd {
                    server.insert("cwd", Item::Value(TomlValue::from(cwd.as_str())));
                }
                if let Some(bearer_env_var) = &args.bearer_token_env_var {
                    server.insert(
                        "bearer_token_env_var",
                        Item::Value(TomlValue::from(bearer_env_var.as_str())),
                    );
                }
                if !args.http_headers.is_empty() {
                    let mut headers = toml_edit::InlineTable::default();
                    for entry in &args.http_headers {
                        let (key, value) = parse_env_entry(entry)?;
                        headers.insert(key, TomlValue::from(value));
                    }
                    server.insert("http_headers", Item::Value(TomlValue::InlineTable(headers)));
                }
                if !args.env_http_headers.is_empty() {
                    let mut env_headers = toml_edit::InlineTable::default();
                    for entry in &args.env_http_headers {
                        let (key, value) = parse_env_entry(entry)?;
                        env_headers.insert(key, TomlValue::from(value));
                    }
                    server.insert(
                        "env_http_headers",
                        Item::Value(TomlValue::InlineTable(env_headers)),
                    );
                }
                if let Some(default) = &args.permission_default {
                    let default = parse_permission(default)?;
                    let mut permissions = Table::new();
                    permissions.insert("default", Item::Value(TomlValue::from(default.as_str())));
                    server.insert("permissions", Item::Table(permissions));
                }
                if !args.enabled_tools.is_empty() {
                    let mut array = toml_edit::Array::default();
                    for tool in &args.enabled_tools {
                        array.push(tool.as_str());
                    }
                    server.insert("enabled_tools", Item::Value(TomlValue::Array(array)));
                }
                if !args.disabled_tools.is_empty() {
                    let mut array = toml_edit::Array::default();
                    for tool in &args.disabled_tools {
                        array.push(tool.as_str());
                    }
                    server.insert("disabled_tools", Item::Value(TomlValue::Array(array)));
                }
                if let Some(var) = &args.bearer_token_env_var {
                    server.insert(
                        "bearer_token_env_var",
                        Item::Value(TomlValue::from(var.as_str())),
                    );
                }
                if !args.http_headers.is_empty() {
                    let mut table = toml_edit::InlineTable::default();
                    for entry in &args.http_headers {
                        let (name, value) = parse_env_entry(entry)?;
                        table.insert(name, TomlValue::from(value));
                    }
                    server.insert("http_headers", Item::Value(TomlValue::InlineTable(table)));
                }
                if !args.env_http_headers.is_empty() {
                    let mut table = toml_edit::InlineTable::default();
                    for entry in &args.env_http_headers {
                        let (name, value) = parse_env_entry(entry)?;
                        table.insert(name, TomlValue::from(value));
                    }
                    server.insert(
                        "env_http_headers",
                        Item::Value(TomlValue::InlineTable(table)),
                    );
                }
                servers.insert(&args.name, Item::Table(server));
                Ok(())
            })
        }
        McpCommand::Enable(args) => set_mcp_enabled(cli, args, true),
        McpCommand::Disable(args) => set_mcp_enabled(cli, args, false),
        McpCommand::Remove(args) => {
            let config = config_from_cli(cli)?;
            update_mcp_settings(&config, &args.scope, |servers| {
                if servers.remove(&args.name).is_none() {
                    return Err(SqueezyError::Config(format!(
                        "MCP server {:?} was not found in selected settings file",
                        args.name
                    )));
                }
                Ok(())
            })
        }
    }
}

fn mcp_status_probe_str(status: &McpServerStatus) -> String {
    match status {
        McpServerStatus::Ready { tools_count, .. } => format!("ready ({tools_count} tools)"),
        McpServerStatus::Stale {
            tools_count,
            outcome,
        } => {
            let reason = match outcome {
                McpStaleOutcome::Failed { error } => format!("discovery failed: {error}"),
                McpStaleOutcome::Cancelled => "discovery cancelled".to_string(),
            };
            format!("stale ({tools_count} cached; {reason})")
        }
        McpServerStatus::Failed { error } => format!("failed: {error}"),
        McpServerStatus::Cancelled => "cancelled".to_string(),
        McpServerStatus::Starting => "did not complete".to_string(),
    }
}

fn mcp_status_probe_json(status: &McpServerStatus) -> serde_json::Value {
    match status {
        McpServerStatus::Ready {
            tools_count,
            cached,
        } => serde_json::json!({
            "status": "ready",
            "tools_count": tools_count,
            "cached": cached,
        }),
        McpServerStatus::Stale {
            tools_count,
            outcome,
        } => {
            let (outcome_str, error) = match outcome {
                McpStaleOutcome::Failed { error } => ("failed", Some(error.as_str())),
                McpStaleOutcome::Cancelled => ("cancelled", None),
            };
            serde_json::json!({
                "status": "stale",
                "tools_count": tools_count,
                "outcome": outcome_str,
                "error": error,
            })
        }
        McpServerStatus::Failed { error } => serde_json::json!({
            "status": "failed",
            "error": error,
        }),
        McpServerStatus::Cancelled => serde_json::json!({"status": "cancelled"}),
        McpServerStatus::Starting => serde_json::json!({"status": "starting"}),
    }
}

fn set_mcp_enabled(cli: &Cli, args: &McpNameScope, enabled: bool) -> squeezy_core::Result<()> {
    let config = config_from_cli(cli)?;
    update_mcp_settings(&config, &args.scope, |servers| {
        let Some(item) = servers.get_mut(&args.name) else {
            return Err(SqueezyError::Config(format!(
                "MCP server {:?} was not found in selected settings file",
                args.name
            )));
        };
        let Some(table) = item.as_table_mut() else {
            return Err(SqueezyError::Config(format!(
                "MCP server {:?} is not a table",
                args.name
            )));
        };
        table.insert("enabled", Item::Value(TomlValue::from(enabled)));
        Ok(())
    })
}

fn handle_skills_command(command: &SkillsCommand, cli: &Cli) -> squeezy_core::Result<()> {
    match command {
        SkillsCommand::List { json } => skills_list(cli, *json),
        SkillsCommand::Enable(args) => skills_set_enabled(cli, args, true),
        SkillsCommand::Disable(args) => skills_set_enabled(cli, args, false),
        SkillsCommand::Validate { json } => skills_validate(cli, *json),
        SkillsCommand::Install { force } => skills_install(cli, *force),
        SkillsCommand::Paths { json } => skills_paths(cli, *json),
        SkillsCommand::Show { name, preview } => skills_show(cli, name, *preview),
    }
}

fn skills_show(cli: &Cli, name: &str, preview: bool) -> squeezy_core::Result<()> {
    let config = config_from_cli(cli)?;
    let catalog = squeezy_skills::SkillCatalog::discover(&config.workspace_root, &config.skills);
    let summaries = catalog.summaries();
    let Some(summary) = summaries.iter().find(|s| s.name == name) else {
        return Err(squeezy_core::SqueezyError::Config(format!(
            "skill `{name}` not found; run `squeezy skills list` to see discovered skills"
        )));
    };
    println!("name:         {}", summary.name);
    println!("description:  {}", summary.description);
    if let Some(wu) = &summary.when_to_use {
        println!("when_to_use:  {wu}");
    }
    println!("source:       {}", summary.source.as_str());
    println!(
        "state:        {}",
        if summary.disabled {
            "disabled"
        } else if catalog.ambiguous_names().contains(&summary.name) {
            "ambiguous (duplicate name at same precedence)"
        } else {
            "enabled"
        }
    );
    println!("context_mode: {}", summary.context_mode.as_str());
    println!("location:     {}", summary.location.display());
    if let Some(manifest) = &summary.manifest {
        if !manifest.tool_deps.is_empty() {
            println!("tool_deps:    {}", manifest.tool_deps.join(", "));
        }
        if let Some(icon) = &manifest.icon {
            println!("icon:         {}", icon.display());
        }
        if let Some(hint) = &manifest.prompt_hint {
            println!("prompt_hint:  {hint}");
        }
    }
    // Show any ambiguous trigger phrases declared by THIS skill.
    let ambiguous_triggers = catalog.ambiguous_triggers();
    if !ambiguous_triggers.is_empty()
        && let Ok(content) = fs::read_to_string(&summary.location)
    {
        let skill_triggers = squeezy_skills::parse_skill_triggers(&content);
        let ambiguous_for_skill: Vec<&String> = ambiguous_triggers
            .iter()
            .filter(|t| skill_triggers.iter().any(|s| s == t.as_str()))
            .collect();
        if !ambiguous_for_skill.is_empty() {
            let listed = ambiguous_for_skill
                .iter()
                .map(|t| format!("`{t}`"))
                .collect::<Vec<_>>()
                .join(", ");
            println!(
                "ambiguous triggers: {listed} (declared by multiple skills; auto-activation skipped — use `/skill {name}` or `load_skill` to select explicitly)"
            );
        }
    }
    if preview {
        match catalog.load(name) {
            Ok(loaded) => {
                let body = &loaded.body;
                let shown = if body.chars().count() > 400 {
                    let truncated: String = body.chars().take(400).collect();
                    format!("{truncated}…")
                } else {
                    body.clone()
                };
                println!("\n--- body preview ---\n{shown}");
            }
            Err(err) => {
                println!("body preview: (error loading skill: {err})");
            }
        }
    }
    Ok(())
}

fn skills_install(cli: &Cli, force: bool) -> squeezy_core::Result<()> {
    let config = config_from_cli(cli)?;
    let target = &config.skills.user_dir;
    if target.is_relative() {
        eprintln!(
            "warning: bundled skills will be installed under a relative path ({}) \
             which is relative to the current working directory. Set HOME or \
             SQUEEZY_SKILLS_USER_DIR to an absolute path to avoid this.",
            target.display(),
        );
    }
    if force {
        for source in skills_bundled_dir_names() {
            let dir = target.join(source);
            if dir.exists() {
                fs::remove_dir_all(&dir).map_err(|err| {
                    SqueezyError::Config(format!("failed to remove {}: {err}", dir.display()))
                })?;
            }
        }
    }
    let written = squeezy_skills::install_bundled_skills(target).map_err(|err| {
        SqueezyError::Config(format!(
            "failed to install bundled skills under {}: {err}",
            target.display()
        ))
    })?;
    if written.is_empty() {
        println!(
            "no new bundled skills installed (all already present under {})",
            target.display()
        );
    } else {
        println!(
            "installed {} bundled skill(s) under {}: {}",
            written.len(),
            target.display(),
            written.join(", ")
        );
    }
    Ok(())
}

/// Names of the in-binary bundled skills. Hardcoded for the `--force`
/// path so the installer doesn't need to enumerate `bundled_skills()`
/// (which would parse every body upfront).
fn skills_bundled_dir_names() -> &'static [&'static str] {
    &[
        "customize-squeezy",
        "release-notes",
        "skill-creator",
        "trace-symbol",
    ]
}

/// Print every directory that will be scanned for skills, in discovery order,
/// together with the configuration tier that produced each path.  Useful for
/// diagnosing unexpected skill-discovery behaviour on Linux where multiple
/// paths (XDG, legacy, extra roots, project, ancestors) may coexist.
fn skills_paths(cli: &Cli, json: bool) -> squeezy_core::Result<()> {
    let config = config_from_cli(cli)?;
    let s = &config.skills;

    // Build an ordered list of (tier, path) entries.
    #[derive(serde::Serialize)]
    struct PathEntry {
        tier: &'static str,
        path: std::path::PathBuf,
        source: &'static str,
    }

    let mut entries: Vec<PathEntry> = Vec::new();

    entries.push(PathEntry {
        tier: "compat_user",
        path: s.compat_user_dir.clone(),
        source: "settings/SQUEEZY_SKILLS_COMPAT_USER_DIR/default",
    });
    entries.push(PathEntry {
        tier: "user",
        path: s.user_dir.clone(),
        source: "settings/SQUEEZY_SKILLS_USER_DIR/default ($HOME/.squeezy/skills)",
    });
    if let Some(xdg) = &s.xdg_user_dir {
        entries.push(PathEntry {
            tier: "xdg_user",
            path: xdg.clone(),
            source: "XDG_DATA_HOME/squeezy/skills or $HOME/.local/share/squeezy/skills",
        });
    }
    for extra in &s.extra_roots {
        entries.push(PathEntry {
            tier: "extra_root",
            path: extra.clone(),
            source: "settings extra_roots",
        });
    }
    entries.push(PathEntry {
        tier: "project (.agents/skills)",
        path: config.workspace_root.join(".agents/skills"),
        source: "workspace root (compat)",
    });
    entries.push(PathEntry {
        tier: "project (.squeezy/skills)",
        path: config.workspace_root.join(".squeezy/skills"),
        source: "workspace root",
    });
    let mut seen_paths = entries
        .iter()
        .map(|entry| entry.path.clone())
        .collect::<BTreeSet<_>>();
    for path in squeezy_skills::skill_scan_dirs(&config.workspace_root, &config.skills) {
        if !seen_paths.insert(path.clone()) {
            continue;
        }
        let tier = if path.ends_with(".agents/skills") {
            "ancestor (.agents/skills)"
        } else if path.ends_with(".squeezy/skills") {
            "ancestor (.squeezy/skills)"
        } else {
            "scan"
        };
        entries.push(PathEntry {
            tier,
            path,
            source: "discovery scan",
        });
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&entries).unwrap_or_default()
        );
        return Ok(());
    }

    if entries.iter().any(|entry| entry.path.is_relative()) {
        eprintln!("warning: one or more skill roots are cwd-relative paths.");
    }

    let width = entries.iter().map(|e| e.tier.len()).max().unwrap_or(4);
    for entry in &entries {
        println!(
            "  {:<width$}  {}",
            entry.tier,
            entry.path.display(),
            width = width,
        );
    }
    Ok(())
}

fn skills_list(cli: &Cli, json: bool) -> squeezy_core::Result<()> {
    let config = config_from_cli(cli)?;
    let catalog = squeezy_skills::SkillCatalog::discover(&config.workspace_root, &config.skills);
    let summaries = catalog.summaries();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&catalog.summaries_json()).unwrap_or_default()
        );
        return Ok(());
    }
    // Show the scanned roots preface so users (especially on Windows) can see
    // exactly which directories Squeezy searched — network shares, OneDrive
    // paths, %APPDATA%, and workspace roots all appear here.
    // Use catalog.scanned_roots() to avoid a second ancestor-walk.
    println!("Scanned roots:");
    for dir in catalog.scanned_roots() {
        println!("  {}", dir.display());
    }
    println!();
    if summaries.is_empty() {
        println!("No skills discovered.");
        return Ok(());
    }
    // Warn about ambiguous triggers so users see them without running validate.
    let ambiguous_triggers = catalog.ambiguous_triggers();
    if !ambiguous_triggers.is_empty() {
        let list = ambiguous_triggers
            .iter()
            .map(|t| format!("`{t}`"))
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "warning: ambiguous trigger phrase(s) declared by multiple skills — auto-activation skipped for {list}. Use `/skill <name>` or `load_skill` to select explicitly, or run `squeezy skills validate` for details.\n"
        );
    }
    const DESC_MAX_CHARS: usize = 48;
    let mut rows: Vec<[String; 5]> = Vec::with_capacity(summaries.len() + 1);
    rows.push([
        "NAME".to_string(),
        "STATE".to_string(),
        "SOURCE".to_string(),
        "DESCRIPTION".to_string(),
        "LOCATION".to_string(),
    ]);
    for summary in &summaries {
        let state = if summary.disabled {
            "disabled"
        } else if catalog.ambiguous_names().contains(&summary.name) {
            "ambiguous"
        } else {
            "enabled"
        };
        let desc = if summary.description.chars().count() <= DESC_MAX_CHARS {
            summary.description.clone()
        } else {
            let truncated: String = summary.description.chars().take(DESC_MAX_CHARS).collect();
            format!("{truncated}…")
        };
        rows.push([
            summary.name.clone(),
            state.to_string(),
            summary.source.as_str().to_string(),
            desc,
            summary.location.display().to_string(),
        ]);
    }
    let widths = (0..4)
        .map(|col| rows.iter().map(|row| row[col].len()).max().unwrap_or(0))
        .collect::<Vec<_>>();
    for row in rows {
        println!(
            "{:<w0$}  {:<w1$}  {:<w2$}  {:<w3$}  {}",
            row[0],
            row[1],
            row[2],
            row[3],
            row[4],
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2],
            w3 = widths[3],
        );
    }
    Ok(())
}

fn skills_set_enabled(
    cli: &Cli,
    args: &SkillsSelectorScope,
    enabled: bool,
) -> squeezy_core::Result<()> {
    let config = config_from_cli(cli)?;
    update_skills_config(&config, &args.scope, |entries| {
        skills_upsert_entry(entries, &args.selector, enabled)
    })
}

/// Maximum body size (in bytes) before `skills validate` emits a size warning.
const SKILL_BODY_WARN_BYTES: usize = 32_768; // 32 KB

fn skills_validate(cli: &Cli, json: bool) -> squeezy_core::Result<()> {
    let config = config_from_cli(cli)?;
    // Walk every configured skill root directly rather than iterating
    // catalog.summaries(). Discovery silently drops malformed SKILL.md
    // files (parse errors, invalid names) with a tracing warn; validate
    // must surface those failures, so it scans the filesystem itself.
    let raw_results = squeezy_skills::validate_skill_dirs(&config.workspace_root, &config.skills);
    // Build the catalog separately for ambiguous-name/trigger detection.
    let catalog = squeezy_skills::SkillCatalog::discover(&config.workspace_root, &config.skills);
    let ambiguous_trigger_set = catalog.ambiguous_triggers();

    let mut diagnostics: Vec<serde_json::Value> = Vec::new();
    let mut ok = 0usize;
    let mut errored = 0usize;
    let mut ambiguous = 0usize;
    let mut warned = 0usize;
    for result in &raw_results {
        let mut errors: Vec<String> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();
        if let Err(err) = &result.outcome {
            errors.push(err.clone());
        }
        // Flag ambiguous names for skills that parsed successfully.
        if let Some(name) = &result.name
            && catalog.ambiguous_names().contains(name)
        {
            ambiguous += 1;
            errors.push(
                "duplicate name at same precedence; auto-trigger activation skipped".to_string(),
            );
        }
        // Extended authoring lint: check for ambiguous triggers, oversized bodies,
        // and missing hook scripts when the file parses successfully.
        if result.outcome.is_ok()
            && let Some(_name) = &result.name
            && let Ok(content) = fs::read_to_string(&result.path)
        {
            let skill_dir = result.path.parent().unwrap_or(std::path::Path::new("."));
            let lint_issues = squeezy_skills::lint_skill_extended(
                &content,
                skill_dir,
                ambiguous_trigger_set,
                SKILL_BODY_WARN_BYTES,
            );
            for (severity, message) in lint_issues {
                if severity == "warning" {
                    warnings.push(message);
                } else {
                    errors.push(message);
                }
            }
        }

        let has_errors = !errors.is_empty();
        let has_warnings = !warnings.is_empty();
        if has_errors {
            errored += 1;
        } else if has_warnings {
            warned += 1;
        } else {
            ok += 1;
        }
        let display_name = result.name.as_deref().unwrap_or(
            result
                .path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("?"),
        );
        let mut all_issues = errors.clone();
        all_issues.extend(warnings.iter().map(|w| format!("warning: {w}")));
        diagnostics.push(serde_json::json!({
            "name": result.name,
            "location": result.path,
            "errors": errors,
            "warnings": warnings,
            "issues": all_issues,
            "display_name": display_name,
        }));
    }
    let total = raw_results.len();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "skills": diagnostics,
                "summary": {
                    "ok": ok,
                    "warned": warned,
                    "errored": errored,
                    "ambiguous": ambiguous,
                    "total": total,
                },
            }))
            .unwrap_or_default()
        );
    } else {
        for entry in &diagnostics {
            let name = entry
                .get("display_name")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let errors = entry
                .get("errors")
                .and_then(|v| v.as_array())
                .map(|arr| arr.len())
                .unwrap_or(0);
            let warnings = entry
                .get("warnings")
                .and_then(|v| v.as_array())
                .map(|arr| arr.len())
                .unwrap_or(0);
            if errors > 0 {
                println!("error   {name}");
                if let Some(arr) = entry.get("errors").and_then(|v| v.as_array()) {
                    for item in arr {
                        if let Some(text) = item.as_str() {
                            println!("          error:   {text}");
                        }
                    }
                }
            } else if warnings > 0 {
                println!("warn    {name}");
                if let Some(arr) = entry.get("warnings").and_then(|v| v.as_array()) {
                    for item in arr {
                        if let Some(text) = item.as_str() {
                            println!("          warning: {text}");
                        }
                    }
                }
            } else {
                println!("ok      {name}");
            }
        }
        println!(
            "{} ok, {} warning(s), {} error(s), {} ambiguous, {} total",
            ok, warned, errored, ambiguous, total
        );
    }
    if errored > 0 || ambiguous > 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn skills_settings_path(config: &AppConfig, scope: &SkillsConfigScope) -> PathBuf {
    if scope.user {
        default_settings_path()
    } else {
        config.workspace_root.join(PROJECT_SETTINGS_FILE)
    }
}

fn update_skills_config(
    config: &AppConfig,
    scope: &SkillsConfigScope,
    update: impl FnOnce(&mut toml_edit::ArrayOfTables) -> squeezy_core::Result<()>,
) -> squeezy_core::Result<()> {
    let path = skills_settings_path(config, scope);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    let mut doc = text
        .parse::<DocumentMut>()
        .map_err(|err| SqueezyError::Config(format!("{}: {err}", path.display())))?;
    let entries = ensure_skills_config_array(&mut doc)?;
    update(entries)?;
    write_settings_atomic(&path, doc.to_string().as_bytes())?;
    println!("updated {}", path.display());
    Ok(())
}

fn ensure_skills_config_array(
    doc: &mut DocumentMut,
) -> squeezy_core::Result<&mut toml_edit::ArrayOfTables> {
    let skills = ensure_table(doc.as_table_mut(), "skills")?;
    let item = skills
        .entry("config")
        .or_insert_with(|| Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
    if !item.is_array_of_tables() {
        return Err(SqueezyError::Config(
            "skills.config exists but is not an array of tables".to_string(),
        ));
    }
    Ok(item
        .as_array_of_tables_mut()
        .expect("checked array of tables"))
}

/// Find an existing `[[skills.config]]` entry that matches `selector`
/// or push a new one, then set its `enabled` field. Honors the same
/// `name` XOR `path` selector contract that `SkillCatalog::apply_config_rules`
/// enforces at load time.
fn skills_upsert_entry(
    entries: &mut toml_edit::ArrayOfTables,
    selector: &SkillsSelector,
    enabled: bool,
) -> squeezy_core::Result<()> {
    let selector_path = selector
        .path
        .as_ref()
        .map(|path| path.display().to_string());
    for entry in entries.iter_mut() {
        let entry_name = entry.get("name").and_then(|item| item.as_str());
        let entry_path = entry.get("path").and_then(|item| item.as_str());
        let matches_name = match (selector.name.as_deref(), entry_name) {
            (Some(needle), Some(found)) => needle == found,
            _ => false,
        };
        let matches_path = match (selector_path.as_deref(), entry_path) {
            (Some(needle), Some(found)) => needle == found,
            _ => false,
        };
        if matches_name || matches_path {
            entry.insert("enabled", Item::Value(TomlValue::from(enabled)));
            return Ok(());
        }
    }
    let mut entry = Table::new();
    if let Some(name) = selector.name.as_deref() {
        entry.insert("name", Item::Value(TomlValue::from(name)));
    } else if let Some(path) = selector_path.as_deref() {
        entry.insert("path", Item::Value(TomlValue::from(path)));
    } else {
        return Err(SqueezyError::Config(
            "skills enable/disable requires --name or --path".to_string(),
        ));
    }
    entry.insert("enabled", Item::Value(TomlValue::from(enabled)));
    entries.push(entry);
    Ok(())
}

fn update_mcp_settings(
    config: &AppConfig,
    scope: &McpConfigScope,
    update: impl FnOnce(&mut Table) -> squeezy_core::Result<()>,
) -> squeezy_core::Result<()> {
    let path = mcp_settings_path(config, scope);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    let mut doc = text
        .parse::<DocumentMut>()
        .map_err(|err| SqueezyError::Config(format!("{}: {err}", path.display())))?;
    let servers = ensure_mcp_servers_table(&mut doc)?;
    update(servers)?;
    write_settings_atomic(&path, doc.to_string().as_bytes())?;
    println!("updated {}", path.display());
    Ok(())
}

async fn handle_refresh_models(args: &RefreshModelsArgs) -> squeezy_core::Result<()> {
    let targets: Vec<OpenAiCompatiblePreset> = if args.providers.is_empty() {
        // Default: refresh every preset whose API-key env var is currently
        // populated. This is what a user running `squeezy refresh-models`
        // without arguments most likely wants — refresh whatever they have
        // credentials for.
        OpenAiCompatiblePreset::all()
            .into_iter()
            .filter(|preset| {
                let env_var = preset.default_api_key_env();
                if env_var.is_empty() {
                    return false;
                }
                env::var(env_var)
                    .map(|value| !value.trim().is_empty())
                    .unwrap_or(false)
            })
            .collect()
    } else {
        let mut out = Vec::with_capacity(args.providers.len());
        for name in &args.providers {
            let preset = OpenAiCompatiblePreset::parse(name).ok_or_else(|| {
                SqueezyError::Config(format!("refresh-models: unknown provider preset {name:?}"))
            })?;
            out.push(preset);
        }
        out
    };

    if targets.is_empty() {
        eprintln!(
            "refresh-models: no providers selected. Set an aggregator API key (e.g. OPENROUTER_API_KEY) or pass --provider <name>."
        );
        return Ok(());
    }

    let mut summaries = Vec::with_capacity(targets.len());
    for preset in targets {
        let env_var = preset.default_api_key_env();
        let api_key = if env_var.is_empty() {
            None
        } else {
            env::var(env_var).ok().filter(|v| !v.trim().is_empty())
        };
        let base_url = match preset.default_base_url() {
            "" => {
                eprintln!(
                    "refresh-models: {} has no fixed base_url; configure providers.{}.base_url and re-run",
                    preset.display_name(),
                    preset.as_str(),
                );
                continue;
            }
            url => url.to_string(),
        };
        match squeezy_llm::model_discovery::refresh(preset.as_str(), &base_url, api_key.as_deref())
            .await
        {
            Ok(catalog) => {
                let count = catalog.models.len();
                if args.json {
                    let body = serde_json::to_string_pretty(&catalog)
                        .map_err(|err| SqueezyError::Config(err.to_string()))?;
                    println!("{body}");
                } else {
                    println!("{}: {} models cached", preset.display_name(), count);
                }
                summaries.push((preset, count));
            }
            Err(err) => {
                eprintln!("{}: refresh failed: {err}", preset.display_name());
            }
        }
    }

    if !args.json && !summaries.is_empty() {
        eprintln!(
            "refresh-models: refreshed {} provider(s); cache lives under ~/.squeezy/cache/models/.",
            summaries.len()
        );
    }
    Ok(())
}

async fn handle_ask_command(args: &AskArgs) -> squeezy_core::Result<()> {
    const ASK_SOCKET_ENV: &str = "SQUEEZY_ASK_SOCKET";
    let socket = env::var_os(ASK_SOCKET_ENV).ok_or_else(|| {
        SqueezyError::Permission(format!(
            "{ASK_SOCKET_ENV} is not set; this command must run inside a Squeezy shell session"
        ))
    })?;
    let endpoint = squeezy_tools::IpcEndpoint::from_env_value(&socket);
    let mut stream = squeezy_tools::IpcStream::connect(&endpoint).await?;
    let request = serde_json::json!({
        "command": args.command,
        "justification": args.justification,
    });
    let request = serde_json::to_string(&request)
        .map_err(|err| SqueezyError::Parse(format!("invalid ask request: {err}")))?;
    stream.write_all(request.as_bytes()).await?;
    stream.shutdown().await?;

    let mut response_bytes = Vec::new();
    stream.read_to_end(&mut response_bytes).await?;
    let response: serde_json::Value = serde_json::from_slice(&response_bytes)
        .map_err(|err| SqueezyError::Parse(format!("invalid ask response: {err}")))?;
    if response
        .get("allow")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        println!("approved");
        Ok(())
    } else {
        let reason = response
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("in-flight permission denied")
            .to_string();
        Err(SqueezyError::Permission(reason))
    }
}

fn mcp_settings_path(config: &AppConfig, scope: &McpConfigScope) -> PathBuf {
    if scope.user {
        default_settings_path()
    } else {
        config.workspace_root.join(PROJECT_SETTINGS_FILE)
    }
}

fn ensure_mcp_servers_table(doc: &mut DocumentMut) -> squeezy_core::Result<&mut Table> {
    let mcp = ensure_table(doc.as_table_mut(), "mcp")?;
    ensure_table(mcp, "servers")
}

fn ensure_table<'a>(table: &'a mut Table, key: &str) -> squeezy_core::Result<&'a mut Table> {
    let item = table
        .entry(key)
        .or_insert_with(|| Item::Table(Table::new()));
    if !item.is_table() {
        return Err(SqueezyError::Config(format!(
            "{key} exists but is not a TOML table"
        )));
    }
    Ok(item.as_table_mut().expect("checked table"))
}

fn validate_mcp_name(name: &str) -> squeezy_core::Result<()> {
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-');
    if valid {
        Ok(())
    } else {
        Err(SqueezyError::Config(
            "MCP server names may contain only ASCII letters, digits, '_' and '-'".to_string(),
        ))
    }
}

fn parse_mcp_transport(value: &str) -> squeezy_core::Result<McpTransport> {
    match value.trim().to_ascii_lowercase().as_str() {
        "stdio" => Ok(McpTransport::Stdio),
        "http" => Ok(McpTransport::Http),
        "sse" => Ok(McpTransport::Sse),
        _ => Err(SqueezyError::Config(format!(
            "invalid MCP transport {value:?}; expected stdio, http, or sse"
        ))),
    }
}

fn parse_permission(value: &str) -> squeezy_core::Result<PermissionMode> {
    PermissionMode::parse(value).ok_or_else(|| {
        SqueezyError::Config(format!(
            "invalid permission mode {value:?}; expected allow, ask, or deny"
        ))
    })
}

fn parse_env_entry(entry: &str) -> squeezy_core::Result<(&str, &str)> {
    let Some((key, value)) = entry.split_once('=') else {
        return Err(SqueezyError::Config(format!(
            "invalid --env entry {entry:?}; expected KEY=VALUE"
        )));
    };
    if key.trim().is_empty() {
        return Err(SqueezyError::Config(
            "invalid --env entry with empty key".to_string(),
        ));
    }
    Ok((key, value))
}

fn mcp_stale_outcome_detail(outcome: &McpStaleOutcome) -> String {
    match outcome {
        McpStaleOutcome::Failed { error } => format!("discovery failed: {error}"),
        McpStaleOutcome::Cancelled => "discovery was cancelled".to_string(),
    }
}

fn handle_repo_command(command: &RepoCommand, cli: &Cli) -> squeezy_core::Result<()> {
    let config = config_from_cli(cli)?;
    match command {
        RepoCommand::Inspect { json } => {
            let loaded = ensure_repo_profile(&config.workspace_root, &config.graph)?;
            if *json {
                let body = serde_json::json!({
                    "status": loaded.status.as_str(),
                    "registry_path": loaded.registry_path,
                    "profile": loaded.profile,
                });
                println!(
                    "{}",
                    serde_json::to_string_pretty(&body).map_err(|err| {
                        SqueezyError::Parse(format!("failed to serialize repo profile: {err}"))
                    })?
                );
            } else {
                println!("{}", loaded.profile.render_human());
                println!(
                    "registry: {} ({})",
                    loaded.registry_path.display(),
                    loaded.status.as_str()
                );
            }
            Ok(())
        }
        RepoCommand::Refresh => {
            let loaded = refresh_repo_profile(&config.workspace_root, &config.graph)?;
            println!("{}", loaded.profile.compact_summary(loaded.status));
            println!("registry: {}", loaded.registry_path.display());
            Ok(())
        }
        RepoCommand::Recommendations => {
            let loaded = ensure_repo_profile(&config.workspace_root, &config.graph)?;
            print!("{}", loaded.profile.recommendations_toml());
            Ok(())
        }
        RepoCommand::Languages { json } => {
            handle_repo_languages(&config.workspace_root, &config.graph, *json)
        }
    }
}

fn crawl_options_from_graph(config: &GraphConfig) -> CrawlOptions {
    CrawlOptions {
        include_hidden: config.include_hidden,
        max_file_bytes: config.max_file_bytes,
        require_indexing_signal: config.require_indexing_signal,
        languages: config.languages.clone(),
        policy: IndexingPolicy {
            include: config.include.clone(),
            exclude: config.exclude.clone(),
            include_classes: config.include_classes.clone(),
            exclude_classes: config.exclude_classes.clone(),
        },
    }
}

fn handle_repo_languages(
    root: &Path,
    graph_config: &GraphConfig,
    json: bool,
) -> squeezy_core::Result<()> {
    use squeezy_core::LanguageKind;

    let snapshot = WorkspaceCrawler::try_new(crawl_options_from_graph(graph_config))?
        .crawl(root)
        .map_err(|e| SqueezyError::Tool(format!("workspace crawl failed: {e}")))?;

    let mut by_language: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_extension: BTreeMap<String, usize> = BTreeMap::new();
    let mut header_heuristic = 0usize;
    let mut unsupported = 0usize;

    for file in &snapshot.files {
        let lang_name = file.language.display_name().to_string();
        *by_language.entry(lang_name).or_default() += 1;

        // Collect the raw (original-casing) extension for the inventory
        let ext = std::path::Path::new(&file.relative_path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_string();
        if !ext.is_empty() {
            *by_extension.entry(ext).or_default() += 1;
        }

        // A plain `.h` or `.H` file classified to C or C++ went through the
        // refine_c_family_header_languages heuristic (or defaulted via project
        // majority). Compare the extension case-insensitively so uppercase
        // filenames (e.g. `HEADER.H`) on Linux are counted correctly.
        let is_h = std::path::Path::new(&file.relative_path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("h"))
            .unwrap_or(false);
        if is_h && matches!(file.language, LanguageKind::C | LanguageKind::Cpp) {
            header_heuristic += 1;
        }
    }
    for uf in &snapshot.unsupported {
        unsupported += 1;
        if let Some(ext) = std::path::Path::new(&uf.relative_path)
            .extension()
            .and_then(|e| e.to_str())
        {
            *by_extension.entry(ext.to_string()).or_default() += 1;
        }
    }

    if json {
        let body = serde_json::json!({
            "root": root.display().to_string(),
            "total_files": snapshot.files.len(),
            "unsupported_files": unsupported,
            "header_heuristic_files": header_heuristic,
            "by_language": by_language,
            "by_extension": by_extension,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&body).unwrap_or_default()
        );
    } else {
        println!("root: {}", root.display());
        println!("total_files: {}", snapshot.files.len());
        println!("unsupported_files: {unsupported}");
        println!(
            "header_heuristic_files: {header_heuristic} \
             (classified via sibling/project-majority, not exact match)"
        );
        println!("by_language:");
        for (lang, count) in &by_language {
            println!("  {lang}: {count}");
        }
        println!("by_extension:");
        for (ext, count) in &by_extension {
            println!("  .{ext}: {count}");
        }
    }
    Ok(())
}

fn handle_parse_command(command: &ParseCommand) -> squeezy_core::Result<()> {
    match command {
        ParseCommand::Smoke { json } => {
            let results = smoke_all_languages();
            let all_ok = results.iter().all(|r| r.ok);
            if *json {
                let body: Vec<_> = results
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "language": format!("{:?}", r.language),
                            "ok": r.ok,
                            "error": r.error,
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "all_ok": all_ok,
                        "results": body,
                    }))
                    .unwrap_or_default()
                );
                if !all_ok {
                    let fail_count = results.iter().filter(|r| !r.ok).count();
                    eprintln!("{fail_count} grammar(s) failed");
                    std::process::exit(1);
                }
            } else {
                for r in &results {
                    let status = if r.ok { "ok  " } else { "FAIL" };
                    let suffix = r
                        .error
                        .as_deref()
                        .map(|e| format!("  {e}"))
                        .unwrap_or_default();
                    println!("[{status}] {:?}{suffix}", r.language);
                }
                if all_ok {
                    println!("all grammars ok");
                } else {
                    let fail_count = results.iter().filter(|r| !r.ok).count();
                    eprintln!("{fail_count} grammar(s) failed");
                    std::process::exit(1);
                }
            }
            Ok(())
        }
    }
}

async fn handle_sessions_command(command: &SessionsCommand, cli: &Cli) -> squeezy_core::Result<()> {
    let config = config_from_cli(cli)?;
    let store = SessionStore::open(&config);
    match command {
        SessionsCommand::List(args) => {
            let sessions = store.list(&session_query_from_args(args)?)?;
            if args.json {
                let body = sessions
                    .iter()
                    .map(session_metadata_for_cli)
                    .collect::<squeezy_core::Result<Vec<_>>>()?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&body).map_err(|err| {
                        SqueezyError::Parse(format!("failed to serialize sessions: {err}"))
                    })?
                );
            } else {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                for session in sessions {
                    let handle = PublicSessionHandle::for_store_id(&session.session_id);
                    let stale_marker = if session.status == SessionStatus::Running
                        && now_ms.saturating_sub(session.started_at_ms)
                            > STALE_RUNNING_SESSION_THRESHOLD_MS
                    {
                        " [stale-running]"
                    } else {
                        ""
                    };
                    println!(
                        "{}\t{}{}\t{}\t{}\t{}\t{}",
                        handle,
                        session.status.as_str(),
                        stale_marker,
                        session.started_at_ms,
                        session.branch.unwrap_or_else(|| "-".to_string()),
                        session.provider,
                        session
                            .first_user_task
                            .or(session.latest_summary)
                            .unwrap_or_default()
                            .replace('\n', " ")
                    );
                }
            }
            Ok(())
        }
        SessionsCommand::Show { id, json } => {
            let resolved = resolve_session_input(&store, id)?;
            let record = store.show(&resolved)?;
            if *json {
                let mut session_id_map = public_session_id_map_for_metadata(&record.metadata);
                let metadata = session_metadata_for_cli(&record.metadata)?;
                let events = session_events_for_cli(&record.events, &mut session_id_map)?;
                let replay = record
                    .replay
                    .map(|tape| session_replay_for_cli_with_mapping(tape, &mut session_id_map))
                    .transpose()?;
                let mut body = serde_json::json!({
                    "metadata": metadata,
                    "events": events,
                    "event_warnings": record.event_warnings,
                    "resume_state": record.resume_state,
                    "attachments": record.attachments,
                    "replay": replay,
                });
                // Discover any session IDs present only in resume_state/attachments
                // before the final sanitization pass.
                add_public_session_id_mappings_from_value(&body, &mut session_id_map);
                sanitize_session_ids_in_value(&mut body, &session_id_map);
                println!(
                    "{}",
                    serde_json::to_string_pretty(&body).map_err(|err| {
                        SqueezyError::Parse(format!("failed to serialize session: {err}"))
                    })?
                );
            } else {
                let handle = PublicSessionHandle::for_store_id(&record.metadata.session_id);
                println!("id={handle}");
                println!("status={}", record.metadata.status.as_str());
                println!("started_at_ms={}", record.metadata.started_at_ms);
                println!(
                    "ended_at_ms={}",
                    format_optional_u64(record.metadata.ended_at_ms)
                );
                println!("cwd={}", record.metadata.cwd);
                println!("workspace_root={}", record.metadata.workspace_root);
                println!(
                    "repo_root={}",
                    record.metadata.repo_root.unwrap_or_else(|| "-".to_string())
                );
                println!(
                    "branch={}",
                    record.metadata.branch.unwrap_or_else(|| "-".to_string())
                );
                println!("provider={}", record.metadata.provider);
                println!("model={}", record.metadata.model);
                println!("mode={}", record.metadata.mode.as_str());
                println!("events={}", record.metadata.event_count);
                println!("event_warnings={}", record.event_warnings);
                if let Some(ref tape) = record.replay {
                    println!("replay_warnings={}", tape.warnings);
                }
                println!("redactions={}", record.metadata.redactions);
                println!("resume_available={}", record.metadata.resume_available);
                if let Some(reason) = record.metadata.resume_unavailable_reason {
                    println!("resume_unavailable_reason={reason}");
                }
                if let Some(task) = record.metadata.first_user_task {
                    println!("first_user_task={}", task.replace('\n', " "));
                }
                if let Some(summary) = record.metadata.latest_summary {
                    println!("latest_summary={}", summary.replace('\n', " "));
                }
            }
            // Warn when a session is still marked running but its last event
            // is old — this suggests the process was killed (SIGKILL, power loss,
            // terminal teardown) before finalization.
            if record.metadata.status == SessionStatus::Running {
                let last_ms = record
                    .metadata
                    .ended_at_ms
                    .unwrap_or(record.metadata.started_at_ms);
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                if now_ms.saturating_sub(last_ms) > STALE_RUNNING_SESSION_THRESHOLD_MS {
                    eprintln!(
                        "warning: session is marked running but last event was >{}h ago; \
                         the process may have been killed before finalization",
                        STALE_RUNNING_SESSION_THRESHOLD_MS / (3600 * 1000)
                    );
                }
            }
            Ok(())
        }
        SessionsCommand::Resume {
            id,
            force_cross_project,
        } => {
            // Accept either the opaque `sess_<16hex>` handle published
            // by `squeezy sessions list` or a raw id / raw-id prefix.
            // Ambiguous and unknown values surface as actionable errors
            // instead of being forwarded as-is into a "session not
            // found" downstream.
            let resolved = resolve_session_input(&store, id)?;
            // F07: when the recorded `metadata.cwd` differs from the
            // caller's current directory, gate the resume behind a y/N
            // prompt so a stray `squeezy sessions resume <id>` from an
            // unrelated checkout cannot silently mutate a session that
            // expected a different repo. `--force-cross-project` on
            // either the top-level CLI or this subcommand bypasses the
            // prompt for scripted callers.
            let force = cli.force_cross_project || *force_cross_project;
            if !confirm_cross_project_resume_stdio(&store, &resolved, force)? {
                println!("resume cancelled");
                return Ok(());
            }
            let provider = provider_from_app_config(&config);
            squeezy_tui::resume(config, provider, resolved).await
        }
        SessionsCommand::Fork { id } => {
            // Offline fork: stamp a child with the parent's resume state
            // on disk, then drop into the TUI resuming the child. The
            // parent session is preserved so `squeezy sessions resume
            // <parent>` still works.
            let resolved = resolve_session_input(&store, id)?;
            let provider = provider_from_app_config(&config);
            let child_metadata = SessionMetadata::new(&config, provider.name());
            let child = store.fork_session(&resolved, child_metadata)?;
            let child_id = child.session_id().to_string();
            drop(child);
            squeezy_tui::resume(config, provider, child_id).await
        }
        SessionsCommand::Replay { id, json } => {
            let resolved = resolve_session_input(&store, id)?;
            let report = Agent::replay_session(config, &resolved).await?;
            if *json {
                let body = session_replay_report_for_cli(&report)?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&body).map_err(|err| {
                        SqueezyError::Tool(format!("failed to serialize replay report: {err}"))
                    })?
                );
            } else {
                println!("turns={}", report.turns);
                println!("events_replayed={}", report.events_replayed);
                println!("requests={}", report.request_count);
                println!("tool_results={}", report.tool_results);
                println!("final_answer={}", report.final_answer.replace('\n', " "));
            }
            Ok(())
        }
        SessionsCommand::Export(args) => handle_session_export_command(&store, args),
        SessionsCommand::Report(args) => handle_session_report_command(args, &config).await,
        SessionsCommand::Cleanup {
            ids,
            archive: _,
            purge,
        } => {
            let mode = if *purge {
                CleanupMode::Purge
            } else {
                CleanupMode::Archive
            };
            let resolved_ids = ids
                .iter()
                .map(|id| resolve_session_input(&store, id))
                .collect::<squeezy_core::Result<Vec<_>>>()?;
            let report = store.cleanup_with(&resolved_ids, None, mode)?;
            for id in report.archived {
                let handle = PublicSessionHandle::for_store_id(&id);
                println!("archived {handle}");
            }
            for id in report.removed {
                let handle = PublicSessionHandle::for_store_id(&id);
                println!("removed {handle}");
            }
            Ok(())
        }
        SessionsCommand::Archive { id } => {
            let resolved = resolve_session_input(&store, id)?;
            store.archive_session(&resolved)?;
            let handle = PublicSessionHandle::for_store_id(&resolved);
            println!("archived {handle}");
            Ok(())
        }
        SessionsCommand::Unarchive { id } => {
            let resolved = resolve_session_input(&store, id)?;
            store.unarchive_session(&resolved)?;
            let handle = PublicSessionHandle::for_store_id(&resolved);
            println!("unarchived {handle}");
            Ok(())
        }
    }
}

async fn handle_feedback_command(args: &FeedbackArgs, cli: &Cli) -> squeezy_core::Result<()> {
    let config = config_from_cli(cli)?;
    let message = feedback_message(args)?;
    let prepared = prepare_feedback(&config, &message, "cli")?;
    println!("feedback preview:");
    println!("{}", prepared.message);
    println!(
        "bytes={} redactions={}",
        prepared.message_bytes, prepared.redactions
    );
    if args.preview {
        return Ok(());
    }
    if !args.yes && !confirm("Send feedback to Squeezy maintainers?")? {
        println!("feedback not sent");
        return Ok(());
    }
    let result = FeedbackClient::from_config(&config)
        .submit_feedback(&prepared)
        .await
        .map_err(|error| SqueezyError::Tool(error.to_string()))?;
    println!("feedback sent: {}", result.id);
    Ok(())
}

fn handle_session_export_command(
    store: &SessionStore,
    args: &SessionExportArgs,
) -> squeezy_core::Result<()> {
    let resolved = resolve_session_input(store, &args.id)?;
    if args.html {
        let record = store.show(&resolved)?;
        let opts = squeezy_agent::ExportOpts {
            include_tool_outputs: !args.no_tool_outputs,
            theme: args.theme.to_theme(),
        };
        let html = squeezy_agent::export_session_to_html(&record, &opts)
            .map_err(|err| SqueezyError::Tool(format!("failed to render session html: {err}")))?;
        // Public handle keeps the default filename safe to share without
        // leaking the raw `<ms>-<pid>-<counter>` id.
        let default_name = format!(
            "squeezy-session-{}.html",
            PublicSessionHandle::for_store_id(&resolved)
        );
        let target = args
            .output
            .clone()
            .unwrap_or_else(|| PathBuf::from(default_name));
        fs::write(&target, &html)?;
        println!("wrote {} ({} bytes)", target.display(), html.len());
        return Ok(());
    }
    let value = store.export(&resolved)?;
    let json = serde_json::to_string_pretty(&value)
        .map_err(|err| SqueezyError::Tool(format!("failed to serialize session export: {err}")))?;
    if let Some(target) = args.output.as_ref() {
        fs::write(target, json.as_bytes())?;
        println!("wrote {} ({} bytes)", target.display(), json.len());
    } else {
        println!("{json}");
    }
    Ok(())
}

async fn handle_session_report_command(
    args: &SessionReportArgs,
    config: &AppConfig,
) -> squeezy_core::Result<()> {
    let excluded_sections = parse_excluded_sections(&args.exclude)?;
    let options = BugReportOptions {
        excluded_sections,
        max_section_bytes: config.session_logs.max_event_bytes,
        max_archive_bytes: config.feedback.max_report_bytes,
    };
    let store = SessionStore::open(config);
    let resolved = resolve_session_input(&store, &args.id)?;
    let bundle = store.build_bug_report(config, &resolved, options)?;
    if args.preview || args.send {
        print!("{}", bundle.preview_text());
    }
    if args.preview && !args.send && args.output.is_none() {
        return Ok(());
    }
    if args.send {
        if !args.yes && !confirm("Upload this redacted report archive to Squeezy maintainers?")? {
            println!("report not sent");
            return Ok(());
        }
        let sections = bundle
            .sections
            .iter()
            .map(|section| section.name.clone())
            .collect::<Vec<_>>();
        match FeedbackClient::from_config(config)
            .submit_report(ReportUpload {
                report_id: &bundle.report_id,
                session_id: &bundle.session_id,
                archive_bytes: &bundle.archive_bytes,
                redactions: bundle.redactions,
                sections,
                source: "cli",
            })
            .await
        {
            Ok(result) => {
                println!("report sent: {}", result.id);
                return Ok(());
            }
            Err(error) => {
                eprintln!("report upload failed: {error}");
                eprintln!("writing local archive instead");
            }
        }
    }
    // Use the public handle for the default filename so the on-disk
    // bug-report artifact also avoids leaking the raw
    // `<ms>-<pid>-<counter>` id. `default_bug_report_path` already
    // sanitizes input for filesystem-safe characters.
    let handle = PublicSessionHandle::for_store_id(&resolved);
    let path = args
        .output
        .clone()
        .unwrap_or_else(|| default_bug_report_path(config, handle.as_ref()));
    bundle.write_archive(&path)?;
    println!("report archive: {}", path.display());
    Ok(())
}

fn feedback_message(args: &FeedbackArgs) -> squeezy_core::Result<String> {
    if !args.message.is_empty() {
        return Ok(args.message.join(" "));
    }
    eprint!("What happened? ");
    io::stderr().flush()?;
    let mut message = String::new();
    io::stdin().read_line(&mut message)?;
    Ok(message)
}

fn confirm(prompt: &str) -> squeezy_core::Result<bool> {
    eprint!("{prompt} [y/N] ");
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Render the cross-project resume prompt body when the recorded
/// session cwd differs from the caller's current cwd; return `None`
/// when the two refer to the same location so callers can skip the
/// confirmation entirely.
///
/// Kept pure (no I/O) so it round-trips cleanly in unit tests and can be
/// reused by any future surface (e.g. TUI) that needs the same warning
/// string. Paths are normalized via [`normalize_cwd_for_compare`] before
/// comparison so that Windows drive-letter case, slash direction, and
/// verbatim/UNC prefixes do not trigger a spurious cross-project prompt.
/// The message itself preserves the original strings so the operator can
/// spot any real discrepancy.
fn cross_project_resume_prompt(session_cwd: &str, current_cwd: &str) -> Option<String> {
    if normalize_cwd_for_compare(session_cwd) == normalize_cwd_for_compare(current_cwd) {
        return None;
    }
    Some(format!(
        "Resume session from {session_cwd} in current cwd {current_cwd}? [y/N] "
    ))
}

/// Normalize a cwd string for equality comparisons across sessions.
///
/// Applies the following transformations so that the same filesystem
/// location expressed in different forms compares equal:
///
/// 1. Strip Windows verbatim (`\\?\`) and UNC (`\\server\share` →
///    `//server/share`) prefixes.
/// 2. Fold backslashes to forward slashes.
/// 3. Fold the drive letter to lowercase (`C:/` → `c:/`).
/// 4. On Windows targets, fold the entire string to lowercase so that
///    directory-component case differences (e.g. `C:\Repo\Sub` vs
///    `C:\repo\sub`) also compare equal — NTFS is case-insensitive by
///    default. On non-Windows targets the rest of the string is left
///    alone because POSIX paths are case-sensitive.
/// 5. Strip a trailing separator.
///
/// Transformations 1–3 run unconditionally on every platform. The shapes
/// they normalize away (verbatim/UNC prefixes, backslashes, drive
/// letters) are essentially never produced by POSIX tools, so applying
/// them on Linux/macOS is harmless and keeps the cross-platform unit
/// tests below honest — a regression in Windows behavior surfaces on
/// the much faster macOS/Linux dev-loop CI rather than only in the
/// 15-minute Windows job.
///
/// Intentionally not covered (silent partial fix):
///
/// - DOS device paths such as `\\.\C:\…` and `\\?\GLOBALROOT\…` are
///   left as-is. These almost never appear in cwd display strings.
/// - Junction-target resolution, short (8.3) vs long-path
///   canonicalization, and substituted-drive resolution. These would
///   require real I/O (`GetFinalPathNameByHandle` and friends); this
///   helper is intentionally pure so it stays cheap and unit-testable.
///
/// The function is cheap and allocation-based. It is called once per
/// `cross_project_resume_prompt` invocation, and in
/// `resolve_resume_session`'s `Continue` arm once per candidate session
/// in the list (plus once for the query cwd). For typical workspaces
/// (a few dozen resumable sessions) this is comfortably below any hot
/// path threshold.
fn normalize_cwd_for_compare(value: &str) -> String {
    let mut s = value.to_string();

    // Strip Windows verbatim prefix \\?\ or //?/
    if s.starts_with(r"\\?\") || s.starts_with("//?/") {
        s = s[4..].to_string();
    }

    // Normalize backslashes to forward slashes for uniform comparison.
    s = s.replace('\\', "/");

    // After stripping verbatim prefix, \\?\UNC\server\share becomes
    // UNC/server/share. Re-normalise to the bare UNC form
    // //server/share so it compares equal to \\server\share (which
    // becomes //server/share directly). Match the "UNC" segment
    // case-insensitively because Windows accepts `\\?\unc\…` and
    // `\\?\UNC\…` as the same prefix.
    if s.len() >= 4 && s.as_bytes()[..4].eq_ignore_ascii_case(b"UNC/") {
        s = format!("//{}", &s[4..]);
    }

    // Fold drive-letter to lowercase: "C:/" → "c:/"
    if s.len() >= 2 && s.as_bytes()[1] == b':' {
        let drive = s.as_bytes()[0].to_ascii_lowercase() as char;
        s = format!("{drive}{}", &s[1..]);
    }

    // On Windows, fold the entire path to lowercase so directory case
    // differences do not falsely trigger the cross-project prompt.
    // Gated to `cfg!(target_os = "windows")` because POSIX paths are
    // case-sensitive (`/home/User` and `/home/user` may name different
    // directories).
    #[cfg(target_os = "windows")]
    {
        s = s.to_ascii_lowercase();
    }

    // Strip trailing separator.
    while s.ends_with('/') && s.len() > 1 {
        s.pop();
    }

    s
}

/// Drive a y/N confirmation through caller-supplied I/O. Returns `true`
/// when the resume should proceed: the cwd already matches, the caller
/// passed `force = true` (matching `--force-cross-project`), or the
/// operator typed `y`/`yes`. Any other input — including end-of-stream
/// — defaults to declining the resume, matching the documented "[y/N]"
/// shape.
fn confirm_cross_project_resume<R, W>(
    session_cwd: &str,
    current_cwd: &str,
    force: bool,
    reader: &mut R,
    writer: &mut W,
) -> io::Result<bool>
where
    R: io::BufRead,
    W: Write,
{
    if force {
        return Ok(true);
    }
    let Some(prompt) = cross_project_resume_prompt(session_cwd, current_cwd) else {
        return Ok(true);
    };
    writer.write_all(prompt.as_bytes())?;
    writer.flush()?;
    let mut answer = String::new();
    reader.read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// `confirm_cross_project_resume` wired to real `stdin`/`stderr`. Looks
/// up the session metadata via [`SessionStore::read_metadata`] so the
/// caller only needs to know the resolved session id, and resolves
/// `current_cwd` through `std::env::current_dir()` (falling back to
/// `"."` if the process has no working directory — same fallback
/// `SessionMetadata::new` uses when recording the original cwd).
///
/// Note: this surface and `resolve_resume_session` source the "current
/// cwd" string from different places — `env::current_dir()` here vs
/// `config.workspace_root.display()` over there. On Windows those can
/// differ in display form (verbatim prefix, drive-letter case, slash
/// direction). Both flows funnel the strings through
/// [`normalize_cwd_for_compare`] before comparison, so the drift is
/// absorbed; do not "tidy" one of them to match the other without
/// preserving the normalization.
fn confirm_cross_project_resume_stdio(
    store: &SessionStore,
    session_id: &str,
    force: bool,
) -> squeezy_core::Result<bool> {
    let metadata = store.read_metadata(session_id)?;
    let current_cwd = env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .display()
        .to_string();
    let stdin = io::stdin();
    let stderr = io::stderr();
    let mut reader = stdin.lock();
    let mut writer = stderr.lock();
    Ok(confirm_cross_project_resume(
        &metadata.cwd,
        &current_cwd,
        force,
        &mut reader,
        &mut writer,
    )?)
}

fn parse_excluded_sections(values: &[String]) -> squeezy_core::Result<BTreeSet<String>> {
    let mut excluded = BTreeSet::new();
    for value in values {
        for part in value.split(',') {
            if part.trim().is_empty() {
                continue;
            }
            let Some(section) = parse_bug_report_section(part) else {
                return Err(SqueezyError::Config(format!(
                    "unknown report section {part:?}"
                )));
            };
            excluded.insert(section.to_string());
        }
    }
    Ok(excluded)
}

struct PreparedRepoProfile {
    visible_summary: Option<String>,
    language_summary: String,
}

fn prepare_repo_profile(config: &mut AppConfig) -> PreparedRepoProfile {
    let loaded = ensure_repo_profile(&config.workspace_root, &config.graph);
    prepare_repo_profile_from_load(config, loaded)
}

fn prepare_repo_profile_from_load(
    config: &mut AppConfig,
    loaded: squeezy_core::Result<RepoProfileLoad>,
) -> PreparedRepoProfile {
    let loaded = match loaded {
        Ok(loaded) => loaded,
        Err(error) => {
            return PreparedRepoProfile {
                visible_summary: Some(format!("Repo profile unavailable: {error}")),
                language_summary: String::new(),
            };
        }
    };
    append_repo_profile_instructions(config, &loaded);
    let language_summary = startup_language_summary(&loaded);
    let visible_summary = loaded
        .status
        .should_show_onboarding()
        .then(|| loaded.profile.compact_summary(loaded.status));
    PreparedRepoProfile {
        visible_summary,
        language_summary,
    }
}

fn startup_language_summary(loaded: &RepoProfileLoad) -> String {
    let mut families = BTreeMap::<String, (String, usize)>::new();
    for language in &loaded.profile.languages {
        if language.files == 0 || language.semantic_support != SemanticSupport::Supported {
            continue;
        }
        let family = language
            .family
            .as_deref()
            .unwrap_or(language.name.as_str())
            .to_string();
        let display = language_family_display(&family, &language.name).to_string();
        let entry = families.entry(family).or_insert((display, 0));
        entry.1 += language.files;
    }
    render_language_summary(families.into_values().collect())
}

fn render_language_summary(mut entries: Vec<(String, usize)>) -> String {
    entries.retain(|(_, files)| *files > 0);
    entries.sort_by(|(left_name, left_files), (right_name, right_files)| {
        right_files
            .cmp(left_files)
            .then_with(|| left_name.cmp(right_name))
    });
    if entries.is_empty() {
        return "none".to_string();
    }
    let mut summary = String::new();
    for (name, files) in entries {
        if !summary.is_empty() {
            summary.push_str(", ");
        }
        let _ = write!(summary, "{name} {files}");
    }
    summary
}

fn language_family_display<'a>(family: &str, fallback: &'a str) -> &'a str {
    match family {
        "rust" => "Rust",
        "python" => "Python",
        "java" => "Java",
        "csharp" => "C#",
        "go" => "Go",
        "c-family" => "C/C++",
        "js-ts" => "JS/TS",
        "ruby" => "Ruby",
        "php" => "PHP",
        "kotlin" => "Kotlin",
        "swift" => "Swift",
        "scala" => "Scala",
        "dart" => "Dart",
        _ => fallback,
    }
}

fn append_repo_profile_instructions(config: &mut AppConfig, loaded: &RepoProfileLoad) {
    config.instructions = format!(
        "{}\n\n{}",
        config.instructions,
        loaded.profile.model_context()
    );
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ModelSelectionState {
    provider: bool,
    model: bool,
    selection_version: Option<u32>,
}

impl ModelSelectionState {
    fn merge(&mut self, next: Self) {
        self.provider |= next.provider;
        self.model |= next.model;
        self.selection_version = self.selection_version.max(next.selection_version);
    }

    fn configured(&self) -> bool {
        self.provider && self.model
    }

    #[cfg(test)]
    fn complete(self) -> bool {
        self.configured() && self.selection_version.unwrap_or(0) >= MODEL_SELECTION_VERSION
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProviderChoice {
    provider: &'static str,
    label: String,
    api_key_env: Option<String>,
    base_url: Option<String>,
    requires_key_setup: bool,
    models: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StartupModelSelection {
    theme: String,
    provider: &'static str,
    model: String,
    api_key_env: Option<String>,
    base_url: Option<String>,
    reasoning_effort: Option<ReasoningEffort>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StartupSetupSelection {
    question_count: usize,
    open_config_section: Option<squeezy_core::config_schema::SectionId>,
}

fn should_run_startup_model_selector(cli: &Cli, config: &AppConfig) -> squeezy_core::Result<bool> {
    if !cli.prompt.is_empty() || cli.list_models || cli.list_providers {
        return Ok(false);
    }
    if !io::stdin().is_terminal() {
        return Ok(false);
    }
    if cli.no_default {
        return Ok(true);
    }
    if cli.provider.is_some()
        || cli.model.is_some()
        || env_var_is_nonempty("SQUEEZY_PROVIDER")
        || env_var_is_nonempty("SQUEEZY_MODEL")
    {
        return Ok(false);
    }
    Ok(!current_model_selection_state(&config.workspace_root)?.configured())
}

fn env_var_is_nonempty(name: &str) -> bool {
    env::var(name)
        .ok()
        .is_some_and(|value| !value.trim().is_empty())
}

fn current_model_selection_state(
    workspace_root: &Path,
) -> squeezy_core::Result<ModelSelectionState> {
    let user_path = default_settings_path();
    let project_path = find_project_settings_path(workspace_root);
    let repo_root = project_path
        .as_deref()
        .and_then(Path::parent)
        .unwrap_or(workspace_root);
    let repo_path = per_repo_settings_path(repo_root);
    model_selection_state_from_paths(&user_path, project_path.as_deref(), &repo_path)
}

fn model_selection_state_from_paths(
    user_path: &Path,
    project_path: Option<&Path>,
    repo_path: &Path,
) -> squeezy_core::Result<ModelSelectionState> {
    let mut state = ModelSelectionState::default();
    state.merge(model_selection_state_from_settings(user_path)?);
    if let Some(path) = project_path {
        state.merge(model_selection_state_from_settings(path)?);
    }
    state.merge(model_selection_state_from_settings(repo_path)?);
    Ok(state)
}

fn startup_resume_question_available(cli: &Cli, config: &AppConfig) -> bool {
    // The picker is opt-in, so bare launches never offer a resume question and
    // never pay the candidate scan; only `--resume` reaches the on-disk lookup.
    if !cli.resume || cli.no_resume_picker || cli.session.is_some() || cli.continue_session {
        return false;
    }
    squeezy_tui::startup_resume_question_available(config)
}

fn model_selection_state_from_settings(path: &Path) -> squeezy_core::Result<ModelSelectionState> {
    let settings = SettingsFile::load_optional(path)?;
    Ok(model_selection_state(&settings))
}

fn model_selection_state(settings: &SettingsFile) -> ModelSelectionState {
    let provider = settings
        .model_settings
        .as_ref()
        .and_then(|settings| settings.provider.as_ref())
        .or(settings.provider.as_ref())
        .is_some_and(|value| !value.trim().is_empty());
    let model = settings
        .model_settings
        .as_ref()
        .and_then(|settings| settings.model.as_ref())
        .or(settings.model.as_ref())
        .is_some_and(|value| !value.trim().is_empty());
    let selection_version = settings
        .model_settings
        .as_ref()
        .and_then(|settings| settings.selection_version);
    ModelSelectionState {
        provider,
        model,
        selection_version,
    }
}

async fn run_startup_model_selector(
    config: &AppConfig,
    trailing_question_count: usize,
    terminal: Option<&mut squeezy_tui::StartupTerminal>,
) -> squeezy_core::Result<Option<StartupSetupSelection>> {
    let settings_path = default_settings_path();
    let choices = detect_provider_choices(config).await;
    if choices.is_empty() {
        return Err(SqueezyError::Config(
            "no startup provider choices are available; open /config after startup to configure a custom provider"
                .to_string(),
        ));
    }

    let picker_choices = choices
        .iter()
        .map(|choice| squeezy_tui::StartupModelPickerProvider {
            label: choice.label.clone(),
            credential: if choice.requires_key_setup {
                squeezy_tui::StartupProviderCredential::NeedsConfig {
                    env_var: choice
                        .api_key_env
                        .clone()
                        .unwrap_or_else(|| "API key".to_string()),
                }
            } else if choice.api_key_env.is_some() {
                squeezy_tui::StartupProviderCredential::Configured
            } else {
                squeezy_tui::StartupProviderCredential::NotRequired
            },
            models: choice
                .models
                .iter()
                .map(|model| {
                    let model_id = parse_model_choice_id(model);
                    squeezy_tui::StartupModelPickerModel {
                        label: model.clone(),
                        reasoning_effort: capabilities_for(choice.provider, &model_id)
                            .is_some_and(|capabilities| capabilities.reasoning_effort),
                    }
                })
                .collect(),
        })
        .collect::<Vec<_>>();
    let result = if let Some(terminal) = terminal {
        squeezy_tui::pick_startup_model_selection_in_terminal(
            terminal,
            config,
            &settings_path,
            picker_choices,
            trailing_question_count,
        )?
    } else {
        squeezy_tui::pick_startup_model_selection(
            config,
            &settings_path,
            picker_choices,
            trailing_question_count,
        )?
    };
    let Some(result) = result else {
        return Ok(None);
    };
    let squeezy_tui::StartupModelPickerResult::Selected(picked) = result;
    let question_count =
        3 + usize::from(picked.open_model_config) + usize::from(picked.reasoning_effort.is_some());
    let provider = choices
        .get(picked.provider_index)
        .ok_or_else(|| SqueezyError::Config("startup provider selection out of range".into()))?;
    let model_choice = provider
        .models
        .get(picked.model_index)
        .ok_or_else(|| SqueezyError::Config("startup model selection out of range".into()))?;
    let selection = StartupModelSelection {
        theme: picked.theme,
        provider: provider.provider,
        model: parse_model_choice_id(model_choice),
        api_key_env: provider.api_key_env.clone(),
        base_url: provider.base_url.clone(),
        reasoning_effort: picked.reasoning_effort,
    };
    save_startup_model_selection(&settings_path, &selection)?;
    Ok(Some(StartupSetupSelection {
        question_count,
        open_config_section: if picked.open_theme_config {
            Some(squeezy_core::config_schema::SectionId::Themes)
        } else {
            picked
                .open_model_config
                .then_some(squeezy_core::config_schema::SectionId::Models)
        },
    }))
}

async fn detect_provider_choices(_config: &AppConfig) -> Vec<ProviderChoice> {
    let mut choices = vec![
        hosted_provider_choice(
            "openai",
            "OpenAI",
            preferred_env_name("OPENAI_API_KEY_ENV", &["OPENAI_API_KEY"]),
            None,
        ),
        hosted_provider_choice(
            "anthropic",
            "Anthropic",
            preferred_env_name("ANTHROPIC_API_KEY_ENV", &["ANTHROPIC_API_KEY"]),
            None,
        ),
        hosted_provider_choice(
            "google",
            "Gemini",
            preferred_env_name("GOOGLE_API_KEY_ENV", &["GEMINI_API_KEY", "GOOGLE_API_KEY"]),
            None,
        ),
        hosted_provider_choice(
            "azure_openai",
            "Azure OpenAI",
            preferred_env_name("AZURE_OPENAI_API_KEY_ENV", &["AZURE_OPENAI_API_KEY"]),
            env::var("AZURE_OPENAI_BASE_URL")
                .ok()
                .filter(|value| !value.trim().is_empty()),
        ),
    ];

    // Aggregators and OpenAI-compatible hosts stay visible even before a key is
    // exported. Static registry entries make model selection deterministic; a
    // live catalog refresh only runs when the key is actually available.
    for preset in OpenAiCompatiblePreset::all() {
        if matches!(preset, OpenAiCompatiblePreset::Custom) {
            continue;
        }
        let env_var = preset.default_api_key_env();
        if env_var.is_empty() {
            continue;
        }
        let base_url = env::var(format!("{}_BASE_URL", preset.as_str().to_ascii_uppercase()))
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| preset.default_base_url().to_string());
        choices.push(compatible_provider_choice(
            preset,
            env_var.to_string(),
            base_url,
        ));
    }

    let ollama_base_url = env::var("OLLAMA_BASE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_OLLAMA_BASE_URL.to_string());
    let ollama_models = fetch_ollama_model_names(&ollama_base_url).await;
    let mut models = ollama_models;
    models.sort_by_key(|model| {
        if model.starts_with(squeezy_core::DEFAULT_OLLAMA_MODEL) {
            0
        } else {
            1
        }
    });
    if models.is_empty() {
        models.push(format!(
            "{} (local default)",
            squeezy_core::DEFAULT_OLLAMA_MODEL
        ));
    }
    choices.push(ProviderChoice {
        provider: "ollama",
        label: format!("Ollama local ({ollama_base_url})"),
        api_key_env: None,
        base_url: Some(ollama_base_url),
        requires_key_setup: false,
        models,
    });

    choices.retain(|choice| !choice.models.is_empty());
    choices
}

fn compatible_provider_choice(
    preset: OpenAiCompatiblePreset,
    api_key_env: String,
    base_url: String,
) -> ProviderChoice {
    let mut curated: Vec<String> = models_for_provider(preset.as_str())
        .map(model_choice_label)
        .collect();
    let curated_ids: std::collections::BTreeSet<String> = models_for_provider(preset.as_str())
        .map(|m| m.id.to_string())
        .collect();
    // Merge cached live-discovered models into the picker so the user sees the
    // current catalog without waiting for a release. The cache is populated by
    // a previous run or by `squeezy refresh-models`; if it's stale or missing
    // we kick off a background refresh so the next run benefits.
    let cached = squeezy_llm::model_discovery::read_cached(preset.as_str());
    let needs_refresh = cached.as_ref().map(|c| !c.is_fresh()).unwrap_or(true);
    if let Some(catalog) = &cached {
        for model in &catalog.models {
            if curated_ids.contains(&model.id) {
                continue;
            }
            curated.push(discovered_model_label(model));
        }
    }
    let configured = env_var_is_nonempty(&api_key_env);
    if configured && needs_refresh {
        spawn_background_refresh(
            preset.as_str().to_string(),
            base_url.clone(),
            api_key_env.clone(),
        );
    }
    let mut models = curated;
    if models.is_empty() {
        let default_model = preset.default_model();
        if !default_model.is_empty() {
            models.push(format!("{default_model} (balanced, vendor default)"));
        }
    }
    if models.is_empty() {
        // PortKey + Custom can't ship a sensible default model id since both
        // expect the user to configure their own virtual key or self-host
        // endpoint. Offer a placeholder choice so the picker still shows
        // the provider; users will edit settings.toml afterwards.
        models.push("(set providers.<name>.default_model in settings.toml)".to_string());
    }
    ProviderChoice {
        provider: preset.as_str(),
        label: format!(
            "{} ({})",
            preset.display_name(),
            credential_label(&api_key_env, configured)
        ),
        api_key_env: Some(api_key_env),
        base_url: Some(base_url),
        requires_key_setup: !configured,
        models,
    }
}

fn discovered_model_label(model: &squeezy_llm::model_discovery::DiscoveredModel) -> String {
    let price = match (
        model.pricing_input_usd_micros_per_mtok,
        model.pricing_output_usd_micros_per_mtok,
    ) {
        (Some(input), Some(output)) => format!(
            "${:.3}/M in, ${:.3}/M out",
            input as f64 / 1_000_000.0,
            output as f64 / 1_000_000.0,
        ),
        _ => "live catalog".to_string(),
    };
    let context = model
        .context_length
        .map(|n| format!(", context {}K", n / 1024))
        .unwrap_or_default();
    format!("{} (discovered, {price}{context})", model.id)
}

fn spawn_background_refresh(provider: String, base_url: String, api_key_env: String) {
    let Some(api_key_value) = env::var(&api_key_env).ok() else {
        return;
    };
    if api_key_value.trim().is_empty() {
        return;
    }
    tokio::spawn(async move {
        let _ = squeezy_llm::model_discovery::refresh(
            &provider,
            &base_url,
            Some(api_key_value.as_str()),
        )
        .await;
    });
}

fn preferred_env_name(selector_env: &str, defaults: &[&str]) -> String {
    if let Some(name) = env::var(selector_env)
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        return name;
    }
    defaults.first().copied().unwrap_or("API_KEY").to_string()
}

fn credential_label(api_key_env: &str, configured: bool) -> String {
    if configured {
        format!("key from {api_key_env}")
    } else {
        format!("add {api_key_env} in /config")
    }
}

fn hosted_provider_choice(
    provider: &'static str,
    label: &str,
    api_key_env: String,
    base_url: Option<String>,
) -> ProviderChoice {
    let configured = env_var_is_nonempty(&api_key_env);
    ProviderChoice {
        provider,
        label: format!("{label} ({})", credential_label(&api_key_env, configured)),
        api_key_env: Some(api_key_env),
        base_url,
        requires_key_setup: !configured,
        models: models_for_provider(provider)
            .map(model_choice_label)
            .collect::<Vec<_>>(),
    }
}

fn model_choice_label(model: &ModelInfo) -> String {
    let price = model
        .pricing
        .map(|pricing| {
            format!(
                "${:.3}/M in, ${:.3}/M out",
                pricing.input_usd_micros_per_mtok as f64 / 1_000_000.0,
                pricing.output_usd_micros_per_mtok as f64 / 1_000_000.0
            )
        })
        .unwrap_or_else(|| "local/unknown price".to_string());
    format!("{} ({}, {price})", model.id, model.profile.as_str())
}

fn parse_model_choice_id(choice: &str) -> String {
    choice
        .split_once(" (")
        .map(|(id, _)| id)
        .unwrap_or(choice)
        .to_string()
}

fn save_startup_model_selection(
    path: &Path,
    selection: &StartupModelSelection,
) -> squeezy_core::Result<()> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    let mut doc = text
        .parse::<DocumentMut>()
        .map_err(|err| SqueezyError::Config(format!("{}: {err}", path.display())))?;

    let tui = ensure_doc_table(&mut doc, "tui")?;
    tui.insert(
        "theme",
        Item::Value(TomlValue::from(selection.theme.as_str())),
    );

    let model = ensure_doc_table(&mut doc, "model")?;
    model.insert("provider", Item::Value(TomlValue::from(selection.provider)));
    model.insert(
        "model",
        Item::Value(TomlValue::from(selection.model.as_str())),
    );
    model.insert(
        "selection_version",
        Item::Value(TomlValue::from(i64::from(MODEL_SELECTION_VERSION))),
    );
    if let Some(reasoning_effort) = selection.reasoning_effort {
        model.insert(
            "reasoning_effort",
            Item::Value(TomlValue::from(reasoning_effort.as_str())),
        );
    } else {
        model.remove("reasoning_effort");
    }

    if selection.api_key_env.is_some() || selection.base_url.is_some() {
        let providers = ensure_doc_table(&mut doc, "providers")?;
        let provider = ensure_table(providers, selection.provider)?;
        if let Some(api_key_env) = &selection.api_key_env {
            provider.insert(
                "api_key_env",
                Item::Value(TomlValue::from(api_key_env.as_str())),
            );
        }
        if let Some(base_url) = &selection.base_url {
            provider.insert("base_url", Item::Value(TomlValue::from(base_url.as_str())));
        }
    }

    // Use the hardened atomic writer: the startup selection file lives under
    // ~/.squeezy/ and may contain inline api_key_env / base_url values that
    // act as credentials, so owner-only permissions are required.
    write_settings_atomic(path, doc.to_string().as_bytes())?;
    Ok(())
}

fn ensure_doc_table<'a>(
    doc: &'a mut DocumentMut,
    key: &str,
) -> squeezy_core::Result<&'a mut Table> {
    if !doc.as_table().contains_key(key) {
        doc[key] = Item::Table(Table::new());
    }
    doc[key]
        .as_table_mut()
        .ok_or_else(|| SqueezyError::Config(format!("{key} is not a table")))
}

fn session_query_from_args(args: &SessionListArgs) -> squeezy_core::Result<SessionQuery> {
    Ok(SessionQuery {
        since_ms: args.since,
        until_ms: args.until,
        cwd: args.cwd.clone(),
        repo: args.repo.clone(),
        branch: args.branch.clone(),
        provider: args.provider.clone(),
        model: args.model.clone(),
        status: args
            .status
            .as_deref()
            .map(parse_session_status)
            .transpose()?,
        query: args.query.clone(),
        include_archived: args.include_archived,
    })
}

/// What the `--continue` / `--session` pair (or neither) requests for
/// startup. `Continue` resolves to the most-recent resumable session in
/// `cwd_str`; `Explicit` is taken at face value and downstream
/// errors-out if the id is unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResumeFlag<'a> {
    None,
    Continue,
    Explicit(&'a str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResumeResolution {
    /// `Some(id)` requests resuming that session; `None` means start
    /// fresh.
    session_id: Option<String>,
    /// Human-readable note to print on stderr (e.g. fallback warning).
    note: Option<String>,
}

/// Pure: pick the session id to resume given the parsed flags, a
/// snapshot of `SessionStore::list(&SessionQuery::default())`, and the
/// caller's cwd. `sessions` is expected to be sorted newest-first by
/// `started_at_ms`, which is what `SessionStore::list` already
/// guarantees.
fn resolve_resume_session(
    flag: ResumeFlag<'_>,
    sessions: &[SessionMetadata],
    cwd_str: &str,
) -> ResumeResolution {
    match flag {
        ResumeFlag::None => ResumeResolution {
            session_id: None,
            note: None,
        },
        ResumeFlag::Explicit(id) => ResumeResolution {
            session_id: Some(id.to_string()),
            note: None,
        },
        ResumeFlag::Continue => {
            let cwd_norm = normalize_cwd_for_compare(cwd_str);
            let found = sessions.iter().find(|meta| {
                meta.resume_available && normalize_cwd_for_compare(&meta.cwd) == cwd_norm
            });
            if let Some(meta) = found {
                // Emit a hint when the match was via path normalization (e.g.
                // drive-letter case on Windows) so the user can see which
                // recorded path was resolved.
                let note = if meta.cwd != cwd_str {
                    Some(format!(
                        "squeezy: --continue: resuming session recorded at {} \
                         (matched current directory via path normalization)",
                        meta.cwd
                    ))
                } else {
                    None
                };
                ResumeResolution {
                    session_id: Some(meta.session_id.clone()),
                    note,
                }
            } else {
                ResumeResolution {
                    session_id: None,
                    note: Some(
                        "squeezy: --continue: no resumable session found for this directory; starting fresh"
                            .to_string(),
                    ),
                }
            }
        }
    }
}

fn parse_session_status(value: &str) -> squeezy_core::Result<SessionStatus> {
    match value.trim().to_ascii_lowercase().as_str() {
        "running" => Ok(SessionStatus::Running),
        "archived" => Ok(SessionStatus::Archived),
        "completed" => Ok(SessionStatus::Completed),
        "cancelled" | "canceled" => Ok(SessionStatus::Cancelled),
        "failed" => Ok(SessionStatus::Failed),
        "truncated" => Ok(SessionStatus::Truncated),
        _ => Err(SqueezyError::Config(format!(
            "invalid session status {value:?}; expected running, archived, completed, cancelled, failed, or truncated"
        ))),
    }
}

/// Opaque, stable pseudonym derived from a raw `<ms>-<pid>-<counter>`
/// session id. CLI output uses this form everywhere a session id would
/// otherwise be logged (CodeQL's "cleartext logging of sensitive
/// information" heuristic flags the raw shape), and
/// [`resolve_session_input`] reverses it when the user passes a
/// `sess_…` value back in.
///
/// **Wire format:** `sess_<16 lowercase hex>` (8 bytes / 64 bits of the
/// SHA-256 digest). 64 bits is astronomically collision-resistant for a
/// local single-user session store — even at a few hundred thousand
/// sessions the expected collision count stays well below 1 — and the
/// `v1` tag in the domain-separation key (`squeezy-public-session-id-v1\0`)
/// reserves room to lengthen the digest without changing the prefix if
/// future use lengthens the address space.
///
/// **Hash choice:** the cryptographic strength of SHA-256 is not load-
/// bearing here (this is a pseudonym, not credential material). A
/// cheaper hash with the same 64-bit pre-image property would be a
/// drop-in replacement; the only consumer is this file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PublicSessionHandle(String);

impl PublicSessionHandle {
    fn for_store_id(value: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"squeezy-public-session-id-v1\0");
        hasher.update(value.as_bytes());
        let digest = hasher.finalize();
        let mut out = String::from("sess_");
        for byte in &digest[..8] {
            let _ = write!(&mut out, "{byte:02x}");
        }
        Self(out)
    }
}

impl AsRef<str> for PublicSessionHandle {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PublicSessionHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for PublicSessionHandle {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

fn session_metadata_for_cli(metadata: &SessionMetadata) -> squeezy_core::Result<serde_json::Value> {
    let session_id_map = public_session_id_map_for_metadata(metadata);
    let mut value = serde_json::to_value(metadata).map_err(|err| {
        SqueezyError::Parse(format!("failed to serialize session metadata: {err}"))
    })?;
    sanitize_session_ids_in_value(&mut value, &session_id_map);
    let Some(object) = value.as_object_mut() else {
        return Err(SqueezyError::Parse(
            "session metadata did not serialize to an object".to_string(),
        ));
    };
    object.remove("session_id");
    let public_id = session_id_map
        .get(&metadata.session_id)
        .cloned()
        .unwrap_or_else(|| PublicSessionHandle::for_store_id(&metadata.session_id).0);
    object.insert("id".to_string(), serde_json::Value::String(public_id));
    Ok(value)
}

#[cfg(test)]
fn session_replay_for_cli(tape: SessionReplayTape) -> squeezy_core::Result<serde_json::Value> {
    let mut session_id_map = BTreeMap::new();
    add_public_session_id_mapping(&mut session_id_map, &tape.session_id);
    session_replay_for_cli_with_mapping(tape, &mut session_id_map)
}

fn session_replay_for_cli_with_mapping(
    tape: SessionReplayTape,
    session_id_map: &mut BTreeMap<String, String>,
) -> squeezy_core::Result<serde_json::Value> {
    let mut events = serde_json::to_value(tape.events).map_err(|err| {
        SqueezyError::Parse(format!("failed to serialize session replay events: {err}"))
    })?;
    add_public_session_id_mappings_from_value(&events, session_id_map);
    sanitize_session_ids_in_value(&mut events, session_id_map);
    Ok(serde_json::json!({
        "schema_version": tape.schema_version,
        "id": PublicSessionHandle::for_store_id(&tape.session_id),
        "events": events,
        "warnings": tape.warnings,
    }))
}

fn session_replay_report_for_cli(
    report: &SessionReplayReport,
) -> squeezy_core::Result<serde_json::Value> {
    let mut value = serde_json::to_value(report)
        .map_err(|err| SqueezyError::Parse(format!("failed to serialize replay report: {err}")))?;
    let mut session_id_map = BTreeMap::new();
    add_public_session_id_mapping(&mut session_id_map, &report.session_id);
    sanitize_session_ids_in_value(&mut value, &session_id_map);
    let Some(object) = value.as_object_mut() else {
        return Err(SqueezyError::Parse(
            "replay report did not serialize to an object".to_string(),
        ));
    };
    object.remove("session_id");
    let public_id = session_id_map
        .get(&report.session_id)
        .cloned()
        .unwrap_or_else(|| PublicSessionHandle::for_store_id(&report.session_id).0);
    object.insert("id".to_string(), serde_json::Value::String(public_id));
    Ok(value)
}

fn session_events_for_cli(
    events: &[SessionEvent],
    session_id_map: &mut BTreeMap<String, String>,
) -> squeezy_core::Result<serde_json::Value> {
    let mut value = serde_json::to_value(events)
        .map_err(|err| SqueezyError::Parse(format!("failed to serialize session events: {err}")))?;
    add_public_session_id_mappings_from_value(&value, session_id_map);
    sanitize_session_ids_in_value(&mut value, session_id_map);
    Ok(value)
}

fn public_session_id_map_for_metadata(metadata: &SessionMetadata) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    add_public_session_id_mapping(&mut map, &metadata.session_id);
    if let Some(parent_id) = metadata.parent_id.as_deref() {
        add_public_session_id_mapping(&mut map, parent_id);
    }
    map
}

fn add_public_session_id_mapping(map: &mut BTreeMap<String, String>, session_id: &str) {
    if session_id.is_empty() {
        return;
    }
    map.insert(
        session_id.to_string(),
        PublicSessionHandle::for_store_id(session_id)
            .as_ref()
            .to_string(),
    );
}

fn add_public_session_id_mappings_from_value(
    value: &serde_json::Value,
    session_id_map: &mut BTreeMap<String, String>,
) {
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                add_public_session_id_mappings_from_value(item, session_id_map);
            }
        }
        serde_json::Value::Object(object) => {
            for (key, item) in object {
                if is_session_id_json_key(key)
                    && let Some(session_id) = item.as_str()
                {
                    add_public_session_id_mapping(session_id_map, session_id);
                }
                add_public_session_id_mappings_from_value(item, session_id_map);
            }
        }
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => {}
    }
}

fn is_session_id_json_key(key: &str) -> bool {
    key == "session_id"
        || key == "parent_id"
        || key.ends_with("_session_id")
        || key == "cleared_from"
}

fn sanitize_session_ids_in_value(
    value: &mut serde_json::Value,
    session_id_map: &BTreeMap<String, String>,
) {
    let mut replacements: Vec<(&String, &String)> = session_id_map.iter().collect();
    // Longest raw id first so a shorter id that is a substring of a
    // longer one cannot clobber the longer one's match.
    replacements.sort_by_key(|(raw, _)| std::cmp::Reverse(raw.len()));
    sanitize_session_ids_in_value_inner(value, &replacements);
}

fn sanitize_session_ids_in_value_inner(
    value: &mut serde_json::Value,
    replacements: &[(&String, &String)],
) {
    match value {
        serde_json::Value::String(text) => {
            for (raw, public) in replacements {
                if raw != public {
                    *text = text.replace(raw.as_str(), public.as_str());
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                sanitize_session_ids_in_value_inner(item, replacements);
            }
        }
        serde_json::Value::Object(object) => {
            for item in object.values_mut() {
                sanitize_session_ids_in_value_inner(item, replacements);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

/// Translate a user-supplied session identifier into a raw on-disk
/// `<ms>-<pid>-<counter>` session id that the store layer accepts.
///
/// CLI output exposes only the opaque [`PublicSessionHandle`] form
/// (`sess_<16hex>`) so the raw id is never written to a log surface that
/// CodeQL flags as cleartext-sensitive, but the inputs the user actually
/// types still have to round-trip back to a real session — otherwise
/// `sessions list | xargs sessions resume` and friends silently break.
/// This resolver is the single source of truth for that translation and
/// is shared by every session-id-consuming subcommand and by
/// `--session <id>` at startup.
///
/// Resolution order (first match wins):
///
/// 1. **Exact raw id**: if the input names an existing session
///    directory (live or archived) directly, return it unchanged. This
///    is the cheap happy path and also keeps backwards compatibility
///    with scripts that already capture raw ids from `events.jsonl`
///    or `.squeezy/sessions/<id>/` directly.
/// 2. **Public handle**: if the input looks like a [`PublicSessionHandle`]
///    (`sess_<16hex>`) we linear-scan the live and archived sessions and
///    return the underlying raw id whose handle matches. The scan
///    excludes archived sessions when the live root already produced a
///    match, but this code path is dominated by the disk read so
///    excluding archived first would not be a meaningful speedup.
/// 3. **Prefix**: otherwise we hand the input to
///    [`SessionStore::resolve_session_id_prefix`], which handles
///    raw-id prefix matching with its own ambiguity diagnostics.
///
/// The function is intentionally infallible by default for valid public
/// handles: callers should not need to know whether the user typed the
/// public form or the raw form. For the prefix path it surfaces the
/// store's ambiguity / not-found errors verbatim, prefixed by the
/// caller's context where applicable.
fn resolve_session_input(store: &SessionStore, input: &str) -> squeezy_core::Result<String> {
    if input.is_empty() {
        return Err(SqueezyError::Tool(
            "session id is required; pass an id from `squeezy sessions list`".to_string(),
        ));
    }

    if store.read_metadata(input).is_ok() {
        return Ok(input.to_string());
    }

    if looks_like_public_session_handle(input) {
        let query = SessionQuery {
            include_archived: true,
            ..Default::default()
        };
        for metadata in store.list(&query)? {
            if PublicSessionHandle::for_store_id(&metadata.session_id).0 == input {
                return Ok(metadata.session_id);
            }
        }
        return Err(SqueezyError::Tool(format!(
            "no session found for handle {input:?}; the handle is published by `squeezy sessions list`"
        )));
    }

    store
        .resolve_session_id_prefix(input)
        .map_err(|err| SqueezyError::Tool(err.to_string()))
}

/// Return true when `value` matches the [`PublicSessionHandle`] shape
/// (`sess_<16hex>`). Used by [`resolve_session_input`] to decide whether
/// to scan the on-disk session list or treat the value as a raw prefix.
/// The check is intentionally cheap; mismatches fall through to prefix
/// resolution rather than erroring out.
fn looks_like_public_session_handle(value: &str) -> bool {
    let Some(suffix) = value.strip_prefix("sess_") else {
        return false;
    };
    suffix.len() == 16 && suffix.bytes().all(|b| b.is_ascii_hexdigit())
}

fn format_optional_u64(value: Option<u64>) -> String {
    value.map_or_else(|| "-".to_string(), |value| value.to_string())
}

async fn run_prompts(
    config: AppConfig,
    provider: Arc<dyn LlmProvider>,
    prompts: Vec<print_mode::PromptInput>,
    format: PromptFormat,
    permission_mode: PromptPermissionMode,
    resume_session_id: Option<String>,
    telemetry: TelemetryClient,
) -> squeezy_core::Result<()> {
    // Print mode used to skip the agent loop entirely and stream the
    // provider response with `tools: []`, which meant the model couldn't
    // call `read_file`, `apply_patch`, `bash`, MCP, or anything else. CI
    // and scripted callers therefore got an LLM-only single-shot — not
    // the agent they expected from `--prompt`. Routing through
    // `Agent::new` / `Agent::resume` mirrors what the TUI does, so the
    // same tool registry (semantic graph tools, file ops, shell, MCP,
    // skills) is available; session persistence and redaction now live
    // inside the agent and don't need to be re-implemented here.
    let agent = if let Some(id) = resume_session_id {
        Agent::resume_with_telemetry(config, provider, &id, telemetry)?.0
    } else {
        Agent::new_with_telemetry(config, provider, telemetry)
    };
    let stdout = io::stdout();
    let stderr = io::stderr();
    let mut stdout = stdout.lock();
    let mut stderr = stderr.lock();
    let result = pump_prompts(
        &agent,
        prompts,
        format,
        permission_mode,
        &mut stdout,
        &mut stderr,
    )
    .await;
    let exit_status = if result.is_ok() {
        SessionStatus::Completed
    } else {
        SessionStatus::Failed
    };
    agent.finish_session(exit_status).await;
    // Flush the agent's telemetry so the session summary is persisted and
    // sent before the process exits. Without this the headless path may
    // exit before the background flush timer fires.
    agent.flush_telemetry().await;
    result
}

/// Walk the resolved print-mode prompts and drive each one through the
/// agent, honoring the per-prompt `exclude_from_context` flag on the way.
/// Extracted from `run_prompts` so `main_tests.rs` can exercise the `!!`
/// local-shell semantic against a scripted provider without paying
/// the cost of building a real session log root.
async fn pump_prompts<O, E>(
    agent: &Agent,
    prompts: Vec<print_mode::PromptInput>,
    format: PromptFormat,
    permission_mode: PromptPermissionMode,
    stdout: &mut O,
    stderr: &mut E,
) -> squeezy_core::Result<()>
where
    O: Write,
    E: Write,
{
    for prompt in prompts {
        let input = if prompt.exclude_from_context {
            format!("!!{}", prompt.content)
        } else {
            prompt.content
        };
        let rx = agent.start_turn(input, CancellationToken::new());
        pump_prompt_events(rx, format, permission_mode, stdout, stderr).await?;
    }
    Ok(())
}

/// Drive a single `Agent::start_turn` mpsc receiver to completion and
/// surface the relevant events on `stdout`/`stderr`. Extracted so
/// `main_tests.rs` can exercise the end-to-end print-mode wiring with
/// captured writers and a scripted provider — verifying that print mode
/// actually runs tools end-to-end.
async fn pump_prompt_events<O, E>(
    mut rx: tokio::sync::mpsc::Receiver<AgentEvent>,
    format: PromptFormat,
    permission_mode: PromptPermissionMode,
    stdout: &mut O,
    stderr: &mut E,
) -> squeezy_core::Result<()>
where
    O: Write,
    E: Write,
{
    let mut result: squeezy_core::Result<()> = Ok(());
    let mut completed = false;
    let mut wrote_text_delta = false;

    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::Started { .. } if format == PromptFormat::Json => {
                emit_prompt_event(stdout, &PromptWireEvent::Started)?;
            }
            AgentEvent::Started { .. } => {}
            AgentEvent::AssistantDelta { delta, .. } => match format {
                PromptFormat::Default => {
                    write!(stdout, "{delta}")?;
                    stdout.flush()?;
                    wrote_text_delta = true;
                }
                PromptFormat::Json => {
                    emit_prompt_event(stdout, &PromptWireEvent::TextDelta(delta))?;
                }
            },
            AgentEvent::ReasoningDelta { delta, .. } if format == PromptFormat::Json => {
                emit_prompt_event(stdout, &PromptWireEvent::ReasoningDelta(delta))?;
            }
            AgentEvent::ReasoningDelta { .. } => {}
            AgentEvent::ToolCallStarted { call, .. } => match format {
                PromptFormat::Default => {
                    let label = squeezy_tools::human_label_for_call(&call.name, &call.arguments);
                    writeln!(stderr, "tool: {} {label}", call.name)?;
                    stderr.flush()?;
                }
                PromptFormat::Json => {
                    emit_prompt_event(stdout, &PromptWireEvent::ToolCallStarted(call))?;
                }
            },
            AgentEvent::ToolCallCompleted {
                result: tool_result,
                ..
            } => match format {
                PromptFormat::Default => {
                    writeln!(
                        stderr,
                        "tool: {} -> {}",
                        tool_result.tool_name,
                        tool_status_label(tool_result.status),
                    )?;
                    stderr.flush()?;
                }
                PromptFormat::Json => {
                    emit_prompt_event(stdout, &PromptWireEvent::ToolCallCompleted(tool_result))?;
                }
            },
            AgentEvent::ApprovalRequested {
                request,
                decision_tx,
                ..
            } => {
                let tool_name = request.tool_name.clone();
                let reason = request.reason.clone();
                match permission_mode {
                    PromptPermissionMode::AutoApprove => {
                        match format {
                            PromptFormat::Default => {
                                writeln!(
                                    stderr,
                                    "approval: auto-approving {tool_name} ({reason})"
                                )?;
                                stderr.flush()?;
                            }
                            PromptFormat::Json => {
                                emit_prompt_event(
                                    stdout,
                                    &PromptWireEvent::ApprovalAutoApproved { tool_name, reason },
                                )?;
                            }
                        }
                        let _ = decision_tx.send(ToolApprovalDecision::AllowOnce);
                    }
                    PromptPermissionMode::Deny => {
                        match format {
                            PromptFormat::Default => {
                                writeln!(stderr, "approval: denying {tool_name} ({reason})")?;
                                stderr.flush()?;
                            }
                            PromptFormat::Json => {
                                emit_prompt_event(
                                    stdout,
                                    &PromptWireEvent::ApprovalDenied { tool_name, reason },
                                )?;
                            }
                        }
                        let _ = decision_tx.send(ToolApprovalDecision::Denied);
                    }
                    PromptPermissionMode::Fail => {
                        match format {
                            PromptFormat::Default => {
                                writeln!(stderr, "approval: failing on {tool_name} ({reason})")?;
                                stderr.flush()?;
                            }
                            PromptFormat::Json => {
                                emit_prompt_event(
                                    stdout,
                                    &PromptWireEvent::ApprovalDenied { tool_name, reason },
                                )?;
                            }
                        }
                        let _ = decision_tx.send(ToolApprovalDecision::Denied);
                        result = Err(SqueezyError::Tool(
                            "non-interactive prompt requested permission; \
                             rerun with --prompt-permission-mode auto-approve-ask for a one-shot bypass, \
                             or add a matching `[permissions]` default / `[[permissions.rules]]` entry \
                             in settings.toml for a persistent allow"
                                .to_string(),
                        ));
                        break;
                    }
                }
            }
            AgentEvent::McpElicitationRequested { response_tx, .. } => {
                let _ = response_tx.send(McpElicitationResponse::cancel());
            }
            AgentEvent::RequestUserInputRequested { response_tx, .. } => {
                let _ = response_tx.send(RequestUserInputResponse::cancelled());
            }
            AgentEvent::Completed {
                message,
                response_id,
                cost,
                ..
            } => {
                completed = true;
                match format {
                    PromptFormat::Default => {
                        if !wrote_text_delta && !message.content.is_empty() {
                            write!(stdout, "{}", message.content)?;
                        }
                        writeln!(stdout)?;
                        stdout.flush()?;
                        writeln!(
                            stderr,
                            "tokens: input={} output={} cached={} cache_write={} cost_usd={}",
                            format_token(cost.input_tokens),
                            format_token(cost.output_tokens),
                            format_token(cost.cached_input_tokens),
                            format_token(cost.cache_write_input_tokens),
                            format_usd_micros(cost.estimated_usd_micros),
                        )?;
                        stderr.flush()?;
                    }
                    PromptFormat::Json => {
                        emit_prompt_event(
                            stdout,
                            &PromptWireEvent::Completed { response_id, cost },
                        )?;
                    }
                }
                break;
            }
            AgentEvent::Failed { error, .. } => {
                if format == PromptFormat::Json {
                    let _ = emit_prompt_event(stdout, &PromptWireEvent::Failed(error.to_string()));
                } else {
                    let _ = writeln!(stderr, "error: {error}");
                    let _ = stderr.flush();
                }
                result = Err(error);
                break;
            }
            AgentEvent::Cancelled { .. } => {
                if format == PromptFormat::Json {
                    let _ = emit_prompt_event(stdout, &PromptWireEvent::Cancelled);
                } else {
                    let _ = writeln!(stderr, "cancelled");
                    let _ = stderr.flush();
                }
                break;
            }
            // Bookkeeping events (job notifications, MCP status, cost
            // updates, context compactions, sub-agent lifecycle, etc.)
            // are silent in print mode. They are still recorded in the
            // session log that the agent maintains internally, so
            // `squeezy sessions show <id>` keeps the full record for
            // post-mortem.
            _ => {}
        }
    }

    if !completed && result.is_ok() && format == PromptFormat::Default {
        // Receiver closed without a Completed event (e.g. the agent
        // dropped the channel because the user hit Ctrl-C externally).
        // Make sure stdout ends on a newline so the shell prompt is not
        // glued to the last assistant token.
        let _ = writeln!(stdout);
        let _ = stdout.flush();
    }
    result
}

/// Wire-friendly subset of `AgentEvent` used by `--prompt --format
/// json`. Keeps the `{"type": ..., "data": ...}` tag/content shape that
/// the previous `LlmEvent`-based stream documented; adds `tool_*` and
/// `approval_auto_approved` entries so callers can observe the new
/// tool-loop behaviour. Schema is still labeled experimental in the CLI
/// help — additive changes are fine, but breaking ones should bump that
/// disclaimer.
///
#[derive(Debug, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum PromptWireEvent {
    Started,
    TextDelta(String),
    ReasoningDelta(String),
    ToolCallStarted(ToolCall),
    ToolCallCompleted(ToolResult),
    ApprovalAutoApproved {
        tool_name: String,
        reason: String,
    },
    ApprovalDenied {
        tool_name: String,
        reason: String,
    },
    Completed {
        response_id: Option<String>,
        cost: CostSnapshot,
    },
    Failed(String),
    Cancelled,
}

fn emit_prompt_event<W: Write>(
    writer: &mut W,
    event: &PromptWireEvent,
) -> squeezy_core::Result<()> {
    let line = serde_json::to_string(event)
        .map_err(|err| SqueezyError::Parse(format!("failed to serialize prompt event: {err}")))?;
    writeln!(writer, "{line}")?;
    writer.flush()?;
    Ok(())
}

fn tool_status_label(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Success => "ok",
        ToolStatus::Error => "error",
        ToolStatus::Denied => "denied",
        ToolStatus::Stale => "stale",
        ToolStatus::Cancelled => "cancelled",
    }
}

fn format_token(value: Option<u64>) -> String {
    value.map_or_else(|| "-".to_string(), |value| value.to_string())
}

fn format_usd_micros(value: Option<u64>) -> String {
    match value {
        Some(value) => format!("${:.6}", value as f64 / 1_000_000.0),
        None => "-".to_string(),
    }
}

fn provider_from_app_config(config: &AppConfig) -> Arc<dyn LlmProvider> {
    match provider_from_config(&config.provider) {
        Ok(provider) => provider,
        Err(error) => Arc::new(UnavailableProvider::new(
            squeezy_llm::provider_name(&config.provider),
            error.to_string(),
        )),
    }
}

fn config_from_cli_provider(
    provider: Option<&str>,
    profile: Option<&str>,
) -> squeezy_core::Result<AppConfig> {
    if profile.is_some() {
        return AppConfig::from_env_and_settings_with_profile(provider, profile);
    }
    let Some(provider) = provider else {
        return AppConfig::from_env_and_settings();
    };
    AppConfig::from_env_and_settings_with_provider(provider)
}

fn show_telemetry_notice_once(config: &AppConfig) {
    if !config.telemetry.enabled {
        return;
    }
    let path = telemetry_notice_path();
    if path.exists() {
        return;
    }
    eprintln!(
        "Squeezy sends anonymous usage telemetry: version, OS, tool timings/status, graph performance, and coarse failures. No prompts, file contents, paths, commands, URLs, or tool arguments are sent. Opt out with SQUEEZY_TELEMETRY=off."
    );
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, b"shown\n");
}

fn handle_help_command(topic: Option<&str>, cli: &Cli) -> squeezy_core::Result<()> {
    let config = config_from_cli(cli)?;
    let config_inspect = config.inspect_redacted();
    let help = squeezy_skills::SqueezyHelp::new(config_inspect);
    let answer = cli_help_answer(&help, topic);
    let rendered = answer.render_markdown();
    println!("{rendered}");
    Ok(())
}

fn cli_help_answer(
    help: &squeezy_skills::SqueezyHelp,
    topic: Option<&str>,
) -> squeezy_skills::HelpAnswer {
    let Some(topic) = topic.map(str::trim).filter(|topic| !topic.is_empty()) else {
        return help.topic_index();
    };
    let input = format!("/help {topic}");
    help.answer_for_input(&input)
        .unwrap_or_else(|| help.answer_topic(topic))
}

fn telemetry_notice_path() -> PathBuf {
    env::var_os("SQUEEZY_TELEMETRY_NOTICE_PATH")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".squeezy/telemetry_notice"))
        })
        .unwrap_or_else(|| PathBuf::from(".squeezy/telemetry_notice"))
}

/// Resolves where `config init --project` writes its template.
///
/// Mirrors project-settings discovery so the write target and the
/// overwrite guard refer to the file that loading would actually pick up:
/// an existing ancestor `squeezy.toml` when one exists, otherwise
/// `cwd/squeezy.toml`. This prevents writing a closer file from a
/// subdirectory that would silently shadow the repo's real project config.
fn project_init_target(cwd: impl AsRef<Path>) -> PathBuf {
    let cwd = cwd.as_ref();
    find_project_settings_path(cwd).unwrap_or_else(|| cwd.join(PROJECT_SETTINGS_FILE))
}

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
