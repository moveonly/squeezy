use std::io::{self, BufRead, BufReader, IsTerminal, Write};

use clap::{Args, Subcommand};
use squeezy_core::SqueezyError;
use squeezy_llm::{DefaultCredentialStore, KeyringCredentialStore, save_api_key_with_store};

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    #[command(about = "Store a provider API key in the OS keyring")]
    Set(AuthSetArgs),
}

#[derive(Debug, Args)]
pub struct AuthSetArgs {
    /// Provider id (openai, anthropic, google, azure, bedrock).
    pub provider: String,
    /// API key value. If omitted, read from stdin so it isn't captured in shell history.
    #[arg(long, help = "Inline API key value (otherwise read from stdin)")]
    pub value: Option<String>,
    /// Override the env var name used as the keyring account; defaults to the provider's
    /// canonical env var (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, ...).
    #[arg(long, help = "Override the env var / keyring account name")]
    pub env: Option<String>,
}

pub fn handle_auth_command(command: &AuthCommand) -> squeezy_core::Result<()> {
    match command {
        AuthCommand::Set(args) => {
            handle_auth_set_with_store(args, &DefaultCredentialStore, read_api_key_from_stdin)
        }
    }
}

fn read_api_key_from_stdin() -> squeezy_core::Result<String> {
    if io::stdin().is_terminal() {
        eprint!("API key: ");
        let _ = io::stderr().flush();
    }
    let mut reader = BufReader::new(io::stdin());
    let mut buffer = String::new();
    reader
        .read_line(&mut buffer)
        .map_err(|err| SqueezyError::Config(format!("failed to read API key from stdin: {err}")))?;
    Ok(buffer.trim().to_string())
}

pub(crate) fn provider_env_var_for(provider: &str) -> squeezy_core::Result<&'static str> {
    match provider {
        "openai" => Ok("OPENAI_API_KEY"),
        "anthropic" => Ok("ANTHROPIC_API_KEY"),
        "google" | "gemini" => Ok("GOOGLE_API_KEY"),
        "azure" | "azure-openai" | "azure_openai" => Ok("AZURE_OPENAI_API_KEY"),
        "bedrock" => Err(SqueezyError::Config(
            "bedrock uses the AWS default credential chain; configure credentials with aws configure"
                .to_string(),
        )),
        "ollama" | "local" => Err(SqueezyError::Config(
            "ollama runs locally and does not require an API key".to_string(),
        )),
        other => Err(SqueezyError::Config(format!(
            "unknown provider {other}; pass --env to override the keyring account name"
        ))),
    }
}

pub(crate) fn handle_auth_set_with_store(
    args: &AuthSetArgs,
    store: &dyn KeyringCredentialStore,
    read_stdin: impl Fn() -> squeezy_core::Result<String>,
) -> squeezy_core::Result<()> {
    let env_var = match &args.env {
        Some(env) => env.clone(),
        None => provider_env_var_for(&args.provider)?.to_string(),
    };
    let value = match &args.value {
        Some(value) => value.clone(),
        None => read_stdin()?,
    };
    save_api_key_with_store(&env_var, &value, store)?;
    println!("stored api key for {} in OS keyring", args.provider);
    Ok(())
}

#[cfg(test)]
#[path = "auth_tests.rs"]
mod tests;
