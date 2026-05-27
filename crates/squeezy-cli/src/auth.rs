use std::collections::BTreeMap;
use std::env;
use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};
use squeezy_core::{
    SeparatedSources, SqueezyError, load_separated_settings_sources,
    settings_writer::{EditOp, SettingsEdit, SettingsScope, apply_edits},
};

/// Every `[providers.<section>]` name that can carry an inline `api_key`,
/// paired with the CLI alias used in error messages. Order is the
/// canonical listing for `auth list` / `auth status` without a provider.
const KNOWN_PROVIDERS: &[KnownProvider] = &[
    KnownProvider {
        section: "openai",
        cli: "openai",
        env: "SQUEEZY_OPENAI_KEY",
        fallback_env: Some("OPENAI_API_KEY"),
    },
    KnownProvider {
        section: "anthropic",
        cli: "anthropic",
        env: "SQUEEZY_ANTHROPIC_KEY",
        fallback_env: Some("ANTHROPIC_API_KEY"),
    },
    KnownProvider {
        section: "google",
        cli: "google",
        env: "SQUEEZY_GOOGLE_KEY",
        fallback_env: Some("GOOGLE_API_KEY"),
    },
    KnownProvider {
        section: "azure_openai",
        cli: "azure",
        env: "SQUEEZY_AZURE_OPENAI_KEY",
        fallback_env: Some("AZURE_OPENAI_API_KEY"),
    },
    KnownProvider {
        section: "openrouter",
        cli: "openrouter",
        env: "SQUEEZY_OPENROUTER_KEY",
        fallback_env: Some("OPENROUTER_API_KEY"),
    },
    KnownProvider {
        section: "vercel",
        cli: "vercel",
        env: "SQUEEZY_VERCEL_KEY",
        fallback_env: Some("AI_GATEWAY_API_KEY"),
    },
    KnownProvider {
        section: "portkey",
        cli: "portkey",
        env: "SQUEEZY_PORTKEY_KEY",
        fallback_env: Some("PORTKEY_API_KEY"),
    },
    KnownProvider {
        section: "groq",
        cli: "groq",
        env: "SQUEEZY_GROQ_KEY",
        fallback_env: Some("GROQ_API_KEY"),
    },
    KnownProvider {
        section: "xai",
        cli: "xai",
        env: "SQUEEZY_XAI_KEY",
        fallback_env: Some("XAI_API_KEY"),
    },
    KnownProvider {
        section: "deepseek",
        cli: "deepseek",
        env: "SQUEEZY_DEEPSEEK_KEY",
        fallback_env: Some("DEEPSEEK_API_KEY"),
    },
    KnownProvider {
        section: "vertex",
        cli: "vertex",
        env: "SQUEEZY_VERTEX_KEY",
        fallback_env: Some("VERTEX_ACCESS_TOKEN"),
    },
    KnownProvider {
        section: "mistral",
        cli: "mistral",
        env: "SQUEEZY_MISTRAL_KEY",
        fallback_env: Some("MISTRAL_API_KEY"),
    },
    KnownProvider {
        section: "together",
        cli: "together",
        env: "SQUEEZY_TOGETHER_KEY",
        fallback_env: Some("TOGETHER_API_KEY"),
    },
    KnownProvider {
        section: "fireworks",
        cli: "fireworks",
        env: "SQUEEZY_FIREWORKS_KEY",
        fallback_env: Some("FIREWORKS_API_KEY"),
    },
    KnownProvider {
        section: "cerebras",
        cli: "cerebras",
        env: "SQUEEZY_CEREBRAS_KEY",
        fallback_env: Some("CEREBRAS_API_KEY"),
    },
    // Local self-hosted OpenAI-compatible servers. They typically run without
    // authentication on a loopback port; the inline-key slot exists so users
    // can stand up a reverse proxy that requires a bearer token.
    KnownProvider {
        section: "lmstudio",
        cli: "lmstudio",
        env: "SQUEEZY_LMSTUDIO_KEY",
        fallback_env: Some("LMSTUDIO_API_KEY"),
    },
    KnownProvider {
        section: "vllm",
        cli: "vllm",
        env: "SQUEEZY_VLLM_KEY",
        fallback_env: Some("VLLM_API_KEY"),
    },
    KnownProvider {
        section: "llamacpp",
        cli: "llamacpp",
        env: "SQUEEZY_LLAMACPP_KEY",
        fallback_env: Some("LLAMACPP_API_KEY"),
    },
    KnownProvider {
        section: "cloudflare_workers_ai",
        cli: "cloudflare_workers_ai",
        env: "SQUEEZY_CLOUDFLARE_WORKERS_AI_KEY",
        fallback_env: Some("CLOUDFLARE_API_KEY"),
    },
    KnownProvider {
        section: "cloudflare_ai_gateway",
        cli: "cloudflare_ai_gateway",
        env: "SQUEEZY_CLOUDFLARE_AI_GATEWAY_KEY",
        fallback_env: Some("CLOUDFLARE_API_KEY"),
    },
    KnownProvider {
        section: "openai_compatible",
        cli: "openai_compatible",
        env: "SQUEEZY_OPENAI_COMPATIBLE_KEY",
        fallback_env: None,
    },
];

