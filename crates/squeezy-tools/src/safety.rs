use std::path::{Component, Path, PathBuf};

use squeezy_core::ShellSandboxConfig;

use crate::shell::shell_command_references_sensitive_path;
use crate::shell_parse::{
    destructive_shell_segment_reason, expand_wrapper_segments, is_read_only_shell_segment,
    path_has_unresolved_var, shell_segments,
};

/// Pre-AI structural classifier for shell commands. Runs unconditionally
/// between the policy verdict and the AI reviewer to short-circuit obvious
/// cases without paying an LLM round-trip — a structural allowlist plus a
/// dangerous-pattern check, layered before the LLM-driven classifier.
///
/// Tokenisation reuses the tree-sitter-bash backed segmenter
/// (`shell_segments` + `expand_wrapper_segments`) so wrapped commands like
/// `sh -c "rm -rf /"` or `env CI=1 python -c '...'` are inspected on their
/// real payload, not the wrapper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellPreClassification {
    /// All segments are structurally trivial reads (`ls`, `grep`, `cat`, …);
    /// the LLM reviewer round-trip is unnecessary.
    AutoAllow { reason: &'static str },
    /// At least one segment is a destructive verb, a dangerous interpreter
    /// (`python -c`, `node -e`, `eval`, `sudo`, …), or references a
    /// sensitive path. The runtime uses this classification to *raise
    /// permissive verdicts to an Ask floor* so the command cannot run
    /// silently; an existing Ask or Deny verdict is left unchanged.
    /// The LLM reviewer may still run after this classification and can
    /// override the resulting Ask, which keeps false-positive cases
    /// recoverable by approval rather than causing unrecoverable denial.
    RequiresApproval { reason: String },
    /// Ambiguous shape; fall through to the AI reviewer (or to the user
    /// prompt when the reviewer is disabled).
    AskAi,
}

pub fn pre_classify_shell(
    command: &str,
    shell_sandbox: &ShellSandboxConfig,
) -> ShellPreClassification {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return ShellPreClassification::AskAi;
    }
    if let Some(pattern) =
        shell_command_references_sensitive_path(trimmed, &shell_sandbox.sensitive_path_patterns)
    {
        return ShellPreClassification::RequiresApproval {
            reason: format!("references sensitive path pattern {pattern:?}"),
        };
    }
    let raw_segments = shell_segments(trimmed);
    if raw_segments.is_empty() {
        return ShellPreClassification::AskAi;
    }
    let segments = expand_wrapper_segments(raw_segments);
    for segment in &segments {
        if let Some(reason) = destructive_shell_segment_reason(segment) {
            return ShellPreClassification::RequiresApproval { reason };
        }
        if let Some(interpreter) = dangerous_interpreter(segment) {
            return ShellPreClassification::RequiresApproval {
                reason: format!("dangerous interpreter {interpreter:?}"),
            };
        }
    }
    if segments
        .iter()
        .all(|segment| is_read_only_shell_segment(segment))
    {
        return ShellPreClassification::AutoAllow {
            reason: "read-only shell verbs",
        };
    }
    ShellPreClassification::AskAi
}

/// Returns the interpreter name when `segment`'s argv head matches a
/// known arbitrary-code runner. Uses the *raw* first token rather than
/// `shell_command_prefix`, because the prefix folds `sudo`/`bash`/`env`
/// into the generic `"shell"` label and we lose the signal we need here.
/// Plain `python script.py` is *not* a dangerous interpreter (it runs a
/// vetted file on disk); only inline-code forms (`python -c '…'`,
/// `node -e '…'`) and elevation verbs (`sudo`, `doas`) are denied.
fn dangerous_interpreter(segment: &str) -> Option<&'static str> {
    let mut tokens = segment.split_whitespace();
    // Skip leading env-var assignments so `CI=1 python -c '…'` still
    // surfaces the interpreter.
    let head = loop {
        let tok = tokens.next()?;
        if tok.split_once('=').is_some_and(|(name, _)| {
            !name.is_empty()
                && name
                    .chars()
                    .all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
        }) {
            continue;
        }
        break tok;
    };
    // Normalize the head so path-qualified (`/usr/bin/python3`, `./python`)
    // and version-suffixed (`python3.11`) invocations are not trivially
    // mistaken for unknown programs. Strip directory components and, on
    // Windows, a trailing `.exe`.
    let program = interpreter_program_name(head);
    let interpreter = match program {
        "node" => "node",
        "deno" => "deno",
        "ruby" => "ruby",
        "perl" => "perl",
        "php" => "php",
        "lua" => "lua",
        "tclsh" => "tclsh",
        "osascript" => "osascript",
        "eval" => "eval",
        "exec" => "exec",
        "sudo" => "sudo",
        "doas" => "doas",
        // Python ships under many version-suffixed names
        // (`python`, `python2`, `python2.7`, `python3`, `python3.11`, …);
        // collapse them to the major-series label so inline-code forms
        // surface regardless of the suffix.
        other => python_series_label(other)?,
    };
    // `sudo`, `doas`, `eval`, `exec` are always elevation/arbitrary-code
    // verbs regardless of args, so deny on bare head match.
    if matches!(interpreter, "sudo" | "doas" | "eval" | "exec") {
        return Some(interpreter);
    }
    // Language interpreters only deny when invoked with an inline-code
    // flag; running a vetted script file (`python build.py`) is
    // structurally similar to `cargo build` and should fall through to
    // AskAi or the AI reviewer for context.
    let inline_code_flag = tokens.any(|tok| {
        matches!(
            tok,
            "-c" | "-e" | "-E" | "-m" | "--command" | "--eval" | "--code"
        )
    });
    if inline_code_flag {
        Some(interpreter)
    } else {
        None
    }
}

