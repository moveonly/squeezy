//! `@<path>` file mention popup for the composer.
//!
//! Detects an `@`-prefixed token at the cursor and lists matching
//! workspace files ranked by a simple subsequence/prefix scorer. The
//! popup shows up to `MAX_MATCHES` (10) entries plus an `(idx/total)`
//! footer that reflects the pre-truncation candidate count, and
//! dismisses on Esc or when the cursor leaves the `@<word>` token.

#![allow(dead_code)]

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

const MAX_MATCHES: usize = 10;
const MAX_WORKSPACE_FILES: usize = 5000;

/// Floor between background rebuilds of the workspace file list when the
/// `.git/index` mtime has not changed. The floor still picks up untracked
/// files, which don't bump the index. 5s keeps suggestion updates
/// responsive without re-walking the workspace on every keystroke.
pub(crate) const WORKSPACE_REFRESH_THROTTLE: Duration = Duration::from_secs(5);

/// The state of an in-flight `@`-mention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MentionQuery {
    /// Byte offset of the `@` in the input string.
    pub start: usize,
    /// Byte offset just past the current word.
    pub end: usize,
    /// The token after `@` (may be empty just after typing `@`).
    pub query: String,
}

/// Returns `Some(MentionQuery)` if the cursor sits inside an `@<word>`
/// token preceded by start-of-input or whitespace. `None` otherwise.
///
/// The token is normally read up to the next whitespace, but `@"..."`
/// and `@'...'` quoted forms keep their inner spaces and strip the
/// surrounding quotes from the returned `query`. A mismatched quote (no
/// matching close before EOF) falls back to the unquoted form so the
/// leading `"`/`'` is treated as a literal character, preserving the
/// original detector's behaviour for malformed input.
pub(crate) fn detect_mention(input: &str, cursor: usize) -> Option<MentionQuery> {
    let bytes = input.as_bytes();
    let cursor = cursor.min(input.len());
    let mut i = 0;
    while i < bytes.len() {
        // Skip until we find an `@` preceded by start-of-input or whitespace.
        if bytes[i] != b'@' || (i > 0 && !bytes[i - 1].is_ascii_whitespace()) {
            i += 1;
            continue;
        }
        let start = i;
        let after_at = i + 1;
        let (end, query) = parse_mention_token(input, bytes, after_at);
        if cursor > start && cursor <= end {
            return Some(MentionQuery { start, end, query });
        }
        i = end + 1;
    }
    None
}

/// Parse the token immediately after `@`. Returns the end byte index
/// (one past the last byte of the token) and the query string with any
/// surrounding `"`/`'` quotes stripped.
///
/// `@"..."` and `@'...'` are honoured as quoted spans that may contain
/// whitespace. If the opening quote has no matching close before EOF the
/// parse falls back to the unquoted form, so the leading quote ends up
/// inside the returned query and the token terminates at the next
/// whitespace — matching what the original detector produced for that
/// input.
fn parse_mention_token(input: &str, bytes: &[u8], after_at: usize) -> (usize, String) {
    if after_at < bytes.len() && (bytes[after_at] == b'"' || bytes[after_at] == b'\'') {
        let quote = bytes[after_at];
        let content_start = after_at + 1;
        if let Some(rel) = bytes[content_start..].iter().position(|&b| b == quote) {
            let close = content_start + rel;
            return (close + 1, input[content_start..close].to_string());
        }
    }
    let mut end = after_at;
    while end < bytes.len() && !bytes[end].is_ascii_whitespace() {
        end += 1;
    }
    (end, input[after_at..end].to_string())
}

/// Walk the workspace rooted at `root` (respecting .gitignore via the
/// `ignore` crate) and return up to `MAX_WORKSPACE_FILES` file paths
/// relative to `root`. Used as the candidate set for the popup.
///
/// The second return value is `true` when the walk hit the
/// `MAX_WORKSPACE_FILES` cap and stopped early, so callers can warn the
/// user that the candidate set is incomplete rather than silently
/// dropping the remaining files.
pub(crate) fn load_workspace_files(root: &Path) -> (Vec<PathBuf>, bool) {
    let mut files = Vec::new();
    let mut truncated = false;
    let walker = ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(false)
        .build();
    for entry in walker {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let rel = path.strip_prefix(root).unwrap_or(path).to_path_buf();
        files.push(rel);
        if files.len() >= MAX_WORKSPACE_FILES {
            truncated = true;
            break;
        }
    }
    (files, truncated)
}

