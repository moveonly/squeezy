//! Lightweight mtime-poll watcher for the three settings.toml tiers (user,
//! project, local). The TUI's `/options` save path already calls
//! `reload_sources_and_agent` when the user edits settings from inside the
//! app; this watcher closes the gap for *external* edits (sed, $EDITOR, a
//! separate Squeezy process, …) so a running session doesn't need a restart
//! to pick up file changes.
//!
//! Polling rather than `notify`-style filesystem events keeps the
//! dependency footprint flat and is plenty for a file that changes maybe
//! once per minute; we already wake every tick to redraw, so checking three
//! `stat()` calls every ~1s is negligible.

use std::path::PathBuf;
use std::time::SystemTime;

use squeezy_core::load_separated_settings_sources;

#[derive(Debug, Default)]
pub(crate) struct SettingsWatcher {
    files: Vec<WatchedFile>,
}

#[derive(Debug)]
struct WatchedFile {
    path: PathBuf,
    mtime: Option<SystemTime>,
}

impl SettingsWatcher {
    /// Snapshot the current mtimes for the three tier paths so the first
    /// `poll()` only reports a change when the file actually moves.
    pub(crate) fn new() -> Self {
        let paths = tier_paths();
        let files = paths
            .into_iter()
            .map(|path| {
                let mtime = current_mtime(&path);
                WatchedFile { path, mtime }
            })
            .collect();
        Self { files }
    }

    /// Returns `true` when any tracked file's mtime moved (or the file
    /// appeared / disappeared) since the previous call. Updates the cached
    /// mtimes in place so consecutive polls don't keep re-firing on the
    /// same edit.
    pub(crate) fn poll(&mut self) -> bool {
        let mut changed = false;
        for file in &mut self.files {
            let current = current_mtime(&file.path);
            if current != file.mtime {
                file.mtime = current;
                changed = true;
            }
        }
        changed
    }
}

fn tier_paths() -> Vec<PathBuf> {
    match load_separated_settings_sources() {
        Ok(sources) => vec![
            sources.user_path_default,
            sources.project_path_default,
            sources.repo_path_default,
        ],
        // If we can't even compute the paths there's nothing to watch yet;
        // a later poll won't recover, but the existing behaviour was to
        // require a restart, so failing closed is fine.
        Err(_) => Vec::new(),
    }
}

fn current_mtime(path: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
}

#[cfg(test)]
#[path = "settings_watcher_tests.rs"]
mod tests;
