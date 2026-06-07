//! Pure ssh-config dependency parser.
//!
//! Parses `~/.ssh/config` for file-path directives (`IdentityFile`, `Include`,
//! `UserKnownHostsFile`, `IdentityAgent`) and returns the concrete set of paths
//! they reference.  `Include` directives are followed recursively up to a depth
//! cap.
//!
//! No Win32 calls — pure filesystem / string logic — so this module is
//! unit-testable on any host platform.
//!
//! NOTE: This function is not yet called on the restricted-token tier; the
//! elevated tier (a later phase) consumes it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// The ssh_config directives whose arguments are file-system paths we must
/// protect.  Lower-cased for case-insensitive matching.
const PATH_DIRECTIVES: &[&str] = &[
    "identityfile",
    "include",
    "userknownhostsfile",
    "identityagent",
];

/// Maximum recursion depth when following `Include` chains.
const MAX_INCLUDE_DEPTH: usize = 32;

/// Parse `~/.ssh/config` (and any files it includes) and return all existing
/// paths referenced by file-path directives.
///
/// The returned vec always starts with `<home>/.ssh/config` itself (if `home`
/// is `Some`), whether or not that file exists, so callers can protect the
/// config file even before it is created.  All other entries are paths that
/// were reachable from existing config content.
#[allow(dead_code)] // consumed by the elevated tier (later phase)
pub(crate) fn ssh_config_dependency_paths(home: Option<&Path>) -> Vec<PathBuf> {
    let Some(home) = home else {
        return Vec::new();
    };

    let ssh_dir = home.join(".ssh");
    let config_path = ssh_dir.join("config");

    // Always include the config file itself.
    let mut paths: Vec<PathBuf> = vec![config_path.clone()];
    let mut visited: HashSet<PathBuf> = HashSet::new();

    visit_config(&config_path, home, &ssh_dir, &mut visited, &mut paths, 0);

    paths
}

