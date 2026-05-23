use std::{
    env, fs,
    io::{self, Write},
    path::PathBuf,
    sync::Arc,
};

use clap::{Args, Parser, Subcommand};
use futures_util::StreamExt;
use squeezy_core::{
    AppConfig, ModelProfile, PROJECT_SETTINGS_FILE, SqueezyError, default_settings_path,
    project_settings_template, user_settings_template,
};
use squeezy_llm::{
    LlmEvent, LlmInputItem, LlmProvider, LlmRequest, PROVIDERS, UnavailableProvider,
    models_for_provider, provider_from_config,
};
use squeezy_telemetry::{TelemetryClient, TelemetryEvent};
use tokio_util::sync::CancellationToken;

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

#[tokio::main]
async fn main() -> squeezy_core::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    if let Some(Command::Config { command }) = &cli.command {
        return handle_config_command(command, &cli);
    }

    let config = config_from_cli(&cli)?;

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
                "{}\t{}\t{:?}\tstreaming={} tools={} json={} vision={} state={}",
                model.provider,
                model.id,
                model.profile,
                model.capabilities.streaming,
                model.capabilities.tool_calling,
                model.capabilities.json_mode,
                model.capabilities.vision,
                model.capabilities.response_state
            );
        }
        return Ok(());
    }

    if cli.health {
        println!("squeezy: ok");
        println!("config_sources={}", config.config_sources.join(","));
        println!(
            "config_source_labels={}",
            config.config_source_labels().join(",")
        );
        return Ok(());
    }

    show_telemetry_notice_once(&config);
    let telemetry = TelemetryClient::from_config(&config);
    telemetry.record(TelemetryEvent::app_started(&config)).await;

    let provider = provider_from_app_config(&config);
    if let Some(prompt) = cli.prompt {
        let result = run_prompt(config, provider, prompt).await;
        let _ = telemetry.flush().await;
        return result;
    }

    let result = squeezy_tui::run(config, provider).await;
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

async fn run_prompt(
    config: AppConfig,
    provider: Arc<dyn LlmProvider>,
    prompt: String,
) -> squeezy_core::Result<()> {
    let request = LlmRequest {
        model: config.model,
        instructions: config.instructions,
        input: vec![LlmInputItem::UserText(prompt)],
        max_output_tokens: config.max_output_tokens,
        previous_response_id: None,
        tools: Vec::new(),
        store: config.store_responses,
    };
    let mut stream = provider.stream_response(request, CancellationToken::new());
    let mut stdout = io::stdout().lock();

    while let Some(event) = stream.next().await {
        match event? {
            LlmEvent::Started => {}
            LlmEvent::TextDelta(delta) => {
                write!(stdout, "{delta}")?;
                stdout.flush()?;
            }
            LlmEvent::ToolCall(tool_call) => {
                eprintln!(
                    "tool call requested but prompt mode has no tools: {}",
                    tool_call.name
                );
            }
            LlmEvent::Completed { cost, .. } => {
                writeln!(stdout)?;
                stdout.flush()?;
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
