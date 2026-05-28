use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use clap::{Args, Parser, Subcommand, ValueEnum};
use squeezy_agent::{Agent, AgentEvent, RequestUserInputResponse, ToolApprovalDecision};
use squeezy_core::{
    AppConfig, CostSnapshot, DEFAULT_OLLAMA_BASE_URL, MODEL_SELECTION_VERSION, McpTransport,
    ModelProfile, OpenAiCompatiblePreset, PROJECT_SETTINGS_FILE, PermissionMode, ReasoningEffort,
    SessionMode, SettingsFile, SqueezyError, default_settings_path, find_project_settings_path,
    project_settings_template, user_settings_template,
};
use squeezy_llm::{
    LlmProvider, ModelInfo, PROVIDERS, UnavailableProvider, capabilities_for,
    fetch_ollama_model_names, models_for_provider, provider_from_config,
};

mod auth;
mod doctor;
mod print_mode;
mod providers;
mod update;
use auth::handle_auth_command;
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
use squeezy_tools::{McpElicitationResponse, ToolCall, ToolResult, ToolStatus};
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
        long = "no-resume-picker",
        help = "Skip the startup picker that offers to resume a recent session for this directory"
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
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    #[command(about = "Inspect or initialize Squeezy configuration")]
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
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
    #[command(about = "Print the effective merged configuration with secrets redacted")]
    Inspect,
    #[command(about = "Create a default user or project settings file")]
    Init {
        #[command(flatten)]
        scope: InitScope,
        #[arg(long, help = "Overwrite an existing file")]
        force: bool,
    },
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
    },
    #[command(about = "Add an MCP server to user or project settings")]
    Add(McpAddArgs),
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
    #[arg(long, help = "Command for stdio MCP servers")]
    command: Option<String>,
    #[arg(long = "arg", help = "Command argument; repeat for multiple args")]
    args: Vec<String>,
    #[arg(long, help = "URL for http or sse MCP servers")]
    url: Option<String>,
    #[arg(long, help = "Timeout in milliseconds")]
    timeout_ms: Option<u64>,
    #[arg(long = "env", help = "Environment entry in KEY=VALUE form")]
    env: Vec<String>,
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
    Resume { id: String },
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

