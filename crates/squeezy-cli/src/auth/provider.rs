use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};

use squeezy_core::{
    ProviderAuthMeta, SeparatedSources, SqueezyError, load_separated_settings_sources,
    provider_auth_for_section, provider_auth_metadata, provider_section_for_cli,
    settings_writer::{EditOp, SettingsEdit, SettingsScope, apply_edits},
};

use super::rendering::{print_inline_keys_table, print_status_table, redact_secret};
use super::{AuthListArgs, AuthRemoveArgs, AuthSetArgs, AuthStatusArgs};

/// Map a CLI-supplied provider id to its `[providers.<section>]` TOML
/// section name. Bedrock and Ollama have no single inline key — we surface
/// that as an actionable error instead of writing nothing.
pub(crate) fn provider_section_for(provider: &str) -> squeezy_core::Result<&'static str> {
    provider_section_for_cli(provider)
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

pub(super) fn handle_auth_list(args: &AuthListArgs) -> squeezy_core::Result<()> {
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

pub(super) fn handle_auth_remove(args: &AuthRemoveArgs) -> squeezy_core::Result<()> {
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

pub(super) fn handle_auth_status(args: &AuthStatusArgs) -> squeezy_core::Result<()> {
    let sources = load_separated_settings_sources()?;
    handle_auth_status_with_env(args, &sources, &|name| env::var(name).ok())
}

pub(crate) fn handle_auth_status_with_env(
    args: &AuthStatusArgs,
    sources: &SeparatedSources,
    env_lookup: &dyn Fn(&str) -> Option<String>,
) -> squeezy_core::Result<()> {
    let rows: Vec<StatusRow> = match &args.provider {
        Some(provider) => {
            let section = provider_section_for(provider)?;
            if section.is_empty() {
                return Err(SqueezyError::Config(format!(
                    "unknown provider {}; pass a known provider id (openai, anthropic, portkey, …)",
                    provider
                )));
            }
            let known = provider_auth_for_section(section).ok_or_else(|| {
                SqueezyError::Config(format!(
                    "no status view for provider {}; unknown section {}",
                    provider, section
                ))
            })?;
            vec![compute_status_row(known, sources, env_lookup)]
        }
        None => provider_auth_metadata()
            .into_iter()
            .map(|p| compute_status_row(p, sources, env_lookup))
            .collect(),
    };
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
pub(crate) struct InlineKeyEntry {
    pub(crate) provider: String,
    pub(crate) tier: TierLabel,
    pub(crate) path: PathBuf,
    pub(crate) redacted: String,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum TierLabel {
    User,
    Project,
    Repo,
}

impl TierLabel {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            TierLabel::User => "user",
            TierLabel::Project => "project",
            TierLabel::Repo => "local",
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct InlineKeyList {
    pub(crate) entries: Vec<InlineKeyEntry>,
}

impl InlineKeyList {
    pub(crate) fn to_json(&self) -> serde_json::Value {
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

pub(crate) fn collect_inline_keys(sources: &SeparatedSources) -> InlineKeyList {
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
        if let Some(value) = inline_api_key_value(item)
            && !value.trim().is_empty()
        {
            out.insert(section_name.to_string(), value.to_string());
        }
    }
    out
}

fn inline_api_key_value(item: &toml_edit::Item) -> Option<&str> {
    match item {
        toml_edit::Item::Table(t) => match t.get("api_key") {
            Some(toml_edit::Item::Value(toml_edit::Value::String(s))) => Some(s.value()),
            _ => None,
        },
        toml_edit::Item::Value(toml_edit::Value::InlineTable(t)) => match t.get("api_key") {
            Some(toml_edit::Value::String(s)) => Some(s.value()),
            _ => None,
        },
        _ => None,
    }
}

fn doc_has_inline_key(doc: &toml_edit::DocumentMut, section: &str) -> bool {
    let Some(toml_edit::Item::Table(providers)) = doc.as_table().get("providers") else {
        return false;
    };
    providers
        .get(section)
        .and_then(inline_api_key_value)
        .is_some_and(|value| !value.trim().is_empty())
}

fn tier_has_inline_key(path: &Path, section: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(doc) = text.parse::<toml_edit::DocumentMut>() else {
        return false;
    };
    doc_has_inline_key(&doc, section)
}

// Detail about where one provider's key resolves: which tier (if any)
// holds the inline value, and which env var (if any) is set. The CLI
// surface shows the highest-priority source first; the JSON form keeps
// every signal so scripts can decide for themselves.
#[derive(Debug, Clone)]
pub(super) struct StatusRow {
    pub(super) provider: &'static str,
    pub(super) section: &'static str,
    pub(super) inline_tier: Option<TierLabel>,
    pub(super) inline_path: Option<PathBuf>,
    pub(super) env_var: &'static str,
    pub(super) env_set: bool,
    pub(super) fallback_env_var: Option<&'static str>,
    pub(super) fallback_env_set: bool,
    /// Whether the inline key lives in a file-backed TOML tier (as
    /// opposed to an env var). On Windows this is reported as
    /// "file-backed" to distinguish it from a Credential Manager entry
    /// that does not exist yet.
    pub(super) credentials_file_set: bool,
}

impl StatusRow {
    pub(super) fn effective_source(&self) -> &'static str {
        if self.inline_tier.is_some() {
            "inline"
        } else if self.credentials_file_set {
            "credentials.json"
        } else if self.env_set {
            "env"
        } else if self.fallback_env_set {
            "env (fallback)"
        } else {
            "missing"
        }
    }

    /// Whether the active key is file-backed rather than sourced from an
    /// environment variable or a credential manager. On Windows this
    /// distinction matters because file-backed keys are not protected by
    /// DPAPI or Windows Credential Manager.
    pub(super) fn is_file_backed(&self) -> bool {
        self.inline_tier.is_some() || self.credentials_file_set
    }

    pub(super) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "provider": self.provider,
            "section": self.section,
            "inline_tier": self.inline_tier.map(|t| t.as_str()),
            "inline_path": self.inline_path.as_ref().map(|p| p.display().to_string()),
            "env_var": self.env_var,
            "env_set": self.env_set,
            "fallback_env_var": self.fallback_env_var,
            "fallback_env_set": self.fallback_env_set,
            "credentials_file_set": self.credentials_file_set,
            "effective_source": self.effective_source(),
            "file_backed": self.is_file_backed(),
        })
    }
}

fn compute_status_row(
    provider: ProviderAuthMeta,
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
        if doc_has_inline_key(&tier.doc, provider.section) {
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
    // Check if the provider key is available through credentials.json so
    // `auth status` can distinguish "env-backed" from "file-backed" entries.
    // We re-use the same env_var name the resolution chain uses.
    let credentials_file_set =
        squeezy_llm::resolve_api_key_from_credentials_file(provider.env).is_some();
    StatusRow {
        provider: provider.cli,
        section: provider.section,
        inline_tier,
        inline_path,
        env_var: provider.env,
        env_set,
        fallback_env_var: provider.fallback_env,
        fallback_env_set,
        credentials_file_set,
    }
}