/// Cached workspace file list with an `.git/index` mtime poll and a 5 s
/// refresh floor for untracked-file additions.
///
/// Without a cache the popup re-walked the workspace on every keypress;
/// with a permanent cache it never picked up files created after TUI
/// startup. The poll keeps the cache fresh after git operations
/// (checkout/commit/rm bump `.git/index`) and the floor catches new
/// untracked files.
#[derive(Debug, Clone)]
pub(crate) struct WorkspaceFileCache {
    files: Arc<Vec<PathBuf>>,
    truncated: bool,
    built_at: Instant,
    git_index_mtime: Option<SystemTime>,
}

impl WorkspaceFileCache {
    /// Walk `root` once and snapshot the `.git/index` mtime.
    pub(crate) fn build(root: &Path) -> Self {
        let (files, truncated) = load_workspace_files(root);
        let git_index_mtime = git_index_mtime(root);
        Self {
            files: Arc::new(files),
            truncated,
            built_at: Instant::now(),
            git_index_mtime,
        }
    }

    /// Shared handle to the cached file list. Cheap to clone via `Arc`.
    pub(crate) fn files(&self) -> &Arc<Vec<PathBuf>> {
        &self.files
    }

    /// `true` when the walk hit `MAX_WORKSPACE_FILES` and the candidate
    /// set excludes some workspace files.
    pub(crate) fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// Returns `true` when the cache should be rebuilt: the `.git/index`
    /// mtime has changed (tracked-file mutations) or the refresh floor
    /// has elapsed (catches new untracked files).
    pub(crate) fn should_rebuild(&self, root: &Path) -> bool {
        self.should_rebuild_at(root, Instant::now())
    }

    /// Variant of `should_rebuild` that takes an explicit `now`, so tests
    /// can avoid sleeping for the refresh floor.
    pub(crate) fn should_rebuild_at(&self, root: &Path, now: Instant) -> bool {
        let current_mtime = git_index_mtime(root);
        if current_mtime != self.git_index_mtime {
            return true;
        }
        now.saturating_duration_since(self.built_at) >= WORKSPACE_REFRESH_THROTTLE
    }

    /// Test-only constructor that seeds the cache with a fixed file list,
    /// skipping the workspace walk and the `.git/index` stat.
    #[cfg(test)]
    pub(crate) fn from_paths_for_tests(files: Vec<PathBuf>) -> Self {
        Self {
            files: Arc::new(files),
            truncated: false,
            built_at: Instant::now(),
            git_index_mtime: None,
        }
    }

