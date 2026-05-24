use std::{
    collections::BTreeSet,
    env, fs,
    io::{self, Write},
    path::PathBuf,
    sync::Arc,
};

use clap::{Args, Parser, Subcommand};
use futures_util::StreamExt;
use squeezy_core::{
    AppConfig, McpTransport, ModelProfile, PROJECT_SETTINGS_FILE, PermissionMode, SessionMode,
    SqueezyError, default_settings_path, project_settings_template, user_settings_template,
};
use squeezy_llm::{
    LlmEvent, LlmInputItem, LlmProvider, LlmRequest, PROVIDERS, UnavailableProvider,
    models_for_provider, provider_from_config,
};
use squeezy_store::{
    BugReportOptions, RepoProfileLoad, ResumeItem, SessionEvent, SessionMetadata, SessionQuery,
    SessionResumeState, SessionStatus, SessionStore, default_bug_report_path, ensure_repo_profile,
    parse_bug_report_section, refresh_repo_profile,
};
use squeezy_telemetry::{
    FeedbackClient, ReportUpload, TelemetryClient, TelemetryEvent, prepare_feedback,
};
use tokio_util::sync::CancellationToken;
use toml_edit::{DocumentMut, Item, Table, Value as TomlValue};

#[derive(Debug, Parser)]
#[command(name = "squeezy", version, about = "Cost-aware coding agent TUI")]
struct Cli {
    /// Provider id. `SQUEEZY_PROVIDER` is also honored, but goes through the
    /// env source layer so it is tagged correctly by `config inspect`.
    #[arg(long, help = "Provider id (openai, anthropic, google, ...)")]
    provider: Option<String>,
    #[arg(long, help = "Model id; overrides settings and SQUEEZY_MODEL")]
    model: Option<String>,
    #[arg(long, help = "Model profile: cheap, balanced, or strong")]
    profile: Option<String>,
    #[arg(long, help = "Max output tokens; overrides SQUEEZY_MAX_OUTPUT_TOKENS")]
    max_output_tokens: Option<u32>,
    #[arg(long, help = "Start session mode: plan or build")]
    mode: Option<String>,
    #[arg(long, help = "List configured built-in providers")]
    list_providers: bool,
    #[arg(long, help = "List built-in model metadata")]
    list_models: bool,
    #[arg(long, help = "Run one non-interactive prompt and print streamed text")]
    prompt: Option<String>,
    #[arg(long, help = "Check configuration and exit without opening the TUI")]
    health: bool,
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
    #[command(about = "Export a redacted local session bundle as JSON")]
    Export { id: String },
    #[command(about = "Preview, save, or send a redacted bug-report archive")]
    Report(SessionReportArgs),
    #[command(about = "Remove expired sessions or explicit session ids")]
    Cleanup {
        #[arg(long = "id")]
        ids: Vec<String>,
    },
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
    #[arg(long, help = "running, completed, cancelled, failed, or truncated")]
    status: Option<String>,
    #[arg(long)]
    query: Option<String>,
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
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match &cli.command {
        Some(Command::Config { command }) => return handle_config_command(command, &cli),
        Some(Command::Repo { command }) => return handle_repo_command(command, &cli),
        Some(Command::Sessions { command }) => {
            return handle_sessions_command(command, &cli).await;
        }
        Some(Command::Feedback(args)) => return handle_feedback_command(args, &cli).await,
        Some(Command::Mcp { command }) => return handle_mcp_command(command, &cli),
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
            println!(
                "{}\t{}\t{:?}\tstreaming={} tools={} json={} vision={} state={} reasoning_tokens={} reasoning_effort={} verbosity={}",
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
                model.capabilities.text_verbosity
            );
        }
        return Ok(());
    }

    if cli.health {
        let onboarding = prepare_repo_profile(&mut config)?;
        println!("squeezy: ok");
        println!("config_sources={}", config.config_sources.join(","));
        println!(
            "config_source_labels={}",
            config.config_source_labels().join(",")
        );
        if let Some(summary) = onboarding.visible_summary {
            println!("{summary}");
        }
        return Ok(());
    }

    let onboarding = prepare_repo_profile(&mut config)?;

    show_telemetry_notice_once(&config);
    let telemetry = TelemetryClient::from_config(&config);
    telemetry.record(TelemetryEvent::app_started(&config)).await;

    let provider = provider_from_app_config(&config);
    if let Some(prompt) = cli.prompt {
        // Non-interactive prompt mode has no TUI to seed the summary into,
        // so surface it on stderr before the streamed completion lands on
        // stdout. The TUI path skips this print because it shows the same
        // summary in the transcript's system row.
        if let Some(summary) = &onboarding.visible_summary {
            eprintln!("{summary}");
        }
        let result = run_prompt(config, provider, prompt).await;
        let _ = telemetry.flush().await;
        return result;
    }

    let result =
        squeezy_tui::run_with_onboarding(config, provider, onboarding.visible_summary).await;
    let _ = telemetry.flush().await;
    result
}

fn config_from_cli(cli: &Cli) -> squeezy_core::Result<AppConfig> {
    let mut config = config_from_cli_provider(cli.provider.as_deref())?;
    let mut cli_used = false;
    if let Some(model) = &cli.model {
        cli_used = true;
        config.model = model.clone();
    }
    if let Some(profile) = cli.profile.as_deref().and_then(ModelProfile::parse) {
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
        SessionsCommand::Export { id } => {
            let value = store.export(id)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&value).map_err(|err| {
                    SqueezyError::Tool(format!("failed to serialize session export: {err}"))
                })?
            );
            Ok(())
        }
        SessionsCommand::Report(args) => handle_session_report_command(args, &config).await,
        SessionsCommand::Cleanup { ids } => {
            let report = store.cleanup(ids)?;
            for id in report.removed {
                println!("removed {id}");
            }
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
}

