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
        .map(|e| env_column_label(e).len())
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
            env_column_label(entry),
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
    println!("  api_key_env {}", env_column_label(&entry));
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

/// Label printed in the `env` column of `providers list` / `providers info`.
/// Bedrock and Ollama deliberately set `api_key_env = ""` (Bedrock uses the
/// AWS credential chain; Ollama runs unauthenticated by default), so the
/// generic `(unset)` sentinel from [`non_empty`] would mislead operators
/// into thinking the column is reporting a missing env var. Surface
/// `(none required)` for those two rows instead so the UX matches the
/// comments on the [`BASE_PROVIDERS`] entries.
fn env_column_label(entry: &ProviderEntry) -> &'static str {
    if entry.api_key_env.is_empty() {
        match entry.name {
            "bedrock" | "ollama" => "(none required)",
            _ => "(unset)",
        }
    } else {
        entry.api_key_env
    }
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
            env_set(env_lookup, entry.api_key_env)
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
            configured: env_set(env_lookup, preset.default_api_key_env()),
            full_tier: preset.is_full_tier(),
            model_count: models_for_provider(preset.as_str()).count(),
        });
    }
    entries
}

pub(crate) fn env_set(env_lookup: &dyn Fn(&str) -> Option<String>, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    env_lookup(name)
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}

/// AWS credential env vars that signal a configured Bedrock deployment.
/// Profile-based and IAM-instance-role auth do not surface as env vars at
/// all — this list only enumerates the cheap best-effort signals usable by
/// non-network checks. `squeezy doctor --probe` covers the rest.
pub(crate) const AWS_CRED_VARS: &[&str] = &[
    "AWS_ACCESS_KEY_ID",
    "AWS_PROFILE",
    "AWS_DEFAULT_PROFILE",
    "AWS_ROLE_ARN",
    "AWS_WEB_IDENTITY_TOKEN_FILE",
    "AWS_BEARER_TOKEN_BEDROCK",
    "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
    "AWS_CONTAINER_CREDENTIALS_FULL_URI",
];

/// Check whether any common AWS credential signal is present in the environment.
/// The full AWS credential chain (profile files, IMDSv2, IRSA, SSO) cannot be
/// probed with env vars alone — this is a best-effort check that covers the
/// most common CI and developer workstation cases. `squeezy doctor --probe`
/// gives a definitive live verdict. Shared between `providers list` and
/// `doctor` so the two stay in sync.
pub(crate) fn bedrock_configured(env_lookup: &dyn Fn(&str) -> Option<String>) -> bool {
    AWS_CRED_VARS.iter().any(|var| env_set(env_lookup, var))
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
        // Bedrock uses the full AWS credential chain: access keys, named
        // profiles, IAM instance roles, IRSA, and SSO/bearer tokens are all
        // valid — `AWS_ACCESS_KEY_ID` is just the key-id half of static
        // access-key auth and is absent in profile/IAM/IRSA deployments.
        // Report `BEDROCK_CONFIGURED` as an always-present placeholder so the
        // table does not mislead profile/IAM users into thinking Bedrock is
        // unconfigured. Use `squeezy doctor --probe` for a live credential check.
        api_key_env: "",
        aliases: &["bedrock", "aws_bedrock", "aws"],
    },
    BaseProvider {
        name: "ollama",
        display_name: "Ollama",
        base_url: DEFAULT_OLLAMA_BASE_URL,
        // Ollama runs locally without auth by default; treat it as always
        // configured so it doesn't appear unconfigured just because
        // OLLAMA_API_KEY is absent. Users running behind a reverse proxy
        // can set OLLAMA_API_KEY or providers.ollama.api_key in TOML.
        api_key_env: "",
        aliases: &["ollama"],
    },
    BaseProvider {
        name: "github_copilot",
        display_name: "GitHub Copilot",
        base_url: "(token-derived)",
        api_key_env: "squeezy auth github-copilot login",
        aliases: &["github_copilot", "github-copilot", "copilot"],
    },
];

#[cfg(test)]
#[path = "providers_tests.rs"]
mod tests;
