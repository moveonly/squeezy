use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use squeezy_core::{Result, SkillConfigEntry, SkillsConfig, SqueezyError};
use tracing::warn;

pub mod help;
pub mod implicit;
pub mod render;

pub use help::{HelpAnswer, HelpCitation, HelpStatus, SqueezyHelp, matches_squeezy_help_input};
pub use render::SkillPreambleRender;

const SKILL_FILE: &str = "SKILL.md";
const PROJECT_SKILLS_DIR: &str = ".squeezy/skills";
const COMPAT_PROJECT_SKILLS_DIR: &str = ".agents/skills";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillSource {
    CompatUser,
    User,
    CompatProject,
    Project,
}

impl SkillSource {
    pub(crate) const fn precedence(self) -> u8 {
        match self {
            Self::CompatUser => 0,
            Self::User => 1,
            Self::CompatProject => 2,
            Self::Project => 3,
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::CompatUser => "compat_user",
            Self::User => "user",
            Self::CompatProject => "compat_project",
            Self::Project => "project",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
    pub when_to_use: Option<String>,
    pub source: SkillSource,
    pub location: PathBuf,
    pub disabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedSkill {
    pub summary: SkillSummary,
    pub base_dir: PathBuf,
    pub body: String,
}

impl LoadedSkill {
    pub fn prompt_block(&self) -> String {
        let SkillSummary {
            name,
            description,
            when_to_use,
            source,
            location,
            disabled: _,
        } = &self.summary;
        let when_to_use = when_to_use
            .as_ref()
            .map(|value| format!("\n<when_to_use>{}</when_to_use>", xml_escape(value)))
            .unwrap_or_default();
        format!(
            "<skill name=\"{}\" source=\"{}\">\n<description>{}</description>{when_to_use}\n<location>{}</location>\n<base_directory>{}</base_directory>\n<content>\n{}\n</content>\n</skill>",
            xml_escape(name),
            source.as_str(),
            xml_escape(description),
            location.display(),
            self.base_dir.display(),
            escape_body_breakouts(self.body.trim())
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SkillEntry {
    summary: SkillSummary,
    base_dir: PathBuf,
    triggers: Vec<String>,
}

#[derive(Debug)]
pub struct SkillCatalog {
    skills: BTreeMap<String, SkillEntry>,
    cache: Mutex<BTreeMap<String, LoadedSkill>>,
    ambiguous_names: BTreeSet<String>,
    implicit_by_scripts_dir: BTreeMap<PathBuf, String>,
    implicit_by_doc_path: BTreeMap<PathBuf, String>,
    active_budget_chars: usize,
    active_body_cap_chars: usize,
    preamble_enabled: bool,
    preamble_budget_chars: usize,
}

impl Default for SkillCatalog {
    fn default() -> Self {
        let defaults = SkillsConfig::default();
        Self {
            skills: BTreeMap::new(),
            cache: Mutex::new(BTreeMap::new()),
            ambiguous_names: BTreeSet::new(),
            implicit_by_scripts_dir: BTreeMap::new(),
            implicit_by_doc_path: BTreeMap::new(),
            active_budget_chars: defaults.active_budget_chars,
            active_body_cap_chars: defaults.active_body_cap_chars,
            preamble_enabled: defaults.preamble_enabled,
            preamble_budget_chars: defaults.preamble_budget_chars,
        }
    }
}

impl SkillCatalog {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn discover(workspace_root: &Path, config: &SkillsConfig) -> Self {
        let mut catalog = Self {
            active_budget_chars: config.active_budget_chars,
            active_body_cap_chars: config.active_body_cap_chars,
            preamble_enabled: config.preamble_enabled,
            preamble_budget_chars: config.preamble_budget_chars,
            ..Self::default()
        };
        catalog.discover_dir(&config.compat_user_dir, SkillSource::CompatUser);
        catalog.discover_dir(&config.user_dir, SkillSource::User);
        catalog.discover_dir(
            &workspace_root.join(COMPAT_PROJECT_SKILLS_DIR),
            SkillSource::CompatProject,
        );
        catalog.discover_dir(
            &workspace_root.join(PROJECT_SKILLS_DIR),
            SkillSource::Project,
        );
        catalog.apply_config_rules(workspace_root, &config.config);
        catalog.rebuild_implicit_indexes();
        catalog
    }

    pub fn summaries(&self) -> Vec<SkillSummary> {
        self.skills
            .values()
            .map(|entry| entry.summary.clone())
            .collect()
    }

    pub fn summaries_json(&self) -> Value {
        json!({
            "skills": self.summaries()
                .into_iter()
                .map(|summary| {
                    json!({
                        "name": summary.name,
                        "description": summary.description,
                        "when_to_use": summary.when_to_use,
                        "source": summary.source.as_str(),
                        "location": summary.location,
                        "disabled": summary.disabled,
                    })
                })
                .collect::<Vec<_>>()
        })
    }

    pub fn load(&self, name: &str) -> Result<LoadedSkill> {
        if let Ok(cache) = self.cache.lock()
            && let Some(cached) = cache.get(name)
        {
            return Ok(cached.clone());
        }
        let Some(entry) = self.skills.get(name) else {
            return Err(SqueezyError::Tool(format!("skill not found: {name}")));
        };
        if entry.summary.disabled {
            return Err(SqueezyError::Tool(format!("skill disabled: {name}")));
        }
        let content = fs::read_to_string(&entry.summary.location)?;
        let (_metadata, body) = parse_skill_file(&content).map_err(SqueezyError::Tool)?;
        let loaded = LoadedSkill {
            summary: entry.summary.clone(),
            base_dir: entry.base_dir.clone(),
            body,
        };
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(name.to_string(), loaded.clone());
        }
        Ok(loaded)
    }

    pub fn activate_for_input(&self, input: &str) -> Result<SkillActivation> {
        let mut task = input.to_string();
        let mut names = Vec::new();
        if let Some((name, rest)) = parse_explicit_skill_command(input) {
            names.push(name.to_string());
            task = rest.to_string();
        }

        let lowered = task.to_ascii_lowercase();
        for entry in self.skills.values() {
            if entry.summary.disabled || self.ambiguous_names.contains(&entry.summary.name) {
                continue;
            }
            if entry
                .triggers
                .iter()
                .any(|trigger| input_matches_trigger(&lowered, trigger))
            {
                names.push(entry.summary.name.clone());
            }
        }

        let mut seen = BTreeSet::new();
        let mut loaded = Vec::new();
        for name in names {
            if seen.insert(name.clone()) {
                loaded.push(self.load(&name)?);
            }
        }
        Ok(SkillActivation {
            task_input: task,
            skills: loaded,
        })
    }

    pub fn render_active_skills(&self, skills: &[LoadedSkill]) -> Option<String> {
        render::render_active_skills(skills, self.active_budget_chars, self.active_body_cap_chars)
    }

    pub fn render_preamble(&self) -> Option<SkillPreambleRender> {
        self.preamble_enabled
            .then(|| {
                render::render_skill_preamble(
                    &self
                        .summaries()
                        .into_iter()
                        .filter(|summary| !summary.disabled)
                        .collect::<Vec<_>>(),
                    self.preamble_budget_chars,
                )
            })
            .flatten()
    }

    pub fn ambiguous_names(&self) -> &BTreeSet<String> {
        &self.ambiguous_names
    }

    pub fn detect_for_command(&self, command: &str, workdir: &Path) -> Option<SkillSummary> {
        implicit::detect_for_command(
            command,
            workdir,
            &self.implicit_by_scripts_dir,
            &self.implicit_by_doc_path,
            &self.skills,
        )
    }

    fn discover_dir(&mut self, dir: &Path, source: SkillSource) {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => {
                warn!(
                    target: "squeezy_skills",
                    dir = %dir.display(),
                    error = %error,
                    "skipping skill directory due to read error"
                );
                return;
            }
        };

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    warn!(
                        target: "squeezy_skills",
                        dir = %dir.display(),
                        error = %error,
                        "skipping skill entry due to read error"
                    );
                    continue;
                }
            };
            let path = entry.path();
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(error) => {
                    warn!(
                        target: "squeezy_skills",
                        path = %path.display(),
                        error = %error,
                        "skipping skill entry due to metadata error"
                    );
                    continue;
                }
            };
            if !metadata.is_dir() {
                continue;
            }
            let skill_path = path.join(SKILL_FILE);
            let content = match fs::read_to_string(&skill_path) {
                Ok(content) => content,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    warn!(
                        target: "squeezy_skills",
                        path = %skill_path.display(),
                        error = %error,
                        "skipping skill due to read error"
                    );
                    continue;
                }
            };
            let parsed = match parse_skill_file(&content) {
                Ok(parsed) => parsed,
                Err(error) => {
                    warn!(
                        target: "squeezy_skills",
                        path = %skill_path.display(),
                        error = %error,
                        "skipping malformed SKILL.md"
                    );
                    continue;
                }
            };
            let (metadata, _body) = parsed;
            if !is_valid_skill_name(&metadata.name) {
                warn!(
                    target: "squeezy_skills",
                    path = %skill_path.display(),
                    name = %metadata.name,
                    "skipping SKILL.md with invalid name"
                );
                continue;
            }
            let summary = SkillSummary {
                name: metadata.name.clone(),
                description: metadata.description,
                when_to_use: metadata.when_to_use,
                source,
                location: skill_path,
                disabled: false,
            };
            self.insert(SkillEntry {
                summary,
                base_dir: path,
                triggers: metadata.triggers,
            });
        }
    }

    fn insert(&mut self, entry: SkillEntry) {
        match self.skills.get(&entry.summary.name) {
            Some(existing)
                if existing.summary.source.precedence() > entry.summary.source.precedence() => {}
            Some(existing)
                if existing.summary.source.precedence() == entry.summary.source.precedence() =>
            {
                warn!(
                    target: "squeezy_skills",
                    name = %entry.summary.name,
                    existing = %existing.summary.location.display(),
                    incoming = %entry.summary.location.display(),
                    "same-precedence skill name collision; trigger activation will require explicit selection"
                );
                self.ambiguous_names.insert(entry.summary.name.clone());
                if let Ok(mut cache) = self.cache.lock() {
                    cache.remove(&entry.summary.name);
                }
                self.skills.insert(entry.summary.name.clone(), entry);
            }
            _ => {
                self.ambiguous_names.remove(&entry.summary.name);
                if let Ok(mut cache) = self.cache.lock() {
                    cache.remove(&entry.summary.name);
                }
                self.skills.insert(entry.summary.name.clone(), entry);
            }
        }
    }

    fn apply_config_rules(&mut self, workspace_root: &Path, rules: &[SkillConfigEntry]) {
        for rule in rules {
            match (rule.name.as_ref(), rule.path.as_ref()) {
                (Some(_), Some(_)) | (None, None) => {
                    warn!(
                        target: "squeezy_skills",
                        name = ?rule.name,
                        path = ?rule.path,
                        "ignoring skill config entry with invalid selector"
                    );
                    continue;
                }
                (Some(name), None) => {
                    if let Some(entry) = self.skills.get_mut(name) {
                        entry.summary.disabled = !rule.enabled;
                        if let Ok(mut cache) = self.cache.lock() {
                            cache.remove(name);
                        }
                    }
                }
                (None, Some(path)) => {
                    let selector = config_selector_path(workspace_root, path);
                    for entry in self.skills.values_mut() {
                        if skill_path_matches(&selector, entry) {
                            entry.summary.disabled = !rule.enabled;
                            if let Ok(mut cache) = self.cache.lock() {
                                cache.remove(&entry.summary.name);
                            }
                        }
                    }
                }
            }
        }
    }

    fn rebuild_implicit_indexes(&mut self) {
        self.implicit_by_scripts_dir.clear();
        self.implicit_by_doc_path.clear();
        for entry in self.skills.values() {
            if entry.summary.disabled {
                continue;
            }
            self.implicit_by_doc_path.insert(
                implicit::normalize_path(&entry.summary.location),
                entry.summary.name.clone(),
            );
            self.implicit_by_scripts_dir.insert(
                implicit::normalize_path(&entry.base_dir.join("scripts")),
                entry.summary.name.clone(),
            );
        }
    }
}

