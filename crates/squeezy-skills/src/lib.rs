use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Mutex,
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use squeezy_core::{Result, SkillConfigEntry, SkillsConfig, SqueezyError};
use squeezy_hooks::{HookContext, HookEvent, HookHandler, HookRegistry, HookResult};
use tracing::warn;

pub mod help;
pub mod implicit;
pub mod prompt_templates;
pub mod render;

pub use help::{
    APPROVAL_POLICY_DOC_PATH, BundledDoc, HelpAnswer, HelpCitation, HelpStatus, SqueezyHelp,
    bundled_doc, bundled_doc_paths, bundled_docs, matches_squeezy_help_input,
    relevant_docs_for_input,
};
pub use prompt_templates::{
    PROJECT_PROMPTS_DIR, PromptTemplate, PromptTemplateCatalog, PromptTemplateSource,
    USER_PROMPTS_SUBPATH, parse_command_args as parse_prompt_template_args,
    substitute_args as substitute_prompt_template_args,
};
pub use render::SkillPreambleRender;

const SKILL_FILE: &str = "SKILL.md";
const SKILL_MANIFEST_FILE: &str = "skill.toml";
const PROJECT_SKILLS_DIR: &str = ".squeezy/skills";
const COMPAT_PROJECT_SKILLS_DIR: &str = ".agents/skills";

/// Source identifier used for the in-binary skills returned by
/// [`bundled_skills`]; the on-disk catalog uses real filesystem roots, so the
/// `location` and `base_dir` on these summaries reference a sentinel path that
/// will never collide with a real skill on disk.
const BUNDLED_VIRTUAL_ROOT: &str = "<squeezy-builtin>";

struct BundledSkillSource {
    dir_name: &'static str,
    content: &'static str,
}

const BUNDLED_SKILL_SOURCES: &[BundledSkillSource] = &[
    BundledSkillSource {
        dir_name: "customize-squeezy",
        content: include_str!("../builtin/customize-squeezy/SKILL.md"),
    },
    BundledSkillSource {
        dir_name: "release-notes",
        content: include_str!("../builtin/release-notes/SKILL.md"),
    },
    BundledSkillSource {
        dir_name: "skill-creator",
        content: include_str!("../builtin/skill-creator/SKILL.md"),
    },
];

/// Return the in-binary sample skills that ship with Squeezy.
///
/// These are not registered into a [`SkillCatalog`] automatically; callers
/// that want to surface them as first-run examples can write them under a
/// user-controlled skills root (typically `~/.squeezy/skills/`) before
/// constructing the catalog, or render them directly without disk install.
/// The on-disk discovery flow remains the authoritative path for normal use.
pub fn bundled_skills() -> Vec<LoadedSkill> {
    BUNDLED_SKILL_SOURCES
        .iter()
        .map(|source| {
            let (metadata, body) = parse_skill_file(source.content).unwrap_or_else(|err| {
                panic!("bundled skill {} is malformed: {err}", source.dir_name)
            });
            assert!(
                is_valid_skill_name(&metadata.name),
                "bundled skill {} has invalid name {}",
                source.dir_name,
                metadata.name
            );
            assert_eq!(
                metadata.name, source.dir_name,
                "bundled skill {} has mismatched frontmatter name {}",
                source.dir_name, metadata.name
            );
            let virtual_root = PathBuf::from(BUNDLED_VIRTUAL_ROOT);
            let base_dir = virtual_root.join(source.dir_name);
            let location = base_dir.join(SKILL_FILE);
            LoadedSkill {
                summary: SkillSummary {
                    name: metadata.name,
                    description: metadata.description,
                    when_to_use: metadata.when_to_use,
                    source: SkillSource::User,
                    location,
                    disabled: false,
                    manifest: None,
                    context_mode: metadata.context_mode,
                },
                base_dir,
                body,
                hooks: metadata.hooks,
            }
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillSource {
    CompatUser,
    User,
    /// Skill loaded from a `SkillsConfig::extra_roots` entry — typically a
    /// team-shared mount or vendored git submodule. Ranks above the
    /// personal `user_dir` (so an explicitly configured shared root
    /// wins over the per-user default) but below project-local skills
    /// so a workspace's `.squeezy/skills/` still overrides on collision.
    ExtraRoot,
    CompatProject,
    Project,
}

impl SkillSource {
    pub(crate) const fn precedence(self) -> u8 {
        match self {
            Self::CompatUser => 0,
            Self::User => 1,
            Self::ExtraRoot => 2,
            Self::CompatProject => 3,
            Self::Project => 4,
        }
    }

    /// Stable kebab/snake-case label for this source. Used by JSON and
    /// human renderers across the workspace (e.g.
    /// `squeezy config browse`) so callers don't have to re-derive a
    /// display string from the enum variant.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CompatUser => "compat_user",
            Self::User => "user",
            Self::ExtraRoot => "extra_root",
            Self::CompatProject => "compat_project",
            Self::Project => "project",
        }
    }
}

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
    fn is_empty(&self) -> bool {
        self.tool_deps.is_empty() && self.icon.is_none() && self.prompt_hint.is_none()
    }
}

