//! Pure glob-based resolution of deny-read path patterns.
//!
//! Used by the elevated tier to build the concrete list of paths whose reads
//! must be denied via ACL.  No Win32 calls — pure filesystem + glob logic —
//! so this module is unit-testable on any host platform.
//!
//! NOTE: This function is not yet called on the restricted-token tier; the
//! elevated tier (a later phase) consumes it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSetBuilder};

use super::path_norm;

/// Resolve deny-read patterns into a deduplicated, sorted list of **existing**
/// concrete paths.
///
/// Resolution rules:
/// - Each `pattern` is tried as a glob against both `home` (if provided) and
///   `workspace` as alternative roots, using the pattern as relative if it
///   contains no absolute component.
/// - Absolute patterns are matched directly.
/// - `explicit` paths are included verbatim (regardless of existence, matching
///   the model used by the elevated tier for exact paths the caller already
///   resolved).
/// - Results are deduplicated by `path_norm::canonical_key`.
///
/// Examples of patterns:
///   `.ssh/**`  →  expands under `home/.ssh/` recursively
///   `.netrc`   →  `home/.netrc` (single file)
///   `.aws/**`  →  `home/.aws/` recursively
#[allow(dead_code)] // consumed by the elevated tier (later phase)
pub(crate) fn resolve_deny_read_paths(
    patterns: &[String],
    explicit: &[PathBuf],
    home: Option<&Path>,
    workspace: &Path,
) -> Vec<PathBuf> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<PathBuf> = Vec::new();

    let mut push = |p: PathBuf| {
        let key = path_norm::canonical_key(&p);
        if seen.insert(key) {
            out.push(p);
        }
    };

    // Explicit paths go in first, verbatim.
    for p in explicit {
        push(p.clone());
    }

    // Expand each glob pattern against home and workspace roots.
    for pattern in patterns {
        let pat_path = Path::new(pattern.as_str());

        if pat_path.is_absolute() {
            // Absolute glob: match directly.
            collect_glob_matches(pattern, None, &mut |p| push(p));
        } else {
            // Relative: try home first, then workspace.
            if let Some(h) = home {
                let rooted = h.join(pattern);
                let rooted_str = rooted.to_string_lossy().replace('\\', "/");
                collect_glob_matches(&rooted_str, None, &mut |p| push(p));
            }
            // Also try workspace root.
            let rooted = workspace.join(pattern);
            let rooted_str = rooted.to_string_lossy().replace('\\', "/");
            collect_glob_matches(&rooted_str, None, &mut |p| push(p));
        }
    }

    out
}

/// Walk the filesystem starting at the deepest literal prefix of `pattern` and
/// collect every existing path that matches the full glob.
fn collect_glob_matches(pattern: &str, _hint: Option<()>, push: &mut impl FnMut(PathBuf)) {
    // Build a globset for matching.
    let glob = match Glob::new(pattern) {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!(pattern, err = %e, "deny_read_resolver: invalid glob pattern; skipping");
            return;
        }
    };
    let mut builder = GlobSetBuilder::new();
    builder.add(glob);
    let glob_set = match builder.build() {
        Ok(gs) => gs,
        Err(e) => {
            tracing::warn!(pattern, err = %e, "deny_read_resolver: failed to build globset; skipping");
            return;
        }
    };

    // Find the deepest literal directory prefix before the first glob metacharacter.
    let scan_root = literal_scan_root(pattern);

    if !scan_root.exists() {
        return;
    }

    walk_and_match(&scan_root, &glob_set, push);
}

/// Return the deepest existing directory that is the literal prefix of `pattern`
/// before the first glob metacharacter (`*`, `?`, `[`).
fn literal_scan_root(pattern: &str) -> PathBuf {
    let first_glob = pattern
        .char_indices()
        .find(|(_, ch)| matches!(ch, '*' | '?' | '['))
        .map(|(i, _)| i)
        .unwrap_or(pattern.len());

    let literal_prefix = &pattern[..first_glob];

    // Trim to the last path separator within the literal prefix.
    let sep_idx = literal_prefix.rfind(['/', '\\']).unwrap_or(0);

    let dir_part = if sep_idx == 0 && !literal_prefix.is_empty() {
        // The glob starts at the root or has no separator: scan the pattern root.
        Path::new(literal_prefix).parent().unwrap_or(Path::new("."))
    } else {
        Path::new(&literal_prefix[..sep_idx])
    };

    let candidate = if dir_part.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        dir_part.to_path_buf()
    };

    // Walk up until we find an existing directory (guards against non-existent
    // intermediate prefixes).
    let mut p = candidate;
    loop {
        if p.exists() {
            return p;
        }
        match p.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => p = parent.to_path_buf(),
            _ => return PathBuf::from("."),
        }
    }
}

/// Recursively descend from `dir`, calling `push` for every path (file or dir)
/// whose full path matches `glob_set`.
fn walk_and_match(dir: &Path, glob_set: &globset::GlobSet, push: &mut impl FnMut(PathBuf)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        let Ok(ft) = entry.file_type() else { continue };

        // Use the normalized path string for glob matching.
        let p_str = p.to_string_lossy().replace('\\', "/");
        if glob_set.is_match(&p_str) {
            push(p.clone());
        }

        // Recurse into directories (but not symlinks, to avoid cycles).
        if ft.is_dir() && !ft.is_symlink() {
            walk_and_match(&p, glob_set, push);
        }
    }
}

#[cfg(test)]
#[path = "deny_read_resolver_tests.rs"]
mod tests;