#[derive(Debug, Clone, Copy)]
struct KnownProvider {
    section: &'static str,
    cli: &'static str,
    env: &'static str,
    fallback_env: Option<&'static str>,
}

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    #[command(about = "Store a provider API key as inline `api_key` in the project-local TOML")]
    Set(AuthSetArgs),
    #[command(
        about = "List providers with a stored inline `api_key` across user and project TOMLs"
    )]
    List(AuthListArgs),
    #[command(about = "Remove the inline `api_key` for a provider from the project-local TOML")]
    Remove(AuthRemoveArgs),
    #[command(about = "Report which providers have a key (inline or env) and where it resolves")]
    Status(AuthStatusArgs),
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

#[derive(Debug, Args, Default)]
pub struct AuthListArgs {
    /// Emit the list as JSON for scripting.
    #[arg(long, help = "Emit the list as JSON")]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct AuthRemoveArgs {
    /// Provider id (openai, anthropic, google, azure, …, openrouter, portkey).
    pub provider: String,
    /// Remove from `~/.squeezy/settings.toml` instead of the project-local
    /// `~/.squeezy/projects/<slug>/settings.toml`.
    #[arg(
        long,
        help = "Edit the user-level settings TOML instead of project-local"
    )]
    pub user: bool,
}

#[derive(Debug, Args)]
pub struct AuthStatusArgs {
    /// Optional provider id. When omitted, every known provider is listed.
    pub provider: Option<String>,
    /// Emit the status as JSON for scripting.
    #[arg(long, help = "Emit the status as JSON")]
    pub json: bool,
}