/// Execution context a skill declares in its `SKILL.md` frontmatter.
///
/// `Inline` (the default and current behaviour) injects the skill body
/// into the main turn's instructions, so any tool calls the model issues
/// run on the parent thread. `Fork` is the marker that the skill author
/// expects this body to be dispatched into a clean subagent — the
/// downstream dispatcher (see `F10-cc-disk-loaded-agent-definitions`)
/// will read the field once it lands. Until then this surfaces the
/// declaration so callers can branch on it without re-parsing the
/// frontmatter, and an unknown value (`context: bogus`) is mapped to
/// `Inline` with a `tracing::warn!` rather than rejecting the skill.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillContextMode {
    #[default]
    Inline,
    Fork,
}

impl SkillContextMode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Inline => "inline",
            Self::Fork => "fork",
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<SkillManifest>,
    /// Execution context declared in the skill frontmatter; defaults to
    /// `Inline` when the field is absent or unrecognised.
    #[serde(default, skip_serializing_if = "is_inline_context_mode")]
    pub context_mode: SkillContextMode,
}

fn is_inline_context_mode(mode: &SkillContextMode) -> bool {
    matches!(mode, SkillContextMode::Inline)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedSkill {
    pub summary: SkillSummary,
    pub base_dir: PathBuf,
    pub body: String,
    /// Hook specs parsed from `hooks:` frontmatter, indexed by event name.
    /// Empty when the skill declares no hooks. Registered against a
    /// `HookRegistry` via [`register_skill_hooks`] when the skill becomes
    /// active.
    pub hooks: BTreeMap<HookEvent, Vec<SkillHookMatcher>>,
}

/// One matcher clause inside a per-event hook block.
///
/// `matcher` is an optional tool-name filter consulted by the handler at
/// dispatch time — `None` (or the literal `"*"`) means every payload for
/// the event fires this matcher's hooks. Unknown events and unknown hook
/// kinds drop with a `tracing::warn!` rather than failing the skill load,
/// matching the broader frontmatter parsing contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillHookMatcher {
    pub matcher: Option<String>,
    pub hooks: Vec<SkillHookSpec>,
}

/// One concrete hook handler declaration.
///
/// Today only the `command` kind is implemented: it shells out to the
/// declared `command` line, resolved relative to the skill's `base_dir`
/// when the path is relative. `once: true` semantics live in the handler
/// (self-skipped after the first *successful* run) so the registry stays
/// agnostic; a failed first run is retried on the next dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillHookSpec {
    pub command: String,
    pub once: bool,
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
            manifest,
            context_mode: _,
        } = &self.summary;
        let when_to_use = when_to_use
            .as_ref()
            .map(|value| format!("\n<when_to_use>{}</when_to_use>", xml_escape(value)))
            .unwrap_or_default();
        let manifest_block = manifest
            .as_ref()
            .map(render_manifest_block)
            .unwrap_or_default();
        format!(
            "<skill name=\"{}\" source=\"{}\">\n<description>{}</description>{when_to_use}\n<location>{}</location>\n<base_directory>{}</base_directory>{manifest_block}\n<content>\n{}\n</content>\n</skill>",
            xml_escape(name),
            source.as_str(),
            xml_escape(description),
            location.display(),
            self.base_dir.display(),
            escape_body_breakouts(self.body.trim())
        )
    }

    /// Metadata-only counterpart to [`Self::prompt_block`].
    ///
    /// Emits the same outer `<skill>` shape (name, source, description,
    /// optional `when_to_use`, `location`, `base_directory`, manifest)
    /// but omits the skill body. A short `<instruction>` tells the model
    /// to call `load_skill` when the full instructions are needed. This
    /// is the default rendering path for active skills; the legacy
    /// inline-body form is gated behind `[skills] inline = true`.
    pub fn metadata_block(&self) -> String {
        let SkillSummary {
            name,
            description,
            when_to_use,
            source,
            location,
            disabled: _,
            manifest,
            context_mode: _,
        } = &self.summary;
        let when_to_use = when_to_use
            .as_ref()
            .map(|value| format!("\n<when_to_use>{}</when_to_use>", xml_escape(value)))
            .unwrap_or_default();
        let manifest_block = manifest
            .as_ref()
            .map(render_manifest_block)
            .unwrap_or_default();
        let instruction = format!(
            "Skill body omitted; call load_skill with name \"{}\" to load the full instructions.",
            name
        );
        format!(
            "<skill name=\"{}\" source=\"{}\" body=\"omitted\">\n<description>{}</description>{when_to_use}\n<location>{}</location>\n<base_directory>{}</base_directory>{manifest_block}\n<instruction>{}</instruction>\n</skill>",
            xml_escape(name),
            source.as_str(),
            xml_escape(description),
            location.display(),
            self.base_dir.display(),
            xml_escape(&instruction),
        )
    }
}

