use std::path::{Component, Path, PathBuf};

use squeezy_core::ShellSandboxConfig;

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
