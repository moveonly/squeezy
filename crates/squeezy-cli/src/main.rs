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
use squeezy_agent::{Agent, AgentEvent, RequestUserInputResponse, ToolApprovalDecision};
use squeezy_core::{
    AppConfig, CostSnapshot, DEFAULT_OLLAMA_BASE_URL, MODEL_SELECTION_VERSION, McpTransport,
    ModelProfile, OpenAiCompatiblePreset, PROJECT_SETTINGS_FILE, PermissionMode, ReasoningEffort,
    SessionMode, SettingsFile, SqueezyError, default_settings_path, find_project_settings_path,
    per_repo_settings_path, project_settings_template, user_settings_template,
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
use squeezy_store::{
    BugReportOptions, CleanupMode, RepoProfileLoad, SemanticSupport, SessionMetadata, SessionQuery,
    SessionStatus, SessionStore, default_bug_report_path, ensure_repo_profile,
    parse_bug_report_section, refresh_repo_profile,
};
use squeezy_telemetry::{
    FeedbackClient, ReportUpload, TelemetryClient, TelemetryEvent, prepare_feedback,
};
use squeezy_tools::{
    McpClientRegistry, McpElicitationResponse, McpServerStatus, McpStaleOutcome, ToolCall,
    ToolResult, ToolStatus,
};
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

#[derive(Debug, Parser)]
#[command(name = "squeezy", version, about = "Cost-aware coding agent TUI")]
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
        help = "Non-interactive output format for --prompt: 'default' (text deltas) or 'json' (one event per line). Experimental; schema may change.",
        default_value = "default"
    )]
    format: PromptFormat,
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
    Inspect,
    #[command(about = "Recompute and persist the generated local repo profile")]
    Refresh,
    #[command(about = "Print suggested project config settings for manual adoption")]
    Recommendations,
}

#[derive(Debug, Subcommand)]
enum SessionsCommand {
    #[command(about = "List local sessions")]
    List(SessionListArgs),
    #[command(about = "Show a local session summary")]
    Show { id: String },
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

