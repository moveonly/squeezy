use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use crate::{SkillEntry, SkillSummary};

const RUNNERS: &[&str] = &[
    "python", "python3", "bash", "zsh", "sh", "node", "deno", "ruby", "perl", "pwsh",
];
const SCRIPT_EXTENSIONS: &[&str] = &[".py", ".sh", ".js", ".ts", ".rb", ".pl", ".ps1"];
const READERS_COMMON: &[&str] = &[
    // Unix readers
    "cat",
    "sed",
    "head",
    "tail",
    "less",
    "more",
    "bat",
    "awk",
    // Common search/read tools that may read or locate skill docs.
    "rg",
    "fd",
    "find",
    // PowerShell Get-Content cmdlet and gc alias are safe cross-platform —
    // neither is a standard Unix command that has a different meaning.
    "get-content",
    "gc",
];

// `type` is a Unix shell built-in for command introspection ("type bash" →
// "bash is /bin/bash"), not a file reader. On Unix, treating it as a reader
// would cause `type SKILL.md` in a directory containing a SKILL.md to trigger
// implicit activation. On Windows it is the cmd.exe file-display command and
// is a legitimate file reader.
#[cfg(windows)]
const READERS_WINDOWS_ONLY: &[&str] = &["type"];

fn is_reader_program(program: &str) -> bool {
    if READERS_COMMON.contains(&program) {
        return true;
    }
    #[cfg(windows)]
    {
        if READERS_WINDOWS_ONLY.contains(&program) {
            return true;
        }
    }
    false
}

pub(crate) fn detect_for_command(
    command: &str,
    workdir: &Path,
    by_scripts_dir: &BTreeMap<PathBuf, String>,
    by_doc_path: &BTreeMap<PathBuf, String>,
    doc_filenames: &BTreeSet<String>,
    skills: &BTreeMap<String, SkillEntry>,
) -> Option<SkillSummary> {
    let workdir = normalize_path(workdir);
    let tokens = tokenize_command(command);
    let name = detect_skill_script_run(&tokens, &workdir, by_scripts_dir)
        .or_else(|| detect_skill_doc_read(&tokens, &workdir, by_doc_path, doc_filenames))?;
    let entry = skills.get(&name)?;
    (!entry.summary.disabled).then(|| entry.summary.clone())
}

pub(crate) fn normalize_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn detect_skill_script_run(
    tokens: &[String],
    workdir: &Path,
    by_scripts_dir: &BTreeMap<PathBuf, String>,
) -> Option<String> {
    let script_token = script_run_token(tokens)?;
    let script_path = Path::new(script_token);
    let script_path = if script_path.is_absolute() {
        script_path.to_path_buf()
    } else {
        workdir.join(script_path)
    };
    let script_path = normalize_path(&script_path);
    for ancestor in script_path.ancestors() {
        if let Some(name) = by_scripts_dir.get(ancestor) {
            return Some(name.clone());
        }
    }
    None
}

fn detect_skill_doc_read(
    tokens: &[String],
    workdir: &Path,
    by_doc_path: &BTreeMap<PathBuf, String>,
    doc_filenames: &BTreeSet<String>,
) -> Option<String> {
    if !command_reads_file(tokens) {
        return None;
    }
    for token in tokens.iter().skip(1) {
        if token.starts_with('-') {
            continue;
        }
        if !doc_token_may_match_indexed_path(token, doc_filenames) {
            continue;
        }
        let path = Path::new(token);
        let candidate_path = if path.is_absolute() {
            normalize_path(path)
        } else {
            normalize_path(&workdir.join(path))
        };
        if let Some(name) = by_doc_path.get(&candidate_path) {
            return Some(name.clone());
        }
    }
    None
}

fn doc_token_may_match_indexed_path(token: &str, doc_filenames: &BTreeSet<String>) -> bool {
    let Some(file_name) = Path::new(token).file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    // Any SKILL.md token is always a candidate — it matches the convention
    // for skill documents regardless of catalog contents.
    if file_name == "SKILL.md" {
        return true;
    }
    // O(log n) lookup via the pre-built filename set instead of a full key scan.
    doc_filenames.contains(&file_name.to_ascii_lowercase())
}

fn script_run_token(tokens: &[String]) -> Option<&str> {
    let runner_token = tokens.first()?;
    let runner_base = command_basename(runner_token).to_ascii_lowercase();
    let runner_base = runner_base.strip_suffix(".exe").unwrap_or(&runner_base);

    // Direct executable path: `./scripts/task.sh`, `/abs/path/script.py`, etc.
    // The token itself is the script — no runner prefix needed.
    if is_path_like(runner_token) {
        if SCRIPT_EXTENSIONS
            .iter()
            .any(|ext| runner_token.to_ascii_lowercase().ends_with(ext))
        {
            return Some(runner_token);
        }
        // Path-like but no recognized extension; cannot be a script run.
        return None;
    }

    // `env [options/assignments] <runner> <script>` — skip `env` and any
    // VAR=value assignments or flags before the real runner.
    if runner_base == "env" {
        let rest = skip_env_prefix(tokens.iter().skip(1).map(String::as_str))?;
        return script_run_token_from_runner_and_rest(rest);
    }

    if !RUNNERS.contains(&runner_base) {
        return None;
    }
    script_run_token_from_rest(tokens.iter().skip(1))
}

