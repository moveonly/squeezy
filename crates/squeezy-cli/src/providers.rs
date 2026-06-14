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
    BASE_PROVIDER_METADATA, OpenAiCompatiblePreset, Result, SqueezyError, bedrock_configured,
    canonical_provider_name, env_value_set,
};
use squeezy_llm::{ModelInfo, github_copilot_auth_file_path, models_for_provider};

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
        if args.configured {
            println!(
                "(no providers configured yet) — set an API key with `squeezy auth set <provider>` or add an inline api_key in settings.toml"
            );
        } else {
            println!("(no providers match the filter)");
        }
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
        let caps = &model.capabilities;
        let ctx = model
            .limits
            .map(|l| l.context_window_tokens.to_string())
            .unwrap_or_else(|| "?".into());
        let cap_flags: Vec<&str> = [
            caps.tool_calling.then_some("tools"),
            caps.vision.then_some("vision"),
            caps.reasoning_tokens.then_some("reasoning"),
            caps.reasoning_effort.then_some("effort"),
            caps.prompt_caching.then_some("cache"),
            caps.json_mode.then_some("json"),
            caps.response_state.then_some("state"),
        ]
        .into_iter()
        .flatten()
        .collect();
        println!(
            "    {} (profile={:?}, ctx={}, caps=[{}])",
            model.id,
            model.profile,
            ctx,
            cap_flags.join(","),
        );
    }
    Ok(())
}

fn model_json(model: &ModelInfo) -> Value {
    let caps = &model.capabilities;
    json!({
        "id": model.id,
        "profile": format!("{:?}", model.profile),
        "lifecycle": model.lifecycle.as_str(),
        "context_window": model.limits.map(|l| l.context_window_tokens),
        "max_output_tokens": model.limits.map(|l| l.max_output_tokens),
        "pricing": model.pricing.map(|p| json!({
            "input_usd_micros_per_mtok": p.input_usd_micros_per_mtok,
            "output_usd_micros_per_mtok": p.output_usd_micros_per_mtok,
            "cache_read_usd_micros_per_mtok": p.cache_read_usd_micros_per_mtok,
            "cache_write_usd_micros_per_mtok": p.cache_write_usd_micros_per_mtok,
        })),
        "capabilities": {
            "streaming": caps.streaming,
            "tool_calling": caps.tool_calling,
            "json_mode": caps.json_mode,
            "vision": caps.vision,
            "response_state": caps.response_state,
            "reasoning_tokens": caps.reasoning_tokens,
            "reasoning_effort": caps.reasoning_effort,
            "text_verbosity": caps.text_verbosity,
            "prompt_caching": caps.prompt_caching,
        },
    })
}

fn non_empty(value: &str) -> &str {
    if value.is_empty() { "(unset)" } else { value }
}

/// Resolve user-typed provider names to the canonical registry id. Accepts
/// `OpenAiCompatiblePreset` aliases (e.g. `grok` → `xai`) so users don't have
/// to memorise the snake_case form.
fn canonicalize_provider_name(value: &str) -> Option<&'static str> {
    canonical_provider_name(value)
}

pub(crate) fn registry_entries(env_lookup: &dyn Fn(&str) -> Option<String>) -> Vec<ProviderEntry> {
    let presets = OpenAiCompatiblePreset::all();
    let mut entries = Vec::with_capacity(BASE_PROVIDER_METADATA.len() + presets.len());
    entries.extend(BASE_PROVIDER_METADATA.iter().map(|entry| ProviderEntry {
        name: entry.name,
        display_name: entry.display_name,
        base_url: entry.base_url,
        api_key_env: entry.api_key_env,
        configured: if entry.name == "github_copilot" {
            github_copilot_auth_file_path().is_some_and(|path| path.exists())
        } else if entry.name == "bedrock" {
            // Bedrock uses the full AWS credential chain. Accept static access
            // keys, named profiles, IRSA/OIDC web-identity tokens, or a
            // dedicated bearer token. Any of these env vars signals a configured
            // deployment; profile-based and IAM-instance-role auth have no env
            // var at all — `squeezy doctor --probe` gives a live verdict.
            bedrock_configured(env_lookup)
        } else if entry.name == "ollama" {
            // Ollama operates without auth by default; treat it as always
            // configured. Users who set OLLAMA_API_KEY get a bearer token
            // attached, but the absence of a key does not block usage.
            true
        } else {
            env_value_set(env_lookup, entry.api_key_env)
        },
        full_tier: true,
        model_count: models_for_provider(entry.name).count(),
    }));
    for preset in presets {
        entries.push(ProviderEntry {
            name: preset.as_str(),
            display_name: preset.display_name(),
            base_url: preset.default_base_url(),
            api_key_env: preset.default_api_key_env(),
            configured: env_value_set(env_lookup, preset.default_api_key_env()),
            full_tier: preset.is_full_tier(),
            model_count: models_for_provider(preset.as_str()).count(),
        });
    }
    entries
}

#[cfg(test)]
#[path = "providers_tests.rs"]
mod tests;