/// Recursively parse an ssh_config file and collect path-directive targets.
fn visit_config(
    path: &Path,
    home: &Path,
    ssh_dir: &Path,
    visited: &mut HashSet<PathBuf>,
    paths: &mut Vec<PathBuf>,
    depth: usize,
) {
    if depth >= MAX_INCLUDE_DEPTH {
        return;
    }

    // Use the canonical path for the visited guard (avoids symlink cycles),
    // but fall back to the raw path when canonicalization fails.
    let canon_key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canon_key) {
        return;
    }

    let Ok(contents) = std::fs::read_to_string(path) else {
        return;
    };

    for line in contents.lines() {
        let Some((keyword, args)) = parse_directive(line) else {
            continue;
        };
        let kw_lower = keyword.to_ascii_lowercase();

        match kw_lower.as_str() {
            "include" => {
                for arg in &args {
                    for included in expand_include(arg, home, ssh_dir) {
                        paths.push(included.clone());
                        visit_config(&included, home, ssh_dir, visited, paths, depth + 1);
                    }
                }
            }
            kw if PATH_DIRECTIVES.contains(&kw) && kw != "include" => {
                for arg in &args {
                    if let Some(resolved) = resolve_path_arg(arg, home, None) {
                        paths.push(resolved);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Expand an `Include` argument: resolve `~` prefixes, then glob-expand with
/// `ssh_dir` as the relative base for non-absolute, non-tilde paths.
fn expand_include(arg: &str, home: &Path, ssh_dir: &Path) -> Vec<PathBuf> {
    let Some(pattern_path) = resolve_path_arg(arg, home, Some(ssh_dir)) else {
        return Vec::new();
    };
    let pattern = pattern_path.to_string_lossy().replace('\\', "/");

    // Use globset to expand the pattern.
    let glob = match globset::Glob::new(&pattern) {
        Ok(g) => g,
        Err(_) => return Vec::new(),
    };
    let mut builder = globset::GlobSetBuilder::new();
    builder.add(glob);
    let glob_set = match builder.build() {
        Ok(gs) => gs,
        Err(_) => return Vec::new(),
    };

    // Scan only the immediate directory of the pattern (includes are typically flat).
    let scan_dir = PathBuf::from(&pattern)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| ssh_dir.to_path_buf());

    if !scan_dir.exists() {
        return Vec::new();
    }

    let Ok(entries) = std::fs::read_dir(&scan_dir) else {
        return Vec::new();
    };

    entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let p_str = p.to_string_lossy().replace('\\', "/");
            if glob_set.is_match(&p_str) {
                Some(p)
            } else {
                None
            }
        })
        .collect()
}

/// Resolve a path argument from an ssh_config directive.
///
/// Handles:
/// - `~`, `%d`, `${HOME}` → `home`
/// - `~/…`, `%d/…`, `${HOME}/…` → `home/…`
/// - Absolute paths → as-is
/// - Relative paths → joined to `relative_base` if provided, otherwise `None`
fn resolve_path_arg(arg: &str, home: &Path, relative_base: Option<&Path>) -> Option<PathBuf> {
    if arg.eq_ignore_ascii_case("none") {
        return None;
    }

    // Bare home aliases.
    if arg == "~" || arg == "%d" || arg == "${HOME}" {
        return Some(home.to_path_buf());
    }

    // Home-prefixed aliases.
    let home_prefixes = ["~/", r"~\", "%d/", r"%d\", "${HOME}/", r"${HOME}\"];
    for prefix in &home_prefixes {
        if let Some(rest) = arg.strip_prefix(prefix) {
            return Some(home.join(rest));
        }
    }

    let p = PathBuf::from(arg);
    if p.is_absolute() {
        Some(p)
    } else {
        relative_base.map(|base| base.join(&p))
    }
}

/// Parse a single ssh_config line into `(keyword, arguments)`.
///
/// Handles both space-separated (`Host foo`) and `=`-separated (`IdentityFile=…`)
/// forms.  Comments and empty lines return `None`.
fn parse_directive(line: &str) -> Option<(String, Vec<String>)> {
    let tokens = tokenize(line);
    if tokens.is_empty() {
        return None;
    }

    // The first token may be `KEY=VALUE` or just `KEY`.
    let first = tokens[0].clone();
    if let Some((key, value)) = first.split_once('=') {
        if key.is_empty() {
            return None;
        }
        let mut args: Vec<String> = Vec::new();
        if !value.is_empty() {
            args.push(value.to_string());
        }
        // Remaining tokens after the first (KEY=VALUE ..rest..) are additional args.
        args.extend_from_slice(&tokens[1..]);
        Some((key.to_string(), args))
    } else {
        // Space-separated: `KEY arg1 arg2 …`
        // The second token might start with `=` (e.g. `IdentityFile = path`).
        let key = first;
        let mut rest: Vec<String> = tokens[1..].to_vec();
        // Strip leading `=` from the first argument if present.
        if let Some(first_arg) = rest.first_mut()
            && let Some(stripped) = first_arg.strip_prefix('=')
        {
            *first_arg = stripped.to_string();
        }
        rest.retain(|s| !s.is_empty());
        Some((key, rest))
    }
}

/// Split a config line into tokens, respecting `#` comments and single/double
/// quoting.  Returns an empty vec for blank / comment-only lines.
fn tokenize(line: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = line.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            // Comment: stop processing.
            '#' if quote.is_none() => break,
            // Close quote.
            '\'' | '"' if quote == Some(ch) => quote = None,
            // Open quote.
            '\'' | '"' if quote.is_none() => quote = Some(ch),
            // Escape sequences.
            '\\' => {
                if let Some(&next) = chars.peek() {
                    if matches!(next, '\'' | '"' | '\\') || (quote.is_none() && next == ' ') {
                        if let Some(escaped) = chars.next() {
                            current.push(escaped);
                        }
                    } else {
                        current.push(ch);
                    }
                } else {
                    current.push(ch);
                }
            }
            // Whitespace outside quotes: token boundary.
            ch if ch.is_whitespace() && quote.is_none() => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            ch => current.push(ch),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

#[cfg(test)]
#[path = "ssh_config_tests.rs"]
mod tests;
