use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::UNIX_EPOCH,
};

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use squeezy_core::{ContentHash, FileId, Freshness, LanguageKind, Result, SqueezyError};

pub const CRATE_NAME: &str = "squeezy-workspace";
const SOURCE_SCAN_MAX_DEPTH: usize = 2;
const SOURCE_SCAN_MAX_ENTRIES: usize = 1_000;
const DEFAULT_MAX_FILE_BYTES: u64 = 1_000_000;
const DEFAULT_JAVA_MAX_FILE_BYTES: u64 = 2_000_000;
const BINARY_GENERATED_PREFIX_BYTES: usize = 4096;
const CODE_PROJECT_MARKERS: &[&str] = &[
    "Cargo.toml",
    "CMakeLists.txt",
    "Directory.Build.props",
    "Directory.Build.targets",
    "Dockerfile",
    "Justfile",
    "Makefile",
    "MODULE.bazel",
    "Taskfile.yml",
    "WORKSPACE",
    "build.gradle",
    "build.gradle.kts",
    "composer.json",
    "docker-compose.yml",
    "go.mod",
    "global.json",
    "gradlew",
    "noxfile.py",
    "package.json",
    "package-lock.json",
    "packages.lock.json",
    "pom.xml",
    "pnpm-lock.yaml",
    "pyproject.toml",
    "requirements.txt",
    "setup.cfg",
    "setup.py",
    "tox.ini",
    "tsconfig.json",
    "yarn.lock",
    ".github/workflows",
    "BUILD",
    "BUILD.bazel",
    "Pipfile",
    "poetry.lock",
    "uv.lock",
];
const CODE_DIRECTORY_MARKERS: &[&str] = &[
    "app", "cmd", "crates", "include", "internal", "lib", "packages", "pkg", "src",
];

pub fn crate_name() -> &'static str {
    CRATE_NAME
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrawlOptions {
    pub include_hidden: bool,
    pub max_file_bytes: u64,
    pub require_indexing_signal: bool,
    pub policy: IndexingPolicy,
}

