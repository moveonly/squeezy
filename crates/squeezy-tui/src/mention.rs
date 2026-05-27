//! `@<path>` file mention popup for the composer.
//!
//! Detects an `@`-prefixed token at the cursor and lists matching
//! workspace files ranked by a simple subsequence/prefix scorer. The
//! popup is small (10 entries) and dismisses on Esc or when the
//! cursor leaves the `@<word>` token.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

const MAX_MATCHES: usize = 10;
const MAX_WORKSPACE_FILES: usize = 5000;

/// Floor between background rebuilds of the workspace file list when the
/// `.git/index` mtime has not changed. The floor still picks up untracked
/// files, which don't bump the index. Matches the clear-code peer in
/// `src/hooks/fileSuggestions.ts` (`REFRESH_THROTTLE_MS = 5_000`).
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
pub(crate) fn detect_mention(input: &str, cursor: usize) -> Option<MentionQuery> {
    let bytes = input.as_bytes();
    let cursor = cursor.min(input.len());
    // Walk left from cursor until whitespace or '@'.
    let mut i = cursor;
    while i > 0 {
        let b = bytes[i - 1];
        if b == b'@' {
            // Found the marker. Verify the byte before is whitespace or absent.
            if i >= 2 {
                let prev = bytes[i - 2];
                if !prev.is_ascii_whitespace() {
                    return None;
                }
            }
            // Collect the token from `@` (exclusive) to next whitespace right.
            let start = i - 1;
            let mut end = cursor;
            while end < bytes.len() && !bytes[end].is_ascii_whitespace() {
                end += 1;
            }
            let query = input[i..end].to_string();
            // Reject if there's an embedded whitespace before cursor (shouldn't happen).
            if query.contains(char::is_whitespace) {
                return None;
            }
            return Some(MentionQuery { start, end, query });
        }
        if b.is_ascii_whitespace() {
            return None;
        }
        i -= 1;
    }
    None
}

/// Walk the workspace rooted at `root` (respecting .gitignore via the
/// `ignore` crate) and return up to `MAX_WORKSPACE_FILES` file paths
/// relative to `root`. Used as the candidate set for the popup.
pub(crate) fn load_workspace_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
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
            break;
        }
    }
    files
}

/// Cached workspace file list with an `.git/index` mtime poll and a 5 s
/// refresh floor for untracked-file additions.
///
/// Without a cache the popup re-walked the workspace on every keypress;
/// with a permanent cache it never picked up files created after TUI
/// startup. The poll keeps the cache fresh after git operations
/// (checkout/commit/rm bump `.git/index`) and the floor catches new
/// untracked files, mirroring the clear-code peer in
/// `src/hooks/fileSuggestions.ts:635-686`.
#[derive(Debug, Clone)]
pub(crate) struct WorkspaceFileCache {
    files: Arc<Vec<PathBuf>>,
    built_at: Instant,
    git_index_mtime: Option<SystemTime>,
}

impl WorkspaceFileCache {
    /// Walk `root` once and snapshot the `.git/index` mtime.
    pub(crate) fn build(root: &Path) -> Self {
        let files = Arc::new(load_workspace_files(root));
        let git_index_mtime = git_index_mtime(root);
        Self {
            files,
            built_at: Instant::now(),
            git_index_mtime,
        }
    }

    /// Shared handle to the cached file list. Cheap to clone via `Arc`.
    pub(crate) fn files(&self) -> &Arc<Vec<PathBuf>> {
        &self.files
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

/// Rank `files` against `query`. Returns up to `MAX_MATCHES` paths.
///
/// Delegates to `squeezy_rank::fuzzy::fuzzy_path_score` (lower is better)
/// for the core scoring so the composer typeahead shares the same
/// path-separator normalisation and subsequence matcher as the rest of
/// the workspace ranking surface. A synthetic bonus is layered on top so
/// paths whose basename starts with the query stay at the top of the
/// list (`@lib` → `lib.rs` before `crates/.../lib.rs`).
pub(crate) fn rank_files(query: &str, files: &[PathBuf]) -> Vec<PathBuf> {
    if query.is_empty() {
        return files
            .iter()
            .take(MAX_MATCHES)
            .map(|p| p.to_path_buf())
            .collect();
    }
    let query_lower = query.to_ascii_lowercase();
    let mut scored: Vec<(i32, &Path)> = files
        .iter()
        .filter_map(|path| {
            let score = score_path(&query_lower, path)?;
            Some((score, path.as_path()))
        })
        .collect();
    // Lower score first (fuzzy is min-score); on tie shorter path first
    // since it is usually closer to what the user meant.
    scored.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.as_os_str().len().cmp(&b.1.as_os_str().len()))
    });
    scored
        .into_iter()
        .take(MAX_MATCHES)
        .map(|(_, p)| p.to_path_buf())
        .collect()
}

fn score_path(query: &str, path: &Path) -> Option<i32> {
    let display = path.to_string_lossy();
    let mut score = squeezy_rank::fuzzy_path_score(&display, query)?;
    if let Some(name) = path.file_name().and_then(|n| n.to_str())
        && name.to_ascii_lowercase().starts_with(query)
    {
        // Synthetic bias so a filename-prefix hit always outranks a
        // mid-path subsequence hit even when the latter happens to score
        // lower. Matches the ergonomics of the previous hand-rolled
        // scorer where filename prefix was the top tier.
        score -= 1000;
    }
    Some(score)
}

/// Popup state attached to the app.
#[derive(Debug, Clone, Default)]
pub(crate) struct MentionPopup {
    pub query: String,
    pub start: usize,
    pub end: usize,
    pub matches: Vec<PathBuf>,
    pub selected: usize,
}

impl MentionPopup {
    pub(crate) fn from_query(q: MentionQuery, matches: Vec<PathBuf>) -> Self {
        Self {
            query: q.query,
            start: q.start,
            end: q.end,
            matches,
            selected: 0,
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
