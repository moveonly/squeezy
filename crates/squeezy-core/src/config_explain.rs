use std::fmt;

use crate::{
    AppConfig, SeparatedSources, config_schema, default_judge_prompt, judge_model_for_provider,
    resolve_model_alias, resolved_reroute_filter,
};

pub const REDACTED_VALUE_DISPLAY: &str = "••••";
pub const EMPTY_FIELD_DISPLAY: &str = "—";

pub fn find_config_field_for_path(
    requested_path: &[&str],
) -> Option<&'static config_schema::FieldMeta> {
    config_schema::CONFIG_SECTIONS
        .iter()
        .flat_map(|s| s.fields.iter())
        .find(|field| config_field_path_matches(field.toml_path, requested_path))
}

pub fn config_field_path_matches(schema_path: &[&str], requested_path: &[&str]) -> bool {
    schema_path.len() == requested_path.len()
        && schema_path
            .iter()
            .zip(requested_path.iter())
            .all(|(schema, requested)| *schema == "*" || schema == requested)
}

pub fn split_config_field_path(path: &str) -> Result<Vec<String>, String> {
    if path.is_empty() {
        return Err("empty config field path".to_string());
    }

    let mut segments: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut expects_segment = false;
    let mut chars = path.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '"' | '\'' => {
                if !current.is_empty() {
                    return Err(format!(
                        "unexpected quote {ch} inside bare key segment {current:?}"
                    ));
                }
                let quote = ch;
                let mut quoted = String::new();
                let mut closed = false;
                for c in chars.by_ref() {
                    if c == quote {
                        closed = true;
                        break;
                    }
                    quoted.push(c);
                }
                if !closed {
                    return Err(format!("unterminated quoted segment starting with {quote}"));
                }
                current.push_str(&quoted);
                match chars.peek().copied() {
                    Some('.') => {
                        chars.next();
                        segments.push(std::mem::take(&mut current));
                        expects_segment = true;
                    }
                    Some(other) => {
                        return Err(format!(
                            "unexpected character {other:?} after closing quote {quote}"
                        ));
                    }
                    None => {
                        segments.push(std::mem::take(&mut current));
                        expects_segment = false;
                    }
                }
            }
            '.' => {
                if current.is_empty() {
                    return Err("empty key segment".to_string());
                }
                segments.push(std::mem::take(&mut current));
                expects_segment = true;
            }
            _ => {
                current.push(ch);
                expects_segment = false;
            }
        }
    }

    if !current.is_empty() {
        segments.push(current);
        expects_segment = false;
    }

    if expects_segment {
        return Err("trailing `.` without a final key segment".to_string());
    }

    if segments.is_empty() || segments.iter().any(String::is_empty) {
        return Err("empty key segment".to_string());
    }

    Ok(segments)
}

pub fn resolve_explain_field_source(
    sources: &SeparatedSources,
    field: &config_schema::FieldMeta,
    requested_path: &[&str],
) -> config_schema::FieldSource {
    if let Some(var_name) = field.env_override
        && std::env::var(var_name).is_ok()
    {
        return config_schema::FieldSource::Env;
    }
    if let Some(repo) = &sources.repo
        && repo.contains_path(requested_path)
    {
        return config_schema::FieldSource::Repo;
    }
    if let Some(project) = &sources.project
        && project.contains_path(requested_path)
    {
        return config_schema::FieldSource::Project;
    }
    if let Some(user) = &sources.user
        && user.contains_path(requested_path)
    {
        return config_schema::FieldSource::User;
    }
    config_schema::FieldSource::Default
}

#[derive(Debug, Clone)]
pub struct RedactedDisplay(String);

impl RedactedDisplay {
    fn safe(text: String) -> Self {
        Self(text)
    }

    fn redacted() -> Self {
        Self(REDACTED_VALUE_DISPLAY.to_string())
    }
}

impl fmt::Display for RedactedDisplay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl PartialEq<&str> for RedactedDisplay {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

pub fn explain_effective_value(
    config: &AppConfig,
    field: &config_schema::FieldMeta,
    requested_path: &[&str],
) -> RedactedDisplay {
    use config_schema::FieldKind;

    if field.secret || matches!(field.kind, FieldKind::Secret { .. }) {
        return RedactedDisplay::redacted();
    }

    let text = concrete_explain_value(config, field.toml_path, requested_path)
        .unwrap_or_else(|| (field.get)(config).as_display());
    RedactedDisplay::safe(text)
}

fn concrete_explain_value(
    config: &AppConfig,
    schema_path: &[&str],
    requested_path: &[&str],
) -> Option<String> {
    match (schema_path, requested_path) {
        (["providers", "*", key], ["providers", provider, _]) => {
            provider_explain_value(config, provider, key)
        }
        (["model_limits", "*", "context_window"], ["model_limits", model_key, _]) => Some(
            config
                .model_limits
                .get(*model_key)
                .and_then(|entry| entry.context_window)
                .map(|value| value.to_string())
                .unwrap_or_else(|| "auto".to_string()),
        ),
        _ => None,
    }
}

fn provider_explain_value(config: &AppConfig, provider: &str, key: &str) -> Option<String> {
    match key {
        "cheap_model" => Some(
            provider_cheap_model(config, provider)
                .unwrap_or_else(|| EMPTY_FIELD_DISPLAY.to_string()),
        ),
        "judge_model" => Some(provider_judge_model(config, provider)),
        "judge_prompt" => Some(provider_judge_prompt(config, provider)),
        "expensive_models" => Some(resolved_reroute_filter(config, provider)),
        _ => None,
    }
}

fn provider_cheap_model(config: &AppConfig, provider: &str) -> Option<String> {
    let model = config
        .providers
        .get(provider)
        .and_then(|p| p.cheap_model.clone())
        .filter(|model| !model.trim().is_empty())
        .or_else(|| config.small_fast_model.clone())
        .or_else(|| judge_model_for_provider(provider).map(str::to_string))
        .or_else(|| (provider == "ollama").then(|| crate::DEFAULT_OLLAMA_MODEL.to_string()))?;
    Some(resolve_model_alias_for_display(provider, model))
}

fn provider_judge_model(config: &AppConfig, provider: &str) -> String {
    if let Some(model) = config
        .providers
        .get(provider)
        .and_then(|p| p.judge_model.clone())
        .filter(|model| !model.trim().is_empty())
        .or_else(|| config.routing.judge_model.clone())
    {
        return resolve_model_alias_for_display(provider, model);
    }
    judge_model_for_provider(provider)
        .map(str::to_string)
        .or_else(|| provider_cheap_model(config, provider))
        .unwrap_or_else(|| EMPTY_FIELD_DISPLAY.to_string())
}

fn provider_judge_prompt(config: &AppConfig, provider: &str) -> String {
    config
        .providers
        .get(provider)
        .and_then(|p| p.judge_prompt.clone())
        .filter(|prompt| !prompt.trim().is_empty())
        .or_else(|| config.routing.judge_prompt.clone())
        .unwrap_or_else(|| default_judge_prompt(provider).to_string())
}

fn resolve_model_alias_for_display(provider: &str, model: String) -> String {
    resolve_model_alias(provider, &model)
        .unwrap_or(&model)
        .to_string()
}
