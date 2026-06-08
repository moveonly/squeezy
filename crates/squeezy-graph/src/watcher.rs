//! Cross-platform file-system watcher feeding [`crate::GraphManager`].
//!
//! The watcher runs in a background thread and groups rapid-succession
//! events into a debounce window; when the window closes, the registered
//! callback receives a [`ChangeBatch`] of absolute paths that were
//! created/modified plus paths that were removed. The graph manager's
//! `pending_changed_paths` set is the natural consumer: the watcher
//! callback acquires the mutex, pushes paths, and the next
//! `refresh_before_query` drains them.
//!
//! Decoupled from [`crate::GraphManager`] so callers can also drive
//! the watcher into a different sink (e.g. an MCP gateway that wants to
//! buffer paths before scheduling a refresh).
//!
//! Uses `notify-debouncer-full` which delegates to the OS-native backend:
//!  - macOS: FSEvents
//!  - Linux: inotify
//!  - Windows: ReadDirectoryChangesW
//!
//! No daemon, no IPC: dropping the [`FileWatcher`] stops both the OS
//! watch and the debouncer thread.

use std::any::Any;
use std::path::PathBuf;
use std::time::Duration;

use notify_debouncer_full::notify::{EventKind, RecursiveMode};
use notify_debouncer_full::{DebounceEventResult, new_debouncer};
use squeezy_core::{Result, SqueezyError};
use tracing::warn;

/// Configuration for a [`FileWatcher`].
#[derive(Debug, Clone)]
pub struct WatcherConfig {
    /// Source directories to watch recursively.
    pub src_dirs: Vec<PathBuf>,
    /// Debounce window in milliseconds. The watcher waits this long after
    /// the last event before firing the callback; events in the window
    /// merge into one [`ChangeBatch`].
    pub debounce_ms: u64,
}

/// Default debounce window. Long enough that an editor "save all" or a
/// branch switch coalesces into one batch; short enough that the next
/// query sees the change without a perceptible wait.
pub const DEFAULT_DEBOUNCE_MS: u64 = 10_000;

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            src_dirs: Vec::new(),
            debounce_ms: DEFAULT_DEBOUNCE_MS,
        }
    }
}

/// Native watcher backend used on this platform.
pub const fn native_backend_name() -> &'static str {
    if cfg!(target_os = "linux") {
        "inotify"
    } else if cfg!(target_os = "macos") {
        "fsevents"
    } else if cfg!(target_os = "windows") {
        "read_directory_changes"
    } else {
        "native"
    }
}

/// Polling fallback backend used when the native watcher cannot be registered.
pub const fn polling_backend_name() -> &'static str {
    "polling"
}

/// Batch of file-system changes delivered when the debounce window expires.
#[derive(Debug, Default, Clone)]
pub struct ChangeBatch {
    /// Files that were created or whose content was modified. Absolute,
    /// sorted, deduplicated.
    pub modified: Vec<PathBuf>,
    /// Files that were deleted. Absolute, sorted, deduplicated.
    pub removed: Vec<PathBuf>,
}

impl ChangeBatch {
    pub fn is_empty(&self) -> bool {
        self.modified.is_empty() && self.removed.is_empty()
    }
}

/// Running watcher. Drops the OS watch and debouncer thread when dropped.
pub struct FileWatcher {
    // Type-erased so the concrete Debouncer type (which varies per OS
    // backend) does not leak into the public API.
    _debouncer: Box<dyn Any + Send>,
}

impl FileWatcher {
    /// Start watching the directories in `config`. The callback is invoked
    /// from a background thread when a debounced batch is ready. Returns
    /// an error if the OS watcher cannot be started or any source
    /// directory cannot be registered.
    pub fn start<F>(config: WatcherConfig, on_change: F) -> Result<Self>
    where
        F: Fn(ChangeBatch) + Send + 'static,
    {
        let timeout = Duration::from_millis(config.debounce_ms);

        let mut debouncer = new_debouncer(
            timeout,
            None, // auto tick-rate
            move |result: DebounceEventResult| {
                if let Some(batch) = handle_debounce_result(result) {
                    on_change(batch);
                }
            },
        )
        .map_err(|err| SqueezyError::Tool(format!("watcher: failed to start debouncer: {err}")))?;

        for dir in &config.src_dirs {
            // Canonicalise to resolve OS-level symlinks (e.g. /var → /private/var
            // on macOS) so paths reported by the OS event callback match the
            // absolute paths squeezy compares against elsewhere.
            let real_dir = dir.canonicalize().unwrap_or_else(|_| dir.clone());
            debouncer
                .watch(&real_dir, RecursiveMode::Recursive)
                .map_err(|err| {
                    SqueezyError::Tool(format!("watcher: failed to watch {}: {err}", dir.display()))
                })?;
        }

        Ok(Self {
            _debouncer: Box::new(debouncer),
        })
    }