#[tokio::main]
async fn main() -> squeezy_core::Result<()> {
    squeezy_core::pre_main_hardening(squeezy_core::HardeningConfig::default());
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let stdin_is_tty = print_mode::stdin_is_tty();
    let prompt_mode_active = !cli.prompt.is_empty() || !stdin_is_tty;
    if cli.format == PromptFormat::Json && !prompt_mode_active {
        return Err(SqueezyError::Config(
            "--format json requires --prompt or piped stdin; interactive sessions and subcommands only emit human-formatted output"
                .to_string(),
        ));
    }
    match &cli.command {
        Some(Command::Config { command }) => return handle_config_command(command, &cli),
        Some(Command::Repo { command }) => return handle_repo_command(command, &cli),
        Some(Command::Sessions { command }) => {
            return handle_sessions_command(command, &cli).await;
        }
        Some(Command::Feedback(args)) => return handle_feedback_command(args, &cli).await,
        Some(Command::Mcp { command }) => return handle_mcp_command(command, &cli),
        Some(Command::Ask(args)) => return handle_ask_command(args).await,
        Some(Command::Auth { command }) => return handle_auth_command(command).await,
        Some(Command::Doctor(args)) => {
            let report = doctor::run(args).await?;
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

    if should_run_startup_model_selector(&cli, &config)? {
        run_startup_model_selector(&config).await?;
        config = config_from_cli(&cli)?;
    }

    let onboarding = prepare_repo_profile(&mut config)?;

    show_telemetry_notice_once(&config);
    let telemetry = TelemetryClient::from_config(&config);
    telemetry.record(TelemetryEvent::app_started(&config)).await;

    let provider = provider_from_app_config(&config);
    let resume_flag = if cli.continue_session {
        ResumeFlag::Continue
    } else if let Some(id) = cli.session.as_deref() {
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
    if prompt_mode_active {
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
            prompts,
            cli.format,
            resume_resolution.session_id,
        )
        .await;
        let _ = telemetry.flush().await;
        return result;
    }

    // Best-effort update probe before the TUI takes over the terminal. The
    // helper consults a 24h cache, so this is a no-op HTTP-wise on most
    // startups; the banner stays quiet unless GitHub reports a new release
    // we haven't already nagged the user about.
    let update_banner = update::banner_for_startup(&update::check_for_update().await);
    let resume_session_id = resume_resolution.session_id;
    let skip_resume_picker = cli.no_resume_picker || resume_session_id.is_some();
    let result = squeezy_tui::run_with_startup_profile(
        config,
        provider,
        squeezy_tui::StartupProfile {
            onboarding_summary: onboarding.visible_summary,
            languages: onboarding.language_summary,
            skip_resume_picker,
            update_banner,
            resume_session_id,
        },
    )
    .await;
    let _ = telemetry.flush().await;
    result
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
    if cli_used && !config.config_sources.iter().any(|source| source == "cli") {
        config.config_sources.push("cli".to_string());
    }
    Ok(config)
}

fn handle_config_command(command: &ConfigCommand, cli: &Cli) -> squeezy_core::Result<()> {
    match command {
        ConfigCommand::Inspect => {
            let config = config_from_cli(cli)?;
            print!("{}", config.inspect_redacted());
            Ok(())
        }
        ConfigCommand::Init { scope, force } => {
            let (path, template) = if scope.user {
                (default_settings_path(), user_settings_template())
            } else {
                (
                    PathBuf::from(PROJECT_SETTINGS_FILE),
                    project_settings_template(),
                )
            };
            if path.exists() && !force {
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
            Ok(())
        }
    }
}

fn handle_mcp_command(command: &McpCommand, cli: &Cli) -> squeezy_core::Result<()> {
    match command {
        McpCommand::List { json } => {
            let config = config_from_cli(cli)?;
            if *json {
                let servers = config
                    .mcp_servers
                    .iter()
                    .map(|(name, server)| {
                        serde_json::json!({
                            "name": name,
                            "enabled": server.enabled,
                            "transport": server.transport.as_str(),
                            "command": server.command,
                            "args": server.args,
                            "url": server.url,
                            "timeout_ms": server.timeout_ms,
                            "env": server.env.keys().collect::<Vec<_>>(),
                            "permission_default": server.permissions.default.map(|value| value.as_str()),
                            "permission_rules": server.permissions.rules.len(),
                        })
                    })
                    .collect::<Vec<_>>();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&servers).unwrap_or_default()
                );
            } else if config.mcp_servers.is_empty() {
                println!("No MCP servers configured.");
            } else {
                let mut rows: Vec<[String; 4]> = Vec::with_capacity(config.mcp_servers.len() + 1);
                rows.push([
                    "NAME".to_string(),
                    "STATE".to_string(),
                    "TRANSPORT".to_string(),
                    "ENDPOINT".to_string(),
                ]);
                for (name, server) in &config.mcp_servers {
                    let state = if server.enabled {
                        "enabled"
                    } else {
                        "disabled"
                    };
                    let endpoint = server
                        .command
                        .as_deref()
                        .or(server.url.as_deref())
                        .unwrap_or("-");
                    rows.push([
                        name.clone(),
                        state.to_string(),
                        server.transport.as_str().to_string(),
                        endpoint.to_string(),
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
        SessionsCommand::Resume { id } => {
            let provider = provider_from_app_config(&config);
            squeezy_tui::resume(config, provider, id.clone()).await
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

fn prepare_repo_profile(config: &mut AppConfig) -> squeezy_core::Result<PreparedRepoProfile> {
    let loaded = ensure_repo_profile(&config.workspace_root, &config.graph)?;
    append_repo_profile_instructions(config, &loaded);
    let language_summary = startup_language_summary(&loaded);
    let visible_summary = loaded
        .status
        .should_show_onboarding()
        .then(|| loaded.profile.compact_summary(loaded.status));
    Ok(PreparedRepoProfile {
        visible_summary,
        language_summary,
    })
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
    families
        .into_values()
        .map(|(name, files)| format!("{name} {files}"))
        .collect::<Vec<_>>()
        .join(", ")
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

    fn complete(self) -> bool {
        self.provider
            && self.model
            && self.selection_version.unwrap_or(0) >= MODEL_SELECTION_VERSION
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProviderChoice {
    provider: &'static str,
    label: String,
    api_key_env: Option<String>,
    base_url: Option<String>,
    models: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StartupModelSelection {
    provider: &'static str,
    model: String,
    api_key_env: Option<String>,
    base_url: Option<String>,
    reasoning_effort: Option<ReasoningEffort>,
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
        || env::var_os("SQUEEZY_PROVIDER").is_some()
        || env::var_os("SQUEEZY_MODEL").is_some()
    {
        return Ok(false);
    }
    Ok(!current_model_selection_state(&config.workspace_root)?.complete())
}

fn current_model_selection_state(
    workspace_root: &Path,
) -> squeezy_core::Result<ModelSelectionState> {
    let mut state = ModelSelectionState::default();
    state.merge(model_selection_state_from_settings(
        &default_settings_path(),
    )?);
    if let Some(path) = find_project_settings_path(workspace_root) {
        state.merge(model_selection_state_from_settings(&path)?);
    }
    Ok(state)
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

async fn run_startup_model_selector(config: &AppConfig) -> squeezy_core::Result<()> {
    let settings_path = default_settings_path();
    let choices = detect_provider_choices(config).await;
    if choices.is_empty() {
        return Err(SqueezyError::Config(
            "no provider credentials or local Ollama models detected; set OPENROUTER_API_KEY for the fastest path to many models, or set OPENAI_API_KEY / ANTHROPIC_API_KEY / GEMINI_API_KEY / GROQ_API_KEY / XAI_API_KEY / DEEPSEEK_API_KEY / MISTRAL_API_KEY / TOGETHER_API_KEY / FIREWORKS_API_KEY / CEREBRAS_API_KEY / AI_GATEWAY_API_KEY / PORTKEY_API_KEY, or start Ollama, then run squeezy again"
                .to_string(),
        ));
    }

    eprintln!(
        "Squeezy will save your provider, model, and supported model options to {} and use them as defaults for future sessions.",
        settings_path.display()
    );
    eprintln!("No API key values will be written; only environment variable names are saved.");

    let provider_index = prompt_choice(
        "Choose provider/token:",
        choices.iter().map(|choice| choice.label.as_str()),
    )?;
    let provider = &choices[provider_index];
    let model_index = prompt_choice(
        "Choose model:",
        provider.models.iter().map(|model| model.as_str()),
    )?;
    let model = parse_model_choice_id(&provider.models[model_index]);
    let reasoning_effort = if capabilities_for(provider.provider, &model)
        .is_some_and(|capabilities| capabilities.reasoning_effort)
    {
        let efforts = [
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::XHigh,
        ];
        let effort_index = prompt_choice(
            "Choose reasoning effort:",
            efforts.iter().map(|effort| effort.as_str()),
        )?;
        Some(efforts[effort_index])
    } else {
        None
    };

    let selection = StartupModelSelection {
        provider: provider.provider,
        model,
        api_key_env: provider.api_key_env.clone(),
        base_url: provider.base_url.clone(),
        reasoning_effort,
    };
    save_startup_model_selection(&settings_path, &selection)?;
    eprintln!(
        "Saved provider/model defaults to {}. Edit that file to change them later.",
        settings_path.display()
    );
    Ok(())
}

async fn detect_provider_choices(_config: &AppConfig) -> Vec<ProviderChoice> {
    let mut choices = Vec::new();

    // OpenAI-compatible presets surface first when their API keys are set —
    // aggregators (OpenRouter / Vercel / PortKey) give access to many models
    // through one credit account and are the recommended starting point.
    for preset in OpenAiCompatiblePreset::all() {
        if matches!(preset, OpenAiCompatiblePreset::Custom) {
            continue;
        }
        let env_var = preset.default_api_key_env();
        if env_var.is_empty() {
            continue;
        }
        if env::var_os(env_var).is_none() {
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

    for api_key_env in detected_env_names("OPENAI_API_KEY_ENV", &["OPENAI_API_KEY"]) {
        choices.push(hosted_provider_choice(
            "openai",
            "OpenAI",
            api_key_env,
            None,
        ));
    }
    for api_key_env in detected_env_names("ANTHROPIC_API_KEY_ENV", &["ANTHROPIC_API_KEY"]) {
        choices.push(hosted_provider_choice(
            "anthropic",
            "Anthropic",
            api_key_env,
            None,
        ));
    }
    for api_key_env in
        detected_env_names("GOOGLE_API_KEY_ENV", &["GEMINI_API_KEY", "GOOGLE_API_KEY"])
    {
        choices.push(hosted_provider_choice(
            "google",
            "Gemini",
            api_key_env,
            None,
        ));
    }
    for api_key_env in detected_env_names("AZURE_OPENAI_API_KEY_ENV", &["AZURE_OPENAI_API_KEY"]) {
        if let Some(base_url) = env::var("AZURE_OPENAI_BASE_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
        {
            choices.push(hosted_provider_choice(
                "azure_openai",
                "Azure OpenAI",
                api_key_env,
                Some(base_url),
            ));
        }
    }

    let ollama_base_url = env::var("OLLAMA_BASE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_OLLAMA_BASE_URL.to_string());
    let ollama_models = fetch_ollama_model_names(&ollama_base_url).await;
    if !ollama_models.is_empty() {
        let mut models = ollama_models;
        models.sort_by_key(|model| {
            if model.starts_with(squeezy_core::DEFAULT_OLLAMA_MODEL) {
                0
            } else {
                1
            }
        });
        choices.push(ProviderChoice {
            provider: "ollama",
            label: format!("Ollama local ({ollama_base_url})"),
            api_key_env: None,
            base_url: Some(ollama_base_url),
            models,
        });
    }

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
    if needs_refresh {
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
        label: format!("{} via {api_key_env}", preset.display_name()),
        api_key_env: Some(api_key_env),
        base_url: Some(base_url),
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

fn detected_env_names(selector_env: &str, defaults: &[&str]) -> Vec<String> {
    let mut names = Vec::new();
    if let Some(name) = env::var(selector_env)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .filter(|name| env::var_os(name).is_some())
    {
        names.push(name);
    }
    for default in defaults {
        if env::var_os(default).is_some() && !names.iter().any(|name| name == default) {
            names.push((*default).to_string());
        }
    }
    names
}

fn hosted_provider_choice(
    provider: &'static str,
    label: &str,
    api_key_env: String,
    base_url: Option<String>,
) -> ProviderChoice {
    ProviderChoice {
        provider,
        label: format!("{label} via {api_key_env}"),
        api_key_env: Some(api_key_env),
        base_url,
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

fn prompt_choice<'a>(
    prompt: &str,
    choices: impl IntoIterator<Item = &'a str>,
) -> squeezy_core::Result<usize> {
    let choices = choices.into_iter().collect::<Vec<_>>();
    if choices.is_empty() {
        return Err(SqueezyError::Config(format!(
            "{prompt} no choices available"
        )));
    }
    loop {
        eprintln!("\n{prompt}");
        for (index, choice) in choices.iter().enumerate() {
            eprintln!("  {}. {}", index + 1, choice);
        }
        eprint!("Enter choice number: ");
        io::stderr().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if let Ok(index) = answer.trim().parse::<usize>()
            && (1..=choices.len()).contains(&index)
        {
            return Ok(index - 1);
        }
        eprintln!(
            "Invalid choice; enter a number from 1 to {}.",
            choices.len()
        );
    }
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
    prompts: Vec<String>,
    format: PromptFormat,
    resume_session_id: Option<String>,
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
        Agent::resume(config, provider, &id)?.0
    } else {
        Agent::new(config, provider)
    };
    let rx = agent.start_turn(prompt, CancellationToken::new());
    let stdout = io::stdout();
    let stderr = io::stderr();
    let mut stdout = stdout.lock();
    let mut stderr = stderr.lock();
    pump_prompt_events(rx, format, &mut stdout, &mut stderr).await
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
                response_id, cost, ..
            } => {
                completed = true;
                match format {
                    PromptFormat::Default => {
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
                LlmEvent::ReasoningDelta { .. } | LlmEvent::ReasoningDone(_) => {}
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
        Err(error) => Arc::new(UnavailableProvider::new("unavailable", error.to_string())),
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

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
