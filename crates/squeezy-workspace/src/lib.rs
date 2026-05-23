use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::UNIX_EPOCH,
};

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use squeezy_core::{ContentHash, FileId, Freshness, LanguageKind, Result, SqueezyError};

pub const CRATE_NAME: &str = "squeezy-workspace";
const SOURCE_SCAN_MAX_DEPTH: usize = 2;
const SOURCE_SCAN_MAX_ENTRIES: usize = 1_000;
const BINARY_GENERATED_PREFIX_BYTES: usize = 4096;

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
            max_file_bytes: 1_000_000,
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

#[derive(Debug, Clone, PartialEq, Eq)]
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
    pub coverage: IndexCoverage,
    pub walk_errors: Vec<String>,
    pub indexing_decision: IndexingDecision,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexingDecision {
    pub should_index: bool,
    pub reason: String,
    pub positive_signals: Vec<String>,
    pub negative_signals: Vec<String>,
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
            let extension = path
                .extension()
                .map(|ext| ext.to_string_lossy().to_string());
            let language = classify_language(&path);

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
            if size_bytes > self.options.max_file_bytes
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
                        extension,
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
                    extension,
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

        Ok(WorkspaceSnapshot {
            root,
            files,
            unsupported,
            excluded,
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
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("cs") | Some("csx") => LanguageKind::CSharp,
        Some("go") => LanguageKind::Go,
        Some("py") => LanguageKind::Python,
        Some("rs") => LanguageKind::Rust,
        Some(_) => LanguageKind::Unsupported,
        None => LanguageKind::Unknown,
    }
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

    if is_home_dir(root) {
        negative_signals.push("workspace root is the user's home directory".to_string());
    }
    if is_protected_root(root) {
        negative_signals.push("workspace root is a protected system directory".to_string());
    }

    let mut has_strong_positive = false;

    if let Some(marker) = vcs_marker_signal(root) {
        has_strong_positive = true;
        positive_signals.push(marker);
    }
    if has_readme(root) {
        positive_signals.push("README at workspace root".to_string());
    }
    for marker in code_project_markers(root) {
        has_strong_positive = true;
        positive_signals.push(marker);
    }
    for source in shallow_source_markers(root) {
        has_strong_positive = true;
        positive_signals.push(source);
    }
    for code_dir in code_directory_markers(root) {
        has_strong_positive = true;
        positive_signals.push(code_dir);
    }
    if is_personal_folder(root) {
        negative_signals.push("workspace root looks like a personal folder".to_string());
    }

    let blocked_by_root = negative_signals
        .iter()
        .any(|signal| signal.contains("home directory") || signal.contains("protected system"));
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

fn is_home_dir(root: &Path) -> bool {
    env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|home| fs::canonicalize(home).ok())
        .map(|home| home == root)
        .unwrap_or(false)
}

fn is_protected_root(root: &Path) -> bool {
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
        "desktop" | "documents" | "downloads"
    )
}

fn vcs_marker_signal(root: &Path) -> Option<String> {
    for (index, ancestor) in root.ancestors().enumerate() {
        if index > 0 && (is_home_dir(ancestor) || is_protected_root(ancestor)) {
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

fn vcs_marker_at(path: &Path) -> Option<&'static str> {
    [".git", ".jj", ".hg", ".svn"]
        .into_iter()
        .find(|marker| path.join(marker).exists())
}

fn has_readme(root: &Path) -> bool {
    fs::read_dir(root)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .any(|entry| {
            entry
                .file_name()
                .to_str()
                .map(|name| {
                    name.eq_ignore_ascii_case("readme.md") || name.eq_ignore_ascii_case("readme")
                })
                .unwrap_or(false)
        })
}

fn code_project_markers(root: &Path) -> Vec<String> {
    [
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
    ]
    .into_iter()
    .filter(|marker| root.join(marker).exists())
    .map(|marker| format!("project marker {marker}"))
    .chain(dotnet_project_markers(root))
    .collect()
}

fn dotnet_project_markers(root: &Path) -> Vec<String> {
    fs::read_dir(root)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?.to_string();
            let extension = path.extension()?.to_str()?;
            matches!(extension, "csproj" | "sln" | "slnx").then(|| format!("project marker {name}"))
        })
        .collect()
}

fn shallow_source_markers(root: &Path) -> Vec<String> {
    let mut signals = Vec::new();
    let mut visited = 0;
    collect_source_markers(root, 0, &mut visited, &mut signals);
    signals.sort();
    signals.dedup();
    signals
}

fn collect_source_markers(
    dir: &Path,
    depth: usize,
    visited: &mut usize,
    signals: &mut Vec<String>,
) {
    if depth > SOURCE_SCAN_MAX_DEPTH || *visited >= SOURCE_SCAN_MAX_ENTRIES {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if *visited >= SOURCE_SCAN_MAX_ENTRIES {
            return;
        }
        *visited += 1;
        let path = entry.path();
        if path.is_dir() {
            if depth < SOURCE_SCAN_MAX_DEPTH && should_scan_source_dir(&path) {
                collect_source_markers(&path, depth + 1, visited, signals);
            }
            continue;
        }
        match classify_language(&path) {
            LanguageKind::CSharp => signals.push("shallow C# source".to_string()),
            LanguageKind::Go => signals.push("shallow Go source".to_string()),
            LanguageKind::Rust => signals.push("shallow Rust source".to_string()),
            LanguageKind::Python => signals.push("shallow Python source".to_string()),
            _ => {
                if let Some(label) = code_extension_signal(&path) {
                    signals.push(label);
                }
            }
        }
    }
}

fn code_directory_markers(root: &Path) -> Vec<String> {
    [
        "app", "cmd", "crates", "include", "internal", "lib", "packages", "pkg", "src",
    ]
    .into_iter()
    .filter(|name| {
        let path = root.join(name);
        path.is_dir() && directory_contains_code(&path)
    })
    .map(|name| format!("code directory {name} contains source"))
    .collect()
}

fn directory_contains_code(dir: &Path) -> bool {
    let mut signals = Vec::new();
    let mut visited = 0;
    collect_source_markers(dir, SOURCE_SCAN_MAX_DEPTH, &mut visited, &mut signals);
    !signals.is_empty()
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

fn code_extension_signal(path: &Path) -> Option<String> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    let language = match extension.as_str() {
        "c" | "cc" | "cpp" | "cxx" | "h" | "hh" | "hpp" | "hxx" => "C/C++",
        "cs" | "csx" => "C#",
        "css" | "html" | "scss" | "vue" | "svelte" => "web",
        "go" => "Go",
        "java" => "Java",
        "js" | "jsx" | "mjs" | "cjs" => "JavaScript",
        "kt" | "kts" => "Kotlin",
        "php" => "PHP",
        "rb" => "Ruby",
        "scala" | "sc" => "Scala",
        "sh" | "bash" | "zsh" => "shell",
        "swift" => "Swift",
        "ts" | "tsx" | "mts" | "cts" => "TypeScript",
        _ => return None,
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
    Ok(relative.to_string_lossy().replace('\\', "/"))
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
    let mut normalized = relative_path.replace('\\', "/");
    while normalized.starts_with("./") {
        normalized = normalized[2..].to_string();
    }
    if is_dir && !normalized.ends_with('/') {
        normalized.push('/');
    }
    normalized
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
            "target" | "dist" | "build" | "out" | "bin" | "obj" | ".next" | ".turbo"
            | ".output" | ".cache" | "coverage" | ".nyc_output" => {
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
    if name.ends_with(".generated.rs") || name.ends_with(".pb.go") {
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