pub fn handle_auth_command(command: &AuthCommand) -> squeezy_core::Result<()> {
    match command {
        AuthCommand::Set(args) => handle_auth_set(args, read_api_key_from_stdin),
        AuthCommand::List(args) => handle_auth_list(args),
        AuthCommand::Remove(args) => handle_auth_remove(args),
        AuthCommand::Status(args) => handle_auth_status(args),
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
        "lmstudio" => "lmstudio",
        "vllm" => "vllm",
        "llamacpp" => "llamacpp",
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

fn handle_auth_list(args: &AuthListArgs) -> squeezy_core::Result<()> {
    let sources = load_separated_settings_sources()?;
    let entries = collect_inline_keys(&sources);
    if args.json {
        let json = serde_json::to_string_pretty(&entries.to_json())
            .map_err(|err| SqueezyError::Config(format!("failed to serialize auth list: {err}")))?;
        println!("{json}");
    } else {
        print_inline_keys_table(&entries);
    }
    Ok(())
}

fn handle_auth_remove(args: &AuthRemoveArgs) -> squeezy_core::Result<()> {
    let sources = load_separated_settings_sources()?;
    let target_path = if args.user {
        sources.user_path_default
    } else {
        sources.repo_path_default
    };
    handle_auth_remove_at_path(args, target_path, args.user)
}

pub(crate) fn handle_auth_remove_at_path(
    args: &AuthRemoveArgs,
    target_path: PathBuf,
    user_scope: bool,
) -> squeezy_core::Result<()> {
    let section = provider_section_for(&args.provider)?;
    if section.is_empty() {
        return Err(SqueezyError::Config(format!(
            "unknown provider {}; pass a known provider id (openai, anthropic, portkey, …)",
            args.provider
        )));
    }
    if !tier_has_inline_key(&target_path, section) {
        return Err(SqueezyError::Config(format!(
            "no inline api_key for {} found in {}",
            args.provider,
            target_path.display()
        )));
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
            fields: vec![("api_key", EditOp::Unset)],
        },
    };
    apply_edits(&scope_target, &[edit]).map_err(|err| {
        SqueezyError::Config(format!("failed to write {}: {err}", target_path.display()))
    })?;
    println!(
        "removed api key for {} from {}",
        args.provider,
        target_path.display()
    );
    Ok(())
}

fn handle_auth_status(args: &AuthStatusArgs) -> squeezy_core::Result<()> {
    let sources = load_separated_settings_sources()?;
    handle_auth_status_with_env(args, &sources, &|name| env::var(name).ok())
}

pub(crate) fn handle_auth_status_with_env(
    args: &AuthStatusArgs,
    sources: &SeparatedSources,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> squeezy_core::Result<()> {
    let providers = match &args.provider {
        Some(provider) => {
            let section = provider_section_for(provider)?;
            if section.is_empty() {
                return Err(SqueezyError::Config(format!(
                    "unknown provider {}; pass a known provider id (openai, anthropic, portkey, …)",
                    provider
                )));
            }
            let known = KNOWN_PROVIDERS
                .iter()
                .find(|p| p.section == section)
                .copied()
                .ok_or_else(|| {
                    SqueezyError::Config(format!(
                        "no status view for provider {}; unknown section {}",
                        provider, section
                    ))
                })?;
            vec![known]
        }
        None => KNOWN_PROVIDERS.to_vec(),
    };
    let rows: Vec<StatusRow> = providers
        .into_iter()
        .map(|p| compute_status_row(p, sources, env_lookup))
        .collect();
    if args.json {
        let json =
            serde_json::to_string_pretty(&rows.iter().map(StatusRow::to_json).collect::<Vec<_>>())
                .map_err(|err| {
                    SqueezyError::Config(format!("failed to serialize status: {err}"))
                })?;
        println!("{json}");
    } else {
        print_status_table(&rows);
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct InlineKeyEntry {
    provider: String,
    tier: TierLabel,
    path: PathBuf,
    redacted: String,
}

#[derive(Debug, Clone, Copy)]
enum TierLabel {
    User,
    Project,
    Repo,
}

impl TierLabel {
    fn as_str(self) -> &'static str {
        match self {
            TierLabel::User => "user",
            TierLabel::Project => "project",
            TierLabel::Repo => "local",
        }
    }
}

#[derive(Debug, Default)]
struct InlineKeyList {
    entries: Vec<InlineKeyEntry>,
}

impl InlineKeyList {
    fn to_json(&self) -> serde_json::Value {
        serde_json::Value::Array(
            self.entries
                .iter()
                .map(|entry| {
                    serde_json::json!({
                        "provider": entry.provider,
                        "tier": entry.tier.as_str(),
                        "path": entry.path.display().to_string(),
                        "redacted": entry.redacted,
                    })
                })
                .collect(),
        )
    }
}

fn collect_inline_keys(sources: &SeparatedSources) -> InlineKeyList {
    let mut entries: Vec<InlineKeyEntry> = Vec::new();
    let tiers: [(Option<&squeezy_core::TierSource>, TierLabel); 3] = [
        (sources.user.as_ref(), TierLabel::User),
        (sources.project.as_ref(), TierLabel::Project),
        (sources.repo.as_ref(), TierLabel::Repo),
    ];
    for (tier, label) in tiers {
        let Some(tier) = tier else { continue };
        let inline = extract_inline_keys_from_doc(&tier.doc);
        for (section, value) in inline {
            entries.push(InlineKeyEntry {
                provider: section,
                tier: label,
                path: tier.path.clone(),
                redacted: redact_secret(&value),
            });
        }
    }
    InlineKeyList { entries }
}

fn extract_inline_keys_from_doc(doc: &toml_edit::DocumentMut) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    let providers = match doc.as_table().get("providers") {
        Some(toml_edit::Item::Table(t)) => t,
        _ => return out,
    };
    for (section_name, item) in providers.iter() {
        // A provider section may be a [providers.foo] table or an inline
        // `foo = { api_key = "..." }` table value; treat both shapes.
        let api_key_str: Option<String> = match item {
            toml_edit::Item::Table(t) => match t.get("api_key") {
                Some(toml_edit::Item::Value(toml_edit::Value::String(s))) => {
                    Some(s.value().to_string())
                }
                _ => None,
            },
            toml_edit::Item::Value(toml_edit::Value::InlineTable(t)) => match t.get("api_key") {
                Some(toml_edit::Value::String(s)) => Some(s.value().to_string()),
                _ => None,
            },
            _ => None,
        };
        if let Some(value) = api_key_str
            && !value.trim().is_empty()
        {
            out.insert(section_name.to_string(), value);
        }
    }
    out
}

fn tier_has_inline_key(path: &Path, section: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(doc) = text.parse::<toml_edit::DocumentMut>() else {
        return false;
    };
    extract_inline_keys_from_doc(&doc).contains_key(section)
}

fn redact_secret(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return "<empty>".to_string();
    }
    let len = trimmed.chars().count();
    if len <= 8 {
        return "*".repeat(len);
    }
    let prefix: String = trimmed.chars().take(4).collect();
    let suffix: String = trimmed
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{prefix}…{suffix}")
}

fn print_inline_keys_table(list: &InlineKeyList) {
    if list.entries.is_empty() {
        println!("No inline provider api_key entries found in user, project, or local settings.");
        return;
    }
    let mut rows: Vec<[String; 4]> = Vec::with_capacity(list.entries.len() + 1);
    rows.push([
        "PROVIDER".to_string(),
        "TIER".to_string(),
        "KEY".to_string(),
        "PATH".to_string(),
    ]);
    for entry in &list.entries {
        rows.push([
            entry.provider.clone(),
            entry.tier.as_str().to_string(),
            entry.redacted.clone(),
            entry.path.display().to_string(),
        ]);
    }
    print_table_rows(&rows);
}

// Detail about where one provider's key resolves: which tier (if any)
// holds the inline value, and which env var (if any) is set. The CLI
// surface shows the highest-priority source first; the JSON form keeps
// every signal so scripts can decide for themselves.
#[derive(Debug, Clone)]
struct StatusRow {
    provider: &'static str,
    section: &'static str,
    inline_tier: Option<TierLabel>,
    inline_path: Option<PathBuf>,
    env_var: &'static str,
    env_set: bool,
    fallback_env_var: Option<&'static str>,
    fallback_env_set: bool,
}

impl StatusRow {
    fn effective_source(&self) -> &'static str {
        if self.inline_tier.is_some() {
            "inline"
        } else if self.env_set {
            "env"
        } else if self.fallback_env_set {
            "env (fallback)"
        } else {
            "missing"
        }
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "provider": self.provider,
            "section": self.section,
            "inline_tier": self.inline_tier.map(|t| t.as_str()),
            "inline_path": self.inline_path.as_ref().map(|p| p.display().to_string()),
            "env_var": self.env_var,
            "env_set": self.env_set,
            "fallback_env_var": self.fallback_env_var,
            "fallback_env_set": self.fallback_env_set,
            "effective_source": self.effective_source(),
        })
    }
}

