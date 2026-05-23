use std::{
    io::{self, Write},
    sync::Arc,
};

use clap::Parser;
use futures_util::StreamExt;
use squeezy_core::{AppConfig, ModelProfile};
use squeezy_llm::{
    LlmEvent, LlmInputItem, LlmProvider, LlmRequest, PROVIDERS, UnavailableProvider,
    models_for_provider, provider_from_config,
};
use squeezy_telemetry::{TelemetryClient, TelemetryEvent};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Parser)]
#[command(name = "squeezy", version, about = "Cost-aware coding agent TUI")]
struct Cli {
    #[arg(long, env = "SQUEEZY_PROVIDER", help = "Provider id")]
    provider: Option<String>,
    #[arg(long, env = "SQUEEZY_MODEL")]
    model: Option<String>,
    #[arg(
        long,
        env = "SQUEEZY_PROFILE",
        help = "Model profile: cheap, balanced, or strong"
    )]
    profile: Option<String>,
    #[arg(long, default_value_t = squeezy_core::DEFAULT_MAX_OUTPUT_TOKENS)]
    max_output_tokens: u32,
    #[arg(long, help = "List configured built-in providers")]
    list_providers: bool,
    #[arg(long, help = "List built-in model metadata")]
    list_models: bool,
    #[arg(long, help = "Run one non-interactive prompt and print streamed text")]
    prompt: Option<String>,
    #[arg(long, help = "Check configuration and exit without opening the TUI")]
    health: bool,
}

#[tokio::main]
async fn main() -> squeezy_core::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let mut config = config_from_cli_provider(cli.provider.as_deref())?;
    if let Some(model) = cli.model {
        config.model = model;
    }
    if let Some(profile) = cli.profile.as_deref().and_then(ModelProfile::parse) {
        config.profile = profile;
    }
    config.max_output_tokens = Some(cli.max_output_tokens);

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
        return Ok(());
    }

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
