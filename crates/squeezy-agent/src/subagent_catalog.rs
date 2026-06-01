//! Disk-loaded subagent catalog.
//!
//! Squeezy ships with a small set of compile-time subagent kinds
//! (`delegate`, `explore`, `plan`, `review`) wired into the tool dispatch
//! path in `lib.rs`. To unblock users who want to introduce new
//! subagent kinds without recompiling, this module discovers
//! frontmatter-spec `.md` definitions next to the existing skill layout:
//!
//! * Project: `<workspace>/.squeezy/agents/*.md`
//! * User: `~/.squeezy/agents/*.md`
//!
//! Each `.md` file carries a YAML frontmatter block with `name`,
//! `description`, optional `model`, and an optional CSV/inline-list
//! `tools` field; the body becomes the subagent's system prompt.
//!
//! The catalog is intentionally a read-only query surface today. It
//! merges built-in kinds with disk-loaded entries so a slash command,
//! TUI screen, or external tool can answer "what subagents are
//! available here?" without re-walking the filesystem on every call.
//! Built-in dispatch in `lib.rs` is unchanged; disk-loaded entries are
//! additive metadata that future wiring can consume.

use std::{
    borrow::Cow,
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use tracing::warn;

/// Project-relative directory checked under the workspace root for
/// frontmatter subagent definitions. Mirrors the project skills layout
/// at `.squeezy/skills/` so users learn one convention.
pub const PROJECT_SUBAGENTS_DIR: &str = ".squeezy/agents";

/// HOME-relative directory checked for the user's personal subagents.
/// Sits beside `~/.squeezy/skills/` and `~/.squeezy/settings.toml`.
pub const USER_SUBAGENTS_DIR: &str = ".squeezy/agents";

/// Where a [`SubagentDefinition`] originated. Used to break name
/// collisions deterministically: project entries beat user entries beat
/// built-ins, the same precedence the skills catalog applies. Built-ins
/// are reported so callers can list every routable subagent kind in a
/// single pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentSource {
    Builtin,
    User,
    Project,
}

impl SubagentSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::User => "user",
            Self::Project => "project",
        }
    }

    const fn precedence(self) -> u8 {
        match self {
            Self::Builtin => 0,
            Self::User => 1,
            Self::Project => 2,
        }
    }
}

/// One subagent definition as the catalog exposes it.
///
/// `tools` carries the raw token list declared in the frontmatter so the
/// catalog can stay agnostic about how dispatch validates them. `model`
/// is optional — disk-loaded subagents may inherit the parent model when
/// they don't pin one explicitly. `system_prompt` is the markdown body
/// after the YAML block, trimmed of leading/trailing whitespace.
/// `file_path` is `None` for built-ins (they have no on-disk file) and
/// otherwise points at the source `.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentDefinition {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    pub system_prompt: String,
    pub source: SubagentSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<PathBuf>,
}

/// Read-only catalog of subagent kinds available to the current
/// session.
///
/// Built-in entries are always present; disk-loaded entries are merged
/// in from the user and project directories on `discover`. Same-name
/// collisions resolve by [`SubagentSource::precedence`] (project >
/// user > builtin), matching the skills catalog so the rule stays
/// memorable.
#[derive(Debug, Clone, Default)]
pub struct SubagentCatalog {
    entries: Vec<SubagentDefinition>,
}

impl SubagentCatalog {
    /// Empty catalog. Useful in tests and as a safe default before
    /// discovery has run.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Catalog containing only the compile-time built-in subagent
    /// kinds. Used by tests and by callers that want to enumerate the
    /// shipped kinds without touching the filesystem.
    pub fn builtin() -> Self {
        Self {
            entries: builtin_entries(),
        }
    }

    /// Discover the catalog at the standard locations.
    ///
    /// `workspace_root` is the active workspace; the project agents
    /// directory is resolved as `<workspace_root>/.squeezy/agents`.
    /// `user_dir` overrides the user agents directory — when `None` the
    /// default (`$HOME/.squeezy/agents`) is used. Missing directories
    /// are skipped silently; other read errors warn so a permission
    /// problem doesn't silently disable subagents.
    pub fn discover(workspace_root: &Path, user_dir: Option<&Path>) -> Self {
        let mut staged: Vec<SubagentDefinition> = builtin_entries();
        let user_path = user_dir
            .map(Path::to_path_buf)
            .or_else(default_user_subagents_dir);
        if let Some(user_path) = user_path {
            load_dir(&user_path, SubagentSource::User, &mut staged);
        }
        let project_path = workspace_root.join(PROJECT_SUBAGENTS_DIR);
        load_dir(&project_path, SubagentSource::Project, &mut staged);
        Self::dedupe(staged)
    }