/// Reduces an argv head to its program name for interpreter/elevation
/// matching: strips POSIX (`/`) and Windows (`\`) directory components so
/// `/usr/bin/python3` and `.\sudo.exe` are matched the same as their bare
/// forms, and trims a trailing `.exe` (case-insensitive) so Windows
/// executable names line up with the lowercase table above. This keeps the
/// structural safety floor from being defeated by a leading path or an
/// `.exe` suffix.
fn interpreter_program_name(head: &str) -> &str {
    let base = head
        .rsplit(['/', '\\'])
        .next()
        .filter(|component| !component.is_empty())
        .unwrap_or(head);
    // Trim a trailing `.exe` case-insensitively (`.exe`, `.EXE`, `.Exe`, …)
    // without allocating in the common (no-suffix) path. Compare the final
    // four bytes directly so we never slice at a non-char boundary: a match
    // means those bytes are the ASCII `.exe` suffix, which is a valid cut.
    if let Some(cut) = base.len().checked_sub(4) {
        let suffix = &base.as_bytes()[cut..];
        if suffix.eq_ignore_ascii_case(b".exe") {
            return &base[..cut];
        }
    }
    base
}

/// Maps a Python program name to its major-series label, accepting the
/// version-suffixed names interpreters ship under (`python`, `python2`,
/// `python2.7`, `python3`, `python3.11`, …). Returns `None` for anything
/// that is not part of the Python family so callers can fall through.
fn python_series_label(program: &str) -> Option<&'static str> {
    if program == "python" {
        return Some("python");
    }
    // After the `python` prefix the remainder must be a series digit
    // (`2`/`3`) optionally followed by a dotted minor version
    // (`.7`, `.11`), e.g. `python3`, `python3.11`. This avoids matching
    // unrelated programs that merely begin with `python` (`pythonista`).
    let series = program.strip_prefix("python")?;
    let (major, rest) = series.split_at(series.find('.').unwrap_or(series.len()));
    if let Some(minor) = rest.strip_prefix('.') {
        if minor.is_empty() || !minor.chars().all(|ch| ch.is_ascii_digit()) {
            return None;
        }
    } else if !rest.is_empty() {
        return None;
    }
    match major {
        "2" => Some("python2"),
        "3" => Some("python3"),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathSafetyError {
    OutsideWritableRoots {
        path: PathBuf,
    },
    ProtectedMetadata {
        path: PathBuf,
        metadata_name: String,
    },
}

impl PathSafetyError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::OutsideWritableRoots { .. } => "patch_path_outside_roots",
            Self::ProtectedMetadata { .. } => "protected_metadata_path",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::OutsideWritableRoots { path } => {
                format!("patch target escapes writable roots: {}", path.display())
            }
            Self::ProtectedMetadata {
                path,
                metadata_name,
            } => format!(
                "path targets protected metadata directory {metadata_name}: {}",
                path.display()
            ),
        }
    }
}

