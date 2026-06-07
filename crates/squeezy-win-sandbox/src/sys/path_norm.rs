//! Path canonicalization helpers for key derivation and comparisons.

use std::path::{Path, PathBuf};

/// Canonicalize `path` using the standard library and strip any leading
/// `\\?\` verbatim prefix that the standard library adds on Windows.
pub(crate) fn canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    let canon = std::fs::canonicalize(path)?;
    let s = canon.to_string_lossy();
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        Ok(PathBuf::from(stripped))
    } else {
        Ok(canon)
    }
}

/// Best-effort canonical key: lower-case, forward slashes.
///
/// Falls back to the lossy string representation when canonicalization fails
/// (e.g. path does not exist yet).
pub(crate) fn canonical_key(path: &Path) -> String {
    let base = match canonicalize(path) {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(_) => path.to_string_lossy().into_owned(),
    };
    base.replace('\\', "/").to_ascii_lowercase()
}