fn compute_status_row(
    provider: KnownProvider,
    sources: &SeparatedSources,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> StatusRow {
    let tiers: [(Option<&squeezy_core::TierSource>, TierLabel); 3] = [
        // Highest precedence first: repo (per-machine local), then
        // project (./squeezy.toml), then user (~/.squeezy/settings.toml).
        // Matches `load_settings_from_paths` merge order; the last tier
        // to write `api_key` wins, so we report that tier here.
        (sources.repo.as_ref(), TierLabel::Repo),
        (sources.project.as_ref(), TierLabel::Project),
        (sources.user.as_ref(), TierLabel::User),
    ];
    let mut inline_tier = None;
    let mut inline_path = None;
    for (tier, label) in tiers {
        let Some(tier) = tier else { continue };
        if extract_inline_keys_from_doc(&tier.doc).contains_key(provider.section) {
            inline_tier = Some(label);
            inline_path = Some(tier.path.clone());
            break;
        }
    }
    let env_set = env_lookup(provider.env)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    let fallback_env_set = provider
        .fallback_env
        .and_then(env_lookup)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    StatusRow {
        provider: provider.cli,
        section: provider.section,
        inline_tier,
        inline_path,
        env_var: provider.env,
        env_set,
        fallback_env_var: provider.fallback_env,
        fallback_env_set,
    }
}

fn print_status_table(rows: &[StatusRow]) {
    if rows.is_empty() {
        println!("No providers to report.");
        return;
    }
    let mut grid: Vec<[String; 4]> = Vec::with_capacity(rows.len() + 1);
    grid.push([
        "PROVIDER".to_string(),
        "SOURCE".to_string(),
        "ENV".to_string(),
        "INLINE".to_string(),
    ]);
    for row in rows {
        let env_cell = if row.env_set {
            format!("{} (set)", row.env_var)
        } else if let Some(fallback) = row.fallback_env_var
            && row.fallback_env_set
        {
            format!("{} (fallback set)", fallback)
        } else {
            row.env_var.to_string()
        };
        let inline_cell = match (&row.inline_tier, &row.inline_path) {
            (Some(tier), Some(path)) => format!("{} ({})", tier.as_str(), path.display()),
            _ => "-".to_string(),
        };
        grid.push([
            row.provider.to_string(),
            row.effective_source().to_string(),
            env_cell,
            inline_cell,
        ]);
    }
    print_table_rows(&grid);
}

fn print_table_rows(rows: &[[String; 4]]) {
    let widths: Vec<usize> = (0..4)
        .map(|col| rows.iter().map(|row| row[col].len()).max().unwrap_or(0))
        .collect();
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

#[cfg(test)]
#[path = "auth_tests.rs"]
mod tests;
