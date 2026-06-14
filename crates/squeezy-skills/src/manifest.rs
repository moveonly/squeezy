use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::{SKILL_MANIFEST_FILE, xml_escape};

/// Optional sidecar manifest read from `skill.toml` next to `SKILL.md`.
///
/// The sidecar is a strict superset of the `SKILL.md` frontmatter — the
/// frontmatter remains the catalog identity (name, description). Fields
/// here are catalog metadata that the model can see (tool_deps,
/// prompt_hint) plus pure display metadata that does not affect routing
/// (icon).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillManifest {
    /// Tool dependencies the skill expects to find at activation time
    /// (e.g. `"mcp:exa"`, `"shell"`, `"web_fetch"`). Surfaced in the
    /// active-skill prompt block so the model can refuse early when a
    /// required tool is missing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_deps: Vec<String>,
    /// Display icon path resolved relative to the skill's base directory.
    /// Pure display metadata; not rendered into the prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<PathBuf>,
    /// Short prompt fragment surfaced to the model when the skill is
    /// activated. Distinct from the skill body — used as a one-line
    /// activation hint rather than the full instruction set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_hint: Option<String>,
}

impl SkillManifest {
    pub(crate) fn is_empty(&self) -> bool {
        self.tool_deps.is_empty() && self.icon.is_none() && self.prompt_hint.is_none()
    }
}

pub(crate) fn render_manifest_block(manifest: &SkillManifest) -> String {
    if manifest.is_empty() {
        return String::new();
    }
    let mut lines = Vec::new();
    if !manifest.tool_deps.is_empty() {
        let deps = manifest
            .tool_deps
            .iter()
            .map(|dep| format!("<tool>{}</tool>", xml_escape(dep)))
            .collect::<Vec<_>>()
            .join("");
        lines.push(format!("<tool_deps>{deps}</tool_deps>"));
    }
    if let Some(hint) = manifest.prompt_hint.as_ref() {
        lines.push(format!(
            "<prompt_hint>{}</prompt_hint>",
            xml_escape(hint.trim())
        ));
    }
    if lines.is_empty() {
        return String::new();
    }
    format!("\n<manifest>{}</manifest>", lines.concat())
}

pub(crate) fn load_manifest(base_dir: &Path) -> Option<SkillManifest> {
    let manifest_path = base_dir.join(SKILL_MANIFEST_FILE);
    let content = match fs::read_to_string(&manifest_path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            warn!(
                target: "squeezy_skills",
                path = %manifest_path.display(),
                error = %error,
                "skipping skill.toml due to read error"
            );
            return None;
        }
    };
    match parse_skill_manifest(&content) {
        Ok(manifest) if manifest.is_empty() => None,
        Ok(manifest) => Some(manifest),
        Err(error) => {
            warn!(
                target: "squeezy_skills",
                path = %manifest_path.display(),
                error = %error,
                "ignoring malformed skill.toml"
            );
            None
        }
    }
}

pub(crate) fn parse_skill_manifest(content: &str) -> std::result::Result<SkillManifest, String> {
    toml::from_str::<SkillManifest>(content).map_err(|error| error.to_string())
}

#[cfg(test)]
#[path = "manifest_tests.rs"]
mod tests;
