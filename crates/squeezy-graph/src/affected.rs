//! Affected-set computation for incremental refresh (Item 3).
//!
//! Today's `refresh_now` rebuilds every semantic edge after any change.
//! With the per-file reverse-import index [`crate::SemanticGraph::importers_by_file`]
//! we can compute, given a set of changed files, the strict super-set of
//! files whose resolver outputs could differ from the last run: every
//! changed file itself, plus every file reachable through reverse-import
//! edges from a "propagating" change (one that altered the file's
//! [`crate::cross_file::ExportTable`] or removed the file).
//!
//! This module hosts the pure computation. Wiring it through
//! `refresh_now` so the resolver only re-runs over the affected set
//! lands in Item 3 PR-3 — the read here has no consumer yet.

use std::collections::{HashMap, HashSet};

use squeezy_core::FileId;

/// Compute the affected file set per the algorithm in the plan:
///
/// ```text
/// propagating = { f in changed : new ExportTable(f) != cached ExportTable(f) }
///                 ∪ removed_files
/// affected    = changed ∪ reverse_reachable(importers_by_file, propagating)
/// ```
///
/// `changed` is every file the refresh ran over (parsed anew or otherwise
/// observed to differ). `propagating` is the subset whose exports were
/// observed to have changed — only those need to push downstream
/// invalidation. `removed_files` are always propagating.
pub fn compute_affected(
    changed: &HashSet<FileId>,
    importers_by_file: &HashMap<FileId, Vec<FileId>>,
    propagating: &HashSet<FileId>,
    removed_files: &HashSet<FileId>,
) -> HashSet<FileId> {
    let mut affected: HashSet<FileId> = HashSet::with_capacity(changed.len() + removed_files.len());
    affected.extend(changed.iter().cloned());
    affected.extend(removed_files.iter().cloned());

    let mut frontier: Vec<FileId> = propagating
        .iter()
        .chain(removed_files.iter())
        .cloned()
        .collect();
    let mut visited: HashSet<FileId> = HashSet::with_capacity(frontier.len());
    while let Some(file) = frontier.pop() {
        if !visited.insert(file.clone()) {
            continue;
        }
        if let Some(importers) = importers_by_file.get(&file) {
            for importer in importers {
                if !visited.contains(importer) {
                    frontier.push(importer.clone());
                }
            }
        }
        affected.insert(file);
    }
    affected
}

#[cfg(test)]
#[path = "affected_tests.rs"]
mod tests;