fn render_manifest_block(manifest: &SkillManifest) -> String {
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
    /// When `true`, [`SkillCatalog::render_active_skills`] uses the
    /// legacy inline-body render; when `false` (the default) it emits
    /// metadata-only blocks and relies on the `load_skill` tool to fetch
    /// the body on demand.
    inline: bool,
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
            active_budget_chars: defaults.active_budget_effective_chars(),
            active_body_cap_chars: defaults.active_body_cap_chars,
            preamble_enabled: defaults.preamble_enabled,
            preamble_budget_chars: defaults.preamble_budget_effective_chars(),
            inline: defaults.inline,
        }
    }
}

impl SkillCatalog {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn discover(workspace_root: &Path, config: &SkillsConfig) -> Self {
        let mut catalog = Self {
            // Resolve the active and preamble budgets via the configured
            // mode so they can scale with `model_context_window`. The
            // catalog stores the post-resolution chars so render-time stays
            // a hot, allocation-free char-count comparison.
            active_budget_chars: config.active_budget_effective_chars(),
            active_body_cap_chars: config.active_body_cap_chars,
            preamble_enabled: config.preamble_enabled,
            preamble_budget_chars: config.preamble_budget_effective_chars(),
            inline: config.inline,
            ..Self::default()
        };
        catalog.discover_dir(&config.compat_user_dir, SkillSource::CompatUser);
        catalog.discover_dir(&config.user_dir, SkillSource::User);
        catalog.discover_extra_roots(&config.extra_roots);
        catalog.discover_dir(
            &workspace_root.join(COMPAT_PROJECT_SKILLS_DIR),
            SkillSource::CompatProject,
        );
        catalog.discover_dir(
            &workspace_root.join(PROJECT_SKILLS_DIR),
            SkillSource::Project,
        );
        // Monorepo support: walk strict ancestors of `workspace_root`
        // looking for sibling project skill roots. Inner-scope skills
        // already loaded above shadow any same-name skill discovered in
        // an outer ancestor; the walk stops at the first ancestor that
        // contains a `.git` entry (a file for git worktrees, a directory
        // for vanilla checkouts) or at the filesystem root if no marker
        // is found. The shadow set grows as each ancestor contributes
        // new skills so closer ancestors always win over farther ones.
        let mut shadow_set: BTreeSet<String> = catalog.skills.keys().cloned().collect();
        for ancestor in ancestor_project_roots(workspace_root) {
            let before: BTreeSet<String> = catalog.skills.keys().cloned().collect();
            catalog.discover_dir_filtered(
                &ancestor.join(COMPAT_PROJECT_SKILLS_DIR),
                SkillSource::CompatProject,
                Some(&shadow_set),
            );
            catalog.discover_dir_filtered(
                &ancestor.join(PROJECT_SKILLS_DIR),
                SkillSource::Project,
                Some(&shadow_set),
            );
            for name in catalog.skills.keys() {
                if !before.contains(name) {
                    shadow_set.insert(name.clone());
                }
            }
        }
        catalog.apply_config_rules(workspace_root, &config.config);
        catalog.warn_trigger_collisions();
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
                    let manifest = summary
                        .manifest
                        .as_ref()
                        .map(|manifest| {
                            json!({
                                "tool_deps": manifest.tool_deps,
                                "icon": manifest.icon,
                                "prompt_hint": manifest.prompt_hint,
                            })
                        });
                    json!({
                        "name": summary.name,
                        "description": summary.description,
                        "when_to_use": summary.when_to_use,
                        "source": summary.source.as_str(),
                        "location": summary.location,
                        "disabled": summary.disabled,
                        "manifest": manifest,
                        "context_mode": summary.context_mode.as_str(),
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
        let (metadata, body) = parse_skill_file(&content).map_err(SqueezyError::Tool)?;
        let loaded = LoadedSkill {
            summary: entry.summary.clone(),
            base_dir: entry.base_dir.clone(),
            body,
            hooks: metadata.hooks,
        };
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(name.to_string(), loaded.clone());
        }
        Ok(loaded)
    }

