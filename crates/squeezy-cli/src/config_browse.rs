//! `squeezy config browse` — unified resource picker.
//!
//! Surfaces every discoverable Squeezy resource as a single structured
//! listing so users can answer "what does this checkout know about?"
//! without remembering the four sibling commands (`squeezy providers
//! list`, `squeezy sessions list`, the `/skill` and `/<prompt>` slash
//! menus). Output is text by default, JSON with `--json`, one section
//! per resource type.
//!
//! The aggregator stays a thin shim over the existing in-process
//! catalogs (`SkillCatalog::discover`, `PromptTemplateCatalog::discover`,
//! `SessionStore::list`, the provider registry) so the listings always
//! match what the rest of the agent sees — no parallel discovery code
//! to drift out of sync.

use std::env;

use serde_json::{Value, json};
use squeezy_core::{AppConfig, Result, SqueezyError};
use squeezy_skills::{PromptTemplate, PromptTemplateCatalog, SkillCatalog, SkillSummary};
use squeezy_store::{SessionMetadata, SessionQuery, SessionStore};

use crate::ConfigBrowseArgs;
use crate::providers::{ProviderEntry, registry_entries};

/// Maximum number of sessions surfaced inline in the browse output.
///
/// The picker is meant for orientation, not session management; once
/// the local store accumulates more than a handful of sessions the
/// listing would dominate the page. We surface the most recent few and
/// point users at `squeezy sessions list` for the long form. The same
/// cap is used by the JSON renderer so machine-readable output stays
/// consistent with the text version.
pub(crate) const SESSION_PREVIEW_LIMIT: usize = 10;

/// Aggregated resource snapshot rendered by `squeezy config browse`.
///
/// Splitting collection (which touches the filesystem and env table)
/// from rendering (which is a pure string/value transform) lets the
/// unit tests build a fixture `BrowseInputs` straight from in-memory
/// data and exercise the formatter without spinning up real catalogs
/// or a temp `HOME`.
#[derive(Debug, Default)]
pub(crate) struct BrowseInputs {
    pub(crate) skills: Vec<SkillSummary>,
    pub(crate) providers: Vec<ProviderEntry>,
    pub(crate) sessions: Vec<SessionMetadata>,
    pub(crate) templates: Vec<PromptTemplate>,
}

pub(crate) fn handle_browse_command(config: &AppConfig, args: &ConfigBrowseArgs) -> Result<()> {
    let inputs = collect_inputs(config);
    if args.json {
        let body = render_json(&inputs);
        println!(
            "{}",
            serde_json::to_string_pretty(&body)
                .map_err(|err| SqueezyError::Config(err.to_string()))?
        );
    } else {
        print!("{}", render_text(&inputs));
    }
    Ok(())
}

fn collect_inputs(config: &AppConfig) -> BrowseInputs {
    let workspace_root = config.workspace_root.as_path();
    let skills = SkillCatalog::discover(workspace_root, &config.skills).summaries();
    let templates: Vec<PromptTemplate> = PromptTemplateCatalog::discover(workspace_root)
        .templates()
        .cloned()
        .collect();
    let providers = registry_entries(&|name| env::var(name).ok());
    let sessions = SessionStore::open(config)
        .list(&SessionQuery::default())
        .unwrap_or_default();
    BrowseInputs {
        skills,
        providers,
        sessions,
        templates,
    }
}

pub(crate) fn render_text(inputs: &BrowseInputs) -> String {
    let mut out = String::new();
    render_skills_section(&mut out, &inputs.skills);
    out.push('\n');
    render_providers_section(&mut out, &inputs.providers);
    out.push('\n');
    render_sessions_section(&mut out, &inputs.sessions);
    out.push('\n');
    render_templates_section(&mut out, &inputs.templates);
    out
}

pub(crate) fn render_json(inputs: &BrowseInputs) -> Value {
    json!({
        "skills": inputs.skills.iter().map(skill_json).collect::<Vec<_>>(),
        "providers": &inputs.providers,
        "sessions": inputs
            .sessions
            .iter()
            .take(SESSION_PREVIEW_LIMIT)
            .map(session_json)
            .collect::<Vec<_>>(),
        "session_count": inputs.sessions.len(),
        "prompt_templates": inputs
            .templates
            .iter()
            .map(template_json)
            .collect::<Vec<_>>(),
    })
}

fn skill_json(skill: &SkillSummary) -> Value {
    json!({
        "name": skill.name,
        "source": skill.source.as_str(),
        "location": skill.location,
        "description": skill.description,
        "disabled": skill.disabled,
    })
}

fn session_json(session: &SessionMetadata) -> Value {
    json!({
        "session_id": session.session_id,
        "status": session.status.as_str(),
        "cwd": session.cwd,
        "started_at_ms": session.started_at_ms,
        "provider": session.provider,
        "model": session.model,
        "label": session_label(session),
    })
}