    /// Start a polling watcher. This is slower than the OS-native backend, but
    /// it keeps long-lived indexing alive when native registration fails, for
    /// example when Linux inotify watch limits are exhausted or a recursive
    /// watch cannot be installed on a FUSE/NFS mount.
    pub fn start_polling<F>(config: WatcherConfig, on_change: F) -> Result<Self>
    where
        F: Fn(ChangeBatch) + Send + 'static,
    {
        let timeout = Duration::from_millis(config.debounce_ms);
        // Poll every 50 ms by default, but never faster than the debounce
        // window (minimum 1 ms) so a very short debounce does not spin the
        // poll loop.
        let poll_interval = timeout.clamp(Duration::from_millis(1), Duration::from_millis(50));
        let mut debouncer = notify_debouncer_full::new_debouncer_opt::<
            _,
            notify_debouncer_full::notify::PollWatcher,
            notify_debouncer_full::RecommendedCache,
        >(
            timeout,
            None,
            move |result: DebounceEventResult| {
                if let Some(batch) = handle_debounce_result(result) {
                    on_change(batch);
                }
            },
            notify_debouncer_full::RecommendedCache::new(),
            notify_debouncer_full::notify::Config::default()
                .with_poll_interval(poll_interval)
                .with_compare_contents(true),
        )
        .map_err(|err| {
            SqueezyError::Tool(format!("watcher: failed to start poll debouncer: {err}"))
        })?;

        for dir in &config.src_dirs {
            let real_dir = dir.canonicalize().unwrap_or_else(|_| dir.clone());
            debouncer
                .watch(&real_dir, RecursiveMode::Recursive)
                .map_err(|err| {
                    SqueezyError::Tool(format!("watcher: failed to watch {}: {err}", dir.display()))
                })?;
        }

        Ok(Self {
            _debouncer: Box::new(debouncer),
        })
    }
}

fn handle_debounce_result(result: DebounceEventResult) -> Option<ChangeBatch> {
    let events = match result {
        Ok(evs) => evs,
        Err(errs) => {
            for err in errs {
                warn!("squeezy-graph watcher error: {err:?}");
            }
            return None;
        }
    };

    let mut all_paths: Vec<PathBuf> = Vec::new();
    for event in events {
        classify_event(event.event.kind, &event.event.paths, &mut all_paths);
    }
    all_paths.sort_unstable();
    all_paths.dedup();

    let mut modified = Vec::new();
    let mut removed = Vec::new();
    for path in all_paths {
        // Partition by existence rather than EventKind: on macOS FSEvents
        // may fire a Modify event for a deletion (the file is already gone
        // when we check). The post-debounce existence check is the most
        // portable signal.
        //
        // Do NOT canonicalize event paths here. FileWatcher::start already
        // registers the canonical watched root, so notify reports paths that
        // are already rooted at the canonical root. Calling canonicalize() on
        // individual entries would silently resolve internal workspace symlinks
        // to their targets, breaking refresh attribution for files that the
        // crawler records under their symlink spelling.
        if path.exists() {
            modified.push(path);
        } else {
            removed.push(path);
        }
    }

    let batch = ChangeBatch { modified, removed };
    if batch.is_empty() { None } else { Some(batch) }
}

fn classify_event(kind: EventKind, paths: &[PathBuf], all_paths: &mut Vec<PathBuf>) {
    match kind {
        EventKind::Access(_) | EventKind::Other => {}
        _ => {
            for path in paths {
                all_paths.push(path.clone());
            }
        }
    }
}

#[cfg(test)]
#[path = "watcher_tests.rs"]
mod tests;