impl Clone for SkillCatalog {
    fn clone(&self) -> Self {
        let cache = self
            .cache
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        Self {
            skills: self.skills.clone(),
            cache: Mutex::new(cache),
            ambiguous_names: self.ambiguous_names.clone(),
            implicit_by_scripts_dir: self.implicit_by_scripts_dir.clone(),
            implicit_by_doc_path: self.implicit_by_doc_path.clone(),
            active_budget_chars: self.active_budget_chars,
            active_body_cap_chars: self.active_body_cap_chars,
            preamble_enabled: self.preamble_enabled,
            preamble_budget_chars: self.preamble_budget_chars,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillActivation {
    pub task_input: String,
    pub skills: Vec<LoadedSkill>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SkillMetadata {
    name: String,
    description: String,
    when_to_use: Option<String>,
    triggers: Vec<String>,
}

fn parse_explicit_skill_command(input: &str) -> Option<(&str, &str)> {
    let trimmed = input.trim_start();
    let rest = trimmed.strip_prefix("/skill")?;
    let mut chars = rest.chars();
    let first = chars.next()?;
    if !first.is_whitespace() {
        return None;
    }
    let rest = chars.as_str().trim_start();
    let mut parts = rest.splitn(2, char::is_whitespace);
    let name = parts.next()?.trim();
    if name.is_empty() {
        return None;
    }
    let task = parts.next().unwrap_or("").trim_start();
    Some((name, task))
}

fn parse_skill_file(content: &str) -> std::result::Result<(SkillMetadata, String), String> {
    let mut lines = content.lines();
    if lines.next() != Some("---") {
        return Err("missing YAML frontmatter".to_string());
    }

    let mut frontmatter = Vec::new();
    let mut body = Vec::new();
    let mut in_frontmatter = true;
    for line in lines {
        if in_frontmatter && line.trim() == "---" {
            in_frontmatter = false;
            continue;
        }
        if in_frontmatter {
            frontmatter.push(line);
        } else {
            body.push(line);
        }
    }
    if in_frontmatter {
        return Err("unterminated YAML frontmatter".to_string());
    }
    let metadata = parse_frontmatter(&frontmatter)?;
    Ok((metadata, body.join("\n")))
}

fn parse_frontmatter(lines: &[&str]) -> std::result::Result<SkillMetadata, String> {
    let mut name = None;
    let mut description = None;
    let mut when_to_use = None;
    let mut triggers = Vec::new();
    let mut list_key: Option<&str> = None;

    for raw in lines {
        let line = raw.trim_end();
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let trimmed = line.trim_start();
        if let Some(key) = list_key {
            if let Some(item) = trimmed.strip_prefix("- ") {
                if key == "triggers" {
                    triggers.push(unquote(item.trim()).to_string());
                }
                continue;
            }
            list_key = None;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            return Err(format!("invalid frontmatter line: {line}"));
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "name" => name = Some(unquote(value).to_string()),
            "description" => description = Some(unquote(value).to_string()),
            "when_to_use" => when_to_use = Some(unquote(value).to_string()),
            "triggers" if value.is_empty() => list_key = Some("triggers"),
            "triggers" => triggers.extend(parse_inline_list(value)),
            _ => {}
        }
    }

    let name = name
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "skill frontmatter requires name".to_string())?;
    let description = description
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "skill frontmatter requires description".to_string())?;
    Ok(SkillMetadata {
        name,
        description,
        when_to_use,
        triggers,
    })
}

fn parse_inline_list(value: &str) -> Vec<String> {
    let value = value.trim();
    let Some(inner) = value
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
    else {
        return vec![unquote(value).to_string()];
    };
    inner
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(unquote)
        .map(str::to_string)
        .collect()
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(value)
}

fn is_valid_skill_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '-' | '_'))
        && value
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_lowercase())
}

