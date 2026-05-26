use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::path::PathBuf;

use clap::{Args, Subcommand};
use squeezy_core::{
    SqueezyError, load_separated_settings_sources,
    settings_writer::{EditOp, SettingsEdit, SettingsScope, apply_edits},
};

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    #[command(about = "Store a provider API key as inline `api_key` in the project-local TOML")]
    Set(AuthSetArgs),
}

#[derive(Debug, Args)]
pub struct AuthSetArgs {
    /// Provider id (openai, anthropic, google, azure, …, openrouter, portkey).
    pub provider: String,
    /// API key value. If omitted, read from stdin so it isn't captured in
    /// shell history.
    #[arg(long, help = "Inline API key value (otherwise read from stdin)")]
    pub value: Option<String>,
    /// Write to `~/.squeezy/settings.toml` instead of the project-local
    /// `~/.squeezy/projects/<slug>/settings.toml`. The committed repo
    /// `./squeezy.toml` is never a valid target — keys do not belong in
    /// version control.
    #[arg(
        long,
        help = "Save to the user-level settings TOML instead of project-local"
    )]
    pub user: bool,
}

pub fn handle_auth_command(command: &AuthCommand) -> squeezy_core::Result<()> {
    match command {
        AuthCommand::Set(args) => handle_auth_set(args, read_api_key_from_stdin),
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

/// Map a CLI-supplied provider id to its `[providers.<section>]` TOML
/// section name. Bedrock and Ollama have no single inline key — we surface
/// that as an actionable error instead of writing nothing.
pub(crate) fn provider_section_for(provider: &str) -> squeezy_core::Result<&'static str> {
    match provider {
        "openai" => Ok("openai"),
        "anthropic" | "claude" => Ok("anthropic"),
        "google" | "gemini" => Ok("google"),
        "azure" | "azure-openai" | "azure_openai" => Ok("azure_openai"),
        "bedrock" => Err(SqueezyError::Config(
            "bedrock uses the AWS default credential chain; configure credentials with aws configure"
                .to_string(),
        )),
        "ollama" | "local" => Err(SqueezyError::Config(
            "ollama runs locally and does not require an API key".to_string(),
        )),
        // OpenAI-compatible presets reuse the same name as both the CLI
        // provider id and the TOML section, so pass them through.
        other => Ok(static_section_name(other)),
    }
}

fn static_section_name(provider: &str) -> &'static str {
    // The set of OpenAI-compatible preset names is closed; resolve each to
    // a literal &'static str instead of leaking the heap string.
    match provider {
        "openrouter" => "openrouter",
        "vercel" => "vercel",
        "portkey" => "portkey",
        "groq" => "groq",
        "xai" => "xai",
        "deepseek" => "deepseek",
        "vertex" => "vertex",
        "mistral" => "mistral",
        "together" => "together",
        "fireworks" => "fireworks",
        "cerebras" => "cerebras",
        "openai_compatible" => "openai_compatible",
        // Last-resort: fail closed if the caller passed something we
        // can't statically map. A future provider should be wired into
        // this match rather than silently leaking memory.
        _ => "",
    }
}

pub(crate) fn handle_auth_set(
    args: &AuthSetArgs,
    read_stdin: impl Fn() -> squeezy_core::Result<String>,
) -> squeezy_core::Result<()> {
    let sources = load_separated_settings_sources()?;
    let target_path = if args.user {
        sources.user_path_default
    } else {
        sources.repo_path_default
    };
    handle_auth_set_at_path(args, target_path, args.user, read_stdin)
}

/// Test-friendly variant: the path resolution is hoisted to the caller so
/// unit tests can point at a tempdir.
pub(crate) fn handle_auth_set_at_path(
    args: &AuthSetArgs,
    target_path: PathBuf,
    user_scope: bool,
    read_stdin: impl Fn() -> squeezy_core::Result<String>,
) -> squeezy_core::Result<()> {
    let section = provider_section_for(&args.provider)?;
    if section.is_empty() {
        return Err(SqueezyError::Config(format!(
            "unknown provider {}; pass a known provider id (openai, anthropic, portkey, …)",
            args.provider
        )));
    }
    let value = match &args.value {
        Some(value) => value.clone(),
        None => read_stdin()?,
    };
    if value.trim().is_empty() {
        return Err(SqueezyError::Config(
            "API key must not be empty".to_string(),
        ));
    }
    let scope_target = if user_scope {
        SettingsScope::user(target_path.clone())
    } else {
        SettingsScope::repo(target_path.clone())
    };
    let edit = SettingsEdit {
        path: &[],
        op: EditOp::SetTableEntry {
            table_path: &["providers"],
            key: section.to_string(),
            fields: vec![("api_key", EditOp::SetString(value.trim().to_string()))],
        },
    };
    apply_edits(&scope_target, &[edit]).map_err(|err| {
        SqueezyError::Config(format!("failed to write {}: {err}", target_path.display()))
    })?;
    println!(
        "saved api key for {} to {}",
        args.provider,
        target_path.display()
    );
    Ok(())
}

#[cfg(test)]
#[path = "auth_tests.rs"]
mod tests;
