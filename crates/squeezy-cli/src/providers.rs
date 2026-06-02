//! `squeezy providers list` and `squeezy providers info <name>` subcommands.
//!
//! Surfaces the full provider registry — the six first-party providers plus
//! every `OpenAiCompatiblePreset` — and reports each entry's base URL, API-key
//! env var, configured state, and model count. The plain `--list-providers`
//! root flag only prints provider ids, which is too thin once the registry
//! crossed a dozen entries; the audit ticket asked for a discoverable,
//! JSON-friendly surface.
use std::env;

use clap::{Args, Subcommand};
use serde::Serialize;
use serde_json::{Value, json};
use squeezy_core::{
    DEFAULT_ANTHROPIC_BASE_URL, DEFAULT_BEDROCK_REGION, DEFAULT_GOOGLE_BASE_URL,
    DEFAULT_OLLAMA_BASE_URL, DEFAULT_OPENAI_BASE_URL, OpenAiCompatiblePreset, Result, SqueezyError,
};
use squeezy_llm::{ModelInfo, models_for_provider};

#[derive(Debug, Subcommand)]
pub enum ProvidersCommand {
    #[command(about = "List every known provider with its base URL, env var, and configured state")]
    List(ProvidersListArgs),
    #[command(about = "Show details for a single provider, including its model catalog")]
    Info(ProvidersInfoArgs),
}

#[derive(Debug, Args)]
pub struct ProvidersListArgs {
    #[arg(long, help = "Emit machine-readable JSON instead of the human table")]
    pub json: bool,
    #[arg(
        long,
        help = "Show only providers whose default API-key env var is set"
    )]
    pub configured: bool,
}