    /// Test-only constructor that seeds the cache and marks it truncated,
    /// so the footer hint can be exercised without a 5000-file walk.
    #[cfg(test)]
    pub(crate) fn from_truncated_paths_for_tests(files: Vec<PathBuf>) -> Self {
        Self {
            files: Arc::new(files),
            truncated: true,
            built_at: Instant::now(),
            git_index_mtime: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn built_at_for_tests(&self) -> Instant {
        self.built_at
    }
}

/// Best-effort `.git/index` mtime probe. Returns `None` for non-git dirs,
/// fresh repos with no index yet, or worktrees where `.git` is a file —
/// in each case the caller falls back to the refresh floor.
fn git_index_mtime(root: &Path) -> Option<SystemTime> {
    std::fs::metadata(root.join(".git").join("index"))
        .ok()
        .and_then(|m| m.modified().ok())
}

/// Rank `files` against `query`. Returns up to `MAX_MATCHES` paths
/// plus the total candidate count *before* truncation so the popup
/// can render a `(idx/total)` footer that still reflects how many
/// matches exist beyond the displayed window.
///
/// Delegates to the fuzzy scorer (higher is better) so the
/// composer typeahead shares the same word-boundary / consecutive-run
/// scoring as the slash menu. Filename-prefix hits stay on top
/// naturally — `@lib` matched against `lib.rs` earns word-boundary
/// bonuses on every char and ties with `crates/.../lib.rs` are broken
/// by the shorter-path rule below.
pub(crate) fn rank_files(query: &str, files: &[PathBuf]) -> (Vec<PathBuf>, usize) {
    if query.is_empty() {
        let matches: Vec<PathBuf> = files
            .iter()
            .take(MAX_MATCHES)
            .map(|p| p.to_path_buf())
            .collect();
        return (matches, files.len());
    }
    let query = crate::fuzzy::PreparedQuery::new(query);
    let mut top = Vec::with_capacity(MAX_MATCHES + 1);
    let mut total = 0;
    for (index, path) in files.iter().enumerate() {
        if let Some(score) = {
            let display = path.to_string_lossy();
            crate::fuzzy::score_prepared(&display, &query)
        } {
            total += 1;
            top.push(ScoredPath {
                score,
                path: path.as_path(),
                index,
            });
            top.sort_by(compare_scored_paths);
            if top.len() > MAX_MATCHES {
                top.pop();
            }
        }
    }
    let matches = top
        .into_iter()
        .map(|scored| scored.path.to_path_buf())
        .collect();
    (matches, total)
}

struct ScoredPath<'a> {
    score: i32,
    path: &'a Path,
    index: usize,
}

fn compare_scored_paths(left: &ScoredPath<'_>, right: &ScoredPath<'_>) -> Ordering {
    // Higher score first; on tie shorter path first since it is
    // usually closer to what the user meant. Equal score and length
    // preserve the input order that the previous stable full sort used.
    right
        .score
        .cmp(&left.score)
        .then(
            left.path
                .as_os_str()
                .len()
                .cmp(&right.path.as_os_str().len()),
        )
        .then(left.index.cmp(&right.index))
}

/// Popup state attached to the app.
#[derive(Debug, Clone, Default)]
pub(crate) struct MentionPopup {
    pub query: String,
    pub start: usize,
    pub end: usize,
    pub matches: Vec<PathBuf>,
    pub selected: usize,
    /// Total candidates that matched `query` *before* truncation to
    /// `MAX_MATCHES`. Drives the `(idx/total)` footer so the user can
    /// see when more matches exist than the popup shows.
    pub total: usize,
    /// `true` when the workspace walk hit `MAX_WORKSPACE_FILES`, so the
    /// candidate set excludes some files. Drives a footer hint warning
    /// the user that results are incomplete.
    pub truncated: bool,
}

impl MentionPopup {
    pub(crate) fn from_query(
        q: MentionQuery,
        matches: Vec<PathBuf>,
        total: usize,
        truncated: bool,
    ) -> Self {
        Self {
            query: q.query,
            start: q.start,
            end: q.end,
            matches,
            selected: 0,
            total,
            truncated,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.matches.is_empty()
    }

    pub(crate) fn selected_path(&self) -> Option<&Path> {
        self.matches.get(self.selected).map(|p| p.as_path())
    }

    pub(crate) fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    pub(crate) fn move_down(&mut self) {
        if self.selected + 1 < self.matches.len() {
            self.selected += 1;
        }
    }

    /// Returns the (new_input, new_cursor) after inserting the selected
    /// path into `input` at the mention range.
    pub(crate) fn apply(&self, input: &str) -> Option<(String, usize)> {
        let path = self.selected_path()?;
        let replacement = format!("{} ", path.display());
        let mut out = String::with_capacity(input.len() + replacement.len());
        out.push_str(&input[..self.start]);
        out.push_str(&replacement);
        out.push_str(&input[self.end..]);
        let new_cursor = self.start + replacement.len();
        Some((out, new_cursor))
    }
}

#[cfg(test)]
#[path = "mention_tests.rs"]
mod tests;