impl Default for CrawlOptions {
    fn default() -> Self {
        Self {
            include_hidden: false,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            require_indexing_signal: true,
            policy: IndexingPolicy::default(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexingPolicy {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub include_classes: Vec<String>,
    pub exclude_classes: Vec<String>,
}

impl IndexingPolicy {
    pub fn compile(&self) -> Result<CompiledIndexingPolicy> {
        let include_patterns = self
            .include
            .iter()
            .map(|pattern| normalize_path(pattern, false))
            .collect::<Vec<_>>();
        Ok(CompiledIndexingPolicy {
            include_patterns,
            include: build_glob_set(&self.include)?,
            exclude: build_glob_set(&self.exclude)?,
            include_classes: self
                .include_classes
                .iter()
                .map(|class| normalize_reason_class(class))
                .collect(),
            exclude_classes: self
                .exclude_classes
                .iter()
                .map(|class| normalize_reason_class(class))
                .collect(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct CompiledIndexingPolicy {
    include_patterns: Vec<String>,
    include: GlobSet,
    exclude: GlobSet,
    include_classes: Vec<String>,
    exclude_classes: Vec<String>,
}

impl CompiledIndexingPolicy {
    pub fn empty() -> Self {
        IndexingPolicy::default()
            .compile()
            .expect("default policy compiles")
    }

    pub fn path_reason(&self, relative_path: &str, is_dir: bool) -> Option<ExclusionReason> {
        let normalized = normalize_path(relative_path, is_dir);
        let path = Path::new(&normalized);
        if self.exclude.is_match(path) {
            return Some(ExclusionReason::UserExclude);
        }
        let reason = default_path_reason(&normalized, is_dir)?;
        if self.excludes_class(reason) {
            return Some(reason);
        }
        if self.include.is_match(path) || self.includes_class(reason) {
            return None;
        }
        // Don't prune a directory if an `include` glob can match files
        // *under* it. Otherwise `include = ["vendor/allowed/**"]` would
        // never see `vendor/allowed/foo.rs` because the walker would have
        // already skipped `vendor/`.
        if is_dir && self.include_could_descend(&normalized) {
            return None;
        }
        Some(reason)
    }

    pub fn file_reason(
        &self,
        relative_path: &str,
        size_bytes: u64,
        max_file_bytes: u64,
        prefix: Option<&[u8]>,
    ) -> Option<ExclusionReason> {
        if let Some(reason) = self.path_reason(relative_path, false) {
            return Some(reason);
        }
        if size_bytes > max_file_bytes && !self.includes_class(ExclusionReason::LargeFile) {
            return Some(ExclusionReason::LargeFile);
        }
        let bytes = prefix.unwrap_or_default();
        if looks_binary(bytes) && !self.includes_class(ExclusionReason::Binary) {
            return Some(ExclusionReason::Binary);
        }
        if looks_generated(bytes) && !self.includes_class(ExclusionReason::Generated) {
            return Some(ExclusionReason::Generated);
        }
        None
    }

    pub fn includes_class(&self, reason: ExclusionReason) -> bool {
        let class = reason.as_str();
        self.include_classes
            .iter()
            .any(|candidate| candidate == class)
    }

    pub fn excludes_class(&self, reason: ExclusionReason) -> bool {
        let class = reason.as_str();
        self.exclude_classes
            .iter()
            .any(|candidate| candidate == class)
    }

    fn include_could_descend(&self, dir_with_slash: &str) -> bool {
        if self.include_patterns.is_empty() {
            return false;
        }
        for raw in &self.include_patterns {
            let literal = literal_prefix(raw);
            if literal.is_empty() {
                // Pattern starts with a wildcard (`**/foo`, `*.rs`, etc.):
                // we must descend everywhere to find potential matches.
                return true;
            }
            if literal.starts_with(dir_with_slash) {
                return true;
            }
            if literal.ends_with('/') && dir_with_slash.starts_with(literal) {
                return true;
            }
        }
        false
    }
}

fn literal_prefix(pattern: &str) -> &str {
    let cutoff = pattern.find(['*', '?', '[', '{']).unwrap_or(pattern.len());
    &pattern[..cutoff]
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRecord {
    pub id: FileId,
    pub path: PathBuf,
    pub relative_path: String,
    pub hash: ContentHash,
    pub size_bytes: u64,
    pub modified_unix_millis: u128,
    pub language: LanguageKind,
    pub freshness: Freshness,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedFile {
    pub path: PathBuf,
    pub relative_path: String,
    pub extension: Option<String>,
    pub size_bytes: u64,
    pub reason: UnsupportedReason,
    pub suggested_fallback: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsupportedReason {
    UnsupportedExtension,
    TooLarge,
    BinaryLike,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExcludedPath {
    pub path: PathBuf,
    pub relative_path: String,
    pub size_bytes: u64,
    pub reason: ExclusionReason,
    pub is_dir: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExclusionReason {
    VcsMetadata,
    Vendor,
    DependencyCache,
    BuildOutput,
    Generated,
    Lockfile,
    Binary,
    LargeFile,
    UserExclude,
    Hidden,
}

impl ExclusionReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::VcsMetadata => "vcs_metadata",
            Self::Vendor => "vendor",
            Self::DependencyCache => "dependency_cache",
            Self::BuildOutput => "build_output",
            Self::Generated => "generated",
            Self::Lockfile => "lockfile",
            Self::Binary => "binary",
            Self::LargeFile => "large_file",
            Self::UserExclude => "user_exclude",
            Self::Hidden => "hidden",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexCoverage {
    pub skipped_files: usize,
    pub skipped_dirs: usize,
    pub skipped_bytes: u64,
    pub reasons: BTreeMap<String, IndexReasonCoverage>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexReasonCoverage {
    pub files: usize,
    pub dirs: usize,
    pub bytes: u64,
    pub samples: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSnapshot {
    pub root: PathBuf,
    pub files: Vec<FileRecord>,
    pub unsupported: Vec<UnsupportedFile>,
    pub excluded: Vec<ExcludedPath>,
    pub path_conflicts: Vec<PathConflict>,
    pub coverage: IndexCoverage,
    pub walk_errors: Vec<String>,
    pub indexing_decision: IndexingDecision,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathConflict {
    pub normalized_relative_path: String,
    pub relative_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexingDecision {
    pub should_index: bool,
    pub reason: String,
    pub positive_signals: Vec<String>,
    pub negative_signals: Vec<String>,
}

#[derive(Debug, Clone)]
struct IndexingDecisionContext {
    canonical_homes: Vec<PathBuf>,
}

impl IndexingDecisionContext {
    fn from_env() -> Self {
        Self {
            canonical_homes: home_dirs_from_env(),
        }
    }

    fn is_home_dir(&self, root: &Path) -> bool {
        self.canonical_homes
            .iter()
            .any(|home| filesystem_paths_match(home, root))
    }
}

#[derive(Debug, Clone)]
pub struct WorkspaceCrawler {
    options: CrawlOptions,
    compiled_policy: Arc<CompiledIndexingPolicy>,
}

impl WorkspaceCrawler {
    pub fn new(options: CrawlOptions) -> Self {
        // Default policies always compile; user-supplied policies must be
        // validated up front via `IndexingPolicy::compile` to surface glob
        // syntax errors loudly rather than silently disabling the policy.
        let compiled_policy = options
            .policy
            .compile()
            .expect("policy globs must be valid; validate via IndexingPolicy::compile() first");
        Self {
            options,
            compiled_policy: Arc::new(compiled_policy),
        }
    }

    pub fn try_new(options: CrawlOptions) -> Result<Self> {
        let compiled_policy = Arc::new(options.policy.compile()?);
        Ok(Self {
            options,
            compiled_policy,
        })
    }

    pub fn policy(&self) -> &Arc<CompiledIndexingPolicy> {
        &self.compiled_policy
    }

    pub fn crawl(&self, root: impl AsRef<Path>) -> Result<WorkspaceSnapshot> {
        let root = fs::canonicalize(root.as_ref())?;
        let indexing_decision = decide_indexing(&root, self.options.require_indexing_signal);
        if !indexing_decision.should_index {
            return Ok(WorkspaceSnapshot {
                root,
                files: Vec::new(),
                unsupported: Vec::new(),
                excluded: Vec::new(),
                path_conflicts: Vec::new(),
                coverage: IndexCoverage::default(),
                walk_errors: Vec::new(),
                indexing_decision,
            });
        }

        // Shared collector for directories pruned during walk. Using a Mutex
        // is OK because the sequential walker calls the filter closure on
        // one thread at a time.
        let pruned_dirs: Arc<Mutex<Vec<ExcludedPath>>> = Arc::new(Mutex::new(Vec::new()));

        let mut walker = WalkBuilder::new(&root);
        walker
            .hidden(false)
            .git_ignore(true)
            .git_exclude(true)
            .parents(true)
            .require_git(false);

        let filter_root = root.clone();
        let filter_policy = self.compiled_policy.clone();
        let filter_pruned = pruned_dirs.clone();
        let include_hidden = self.options.include_hidden;
        walker.filter_entry(move |entry| {
            keep_entry(
                entry,
                &filter_root,
                filter_policy.as_ref(),
                include_hidden,
                &filter_pruned,
            )
        });

        let mut files = Vec::new();
        let mut unsupported = Vec::new();
        let mut excluded = Vec::new();
        let mut coverage = IndexCoverage::default();
        let mut walk_errors = Vec::new();

        for entry in walker.build() {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    walk_errors.push(err.to_string());
                    continue;
                }
            };

            let Some(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                continue;
            }
            if !file_type.is_file() && !file_type.is_symlink() {
                continue;
            }
            let path = entry.into_path();
            let relative_path = relative_path(&root, &path)?;

            let metadata = fs::metadata(&path)?;
            if !metadata.is_file() {
                continue;
            }
            if file_type.is_symlink() {
                let target = fs::canonicalize(&path)?;
                if !target.starts_with(&root) {
                    continue;
                }
            }
            let size_bytes = metadata.len();
            let language = classify_language(&path);
            // Java source files frequently contain many nested declarations
            // in a single file, so we lift the default cap when the user has
            // not configured an explicit one.
            let max_file_bytes = if language == LanguageKind::Java
                && self.options.max_file_bytes == DEFAULT_MAX_FILE_BYTES
            {
                DEFAULT_JAVA_MAX_FILE_BYTES
            } else {
                self.options.max_file_bytes
            };

            if let Some(reason) = self.compiled_policy.path_reason(&relative_path, false) {
                record_excluded_file(
                    &mut excluded,
                    &mut coverage,
                    &path,
                    relative_path,
                    size_bytes,
                    reason,
                );
                continue;
            }

            // Cheap rejection: skip the file before reading it when it is
            // bigger than the per-file byte cap, unless the user opted into
            // indexing large files via `include_classes = ["large_file"]`.
            if size_bytes > max_file_bytes
                && !self
                    .compiled_policy
                    .includes_class(ExclusionReason::LargeFile)
            {
                record_excluded_file(
                    &mut excluded,
                    &mut coverage,
                    &path,
                    relative_path,
                    size_bytes,
                    ExclusionReason::LargeFile,
                );
                continue;
            }

            let bytes = fs::read(&path)?;
            if looks_binary(&bytes) {
                if self.compiled_policy.includes_class(ExclusionReason::Binary) {
                    unsupported.push(unsupported_file(
                        &path,
                        relative_path.clone(),
                        extension_string(&path),
                        size_bytes,
                        UnsupportedReason::BinaryLike,
                    ));
                } else {
                    record_excluded_file(
                        &mut excluded,
                        &mut coverage,
                        &path,
                        relative_path,
                        size_bytes,
                        ExclusionReason::Binary,
                    );
                }
                continue;
            }
            if looks_generated(&bytes)
                && !self
                    .compiled_policy
                    .includes_class(ExclusionReason::Generated)
            {
                record_excluded_file(
                    &mut excluded,
                    &mut coverage,
                    &path,
                    relative_path,
                    size_bytes,
                    ExclusionReason::Generated,
                );
                continue;
            }

            if language == LanguageKind::Unsupported {
                unsupported.push(unsupported_file(
                    &path,
                    relative_path.clone(),
                    extension_string(&path),
                    size_bytes,
                    UnsupportedReason::UnsupportedExtension,
                ));
            }

            let modified_unix_millis = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis())
                .unwrap_or_default();

            files.push(FileRecord {
                id: FileId::new(relative_path.clone()),
                path,
                relative_path,
                hash: ContentHash::new(stable_content_hash(&bytes)),
                size_bytes,
                modified_unix_millis,
                language,
                freshness: Freshness::Fresh,
            });
        }

        refine_c_family_header_languages(&mut files);

        // Pull pruned directories collected by `filter_entry` into the
        // snapshot. We do this once so each excluded directory shows up
        // exactly once, regardless of how many children it had.
        let mut pruned_dirs = pruned_dirs
            .lock()
            .map(|mut guard| std::mem::take(&mut *guard))
            .unwrap_or_default();
        for entry in pruned_dirs.drain(..) {
            record_excluded_dir_entry(&mut coverage, &entry);
            excluded.push(entry);
        }

        files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        unsupported.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        excluded.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        let path_conflicts = detect_path_conflicts(&files);

        Ok(WorkspaceSnapshot {
            root,
            files,
            unsupported,
            excluded,
            path_conflicts,
            coverage,
            walk_errors,
            indexing_decision,
        })
    }
}

fn keep_entry(
    entry: &ignore::DirEntry,
    root: &Path,
    policy: &CompiledIndexingPolicy,
    include_hidden: bool,
    pruned: &Mutex<Vec<ExcludedPath>>,
) -> bool {
    let Some(file_type) = entry.file_type() else {
        return true;
    };
    let path = entry.path();
    let Ok(rel) = relative_path(root, path) else {
        return true;
    };
    if rel.is_empty() {
        return true;
    }

    let is_hidden = path
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.starts_with('.'))
        .unwrap_or(false);

    if file_type.is_dir() {
        if let Some(reason) = policy.path_reason(&rel, true) {
            if let Ok(mut guard) = pruned.lock() {
                guard.push(ExcludedPath {
                    path: path.to_path_buf(),
                    relative_path: rel,
                    size_bytes: 0,
                    reason,
                    is_dir: true,
                });
            }
            return false;
        }
        if is_hidden && !include_hidden {
            if let Ok(mut guard) = pruned.lock() {
                guard.push(ExcludedPath {
                    path: path.to_path_buf(),
                    relative_path: rel,
                    size_bytes: 0,
                    reason: ExclusionReason::Hidden,
                    is_dir: true,
                });
            }
            return false;
        }
        return true;
    }

    if !file_type.is_file() {
        return true;
    }

    if is_hidden && !include_hidden && policy.path_reason(&rel, false).is_none() {
        // Unclassified hidden file: skip silently. We don't count these in
        // coverage to keep the report focused on policy-driven exclusions.
        return false;
    }

    true
}

pub fn classify_language(path: &Path) -> LanguageKind {
    let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
        return LanguageKind::Unknown;
    };
    if extension.bytes().any(|byte| byte.is_ascii_uppercase()) {
        LanguageKind::from_extension(&extension.to_ascii_lowercase())
    } else {
        LanguageKind::from_extension(extension)
    }
}

fn extension_string(path: &Path) -> Option<String> {
    path.extension()
        .map(|extension| extension.to_string_lossy().into_owned())
}

fn refine_c_family_header_languages(files: &mut [FileRecord]) {
    let mut by_stem = BTreeMap::<String, CFamilySiblingFlags>::new();
    let mut c_files = 0usize;
    let mut cpp_files = 0usize;
    for file in files.iter() {
        match file.language {
            LanguageKind::C => c_files += 1,
            LanguageKind::Cpp if !is_plain_c_header(&file.relative_path) => cpp_files += 1,
            _ => {}
        }
        if !is_plain_c_header(&file.relative_path)
            && let Some(stem) = path_without_extension(&file.relative_path)
        {
            match file.language {
                LanguageKind::C => by_stem.entry(stem.to_string()).or_default().has_c = true,
                LanguageKind::Cpp => by_stem.entry(stem.to_string()).or_default().has_cpp = true,
                _ => {}
            }
        }
    }
    let project_default = if c_files > cpp_files {
        LanguageKind::C
    } else {
        LanguageKind::Cpp
    };
    for file in files.iter_mut() {
        if !is_plain_c_header(&file.relative_path) {
            continue;
        }
        let Some(stem) = path_without_extension(&file.relative_path) else {
            file.language = project_default;
            continue;
        };
        let sibling_languages = by_stem.get(stem);
        file.language = if sibling_languages
            .map(|languages| languages.has_c)
            .unwrap_or(false)
        {
            LanguageKind::C
        } else if sibling_languages
            .map(|languages| languages.has_cpp)
            .unwrap_or(false)
        {
            LanguageKind::Cpp
        } else {
            project_default
        };
    }
}

#[derive(Debug, Default)]
struct CFamilySiblingFlags {
    has_c: bool,
    has_cpp: bool,
}

fn is_plain_c_header(relative_path: &str) -> bool {
    relative_path
        .rsplit_once('.')
        .map(|(_, extension)| extension.eq_ignore_ascii_case("h"))
        .unwrap_or(false)
}

fn path_without_extension(relative_path: &str) -> Option<&str> {
    relative_path.rsplit_once('.').map(|(stem, _)| stem)
}

pub fn decide_indexing(root: &Path, require_signal: bool) -> IndexingDecision {
    if !require_signal {
        return IndexingDecision {
            should_index: true,
            reason: "indexing signal check disabled".to_string(),
            positive_signals: Vec::new(),
            negative_signals: Vec::new(),
        };
    }

    let mut positive_signals = Vec::new();
    let mut negative_signals = Vec::new();
    let context = IndexingDecisionContext::from_env();
    let workspace_signals = scan_workspace_signals(root);
    let root_profile = WorkspaceRootProfile::from_path(root);
    let mut blocked_by_root = false;

    if context.is_home_dir(root) {
        blocked_by_root = true;
        negative_signals.push(format!(
            "workspace root is the user's home directory ({})",
            root_profile.normalized
        ));
    }
    for signal in root_profile.negative_signals() {
        if signal.blocking {
            blocked_by_root = true;
        }
        negative_signals.push(signal.message);
    }

    let mut has_strong_positive = false;

    if let Some(marker) = vcs_marker_signal(root, &context) {
        has_strong_positive = true;
        positive_signals.push(marker);
    }
    if workspace_signals.has_readme {
        positive_signals.push("README at workspace root".to_string());
    }
    for marker in workspace_signals.project_markers {
        has_strong_positive = true;
        positive_signals.push(marker);
    }
    for source in workspace_signals.shallow_source_markers {
        has_strong_positive = true;
        positive_signals.push(source);
    }
    for code_dir in workspace_signals.code_directory_markers {
        has_strong_positive = true;
        positive_signals.push(code_dir);
    }
    if is_personal_folder(root) {
        negative_signals.push("workspace root looks like a personal folder".to_string());
    }

    let should_index = !blocked_by_root && has_strong_positive;
    let reason = if should_index {
        format!("indexing allowed: {}", positive_signals.join(", "))
    } else if blocked_by_root {
        format!("indexing skipped: {}", negative_signals.join(", "))
    } else if positive_signals
        .iter()
        .any(|signal| signal.contains("README"))
    {
        "indexing skipped: README alone is a weak signal without repository, project config, or shallow source files"
            .to_string()
    } else {
        "indexing skipped: no VCS marker, project config, or shallow source file".to_string()
    };

    IndexingDecision {
        should_index,
        reason,
        positive_signals,
        negative_signals,
    }
}

fn is_protected_root(root: &Path) -> bool {
    if WorkspaceRootProfile::from_path(root)
        .negative_signals()
        .iter()
        .any(|signal| signal.blocking)
    {
        return true;
    }
    const PROTECTED: &[&str] = &[
        "/",
        "/Applications",
        "/Library",
        "/Network",
        "/System",
        "/Users",
        "/Volumes",
        "/bin",
        "/dev",
        "/etc",
        "/opt",
        "/private",
        "/sbin",
        "/usr",
        "/var",
    ];
    PROTECTED.iter().any(|path| Path::new(path) == root)
}

fn is_personal_folder(root: &Path) -> bool {
    let Some(name) = root.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    matches!(
        name.to_ascii_lowercase().as_str(),
        "desktop" | "documents" | "downloads" | "pictures" | "videos" | "music" | "saved games"
    ) || is_cloud_sync_root_name(name)
}

fn vcs_marker_signal(root: &Path, context: &IndexingDecisionContext) -> Option<String> {
    for (index, ancestor) in root.ancestors().enumerate() {
        if index > 0 && (context.is_home_dir(ancestor) || is_protected_root(ancestor)) {
            break;
        }
        if let Some(marker) = vcs_marker_at(ancestor) {
            let location = if index == 0 {
                "workspace root"
            } else {
                "workspace ancestor"
            };
            return Some(format!("VCS marker {marker} at {location}"));
        }
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRootProfile {
    pub original: String,
    pub canonical: Option<String>,
    pub normalized: String,
    pub kind: WorkspaceRootKind,
}

impl WorkspaceRootProfile {
    pub fn from_path(root: &Path) -> Self {
        let original = root.display().to_string();
        let canonical = fs::canonicalize(root)
            .ok()
            .map(|path| normalized_filesystem_path(&path));
        let normalized = normalized_filesystem_path(root);
        let kind = WorkspaceRootKind::from_normalized(&normalized);
        Self {
            original,
            canonical,
            normalized,
            kind,
        }
    }

    fn negative_signals(&self) -> Vec<RootSafetySignal> {
        let mut signals = Vec::new();
        match self.kind {
            WorkspaceRootKind::WindowsDriveRoot => {
                signals.push(RootSafetySignal::blocking(format!(
                    "workspace root is a Windows drive root ({})",
                    self.normalized
                )))
            }
            WorkspaceRootKind::WindowsSystemRoot => {
                signals.push(RootSafetySignal::blocking(format!(
                    "workspace root is a protected Windows system directory ({})",
                    self.normalized
                )))
            }
            WorkspaceRootKind::WindowsProfileRoot => {
                signals.push(RootSafetySignal::blocking(format!(
                    "workspace root is a broad Windows profile container ({})",
                    self.normalized
                )))
            }
            WorkspaceRootKind::WindowsCloudRoot => {
                signals.push(RootSafetySignal::blocking(format!(
                    "workspace root is a cloud-synced Windows folder ({})",
                    self.normalized
                )))
            }
            WorkspaceRootKind::WindowsUncShareRoot => {
                signals.push(RootSafetySignal::blocking(format!(
                    "workspace root is a Windows UNC share root ({})",
                    self.normalized
                )))
            }
            WorkspaceRootKind::WindowsVerbatimRoot => {
                signals.push(RootSafetySignal::blocking(format!(
                    "workspace root is a Windows verbatim root ({})",
                    self.normalized
                )))
            }
            WorkspaceRootKind::UnixProtectedRoot | WorkspaceRootKind::UnixRoot => {
                signals.push(RootSafetySignal::blocking(format!(
                    "workspace root is a protected system directory ({})",
                    self.normalized
                )));
            }
            WorkspaceRootKind::Other => {}
        }
        signals
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceRootKind {
    Other,
    UnixRoot,
    UnixProtectedRoot,
    WindowsDriveRoot,
    WindowsSystemRoot,
    WindowsProfileRoot,
    WindowsCloudRoot,
    WindowsUncShareRoot,
    WindowsVerbatimRoot,
}

impl WorkspaceRootKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Other => "other",
            Self::UnixRoot => "unix_root",
            Self::UnixProtectedRoot => "unix_protected_root",
            Self::WindowsDriveRoot => "windows_drive_root",
            Self::WindowsSystemRoot => "windows_system_root",
            Self::WindowsProfileRoot => "windows_profile_root",
            Self::WindowsCloudRoot => "windows_cloud_root",
            Self::WindowsUncShareRoot => "windows_unc_share_root",
            Self::WindowsVerbatimRoot => "windows_verbatim_root",
        }
    }

    fn from_normalized(path: &str) -> Self {
        if path == "/" {
            return Self::UnixRoot;
        }
        if matches!(
            path,
            "/Applications"
                | "/Library"
                | "/Network"
                | "/System"
                | "/Users"
                | "/Volumes"
                | "/bin"
                | "/dev"
                | "/etc"
                | "/opt"
                | "/private"
                | "/sbin"
                | "/usr"
                | "/var"
        ) {
            return Self::UnixProtectedRoot;
        }
        let lowered = path.to_ascii_lowercase();
        if is_windows_verbatim_root(&lowered) {
            return Self::WindowsVerbatimRoot;
        }
        if is_windows_unc_share_root(&lowered) {
            return Self::WindowsUncShareRoot;
        }
        if is_windows_drive_root(&lowered) {
            return Self::WindowsDriveRoot;
        }
        if is_windows_system_root(&lowered) {
            return Self::WindowsSystemRoot;
        }
        if is_windows_profile_container(&lowered) {
            return Self::WindowsProfileRoot;
        }
        if looks_windows_path(path)
            && path
                .rsplit('/')
                .find(|part| !part.is_empty())
                .map(is_cloud_sync_root_name)
                .unwrap_or(false)
        {
            return Self::WindowsCloudRoot;
        }
        Self::Other
    }
}

#[derive(Debug, Clone)]
struct RootSafetySignal {
    message: String,
    blocking: bool,
}

impl RootSafetySignal {
    fn blocking(message: String) -> Self {
        Self {
            message,
            blocking: true,
        }
    }
}

pub fn normalized_filesystem_path(path: &Path) -> String {
    normalize_filesystem_path_text(&path.to_string_lossy())
}

pub fn filesystem_path_key(path: &Path) -> String {
    let normalized = normalized_filesystem_path(path);
    if looks_windows_path(&normalized) {
        normalized.to_ascii_lowercase()
    } else {
        normalized
    }
}

pub fn filesystem_paths_match(left: &Path, right: &Path) -> bool {
    left == right
        || fs::canonicalize(left)
            .ok()
            .zip(fs::canonicalize(right).ok())
            .map(|(left, right)| left == right)
            .unwrap_or(false)
        || filesystem_path_key(left) == filesystem_path_key(right)
}

fn normalize_filesystem_path_text(path: &str) -> String {
    let mut text = path.replace('\\', "/");
    if let Some(rest) = strip_ascii_prefix(&text, "//?/UNC/") {
        text = format!("//{rest}");
    } else if let Some(rest) = strip_ascii_prefix(&text, "//?/") {
        text = rest.to_string();
    } else if let Some(rest) = strip_ascii_prefix(&text, "//./") {
        text = rest.to_string();
    }

    if text.len() >= 2 && text.as_bytes()[1] == b':' && text.as_bytes()[0].is_ascii_alphabetic() {
        let drive = text[..1].to_ascii_lowercase();
        text.replace_range(0..1, &drive);
    }

    while text.contains("//") && !text.starts_with("//") {
        text = text.replace("//", "/");
    }
    while text.len() > 1 && text.ends_with('/') && !is_windows_drive_root(&text) {
        if is_windows_unc_share_root(&text) {
            text = text.trim_end_matches('/').to_string();
            break;
        }
        text.pop();
    }
    text
}

fn strip_ascii_prefix<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .and_then(|_| text.get(prefix.len()..))
}

fn looks_windows_path(path: &str) -> bool {
    is_windows_drive_root(path)
        || (path.len() >= 3
            && path.as_bytes()[1] == b':'
            && path.as_bytes()[0].is_ascii_alphabetic()
            && path.as_bytes()[2] == b'/')
        || path.starts_with("//")
}

fn is_windows_drive_root(path: &str) -> bool {
    path.len() == 3
        && path.as_bytes()[1] == b':'
        && path.as_bytes()[0].is_ascii_alphabetic()
        && path.as_bytes()[2] == b'/'
}

fn is_windows_verbatim_root(path: &str) -> bool {
    path == "//?/" || path == "//?/unc"
}

fn is_windows_unc_share_root(path: &str) -> bool {
    if !path.starts_with("//") {
        return false;
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed != path {
        return is_windows_unc_share_root(trimmed);
    }
    let parts = path
        .trim_start_matches('/')
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    parts.len() == 2 && !path[2..].contains("//")
}

fn is_windows_system_root(path: &str) -> bool {
    let rest = drive_rest(path);
    matches!(
        rest,
        Some("windows") | Some("program files") | Some("program files (x86)") | Some("programdata")
    )
}

fn is_windows_profile_container(path: &str) -> bool {
    matches!(
        drive_rest(path),
        Some("users") | Some("documents and settings")
    )
}

fn drive_rest(path: &str) -> Option<&str> {
    if path.len() < 4 || path.as_bytes()[1] != b':' || path.as_bytes()[2] != b'/' {
        return None;
    }
    let rest = path[3..].trim_matches('/');
    (!rest.contains('/')).then_some(rest)
}

fn is_cloud_sync_root_name(name: &str) -> bool {
    let lowered = name.to_ascii_lowercase();
    lowered == "onedrive"
        || lowered.starts_with("onedrive - ")
        || lowered == "dropbox"
        || lowered == "google drive"
        || lowered == "icloud drive"
}

fn home_dirs_from_env() -> Vec<PathBuf> {
    let mut homes = Vec::new();
    push_home_candidate(&mut homes, env::var_os("HOME").map(PathBuf::from));
    push_home_candidate(&mut homes, env::var_os("USERPROFILE").map(PathBuf::from));
    let drive = env::var_os("HOMEDRIVE");
    let path = env::var_os("HOMEPATH");
    if let (Some(drive), Some(path)) = (drive, path) {
        let mut combined = OsString::new();
        combined.push(drive);
        combined.push(path);
        push_home_candidate(&mut homes, Some(PathBuf::from(combined)));
    }
    homes.sort_by_key(|path| filesystem_path_key(path));
    homes.dedup_by(|left, right| filesystem_paths_match(left, right));
    homes
}

fn push_home_candidate(homes: &mut Vec<PathBuf>, candidate: Option<PathBuf>) {
    let Some(candidate) = candidate.filter(|path| !path.as_os_str().is_empty()) else {
        return;
    };
    homes.push(fs::canonicalize(&candidate).unwrap_or(candidate));
}

fn vcs_marker_at(path: &Path) -> Option<&'static str> {
    [".git", ".jj", ".hg", ".svn"]
        .into_iter()
        .find(|marker| path.join(marker).exists())
}

#[derive(Debug, Default)]
struct WorkspaceSignalScan {
    has_readme: bool,
    project_markers: Vec<String>,
    shallow_source_markers: Vec<String>,
    code_directory_markers: Vec<String>,
}

fn scan_workspace_signals(root: &Path) -> WorkspaceSignalScan {
    let Some(root_entries) = read_dir_entries(root) else {
        return WorkspaceSignalScan {
            project_markers: project_markers_from_root(root, None),
            code_directory_markers: code_directory_markers(root, &BTreeSet::new()),
            ..WorkspaceSignalScan::default()
        };
    };
    let root_entry_names = root_entry_names(&root_entries);
    let has_readme = root_entries.iter().any(is_readme_entry);
    let project_markers = project_markers_from_root(root, Some((&root_entry_names, &root_entries)));

    let mut source_scan = SourceMarkerScan::default();
    collect_source_markers_from_entries(&root_entries, 0, None, &mut source_scan);
    source_scan.signals.sort();
    source_scan.signals.dedup();

    let code_directory_markers = code_directory_markers(root, &source_scan.direct_code_dirs);
    WorkspaceSignalScan {
        has_readme,
        project_markers,
        shallow_source_markers: source_scan.signals,
        code_directory_markers,
    }
}

fn read_dir_entries(root: &Path) -> Option<Vec<fs::DirEntry>> {
    Some(
        fs::read_dir(root)
            .ok()?
            .filter_map(|entry| entry.ok())
            .collect(),
    )
}

fn root_entry_names(entries: &[fs::DirEntry]) -> BTreeSet<OsString> {
    entries.iter().map(fs::DirEntry::file_name).collect()
}

fn is_readme_entry(entry: &fs::DirEntry) -> bool {
    entry
        .file_name()
        .to_str()
        .map(|name| name.eq_ignore_ascii_case("readme.md") || name.eq_ignore_ascii_case("readme"))
        .unwrap_or(false)
}

fn project_markers_from_root(
    root: &Path,
    root_scan: Option<(&BTreeSet<OsString>, &[fs::DirEntry])>,
) -> Vec<String> {
    CODE_PROJECT_MARKERS
        .iter()
        .copied()
        .filter(|marker| project_marker_exists(root, marker, root_scan.map(|(names, _)| names)))
        .map(|marker| format!("project marker {marker}"))
        .chain(dotnet_project_markers(
            root,
            root_scan.map(|(_, entries)| entries),
        ))
        .collect()
}

fn project_marker_exists(
    root: &Path,
    marker: &str,
    root_entry_names: Option<&BTreeSet<OsString>>,
) -> bool {
    if marker.contains('/') {
        return root.join(marker).exists();
    }
    // The read_dir listing is a fast positive shortcut: an exact-case hit
    // proves existence without a stat. On a miss we must still defer to
    // `exists()`, which is authoritative and matches the filesystem's own
    // case semantics (e.g. `cargo.toml` satisfying `Cargo.toml` on
    // case-insensitive volumes). Using `&&` here would drop such markers.
    root_entry_names
        .map(|names| names.contains(std::ffi::OsStr::new(marker)) || root.join(marker).exists())
        .unwrap_or_else(|| root.join(marker).exists())
}

fn dotnet_project_markers(root: &Path, entries: Option<&[fs::DirEntry]>) -> Vec<String> {
    if let Some(entries) = entries {
        return entries
            .iter()
            .filter_map(dotnet_project_marker_from_entry)
            .collect();
    }
    read_dir_entries(root)
        .unwrap_or_default()
        .iter()
        .filter_map(dotnet_project_marker_from_entry)
        .collect()
}

fn dotnet_project_marker_from_entry(entry: &fs::DirEntry) -> Option<String> {
    let file_name = entry.file_name();
    let path = Path::new(&file_name);
    let name = path.file_name()?.to_str()?.to_string();
    let extension = path.extension()?.to_str()?;
    matches!(extension, "csproj" | "sln" | "slnx").then(|| format!("project marker {name}"))
}

#[derive(Debug, Default)]
struct SourceMarkerScan {
    visited: usize,
    signals: Vec<String>,
    direct_code_dirs: BTreeSet<String>,
}

fn collect_source_markers(
    dir: &Path,
    depth: usize,
    direct_code_dir: Option<&str>,
    scan: &mut SourceMarkerScan,
) {
    if depth > SOURCE_SCAN_MAX_DEPTH || scan.visited >= SOURCE_SCAN_MAX_ENTRIES {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        collect_source_marker_entry(&entry.path(), depth, direct_code_dir, scan);
    }
}

fn collect_source_markers_from_entries(
    entries: &[fs::DirEntry],
    depth: usize,
    direct_code_dir: Option<&str>,
    scan: &mut SourceMarkerScan,
) {
    if depth > SOURCE_SCAN_MAX_DEPTH || scan.visited >= SOURCE_SCAN_MAX_ENTRIES {
        return;
    }
    for entry in entries {
        collect_source_marker_entry(&entry.path(), depth, direct_code_dir, scan);
    }
}

fn collect_source_marker_entry(
    path: &Path,
    depth: usize,
    direct_code_dir: Option<&str>,
    scan: &mut SourceMarkerScan,
) {
    if scan.visited >= SOURCE_SCAN_MAX_ENTRIES {
        return;
    }
    scan.visited += 1;
    if path.is_dir() {
        if depth < SOURCE_SCAN_MAX_DEPTH && should_scan_source_dir(path) {
            let child_code_dir = if depth == 0 {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .filter(|name| is_code_directory_name(name))
            } else {
                None
            };
            collect_source_markers(path, depth + 1, child_code_dir, scan);
        }
        return;
    }
    if let Some(signal) = source_marker_signal(path) {
        if let Some(name) = direct_code_dir {
            scan.direct_code_dirs.insert(name.to_string());
        }
        scan.signals.push(signal);
    }
}

fn code_directory_markers(root: &Path, direct_code_dirs: &BTreeSet<String>) -> Vec<String> {
    CODE_DIRECTORY_MARKERS
        .iter()
        .copied()
        .filter(|name| {
            let path = root.join(name);
            path.is_dir() && (direct_code_dirs.contains(*name) || directory_contains_code(&path))
        })
        .map(|name| format!("code directory {name} contains source"))
        .collect()
}

fn directory_contains_code(dir: &Path) -> bool {
    let mut scan = SourceMarkerScan::default();
    collect_source_markers(dir, SOURCE_SCAN_MAX_DEPTH, None, &mut scan);
    !scan.signals.is_empty()
}

fn is_code_directory_name(name: &str) -> bool {
    CODE_DIRECTORY_MARKERS.contains(&name)
}

fn should_scan_source_dir(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return true;
    };
    !matches!(
        name,
        ".git"
            | ".hg"
            | ".jj"
            | ".svn"
            | "__pycache__"
            | "node_modules"
            | "target"
            | "vendor"
            | "dist"
            | "build"
            | "out"
            | ".venv"
            | "venv"
            | ".gradle"
    )
}

fn source_marker_signal(path: &Path) -> Option<String> {
    match classify_language(path) {
        LanguageKind::C => Some("shallow C source".to_string()),
        LanguageKind::Cpp => Some("shallow C/C++ source".to_string()),
        LanguageKind::CSharp => Some("shallow C# source".to_string()),
        LanguageKind::Go => Some("shallow Go source".to_string()),
        LanguageKind::Java => Some("shallow Java source".to_string()),
        LanguageKind::JavaScript | LanguageKind::Jsx => {
            Some("shallow JavaScript source".to_string())
        }
        LanguageKind::Rust => Some("shallow Rust source".to_string()),
        LanguageKind::Python => Some("shallow Python source".to_string()),
        LanguageKind::TypeScript | LanguageKind::Tsx => {
            Some("shallow TypeScript source".to_string())
        }
        _ => code_extension_signal(path),
    }
}

fn code_extension_signal(path: &Path) -> Option<String> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    let language = match LanguageKind::from_extension(&extension) {
        LanguageKind::Unsupported => match extension.as_str() {
            "css" | "html" | "scss" | "vue" | "svelte" => "web",
            "kt" | "kts" => "Kotlin",
            "php" => "PHP",
            "rb" => "Ruby",
            "scala" | "sc" => "Scala",
            "sh" | "bash" | "zsh" => "shell",
            "swift" => "Swift",
            _ => return None,
        },
        kind => kind
            .family()
            .map(|family| match family {
                squeezy_core::LanguageFamily::CFamily => "C/C++",
                squeezy_core::LanguageFamily::JsTs => kind.display_name(),
                _ => kind.display_name(),
            })
            .unwrap_or_else(|| kind.display_name()),
    };
    Some(format!("shallow {language} source"))
}

fn relative_path(root: &Path, path: &Path) -> Result<String> {
    let relative = path.strip_prefix(root).map_err(|err| {
        SqueezyError::Workspace(format!(
            "{} is outside {}: {err}",
            path.display(),
            root.display()
        ))
    })?;
    let relative = relative.to_string_lossy();
    if relative.contains('\\') {
        Ok(relative.replace('\\', "/"))
    } else {
        Ok(relative.into_owned())
    }
}

fn unsupported_file(
    path: &Path,
    relative_path: String,
    extension: Option<String>,
    size_bytes: u64,
    reason: UnsupportedReason,
) -> UnsupportedFile {
    UnsupportedFile {
        path: path.to_path_buf(),
        relative_path,
        extension,
        size_bytes,
        reason,
        suggested_fallback: "bounded read/grep/list navigation".to_string(),
    }
}

fn detect_path_conflicts(files: &[FileRecord]) -> Vec<PathConflict> {
    let mut by_identity = BTreeMap::<String, Vec<String>>::new();
    for file in files {
        by_identity
            .entry(normalize_path(&file.relative_path, false).to_ascii_lowercase())
            .or_default()
            .push(file.relative_path.clone());
    }
    by_identity
        .into_iter()
        .filter_map(|(normalized_relative_path, mut relative_paths)| {
            relative_paths.sort();
            relative_paths.dedup();
            (relative_paths.len() > 1).then_some(PathConflict {
                normalized_relative_path,
                relative_paths,
            })
        })
        .collect()
}

fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(1024).any(|byte| *byte == 0)
}

fn looks_generated(bytes: &[u8]) -> bool {
    let prefix = &bytes[..bytes.len().min(BINARY_GENERATED_PREFIX_BYTES)];
    let text = String::from_utf8_lossy(prefix).to_ascii_lowercase();
    [
        "@generated",
        "auto-generated",
        "automatically generated",
        "code generated",
        "do not edit",
    ]
    .into_iter()
    .any(|marker| text.contains(marker))
}

fn build_glob_set(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern).map_err(|err| {
            SqueezyError::Config(format!("invalid graph glob {pattern:?}: {err}"))
        })?);
    }
    builder
        .build()
        .map_err(|err| SqueezyError::Config(format!("invalid graph glob set: {err}")))
}

fn normalize_reason_class(class: &str) -> String {
    class.trim().replace('-', "_").to_ascii_lowercase()
}

fn normalize_path(relative_path: &str, is_dir: bool) -> String {
    let mut start = 0;
    while let Some(rest) = relative_path.get(start..) {
        let bytes = rest.as_bytes();
        if bytes.len() >= 2 && bytes[0] == b'.' && matches!(bytes[1], b'/' | b'\\') {
            start += 2;
        } else {
            break;
        }
    }
    let normalized = &relative_path[start..];
    let has_backslash = normalized.as_bytes().contains(&b'\\');
    let needs_trailing_slash = is_dir && !normalized.ends_with('/') && !normalized.ends_with('\\');
    if !has_backslash && !needs_trailing_slash {
        return normalized.to_string();
    }
    let mut output = String::with_capacity(normalized.len() + usize::from(needs_trailing_slash));
    for ch in normalized.chars() {
        output.push(if ch == '\\' { '/' } else { ch });
    }
    if needs_trailing_slash {
        output.push('/');
    }
    output
}

fn default_path_reason(relative_path: &str, is_dir: bool) -> Option<ExclusionReason> {
    let path = relative_path.trim_end_matches('/');
    let parts = path.split('/').filter(|part| !part.is_empty());
    for part in parts {
        match part {
            ".git" | ".hg" | ".jj" | ".svn" => return Some(ExclusionReason::VcsMetadata),
            "vendor" => return Some(ExclusionReason::Vendor),
            "node_modules" | "bower_components" | ".pnpm-store" | ".npm" | ".venv" | "venv"
            | "__pycache__" | ".pytest_cache" | ".mypy_cache" | ".tox" | ".nox" | ".gradle" => {
                return Some(ExclusionReason::DependencyCache);
            }
            "target" | ".squeezy" | "dist" | "build" | "out" | "bin" | "obj" | ".next"
            | ".turbo" | ".output" | ".cache" | "coverage" | ".nyc_output" => {
                return Some(ExclusionReason::BuildOutput);
            }
            _ => {}
        }
    }
    if is_dir {
        return None;
    }
    let name = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
        .to_ascii_lowercase();
    if is_lockfile_name(&name) {
        return Some(ExclusionReason::Lockfile);
    }
    if name.ends_with(".generated.rs")
        || name.ends_with(".pb.go")
        || name.ends_with(".generated.swift")
        || name.ends_with(".g.dart")
        || name.ends_with(".freezed.dart")
        || name.ends_with(".gr.dart")
    {
        return Some(ExclusionReason::Generated);
    }
    if is_binary_extension(&name) {
        return Some(ExclusionReason::Binary);
    }
    None
}

fn is_lockfile_name(name: &str) -> bool {
    matches!(
        name,
        "cargo.lock"
            | "package-lock.json"
            | "pnpm-lock.yaml"
            | "yarn.lock"
            | "bun.lock"
            | "poetry.lock"
            | "uv.lock"
            | "pipfile.lock"
            | "gemfile.lock"
            | "composer.lock"
            | "go.sum"
    )
}

fn is_binary_extension(name: &str) -> bool {
    let Some((_, extension)) = name.rsplit_once('.') else {
        return false;
    };
    matches!(
        extension,
        "a" | "apk"
            | "app"
            | "bin"
            | "bmp"
            | "bz2"
            | "class"
            | "dll"
            | "dmg"
            | "doc"
            | "docx"
            | "dylib"
            | "ear"
            | "exe"
            | "flac"
            | "gif"
            | "gz"
            | "ico"
            | "iso"
            | "jar"
            | "jpeg"
            | "jpg"
            | "lib"
            | "m4a"
            | "mov"
            | "mp3"
            | "mp4"
            | "o"
            | "obj"
            | "pdf"
            | "png"
            | "ppt"
            | "pptx"
            | "rar"
            | "so"
            | "tar"
            | "ttf"
            | "wasm"
            | "webm"
            | "webp"
            | "woff"
            | "woff2"
            | "xls"
            | "xlsx"
            | "xz"
            | "zip"
    )
}

fn record_excluded_file(
    excluded: &mut Vec<ExcludedPath>,
    coverage: &mut IndexCoverage,
    path: &Path,
    relative_path: String,
    size_bytes: u64,
    reason: ExclusionReason,
) {
    coverage.skipped_files += 1;
    coverage.skipped_bytes += size_bytes;
    let entry = coverage
        .reasons
        .entry(reason.as_str().to_string())
        .or_default();
    entry.files += 1;
    entry.bytes += size_bytes;
    if entry.samples.len() < 5 {
        entry.samples.push(relative_path.clone());
    }
    excluded.push(ExcludedPath {
        path: path.to_path_buf(),
        relative_path,
        size_bytes,
        reason,
        is_dir: false,
    });
}

fn record_excluded_dir_entry(coverage: &mut IndexCoverage, entry: &ExcludedPath) {
    coverage.skipped_dirs += 1;
    let bucket = coverage
        .reasons
        .entry(entry.reason.as_str().to_string())
        .or_default();
    bucket.dirs += 1;
    if bucket.samples.len() < 5 {
        bucket.samples.push(entry.relative_path.clone());
    }
}

pub fn stable_content_hash(bytes: &[u8]) -> String {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x00000100000001b3;

    let mut hash = FNV_OFFSET;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