    fn dedupe(entries: Vec<SubagentDefinition>) -> Self {
        let mut by_name: BTreeMap<String, SubagentDefinition> = BTreeMap::new();
        for entry in entries {
            match by_name.get(&entry.name) {
                Some(existing) if existing.source.precedence() > entry.source.precedence() => {
                    warn!(
                        target: "squeezy_agent::subagent_catalog",
                        name = %entry.name,
                        kept = %existing.source.as_str(),
                        shadowed = %entry.source.as_str(),
                        shadowed_path = ?entry.file_path,
                        "subagent name reused at lower precedence; shadowed definition will not load"
                    );
                }
                _ => {
                    by_name.insert(entry.name.clone(), entry);
                }
            }
        }
        Self {
            entries: by_name.into_values().collect(),
        }
    }

    /// All catalog entries in deterministic (name-sorted) order.
    pub fn entries(&self) -> &[SubagentDefinition] {
        &self.entries
    }

    /// Lookup by `name`. Returns the winning definition after
    /// precedence resolution.
    pub fn find(&self, name: &str) -> Option<&SubagentDefinition> {
        self.entries.iter().find(|entry| entry.name == name)
    }

    /// Iterator over only the disk-loaded (non-built-in) entries.
    /// Useful for "what additional subagents has the user defined?"
    /// surfaces in a slash command or TUI.
    pub fn user_provided(&self) -> impl Iterator<Item = &SubagentDefinition> {
        self.entries
            .iter()
            .filter(|entry| !matches!(entry.source, SubagentSource::Builtin))
    }
}

/// Default user agents directory, derived from `$HOME` on Unix-like
/// systems. Returns `None` when `HOME` is unset (Windows callers should
/// pass an explicit `user_dir` until cross-platform support lands).
fn default_user_subagents_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(USER_SUBAGENTS_DIR))
}

fn load_dir(dir: &Path, source: SubagentSource, out: &mut Vec<SubagentDefinition>) {
    let read = match fs::read_dir(dir) {
        Ok(read) => read,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            warn!(
                target: "squeezy_agent::subagent_catalog",
                dir = %dir.display(),
                error = %error,
                "skipping subagents directory due to read error"
            );
            return;
        }
    };
    for entry in read {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warn!(
                    target: "squeezy_agent::subagent_catalog",
                    dir = %dir.display(),
                    error = %error,
                    "skipping subagent directory entry due to read error"
                );
                continue;
            }
        };
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                warn!(
                    target: "squeezy_agent::subagent_catalog",
                    path = %path.display(),
                    error = %error,
                    "skipping subagent entry due to metadata error"
                );
                continue;
            }
        };
        if !metadata.is_file() {
            continue;
        }
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) => {
                warn!(
                    target: "squeezy_agent::subagent_catalog",
                    path = %path.display(),
                    error = %error,
                    "skipping subagent due to read error"
                );
                continue;
            }
        };
        match parse_subagent_file(&content) {
            Ok((frontmatter, body)) => {
                if !is_valid_subagent_name(&frontmatter.name) {
                    warn!(
                        target: "squeezy_agent::subagent_catalog",
                        path = %path.display(),
                        name = %frontmatter.name,
                        "skipping subagent with invalid name"
                    );
                    continue;
                }
                out.push(SubagentDefinition {
                    name: frontmatter.name,
                    description: frontmatter.description,
                    model: frontmatter.model,
                    tools: frontmatter.tools,
                    system_prompt: body,
                    source,
                    file_path: Some(path),
                });
            }
            Err(error) => {
                warn!(
                    target: "squeezy_agent::subagent_catalog",
                    path = %path.display(),
                    error = %error,
                    "skipping malformed subagent .md"
                );
            }
        }
    }
}

/// Raw frontmatter view used by the parser. Kept private because it is
/// intentionally a strict subset of what a future loader may accept; we
/// don't want callers building on fields they shouldn't depend on yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubagentFrontmatter {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) model: Option<String>,
    pub(crate) tools: Vec<String>,
}

