use std::{
    collections::BTreeSet,
    fs,
    io::Read as _,
    path::{Path, PathBuf},
};

use crate::{
    COMPAT_PROJECT_SKILLS_DIR, PROJECT_SKILLS_DIR, SKILL_FILE, SkillCatalog,
    catalog::ancestor_project_roots,
    frontmatter::{is_valid_skill_name, parse_skill_file},
};

pub fn parse_skill_triggers(content: &str) -> Vec<String> {
    parse_skill_file(content)
        .map(|(meta, _)| {
            meta.triggers
                .into_iter()
                .map(|t| t.trim().to_ascii_lowercase())
                .filter(|t| !t.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Extended authoring lint for a single `SKILL.md` file.
///
/// Returns a list of `(severity, message)` pairs where severity is either
/// `"error"` or `"warning"`. Callers pass the file content, the skill's
/// parent directory (for script-existence checks), and the set of trigger
/// phrases that are ambiguous across the catalog.
///
/// This is a best-effort scan: the first parse error terminates the lint
/// early. An empty return means no issues were found.
pub fn lint_skill_extended(
    content: &str,
    skill_dir: &std::path::Path,
    ambiguous_triggers: &std::collections::BTreeSet<String>,
    body_warn_bytes: usize,
) -> Vec<(&'static str, String)> {
    let Ok((meta, body)) = parse_skill_file(content) else {
        return Vec::new();
    };
    let mut issues: Vec<(&'static str, String)> = Vec::new();

    // Oversized body.
    if body.len() > body_warn_bytes {
        issues.push((
            "warning",
            format!(
                "body is {} bytes (>{} bytes); consider splitting or summarising to reduce context cost",
                body.len(),
                body_warn_bytes
            ),
        ));
    }

    // Ambiguous trigger phrases.
    for trigger in &meta.triggers {
        let normalised = trigger.trim().to_ascii_lowercase();
        if ambiguous_triggers.contains(&normalised) {
            issues.push((
                "warning",
                format!(
                    "trigger `{trigger}` is declared by more than one skill; \
                     auto-activation skipped for this phrase"
                ),
            ));
        }
    }

    // Missing hook scripts (hooks referencing relative script paths).
    for matchers in meta.hooks.values() {
        for matcher in matchers {
            for hook in &matcher.hooks {
                let cmd = hook.command.trim();
                if cmd.starts_with("scripts/") || cmd.starts_with("./scripts/") {
                    let script_path = skill_dir.join(cmd);
                    if !script_path.exists() {
                        issues.push((
                            "warning",
                            format!("hook command `{cmd}` references a script that does not exist"),
                        ));
                    }
                }
            }
        }
    }

    issues
}

pub fn unmet_tool_deps(
    deps: &[String],
    available_tools: &BTreeSet<String>,
    available_mcp_servers: &BTreeSet<String>,
) -> Vec<String> {
    deps.iter()
        .filter(|dep| {
            let trimmed = dep.trim();
            if trimmed.is_empty() {
                return false;
            }
            if let Some(server) = trimmed.strip_prefix("mcp:") {
                !available_mcp_servers.contains(server.trim())
            } else {
                !available_tools.contains(trimmed)
            }
        })
        .cloned()
        .collect()
}

/// Parse `SKILL.md` content and report whether it satisfies the
/// catalog's frontmatter and naming rules.
///
/// Reuses the same parser the discovery walker uses, so a `Ok(name)`
/// means the file is byte-for-byte loadable into the catalog. The
/// returned `name` is the canonical skill name read from the
/// frontmatter `name:` field, useful for the CLI's `validate`
/// subcommand to surface what is being validated.
pub fn validate_skill_md(content: &str) -> std::result::Result<String, String> {
    let (metadata, _body) = parse_skill_file(content)?;
    if !is_valid_skill_name(&metadata.name) {
        return Err(format!(
            "invalid skill name {:?}: must start with a lowercase ASCII letter and contain only lowercase letters, digits, '-', or '_'",
            metadata.name
        ));
    }
    Ok(metadata.name)
}

/// Outcome of validating a single `SKILL.md` file found in a skill root.
#[derive(Debug, Clone)]
pub struct SkillValidationResult {
    /// Path to the `SKILL.md` file.
    pub path: PathBuf,
    /// Skill name from frontmatter, or the raw string that failed naming rules.
    /// `None` when the file could not be read or the frontmatter had no
    /// parseable `name:` field.
    pub name: Option<String>,
    /// `Ok(())` when the file parsed cleanly and the name is valid;
    /// `Err(reason)` with a human-readable description of the first issue
    /// found.
    pub outcome: std::result::Result<(), String>,
}

/// Return every skill *root directory* that [`SkillCatalog::discover`] will
/// scan for a given `(workspace_root, config)` pair, in the same order
/// `discover` uses. This includes:
///
/// - User-level roots (`compat_user_dir`, `user_dir`, optional XDG user dir).
/// - `extra_roots` from config.
/// - The current workspace's project roots (`.agents/skills`,
///   `.squeezy/skills`).
/// - **All ancestor project roots** up to the git root (monorepo support) —
///   the same directories scanned by `ancestor_project_roots`.
///
/// [`validate_skill_dirs`] calls this so its scan is always identical to what
/// runtime discovery will load.
pub fn skill_scan_dirs(workspace_root: &Path, config: &squeezy_core::SkillsConfig) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    dirs.push(config.compat_user_dir.clone());
    dirs.push(config.user_dir.clone());
    if let Some(xdg_dir) = &config.xdg_user_dir {
        dirs.push(xdg_dir.clone());
    }
    dirs.extend(config.extra_roots.iter().cloned());
    dirs.push(workspace_root.join(COMPAT_PROJECT_SKILLS_DIR));
    dirs.push(workspace_root.join(PROJECT_SKILLS_DIR));
    // Monorepo ancestor walk — mirrors discover's ancestor_project_roots loop.
    for ancestor in ancestor_project_roots(workspace_root) {
        dirs.push(ancestor.join(COMPAT_PROJECT_SKILLS_DIR));
        dirs.push(ancestor.join(PROJECT_SKILLS_DIR));
    }
    dirs
}

/// Walk every configured skill root from `config` (the same roots that
/// [`SkillCatalog::discover`] uses, including ancestor project roots for
/// monorepo launches from subdirectories) and attempt to parse each
/// `SKILL.md`.
///
/// Unlike discovery, this function records parse failures rather than
/// silently skipping them, so `squeezy skills validate` can surface the
/// errors that discovery drops. Non-existent roots and directories without
/// a `SKILL.md` are skipped without an error, mirroring discovery behaviour.
pub fn validate_skill_dirs(
    workspace_root: &Path,
    config: &squeezy_core::SkillsConfig,
) -> Vec<SkillValidationResult> {
    let scan_dirs = skill_scan_dirs(workspace_root, config);
    let mut results = Vec::new();
    for dir in &scan_dirs {
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let skill_path = path.join(SKILL_FILE);
            if !skill_path.exists() {
                continue;
            }
            let content = match fs::read_to_string(&skill_path) {
                Ok(c) => c,
                Err(err) => {
                    results.push(SkillValidationResult {
                        path: skill_path,
                        name: None,
                        outcome: Err(format!("could not read file: {err}")),
                    });
                    continue;
                }
            };
            let outcome = validate_skill_md(&content).map(|_| ());
            let name = parse_skill_file(&content).ok().map(|(meta, _)| meta.name);
            results.push(SkillValidationResult {
                path: skill_path,
                name,
                outcome,
            });
        }
    }
    results
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookDoctorIssue {
    /// Skill name the issue belongs to.
    pub skill: String,
    /// Human-readable description of the problem.
    pub message: String,
    /// Whether this is a hard error (`true`) or an advisory note (`false`).
    pub is_error: bool,
}

/// Inspect every non-disabled skill's hook declarations and return
/// diagnostic issues without registering handlers or running commands.
///
/// Checks performed:
/// - Missing script files for path-like commands (relative to the skill's
///   `base_dir`).
/// - Non-executable script files on Unix (exit 126 at runtime).
/// - Missing shebang line in script files (may cause interpreter errors).
/// - Commands that are inline shell snippets — noted as advisory because
///   snippet behaviour depends on distro shell semantics.
///
/// This is the static analysis pass used by `squeezy doctor` to surface
/// hook problems before a session starts. It does **not** spawn any
/// processes; all checks are pure filesystem/metadata operations.
pub fn catalog_hook_issues(catalog: &SkillCatalog) -> Vec<HookDoctorIssue> {
    let mut issues = Vec::new();
    for summary in catalog.summaries() {
        if summary.disabled {
            continue;
        }
        let loaded = match catalog.load(&summary.name) {
            Ok(l) => l,
            Err(_) => continue,
        };
        for matchers in loaded.hooks.values() {
            for matcher in matchers {
                for spec in &matcher.hooks {
                    let trimmed = spec.command.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    // Classify the command: inline snippet vs file path.
                    let is_inline_snippet = trimmed.contains(['|', ';', '>', '<', '&', '(', ')'])
                        || trimmed.contains('\n');
                    if is_inline_snippet {
                        issues.push(HookDoctorIssue {
                            skill: summary.name.clone(),
                            message: format!(
                                "hook command is an inline shell snippet; behaviour depends on \
                                 the distro shell (`/bin/sh`): {trimmed:.80}"
                            ),
                            is_error: false,
                        });
                        continue;
                    }
                    // The first whitespace-delimited token is the executable;
                    // skip argv-style commands with arguments (e.g. `python3 hook.py`).
                    let executable = trimmed.split_whitespace().next().unwrap_or(trimmed);
                    // Only validate path-like executables (contains '/' or starts with '.').
                    if !executable.contains('/') && !executable.starts_with('.') {
                        continue;
                    }
                    let script_path = if Path::new(executable).is_absolute() {
                        PathBuf::from(executable)
                    } else {
                        loaded.base_dir.join(executable)
                    };
                    match fs::metadata(&script_path) {
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            issues.push(HookDoctorIssue {
                                skill: summary.name.clone(),
                                message: format!(
                                    "hook script not found: {}",
                                    script_path.display()
                                ),
                                is_error: true,
                            });
                        }
                        Ok(meta) => {
                            #[cfg(unix)]
                            {
                                use std::os::unix::fs::PermissionsExt;
                                if meta.permissions().mode() & 0o111 == 0 {
                                    issues.push(HookDoctorIssue {
                                        skill: summary.name.clone(),
                                        message: format!(
                                            "hook script not executable (chmod +x): {}",
                                            script_path.display()
                                        ),
                                        is_error: true,
                                    });
                                }
                            }
                            // Check for shebang line to catch missing interpreter errors.
                            // Only read the first 2 bytes to avoid slurping large scripts.
                            if meta.is_file() {
                                let has_shebang = fs::File::open(&script_path)
                                    .and_then(|mut f| {
                                        let mut buf = [0u8; 2];
                                        f.read_exact(&mut buf).map(|_| buf)
                                    })
                                    .map(|buf| buf[0] == b'#' && buf[1] == b'!')
                                    .unwrap_or(true); // on read error, don't produce a false note
                                if !has_shebang {
                                    issues.push(HookDoctorIssue {
                                        skill: summary.name.clone(),
                                        message: format!(
                                            "hook script missing shebang line: {}",
                                            script_path.display()
                                        ),
                                        is_error: false,
                                    });
                                }
                            }
                        }
                        Err(e) => {
                            issues.push(HookDoctorIssue {
                                skill: summary.name.clone(),
                                message: format!(
                                    "hook script not accessible ({}): {}",
                                    e,
                                    script_path.display()
                                ),
                                is_error: true,
                            });
                        }
                    }
                }
            }
        }
    }
    issues
}

#[cfg(test)]
#[path = "validation_tests.rs"]
mod tests;