/// Cross-platform OS temp directories treated as "safe to write" without a
/// permission prompt. Mirrors the shell sandbox's writable temp roots and
/// codex's workspace-write temp handling (cwd + `/tmp` + `$TMPDIR`), but is
/// available on every target — unlike the sandbox's `shell_writable_roots`,
/// which is compiled only on macOS/Linux.
pub(crate) fn temp_dir_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for name in ["TMPDIR", "TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(name) {
            roots.push(PathBuf::from(value));
        }
    }
    if cfg!(windows) {
        // `%LOCALAPPDATA%\Temp` is the canonical per-user temp when
        // `%TEMP%`/`%TMP%` are unset.
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            roots.push(PathBuf::from(local).join("Temp"));
        }
    } else {
        roots.push(PathBuf::from("/tmp"));
        // macOS resolves `/tmp` and the per-user `$TMPDIR` under `/private`.
        roots.push(PathBuf::from("/private/tmp"));
        roots.push(PathBuf::from("/private/var/folders"));
    }
    roots
}

/// Effective set of roots a command may write to without escalating to a
/// permission prompt: the workspace, OS temp dirs, and any configured
/// `write_roots`. Shared notion of "local/safe" between the permission
/// classifier and the shell sandbox so the two layers agree.
pub(crate) fn permission_writable_roots(
    workspace_root: &Path,
    shell_sandbox: &ShellSandboxConfig,
) -> Vec<PathBuf> {
    let mut roots = vec![workspace_root.to_path_buf()];
    roots.extend(temp_dir_roots());
    roots.extend(shell_sandbox.write_roots.iter().cloned());
    roots
}

/// True when `raw` resolves outside every permission-writable root
/// (workspace + temp + configured `write_roots`). Relative paths resolve
/// under the workspace; `..` traversal is normalized first so an in-bounds
/// relative path that climbs out (`../../etc/x`) is still caught.
pub(crate) fn path_escapes_permission_writable_roots(
    raw: &str,
    workspace_root: &Path,
    shell_sandbox: &ShellSandboxConfig,
) -> bool {
    // An unresolved shell variable (`$VAR`/`${VAR}`/`%VAR%` left after env
    // expansion) means we cannot prove the target stays in the workspace —
    // escalate rather than silently allow it.
    if path_has_unresolved_var(raw) {
        return true;
    }
    let normalized = normalize_candidate(raw, workspace_root);
    let roots = permission_writable_roots(workspace_root, shell_sandbox);
    !roots.iter().any(|root| normalized.starts_with(root))
}

pub fn assess_write_path(
    raw: &str,
    workspace_root: &Path,
    shell_sandbox: &ShellSandboxConfig,
) -> Result<PathBuf, PathSafetyError> {
    let normalized = normalize_candidate(raw, workspace_root);
    if !is_under_writable_root(&normalized, workspace_root, shell_sandbox) {
        return Err(PathSafetyError::OutsideWritableRoots { path: normalized });
    }
    if let Some(metadata_name) =
        protected_metadata_component(&normalized, workspace_root, shell_sandbox)
    {
        return Err(PathSafetyError::ProtectedMetadata {
            path: normalized,
            metadata_name,
        });
    }
    Ok(normalized)
}

pub fn path_targets_protected_metadata(
    path: &Path,
    workspace_root: &Path,
    shell_sandbox: &ShellSandboxConfig,
) -> Option<String> {
    protected_metadata_component(path, workspace_root, shell_sandbox)
}

fn normalize_candidate(raw: &str, workspace_root: &Path) -> PathBuf {
    let raw_path = Path::new(raw);
    let mut path = if raw_path.is_absolute() {
        PathBuf::new()
    } else {
        workspace_root.to_path_buf()
    };
    for component in raw_path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                path.pop();
            }
            Component::RootDir | Component::Prefix(_) => {
                path.push(component.as_os_str());
            }
            Component::Normal(part) => path.push(part),
        }
    }
    path
}

fn is_under_writable_root(
    path: &Path,
    workspace_root: &Path,
    shell_sandbox: &ShellSandboxConfig,
) -> bool {
    path.starts_with(workspace_root)
        || shell_sandbox
            .write_roots
            .iter()
            .any(|root| path.starts_with(root))
}

fn protected_metadata_component(
    path: &Path,
    workspace_root: &Path,
    shell_sandbox: &ShellSandboxConfig,
) -> Option<String> {
    if shell_sandbox.protected_metadata_names.is_empty() {
        return None;
    }
    let roots = std::iter::once(workspace_root)
        .chain(shell_sandbox.write_roots.iter().map(PathBuf::as_path));
    for root in roots {
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        for component in relative.components() {
            let Component::Normal(part) = component else {
                continue;
            };
            let Some(part) = part.to_str() else {
                continue;
            };
            if shell_sandbox
                .protected_metadata_names
                .iter()
                .any(|name| name == part)
            {
                return Some(part.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
#[path = "safety_tests.rs"]
mod tests;