/// Split a subagent `.md` file into frontmatter + body.
///
/// The format is YAML between `---` fences at the top, then markdown
/// body. The parser is intentionally line-based (no `yaml` crate
/// dependency) to match the existing skills frontmatter parser and keep
/// the dependency footprint flat. Unknown frontmatter keys are ignored
/// so adding a new optional field in a `.md` file never breaks an older
/// Squeezy build.
pub(crate) fn parse_subagent_file(
    content: &str,
) -> std::result::Result<(SubagentFrontmatter, String), String> {
    let normalized: Cow<'_, str> = if content.contains('\r') {
        Cow::Owned(content.replace("\r\n", "\n").replace('\r', "\n"))
    } else {
        Cow::Borrowed(content)
    };
    let mut lines = normalized.lines();
    if lines.next() != Some("---") {
        return Err("missing YAML frontmatter".to_string());
    }
    let mut frontmatter_lines: Vec<&str> = Vec::new();
    let mut body = String::new();
    let mut body_started = false;
    let mut in_frontmatter = true;
    for line in lines {
        if in_frontmatter && line.trim() == "---" {
            in_frontmatter = false;
            continue;
        }
        if in_frontmatter {
            frontmatter_lines.push(line);
        } else {
            if body_started {
                body.push('\n');
            } else {
                body_started = true;
            }
            body.push_str(line);
        }
    }
    if in_frontmatter {
        return Err("unterminated YAML frontmatter".to_string());
    }
    let frontmatter = parse_frontmatter_lines(&frontmatter_lines)?;
    let body = body.trim().to_string();
    Ok((frontmatter, body))
}

fn parse_frontmatter_lines(lines: &[&str]) -> std::result::Result<SubagentFrontmatter, String> {
    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut model: Option<String> = None;
    let mut tools: Vec<String> = Vec::new();

    for raw in lines {
        let line = raw.trim_end();
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = unquote(value.trim()).to_string();
        match key {
            "name" => name = Some(value),
            "description" => description = Some(value),
            "model" if !value.is_empty() => model = Some(value),
            "tools" => tools = parse_tool_list(&value),
            _ => {}
        }
    }

    let name = name
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "subagent frontmatter requires name".to_string())?;
    let description = description
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "subagent frontmatter requires description".to_string())?;
    Ok(SubagentFrontmatter {
        name,
        description,
        model,
        tools,
    })
}

/// Parse the `tools:` value. Accepts inline YAML lists (`[a, b]`) and
/// the more pi-idiomatic comma-separated string (`a, b`). Returns an
/// empty list when the value is empty.
fn parse_tool_list(value: &str) -> Vec<String> {
    let value = value.trim();
    if value.is_empty() {
        return Vec::new();
    }
    if let Some(inner) = value
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
    {
        return inner
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| unquote(value).to_string())
            .collect();
    }
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| unquote(value).to_string())
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

/// Subagent names must be `[a-z][a-z0-9_-]*`. Mirrors `is_valid_skill_name`
/// in `squeezy-skills` so users learn one convention and so the names are
/// safe to render as identifiers in slash commands and tool labels.
fn is_valid_subagent_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '-' | '_'))
        && value
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_lowercase())
}

/// The four user-facing built-in subagent kinds.
///
/// Names match the dispatch tool names (`delegate`, `explore`, `plan`,
/// `review`) so users see the same identifier in the catalog and in the
/// tool call. The hidden `doc_help` subagent is intentionally omitted —
/// it is an internal `/help` mechanism, not a kind users can route to
/// directly. Built-in entries carry no `file_path`; the dispatch logic
/// in `lib.rs` is the source of truth for their behavior.
fn builtin_entries() -> Vec<SubagentDefinition> {
    vec![
        SubagentDefinition {
            name: "delegate".to_string(),
            description:
                "Isolated read-only research subagent for investigating questions without polluting the parent's context."
                    .to_string(),
            model: None,
            tools: Vec::new(),
            system_prompt: String::new(),
            source: SubagentSource::Builtin,
            file_path: None,
        },
        SubagentDefinition {
            name: "explore".to_string(),
            description: "Graph-first codebase exploration.".to_string(),
            model: None,
            tools: Vec::new(),
            system_prompt: String::new(),
            source: SubagentSource::Builtin,
            file_path: None,
        },
        SubagentDefinition {
            name: "plan".to_string(),
            description: "Read-only graph-backed implementation planning.".to_string(),
            model: None,
            tools: Vec::new(),
            system_prompt: String::new(),
            source: SubagentSource::Builtin,
            file_path: None,
        },
        SubagentDefinition {
            name: "review".to_string(),
            description: "Read-only review of changed code.".to_string(),
            model: None,
            tools: Vec::new(),
            system_prompt: String::new(),
            source: SubagentSource::Builtin,
            file_path: None,
        },
    ]
}

#[cfg(test)]
#[path = "subagent_catalog_tests.rs"]
mod tests;
