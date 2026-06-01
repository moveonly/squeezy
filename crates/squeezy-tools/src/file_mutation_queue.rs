//! Per-realpath mutation queue.
//!
//! `write_file`, `apply_patch`, and `notebook_edit` historically advertised
//! themselves as non-parallel-safe so the agent serialised every mutation
//! tool call. That blanket serialisation is wasteful when the calls touch
//! disjoint files. This module exposes a finer-grained mutex keyed on the
//! canonical (symlink-resolved) target path: mutations against the same
//! realpath serialise, mutations against distinct realpaths run
//! concurrently.
//!
//! The key derivation:
//!
//! 1. Canonicalise the absolute path. If the file already exists, the
//!    symlink chain is resolved so a write to `link.txt` and a write to its
//!    target collapse onto the same lock.
//! 2. If canonicalisation fails (typically `ENOENT` because the file does
//!    not exist yet), canonicalise the parent directory and re-attach the
//!    file name. This catches the create-file case where the parent exists
//!    but the leaf does not.
//! 3. Otherwise fall back to the supplied absolute path verbatim.
//!
//! Locks are acquired in deterministic (sorted) order so concurrent
//! multi-path callers (`apply_patch` with overlapping operation sets)
//! cannot deadlock on each other.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex as StdMutex},
};

use tokio::sync::{Mutex, OwnedMutexGuard};

/// Process-wide map from realpath → mutex. Lookups acquire the outer
/// `StdMutex` only long enough to fetch/insert the inner `Arc<Mutex<()>>`
/// — the actual wait happens on the inner async mutex.
static MUTATION_LOCKS: LazyLock<StdMutex<HashMap<PathBuf, Arc<Mutex<()>>>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

/// Bundle of owned async-mutex guards held for the lifetime of a mutation.
/// Dropping the bundle releases every guard it owns.
#[must_use = "the guards must stay alive for the duration of the mutation"]
pub(crate) struct MutationGuards {
    _guards: Vec<OwnedMutexGuard<()>>,
}

impl MutationGuards {
    #[cfg(test)]
    pub(crate) fn held_count(&self) -> usize {
        self._guards.len()
    }
}

/// Resolve every supplied path to its realpath key and acquire the
/// corresponding per-realpath async mutex. Returns once every lock has been
/// taken. Locks are acquired in sorted-key order so two concurrent callers
/// with overlapping path sets serialise in a predictable order instead of
/// deadlocking on each other.
///
/// Inputs are expected to be absolute paths (e.g. the result of
/// `ToolRegistry::resolve_for_write`); relative paths still work but lose
/// the symlink-collapsing guarantee because canonicalisation needs an
/// absolute anchor.
pub(crate) async fn lock_paths_for_mutation<I, P>(paths: I) -> MutationGuards
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    let mut keys = {
        let paths = paths.into_iter();
        let mut keys = Vec::with_capacity(paths.size_hint().0);
        for path in paths {
            keys.push(mutation_key(path.as_ref()));
        }
        keys
    };
    keys.sort_unstable();
    keys.dedup();

    let mut guards = Vec::with_capacity(keys.len());
    for key in keys {
        let lock = acquire_named_lock(&key);
        guards.push(lock.lock_owned().await);
    }
    MutationGuards { _guards: guards }
}

fn acquire_named_lock(key: &Path) -> Arc<Mutex<()>> {
    let mut map = MUTATION_LOCKS.lock().unwrap_or_else(|err| err.into_inner());
    if let Some(lock) = map.get(key) {
        return Arc::clone(lock);
    }
    let lock = Arc::new(Mutex::new(()));
    map.insert(key.to_path_buf(), Arc::clone(&lock));
    lock
}

/// Compute the deterministic lock key for a mutation target. See the module
/// docs for the full resolution order.
pub(crate) fn mutation_key(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }
    if let (Some(parent), Some(file_name)) = (path.parent(), path.file_name())
        && let Ok(mut canonical_parent) = std::fs::canonicalize(parent)
    {
        canonical_parent.push(file_name);
        return canonical_parent;
    }
    path.to_path_buf()
}

#[cfg(test)]
#[path = "file_mutation_queue_tests.rs"]
mod tests;