fn prepare_repo_profile(config: &mut AppConfig) -> squeezy_core::Result<PreparedRepoProfile> {
    let loaded = ensure_repo_profile(&config.workspace_root, &config.graph)?;
    append_repo_profile_instructions(config, &loaded);
    let visible_summary = loaded
        .status
        .should_show_onboarding()
        .then(|| loaded.profile.compact_summary(loaded.status));
    Ok(PreparedRepoProfile { visible_summary })
}

fn append_repo_profile_instructions(config: &mut AppConfig, loaded: &RepoProfileLoad) {
    config.instructions = format!(
        "{}\n\n{}",
        config.instructions,
        loaded.profile.model_context()
    );
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
    })
}

fn parse_session_status(value: &str) -> squeezy_core::Result<SessionStatus> {
    match value.trim().to_ascii_lowercase().as_str() {
        "running" => Ok(SessionStatus::Running),
        "completed" => Ok(SessionStatus::Completed),
        "cancelled" | "canceled" => Ok(SessionStatus::Cancelled),
        "failed" => Ok(SessionStatus::Failed),
        "truncated" => Ok(SessionStatus::Truncated),
        _ => Err(SqueezyError::Config(format!(
            "invalid session status {value:?}; expected running, completed, cancelled, failed, or truncated"
        ))),
    }
}

fn format_optional_u64(value: Option<u64>) -> String {
    value.map_or_else(|| "-".to_string(), |value| value.to_string())
}

async fn run_prompt(
    config: AppConfig,
    provider: Arc<dyn LlmProvider>,
    prompt: String,
) -> squeezy_core::Result<()> {
    let redactor = config.redaction.redactor()?;
    let session = SessionStore::open(&config)
        .start_session(SessionMetadata::new(&config, provider.name()))
        .ok();
    let mut redactions: u64 = 0;
    let redacted_prompt = redactor.redact(&prompt);
    redactions = redactions.saturating_add(redacted_prompt.redactions);
    let redacted_prompt = redacted_prompt.text;
    let redacted_instructions = redactor.redact(&config.instructions);
    redactions = redactions.saturating_add(redacted_instructions.redactions);
    let redacted_instructions = redacted_instructions.text;
    if let Some(session) = &session {
        let _ = session.append_event(SessionEvent::new(
            "user_message",
            None,
            Some(redacted_prompt.clone()),
            serde_json::json!({}),
        ));
    }
    let request = LlmRequest {
        model: config.model.clone(),
        instructions: redacted_instructions,
        input: vec![LlmInputItem::UserText(redacted_prompt.clone())],
        max_output_tokens: config.max_output_tokens,
        response_verbosity: Some(config.tui.response_verbosity),
        reasoning_effort: config.reasoning_effort,
        previous_response_id: None,
        tools: Vec::new(),
        store: config.store_responses,
    };
    let mut stream = provider.stream_response(request, CancellationToken::new());
    let mut stdout = io::stdout().lock();
    let mut assistant = String::new();

    while let Some(event) = stream.next().await {
        match event? {
            LlmEvent::Started => {}
            LlmEvent::TextDelta(delta) => {
                assistant.push_str(&delta);
                write!(stdout, "{delta}")?;
                stdout.flush()?;
            }
            LlmEvent::ToolCall(tool_call) => {
                eprintln!(
                    "tool call requested but prompt mode has no tools: {}",
                    tool_call.name
                );
            }
            LlmEvent::Completed { response_id, cost } => {
                writeln!(stdout)?;
                stdout.flush()?;
                let redacted_assistant = redactor.redact(&assistant);
                redactions = redactions.saturating_add(redacted_assistant.redactions);
                let redacted_assistant = redacted_assistant.text;
                if let Some(session) = &session {
                    let _ = session.append_event(SessionEvent::new(
                        "assistant_completed",
                        None,
                        Some(redacted_assistant.clone()),
                        serde_json::json!({ "response_id": response_id, "cost": cost }),
                    ));
                    let _ = session.write_resume_state(&SessionResumeState {
                        resume_available: true,
                        previous_response_id: response_id,
                        conversation: vec![
                            ResumeItem::UserText {
                                text: redacted_prompt.clone(),
                            },
                            ResumeItem::AssistantText {
                                text: redacted_assistant.clone(),
                            },
                        ],
                        transcript: vec![
                            squeezy_core::TranscriptItem::user(redacted_prompt.clone()),
                            squeezy_core::TranscriptItem::assistant(redacted_assistant.clone()),
                        ],
                        context_attachments: Vec::new(),
                        context_compaction: Default::default(),
                    });
                    let metrics = squeezy_core::SessionMetrics {
                        turns: 1,
                        model_output_bytes: redacted_assistant.len() as u64,
                        redactions,
                        provider: cost.clone(),
                        ..squeezy_core::SessionMetrics::default()
                    };
                    let _ =
                        session.finish(SessionStatus::Completed, cost.clone(), metrics, redactions);
                }
                eprintln!(
                    "tokens: input={} output={} cached={} cache_write={} cost_usd={}",
                    format_token(cost.input_tokens),
                    format_token(cost.output_tokens),
                    format_token(cost.cached_input_tokens),
                    format_token(cost.cache_write_input_tokens),
                    format_usd_micros(cost.estimated_usd_micros)
                );
            }
            LlmEvent::Cancelled => {
                eprintln!("cancelled");
                break;
            }
        }
    }

    Ok(())
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

fn config_from_cli_provider(provider: Option<&str>) -> squeezy_core::Result<AppConfig> {
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