    // Resolve `--session <prefix>` against the on-disk session store
    // before any downstream code sees it, so the user can pass a short
    // unique prefix (`squeezy --session abc12`) the same way `squeezy
    // sessions resume abc12` works. Ambiguous and unknown prefixes
    // fail fast with a clear message instead of being forwarded into a
    // generic "session not found" later.
    let resolved_session_id: Option<String> = match cli.session.as_deref() {
        Some(id) => Some(
            SessionStore::open(&config)
                .resolve_session_id_prefix(id)
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
        let sessions = SessionStore::open(&config)
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
        let store = SessionStore::open(&config);
        if !confirm_cross_project_resume_stdio(&store, id, cli.force_cross_project)? {
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
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                fs::create_dir_all(parent)?;
            }
            fs::write(&path, template)?;
            println!("wrote {}", path.display());
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
    }
}

fn skills_install(cli: &Cli, force: bool) -> squeezy_core::Result<()> {
    let config = config_from_cli(cli)?;
    let target = &config.skills.user_dir;
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
    if summaries.is_empty() {
        println!("No skills discovered.");
        return Ok(());
    }
    let mut rows: Vec<[String; 4]> = Vec::with_capacity(summaries.len() + 1);
    rows.push([
        "NAME".to_string(),
        "STATE".to_string(),
        "SOURCE".to_string(),
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
        rows.push([
            summary.name.clone(),
            state.to_string(),
            summary.source.as_str().to_string(),
            summary.location.display().to_string(),
        ]);
    }
    let widths = (0..4)
        .map(|col| rows.iter().map(|row| row[col].len()).max().unwrap_or(0))
        .collect::<Vec<_>>();
    for row in rows {
        println!(
            "{:<w0$}  {:<w1$}  {:<w2$}  {}",
            row[0],
            row[1],
            row[2],
            row[3],
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2],
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

fn skills_validate(cli: &Cli, json: bool) -> squeezy_core::Result<()> {
    let config = config_from_cli(cli)?;
    // Walk every configured skill root directly rather than iterating
    // catalog.summaries(). Discovery silently drops malformed SKILL.md
    // files (parse errors, invalid names) with a tracing warn; validate
    // must surface those failures, so it scans the filesystem itself.
    let raw_results = squeezy_skills::validate_skill_dirs(&config.workspace_root, &config.skills);
    // Build the catalog separately for ambiguous-name detection.
    let catalog = squeezy_skills::SkillCatalog::discover(&config.workspace_root, &config.skills);

    let mut diagnostics: Vec<serde_json::Value> = Vec::new();
    let mut ok = 0usize;
    let mut errored = 0usize;
    let mut ambiguous = 0usize;
    for result in &raw_results {
        let mut issues: Vec<String> = Vec::new();
        if let Err(err) = &result.outcome {
            issues.push(err.clone());
        }
        // Also flag ambiguous names for skills that parsed successfully.
        if let Some(name) = &result.name
            && catalog.ambiguous_names().contains(name)
        {
            ambiguous += 1;
            issues.push(
                "duplicate name at same precedence; auto-trigger activation skipped".to_string(),
            );
        }
        if issues.is_empty() {
            ok += 1;
        } else {
            errored += 1;
        }
        let display_name = result.name.as_deref().unwrap_or(
            result
                .path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("?"),
        );
        diagnostics.push(serde_json::json!({
            "name": result.name,
            "location": result.path,
            "issues": issues,
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
            let issues = entry
                .get("issues")
                .and_then(|v| v.as_array())
                .map(|arr| arr.len())
                .unwrap_or(0);
            if issues == 0 {
                println!("ok      {name}");
            } else {
                println!("error   {name}");
                if let Some(issue_arr) = entry.get("issues").and_then(|v| v.as_array()) {
                    for issue in issue_arr {
                        if let Some(text) = issue.as_str() {
                            println!("          {text}");
                        }
                    }
                }
            }
        }
        println!(
            "{} ok, {} error(s), {} ambiguous, {} total",
            ok, errored, ambiguous, total
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
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, doc.to_string())?;
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
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, doc.to_string())?;
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

fn handle_repo_command(command: &RepoCommand, cli: &Cli) -> squeezy_core::Result<()> {
    let config = config_from_cli(cli)?;
    match command {
        RepoCommand::Inspect => {
            let loaded = ensure_repo_profile(&config.workspace_root, &config.graph)?;
            println!("{}", loaded.profile.render_human());
            println!(
                "registry: {} ({})",
                loaded.registry_path.display(),
                loaded.status.as_str()
            );
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
    }
}

async fn handle_sessions_command(command: &SessionsCommand, cli: &Cli) -> squeezy_core::Result<()> {
    let config = config_from_cli(cli)?;
    let store = SessionStore::open(&config);
    match command {
        SessionsCommand::List(args) => {
            let sessions = store.list(&session_query_from_args(args)?)?;
            for session in sessions {
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}",
                    session.session_id,
                    session.status.as_str(),
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
            Ok(())
        }
        SessionsCommand::Show { id } => {
            let record = store.show(id)?;
            println!("id={}", record.metadata.session_id);
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
            Ok(())
        }
        SessionsCommand::Resume {
            id,
            force_cross_project,
        } => {
            // Pi-style prefix resolution: the user can type a short
            // unique prefix of a session id (e.g. `squeezy sessions
            // resume abc12`) and the store expands it to the full id
            // before we hand it to the TUI. Ambiguous and unknown
            // prefixes surface as actionable errors instead of being
            // forwarded as-is into a "session not found" downstream.
            let resolved = store
                .resolve_session_id_prefix(id)
                .map_err(|err| SqueezyError::Tool(err.to_string()))?;
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
            let provider = provider_from_app_config(&config);
            let child_metadata = SessionMetadata::new(&config, provider.name());
            let child = store.fork_session(id, child_metadata)?;
            let child_id = child.session_id().to_string();
            drop(child);
            squeezy_tui::resume(config, provider, child_id).await
        }
        SessionsCommand::Replay { id, json } => {
            let report = Agent::replay_session(config, id).await?;
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).map_err(|err| {
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
            let report = store.cleanup_with(ids, None, mode)?;
            for id in report.archived {
                println!("archived {id}");
            }
            for id in report.removed {
                println!("removed {id}");
            }
            Ok(())
        }
        SessionsCommand::Archive { id } => {
            store.archive_session(id)?;
            println!("archived {id}");
            Ok(())
        }
        SessionsCommand::Unarchive { id } => {
            store.unarchive_session(id)?;
            println!("unarchived {id}");
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
    if args.html {
        let record = store.show(&args.id)?;
        let opts = squeezy_agent::ExportOpts {
            include_tool_outputs: !args.no_tool_outputs,
            theme: args.theme.to_theme(),
        };
        let html = squeezy_agent::export_session_to_html(&record, &opts)
            .map_err(|err| SqueezyError::Tool(format!("failed to render session html: {err}")))?;
        let target = args
            .output
            .clone()
            .unwrap_or_else(|| PathBuf::from(format!("squeezy-session-{}.html", args.id)));
        fs::write(&target, &html)?;
        println!("wrote {} ({} bytes)", target.display(), html.len());
        return Ok(());
    }
    let value = store.export(&args.id)?;
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
    let bundle = SessionStore::open(config).build_bug_report(config, &args.id, options)?;
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
    let path = args
        .output
        .clone()
        .unwrap_or_else(|| default_bug_report_path(config, &args.id));
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
/// string. Trailing path separators are trimmed before comparison so a
/// recorded `"/repo"` and a current `"/repo/"` are treated as equal —
/// the message itself preserves the original strings so the operator
/// can spot the discrepancy.
fn cross_project_resume_prompt(session_cwd: &str, current_cwd: &str) -> Option<String> {
    fn normalize(value: &str) -> &str {
        value.trim_end_matches(['/', std::path::MAIN_SEPARATOR])
    }
    if normalize(session_cwd) == normalize(current_cwd) {
        return None;
    }
    Some(format!(
        "Resume session from {session_cwd} in current cwd {current_cwd}? [y/N] "
    ))
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
    if families.is_empty() {
        return "none".to_string();
    }
    let mut summary = String::new();
    for (name, files) in families.into_values() {
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

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, doc.to_string())?;
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
            let pick = sessions
                .iter()
                .find(|meta| meta.resume_available && meta.cwd == cwd_str)
                .map(|meta| meta.session_id.clone());
            if pick.is_some() {
                ResumeResolution {
                    session_id: pick,
                    note: None,
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

fn format_optional_u64(value: Option<u64>) -> String {
    value.map_or_else(|| "-".to_string(), |value| value.to_string())
}

async fn run_prompts(
    config: AppConfig,
    provider: Arc<dyn LlmProvider>,
    prompts: Vec<print_mode::PromptInput>,
    format: PromptFormat,
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
    let result = pump_prompts(&agent, prompts, format, &mut stdout, &mut stderr).await;
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
        pump_prompt_events(rx, format, stdout, stderr).await?;
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
                // There is nobody to prompt in print mode. The
                // permission policy already filtered out the
                // hard-default deny cases (read=Allow, edit=Allow),
                // so anything that reaches here was flagged Ask by
                // configuration. Approving once keeps CI moving;
                // operators who want stricter control can set
                // permission rules in settings.toml or pin Plan
                // mode via `--mode plan`.
                let tool_name = request.tool_name.clone();
                let reason = request.reason.clone();
                match format {
                    PromptFormat::Default => {
                        writeln!(stderr, "approval: auto-approving {tool_name} ({reason})")?;
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
#[derive(Debug, serde::Serialize)]
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
