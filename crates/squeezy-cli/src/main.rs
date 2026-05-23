use std::{
    io::{self, Write},
    sync::Arc,
};

use clap::Parser;
use futures_util::StreamExt;
use squeezy_core::{AppConfig, ProviderConfig};
use squeezy_llm::{LlmEvent, LlmProvider, LlmRequest, OpenAiProvider, UnavailableProvider};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Parser)]
#[command(name = "squeezy", version, about = "Cost-aware coding agent TUI")]
struct Cli {
    #[arg(long, env = "SQUEEZY_MODEL", default_value = squeezy_core::DEFAULT_OPENAI_MODEL)]
    model: String,
    #[arg(long, default_value_t = squeezy_core::DEFAULT_MAX_OUTPUT_TOKENS)]
    max_output_tokens: u32,
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
    let mut config = AppConfig::from_env();
    config.model = cli.model;
    config.max_output_tokens = Some(cli.max_output_tokens);

    if cli.health {
        println!("squeezy: ok");
        return Ok(());
    }

    let provider = provider_from_config(&config);
    if let Some(prompt) = cli.prompt {
        return run_prompt(config, provider, prompt).await;
    }

    squeezy_tui::run(config, provider).await
}

async fn run_prompt(
    config: AppConfig,
    provider: Arc<dyn LlmProvider>,
    prompt: String,
) -> squeezy_core::Result<()> {
    let request = LlmRequest {
        model: config.model,
        instructions: config.instructions,
        input: prompt,
        max_output_tokens: config.max_output_tokens,
        previous_response_id: None,
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
            LlmEvent::Completed { cost, .. } => {
                writeln!(stdout)?;
                stdout.flush()?;
                eprintln!(
                    "tokens: input={} output={} cached={}",
                    format_token(cost.input_tokens),
                    format_token(cost.output_tokens),
                    format_token(cost.cached_input_tokens)
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

fn provider_from_config(config: &AppConfig) -> Arc<dyn LlmProvider> {
    match &config.provider {
        ProviderConfig::OpenAi(openai) => match OpenAiProvider::from_config(openai) {
            Ok(provider) => Arc::new(provider),
            Err(error) => Arc::new(UnavailableProvider::new("openai", error.to_string())),
        },
    }
}