fn template_json(template: &PromptTemplate) -> Value {
    json!({
        "name": template.name,
        "source": template.source.as_str(),
        "description": template.description,
        "argument_hint": template.argument_hint,
        "path": template.path,
    })
}

fn render_skills_section(out: &mut String, skills: &[SkillSummary]) {
    out.push_str(&format!("SKILLS ({})\n", skills.len()));
    if skills.is_empty() {
        out.push_str("  (none discovered)\n");
        return;
    }
    let name_width = max_width(skills.iter().map(|s| s.name.as_str()));
    let src_width = max_width(skills.iter().map(|s| s.source.as_str()));
    for skill in skills {
        let state = if skill.disabled { " (disabled)" } else { "" };
        out.push_str(&format!(
            "  {name:<name_w$}  {src:<src_w$}  {desc}{state}\n",
            name = skill.name,
            src = skill.source.as_str(),
            desc = skill.description,
            name_w = name_width,
            src_w = src_width,
        ));
    }
}

fn render_providers_section(out: &mut String, providers: &[ProviderEntry]) {
    let configured = providers.iter().filter(|p| p.configured).count();
    out.push_str(&format!(
        "PROVIDERS ({} known, {} configured)\n",
        providers.len(),
        configured
    ));
    if providers.is_empty() {
        out.push_str("  (none registered)\n");
        return;
    }
    let name_width = max_width(providers.iter().map(|p| p.name));
    let env_width = max_width(providers.iter().map(|p| {
        if p.api_key_env.is_empty() {
            "(no env)"
        } else {
            p.api_key_env
        }
    }));
    for provider in providers {
        let env_label = if provider.api_key_env.is_empty() {
            "(no env)"
        } else {
            provider.api_key_env
        };
        out.push_str(&format!(
            "  {name:<name_w$}  {env:<env_w$}  {state:<10}  {models} model(s)\n",
            name = provider.name,
            env = env_label,
            state = if provider.configured {
                "configured"
            } else {
                "unset"
            },
            models = provider.model_count,
            name_w = name_width,
            env_w = env_width,
        ));
    }
}

fn render_sessions_section(out: &mut String, sessions: &[SessionMetadata]) {
    out.push_str(&format!("SESSIONS ({})\n", sessions.len()));
    if sessions.is_empty() {
        out.push_str("  (no sessions recorded)\n");
        return;
    }
    let preview: Vec<&SessionMetadata> = sessions.iter().take(SESSION_PREVIEW_LIMIT).collect();
    let id_width = max_width(preview.iter().map(|s| s.session_id.as_str()));
    let status_width = max_width(preview.iter().map(|s| s.status.as_str()));
    for session in &preview {
        out.push_str(&format!(
            "  {id:<id_w$}  {status:<status_w$}  {label}\n",
            id = session.session_id,
            status = session.status.as_str(),
            label = session_label(session),
            id_w = id_width,
            status_w = status_width,
        ));
    }
    if sessions.len() > SESSION_PREVIEW_LIMIT {
        out.push_str(&format!(
            "  … {} more (run `squeezy sessions list`)\n",
            sessions.len() - SESSION_PREVIEW_LIMIT,
        ));
    }
}

fn render_templates_section(out: &mut String, templates: &[PromptTemplate]) {
    out.push_str(&format!("PROMPT TEMPLATES ({})\n", templates.len()));
    if templates.is_empty() {
        out.push_str("  (none discovered)\n");
        return;
    }
    // We render names as "/name" so reserve column width for the slash.
    let name_width = templates
        .iter()
        .map(|t| t.name.len() + 1)
        .max()
        .unwrap_or(0);
    let source_width = max_width(templates.iter().map(|t| t.source.as_str()));
    for template in templates {
        let hint = template
            .argument_hint
            .as_deref()
            .map(|h| format!(" {h}"))
            .unwrap_or_default();
        let slashed = format!("/{}", template.name);
        out.push_str(&format!(
            "  {name:<name_w$}  {src:<src_w$}  {desc}{hint}\n",
            name = slashed,
            src = template.source.as_str(),
            desc = template.description,
            name_w = name_width,
            src_w = source_width,
        ));
    }
}

fn session_label(session: &SessionMetadata) -> String {
    session
        .display_name
        .clone()
        .or_else(|| session.first_user_task.clone())
        .or_else(|| session.latest_summary.clone())
        .unwrap_or_default()
        .replace('\n', " ")
}

fn max_width<'a, I>(values: I) -> usize
where
    I: IntoIterator<Item = &'a str>,
{
    values.into_iter().map(str::len).max().unwrap_or(0)
}

#[cfg(test)]
#[path = "config_browse_tests.rs"]
mod tests;
