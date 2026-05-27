use std::path::{Component, Path, PathBuf};

use squeezy_core::ShellSandboxConfig;

use crate::shell::shell_command_references_sensitive_path;
use crate::shell_parse::{
    expand_wrapper_segments, is_destructive_shell_segment, is_read_only_shell_segment,
    shell_segments,
};

/// Pre-AI structural classifier for shell commands. Runs unconditionally
/// between the policy verdict and the AI reviewer to short-circuit obvious
/// cases without paying an LLM round-trip. Mirrors clear-code's
/// `isAutoModeAllowlistedTool` + dangerous-pattern stripper layered before
/// the yolo classifier.
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
    /// sensitive path. The LLM reviewer cannot be trusted to override this
    /// structural denial.
    AutoDeny { reason: String },
    /// Ambiguous shape; fall through to the AI reviewer (or to the user
    /// prompt when the reviewer is disabled).
    AskAi,
}

/// Interpreter executables that can run arbitrary code from an inline
/// argument (`-c`, `-e`, etc.) or stdin. Approving a single Ask
/// short-circuit for these effectively approves anything the wrapped
/// language can do, so the pre-classifier always denies them here and
/// forces a per-call prompt.
const DANGEROUS_INTERPRETERS: &[&str] = &[
    "python",
    "python2",
    "python3",
    "node",
    "deno",
    "ruby",
    "perl",
    "php",
    "lua",
    "tclsh",
    "osascript",
    "eval",
    "exec",
    "sudo",
    "doas",
];

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
        return ShellPreClassification::AutoDeny {
            reason: format!("references sensitive path pattern {pattern:?}"),
        };
    }
    let raw_segments = shell_segments(trimmed);
    if raw_segments.is_empty() {
        return ShellPreClassification::AskAi;
    }
    let segments = expand_wrapper_segments(raw_segments);
    for segment in &segments {
        if is_destructive_shell_segment(segment) {
            let label = segment
                .split_whitespace()
                .next()
                .unwrap_or(segment.as_str());
            return ShellPreClassification::AutoDeny {
                reason: format!("destructive verb {label:?}"),
            };
        }
        if let Some(interpreter) = dangerous_interpreter(segment) {
            return ShellPreClassification::AutoDeny {
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
    let interpreter = DANGEROUS_INTERPRETERS
        .iter()
        .copied()
        .find(|name| *name == head)?;
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
