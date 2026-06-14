use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read as _,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use squeezy_core::{Result, SkillConfigEntry, SkillsConfig, SqueezyError};
use squeezy_hooks::{HookContext, HookEvent, HookHandler, HookPayload, HookRegistry, HookResult};
use tracing::warn;

/// Default number of seconds to wait for a skill hook command before
/// killing it and returning a deny result. Avoids blocking the agent
/// turn on a hook that hangs (e.g. `sleep infinity`, blocked I/O).
pub const DEFAULT_HOOK_TIMEOUT_SECS: u64 = 30;

pub mod help;
pub mod implicit;
pub mod prompt_templates;
pub mod render;

pub use help::{
    APPROVAL_POLICY_DOC_PATH, BundledDoc, DocSection, HelpAnswer, HelpAnswerSource, HelpCitation,
    HelpStatus, SQUEEZY_REPO_SLUG, SQUEEZY_REPO_URL, SQUEEZY_WEBSITE_URL, SqueezyHelp, bundled_doc,
    bundled_doc_paths, bundled_docs, chunk_doc_sections, matches_squeezy_help_input,
    relevant_doc_sections_for_input, relevant_docs_for_input, slash_command_help_names,
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
    BundledSkillSource {
        dir_name: "trace-symbol",
        content: include_str!("../builtin/trace-symbol/SKILL.md"),
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
    pub const fn as_str(self) -> &'static str {
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

/// Per-skill context cost breakdown produced by [`SkillCatalog::context_breakdown`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillContextBreakdown {
    pub name: String,
    pub description: String,
    /// `true` when the body is materialized in this session's cache.
    pub loaded: bool,
    /// Byte size of the always-present metadata block (no body).
    pub metadata_bytes: usize,
    /// Body byte size: exact for loaded skills, on-disk `SKILL.md` size
    /// (first-load cost) otherwise.
    pub body_bytes: usize,
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

/// What to do when a skill hook command fails to spawn (e.g. missing `sh` on
/// Windows). Defaults to `Allow` to preserve existing behavior, but operators
/// can set `Deny` for policy-enforcement hooks where a spawn failure must not
/// silently become permissive.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HookFailurePolicy {
    #[default]
    Allow,
    Deny,
}

/// One concrete hook handler declaration.
///
/// Today only the `command` kind is implemented: it shells out to the
/// declared `command` line, resolved relative to the skill's `base_dir`
/// when the path is relative. `once: true` semantics live in the handler
/// (self-skipped after the first *successful* run) so the registry stays
/// agnostic; a failed first run is retried on the next dispatch.
///
/// `kind_valid` is `false` when the spec's `type:` field was set to an
/// unsupported value. Such specs are dropped before handler registration
/// so a frontmatter block with `type: webhook` + `command: ...` does
/// not silently execute as a shell command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillHookSpec {
    pub command: String,
    pub once: bool,
    /// Maximum seconds to wait for the hook command before killing it
    /// and returning a deny result. Defaults to
    /// [`DEFAULT_HOOK_TIMEOUT_SECS`] when `None`.
    pub timeout_secs: Option<u64>,
    /// When `false` (fail-closed), a spawn error or `wait()` error returns
    /// a deny result instead of silently allowing execution. Defaults to
    /// `true` for backward-compatibility with the original fail-open
    /// behaviour; set `fail_open = false` in the frontmatter for enforcement
    /// hooks that must not silently pass when the interpreter is missing.
    ///
    /// **Note**: a hook that exceeds `timeout_secs` always returns deny
    /// regardless of `fail_open`, because a hung hook is an anomaly that
    /// should not silently pass in either audit or enforcement configurations.
    pub fail_open: bool,
    /// `false` when an unsupported `type:` was declared; prevents
    /// execution even if a `command:` line was also present.
    pub kind_valid: bool,
    /// Policy applied when the hook command fails to spawn (e.g. shell not in
    /// `PATH`). `Allow` (default) preserves backward compatibility. `Deny`
    /// makes spawn failures behave like a non-zero exit, preventing a missing
    /// shell from silently neutralizing a policy hook.
    pub failure_policy: HookFailurePolicy,
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
        render_metadata_block(&self.summary, &self.base_dir)
    }
}