/// Given a slice that starts right after the `env` token, skip option flags
/// (`-i`, `-u NAME`, etc.) and `NAME=VALUE` assignments, then return the
/// remaining tokens starting from the real runner command.
fn skip_env_prefix<'a>(mut iter: impl Iterator<Item = &'a str>) -> Option<Vec<&'a str>> {
    let mut remaining = Vec::new();
    // Collect all tokens so we can index them.
    for tok in iter.by_ref() {
        remaining.push(tok);
    }
    let mut i = 0;
    while i < remaining.len() {
        let tok = remaining[i];
        // `-` or `--` ends option parsing for env(1).
        if tok == "-" || tok == "--" {
            i += 1;
            break;
        }
        if tok.starts_with('-') {
            // Options that consume the next argument as a plain value: -u
            // (unset), -C (chdir). -S / --split-string embeds a sub-command
            // string that would require recursive parsing; skip it the same
            // as a bare flag (discards just the flag token) so we don't
            // silently drop the embedded command.
            if matches!(tok, "-u" | "-C") {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        // NAME=VALUE assignment — skip.
        if tok.contains('=') && !tok.starts_with('=') {
            let name_part = &tok[..tok.find('=').unwrap_or(0)];
            if name_part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                i += 1;
                continue;
            }
        }
        // First non-option, non-assignment token is the runner.
        break;
    }
    if i >= remaining.len() {
        return None;
    }
    Some(remaining[i..].to_vec())
}

/// Recognises runner + optional flags + script from a `[runner, rest…]` slice
/// where `rest` has already had the `env` prefix stripped.
fn script_run_token_from_runner_and_rest(tokens: Vec<&str>) -> Option<&str> {
    let runner = tokens.first()?;
    let runner_base = command_basename(runner).to_ascii_lowercase();
    let runner_base = runner_base.strip_suffix(".exe").unwrap_or(&runner_base);
    if !RUNNERS.contains(&runner_base) {
        return None;
    }
    // Re-use the rest logic on the slice after the runner.
    for tok in tokens.iter().skip(1) {
        if *tok == "--" || tok.starts_with('-') {
            continue;
        }
        if SCRIPT_EXTENSIONS
            .iter()
            .any(|ext| tok.to_ascii_lowercase().ends_with(ext))
        {
            return Some(tok);
        }
        return None;
    }
    None
}

fn script_run_token_from_rest<'a, I>(mut iter: I) -> Option<&'a str>
where
    I: Iterator<Item = &'a String>,
{
    for token in iter.by_ref() {
        if token == "--" || token.starts_with('-') {
            continue;
        }
        if SCRIPT_EXTENSIONS
            .iter()
            .any(|extension| token.to_ascii_lowercase().ends_with(extension))
        {
            return Some(token);
        }
        return None;
    }
    None
}

/// Returns `true` when a token looks like a filesystem path rather than a bare
/// command name: starts with `./`, `../`, or `/`, or is an absolute Windows
/// path (letter + `:\`).
fn is_path_like(token: &str) -> bool {
    token.starts_with("./")
        || token.starts_with("../")
        || token.starts_with('/')
        || (token.len() >= 3
            && token.as_bytes()[0].is_ascii_alphabetic()
            && token.as_bytes()[1] == b':'
            && (token.as_bytes()[2] == b'\\' || token.as_bytes()[2] == b'/'))
}

fn command_reads_file(tokens: &[String]) -> bool {
    let Some(program) = tokens.first() else {
        return false;
    };
    let program = command_basename(program).to_ascii_lowercase();
    is_reader_program(&program)
}

fn command_basename(command: &str) -> String {
    Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(command)
        .to_string()
}

fn tokenize_command(command: &str) -> Vec<String> {
    // Dispatch to the correct variant based on the host platform. On Windows,
    // backslash is a path separator (PowerShell uses backtick for escaping),
    // so we treat `\` as a literal. On Unix, `\` outside quotes escapes the
    // next character. Both functions are compiled on all platforms so they can
    // be exercised by cross-platform unit tests.
    if cfg!(windows) {
        tokenize_command_windows(command)
    } else {
        tokenize_command_unix(command)
    }
}

/// Unix tokenizer: `\` outside quotes escapes the next character.
/// Compiled on all platforms so Windows-path tests can verify the Unix
/// variant's behavior and vice versa, reducing the risk of silent divergence.
fn tokenize_command_unix(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;

    for ch in command.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && quote.is_none() {
            escaped = true;
            continue;
        }
        match quote {
            Some(active) if ch == active => quote = None,
            Some(_) => current.push(ch),
            None if ch == '\'' || ch == '"' => quote = Some(ch),
            None if ch.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            None => current.push(ch),
        }
    }

    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Windows tokenizer: `\` is a path separator, not an escape character.
/// PowerShell uses backtick (`) for escaping. Compiled on all platforms so
/// the logic can be unit-tested on Linux and macOS CI runners.
fn tokenize_command_windows(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;

    for ch in command.chars() {
        match quote {
            Some(active) if ch == active => quote = None,
            Some(_) => current.push(ch),
            None if ch == '\'' || ch == '"' => quote = Some(ch),
            None if ch.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            None => current.push(ch),
        }
    }

    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

#[cfg(test)]
#[path = "implicit_tests.rs"]
mod tests;