#[derive(Debug, Args)]
pub struct ProvidersInfoArgs {
    pub name: String,
    #[arg(long, help = "Emit machine-readable JSON instead of the human table")]
    pub json: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ProviderEntry {
    pub(crate) name: &'static str,
    pub(crate) display_name: &'static str,
    pub(crate) base_url: &'static str,
    pub(crate) api_key_env: &'static str,
    pub(crate) configured: bool,
    pub(crate) full_tier: bool,
    pub(crate) model_count: usize,
}

pub fn handle_providers_command(command: &ProvidersCommand) -> Result<()> {
    match command {
        ProvidersCommand::List(args) => handle_list(args, &|name| env::var(name).ok()),
        ProvidersCommand::Info(args) => handle_info(args, &|name| env::var(name).ok()),
    }
}

fn handle_list(
    args: &ProvidersListArgs,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<()> {
    let entries: Vec<ProviderEntry> = registry_entries(env_lookup)
        .into_iter()
        .filter(|entry| !args.configured || entry.configured)
        .collect();
    if args.json {
        let body = json!({ "providers": entries });
        println!(
            "{}",
            serde_json::to_string_pretty(&body)
                .map_err(|err| SqueezyError::Config(err.to_string()))?
        );
        return Ok(());
    }
    if entries.is_empty() {
        println!("(no providers match the filter)");
        return Ok(());
    }
    let name_w = entries
        .iter()
        .map(|e| e.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let env_w = entries
        .iter()
        .map(|e| e.api_key_env.len())
        .max()
        .unwrap_or(7)
        .max(7);
    println!(
        "{:<name_w$}  {:<env_w$}  {:<10}  models  base_url",
        "name", "env", "configured",
    );
    for entry in &entries {
        println!(
            "{:<name_w$}  {:<env_w$}  {:<10}  {:>6}  {}",
            entry.name,
            non_empty(entry.api_key_env),
            if entry.configured { "yes" } else { "no" },
            entry.model_count,
            non_empty(entry.base_url),
        );
    }
    Ok(())
}

fn handle_info(
    args: &ProvidersInfoArgs,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<()> {
    let canonical = canonicalize_provider_name(&args.name).ok_or_else(|| {
        SqueezyError::Config(format!(
            "providers info: unknown provider {:?} (run `squeezy providers list`)",
            args.name,
        ))
    })?;
    let entry = registry_entries(env_lookup)
        .into_iter()
        .find(|entry| entry.name == canonical)
        .expect("canonical name resolved from same registry");
    if args.json {
        let models: Vec<_> = models_for_provider(canonical).map(model_json).collect();
        let body = json!({
            "name": entry.name,
            "display_name": entry.display_name,
            "base_url": entry.base_url,
            "api_key_env": entry.api_key_env,
            "configured": entry.configured,
            "full_tier": entry.full_tier,
            "model_count": entry.model_count,
            "models": models,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&body)
                .map_err(|err| SqueezyError::Config(err.to_string()))?
        );
        return Ok(());
    }
    println!("{} ({})", entry.display_name, entry.name);
    println!("  base_url    {}", non_empty(entry.base_url));
    println!("  api_key_env {}", non_empty(entry.api_key_env));
    println!(
        "  configured  {}",
        if entry.configured { "yes" } else { "no" }
    );
    println!("  models      {}", entry.model_count);
    for model in models_for_provider(canonical) {
        println!(
            "    {} (profile={:?}, tools={}, ctx={})",
            model.id,
            model.profile,
            model.capabilities.tool_calling,
            model
                .limits
                .map(|limits| limits.context_window_tokens)
                .map(|v| v.to_string())
                .unwrap_or_else(|| "?".into()),
        );
    }
    Ok(())
}

fn model_json(model: &ModelInfo) -> Value {
    json!({
        "id": model.id,
        "profile": format!("{:?}", model.profile),
        "tool_calling": model.capabilities.tool_calling,
        "context_window": model.limits.map(|l| l.context_window_tokens),
        "lifecycle": model.lifecycle.as_str(),
    })
}

fn non_empty(value: &str) -> &str {
    if value.is_empty() { "(unset)" } else { value }
}

/// Resolve user-typed provider names to the canonical registry id. Accepts
/// `OpenAiCompatiblePreset` aliases (e.g. `grok` → `xai`) so users don't have
/// to memorise the snake_case form.
fn canonicalize_provider_name(value: &str) -> Option<&'static str> {
    let trimmed = value.trim();
    let lower = trimmed.to_ascii_lowercase();
    for entry in BASE_PROVIDERS {
        if entry.aliases.iter().any(|alias| *alias == lower) {
            return Some(entry.name);
        }
    }
    OpenAiCompatiblePreset::parse(trimmed).map(|preset| preset.as_str())
}

pub(crate) fn registry_entries(env_lookup: &dyn Fn(&str) -> Option<String>) -> Vec<ProviderEntry> {
    let presets = OpenAiCompatiblePreset::all();
    let mut entries = Vec::with_capacity(BASE_PROVIDERS.len() + presets.len());
    entries.extend(BASE_PROVIDERS.iter().map(|entry| ProviderEntry {
        name: entry.name,
        display_name: entry.display_name,
        base_url: entry.base_url,
        api_key_env: entry.api_key_env,
        configured: env_set(env_lookup, entry.api_key_env),
        full_tier: true,
        model_count: models_for_provider(entry.name).count(),
    }));
    for preset in presets {
        entries.push(ProviderEntry {
            name: preset.as_str(),
            display_name: preset.display_name(),
            base_url: preset.default_base_url(),
            api_key_env: preset.default_api_key_env(),
            configured: env_set(env_lookup, preset.default_api_key_env()),
            full_tier: preset.is_full_tier(),
            model_count: models_for_provider(preset.as_str()).count(),
        });
    }
    entries
}

fn env_set(env_lookup: &dyn Fn(&str) -> Option<String>, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    env_lookup(name)
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}

struct BaseProvider {
    name: &'static str,
    display_name: &'static str,
    base_url: &'static str,
    api_key_env: &'static str,
    aliases: &'static [&'static str],
}

const BASE_PROVIDERS: &[BaseProvider] = &[
    BaseProvider {
        name: "openai",
        display_name: "OpenAI",
        base_url: DEFAULT_OPENAI_BASE_URL,
        api_key_env: "OPENAI_API_KEY",
        aliases: &["openai", "open_ai"],
    },
    BaseProvider {
        name: "anthropic",
        display_name: "Anthropic",
        base_url: DEFAULT_ANTHROPIC_BASE_URL,
        api_key_env: "ANTHROPIC_API_KEY",
        aliases: &["anthropic", "claude"],
    },
    BaseProvider {
        name: "google",
        display_name: "Google AI Studio",
        base_url: DEFAULT_GOOGLE_BASE_URL,
        api_key_env: "GOOGLE_API_KEY",
        aliases: &["google", "gemini", "google_ai", "google_ai_studio"],
    },
    BaseProvider {
        name: "azure_openai",
        display_name: "Azure OpenAI",
        // Per-deployment URL; no useful constant default.
        base_url: "",
        api_key_env: "AZURE_OPENAI_API_KEY",
        aliases: &["azure_openai", "azure", "azure_ai"],
    },
    BaseProvider {
        name: "bedrock",
        display_name: "AWS Bedrock",
        // Bedrock's "base URL" is a region; surface it under the same column
        // so the table stays uniform.
        base_url: DEFAULT_BEDROCK_REGION,
        api_key_env: "AWS_ACCESS_KEY_ID",
        aliases: &["bedrock", "aws_bedrock", "aws"],
    },
    BaseProvider {
        name: "ollama",
        display_name: "Ollama",
        base_url: DEFAULT_OLLAMA_BASE_URL,
        // Ollama doesn't require auth by default; show the documented env var
        // for users running behind a reverse proxy.
        api_key_env: "OLLAMA_API_KEY",
        aliases: &["ollama"],
    },
];

#[cfg(test)]
#[path = "providers_tests.rs"]
mod tests;