    pub fn activate_for_input(&self, input: &str) -> Result<SkillActivation> {
        let mut task = input.to_string();
        let mut candidates: Vec<(String, SkillActivationKind)> = Vec::new();
        if let Some((name, rest)) = parse_explicit_skill_command(input) {
            candidates.push((name.to_string(), SkillActivationKind::Explicit));
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
                candidates.push((entry.summary.name.clone(), SkillActivationKind::Trigger));
            }
        }

        let mut seen = BTreeSet::new();
        let mut loaded = Vec::new();
        let mut kinds = Vec::new();
        for (name, kind) in candidates {
            if seen.insert(name.clone()) {
                loaded.push(self.load(&name)?);
                kinds.push(kind);
            }
        }
        Ok(SkillActivation {
            task_input: task,
            skills: loaded,
            kinds,
        })
    }

    pub fn render_active_skills(&self, skills: &[LoadedSkill]) -> Option<String> {
        if self.inline {
            // Legacy behavior: inline each activated skill's full body
            // into the system prompt, with budget-aware stub fallback.
            render::render_active_skills(
                skills,
                self.active_budget_chars,
                self.active_body_cap_chars,
            )
        } else {
            // Default behavior: emit metadata-only blocks. The model
            // calls `load_skill` when it needs the body.
            render::render_active_skills_metadata(skills, self.active_budget_chars)
        }
    }

