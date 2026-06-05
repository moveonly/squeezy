use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use crate::{SkillEntry, SkillSummary};

const RUNNERS: &[&str] = &[
    "python", "python3", "bash", "zsh", "sh", "node", "deno", "ruby", "perl", "pwsh",
];
const SCRIPT_EXTENSIONS: &[&str] = &[".py", ".sh", ".js", ".ts", ".rb", ".pl", ".ps1"];
const READERS: &[&str] = &["cat", "sed", "head", "tail", "less", "more", "bat", "awk"];

pub(crate) fn detect_for_command(
    command: &str,
    workdir: &Path,
    by_scripts_dir: &BTreeMap<PathBuf, String>,
    by_doc_path: &BTreeMap<PathBuf, String>,
    skills: &BTreeMap<String, SkillEntry>,
) -> Option<SkillSummary> {
    let workdir = normalize_path(workdir);
    let tokens = tokenize_command(command);
    let name = detect_skill_script_run(&tokens, &workdir, by_scripts_dir)
        .or_else(|| detect_skill_doc_read(&tokens, &workdir, by_doc_path))?;
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
) -> Option<String> {
    if !command_reads_file(tokens) {
        return None;
    }
    for token in tokens.iter().skip(1) {
        if token.starts_with('-') {
            continue;
        }
        if !doc_token_may_match_indexed_path(token, by_doc_path) {
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

fn doc_token_may_match_indexed_path(token: &str, by_doc_path: &BTreeMap<PathBuf, String>) -> bool {
    let Some(file_name) = Path::new(token).file_name() else {
        return false;
    };
    by_doc_path
        .keys()
        .any(|path| path.file_name() == Some(file_name))
}

fn script_run_token(tokens: &[String]) -> Option<&str> {
    let runner_token = tokens.first()?;
    let runner = command_basename(runner_token).to_ascii_lowercase();
    let runner = runner.strip_suffix(".exe").unwrap_or(&runner);
    if !RUNNERS.contains(&runner) {
        return None;
    }
    for token in tokens.iter().skip(1) {
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

fn command_reads_file(tokens: &[String]) -> bool {
    let Some(program) = tokens.first() else {
        return false;
    };
    let program = command_basename(program).to_ascii_lowercase();
    READERS.contains(&program.as_str())
}

fn command_basename(command: &str) -> String {
    Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(command)
        .to_string()
}

fn tokenize_command(command: &str) -> Vec<String> {
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
        if ch == '\\' {
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

#[cfg(test)]
#[path = "implicit_tests.rs"]
mod tests;
