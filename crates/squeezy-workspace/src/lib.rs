use std::{
    fs,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use ignore::WalkBuilder;
use squeezy_core::{ContentHash, FileId, Freshness, LanguageKind, Result, SqueezyError};

pub const CRATE_NAME: &str = "squeezy-workspace";

pub fn crate_name() -> &'static str {
    CRATE_NAME
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrawlOptions {
    pub include_hidden: bool,
    pub max_file_bytes: u64,
}

impl Default for CrawlOptions {
    fn default() -> Self {
        Self {
            include_hidden: false,
            max_file_bytes: 1_000_000,
        }
    }
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
pub struct WorkspaceSnapshot {
    pub root: PathBuf,
    pub files: Vec<FileRecord>,
    pub unsupported: Vec<UnsupportedFile>,
    pub walk_errors: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct WorkspaceCrawler {
    options: CrawlOptions,
}

impl WorkspaceCrawler {
    pub fn new(options: CrawlOptions) -> Self {
        Self { options }
    }

    pub fn crawl(&self, root: impl AsRef<Path>) -> Result<WorkspaceSnapshot> {
        let root = fs::canonicalize(root.as_ref())?;
        let mut walker = WalkBuilder::new(&root);
        walker
            .hidden(!self.options.include_hidden)
            .git_ignore(true)
            .git_exclude(true)
            .parents(true)
            .require_git(false);

        let mut files = Vec::new();
        let mut unsupported = Vec::new();
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
            if !file_type.is_file() {
                continue;
            }

            let path = entry.into_path();
            let metadata = fs::metadata(&path)?;
            let size_bytes = metadata.len();
            let relative_path = relative_path(&root, &path)?;
            let extension = path
                .extension()
                .map(|ext| ext.to_string_lossy().to_string());
            let language = classify_language(&path);

            if size_bytes > self.options.max_file_bytes {
                unsupported.push(unsupported_file(
                    &path,
                    relative_path,
                    extension,
                    size_bytes,
                    UnsupportedReason::TooLarge,
                ));
                continue;
            }

            let bytes = fs::read(&path)?;
            if looks_binary(&bytes) {
                unsupported.push(unsupported_file(
                    &path,
                    relative_path,
                    extension,
                    size_bytes,
                    UnsupportedReason::BinaryLike,
                ));
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

        files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        unsupported.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));

        Ok(WorkspaceSnapshot {
            root,
            files,
            unsupported,
            walk_errors,
        })
    }
}

pub fn classify_language(path: &Path) -> LanguageKind {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("rs") => LanguageKind::Rust,
        Some(_) => LanguageKind::Unsupported,
        None => LanguageKind::Unknown,
    }
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