    /// Render the activated fork-mode skills (those whose frontmatter
    /// declares `context: fork`) into a `<fork_skills>` system block.
    /// Returns `None` when `skills` is empty.
    ///
    /// Fork-mode skills are intentionally rendered separately from
    /// `<active_skills>` because the design intent is for the model to
    /// dispatch them through a focused subagent (via the existing
    /// `delegate` tool) rather than executing the body inline.
    pub fn render_fork_skills(&self, skills: &[LoadedSkill]) -> Option<String> {
        render::render_fork_skills(skills, self.active_budget_chars, self.active_body_cap_chars)
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

    /// Register `hooks:` declared in every non-disabled discovered
    /// skill's frontmatter against `registry`. Loads each skill once
    /// (so the existing `load`-cache picks them up for the rest of the
    /// session) and aggregates the handler count for telemetry.
    ///
    /// Caller-side gating (e.g. `[skills] hooks_enabled = false`) is
    /// expected to skip the whole call — this method intentionally has
    /// no opinion on whether hooks should be active because that policy
    /// belongs to the agent constructor.
    pub fn register_hooks(&self, registry: &mut HookRegistry) -> usize {
        let mut installed = 0;
        for entry in self.skills.values() {
            if entry.summary.disabled {
                continue;
            }
            match self.load(&entry.summary.name) {
                Ok(loaded) => {
                    if !loaded.hooks.is_empty() {
                        installed += register_skill_hooks(&loaded, registry);
                    }
                }
                Err(error) => {
                    warn!(
                        target: "squeezy_skills",
                        skill = %entry.summary.name,
                        error = %error,
                        "skipping skill hook registration: load failed"
                    );
                }
            }
        }
        installed
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

    /// Walk each configured extra skills root. Unlike the default user
    /// and project roots, these are explicitly opted-in by the operator
    /// via `SkillsConfig::extra_roots`, so a missing or non-directory
    /// entry is reported with a `tracing::warn!` rather than skipped
    /// silently — the typical failure mode is a network mount that did
    /// not come up or a stale path baked into a shared settings file.
    /// Discovery continues for the remaining roots regardless.
    fn discover_extra_roots(&mut self, roots: &[PathBuf]) {
        for root in roots {
            match fs::metadata(root) {
                Ok(metadata) if metadata.is_dir() => {
                    self.discover_dir(root, SkillSource::ExtraRoot);
                }
                Ok(_) => {
                    warn!(
                        target: "squeezy_skills",
                        root = %root.display(),
                        "ignoring skills.extra_roots entry: not a directory"
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    warn!(
                        target: "squeezy_skills",
                        root = %root.display(),
                        "ignoring skills.extra_roots entry: directory does not exist"
                    );
                }
                Err(error) => {
                    warn!(
                        target: "squeezy_skills",
                        root = %root.display(),
                        error = %error,
                        "ignoring skills.extra_roots entry: cannot stat"
                    );
                }
            }
        }
    }

    fn discover_dir(&mut self, dir: &Path, source: SkillSource) {
        self.discover_dir_filtered(dir, source, None);
    }

    /// Like [`Self::discover_dir`] but optionally skips skills whose
    /// `name` appears in `shadow_set`.
    ///
    /// Used by the ancestor walk in [`Self::discover`] to enforce
    /// "inner-scope skills win over outer ancestors on same-name
    /// collision" without dragging that policy into the cwd-local
    /// discovery path. Shadowed skills are logged at debug level rather
    /// than warn — being hidden by an inner copy is the expected
    /// monorepo behavior, not a misconfiguration.
    fn discover_dir_filtered(
        &mut self,
        dir: &Path,
        source: SkillSource,
        shadow_set: Option<&BTreeSet<String>>,
    ) {
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
            if let Some(set) = shadow_set
                && set.contains(&metadata.name)
            {
                tracing::debug!(
                    target: "squeezy_skills",
                    name = %metadata.name,
                    path = %skill_path.display(),
                    "ancestor skill shadowed by inner-scope skill of same name"
                );
                continue;
            }
            let manifest = load_manifest(&path);
            let summary = SkillSummary {
                name: metadata.name.clone(),
                description: metadata.description,
                when_to_use: metadata.when_to_use,
                source,
                location: skill_path,
                disabled: false,
                manifest,
                context_mode: metadata.context_mode,
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
                if existing.summary.source.precedence() > entry.summary.source.precedence() =>
            {
                warn!(
                    target: "squeezy_skills",
                    name = %entry.summary.name,
                    kept = %existing.summary.location.display(),
                    kept_source = existing.summary.source.as_str(),
                    shadowed = %entry.summary.location.display(),
                    shadowed_source = entry.summary.source.as_str(),
                    "skill name reused at lower precedence; lower-precedence copy will not load"
                );
            }
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
            Some(existing) => {
                warn!(
                    target: "squeezy_skills",
                    name = %entry.summary.name,
                    overridden = %existing.summary.location.display(),
                    overridden_source = existing.summary.source.as_str(),
                    overriding = %entry.summary.location.display(),
                    overriding_source = entry.summary.source.as_str(),
                    "skill name reused at higher precedence; lower-precedence copy will not load"
                );
                self.ambiguous_names.remove(&entry.summary.name);
                if let Ok(mut cache) = self.cache.lock() {
                    cache.remove(&entry.summary.name);
                }
                self.skills.insert(entry.summary.name.clone(), entry);
            }
            None => {
                self.skills.insert(entry.summary.name.clone(), entry);
            }
        }
    }

    fn warn_trigger_collisions(&self) {
        let mut by_trigger: BTreeMap<String, Vec<&str>> = BTreeMap::new();
        for entry in self.skills.values() {
            for trigger in &entry.triggers {
                let normalized = trigger.trim().to_ascii_lowercase();
                if normalized.is_empty() {
                    continue;
                }
                by_trigger
                    .entry(normalized)
                    .or_default()
                    .push(entry.summary.name.as_str());
            }
        }
        for (trigger, mut names) in by_trigger {
            if names.len() < 2 {
                continue;
            }
            names.sort_unstable();
            names.dedup();
            if names.len() < 2 {
                continue;
            }
            warn!(
                target: "squeezy_skills",
                trigger = %trigger,
                skills = ?names,
                "duplicate skill trigger across skills; activation will load every match"
            );
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
            inline: self.inline,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillActivation {
    pub task_input: String,
    pub skills: Vec<LoadedSkill>,
    /// Activation reason per entry in `skills`, same length and order. Lets
    /// callers emit `skill.activation.kind` telemetry so trigger-vs-explicit
    /// hit rates are observable without re-deriving from the input.
    pub kinds: Vec<SkillActivationKind>,
}

/// Why a skill was activated for a turn. Stays in sync with the
/// `skill.activation.kind` telemetry label so producers and consumers agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillActivationKind {
    /// User typed `/skill <name> ...`.
    Explicit,
    /// A configured trigger phrase matched the user's input.
    Trigger,
    /// Inferred from a shell command that touched a skill's `scripts/` dir
    /// or `SKILL.md`. Surfaced from the shell tool, not `activate_for_input`.
    ImplicitShell,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SkillMetadata {
    name: String,
    description: String,
    when_to_use: Option<String>,
    triggers: Vec<String>,
    context_mode: SkillContextMode,
    hooks: BTreeMap<HookEvent, Vec<SkillHookMatcher>>,
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
    let mut context_mode = SkillContextMode::Inline;
    let mut hooks: BTreeMap<HookEvent, Vec<SkillHookMatcher>> = BTreeMap::new();
    let mut list_key: Option<&str> = None;
    let mut idx = 0;

    while idx < lines.len() {
        let raw = lines[idx];
        idx += 1;
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
            "context" => {
                context_mode = parse_context_mode(unquote(value));
            }
            "hooks" if value.is_empty() => {
                let consumed = parse_hooks_block(&lines[idx..], &mut hooks);
                idx += consumed;
            }
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
        context_mode,
        hooks,
    })
}

/// Parse the `context:` frontmatter value into a [`SkillContextMode`].
///
/// Only `fork` (case-insensitive) maps to [`SkillContextMode::Fork`].
/// Anything else — including the explicit `inline` literal, an empty
/// string, or a typo like `bogus` — falls back to
/// [`SkillContextMode::Inline`]. Unknown values warn so authors can
/// catch typos without losing the skill.
fn parse_context_mode(value: &str) -> SkillContextMode {
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "inline" => SkillContextMode::Inline,
        "fork" => SkillContextMode::Fork,
        other => {
            warn!(
                target: "squeezy_skills",
                value = %other,
                "unrecognised skill context mode; defaulting to inline"
            );
            SkillContextMode::Inline
        }
    }
}

/// Parse the nested block following a top-level `hooks:` key.
///
/// Returns how many input lines were consumed so the caller can advance
/// its cursor past the block. The block ends at the first non-blank line
/// with zero indentation (a new top-level frontmatter key). Indent is
/// the structural anchor: event names sit at the shallowest indent,
/// `- matcher:` clauses one level deeper, and per-spec key/value pairs
/// inside a matcher's `hooks:` sub-list one level deeper still.
/// Unrecognised event names and hook kinds log a `tracing::warn!` and
/// drop, matching how unknown top-level keys are handled today.
fn parse_hooks_block(rest: &[&str], out: &mut BTreeMap<HookEvent, Vec<SkillHookMatcher>>) -> usize {
    let mut consumed = 0;
    let mut current_event: Option<HookEvent> = None;
    let mut current_matchers: Vec<SkillHookMatcher> = Vec::new();
    let mut event_indent: Option<usize> = None;
    let mut matcher_indent: Option<usize> = None;

    fn flush_event(
        out: &mut BTreeMap<HookEvent, Vec<SkillHookMatcher>>,
        event: &mut Option<HookEvent>,
        matchers: &mut Vec<SkillHookMatcher>,
    ) {
        if !matchers.is_empty()
            && let Some(ev) = event.take()
        {
            out.entry(ev).or_default().append(matchers);
        }
        *event = None;
        matchers.clear();
    }

    for line in rest {
        let raw = line.trim_end();
        if raw.trim().is_empty() || raw.trim_start().starts_with('#') {
            consumed += 1;
            continue;
        }
        let indent = raw.len() - raw.trim_start().len();
        if indent == 0 {
            break;
        }
        consumed += 1;
        let trimmed = raw.trim_start();

        // Establish the event indent on the first non-blank child of
        // the `hooks:` block; any line at that same indent is treated
        // as a new event name.
        let level = event_indent.get_or_insert(indent);
        if indent == *level {
            flush_event(out, &mut current_event, &mut current_matchers);
            matcher_indent = None;
            if let Some((key, value)) = trimmed.split_once(':')
                && value.trim().is_empty()
            {
                match parse_hook_event(key.trim()) {
                    Some(event) => current_event = Some(event),
                    None => warn!(
                        target: "squeezy_skills",
                        event = %key.trim(),
                        "ignoring unknown skill hook event"
                    ),
                }
            }
            continue;
        }

        // A `- matcher: ...` clause opens a new matcher under the
        // current event. The matcher indent is locked on first sight
        // so later `command:`/`once:` lines can be told apart from a
        // sibling matcher reliably.
        if let Some(item) = trimmed.strip_prefix("- ")
            && let Some((key, value)) = item.split_once(':')
            && key.trim() == "matcher"
        {
            matcher_indent = Some(indent);
            let raw_match = unquote(value.trim()).to_string();
            let matcher = if raw_match.is_empty() || raw_match == "*" {
                None
            } else {
                Some(raw_match)
            };
            current_matchers.push(SkillHookMatcher {
                matcher,
                hooks: Vec::new(),
            });
            continue;
        }

        // A `- type: command` (or any `- key: value`) at indent
        // strictly greater than the matcher line opens a new spec on
        // the active matcher, then the same line's `type:` is parsed
        // as the spec's first key.
        if let Some(item) = trimmed.strip_prefix("- ")
            && matcher_indent.is_some_and(|m| indent > m)
            && let Some(matcher) = current_matchers.last_mut()
        {
            matcher.hooks.push(SkillHookSpec {
                command: String::new(),
                once: false,
            });
            if let Some(spec) = matcher.hooks.last_mut() {
                apply_spec_kv(spec, item);
            }
            continue;
        }

        // Plain `key: value` line below a `- type:` opener — apply to
        // the most recent spec on the current matcher.
        if let Some(matcher) = current_matchers.last_mut()
            && let Some(spec) = matcher.hooks.last_mut()
        {
            apply_spec_kv(spec, trimmed);
        }
    }

    flush_event(out, &mut current_event, &mut current_matchers);
    consumed
}

/// Apply a single `key: value` token to an in-progress spec.
fn apply_spec_kv(spec: &mut SkillHookSpec, line: &str) {
    let Some((key, value)) = line.split_once(':') else {
        return;
    };
    let value = unquote(value.trim());
    match key.trim() {
        "command" => spec.command = value.to_string(),
        "once" => spec.once = matches!(value, "true" | "yes" | "1"),
        "type" if value != "command" => {
            warn!(
                target: "squeezy_skills",
                kind = %value,
                "ignoring unsupported skill hook kind"
            );
        }
        _ => {}
    }
}

/// Map a YAML key to a [`HookEvent`]. Accepts the canonical PascalCase
/// names used in [`HookEvent`] plus the `snake_case` aliases produced by
/// serde so frontmatter authors can use either convention.
fn parse_hook_event(name: &str) -> Option<HookEvent> {
    match name {
        "PreTurn" | "pre_turn" => Some(HookEvent::PreTurn),
        "PreToolUse" | "pre_tool_use" => Some(HookEvent::PreToolUse),
        "PostToolUse" | "post_tool_use" => Some(HookEvent::PostToolUse),
        "PostTool" | "post_tool" => Some(HookEvent::PostTool),
        "PreCompact" | "pre_compact" => Some(HookEvent::PreCompact),
        "PostCompact" | "post_compact" => Some(HookEvent::PostCompact),
        "SubagentStart" | "subagent_start" => Some(HookEvent::SubagentStart),
        "PermissionRequest" | "permission_request" => Some(HookEvent::PermissionRequest),
        _ => None,
    }
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

/// Strict ancestors of `workspace_root` that should be scanned for
/// monorepo sibling skill roots.
///
/// Walks from `workspace_root`'s parent up until the first directory
/// that contains a `.git` entry (file for git worktrees and submodules,
/// directory for vanilla checkouts) is reached, or until the filesystem
/// root is hit when no marker is found. The returned list is in
/// inner-to-outer order so callers can register a closer ancestor's
/// skills before a farther ancestor's same-name copy can shadow them.
///
/// Returns an empty vector when `workspace_root` is itself a git root —
/// at that point all project skills already live under cwd and there is
/// no parent repository to pick up siblings from.
fn ancestor_project_roots(workspace_root: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if is_git_root(workspace_root) {
        return roots;
    }
    let mut current = workspace_root.to_path_buf();
    while let Some(parent) = current.parent().map(Path::to_path_buf) {
        // `Path::parent` returns `None` at the filesystem root, but
        // canonicalization edge cases on Windows can yield a self-parent
        // entry — bail explicitly so the walk always terminates.
        if parent == current {
            break;
        }
        let stop = is_git_root(&parent);
        roots.push(parent.clone());
        if stop {
            break;
        }
        current = parent;
    }
    roots
}

/// True when `dir` looks like a git repository checkout root.
///
/// Accepts both the standard `.git/` directory layout and the worktree
/// or submodule `.git` *file* form so the ancestor walk halts at either
/// flavour. Uses `try_exists` so a transient I/O error doesn't fall
/// through to "no marker"; on failure the walk treats the directory as
/// non-root and keeps climbing, which is the conservative choice when
/// the alternative is to halt early and lose a sibling skill set.
fn is_git_root(dir: &Path) -> bool {
    dir.join(".git").try_exists().unwrap_or(false)
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

/// Read the optional `skill.toml` sidecar next to a skill directory.
///
/// Returns `None` when the file is absent, unreadable, malformed, or
/// empty after parsing. Missing-file is the common case and not logged;
/// every other failure path is logged at WARN so a sidecar typo never
/// silently disables a skill's catalog routing.
fn load_manifest(base_dir: &Path) -> Option<SkillManifest> {
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

/// Parse `SKILL.md` content and report whether it satisfies the
/// catalog's frontmatter and naming rules.
///
/// Reuses the same parser the discovery walker uses, so a `Ok(name)`
/// means the file is byte-for-byte loadable into the catalog. The
/// returned `name` is the canonical skill name read from the
/// frontmatter `name:` field, useful for the CLI's `validate`
/// subcommand to surface what is being validated.
pub fn validate_skill_md(content: &str) -> std::result::Result<String, String> {
    let (metadata, _body) = parse_skill_file(content)?;
    if !is_valid_skill_name(&metadata.name) {
        return Err(format!(
            "invalid skill name {:?}: must start with a lowercase ASCII letter and contain only lowercase letters, digits, '-', or '_'",
            metadata.name
        ));
    }
    Ok(metadata.name)
}

pub(crate) fn parse_skill_manifest(content: &str) -> std::result::Result<SkillManifest, String> {
    toml::from_str::<SkillManifest>(content).map_err(|error| error.to_string())
}

/// [`HookHandler`] implementation that fires a skill's declared shell
/// command when its event matches.
///
/// `event` is the variant from the skill's frontmatter; the handler
/// fast-paths-returns `HookResult::allow()` without spawning a process
/// when `ctx.event` doesn't match, so registering hooks for one event
/// stays cheap on unrelated dispatches. `matcher` (when present) is
/// matched against the `tool_name` payload field on tool-scoped events;
/// `None` means the handler fires for every payload of the event.
/// `base_dir` is the skill's filesystem root and lets the handler
/// resolve relative `command` paths the same way CC resolves
/// `${CLAUDE_PLUGIN_ROOT}`.
pub struct SkillHookHandler {
    skill_name: String,
    event: HookEvent,
    matcher: Option<String>,
    spec: SkillHookSpec,
    base_dir: PathBuf,
    /// Tracks whether a `once: true` hook has already succeeded in this
    /// session. Set only after a successful exit so a failed first run is
    /// retried. Held behind a `Mutex` so the trait method stays `&self`
    /// while still allowing in-place mutation across dispatches.
    fired: Mutex<bool>,
}

impl SkillHookHandler {
    pub fn new(
        skill_name: String,
        event: HookEvent,
        matcher: Option<String>,
        spec: SkillHookSpec,
        base_dir: PathBuf,
    ) -> Self {
        Self {
            skill_name,
            event,
            matcher,
            spec,
            base_dir,
            fired: Mutex::new(false),
        }
    }
}

impl HookHandler for SkillHookHandler {
    fn handle(&self, ctx: &HookContext) -> HookResult {
        if ctx.event != self.event {
            return HookResult::allow();
        }
        let payload_json = ctx.payload_json();
        if let Some(needle) = self.matcher.as_deref() {
            let tool = payload_json
                .get("tool_name")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            if tool != needle {
                return HookResult::allow();
            }
        }
        if self.spec.once
            && let Ok(fired) = self.fired.lock()
            && *fired
        {
            return HookResult::allow();
        }

        // Resolve the command path against the skill's base_dir when
        // relative. The payload is piped through `SQUEEZY_HOOK_PAYLOAD`
        // as JSON projected from the typed `HookPayload`, matching the
        // hook-engine contract documented on `HookContext`.
        let trimmed = self.spec.command.trim();
        if trimmed.is_empty() {
            warn!(
                target: "squeezy_skills",
                skill = %self.skill_name,
                "skipping skill hook with empty command"
            );
            return HookResult::allow();
        }
        let payload = payload_json.to_string();
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(trimmed)
            .current_dir(&self.base_dir)
            .env("SQUEEZY_SKILL_DIR", &self.base_dir)
            .env("SQUEEZY_SKILL_NAME", &self.skill_name)
            .env("SQUEEZY_HOOK_PAYLOAD", payload);
        match command.status() {
            Ok(status) if status.success() => {
                // Only mark a `once: true` hook as fired once it has
                // actually succeeded, so a failed first run can retry.
                if self.spec.once
                    && let Ok(mut fired) = self.fired.lock()
                {
                    *fired = true;
                }
                HookResult::allow()
            }
            Ok(status) => {
                warn!(
                    target: "squeezy_skills",
                    skill = %self.skill_name,
                    code = ?status.code(),
                    "skill hook exited non-zero"
                );
                HookResult::deny(format!(
                    "skill `{}` hook denied the action",
                    self.skill_name
                ))
            }
            Err(error) => {
                warn!(
                    target: "squeezy_skills",
                    skill = %self.skill_name,
                    error = %error,
                    "skill hook failed to spawn"
                );
                HookResult::allow()
            }
        }
    }
}

/// Register every hook declared in a [`LoadedSkill`]'s frontmatter
/// against the given [`HookRegistry`].
///
/// Returns the number of [`SkillHookHandler`]s installed so callers can
/// log the activation count alongside the skill name. The registry takes
/// ownership of each handler; deregistering individual skill hooks is
/// not yet implemented because the registry stores erased boxed
/// handlers — that surface is left to a follow-up when the agent loop
/// learns to drop hooks on skill deactivation.
pub fn register_skill_hooks(skill: &LoadedSkill, registry: &mut HookRegistry) -> usize {
    let mut installed = 0;
    for (event, matchers) in &skill.hooks {
        for matcher in matchers {
            for spec in &matcher.hooks {
                registry.register(Box::new(SkillHookHandler::new(
                    skill.summary.name.clone(),
                    *event,
                    matcher.matcher.clone(),
                    spec.clone(),
                    skill.base_dir.clone(),
                )));
                installed += 1;
            }
        }
    }
    if installed > 0 {
        tracing::info!(
            target: "squeezy_skills",
            skill = %skill.summary.name,
            installed,
            "registered skill frontmatter hooks"
        );
    }
    installed
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
