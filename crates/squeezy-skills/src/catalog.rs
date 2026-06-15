use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use squeezy_core::{Result, SkillConfigEntry, SkillsConfig, SqueezyError};
use squeezy_hooks::{HookEvent, HookRegistry};
use tracing::warn;

use crate::frontmatter::{
    SkillContextMode, input_matches_trigger, is_valid_skill_name, parse_explicit_skill_command,
    parse_skill_file,
};
use crate::hooks::{SkillHookMatcher, register_skill_hooks};
use crate::manifest::{SkillManifest, load_manifest, render_manifest_block};
use crate::render::SkillPreambleRender;
use crate::{COMPAT_PROJECT_SKILLS_DIR, PROJECT_SKILLS_DIR, SKILL_FILE, implicit, render};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillEntry {
    pub(crate) summary: SkillSummary,
    pub(crate) base_dir: PathBuf,
    pub(crate) triggers: Vec<String>,
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

pub(crate) fn ancestor_project_roots(workspace_root: &Path) -> Vec<PathBuf> {
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
pub(crate) fn is_git_root(dir: &Path) -> bool {
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

#[cfg(test)]
#[path = "catalog_tests.rs"]
mod tests;