/// Render the metadata-only `<skill>` block for a summary without needing the
/// body. Shared by [`LoadedSkill::metadata_block`] and the `/context`
/// accounting view, which sizes this block for every discovered skill
/// (loaded or not) since it never carries the body.
pub(crate) fn render_metadata_block(summary: &SkillSummary, base_dir: &Path) -> String {
    let SkillSummary {
        name,
        description,
        when_to_use,
        source,
        location,
        disabled: _,
        manifest,
        context_mode: _,
    } = summary;
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
        base_dir.display(),
        xml_escape(&instruction),
    )
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
    /// Normalized triggers that appear in more than one distinct
    /// skill. A trigger that fires for an ambiguous phrase used to
    /// activate every matching skill at once, which silently inflated
    /// the active-skill budget. Activation now skips auto-trigger
    /// matches against entries in this set; users disambiguate with
    /// `/skill <name>` or `load_skill`.
    ambiguous_triggers: BTreeSet<String>,
    implicit_by_scripts_dir: BTreeMap<PathBuf, String>,
    implicit_by_doc_path: BTreeMap<PathBuf, String>,
    /// Lowercase basenames of all indexed doc paths, kept in sync with
    /// `implicit_by_doc_path`. Used as an O(log n) prefilter in
    /// `doc_token_may_match_indexed_path` so we avoid a full key scan on
    /// every reader token — especially helpful on Windows with large or
    /// slow-mount catalogs.
    implicit_doc_filenames: BTreeSet<String>,
    /// All root directories that were probed during the last [`Self::discover`]
    /// call, in discovery order. Stored so callers (e.g. `squeezy skills list`)
    /// can display the scanned roots without triggering a second ancestor walk.
    scanned_roots: Vec<PathBuf>,
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
            ambiguous_triggers: BTreeSet::new(),
            implicit_by_scripts_dir: BTreeMap::new(),
            implicit_by_doc_path: BTreeMap::new(),
            implicit_doc_filenames: BTreeSet::new(),
            scanned_roots: Vec::new(),
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
        // Populate scanned_roots eagerly so callers (e.g. `squeezy skills list`)
        // can display the probed roots without a second ancestor walk.
        catalog.scanned_roots.push(config.compat_user_dir.clone());
        catalog.scanned_roots.push(config.user_dir.clone());
        catalog
            .scanned_roots
            .extend(config.extra_roots.iter().cloned());
        catalog
            .scanned_roots
            .push(workspace_root.join(COMPAT_PROJECT_SKILLS_DIR));
        catalog
            .scanned_roots
            .push(workspace_root.join(PROJECT_SKILLS_DIR));
        catalog.discover_dir(&config.compat_user_dir, SkillSource::CompatUser);
        catalog.discover_dir(&config.user_dir, SkillSource::User);
        // XDG-aware user directory scanned right after the legacy user dir so
        // that skills placed in `$XDG_DATA_HOME/squeezy/skills` are
        // discoverable on Linux without requiring the user to set
        // `SQUEEZY_SKILLS_USER_DIR`.  All skills already present at this point
        // (from both `compat_user_dir` and `user_dir`) shadow same-name
        // entries from the XDG path so higher-priority legacy directories
        // always take precedence on a name collision.
        if let Some(xdg_dir) = &config.xdg_user_dir {
            let shadow_set: BTreeSet<String> = catalog.skills.keys().cloned().collect();
            catalog.discover_dir_filtered(xdg_dir, SkillSource::User, Some(&shadow_set));
        }
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
            // Track ancestor roots for `scanned_roots()` so callers see the
            // full set without a second ancestor walk.
            catalog
                .scanned_roots
                .push(ancestor.join(COMPAT_PROJECT_SKILLS_DIR));
            catalog
                .scanned_roots
                .push(ancestor.join(PROJECT_SKILLS_DIR));
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
        catalog.collect_trigger_collisions();
        catalog.rebuild_implicit_indexes();
        catalog
    }

    /// Returns all root directories that were probed during the last
    /// [`Self::discover`] call, in discovery order. Callers can display
    /// this list (e.g. `squeezy skills list`) without performing a
    /// second ancestor walk.
    pub fn scanned_roots(&self) -> &[PathBuf] {
        &self.scanned_roots
    }

    pub fn summaries(&self) -> Vec<SkillSummary> {
        self.skills
            .values()
            .map(|entry| entry.summary.clone())
            .collect()
    }

    /// Per-skill context cost breakdown for `/context`: one entry per
    /// discovered skill with the byte size of its always-present metadata
    /// block and its body. A skill is `loaded` once its body is materialized in
    /// this session's cache (via `load_skill`, inline activation, or a prior
    /// `load`); the body bytes are exact for loaded skills (from the cache) and
    /// the on-disk `SKILL.md` size for not-yet-loaded skills (the cost a first
    /// load would add). The metadata block never carries the body, so it is
    /// sized for every skill regardless of load state.
    pub fn context_breakdown(&self) -> Vec<SkillContextBreakdown> {
        let loaded_bodies: BTreeMap<String, usize> = self
            .cache
            .lock()
            .map(|cache| {
                cache
                    .iter()
                    .map(|(name, loaded)| (name.clone(), loaded.body.len()))
                    .collect()
            })
            .unwrap_or_default();
        self.skills
            .values()
            .map(|entry| {
                let loaded_body = loaded_bodies.get(&entry.summary.name).copied();
                let body_bytes = match loaded_body {
                    Some(bytes) => bytes,
                    None => fs::metadata(&entry.summary.location)
                        .map(|meta| meta.len() as usize)
                        .unwrap_or(0),
                };
                SkillContextBreakdown {
                    name: entry.summary.name.clone(),
                    description: entry.summary.description.clone(),
                    loaded: loaded_body.is_some(),
                    metadata_bytes: render_metadata_block(&entry.summary, &entry.base_dir).len(),
                    body_bytes,
                }
            })
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
            // Skip any trigger that collides across skills (same
            // normalized phrase declared by two or more distinct
            // skills). Loading every match silently bloated the
            // active-skill budget. Auto-activation requires a unique
            // trigger; users disambiguate with `/skill <name>` or
            // `load_skill`.
            if entry.triggers.iter().any(|trigger| {
                let normalized = trigger.trim().to_ascii_lowercase();
                !self.ambiguous_triggers.contains(&normalized)
                    && input_matches_trigger(&lowered, trigger)
            }) {
                candidates.push((entry.summary.name.clone(), SkillActivationKind::Trigger));
            }
        }

        let mut seen = BTreeSet::new();
        let mut loaded = Vec::new();
        let mut kinds = Vec::new();
        let mut warnings = Vec::new();
        for (name, kind) in candidates {
            if seen.insert(name.clone()) {
                match self.load(&name) {
                    Ok(skill) => {
                        loaded.push(skill);
                        kinds.push(kind);
                    }
                    Err(error)
                        if matches!(
                            kind,
                            SkillActivationKind::Explicit | SkillActivationKind::Trigger
                        ) =>
                    {
                        warnings.push(SkillActivationWarning {
                            name: name.clone(),
                            message: error.to_string(),
                        });
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(SkillActivation {
            task_input: task,
            skills: loaded,
            kinds,
            warnings,
        })
    }

    pub fn render_active_skills(&self, skills: &[LoadedSkill]) -> Option<String> {
        if self.inline {
            render::render_active_skills(
                skills,
                self.active_budget_chars,
                self.active_body_cap_chars,
            )
        } else {
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

    /// Render the active-skill block and return rendering metrics alongside the
    /// output string. The agent uses the metrics to populate skill activation
    /// telemetry without re-walking the inputs. Uses the metrics-aware variant
    /// (`render_active_skills_with_metrics` in render.rs) which returns accurate
    /// inclusion/dropped/body-truncated counts from its own rendering pass.
    pub fn render_active_skills_with_metrics(
        &self,
        skills: &[LoadedSkill],
    ) -> (Option<String>, render::SkillActivationMetrics) {
        if self.inline {
            render::render_active_skills_with_metrics(
                skills,
                self.active_budget_chars,
                self.active_body_cap_chars,
            )
        } else {
            // Metadata-only mode: body_truncated is always 0, but included/dropped
            // must come from the actual rendering pass because low-priority skills
            // can be dropped when the metadata block exceeds the budget.
            let (rendered, mut metrics) = render::render_active_skills_metadata_with_metrics(
                skills,
                self.active_budget_chars,
            );
            metrics.body_truncated = 0;
            (rendered, metrics)
        }
    }

    /// Telemetry-friendly discovery summary: counts by source, disabled count,
    /// and ambiguous count. Callers convert this to a `SkillActivationReport`
    /// for the `skill_activated` telemetry event.
    pub fn discovery_summary(&self) -> SkillDiscoverySummary {
        let mut by_source: BTreeMap<String, u32> = BTreeMap::new();
        let mut disabled = 0u32;
        for entry in self.skills.values() {
            let source_key = entry.summary.source.as_str().to_string();
            *by_source.entry(source_key).or_default() += 1;
            if entry.summary.disabled {
                disabled += 1;
            }
        }
        SkillDiscoverySummary {
            by_source,
            disabled,
            ambiguous: self.ambiguous_names.len() as u32,
        }
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

    /// Normalized triggers declared by more than one distinct skill.
    /// Auto-trigger activation skips these so the parent turn does
    /// not silently load multiple skills for the same input phrase.
    pub fn ambiguous_triggers(&self) -> &BTreeSet<String> {
        &self.ambiguous_triggers
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
            &self.implicit_doc_filenames,
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

    fn collect_trigger_collisions(&mut self) {
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
        let mut collisions: BTreeSet<String> = BTreeSet::new();
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
                "duplicate skill trigger across skills; auto-activation requires explicit selection"
            );
            collisions.insert(trigger);
        }
        self.ambiguous_triggers = collisions;
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
        self.implicit_doc_filenames.clear();
        for entry in self.skills.values() {
            if entry.summary.disabled {
                continue;
            }
            let doc_path = implicit::normalize_path(&entry.summary.location);
            if let Some(fname) = doc_path.file_name().and_then(|n| n.to_str()) {
                self.implicit_doc_filenames
                    .insert(fname.to_ascii_lowercase());
            }
            self.implicit_by_doc_path
                .insert(doc_path, entry.summary.name.clone());
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
            ambiguous_triggers: self.ambiguous_triggers.clone(),
            implicit_by_scripts_dir: self.implicit_by_scripts_dir.clone(),
            implicit_by_doc_path: self.implicit_by_doc_path.clone(),
            implicit_doc_filenames: self.implicit_doc_filenames.clone(),
            scanned_roots: self.scanned_roots.clone(),
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
    /// Non-fatal issues encountered while activating skills. Explicit
    /// `/skill <name>` requests and trigger matches use this path so stale
    /// or mistyped skill references do not discard the user's turn.
    pub warnings: Vec<SkillActivationWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillActivationWarning {
    pub name: String,
    pub message: String,
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

impl SkillActivationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Trigger => "trigger",
            Self::ImplicitShell => "implicit_shell",
        }
    }
}

/// Plain-data discovery summary for telemetry. No telemetry-crate dependency.
/// The agent converts this into a `SkillActivationReport` count-map.
#[derive(Debug, Clone, Default)]
pub struct SkillDiscoverySummary {
    /// Skill count by `SkillSource::as_str()` key.
    pub by_source: BTreeMap<String, u32>,
    pub disabled: u32,
    pub ambiguous: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
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

/// Return the trigger phrases declared in a `SKILL.md` file (normalised to
/// lowercase), or an empty vec on parse error. Used by `squeezy skills show`
/// to show only the triggers belonging to a specific skill.
pub fn parse_skill_triggers(content: &str) -> Vec<String> {
    parse_skill_file(content)
        .map(|(meta, _)| {
            meta.triggers
                .into_iter()
                .map(|t| t.trim().to_ascii_lowercase())
                .filter(|t| !t.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Extended authoring lint for a single `SKILL.md` file.
///
/// Returns a list of `(severity, message)` pairs where severity is either
/// `"error"` or `"warning"`. Callers pass the file content, the skill's
/// parent directory (for script-existence checks), and the set of trigger
/// phrases that are ambiguous across the catalog.
///
/// This is a best-effort scan: the first parse error terminates the lint
/// early. An empty return means no issues were found.
pub fn lint_skill_extended(
    content: &str,
    skill_dir: &std::path::Path,
    ambiguous_triggers: &std::collections::BTreeSet<String>,
    body_warn_bytes: usize,
) -> Vec<(&'static str, String)> {
    let Ok((meta, body)) = parse_skill_file(content) else {
        return Vec::new();
    };
    let mut issues: Vec<(&'static str, String)> = Vec::new();

    // Oversized body.
    if body.len() > body_warn_bytes {
        issues.push((
            "warning",
            format!(
                "body is {} bytes (>{} bytes); consider splitting or summarising to reduce context cost",
                body.len(),
                body_warn_bytes
            ),
        ));
    }

    // Ambiguous trigger phrases.
    for trigger in &meta.triggers {
        let normalised = trigger.trim().to_ascii_lowercase();
        if ambiguous_triggers.contains(&normalised) {
            issues.push((
                "warning",
                format!(
                    "trigger `{trigger}` is declared by more than one skill; \
                     auto-activation skipped for this phrase"
                ),
            ));
        }
    }

    // Missing hook scripts (hooks referencing relative script paths).
    for matchers in meta.hooks.values() {
        for matcher in matchers {
            for hook in &matcher.hooks {
                let cmd = hook.command.trim();
                if cmd.starts_with("scripts/") || cmd.starts_with("./scripts/") {
                    let script_path = skill_dir.join(cmd);
                    if !script_path.exists() {
                        issues.push((
                            "warning",
                            format!("hook command `{cmd}` references a script that does not exist"),
                        ));
                    }
                }
            }
        }
    }

    issues
}

fn parse_skill_file(content: &str) -> std::result::Result<(SkillMetadata, String), String> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let mut lines = content.lines().skip_while(|line| line.trim().is_empty());
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

        // A YAML block scalar header (`|`/`>`, optionally with chomping/indent
        // indicators) means the real value spans the following indented lines.
        // This parser is line-based rather than a full YAML parser, so gather
        // and fold those continuation lines here. It keeps SKILL.md files
        // portable: frontmatter that other agents accept — which commonly wraps
        // a long `description` in a `>-` block — loads here too.
        let block_value;
        let value = if let Some(header) = parse_block_scalar_header(value) {
            let (folded, consumed) = parse_block_scalar(&lines[idx..], header);
            idx += consumed;
            block_value = folded;
            block_value.as_str()
        } else {
            value
        };

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

        // A matcher item opens a new hook group under the current
        // event. `- matcher: ...` installs a tool-name filter;
        // `- hooks:` is the documented shorthand for an omitted matcher
        // and therefore matches every payload for the event. The
        // matcher indent is locked on first sight so later
        // `command:`/`once:` lines can be told apart from a sibling
        // matcher reliably.
        if let Some(item) = trimmed.strip_prefix("- ")
            && let Some((key, value)) = item.split_once(':')
            && matcher_indent.is_none_or(|m| indent <= m)
        {
            match key.trim() {
                "matcher" => {
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
                "hooks" if value.trim().is_empty() => {
                    matcher_indent = Some(indent);
                    current_matchers.push(SkillHookMatcher {
                        matcher: None,
                        hooks: Vec::new(),
                    });
                    continue;
                }
                _ => {}
            }
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
                timeout_secs: None,
                fail_open: true,
                kind_valid: true,
                failure_policy: HookFailurePolicy::Allow,
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
        "timeout" => {
            if let Ok(secs) = value.parse::<u64>() {
                spec.timeout_secs = Some(secs);
            } else {
                warn!(
                    target: "squeezy_skills",
                    value = %value,
                    "ignoring invalid hook timeout value; expected integer seconds"
                );
            }
        }
        "fail_open" => spec.fail_open = matches!(value, "true" | "yes" | "1"),
        "failure_policy" => {
            spec.failure_policy = match value {
                "deny" => HookFailurePolicy::Deny,
                "allow" => HookFailurePolicy::Allow,
                other => {
                    warn!(
                        target: "squeezy_skills",
                        value = %other,
                        "unrecognized failure_policy value; expected \"allow\" or \"deny\", defaulting to allow"
                    );
                    HookFailurePolicy::Allow
                }
            };
        }
        "type" if value == "command" => {
            // Explicit `type: command` — already the default, no-op.
        }
        "type" => {
            warn!(
                target: "squeezy_skills",
                kind = %value,
                "ignoring unsupported skill hook kind; spec will be dropped"
            );
            spec.kind_valid = false;
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
        "PostToolUseFailure" | "post_tool_use_failure" => Some(HookEvent::PostToolUseFailure),
        "PostTool" | "post_tool" => Some(HookEvent::PostTool),
        "PreCompact" | "pre_compact" => Some(HookEvent::PreCompact),
        "PostCompact" | "post_compact" => Some(HookEvent::PostCompact),
        "SubagentStart" | "subagent_start" => Some(HookEvent::SubagentStart),
        "SubagentStop" | "subagent_stop" => Some(HookEvent::SubagentStop),
        "PermissionRequest" | "permission_request" => Some(HookEvent::PermissionRequest),
        "PermissionDenied" | "permission_denied" => Some(HookEvent::PermissionDenied),
        "UserPromptSubmit" | "user_prompt_submit" => Some(HookEvent::UserPromptSubmit),
        "SessionStart" | "session_start" => Some(HookEvent::SessionStart),
        "Stop" | "stop" => Some(HookEvent::Stop),
        "Setup" | "setup" => Some(HookEvent::Setup),
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

#[derive(Clone, Copy)]
struct BlockScalarHeader {
    /// `true` for literal (`|`) style, `false` for folded (`>`).
    literal: bool,
    chomp: BlockChomp,
}

#[derive(Clone, Copy)]
enum BlockChomp {
    /// `-`: strip every trailing line break.
    Strip,
    /// default: keep a single trailing line break.
    Clip,
    /// `+`: keep all trailing line breaks.
    Keep,
}

/// Parse a YAML block scalar header such as `>`, `>-`, `|`, `|+`, or `|2-`.
///
/// Returns `None` when `value` is an ordinary scalar (the common case), so the
/// caller falls back to treating the rest of the line as the value. The
/// optional indentation-indicator digit is accepted but ignored — block
/// indentation is detected from the first content line instead.
fn parse_block_scalar_header(value: &str) -> Option<BlockScalarHeader> {
    let mut chars = value.chars();
    let literal = match chars.next()? {
        '|' => true,
        '>' => false,
        _ => return None,
    };
    let mut chomp = BlockChomp::Clip;
    for ch in chars {
        match ch {
            '-' => chomp = BlockChomp::Strip,
            '+' => chomp = BlockChomp::Keep,
            c if c.is_ascii_digit() => {} // explicit indentation indicator: ignored
            _ => return None,
        }
    }
    Some(BlockScalarHeader { literal, chomp })
}

/// Collect the indented continuation lines of a block scalar, returning the
/// folded/literal text and the number of lines consumed.
///
/// Block indentation is taken from the first non-blank line; a later non-blank
/// line indented less than that ends the block (and is not consumed, so the
/// caller reparses it as the next key). Folded (`>`) style joins consecutive
/// non-blank lines with a single space and turns blank lines into newlines;
/// literal (`|`) style preserves line breaks verbatim.
fn parse_block_scalar(lines: &[&str], header: BlockScalarHeader) -> (String, usize) {
    let mut consumed = 0;
    let mut block_indent: Option<usize> = None;
    let mut content: Vec<String> = Vec::new();

    for raw in lines {
        let indent = raw.len() - raw.trim_start().len();
        if raw.trim().is_empty() {
            content.push(String::new());
            consumed += 1;
            continue;
        }
        match block_indent {
            Some(bi) if indent < bi => break,
            Some(_) => {}
            None => block_indent = Some(indent),
        }
        content.push(strip_leading_spaces(raw, block_indent.unwrap_or(indent)));
        consumed += 1;
    }

    // Count of trailing blank lines drives chomping; the lines themselves stay
    // in `consumed` so the caller's cursor skips past them.
    let mut trailing_blanks = 0;
    while matches!(content.last(), Some(line) if line.is_empty()) {
        content.pop();
        trailing_blanks += 1;
    }

    let mut folded = String::new();
    if header.literal {
        folded = content.join("\n");
    } else {
        let mut at_start = true;
        for line in &content {
            if line.is_empty() {
                folded.push('\n');
                at_start = true;
            } else {
                if !at_start {
                    folded.push(' ');
                }
                folded.push_str(line);
                at_start = false;
            }
        }
    }

    match header.chomp {
        BlockChomp::Strip => {}
        BlockChomp::Clip if !folded.is_empty() => folded.push('\n'),
        BlockChomp::Clip => {}
        BlockChomp::Keep if !folded.is_empty() => {
            for _ in 0..trailing_blanks + 1 {
                folded.push('\n');
            }
        }
        BlockChomp::Keep => {}
    }
    (folded, consumed)
}

/// Remove up to `n` leading space/tab characters from `raw`, preserving any
/// indentation deeper than the block's base indent (relevant for literal
/// blocks). Stops early at the first non-whitespace character.
fn strip_leading_spaces(raw: &str, n: usize) -> String {
    // `count` is the leading-whitespace char position; every char before the
    // break is a space/tab, so it equals the number stripped so far. Defaults
    // to the full length when the line is entirely whitespace shorter than `n`.
    let mut start = raw.len();
    for (count, (i, ch)) in raw.char_indices().enumerate() {
        if count >= n || (ch != ' ' && ch != '\t') {
            start = i;
            break;
        }
    }
    raw[start..].to_string()
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
        // `start` is a `find` offset and therefore a valid char boundary, so
        // `lowered_input[start..]` is safe. Advance past the first char of the
        // match (one byte for ASCII) instead of a fixed `+ 1`, which could land
        // inside a multi-byte UTF-8 character and panic when the next iteration
        // slices `lowered_input[cursor..]`.
        cursor = start
            + lowered_input[start..]
                .chars()
                .next()
                .map_or(1, char::len_utf8);
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

pub fn xml_escape(value: &str) -> String {
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

/// Materialise the in-binary [`bundled_skills`] under `user_dir` so a
/// fresh install actually has discoverable sample skills.
///
/// Each bundled skill is written to `<user_dir>/<name>/SKILL.md`. If
/// the target directory or file already exists the entry is skipped
/// — repeat calls and partial installs stay idempotent and never
/// clobber an edited user copy.
///
/// Returns the list of skill names that were written this call.
pub fn install_bundled_skills(user_dir: &Path) -> Result<Vec<String>> {
    fs::create_dir_all(user_dir)?;
    let mut written = Vec::new();
    for source in BUNDLED_SKILL_SOURCES {
        let target_dir = user_dir.join(source.dir_name);
        let target_file = target_dir.join(SKILL_FILE);
        if target_file.exists() {
            continue;
        }
        fs::create_dir_all(&target_dir)?;
        fs::write(&target_file, source.content)?;
        written.push(source.dir_name.to_string());
    }
    Ok(written)
}

/// Inspect a `manifest.tool_deps` list and return the subset that is
/// not satisfied by the runtime's available tools and MCP servers.
///
/// A dep starting with `mcp:<server>` matches an MCP server name in
/// `available_mcp_servers`. Any other dep is treated as a built-in
/// tool name and matched against `available_tools`. Comparison is
/// case-sensitive to mirror the lookup the tool registry performs.
pub fn unmet_tool_deps(
    deps: &[String],
    available_tools: &BTreeSet<String>,
    available_mcp_servers: &BTreeSet<String>,
) -> Vec<String> {
    deps.iter()
        .filter(|dep| {
            let trimmed = dep.trim();
            if trimmed.is_empty() {
                return false;
            }
            if let Some(server) = trimmed.strip_prefix("mcp:") {
                !available_mcp_servers.contains(server.trim())
            } else {
                !available_tools.contains(trimmed)
            }
        })
        .cloned()
        .collect()
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

/// Outcome of validating a single `SKILL.md` file found in a skill root.
#[derive(Debug, Clone)]
pub struct SkillValidationResult {
    /// Path to the `SKILL.md` file.
    pub path: PathBuf,
    /// Skill name from frontmatter, or the raw string that failed naming rules.
    /// `None` when the file could not be read or the frontmatter had no
    /// parseable `name:` field.
    pub name: Option<String>,
    /// `Ok(())` when the file parsed cleanly and the name is valid;
    /// `Err(reason)` with a human-readable description of the first issue
    /// found.
    pub outcome: std::result::Result<(), String>,
}

/// Return every skill *root directory* that [`SkillCatalog::discover`] will
/// scan for a given `(workspace_root, config)` pair, in the same order
/// `discover` uses. This includes:
///
/// - User-level roots (`compat_user_dir`, `user_dir`, optional XDG user dir).
/// - `extra_roots` from config.
/// - The current workspace's project roots (`.agents/skills`,
///   `.squeezy/skills`).
/// - **All ancestor project roots** up to the git root (monorepo support) —
///   the same directories scanned by `ancestor_project_roots`.
///
/// [`validate_skill_dirs`] calls this so its scan is always identical to what
/// runtime discovery will load.
pub fn skill_scan_dirs(workspace_root: &Path, config: &squeezy_core::SkillsConfig) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    dirs.push(config.compat_user_dir.clone());
    dirs.push(config.user_dir.clone());
    if let Some(xdg_dir) = &config.xdg_user_dir {
        dirs.push(xdg_dir.clone());
    }
    dirs.extend(config.extra_roots.iter().cloned());
    dirs.push(workspace_root.join(COMPAT_PROJECT_SKILLS_DIR));
    dirs.push(workspace_root.join(PROJECT_SKILLS_DIR));
    // Monorepo ancestor walk — mirrors discover's ancestor_project_roots loop.
    for ancestor in ancestor_project_roots(workspace_root) {
        dirs.push(ancestor.join(COMPAT_PROJECT_SKILLS_DIR));
        dirs.push(ancestor.join(PROJECT_SKILLS_DIR));
    }
    dirs
}

/// Walk every configured skill root from `config` (the same roots that
/// [`SkillCatalog::discover`] uses, including ancestor project roots for
/// monorepo launches from subdirectories) and attempt to parse each
/// `SKILL.md`.
///
/// Unlike discovery, this function records parse failures rather than
/// silently skipping them, so `squeezy skills validate` can surface the
/// errors that discovery drops. Non-existent roots and directories without
/// a `SKILL.md` are skipped without an error, mirroring discovery behaviour.
pub fn validate_skill_dirs(
    workspace_root: &Path,
    config: &squeezy_core::SkillsConfig,
) -> Vec<SkillValidationResult> {
    let scan_dirs = skill_scan_dirs(workspace_root, config);
    let mut results = Vec::new();
    for dir in &scan_dirs {
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let skill_path = path.join(SKILL_FILE);
            if !skill_path.exists() {
                continue;
            }
            let content = match fs::read_to_string(&skill_path) {
                Ok(c) => c,
                Err(err) => {
                    results.push(SkillValidationResult {
                        path: skill_path,
                        name: None,
                        outcome: Err(format!("could not read file: {err}")),
                    });
                    continue;
                }
            };
            let outcome = validate_skill_md(&content).map(|_| ());
            let name = parse_skill_file(&content).ok().map(|(meta, _)| meta.name);
            results.push(SkillValidationResult {
                path: skill_path,
                name,
                outcome,
            });
        }
    }
    results
}

pub(crate) fn parse_skill_manifest(content: &str) -> std::result::Result<SkillManifest, String> {
    toml::from_str::<SkillManifest>(content).map_err(|error| error.to_string())
}

/// JSON payload byte length above which the payload is written to a
/// temporary file instead of the environment variable.
///
/// Windows process environment blocks have practical size limits; large
/// payloads (e.g. prompts or full tool metadata) can push past them.
/// When the threshold is exceeded the handler writes the payload to a
/// temp file and sets `SQUEEZY_HOOK_PAYLOAD_FILE`; `SQUEEZY_HOOK_PAYLOAD`
/// is left unset so that the large JSON is never placed in the env block.
const PAYLOAD_INLINE_THRESHOLD: usize = 8 * 1024;

/// Per-process dispatch sequence number used to generate unique temp-file
/// names. A plain counter is enough: file names only need to be unique
/// within a single process, not globally.
static HOOK_TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Resolve the shell program and arguments used to run a hook command
/// string on the current platform.
///
/// On Unix, returns `("/bin/sh", ["-c"])`. On Windows, walks the candidates
/// `pwsh`, `powershell`, and `cmd` in that order, returning the first
/// one found on `PATH`. Returns `None` only when every candidate is
/// absent — callers treat that as a spawn error and allow the action
/// (fail-open) with a clear diagnostic.
fn resolve_hook_shell_program() -> Option<(String, Vec<String>)> {
    #[cfg(windows)]
    {
        let candidates: &[(&str, &[&str])] = &[
            ("pwsh", &["-NoProfile", "-Command"]),
            ("powershell", &["-NoProfile", "-Command"]),
            ("cmd", &["/C"]),
        ];
        let path_var = std::env::var_os("PATH").unwrap_or_default();
        for &(shell, args) in candidates {
            let found = std::env::split_paths(&path_var)
                .any(|dir| dir.join(format!("{shell}.exe")).exists() || dir.join(shell).exists());
            if found {
                return Some((
                    shell.to_string(),
                    args.iter().map(|s| s.to_string()).collect(),
                ));
            }
        }
        None
    }
    #[cfg(not(windows))]
    {
        Some(("/bin/sh".to_string(), vec!["-c".to_string()]))
    }
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
    /// Tracks whether a `once: true` hook is already claimed or has succeeded
    /// in this session. A failed claimed run resets the flag so it can be
    /// retried. `AtomicBool` with `AcqRel` / `Acquire` ordering is used rather
    /// than `Mutex<bool>` to close the TOCTOU gap where two concurrent
    /// dispatches could both read `false` before either writes `true`, and to
    /// avoid the silent-pass risk of a poisoned mutex.
    fired: AtomicBool,
    /// Cached shell program and arguments for this handler, populated on
    /// the first dispatch. Avoids re-walking PATH on every hook invocation
    /// and makes the resolution cost visible (one OnceLock init per handler
    /// rather than one per dispatch).
    resolved_shell: OnceLock<Option<(String, Vec<String>)>>,
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
            fired: AtomicBool::new(false),
            resolved_shell: OnceLock::new(),
        }
    }
}

impl HookHandler for SkillHookHandler {
    fn handle(&self, ctx: &HookContext) -> HookResult {
        if ctx.event != self.event {
            return HookResult::allow();
        }

        // Match tool_name directly from the typed payload before
        // projecting to JSON, so unrelated tool dispatches pay no
        // serialization cost.
        if let Some(needle) = self.matcher.as_deref() {
            let tool_name = match &ctx.payload {
                HookPayload::PreToolUse { tool_name, .. }
                | HookPayload::PostToolUse { tool_name, .. }
                | HookPayload::PostToolUseFailure { tool_name, .. }
                | HookPayload::PostTool { tool_name, .. }
                | HookPayload::PermissionRequest { tool_name, .. }
                | HookPayload::PermissionDenied { tool_name, .. } => tool_name.as_str(),
                _ => "",
            };
            if tool_name != needle {
                return HookResult::allow();
            }
        }

        let trimmed = self.spec.command.trim();
        if trimmed.is_empty() {
            warn!(
                target: "squeezy_skills",
                skill = %self.skill_name,
                "skipping skill hook with empty command"
            );
            return HookResult::allow();
        }

        let once_claimed = if self.spec.once {
            if self
                .fired
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return HookResult::allow();
            }
            true
        } else {
            false
        };

        let shell_info = self.resolved_shell.get_or_init(resolve_hook_shell_program);
        let (shell, shell_args) = match shell_info {
            Some(pair) => (&pair.0, &pair.1),
            None => {
                warn!(
                    target: "squeezy_skills",
                    skill = %self.skill_name,
                    "skill hook failed to spawn: no suitable hook shell found on PATH"
                );
                if once_claimed {
                    self.fired.store(false, Ordering::Release);
                }
                return if self.spec.fail_open
                    && self.spec.failure_policy == HookFailurePolicy::Allow
                {
                    HookResult::allow()
                } else {
                    HookResult::deny(format!("skill `{}` hook shell not found", self.skill_name))
                };
            }
        };

        let payload = ctx.payload_json().to_string();
        let seq = HOOK_TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let payload_file_path = if payload.len() > PAYLOAD_INLINE_THRESHOLD {
            let path = std::env::temp_dir()
                .join(format!("squeezy_hook_{}_{seq}.json", std::process::id()));
            match fs::write(&path, &payload) {
                Ok(()) => Some(path),
                Err(error) => {
                    warn!(
                        target: "squeezy_skills",
                        skill = %self.skill_name,
                        error = %error,
                        "failed to write hook payload temp file; falling back to env-only delivery"
                    );
                    None
                }
            }
        } else {
            None
        };
        let cleanup = |path: Option<&Path>| {
            if let Some(path) = path {
                let _ = fs::remove_file(path);
            }
        };

        let mut command = Command::new(shell);
        for arg in shell_args {
            command.arg(arg);
        }

        // Put the child in its own process group so a timeout signal reaches
        // grandchildren spawned by the hook shell script, not just the shell.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }

        command
            .arg(trimmed)
            .current_dir(&self.base_dir)
            .env("SQUEEZY_SKILL_DIR", &self.base_dir)
            .env("SQUEEZY_SKILL_NAME", &self.skill_name)
            // Redirect subprocess stdio to /dev/null so hook scripts cannot
            // corrupt the TUI or write to the agent's streams.
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());

        // Deliver payload via file for large payloads and via env var for
        // small ones. Explicitly clear the alternate variable so stale
        // inherited values cannot confuse hook scripts.
        if let Some(ref path) = payload_file_path {
            command
                .env("SQUEEZY_HOOK_PAYLOAD_FILE", path)
                .env_remove("SQUEEZY_HOOK_PAYLOAD");
        } else {
            command
                .env("SQUEEZY_HOOK_PAYLOAD", &payload)
                .env_remove("SQUEEZY_HOOK_PAYLOAD_FILE");
        }

        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                warn!(
                    target: "squeezy_skills",
                    skill = %self.skill_name,
                    shell = %shell,
                    error = %error,
                    "skill hook failed to spawn"
                );
                cleanup(payload_file_path.as_deref());
                if once_claimed {
                    self.fired.store(false, Ordering::Release);
                }
                return if self.spec.fail_open
                    && self.spec.failure_policy == HookFailurePolicy::Allow
                {
                    HookResult::allow()
                } else {
                    HookResult::deny(format!(
                        "skill `{}` hook failed to spawn: {}",
                        self.skill_name, error
                    ))
                };
            }
        };

        // Capture PID before wrapping child in Arc. On Unix, used to send
        // SIGKILL to the process group on timeout so all grandchildren are
        // terminated; elided on non-Unix to avoid an unused-variable warning.
        #[cfg(unix)]
        let child_pid = child.id();

        // Wrap child in Arc<Mutex<Option<...>>> so the main thread can call
        // `kill()` on timeout without a blocking wait-for-lock: the wait
        // thread takes the child out of the Option before calling `wait()`.
        let child_arc = Arc::new(Mutex::new(Some(child)));
        let child_for_thread = Arc::clone(&child_arc);

        let timeout =
            Duration::from_secs(self.spec.timeout_secs.unwrap_or(DEFAULT_HOOK_TIMEOUT_SECS));
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = child_for_thread
                .lock()
                .ok()
                .and_then(|mut guard| guard.take())
                .map(|mut child| child.wait());
            if let Some(result) = result {
                let _ = tx.send(result);
            }
        });

        match rx.recv_timeout(timeout) {
            Ok(Ok(status)) if status.success() => {
                cleanup(payload_file_path.as_deref());
                HookResult::allow()
            }
            Ok(Ok(status)) => {
                cleanup(payload_file_path.as_deref());
                let code = status.code();
                let detail = match code {
                    Some(126) => format!(
                        "skill `{}` hook: command not executable (exit 126)",
                        self.skill_name
                    ),
                    Some(127) => format!(
                        "skill `{}` hook: interpreter or command not found (exit 127)",
                        self.skill_name
                    ),
                    _ => format!("skill `{}` hook denied the action", self.skill_name),
                };
                warn!(
                    target: "squeezy_skills",
                    skill = %self.skill_name,
                    code = ?code,
                    "skill hook exited non-zero"
                );
                if once_claimed {
                    self.fired.store(false, Ordering::Release);
                }
                HookResult::deny(detail)
            }
            Ok(Err(error)) => {
                cleanup(payload_file_path.as_deref());
                warn!(
                    target: "squeezy_skills",
                    skill = %self.skill_name,
                    error = %error,
                    "skill hook wait() error"
                );
                if once_claimed {
                    self.fired.store(false, Ordering::Release);
                }
                if self.spec.fail_open && self.spec.failure_policy == HookFailurePolicy::Allow {
                    HookResult::allow()
                } else {
                    HookResult::deny(format!(
                        "skill `{}` hook wait failed: {}",
                        self.skill_name, error
                    ))
                }
            }
            Err(_timeout_expired) => {
                cleanup(payload_file_path.as_deref());
                if let Ok(mut guard) = child_arc.lock()
                    && let Some(child) = guard.as_mut()
                {
                    let _ = child.kill();
                }
                #[cfg(unix)]
                unsafe {
                    libc::kill(-(child_pid as libc::pid_t), libc::SIGKILL);
                }
                warn!(
                    target: "squeezy_skills",
                    skill = %self.skill_name,
                    timeout_secs = self.spec.timeout_secs.unwrap_or(DEFAULT_HOOK_TIMEOUT_SECS),
                    "skill hook timed out"
                );
                if once_claimed {
                    self.fired.store(false, Ordering::Release);
                }
                HookResult::deny(format!(
                    "skill `{}` hook timed out after {}s",
                    self.skill_name,
                    self.spec.timeout_secs.unwrap_or(DEFAULT_HOOK_TIMEOUT_SECS)
                ))
            }
        }
    }
}

/// Register every hook declared in a [`LoadedSkill`]'s frontmatter
/// against the given [`HookRegistry`].
///
/// Specs with `kind_valid = false` (unsupported `type:` in frontmatter)
/// are silently dropped so they cannot execute as shell commands.
///
/// Handlers are registered via [`HookRegistry::register_for_event`] so
/// the registry can dispatch in O(matching handlers) rather than
/// O(total handlers). Returns the number of handlers installed so
/// callers can log the activation count alongside the skill name.
pub fn register_skill_hooks(skill: &LoadedSkill, registry: &mut HookRegistry) -> usize {
    let mut installed = 0;
    for (event, matchers) in &skill.hooks {
        for matcher in matchers {
            for spec in &matcher.hooks {
                if !spec.kind_valid {
                    continue;
                }
                registry.register_for_event(
                    *event,
                    Box::new(SkillHookHandler::new(
                        skill.summary.name.clone(),
                        *event,
                        matcher.matcher.clone(),
                        spec.clone(),
                        skill.base_dir.clone(),
                    )),
                );
                installed += 1;
            }
        }
    }
    if installed > 0 {
        tracing::info!(
            target: "squeezy_skills",
            skill = %skill.summary.name,
            source = %skill.base_dir.display(),
            installed,
            "registered skill frontmatter hooks"
        );
        // Emit a per-handler snapshot at DEBUG level so session logs
        // include the full hook registry for trusted local debugging.
        for (event, matchers) in &skill.hooks {
            for matcher_spec in matchers {
                for spec in &matcher_spec.hooks {
                    if !spec.kind_valid {
                        continue;
                    }
                    tracing::debug!(
                        target: "squeezy_skills",
                        skill = %skill.summary.name,
                        event = ?event,
                        matcher = ?matcher_spec.matcher,
                        command_path = %spec.command,
                        once = spec.once,
                        source = %skill.base_dir.display(),
                        "hook handler registered"
                    );
                }
            }
        }
    }
    installed
}

/// A single diagnostic finding from [`catalog_hook_issues`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookDoctorIssue {
    /// Skill name the issue belongs to.
    pub skill: String,
    /// Human-readable description of the problem.
    pub message: String,
    /// Whether this is a hard error (`true`) or an advisory note (`false`).
    pub is_error: bool,
}

/// Inspect every non-disabled skill's hook declarations and return
/// diagnostic issues without registering handlers or running commands.
///
/// Checks performed:
/// - Missing script files for path-like commands (relative to the skill's
///   `base_dir`).
/// - Non-executable script files on Unix (exit 126 at runtime).
/// - Missing shebang line in script files (may cause interpreter errors).
/// - Commands that are inline shell snippets — noted as advisory because
///   snippet behaviour depends on distro shell semantics.
///
/// This is the static analysis pass used by `squeezy doctor` to surface
/// hook problems before a session starts. It does **not** spawn any
/// processes; all checks are pure filesystem/metadata operations.
pub fn catalog_hook_issues(catalog: &SkillCatalog) -> Vec<HookDoctorIssue> {
    let mut issues = Vec::new();
    for summary in catalog.summaries() {
        if summary.disabled {
            continue;
        }
        let loaded = match catalog.load(&summary.name) {
            Ok(l) => l,
            Err(_) => continue,
        };
        for matchers in loaded.hooks.values() {
            for matcher in matchers {
                for spec in &matcher.hooks {
                    let trimmed = spec.command.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    // Classify the command: inline snippet vs file path.
                    let is_inline_snippet = trimmed.contains(['|', ';', '>', '<', '&', '(', ')'])
                        || trimmed.contains('\n');
                    if is_inline_snippet {
                        issues.push(HookDoctorIssue {
                            skill: summary.name.clone(),
                            message: format!(
                                "hook command is an inline shell snippet; behaviour depends on \
                                 the distro shell (`/bin/sh`): {trimmed:.80}"
                            ),
                            is_error: false,
                        });
                        continue;
                    }
                    // The first whitespace-delimited token is the executable;
                    // skip argv-style commands with arguments (e.g. `python3 hook.py`).
                    let executable = trimmed.split_whitespace().next().unwrap_or(trimmed);
                    // Only validate path-like executables (contains '/' or starts with '.').
                    if !executable.contains('/') && !executable.starts_with('.') {
                        continue;
                    }
                    let script_path = if Path::new(executable).is_absolute() {
                        PathBuf::from(executable)
                    } else {
                        loaded.base_dir.join(executable)
                    };
                    match fs::metadata(&script_path) {
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            issues.push(HookDoctorIssue {
                                skill: summary.name.clone(),
                                message: format!(
                                    "hook script not found: {}",
                                    script_path.display()
                                ),
                                is_error: true,
                            });
                        }
                        Ok(meta) => {
                            #[cfg(unix)]
                            {
                                use std::os::unix::fs::PermissionsExt;
                                if meta.permissions().mode() & 0o111 == 0 {
                                    issues.push(HookDoctorIssue {
                                        skill: summary.name.clone(),
                                        message: format!(
                                            "hook script not executable (chmod +x): {}",
                                            script_path.display()
                                        ),
                                        is_error: true,
                                    });
                                }
                            }
                            // Check for shebang line to catch missing interpreter errors.
                            // Only read the first 2 bytes to avoid slurping large scripts.
                            if meta.is_file() {
                                let has_shebang = fs::File::open(&script_path)
                                    .and_then(|mut f| {
                                        let mut buf = [0u8; 2];
                                        f.read_exact(&mut buf).map(|_| buf)
                                    })
                                    .map(|buf| buf[0] == b'#' && buf[1] == b'!')
                                    .unwrap_or(true); // on read error, don't produce a false note
                                if !has_shebang {
                                    issues.push(HookDoctorIssue {
                                        skill: summary.name.clone(),
                                        message: format!(
                                            "hook script missing shebang line: {}",
                                            script_path.display()
                                        ),
                                        is_error: false,
                                    });
                                }
                            }
                        }
                        Err(e) => {
                            issues.push(HookDoctorIssue {
                                skill: summary.name.clone(),
                                message: format!(
                                    "hook script not accessible ({}): {}",
                                    e,
                                    script_path.display()
                                ),
                                is_error: true,
                            });
                        }
                    }
                }
            }
        }
    }
    issues
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
