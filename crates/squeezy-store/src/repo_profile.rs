use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use squeezy_core::{GraphConfig, LanguageKind, Result, SqueezyError, repo_settings_id};
use squeezy_workspace::{
    CrawlOptions, ExclusionReason, IndexingPolicy, UnsupportedReason, WorkspaceCrawler,
    WorkspaceSnapshot, classify_language,
};

use crate::fs_util;

pub const REPO_REGISTRY_VERSION: u32 = 1;

const MARKER_FILES: &[&str] = &[
    "squeezy.toml",
    "Cargo.toml",
    "Cargo.lock",
    "package.json",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lockb",
    "pyproject.toml",
    "requirements.txt",
    "uv.lock",
    "poetry.lock",
    "go.mod",
    "go.sum",
    "Makefile",
    "Justfile",
    "CMakeLists.txt",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "Directory.Build.props",
    "Directory.Build.targets",
    "global.json",
    ".gitignore",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoProfileLoad {
    pub profile: RepoProfile,
    pub status: RepoProfileStatus,
    pub registry_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoProfileStatus {
    Created,
    Refreshed,
    Reused,
}

impl RepoProfileStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Refreshed => "refreshed",
            Self::Reused => "reused",
        }
    }

    pub const fn should_show_onboarding(self) -> bool {
        matches!(self, Self::Created | Self::Refreshed)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoRegistry {
    pub version: u32,
    pub repos: Vec<RepoProfile>,
}

impl RepoRegistry {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self {
                version: REPO_REGISTRY_VERSION,
                repos: Vec::new(),
            });
        }
        let text = fs::read_to_string(path)?;
        let mut registry: Self = match toml::from_str(&text) {
            Ok(registry) => registry,
            Err(_) => {
                return Ok(Self {
                    version: REPO_REGISTRY_VERSION,
                    repos: Vec::new(),
                });
            }
        };
        if registry.version == 0 {
            registry.version = REPO_REGISTRY_VERSION;
        }
        Ok(registry)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        // Atomicity: writes go through `fs_util::write_bytes_atomically`,
        // which produces a fresh tmp + `sync_all` + atomic replace (see
        // `fs_util.rs`). Concurrent readers therefore see either the prior
        // complete `repos.toml` or the new one, never a half-written file.
        fs_util::write_bytes_atomically(path, self.to_toml().as_bytes())?;
        Ok(())
    }

    pub fn profile_for_root(&self, root: &Path) -> Option<&RepoProfile> {
        let root = root.display().to_string();
        self.repos.iter().find(|profile| profile.root == root)
    }

    pub fn upsert(&mut self, profile: RepoProfile) {
        if let Some(existing) = self
            .repos
            .iter_mut()
            .find(|existing| existing.root == profile.root)
        {
            *existing = profile;
        } else {
            self.repos.push(profile);
            self.repos.sort_by(|left, right| left.root.cmp(&right.root));
        }
    }

    fn to_toml(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("version = {}\n", self.version));
        for profile in &self.repos {
            out.push_str("\n[[repos]]\n");
            out.push_str(&format!("profile_version = {}\n", profile.profile_version));
            out.push_str(&format!("root = {}\n", toml_string(&profile.root)));
            out.push_str(&format!("repo_id = {}\n", toml_string(&profile.repo_id)));
            out.push_str(&format!(
                "updated_unix_millis = {}\n",
                profile.updated_unix_millis
            ));
            out.push_str(&format!(
                "config_files = {}\n",
                toml_array(&profile.config_files)
            ));

            out.push_str("\n[repos.fingerprint]\n");
            out.push_str(&format!(
                "value = {}\n",
                toml_string(&profile.fingerprint.value)
            ));
            out.push_str(&format!(
                "light_value = {}\n",
                toml_string(&profile.fingerprint.light_value)
            ));
            out.push_str(&format!(
                "markers = {}\n",
                toml_array(&profile.fingerprint.markers)
            ));

            if profile.git != GitState::default() {
                out.push_str("\n[repos.git]\n");
                // Optional fields are written only when present so a loaded
                // profile round-trips back to the same in-memory shape. Writing
                // `""` for `None` would deserialize to `Some("")`, which differs
                // from a freshly detected profile and breaks downstream
                // comparisons.
                push_optional_string_field(&mut out, "vcs_type", profile.git.vcs_type.as_deref());
                push_optional_string_field(&mut out, "branch", profile.git.branch.as_deref());
                push_optional_string_field(&mut out, "head", profile.git.head.as_deref());
                push_optional_string_field(
                    &mut out,
                    "default_branch",
                    profile.git.default_branch.as_deref(),
                );
                if let Some(dirty) = profile.git.dirty {
                    out.push_str(&format!("dirty = {dirty}\n"));
                }
            }

            for language in &profile.languages {
                out.push_str("\n[[repos.languages]]\n");
                out.push_str(&format!("name = {}\n", toml_string(&language.name)));
                push_optional_string_field(&mut out, "family", language.family.as_deref());
                out.push_str(&format!("files = {}\n", language.files));
                out.push_str(&format!(
                    "semantic_support = {}\n",
                    toml_string(language.semantic_support.as_str())
                ));
            }
            for manager in &profile.package_managers {
                out.push_str("\n[[repos.package_managers]]\n");
                out.push_str(&format!("name = {}\n", toml_string(&manager.name)));
                out.push_str(&format!("marker = {}\n", toml_string(&manager.marker)));
            }
            for command in &profile.commands {
                out.push_str("\n[[repos.commands]]\n");
                out.push_str(&format!("kind = {}\n", toml_string(&command.kind)));
                out.push_str(&format!("command = {}\n", toml_string(&command.command)));
                out.push_str(&format!("source = {}\n", toml_string(&command.source)));
                out.push_str(&format!(
                    "confidence = {}\n",
                    toml_string(&command.confidence)
                ));
                out.push_str(&format!("ambiguous = {}\n", command.ambiguous));
            }
            for ignored in &profile.ignored_paths {
                out.push_str("\n[[repos.ignored_paths]]\n");
                out.push_str(&format!("reason = {}\n", toml_string(&ignored.reason)));
                out.push_str(&format!("paths = {}\n", ignored.paths));
                out.push_str(&format!("bytes = {}\n", ignored.bytes));
                out.push_str(&format!("samples = {}\n", toml_array(&ignored.samples)));
            }
            for recommendation in &profile.recommendations {
                out.push_str("\n[[repos.recommendations]]\n");
                out.push_str(&format!(
                    "setting = {}\n",
                    toml_string(&recommendation.setting)
                ));
                out.push_str(&format!("value = {}\n", toml_string(&recommendation.value)));
                out.push_str(&format!(
                    "reason = {}\n",
                    toml_string(&recommendation.reason)
                ));
            }
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoProfile {
    pub profile_version: u32,
    pub root: String,
    #[serde(default)]
    pub repo_id: String,
    pub updated_unix_millis: u64,
    #[serde(default)]
    pub languages: Vec<DetectedLanguage>,
    #[serde(default)]
    pub package_managers: Vec<DetectedPackageManager>,
    #[serde(default)]
    pub commands: Vec<DetectedCommand>,
    #[serde(default)]
    pub config_files: Vec<String>,
    #[serde(default)]
    pub ignored_paths: Vec<IgnoredPathSummary>,
    #[serde(default)]
    pub recommendations: Vec<RepoRecommendation>,
    pub fingerprint: RepoFingerprint,
    #[serde(default)]
    pub git: GitState,
}

impl RepoProfile {
    pub fn detect(root: impl AsRef<Path>, graph: &GraphConfig) -> Result<Self> {
        let root = canonical_root(root)?;
        let crawl_options = crawl_options_from_graph_config(graph);
        let snapshot = WorkspaceCrawler::try_new(crawl_options)?.crawl(&root)?;
        let git = detect_git_state(&root);
        let fingerprint = RepoFingerprint::detect(&root, Some(&snapshot), &git)?;
        let config_files = detect_config_files(&root);
        let package_managers = detect_package_managers(&root);
        let commands = detect_commands(&root);
        let languages = detect_languages(&snapshot);
        let ignored_paths = detect_ignored_paths(&snapshot);
        let recommendations =
            detect_recommendations(&languages, &ignored_paths, graph, &package_managers);

        Ok(Self {
            profile_version: REPO_REGISTRY_VERSION,
            root: root.display().to_string(),
            repo_id: repo_settings_id(&root),
            updated_unix_millis: now_unix_millis(),
            languages,
            package_managers,
            commands,
            config_files,
            ignored_paths,
            recommendations,
            fingerprint,
            git,
        })
    }

    pub fn compact_summary(&self, status: RepoProfileStatus) -> String {
        let languages = join_or(
            self.languages
                .iter()
                .filter(|language| language.files > 0)
                .map(|language| format!("{} {}", language.name, language.files)),
            "none",
        );
        let packages = join_or(
            self.package_managers
                .iter()
                .map(|package| package.name.clone()),
            "none",
        );
        let commands = join_or(
            self.commands
                .iter()
                .take(4)
                .map(|command| format!("{}: {}", command.kind, command.command)),
            "none",
        );
        let semantic = join_or(
            self.languages
                .iter()
                .filter(|language| language.files > 0)
                .map(|language| {
                    format!("{}={}", language.name, language.semantic_support.as_str())
                }),
            "none",
        );
        let ignored = join_or(
            self.ignored_paths
                .iter()
                .take(4)
                .map(|ignored| format!("{} {} paths", ignored.reason, ignored.paths)),
            "none",
        );
        format!(
            "repo profile {}: {}\nlanguages: {}\npackages: {}\ncommands: {}\nsemantic: {}\nignored: {}",
            status.as_str(),
            self.root,
            languages,
            packages,
            commands,
            semantic,
            ignored,
        )
    }

    pub fn render_human(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("root: {}\n", self.root));
        out.push_str(&format!(
            "updated_unix_millis: {}\n",
            self.updated_unix_millis
        ));
        out.push_str(&format!(
            "git: type={} branch={} head={} dirty={}\n",
            self.git.vcs_type.as_deref().unwrap_or("none"),
            self.git.branch.as_deref().unwrap_or("-"),
            self.git.head.as_deref().unwrap_or("-"),
            self.git
                .dirty
                .map(|dirty| dirty.to_string())
                .unwrap_or_else(|| "-".to_string())
        ));
        out.push_str(&format!(
            "languages: {}\n",
            join_or(
                self.languages.iter().map(|language| format!(
                    "{} {} ({})",
                    language.name,
                    language.files,
                    language.semantic_support.as_str()
                )),
                "none",
            )
        ));
        out.push_str(&format!(
            "package_managers: {}\n",
            join_or(
                self.package_managers
                    .iter()
                    .map(|manager| format!("{} via {}", manager.name, manager.marker)),
                "none",
            )
        ));
        out.push_str(&format!(
            "commands: {}\n",
            join_or(
                self.commands.iter().map(|command| {
                    let suffix = if command.ambiguous { " ambiguous" } else { "" };
                    format!(
                        "{}={} [{}{}]",
                        command.kind, command.command, command.source, suffix
                    )
                }),
                "none",
            )
        ));
        out.push_str(&format!(
            "config_files: {}\n",
            join_or(self.config_files.iter().cloned(), "none")
        ));
        out.push_str(&format!(
            "ignored_paths: {}\n",
            join_or(
                self.ignored_paths
                    .iter()
                    .map(|ignored| format!("{} paths={}", ignored.reason, ignored.paths)),
                "none",
            )
        ));
        out.push_str("recommendations:\n");
        for recommendation in &self.recommendations {
            out.push_str(&format!(
                "- {} = {} ({})\n",
                recommendation.setting, recommendation.value, recommendation.reason
            ));
        }
        out
    }

    pub fn model_context(&self) -> String {
        let mut out = String::new();
        out.push_str("Repo profile:\n");
        out.push_str(&format!("- root: {}\n", self.root));
        if let Some(branch) = &self.git.branch {
            out.push_str(&format!("- git branch: {branch}\n"));
        }
        out.push_str(&format!(
            "- languages: {}\n",
            join_or(
                self.languages
                    .iter()
                    .filter(|language| language.files > 0)
                    .map(|language| format!(
                        "{} {} files ({})",
                        language.name,
                        language.files,
                        language.semantic_support.as_str()
                    )),
                "none",
            )
        ));
        out.push_str(&format!(
            "- package/build systems: {}\n",
            join_or(
                self.package_managers
                    .iter()
                    .map(|manager| manager.name.clone()),
                "none",
            )
        ));
        out.push_str(&format!(
            "- likely commands: {}\n",
            join_or(
                self.commands
                    .iter()
                    .take(6)
                    .map(|command| format!("{}: {}", command.kind, command.command)),
                "none",
            )
        ));
        out.push_str(&format!(
            "- config files: {}\n",
            join_or(self.config_files.iter().take(8).cloned(), "none")
        ));
        out.push_str(&format!(
            "- ignored/index coverage: {}\n",
            join_or(
                self.ignored_paths
                    .iter()
                    .take(5)
                    .map(|ignored| format!("{} {} paths", ignored.reason, ignored.paths)),
                "none",
            )
        ));
        out.push_str(
            "Use this profile before exploring. Refresh it if the user says the repo shape changed or the profile looks stale.",
        );
        out
    }

    pub fn recommendations_toml(&self) -> String {
        let mut out = String::new();
        out.push_str("# Suggested squeezy.toml settings. Review before committing.\n");
        for recommendation in &self.recommendations {
            if recommendation.setting == "permissions.rules[].target" {
                out.push_str(&format!(
                    "# {}\n# [[permissions.rules]]\n# capability = \"compiler\"\n# target = {}\n# action = \"allow\"\n# source = \"project\"\n\n",
                    recommendation.reason, recommendation.value
                ));
            } else {
                out.push_str(&format!(
                    "# {}\n{} = {}\n\n",
                    recommendation.reason, recommendation.setting, recommendation.value
                ));
            }
        }
        out
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoFingerprint {
    pub value: String,
    pub light_value: String,
    pub markers: Vec<String>,
}

impl RepoFingerprint {
    pub fn detect(
        root: &Path,
        snapshot: Option<&WorkspaceSnapshot>,
        git: &GitState,
    ) -> Result<Self> {
        let mut markers = detect_config_files(root);
        markers.extend(detect_source_dir_markers(root));
        markers.sort();
        markers.dedup();

        let mut light = Sha256::new();
        hash_str(&mut light, &root.display().to_string());
        hash_optional(&mut light, git.branch.as_deref());
        hash_optional(&mut light, git.head.as_deref());
        hash_optional(&mut light, git.default_branch.as_deref());
        hash_optional(
            &mut light,
            git.dirty.map(|dirty| if dirty { "dirty" } else { "clean" }),
        );
        for marker in &markers {
            hash_str(&mut light, marker);
            hash_path_metadata(&mut light, &root.join(marker))?;
        }
        let light_value = hex_digest(light);

        let mut full = Sha256::new();
        hash_str(&mut full, &light_value);
        if let Some(snapshot) = snapshot {
            for file in &snapshot.files {
                hash_str(&mut full, &file.relative_path);
                hash_str(&mut full, &file.hash.0);
                hash_str(&mut full, file.language.display_name());
            }
            for excluded in &snapshot.excluded {
                hash_str(&mut full, &excluded.relative_path);
                hash_str(&mut full, excluded.reason.as_str());
            }
        }
        Ok(Self {
            value: hex_digest(full),
            light_value,
            markers,
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitState {
    #[serde(default)]
    pub vcs_type: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub head: Option<String>,
    #[serde(default)]
    pub default_branch: Option<String>,
    #[serde(default)]
    pub dirty: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectedLanguage {
    pub name: String,
    #[serde(default)]
    pub family: Option<String>,
    pub files: usize,
    pub semantic_support: SemanticSupport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticSupport {
    Supported,
    Fallback,
}

impl SemanticSupport {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Fallback => "fallback",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectedPackageManager {
    pub name: String,
    pub marker: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectedCommand {
    pub kind: String,
    pub command: String,
    pub source: String,
    pub confidence: String,
    pub ambiguous: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IgnoredPathSummary {
    pub reason: String,
    pub paths: usize,
    pub bytes: u64,
    pub samples: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoRecommendation {
    pub setting: String,
    pub value: String,
    pub reason: String,
}

pub fn default_repo_registry_path() -> PathBuf {
    env::var_os("SQUEEZY_REPOS_PATH")
        .map(PathBuf::from)
        .or_else(|| fs_util::user_squeezy_dir().map(|dir| dir.join("repos.toml")))
        .unwrap_or_else(|| PathBuf::from(".squeezy/repos.toml"))
}

pub fn ensure_repo_profile(root: impl AsRef<Path>, graph: &GraphConfig) -> Result<RepoProfileLoad> {
    ensure_repo_profile_at(default_repo_registry_path(), root, graph)
}

pub fn ensure_repo_profile_at(
    registry_path: impl Into<PathBuf>,
    root: impl AsRef<Path>,
    graph: &GraphConfig,
) -> Result<RepoProfileLoad> {
    let registry_path = registry_path.into();
    let root = canonical_root(root)?;
    let mut registry = RepoRegistry::load(&registry_path)?;
    let git = detect_git_state(&root);
    let light = RepoFingerprint::detect(&root, None, &git)?;
    if let Some(profile) = registry.profile_for_root(&root)
        && profile.fingerprint.light_value == light.light_value
        && !profile.repo_id.is_empty()
    {
        return Ok(RepoProfileLoad {
            profile: profile.clone(),
            status: RepoProfileStatus::Reused,
            registry_path,
        });
    }

    let status = if registry.profile_for_root(&root).is_some() {
        RepoProfileStatus::Refreshed
    } else {
        RepoProfileStatus::Created
    };
    let profile = RepoProfile::detect(&root, graph)?;
    registry.upsert(profile.clone());
    registry.save(&registry_path)?;
    Ok(RepoProfileLoad {
        profile,
        status,
        registry_path,
    })
}

pub fn refresh_repo_profile(
    root: impl AsRef<Path>,
    graph: &GraphConfig,
) -> Result<RepoProfileLoad> {
    refresh_repo_profile_at(default_repo_registry_path(), root, graph)
}

pub fn refresh_repo_profile_at(
    registry_path: impl Into<PathBuf>,
    root: impl AsRef<Path>,
    graph: &GraphConfig,
) -> Result<RepoProfileLoad> {
    let registry_path = registry_path.into();
    let root = canonical_root(root)?;
    let mut registry = RepoRegistry::load(&registry_path)?;
    let status = if registry.profile_for_root(&root).is_some() {
        RepoProfileStatus::Refreshed
    } else {
        RepoProfileStatus::Created
    };
    let profile = RepoProfile::detect(&root, graph)?;
    registry.upsert(profile.clone());
    registry.save(&registry_path)?;
    Ok(RepoProfileLoad {
        profile,
        status,
        registry_path,
    })
}

fn canonical_root(root: impl AsRef<Path>) -> Result<PathBuf> {
    fs::canonicalize(root.as_ref())
        .map_err(|err| SqueezyError::Workspace(format!("invalid workspace root: {err}")))
}

fn crawl_options_from_graph_config(config: &GraphConfig) -> CrawlOptions {
    CrawlOptions {
        include_hidden: config.include_hidden,
        max_file_bytes: config.max_file_bytes,
        require_indexing_signal: config.require_indexing_signal,
        languages: config.languages.clone(),
        policy: IndexingPolicy {
            include: config.include.clone(),
            exclude: config.exclude.clone(),
            include_classes: config.include_classes.clone(),
            exclude_classes: config.exclude_classes.clone(),
        },
    }
}

fn detect_git_state(root: &Path) -> GitState {
    // Skip the four git subprocess invocations when there is no `.git`
    // directory. Onboarding runs on every CLI startup, so avoiding the
    // forks keeps the steady-state cost near zero in non-git roots.
    if !root.join(".git").exists() {
        return GitState::default();
    }
    // The four probes are independent, and `git status --porcelain` alone can
    // run for tens of milliseconds on a large worktree. Fan them out across
    // threads so onboarding pays the cost of the single slowest probe rather
    // than their sum. All four outputs feed the light fingerprint
    // (`RepoFingerprint::detect`), so none can be dropped.
    let (branch, head, default_branch, dirty) = std::thread::scope(|scope| {
        let branch = scope.spawn(|| git_output(root, &["branch", "--show-current"]));
        let head = scope.spawn(|| git_output(root, &["rev-parse", "HEAD"]));
        let default_branch = scope.spawn(|| {
            git_output(
                root,
                &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
            )
        });
        let dirty = scope.spawn(|| {
            git_output(root, &["status", "--porcelain"]).map(|status| !status.is_empty())
        });
        (
            branch.join().ok().flatten(),
            head.join().ok().flatten(),
            default_branch.join().ok().flatten(),
            dirty.join().ok().flatten(),
        )
    });
    GitState {
        vcs_type: Some("git".to_string()),
        branch,
        head,
        default_branch,
        dirty,
    }
}

fn git_output(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn detect_languages(snapshot: &WorkspaceSnapshot) -> Vec<DetectedLanguage> {
    let mut counts = HashMap::<LanguageKind, usize>::new();
    for file in &snapshot.files {
        *counts.entry(file.language).or_default() += 1;
    }
    for file in &snapshot.unsupported {
        if file.reason == UnsupportedReason::LanguageDisabled {
            *counts.entry(classify_language(&file.path)).or_default() += 1;
        }
    }
    let mut languages = counts
        .into_iter()
        .filter(|(kind, _)| !matches!(kind, LanguageKind::Unknown | LanguageKind::Unsupported))
        .map(|(kind, files)| {
            let family = kind.family();
            DetectedLanguage {
                name: kind.display_name().to_string(),
                family: family.map(|family| family.id().to_string()),
                files,
                semantic_support: if family.is_some() {
                    SemanticSupport::Supported
                } else {
                    SemanticSupport::Fallback
                },
            }
        })
        .collect::<Vec<_>>();
    languages.sort_by(|left, right| left.name.cmp(&right.name));
    languages
}

fn detect_package_managers(root: &Path) -> Vec<DetectedPackageManager> {
    let mut managers = Vec::new();
    push_manager(&mut managers, root, "cargo", "Cargo.toml");
    if root.join("package.json").exists() {
        let name = if root.join("pnpm-lock.yaml").exists() {
            "pnpm"
        } else if root.join("yarn.lock").exists() {
            "yarn"
        } else if root.join("bun.lockb").exists() {
            "bun"
        } else {
            "npm"
        };
        managers.push(DetectedPackageManager {
            name: name.to_string(),
            marker: "package.json".to_string(),
        });
    }
    if root.join("pyproject.toml").exists()
        || root.join("requirements.txt").exists()
        || root.join("setup.py").exists()
    {
        let name = if root.join("uv.lock").exists() {
            "uv"
        } else if root.join("poetry.lock").exists() {
            "poetry"
        } else {
            "python"
        };
        managers.push(DetectedPackageManager {
            name: name.to_string(),
            marker: python_marker(root),
        });
    }
    push_manager(&mut managers, root, "go", "go.mod");
    push_manager(&mut managers, root, "make", "Makefile");
    push_manager(&mut managers, root, "just", "Justfile");
    push_manager(&mut managers, root, "cmake", "CMakeLists.txt");
    push_manager(&mut managers, root, "maven", "pom.xml");
    if root.join("build.gradle").exists() || root.join("build.gradle.kts").exists() {
        managers.push(DetectedPackageManager {
            name: "gradle".to_string(),
            marker: if root.join("build.gradle.kts").exists() {
                "build.gradle.kts".to_string()
            } else {
                "build.gradle".to_string()
            },
        });
    }
    if has_extension_at_root(root, "sln") || has_extension_at_root(root, "slnx") {
        managers.push(DetectedPackageManager {
            name: "dotnet".to_string(),
            marker: "solution file".to_string(),
        });
    }
    managers.sort_by(|left, right| left.name.cmp(&right.name));
    managers.dedup_by(|left, right| left.name == right.name);
    managers
}

fn push_manager(managers: &mut Vec<DetectedPackageManager>, root: &Path, name: &str, marker: &str) {
    if root.join(marker).exists() {
        managers.push(DetectedPackageManager {
            name: name.to_string(),
            marker: marker.to_string(),
        });
    }
}

fn python_marker(root: &Path) -> String {
    for marker in ["pyproject.toml", "requirements.txt", "setup.py"] {
        if root.join(marker).exists() {
            return marker.to_string();
        }
    }
    "python files".to_string()
}

fn detect_commands(root: &Path) -> Vec<DetectedCommand> {
    let mut commands = Vec::new();
    if root.join("Cargo.toml").exists() {
        commands.extend([
            command("build", "cargo build --workspace", "Cargo.toml", "high"),
            command("test", "cargo test --workspace", "Cargo.toml", "high"),
            command("fmt", "cargo fmt --check", "Cargo.toml", "high"),
            command(
                "lint",
                "cargo clippy --workspace --all-targets -- -D warnings",
                "Cargo.toml",
                "medium",
            ),
        ]);
    }
    commands.extend(package_json_commands(root));
    if root.join("go.mod").exists() {
        commands.extend([
            command("build", "go build ./...", "go.mod", "high"),
            command("test", "go test ./...", "go.mod", "high"),
            command("fmt", "gofmt -w <files>", "go.mod", "low"),
        ]);
    }
    if root.join("pyproject.toml").exists() || root.join("requirements.txt").exists() {
        let has_tests = root.join("tests").is_dir();
        commands.push(command(
            "test",
            if has_tests {
                "pytest"
            } else {
                "python -m pytest"
            },
            python_marker(root),
            if has_tests { "medium" } else { "low" },
        ));
    }
    if root.join("Makefile").exists() {
        commands.extend([
            command("build", "make", "Makefile", "medium"),
            command("test", "make test", "Makefile", "medium"),
        ]);
    }
    if root.join("Justfile").exists() {
        commands.push(command("test", "just test", "Justfile", "medium"));
    }
    mark_ambiguous_commands(&mut commands);
    commands.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| left.command.cmp(&right.command))
    });
    commands
}

fn package_json_commands(root: &Path) -> Vec<DetectedCommand> {
    let package = root.join("package.json");
    if !package.exists() {
        return Vec::new();
    }
    let runner = if root.join("pnpm-lock.yaml").exists() {
        "pnpm"
    } else if root.join("yarn.lock").exists() {
        "yarn"
    } else if root.join("bun.lockb").exists() {
        "bun"
    } else {
        "npm run"
    };
    let Ok(text) = fs::read_to_string(package) else {
        return vec![command("test", "npm test", "package.json", "low")];
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return vec![command("test", "npm test", "package.json", "low")];
    };
    let scripts = value.get("scripts").and_then(|value| value.as_object());
    let mut commands = Vec::new();
    for (kind, script) in [
        ("build", "build"),
        ("test", "test"),
        ("fmt", "format"),
        ("lint", "lint"),
    ] {
        if scripts.is_some_and(|scripts| scripts.contains_key(script)) {
            let command_text = if runner == "npm run" && script == "test" {
                "npm test".to_string()
            } else if runner == "npm run" {
                format!("npm run {script}")
            } else {
                format!("{runner} {script}")
            };
            commands.push(command(kind, &command_text, "package.json scripts", "high"));
        }
    }
    if commands.is_empty() {
        commands.push(command("test", "npm test", "package.json", "low"));
    }
    commands
}

fn command(
    kind: &str,
    command: &str,
    source: impl Into<String>,
    confidence: &str,
) -> DetectedCommand {
    DetectedCommand {
        kind: kind.to_string(),
        command: command.to_string(),
        source: source.into(),
        confidence: confidence.to_string(),
        ambiguous: false,
    }
}

fn mark_ambiguous_commands(commands: &mut [DetectedCommand]) {
    let mut counts = BTreeMap::<String, usize>::new();
    for command in commands.iter() {
        *counts.entry(command.kind.clone()).or_default() += 1;
    }
    for command in commands {
        command.ambiguous = counts.get(&command.kind).copied().unwrap_or_default() > 1;
    }
}

fn detect_config_files(root: &Path) -> Vec<String> {
    let mut files = MARKER_FILES
        .iter()
        .filter(|marker| root.join(marker).exists())
        .map(|marker| (*marker).to_string())
        .collect::<Vec<_>>();
    if root.join(".github/workflows").is_dir() {
        files.push(".github/workflows".to_string());
    }
    for extension in ["csproj", "sln", "slnx"] {
        files.extend(root_extension_files(root, extension));
    }
    files.sort();
    files.dedup();
    files
}

fn detect_source_dir_markers(root: &Path) -> Vec<String> {
    ["src", "tests", "crates", "app", "cmd", "pkg"]
        .into_iter()
        .filter(|dir| root.join(dir).is_dir())
        .map(str::to_string)
        .collect()
}

fn detect_ignored_paths(snapshot: &WorkspaceSnapshot) -> Vec<IgnoredPathSummary> {
    let mut ignored = snapshot
        .coverage
        .reasons
        .iter()
        .map(|(reason, coverage)| IgnoredPathSummary {
            reason: reason.clone(),
            paths: coverage.files + coverage.dirs,
            bytes: coverage.bytes,
            samples: coverage.samples.clone(),
        })
        .collect::<Vec<_>>();
    ignored.sort_by(|left, right| {
        right
            .paths
            .cmp(&left.paths)
            .then_with(|| left.reason.cmp(&right.reason))
    });
    ignored
}

fn detect_recommendations(
    languages: &[DetectedLanguage],
    ignored_paths: &[IgnoredPathSummary],
    graph: &GraphConfig,
    package_managers: &[DetectedPackageManager],
) -> Vec<RepoRecommendation> {
    let mut recommendations = Vec::new();
    let mut graph_languages = languages
        .iter()
        .filter_map(|language| language.family.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    graph_languages.sort();
    if !graph_languages.is_empty() && graph.languages != graph_languages {
        recommendations.push(RepoRecommendation {
            setting: "graph.languages".to_string(),
            value: toml_array(&graph_languages),
            reason: "detected semantic source languages in this repo".to_string(),
        });
    }
    if ignored_paths
        .iter()
        .any(|ignored| ignored.reason == ExclusionReason::Generated.as_str())
    {
        recommendations.push(RepoRecommendation {
            setting: "graph.exclude_classes".to_string(),
            value: toml_array(&["generated"]),
            reason: "generated files were detected and should stay out of graph context"
                .to_string(),
        });
    }
    if package_managers
        .iter()
        .any(|manager| manager.name == "cargo")
    {
        recommendations.push(RepoRecommendation {
            setting: "permissions.rules[].target".to_string(),
            value: "\"cargo test:*\"".to_string(),
            reason: "Cargo test is the detected verification path; add an explicit rule if desired"
                .to_string(),
        });
    }
    recommendations
}

fn root_extension_files(root: &Path, extension: &str) -> Vec<String> {
    fs::read_dir(root)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let path = entry.path();
            (path.extension().and_then(|ext| ext.to_str()) == Some(extension))
                .then(|| path.file_name()?.to_str().map(str::to_string))
                .flatten()
        })
        .collect()
}

fn has_extension_at_root(root: &Path, extension: &str) -> bool {
    !root_extension_files(root, extension).is_empty()
}

fn hash_path_metadata(hasher: &mut Sha256, path: &Path) -> Result<()> {
    let metadata = fs::metadata(path)?;
    hash_str(hasher, &metadata.len().to_string());
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis().to_string())
        .unwrap_or_default();
    hash_str(hasher, &modified);
    if metadata.is_file() && metadata.len() <= 64 * 1024 {
        let bytes = fs::read(path)?;
        hasher.update(bytes);
    }
    Ok(())
}

fn hash_str(hasher: &mut Sha256, value: &str) {
    hasher.update(value.as_bytes());
    hasher.update([0]);
}

fn hash_optional(hasher: &mut Sha256, value: Option<&str>) {
    hash_str(hasher, value.unwrap_or(""));
}

fn hex_digest(hasher: Sha256) -> String {
    use std::fmt::Write as _;

    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn now_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default()
}

fn join_or(items: impl IntoIterator<Item = String>, fallback: &str) -> String {
    let items = items.into_iter().collect::<Vec<_>>();
    if items.is_empty() {
        fallback.to_string()
    } else {
        items.join(", ")
    }
}

fn toml_array(values: &[impl AsRef<str>]) -> String {
    let body = values
        .iter()
        .map(|value| toml_string(value.as_ref()))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{body}]")
}

fn push_optional_string_field(out: &mut String, key: &str, value: Option<&str>) {
    if let Some(value) = value {
        out.push_str(&format!("{key} = {}\n", toml_string(value)));
    }
}

fn toml_string(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
#[path = "repo_profile_tests.rs"]
mod tests;