fn input_matches_trigger(lowered_input: &str, trigger: &str) -> bool {
    let needle = trigger.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return false;
    }
    let bytes = lowered_input.as_bytes();
    let needle_bytes = needle.as_bytes();
    let mut cursor = 0;
    while cursor + needle_bytes.len() <= bytes.len() {
        let Some(rel) = lowered_input[cursor..].find(needle.as_str()) else {
            return false;
        };
        let start = cursor + rel;
        let end = start + needle_bytes.len();
        let prev_ok = start == 0 || !is_word_byte(bytes[start - 1]);
        let next_ok = end == bytes.len() || !is_word_byte(bytes[end]);
        if prev_ok && next_ok {
            return true;
        }
        cursor = start + 1;
    }
    false
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn config_selector_path(workspace_root: &Path, selector: &Path) -> PathBuf {
    if selector.is_absolute() {
        implicit::normalize_path(selector)
    } else {
        implicit::normalize_path(&workspace_root.join(selector))
    }
}

fn skill_path_matches(selector: &Path, entry: &SkillEntry) -> bool {
    let location = implicit::normalize_path(&entry.summary.location);
    let base_dir = implicit::normalize_path(&entry.base_dir);
    selector == location || selector == base_dir
}

pub(crate) fn xml_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            other => out.push(other),
        }
    }
    out
}

pub(crate) fn escape_body_breakouts(body: &str) -> String {
    body.replace("</content>", "<\\/content>")
        .replace("</skill>", "<\\/skill>")
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
