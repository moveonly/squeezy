//! Local persistence layer for Squeezy.
//!
//! This crate hosts independent on-disk stores that share little code beyond a
//! few small helpers, but live together because both are part of the local-state
//! surface (and so consumers can reach them through a single `squeezy-store`
//! dependency).
//!
//! * `repo_profile` - generated per-repo facts (`~/.squeezy/repos.toml`).
//! * `sessions` - per-session metadata and event logs.
//! * `state.redb` - receipt metadata, read snapshots (keyed by
//!   `(path, start_byte, end_byte)` so distinct windows of the same file do not
//!   overwrite each other), observations, and small session-side cache state.
//! * `graph/v3` - sharded semantic graph partitions and resolver-cache
//!   snapshots. Legacy `graph.redb` is treated as a best-effort warm-start
//!   source only.

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use redb::{
    Database, ReadOnlyDatabase, ReadableDatabase, ReadableTable, ReadableTableMetadata,
    TableDefinition,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use squeezy_core::{FileId, LanguageKind, Result, SqueezyError, repo_settings_id};

mod fs_util;
pub mod memory;
pub mod migrations;
pub mod repo_profile;
pub mod reports;
pub mod sessions;

pub use fs_util::user_squeezy_dir_detail;
pub use migrations::{
    Migration, MigrationRegistry, default_registry, run_migrations, run_registry,
};
pub use repo_profile::*;
pub use reports::*;
pub use sessions::*;

pub const CRATE_NAME: &str = "squeezy-store";
pub const SCHEMA_VERSION: u64 = 3;
pub const GRAPH_SCHEMA_VERSION: u64 = 1;
pub const GRAPH_V3_SCHEMA_VERSION: u64 = 3;
pub const STATE_FILE_NAME: &str = "state.redb";
pub const GRAPH_FILE_NAME: &str = "graph.redb";
pub const GRAPH_DIR_NAME: &str = "graph";
pub const GRAPH_V3_DIR_NAME: &str = "v3";
pub const GRAPH_MANIFEST_FILE_NAME: &str = "manifest.redb";
pub const GRAPH_GLOBAL_FILE_NAME: &str = "global.redb";
pub const GRAPH_SHARDS_DIR_NAME: &str = "shards";
pub const GRAPH_SHARD_COUNT: u8 = 64;

const OVERSIZED_STATE_FAST_ROTATE_BYTES: u64 = 256 * 1024 * 1024;

#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;

const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const GRAPH_MANIFEST: TableDefinition<&str, &[u8]> = TableDefinition::new("graph_manifest");
const GRAPH_FILE_SHARDS: TableDefinition<&str, &[u8]> = TableDefinition::new("graph_file_shards");
const GRAPH_PARTITIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("graph_partitions");
const TOOL_RECEIPTS: TableDefinition<&str, &[u8]> = TableDefinition::new("tool_receipts");
const READ_SNAPSHOTS: TableDefinition<&str, &[u8]> = TableDefinition::new("read_snapshots");
const MCP_TOOL_CACHE: TableDefinition<&str, &[u8]> = TableDefinition::new("mcp_tool_cache");
const OBSERVATIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("observations");
const OBSERVATION_INDEX: TableDefinition<&str, &[u8]> = TableDefinition::new("observation_index");
const COMPACTION_CHECKPOINTS: TableDefinition<&str, &[u8]> =
    TableDefinition::new("compaction_checkpoints");
/// Per-file resolver snapshot: exports, imports, supertypes, builder
/// snapshot. Keyed by `FileId.0`. Sits alongside `GRAPH_PARTITIONS`,
/// which already caches per-file parse output; this table caches the
/// resolver-layer derivatives so warm start after a process restart can
/// reuse the previous run's cross-file work.
const RESOLVER_SNAPSHOT_PER_FILE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("resolver_snapshot_per_file");
/// Single-blob persistent snapshot of the file-level import adjacency
/// graph that the phased scheduler discovers. Stored under one key
/// (`"resolver_import_graph"`) so the import adjacency loads in one
/// table read instead of a per-file scan.
const RESOLVER_IMPORT_GRAPH: TableDefinition<&str, &[u8]> =
    TableDefinition::new("resolver_import_graph");

/// Default retention for `compaction_checkpoints`. Mirrors the VCS
/// checkpoint TTL; intentionally duplicated so this crate does not depend
/// on `squeezy-vcs`.
pub const DEFAULT_COMPACTION_CHECKPOINT_RETENTION_DAYS: u64 = 7;

pub fn crate_name() -> &'static str {
    CRATE_NAME
}

#[derive(Debug)]
pub struct SqueezyStore {
    path: PathBuf,
    database: Database,
    graph_store: Mutex<Option<Arc<GraphStore>>>,
}

#[derive(Debug)]
pub struct GraphStore {
    path: PathBuf,
    backend: GraphStoreBackend,
}

#[derive(Debug)]
enum GraphStoreBackend {
    V3(GraphStoreV3),
}

#[derive(Debug)]
struct GraphStoreV3 {
    root: PathBuf,
    legacy_path: PathBuf,
    manifest: Database,
    global: Database,
    shards: Mutex<BTreeMap<GraphShardKey, Arc<Database>>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphStoreProbe {
    pub path: PathBuf,
    pub schema_version: Option<u64>,
}

#[derive(Debug)]
pub struct WorkspaceStores {
    workspace_root: PathBuf,
    cache_root: Option<PathBuf>,
    state: Mutex<Option<Arc<SqueezyStore>>>,
    graph: Mutex<Option<Arc<GraphStore>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreOpenFailureKind {
    Locked,
    PermissionDenied,
    DiskFull,
    Corrupt,
    UnsupportedFs,
    Other,
}

impl StoreOpenFailureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Locked => "locked",
            Self::PermissionDenied => "permission_denied",
            Self::DiskFull => "disk_full",
            Self::Corrupt => "corrupt",
            Self::UnsupportedFs => "unsupported_fs",
            Self::Other => "other",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Self::Locked => "likely lock contention",
            Self::PermissionDenied => "likely permission problem",
            Self::DiskFull => "likely disk full",
            Self::Corrupt => "possible redb corruption",
            Self::UnsupportedFs => "possible unsupported filesystem behavior",
            Self::Other => "storage open failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreOpenFailure {
    pub path: PathBuf,
    pub kind: StoreOpenFailureKind,
    pub message: String,
}

impl StoreOpenFailure {
    pub fn new(
        path: impl Into<PathBuf>,
        error: &SqueezyError,
        access_denied_may_be_lock: bool,
    ) -> Self {
        let message = error.to_string();
        Self {
            path: path.into(),
            kind: classify_store_open_message(&message, access_denied_may_be_lock),
            message,
        }
    }
}

impl std::fmt::Display for StoreOpenFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} ({}) at {}",
            self.message,
            self.kind.hint(),
            self.path.display()
        )
    }
}

impl std::error::Error for StoreOpenFailure {}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct GraphShardKey {
    language: String,
    bucket: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct GraphManifest {
    schema_version: u64,
    graph_format_version: u64,
    active_generation: u64,
    shard_count: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct GraphShardPlacement {
    language: String,
    bucket: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct VersionedGraphPayload {
    generation: u64,
    payload: Vec<u8>,
}

#[derive(Debug)]
struct GraphPartitionWrite {
    file_id: FileId,
    language: LanguageKind,
    encoded: Vec<u8>,
}

#[derive(Debug)]
struct GraphResolverWrite {
    file_id: FileId,
    language: LanguageKind,
    encoded: Vec<u8>,
}

impl WorkspaceStores {
    pub fn new(workspace_root: impl Into<PathBuf>, cache_root: Option<PathBuf>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            cache_root,
            state: Mutex::new(None),
            graph: Mutex::new(None),
        }
    }

    pub fn state(&self) -> std::result::Result<Arc<SqueezyStore>, StoreOpenFailure> {
        {
            let cached = self
                .state
                .lock()
                .map_err(|_| self.poisoned_failure(self.state_path(), "state store lock"))?;
            if let Some(store) = cached.as_ref() {
                return Ok(Arc::clone(store));
            }
        }
        let store = SqueezyStore::open(&self.workspace_root, self.cache_root.as_deref())
            .map(Arc::new)
            .map_err(|error| StoreOpenFailure::new(self.state_path(), &error, false))?;
        let mut cached = self
            .state
            .lock()
            .map_err(|_| self.poisoned_failure(self.state_path(), "state store lock"))?;
        if let Some(existing) = cached.as_ref() {
            Ok(Arc::clone(existing))
        } else {
            *cached = Some(Arc::clone(&store));
            Ok(store)
        }
    }

    pub fn graph(&self) -> std::result::Result<Arc<GraphStore>, StoreOpenFailure> {
        {
            let cached = self.graph.lock().map_err(|_| {
                self.poisoned_failure(self.graph_manifest_path(), "graph store lock")
            })?;
            if let Some(store) = cached.as_ref() {
                return Ok(Arc::clone(store));
            }
        }
        let store = GraphStore::open(&self.workspace_root, self.cache_root.as_deref())
            .map(Arc::new)
            .map_err(|error| StoreOpenFailure::new(self.graph_manifest_path(), &error, true))?;
        let mut cached = self
            .graph
            .lock()
            .map_err(|_| self.poisoned_failure(self.graph_manifest_path(), "graph store lock"))?;
        if let Some(existing) = cached.as_ref() {
            Ok(Arc::clone(existing))
        } else {
            *cached = Some(Arc::clone(&store));
            Ok(store)
        }
    }

    pub fn state_path(&self) -> PathBuf {
        state_path(&self.workspace_root, self.cache_root.as_deref())
    }

    pub fn graph_manifest_path(&self) -> PathBuf {
        graph_manifest_path(&self.workspace_root, self.cache_root.as_deref())
    }

    fn poisoned_failure(&self, path: PathBuf, label: &str) -> StoreOpenFailure {
        let error = SqueezyError::Tool(format!("{label} poisoned"));
        StoreOpenFailure::new(path, &error, false)
    }
}

pub fn classify_store_open_error(
    error: &SqueezyError,
    access_denied_may_be_lock: bool,
) -> StoreOpenFailureKind {
    classify_store_open_message(&error.to_string(), access_denied_may_be_lock)
}

pub fn classify_store_open_message(
    message: &str,
    access_denied_may_be_lock: bool,
) -> StoreOpenFailureKind {
    let lower = message.to_ascii_lowercase();
    if lower.contains("database already open")
        || lower.contains("cannot acquire lock")
        || lower.contains("lock")
        || lower.contains("busy")
        || lower.contains("would block")
        || lower.contains("being used by another process")
        || lower.contains("another process has locked")
        || lower.contains("sharing violation")
        || (access_denied_may_be_lock && lower.contains("access is denied"))
    {
        StoreOpenFailureKind::Locked
    } else if lower.contains("permission denied")
        || lower.contains("access denied")
        || lower.contains("access is denied")
    {
        StoreOpenFailureKind::PermissionDenied
    } else if lower.contains("no space") || lower.contains("enospc") {
        StoreOpenFailureKind::DiskFull
    } else if lower.contains("corrupt")
        || lower.contains("checksum")
        || lower.contains("invalid database")
        || lower.contains("invalid magic")
    {
        StoreOpenFailureKind::Corrupt
    } else if lower.contains("unsupported")
        || lower.contains("operation not supported")
        || lower.contains("not supported")
    {
        StoreOpenFailureKind::UnsupportedFs
    } else {
        StoreOpenFailureKind::Other
    }
}

fn is_locked_store_error(error: &SqueezyError) -> bool {
    classify_store_open_error(error, true) == StoreOpenFailureKind::Locked
}

impl SqueezyStore {
    pub fn open(workspace_root: impl AsRef<Path>, cache_root: Option<&Path>) -> Result<Self> {
        let workspace_root = workspace_root.as_ref();
        let path = state_path(workspace_root, cache_root);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if oversized_state_needs_fast_rotate(&path)? {
            let backup = backup_path_with_label(&path, "oversized-state");
            match fs_util::rotate_file(&path, &backup) {
                Ok(()) => {
                    sync_parent_dir(&backup)?;
                    bootstrap_store(workspace_root, cache_root)?;
                    tracing::warn!(
                        target: "squeezy::store",
                        threshold_bytes = OVERSIZED_STATE_FAST_ROTATE_BYTES,
                        backup = %backup.display(),
                        "state.redb exceeded the split-cache threshold; existing cache backed up without opening redb",
                    );
                }
                Err(rotate_err) => {
                    // On Windows, a file indexer or AV scanner may hold the
                    // file open, making rename fail. Continue with the
                    // oversized store rather than refusing to start.
                    tracing::warn!(
                        target: "squeezy::store",
                        error = %rotate_err,
                        "state.redb is oversized but rotation failed; continuing with existing store",
                    );
                }
            }
            let database = open_database(&path)?;
            return Ok(Self {
                path,
                database,
                graph_store: Mutex::new(None),
            });
        }
        let initial = open_database(&path)?;
        // Three cases:
        //   * On-disk schema already at target → reuse the open handle.
        //   * On-disk schema at any other version (older or newer) → back up
        //     the file, warn so the reset is observable, and fall through to
        //     bootstrap, which reinitialises the state-only schema and copies
        //     non-graph rows from the backup.
        //   * No schema stamped yet → bootstrap to the target version.
        let database = match current_schema_version(&initial)? {
            Some(SCHEMA_VERSION) => initial,
            Some(on_disk_version) => {
                drop(initial);
                let backup = backup_path(&path, on_disk_version);
                match fs_util::rotate_file(&path, &backup) {
                    Ok(()) => {
                        sync_parent_dir(&backup)?;
                        bootstrap_store(workspace_root, cache_root)?;
                        copy_state_tables(&backup, &path)?;
                        tracing::warn!(
                            target: "squeezy::store",
                            on_disk_version,
                            schema_version = SCHEMA_VERSION,
                            backup = %backup.display(),
                            "state.redb schema mismatch; existing store backed up and reinitialised",
                        );
                    }
                    Err(rotate_err) => {
                        // The plain `fs::rename` rotation above goes through
                        // Win32 `MoveFileExW` *without* the `\\?\` extended-path
                        // prefix, so it loses on paths past MAX_PATH. Retry via
                        // `fs_util::replace_file`, which adds the prefix (and
                        // `MOVEFILE_REPLACE_EXISTING`). When that succeeds we
                        // still get a backup + table copy; only when both
                        // attempts fail do we fall through to the destructive
                        // reset.
                        match fs_util::replace_file(&path, &backup) {
                            Ok(()) => {
                                sync_parent_dir(&backup)?;
                                bootstrap_store(workspace_root, cache_root)?;
                                copy_state_tables(&backup, &path)?;
                                tracing::warn!(
                                    target: "squeezy::store",
                                    on_disk_version,
                                    schema_version = SCHEMA_VERSION,
                                    backup = %backup.display(),
                                    rotate_error = %rotate_err,
                                    "state.redb schema mismatch; rename rotation failed but extended-path replace succeeded; existing store backed up and reinitialised",
                                );
                            }
                            Err(replace_err) => {
                                // Both rotation attempts blocked (e.g.
                                // Windows file lock). Delete and bootstrap
                                // fresh — we cannot safely use a store with
                                // a mismatched schema.
                                let _ = fs::remove_file(&path);
                                bootstrap_store(workspace_root, cache_root)?;
                                tracing::warn!(
                                    target: "squeezy::store",
                                    on_disk_version,
                                    schema_version = SCHEMA_VERSION,
                                    rotate_error = %rotate_err,
                                    replace_error = %replace_err,
                                    "state.redb schema mismatch; rotation failed, existing store deleted and reinitialised without migration",
                                );
                            }
                        }
                    }
                }
                open_database(&path)?
            }
            None => {
                drop(initial);
                bootstrap_store(workspace_root, cache_root)?;
                open_database(&path)?
            }
        };
        Ok(Self {
            path,
            database,
            graph_store: Mutex::new(None),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn set_graph_metadata(&self, metadata: &GraphStoreMetadata) -> Result<()> {
        self.with_graph_store(|store| store.set_graph_metadata(metadata))
    }

    pub fn graph_metadata(&self) -> Result<Option<GraphStoreMetadata>> {
        self.with_graph_store(GraphStore::graph_metadata)
    }

    pub fn put_graph_partition<T: Serialize>(&self, file_id: &FileId, partition: &T) -> Result<()> {
        self.with_graph_store(|store| store.put_graph_partition(file_id, partition))
    }

    pub fn graph_partition<T: DeserializeOwned>(&self, file_id: &FileId) -> Result<Option<T>> {
        self.with_graph_store(|store| store.graph_partition(file_id))
    }

    pub fn remove_graph_partition(&self, file_id: &FileId) -> Result<()> {
        self.with_graph_store(|store| store.remove_graph_partition(file_id))
    }

    pub fn clear_graph_partitions(&self) -> Result<()> {
        self.with_graph_store(GraphStore::clear_graph_partitions)
    }

    /// Apply a coherent set of graph changes (metadata, partition upserts and
    /// removals, resolver snapshots, and resolver import graph) inside a
    /// single redb write transaction. Callers should batch per-refresh churn
    /// through this rather than calling [`set_graph_metadata`],
    /// [`put_graph_partition`], [`remove_graph_partition`], or
    /// [`put_import_graph`] in a tight loop: each of those commits
    /// independently and pays a fresh fsync, which dominates wall-clock cost on
    /// a cold workspace crawl.
    pub fn apply_graph_batch(&self, batch: &GraphWriteBatch) -> Result<()> {
        self.with_graph_store(|store| store.apply_graph_batch(batch))
    }

    /// Upsert a per-file resolver snapshot into the V2 resolver cache.
    /// Callers should fingerprint the file (modified-time + size) into the
    /// stored value so a later open can decide whether the snapshot is
    /// still authoritative.
    pub fn put_resolver_entry<T: Serialize>(&self, file_id: &FileId, entry: &T) -> Result<()> {
        self.with_graph_store(|store| store.put_resolver_entry(file_id, entry))
    }

    pub fn resolver_entry<T: DeserializeOwned>(&self, file_id: &FileId) -> Result<Option<T>> {
        self.with_graph_store(|store| store.resolver_entry(file_id))
    }

    pub fn resolver_entries_for<T: DeserializeOwned>(
        &self,
        file_ids: &[FileId],
    ) -> Result<Vec<(FileId, T)>> {
        self.with_graph_store(|store| store.resolver_entries_for(file_ids))
    }

    pub fn remove_resolver_entry(&self, file_id: &FileId) -> Result<()> {
        self.with_graph_store(|store| store.remove_resolver_entry(file_id))
    }

    pub fn clear_resolver_entries(&self) -> Result<()> {
        self.with_graph_store(GraphStore::clear_resolver_entries)
    }

    /// Replace the persisted file-level import adjacency blob. Stored under
    /// one key so reading on warm-start is a single table get.
    pub fn put_import_graph<T: Serialize>(&self, graph: &T) -> Result<()> {
        self.with_graph_store(|store| store.put_import_graph(graph))
    }

    pub fn import_graph<T: DeserializeOwned>(&self) -> Result<Option<T>> {
        self.with_graph_store(|store| store.import_graph())
    }

    pub fn put_tool_receipt(&self, receipt: &StoredToolReceipt) -> Result<()> {
        let write = self.begin_write()?;
        {
            let mut table = write.open_table(TOOL_RECEIPTS).map_err(store_error)?;
            insert_json(
                &mut table,
                &receipt_key(&receipt.tool_name, &receipt.stable_output_sha256),
                receipt,
            )?;
        }
        write.commit().map_err(store_error)
    }

    pub fn tool_receipts(&self) -> Result<Vec<StoredToolReceipt>> {
        let read = self.database.begin_read().map_err(store_error)?;
        let table = match read.open_table(TOOL_RECEIPTS) {
            Ok(table) => table,
            Err(_) => return Ok(Vec::new()),
        };
        let mut receipts = Vec::new();
        for entry in table.iter().map_err(store_error)? {
            let (_, value) = entry.map_err(store_error)?;
            receipts.push(decode(value.value())?);
        }
        Ok(receipts)
    }

    pub fn put_read_snapshot(&self, snapshot: &StoredReadSnapshot) -> Result<()> {
        let write = self.begin_write()?;
        {
            let mut table = write.open_table(READ_SNAPSHOTS).map_err(store_error)?;
            insert_json(
                &mut table,
                &read_snapshot_key(&snapshot.path, snapshot.start_byte, snapshot.end_byte),
                snapshot,
            )?;
        }
        write.commit().map_err(store_error)
    }

    pub fn put_mcp_tool_cache<T: Serialize>(&self, key: &str, cache: &T) -> Result<()> {
        let write = self.begin_write()?;
        {
            let mut table = write.open_table(MCP_TOOL_CACHE).map_err(store_error)?;
            insert_json(&mut table, key, cache)?;
        }
        write.commit().map_err(store_error)
    }

    pub fn mcp_tool_cache<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        let read = self.database.begin_read().map_err(store_error)?;
        let table = match read.open_table(MCP_TOOL_CACHE) {
            Ok(table) => table,
            Err(_) => return Ok(None),
        };
        read_table_json(&table, key)
    }

    /// Return the most recently created snapshot for `path`, regardless of
    /// window. Useful for diagnostics and call sites that only need to know
    /// whether any snapshot exists.
    pub fn read_snapshot(&self, path: &str) -> Result<Option<StoredReadSnapshot>> {
        let snapshots = self.read_snapshots_for_path(path)?;
        Ok(snapshots
            .into_iter()
            .max_by_key(|snapshot| snapshot.created_unix_millis))
    }

    /// Return every snapshot stored under `path` across all `(start_byte,
    /// end_byte)` windows. Callers that need to match a specific request
    /// window should filter the returned list themselves.
    pub fn read_snapshots_for_path(&self, path: &str) -> Result<Vec<StoredReadSnapshot>> {
        let read = self.database.begin_read().map_err(store_error)?;
        let table = match read.open_table(READ_SNAPSHOTS) {
            Ok(table) => table,
            Err(_) => return Ok(Vec::new()),
        };
        let prefix = read_snapshot_key_prefix(path);
        let mut snapshots = Vec::new();
        for entry in table.range(prefix.as_str()..).map_err(store_error)? {
            let (key, value) = entry.map_err(store_error)?;
            if !key.value().starts_with(prefix.as_str()) {
                break;
            }
            let snapshot: StoredReadSnapshot = decode(value.value())?;
            if snapshot.path == path {
                snapshots.push(snapshot);
            }
        }
        Ok(snapshots)
    }

    /// Return the most recent snapshot for `path` whose stored window exactly
    /// matches `[start_byte, end_byte)`. The caller is expected to compare
    /// `content_sha256` against the current file before treating the snapshot
    /// as a hit.
    pub fn read_snapshot_for_window(
        &self,
        path: &str,
        start_byte: u64,
        end_byte: u64,
    ) -> Result<Option<StoredReadSnapshot>> {
        let read = self.database.begin_read().map_err(store_error)?;
        let table = match read.open_table(READ_SNAPSHOTS) {
            Ok(table) => table,
            Err(_) => return Ok(None),
        };
        read_table_json(
            &table,
            read_snapshot_key(path, start_byte, end_byte).as_str(),
        )
    }

    pub fn put_observation(&self, mut observation: Observation) -> Result<Observation> {
        let now = unix_millis();
        if observation.id.is_empty() {
            observation.id = format!("obs-{now}-{}", unix_nanos());
        }
        if observation.created_unix_millis == 0 {
            observation.created_unix_millis = now;
        }
        observation.updated_unix_millis = now;
        let tokens = observation_tokens(&observation);
        let write = self.begin_write()?;
        {
            let mut observations = write.open_table(OBSERVATIONS).map_err(store_error)?;
            insert_json(&mut observations, observation.id.as_str(), &observation)?;
        }
        {
            let mut index = write.open_table(OBSERVATION_INDEX).map_err(store_error)?;
            for token in tokens {
                let mut ids =
                    read_table_json(&index, token.as_str())?.unwrap_or_else(Vec::<String>::new);
                if !ids.iter().any(|id| id == &observation.id) {
                    ids.push(observation.id.clone());
                    ids.sort();
                }
                insert_json(&mut index, token.as_str(), &ids)?;
            }
        }
        write.commit().map_err(store_error)?;
        Ok(observation)
    }

    pub fn get_observation(&self, id: &str) -> Result<Option<Observation>> {
        let read = self.database.begin_read().map_err(store_error)?;
        let table = match read.open_table(OBSERVATIONS) {
            Ok(table) => table,
            Err(_) => return Ok(None),
        };
        read_table_json(&table, id)
    }

    pub fn search_observations(&self, query: &str, limit: usize) -> Result<Vec<Observation>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let read = self.database.begin_read().map_err(store_error)?;
        let observations = match read.open_table(OBSERVATIONS) {
            Ok(table) => table,
            Err(_) => return Ok(Vec::new()),
        };
        let index = read.open_table(OBSERVATION_INDEX).ok();
        let query_tokens = tokenize(query);
        let mut ids = BTreeSet::new();
        if let Some(index) = index {
            for token in &query_tokens {
                if let Some(indexed) = read_table_json::<Vec<String>, _>(&index, token.as_str())? {
                    ids.extend(indexed);
                }
            }
        }
        let mut matches = Vec::new();
        if ids.is_empty() && !query_tokens.is_empty() {
            for entry in observations.iter().map_err(store_error)? {
                let (_, value) = entry.map_err(store_error)?;
                let observation: Observation = decode(value.value())?;
                if observation_matches(&observation, &query_tokens) {
                    matches.push(observation);
                }
            }
        } else {
            for id in ids {
                if let Some(observation) =
                    read_table_json::<Observation, _>(&observations, id.as_str())?
                    && (query_tokens.is_empty() || observation_matches(&observation, &query_tokens))
                {
                    matches.push(observation);
                }
            }
        }
        matches.sort_by(|left, right| {
            right
                .updated_unix_millis
                .cmp(&left.updated_unix_millis)
                .then(left.id.cmp(&right.id))
        });
        matches.truncate(limit);
        Ok(matches)
    }

    /// Return up to `limit` observations sorted by `updated_unix_millis`
    /// (newest first). Use when there is no specific query but the caller
    /// wants a recency-ordered tail for prompt injection or display.
    pub fn list_recent_observations(&self, limit: usize) -> Result<Vec<Observation>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let read = self.database.begin_read().map_err(store_error)?;
        let table = match read.open_table(OBSERVATIONS) {
            Ok(table) => table,
            Err(_) => return Ok(Vec::new()),
        };
        let mut all = Vec::new();
        for entry in table.iter().map_err(store_error)? {
            let (_, value) = entry.map_err(store_error)?;
            let observation: Observation = decode(value.value())?;
            all.push(observation);
        }
        all.sort_by(|left, right| {
            right
                .updated_unix_millis
                .cmp(&left.updated_unix_millis)
                .then(left.id.cmp(&right.id))
        });
        all.truncate(limit);
        Ok(all)
    }

    pub fn delete_observation(&self, id: &str) -> Result<()> {
        let existing = self.get_observation(id)?;
        let write = self.begin_write()?;
        {
            let mut observations = write.open_table(OBSERVATIONS).map_err(store_error)?;
            observations.remove(id).map_err(store_error)?;
        }
        if let Some(observation) = existing {
            let mut index = write.open_table(OBSERVATION_INDEX).map_err(store_error)?;
            for token in observation_tokens(&observation) {
                let mut ids =
                    read_table_json(&index, token.as_str())?.unwrap_or_else(Vec::<String>::new);
                ids.retain(|existing_id| existing_id != id);
                if ids.is_empty() {
                    index.remove(token.as_str()).map_err(store_error)?;
                } else {
                    insert_json(&mut index, token.as_str(), &ids)?;
                }
            }
        }
        write.commit().map_err(store_error)
    }

    /// Persist a pre-compaction snapshot so a later `compact_context_undo`
    /// can restore the dropped slice. Idempotent on `replacement_id`.
    pub fn put_compaction_checkpoint(&self, checkpoint: &CompactionCheckpoint) -> Result<()> {
        let write = self.begin_write()?;
        {
            let mut table = write
                .open_table(COMPACTION_CHECKPOINTS)
                .map_err(store_error)?;
            insert_json(&mut table, checkpoint.replacement_id.as_str(), checkpoint)?;
        }
        write.commit().map_err(store_error)
    }

    pub fn get_compaction_checkpoint(
        &self,
        replacement_id: &str,
    ) -> Result<Option<CompactionCheckpoint>> {
        let read = self.database.begin_read().map_err(store_error)?;
        let table = match read.open_table(COMPACTION_CHECKPOINTS) {
            Ok(table) => table,
            Err(_) => return Ok(None),
        };
        read_table_json(&table, replacement_id)
    }

    /// Drop every checkpoint whose `created_unix_millis < older_than_unix_millis`.
    /// Returns the number removed.
    pub fn prune_compaction_checkpoints(&self, older_than_unix_millis: u128) -> Result<usize> {
        let stale = {
            let read = self.database.begin_read().map_err(store_error)?;
            let table = match read.open_table(COMPACTION_CHECKPOINTS) {
                Ok(table) => table,
                Err(_) => return Ok(0),
            };
            let mut stale = Vec::new();
            for entry in table.iter().map_err(store_error)? {
                let (key, value) = entry.map_err(store_error)?;
                let checkpoint: CompactionCheckpoint = decode(value.value())?;
                if checkpoint.created_unix_millis < older_than_unix_millis {
                    stale.push(key.value().to_string());
                }
            }
            stale
        };
        if stale.is_empty() {
            return Ok(0);
        }
        let removed = stale.len();
        let write = self.begin_write()?;
        {
            let mut table = write
                .open_table(COMPACTION_CHECKPOINTS)
                .map_err(store_error)?;
            for key in &stale {
                table.remove(key.as_str()).map_err(store_error)?;
            }
        }
        write.commit().map_err(store_error)?;
        Ok(removed)
    }

    fn begin_write(&self) -> Result<redb::WriteTransaction> {
        self.database.begin_write().map_err(store_error)
    }

    fn graph_store(&self) -> Result<Arc<GraphStore>> {
        let mut cached = self
            .graph_store
            .lock()
            .map_err(|_| SqueezyError::Tool("graph store lock poisoned".to_string()))?;
        if let Some(store) = cached.as_ref() {
            return Ok(Arc::clone(store));
        }
        let store = Arc::new(GraphStore::open_path(
            self.path.with_file_name(GRAPH_FILE_NAME),
        )?);
        *cached = Some(Arc::clone(&store));
        Ok(store)
    }

    fn with_graph_store<T>(&self, action: impl FnOnce(&GraphStore) -> Result<T>) -> Result<T> {
        let store = self.graph_store()?;
        action(store.as_ref())
    }
}

impl GraphStore {
    pub fn open(workspace_root: impl AsRef<Path>, cache_root: Option<&Path>) -> Result<Self> {
        Self::open_path(graph_path(workspace_root.as_ref(), cache_root))
    }

    pub fn open_path(path: impl Into<PathBuf>) -> Result<Self> {
        let legacy_path = path.into();
        let cache_dir = legacy_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let root = cache_dir.join(GRAPH_DIR_NAME).join(GRAPH_V3_DIR_NAME);
        fs::create_dir_all(&root)?;
        let manifest_path = root.join(GRAPH_MANIFEST_FILE_NAME);
        let global_path = root.join(GRAPH_GLOBAL_FILE_NAME);
        let manifest = open_graph_v3_database(&manifest_path)?;
        ensure_graph_manifest(&manifest)?;
        let global = open_graph_v3_database(&global_path)?;
        Ok(Self {
            path: manifest_path.clone(),
            backend: GraphStoreBackend::V3(GraphStoreV3 {
                root,
                legacy_path,
                manifest,
                global,
                shards: Mutex::new(BTreeMap::new()),
            }),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn probe_path_read_only(path: impl Into<PathBuf>) -> Result<GraphStoreProbe> {
        let legacy_path = path.into();
        let path = graph_manifest_path_from_legacy_path(&legacy_path);
        let path = if path.exists() { path } else { legacy_path };
        let database = ReadOnlyDatabase::open(&path).map_err(store_error)?;
        let schema_version = current_schema_version(&database)?;
        Ok(GraphStoreProbe {
            path,
            schema_version,
        })
    }

    pub fn set_graph_metadata(&self, metadata: &GraphStoreMetadata) -> Result<()> {
        let v3 = self.v3();
        let write = v3.manifest.begin_write().map_err(store_error)?;
        {
            let mut meta = write.open_table(META).map_err(store_error)?;
            insert_json(&mut meta, "graph_metadata", metadata)?;
        }
        write.commit().map_err(store_error)
    }

    pub fn graph_metadata(&self) -> Result<Option<GraphStoreMetadata>> {
        let v3 = self.v3();
        let read = v3.manifest.begin_read().map_err(store_error)?;
        let table = match read.open_table(META) {
            Ok(table) => table,
            Err(_) => return legacy_graph_metadata(&v3.legacy_path),
        };
        match read_table_json(&table, "graph_metadata")? {
            Some(metadata) => Ok(Some(metadata)),
            None => legacy_graph_metadata(&v3.legacy_path),
        }
    }

    pub fn put_graph_partition<T: Serialize>(&self, file_id: &FileId, partition: &T) -> Result<()> {
        let mut batch = GraphWriteBatch::new();
        batch.upsert_partition(file_id, partition)?;
        self.apply_graph_batch(&batch)
    }

    pub fn graph_partition<T: DeserializeOwned>(&self, file_id: &FileId) -> Result<Option<T>> {
        let v3 = self.v3();
        let active_generation = v3.active_generation()?;
        let Some(shard_key) = v3.placement_for_file(file_id)? else {
            return legacy_graph_partition(&v3.legacy_path, file_id);
        };
        let Some(database) = v3.existing_shard_database(&shard_key)? else {
            return legacy_graph_partition(&v3.legacy_path, file_id);
        };
        let read = database.begin_read().map_err(store_error)?;
        let table = match read.open_table(GRAPH_PARTITIONS) {
            Ok(table) => table,
            Err(_) => return Ok(None),
        };
        read_versioned_graph(&table, file_id.0.as_str(), active_generation)
    }

    /// Decode every stored graph partition into `T`, keyed by its `FileId`.
    ///
    /// Callers pass a *lightweight* `T` that deserializes only the fields they
    /// need (serde ignores the rest), so the warm-start path can read each
    /// file's persisted fingerprint without materialising the full parse
    /// result. Returns an empty vec if the table has never been written.
    pub fn graph_partition_entries<T: DeserializeOwned>(&self) -> Result<Vec<(FileId, T)>> {
        let v3 = self.v3();
        let placements = v3.file_placements()?;
        if placements.is_empty() {
            return legacy_graph_partition_entries(&v3.legacy_path);
        }
        let active_generation = v3.active_generation()?;
        let mut entries = Vec::new();
        for (file_id, shard_key) in placements {
            let Some(database) = v3.existing_shard_database(&shard_key)? else {
                continue;
            };
            let read = database.begin_read().map_err(store_error)?;
            let table = match read.open_table(GRAPH_PARTITIONS) {
                Ok(table) => table,
                Err(_) => continue,
            };
            if let Some(decoded) =
                read_versioned_graph::<T, _>(&table, file_id.0.as_str(), active_generation)?
            {
                entries.push((file_id, decoded));
            }
        }
        Ok(entries)
    }

    pub fn remove_graph_partition(&self, file_id: &FileId) -> Result<()> {
        let mut batch = GraphWriteBatch::new();
        batch.remove_partition(file_id);
        self.apply_graph_batch(&batch)
    }

    pub fn clear_graph_partitions(&self) -> Result<()> {
        let v3 = self.v3();
        for shard_key in v3.shard_keys()? {
            if let Some(database) = v3.existing_shard_database(&shard_key)? {
                let write = database.begin_write().map_err(store_error)?;
                clear_table(&write, GRAPH_PARTITIONS)?;
                write.commit().map_err(store_error)?;
            }
        }
        Ok(())
    }

    pub fn apply_graph_batch(&self, batch: &GraphWriteBatch) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let v3 = self.v3();
        let next_generation = v3.active_generation()?.saturating_add(1);
        let mut touched_shards: BTreeMap<GraphShardKey, Vec<&GraphPartitionWrite>> =
            BTreeMap::new();
        let mut touched_resolvers: BTreeMap<GraphShardKey, Vec<&GraphResolverWrite>> =
            BTreeMap::new();
        for upsert in &batch.upserts {
            touched_shards
                .entry(GraphShardKey::for_file(&upsert.file_id, upsert.language))
                .or_default()
                .push(upsert);
        }
        for upsert in &batch.resolver_upserts {
            touched_resolvers
                .entry(GraphShardKey::for_file(&upsert.file_id, upsert.language))
                .or_default()
                .push(upsert);
        }

        for (shard_key, upserts) in &touched_shards {
            let database = match v3.shard_database(shard_key) {
                Ok(database) => database,
                Err(error) if is_locked_store_error(&error) => continue,
                Err(error) => return Err(error),
            };
            let write = database.begin_write().map_err(store_error)?;
            {
                let mut table = write.open_table(GRAPH_PARTITIONS).map_err(store_error)?;
                for upsert in upserts {
                    let value = encode_versioned_graph_payload(next_generation, &upsert.encoded)?;
                    table
                        .insert(upsert.file_id.0.as_str(), value.as_slice())
                        .map_err(store_error)?;
                }
            }
            write.commit().map_err(store_error)?;
        }

        for (shard_key, upserts) in &touched_resolvers {
            let database = match v3.shard_database(shard_key) {
                Ok(database) => database,
                Err(error) if is_locked_store_error(&error) => continue,
                Err(error) => return Err(error),
            };
            let write = database.begin_write().map_err(store_error)?;
            {
                let mut table = write
                    .open_table(RESOLVER_SNAPSHOT_PER_FILE)
                    .map_err(store_error)?;
                for upsert in upserts {
                    let value = encode_versioned_graph_payload(next_generation, &upsert.encoded)?;
                    table
                        .insert(upsert.file_id.0.as_str(), value.as_slice())
                        .map_err(store_error)?;
                }
            }
            write.commit().map_err(store_error)?;
        }

        for key in batch.removals.iter().chain(batch.resolver_removals.iter()) {
            let file_id = FileId::new(key.clone());
            let Some(shard_key) = v3.placement_for_file(&file_id)? else {
                continue;
            };
            if let Some(database) = v3.existing_shard_database(&shard_key)? {
                let write = database.begin_write().map_err(store_error)?;
                {
                    if batch.removals.iter().any(|removed| removed == key)
                        && let Ok(mut table) = write.open_table(GRAPH_PARTITIONS)
                    {
                        table.remove(key.as_str()).map_err(store_error)?;
                    }
                    if batch.resolver_removals.iter().any(|removed| removed == key)
                        && let Ok(mut table) = write.open_table(RESOLVER_SNAPSHOT_PER_FILE)
                    {
                        table.remove(key.as_str()).map_err(store_error)?;
                    }
                }
                write.commit().map_err(store_error)?;
            }
        }

        if let Some(blob) = &batch.import_graph {
            match v3.global.begin_write().map_err(store_error) {
                Ok(write) => {
                    {
                        let mut table = write
                            .open_table(RESOLVER_IMPORT_GRAPH)
                            .map_err(store_error)?;
                        let value = encode_versioned_graph_payload(next_generation, blob)?;
                        table
                            .insert("resolver_import_graph", value.as_slice())
                            .map_err(store_error)?;
                    }
                    write.commit().map_err(store_error)?;
                }
                Err(error) if is_locked_store_error(&error) => {}
                Err(error) => return Err(error),
            }
        }

        let manifest_write = v3.manifest.begin_write().map_err(store_error)?;
        {
            let mut manifest = v3.manifest_record()?;
            manifest.active_generation = next_generation;
            let mut table = manifest_write
                .open_table(GRAPH_MANIFEST)
                .map_err(store_error)?;
            insert_json(&mut table, "manifest", &manifest)?;
        }
        if let Some(metadata) = &batch.metadata {
            let mut meta = manifest_write.open_table(META).map_err(store_error)?;
            insert_json(&mut meta, "graph_metadata", metadata)?;
        }
        {
            let mut table = manifest_write
                .open_table(GRAPH_FILE_SHARDS)
                .map_err(store_error)?;
            for (shard_key, upserts) in &touched_shards {
                for upsert in upserts {
                    insert_json(
                        &mut table,
                        upsert.file_id.0.as_str(),
                        &GraphShardPlacement::from(shard_key),
                    )?;
                }
            }
            for (shard_key, upserts) in &touched_resolvers {
                for upsert in upserts {
                    insert_json(
                        &mut table,
                        upsert.file_id.0.as_str(),
                        &GraphShardPlacement::from(shard_key),
                    )?;
                }
            }
            for key in &batch.removals {
                if batch.resolver_removals.iter().any(|removed| removed == key) {
                    table.remove(key.as_str()).map_err(store_error)?;
                }
            }
        }
        manifest_write.commit().map_err(store_error)
    }

    pub fn put_resolver_entry<T: Serialize>(&self, file_id: &FileId, entry: &T) -> Result<()> {
        let mut batch = GraphWriteBatch::new();
        batch.upsert_resolver_entry(file_id, entry)?;
        self.apply_graph_batch(&batch)
    }

    pub fn resolver_entry<T: DeserializeOwned>(&self, file_id: &FileId) -> Result<Option<T>> {
        let mut entries = self.resolver_entries_for(std::slice::from_ref(file_id))?;
        Ok(entries.pop().map(|(_, entry)| entry))
    }

    pub fn resolver_entries_for<T: DeserializeOwned>(
        &self,
        file_ids: &[FileId],
    ) -> Result<Vec<(FileId, T)>> {
        let v3 = self.v3();
        let active_generation = v3.active_generation()?;
        let mut out = Vec::with_capacity(file_ids.len());
        for file_id in file_ids {
            let Some(shard_key) = v3.placement_for_file(file_id)? else {
                if let Some(value) = legacy_resolver_entry(&v3.legacy_path, file_id)? {
                    out.push((file_id.clone(), value));
                }
                continue;
            };
            let Some(database) = v3.existing_shard_database(&shard_key)? else {
                continue;
            };
            let read = database.begin_read().map_err(store_error)?;
            let table = match read.open_table(RESOLVER_SNAPSHOT_PER_FILE) {
                Ok(table) => table,
                Err(_) => continue,
            };
            if let Some(value) =
                read_versioned_graph::<T, _>(&table, file_id.0.as_str(), active_generation)?
            {
                out.push((file_id.clone(), value));
            }
        }
        Ok(out)
    }

    pub fn remove_resolver_entry(&self, file_id: &FileId) -> Result<()> {
        let mut batch = GraphWriteBatch::new();
        batch.remove_resolver_entry(file_id);
        self.apply_graph_batch(&batch)
    }

    pub fn clear_resolver_entries(&self) -> Result<()> {
        let v3 = self.v3();
        for shard_key in v3.shard_keys()? {
            if let Some(database) = v3.existing_shard_database(&shard_key)? {
                let write = database.begin_write().map_err(store_error)?;
                clear_table(&write, RESOLVER_SNAPSHOT_PER_FILE)?;
                write.commit().map_err(store_error)?;
            }
        }
        Ok(())
    }

    pub fn put_import_graph<T: Serialize>(&self, graph: &T) -> Result<()> {
        let mut batch = GraphWriteBatch::new();
        batch.set_import_graph(graph)?;
        self.apply_graph_batch(&batch)
    }

    pub fn import_graph<T: DeserializeOwned>(&self) -> Result<Option<T>> {
        let v3 = self.v3();
        let active_generation = v3.active_generation()?;
        let read = v3.global.begin_read().map_err(store_error)?;
        let table = match read.open_table(RESOLVER_IMPORT_GRAPH) {
            Ok(table) => table,
            Err(_) => return legacy_import_graph(&v3.legacy_path),
        };
        match read_versioned_graph(&table, "resolver_import_graph", active_generation)? {
            Some(graph) => Ok(Some(graph)),
            None => legacy_import_graph(&v3.legacy_path),
        }
    }

    fn v3(&self) -> &GraphStoreV3 {
        match &self.backend {
            GraphStoreBackend::V3(store) => store,
        }
    }
}

impl GraphStoreV3 {
    fn manifest_record(&self) -> Result<GraphManifest> {
        let read = self.manifest.begin_read().map_err(store_error)?;
        let table = match read.open_table(GRAPH_MANIFEST) {
            Ok(table) => table,
            Err(_) => return Ok(default_graph_manifest()),
        };
        Ok(read_table_json(&table, "manifest")?.unwrap_or_else(default_graph_manifest))
    }

    fn active_generation(&self) -> Result<u64> {
        self.manifest_record()
            .map(|manifest| manifest.active_generation)
    }

    fn shard_database(&self, key: &GraphShardKey) -> Result<Arc<Database>> {
        {
            let cached = self
                .shards
                .lock()
                .map_err(|_| SqueezyError::Tool("graph shard map lock poisoned".to_string()))?;
            if let Some(database) = cached.get(key) {
                return Ok(Arc::clone(database));
            }
        }
        let database = Arc::new(open_graph_v3_database(&self.shard_path(key))?);
        let mut cached = self
            .shards
            .lock()
            .map_err(|_| SqueezyError::Tool("graph shard map lock poisoned".to_string()))?;
        if let Some(existing) = cached.get(key) {
            Ok(Arc::clone(existing))
        } else {
            cached.insert(key.clone(), Arc::clone(&database));
            Ok(database)
        }
    }

    fn existing_shard_database(&self, key: &GraphShardKey) -> Result<Option<Arc<Database>>> {
        if !self.shard_path(key).exists() {
            return Ok(None);
        }
        match self.shard_database(key) {
            Ok(database) => Ok(Some(database)),
            Err(error) if is_locked_store_error(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn shard_path(&self, key: &GraphShardKey) -> PathBuf {
        self.root
            .join(GRAPH_SHARDS_DIR_NAME)
            .join(key.language.as_str())
            .join(format!("{:02}.redb", key.bucket))
    }

    fn placement_for_file(&self, file_id: &FileId) -> Result<Option<GraphShardKey>> {
        let read = self.manifest.begin_read().map_err(store_error)?;
        let table = match read.open_table(GRAPH_FILE_SHARDS) {
            Ok(table) => table,
            Err(_) => return Ok(None),
        };
        Ok(
            read_table_json::<GraphShardPlacement, _>(&table, file_id.0.as_str())?
                .map(GraphShardKey::from),
        )
    }

    fn file_placements(&self) -> Result<Vec<(FileId, GraphShardKey)>> {
        let read = self.manifest.begin_read().map_err(store_error)?;
        let table = match read.open_table(GRAPH_FILE_SHARDS) {
            Ok(table) => table,
            Err(_) => return Ok(Vec::new()),
        };
        let mut placements = Vec::new();
        for entry in table.iter().map_err(store_error)? {
            let (key, value) = entry.map_err(store_error)?;
            let placement: GraphShardPlacement = decode(value.value())?;
            placements.push((
                FileId::new(key.value().to_string()),
                GraphShardKey::from(placement),
            ));
        }
        Ok(placements)
    }

    fn shard_keys(&self) -> Result<Vec<GraphShardKey>> {
        let mut keys = self
            .file_placements()?
            .into_iter()
            .map(|(_, key)| key)
            .collect::<Vec<_>>();
        keys.sort();
        keys.dedup();
        Ok(keys)
    }
}

impl GraphShardKey {
    fn for_file(file_id: &FileId, language: LanguageKind) -> Self {
        let digest = Sha256::digest(file_id.0.as_bytes());
        Self {
            language: language_shard_name(language).to_string(),
            bucket: digest[0] % GRAPH_SHARD_COUNT,
        }
    }
}

impl From<&GraphShardKey> for GraphShardPlacement {
    fn from(key: &GraphShardKey) -> Self {
        Self {
            language: key.language.clone(),
            bucket: key.bucket,
        }
    }
}

impl From<GraphShardPlacement> for GraphShardKey {
    fn from(placement: GraphShardPlacement) -> Self {
        Self {
            language: placement.language,
            bucket: placement.bucket,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphStoreMetadata {
    pub workspace_root: String,
    pub crawl_options_hash: String,
    pub language_registry_version: String,
    pub graph_format_version: u64,
}

/// Buffered set of graph state changes to commit atomically via
/// [`SqueezyStore::apply_graph_batch`]. Encoded payloads accumulate in memory
/// so the resulting redb write transaction touches each affected table only
/// once.
#[derive(Debug, Default)]
pub struct GraphWriteBatch {
    metadata: Option<GraphStoreMetadata>,
    upserts: Vec<GraphPartitionWrite>,
    removals: Vec<String>,
    resolver_upserts: Vec<GraphResolverWrite>,
    resolver_removals: Vec<String>,
    /// Encoded replacement for the single-blob `RESOLVER_IMPORT_GRAPH` entry.
    /// When `Some`, the batch writes this value in the same transaction as the
    /// per-file resolver upserts/removals, so all resolver-cache state is
    /// committed atomically.
    import_graph: Option<Vec<u8>>,
}

impl GraphWriteBatch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_metadata(&mut self, metadata: GraphStoreMetadata) {
        self.metadata = Some(metadata);
    }

    pub fn upsert_partition<T: Serialize>(
        &mut self,
        file_id: &FileId,
        partition: &T,
    ) -> Result<()> {
        self.upsert_partition_for_language(file_id, LanguageKind::Unknown, partition)
    }

    pub fn upsert_partition_for_language<T: Serialize>(
        &mut self,
        file_id: &FileId,
        language: LanguageKind,
        partition: &T,
    ) -> Result<()> {
        let encoded = encode_graph(partition)?;
        self.upserts.push(GraphPartitionWrite {
            file_id: file_id.clone(),
            language,
            encoded,
        });
        Ok(())
    }

    pub fn remove_partition(&mut self, file_id: &FileId) {
        self.removals.push(file_id.0.clone());
    }

    pub fn upsert_resolver_entry<T: Serialize>(
        &mut self,
        file_id: &FileId,
        entry: &T,
    ) -> Result<()> {
        self.upsert_resolver_entry_for_language(file_id, LanguageKind::Unknown, entry)
    }

    pub fn upsert_resolver_entry_for_language<T: Serialize>(
        &mut self,
        file_id: &FileId,
        language: LanguageKind,
        entry: &T,
    ) -> Result<()> {
        let encoded = encode_graph(entry)?;
        self.resolver_upserts.push(GraphResolverWrite {
            file_id: file_id.clone(),
            language,
            encoded,
        });
        Ok(())
    }

    pub fn remove_resolver_entry(&mut self, file_id: &FileId) {
        self.resolver_removals.push(file_id.0.clone());
    }

    /// Encode and stage a replacement for the single-blob import-adjacency
    /// graph. Committed in the same transaction as any resolver-entry
    /// upserts/removals so all resolver-cache state lands atomically.
    pub fn set_import_graph<T: Serialize>(&mut self, graph: &T) -> Result<()> {
        self.import_graph = Some(encode_graph(graph)?);
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.metadata.is_none()
            && self.upserts.is_empty()
            && self.removals.is_empty()
            && self.resolver_upserts.is_empty()
            && self.resolver_removals.is_empty()
            && self.import_graph.is_none()
    }

    pub fn len(&self) -> usize {
        usize::from(self.metadata.is_some())
            + self.upserts.len()
            + self.removals.len()
            + self.resolver_upserts.len()
            + self.resolver_removals.len()
            + usize::from(self.import_graph.is_some())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredToolReceipt {
    pub tool_name: String,
    pub stable_output_sha256: String,
    pub call_id: String,
    pub content_sha256: Option<String>,
    pub model_output_bytes: usize,
    pub created_unix_millis: u128,
    #[serde(default)]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredReadSnapshot {
    pub path: String,
    pub tool_name: String,
    pub call_id: String,
    pub stable_output_sha256: String,
    pub content_sha256: Option<String>,
    pub start_byte: u64,
    pub end_byte: u64,
    pub content: String,
    pub model_output_bytes: usize,
    pub created_unix_millis: u128,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub id: String,
    pub kind: ObservationKind,
    pub text: String,
    pub tags: Vec<String>,
    pub source: String,
    pub created_unix_millis: u128,
    pub updated_unix_millis: u128,
}

impl Observation {
    pub fn new(kind: ObservationKind, text: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            id: String::new(),
            kind,
            text: text.into(),
            tags: Vec::new(),
            source: source.into(),
            created_unix_millis: 0,
            updated_unix_millis: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObservationKind {
    Preference,
    Decision,
    Convention,
    DeadEnd,
    Note,
}

/// A snapshot of the pre-compaction conversation slice, persisted so the
/// agent can later restore it via `compact_context_undo`. Keyed in redb by
/// `replacement_id` (typically `format!("ckpt-{generation}-{ms}")`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionCheckpoint {
    pub replacement_id: String,
    pub session_id: String,
    pub generation: u64,
    pub items: Vec<crate::sessions::ResumeItem>,
    pub created_unix_millis: u128,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheFileReport {
    pub path: PathBuf,
    pub exists: bool,
    pub size_bytes: u64,
    pub modified_unix_ms: Option<u128>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheDiagnostics {
    pub cache_dir: PathBuf,
    pub state: CacheFileReport,
    pub graph: CacheFileReport,
    pub state_stats: Option<StateCacheStats>,
    pub graph_stats: Option<GraphCacheStats>,
    pub backups: Vec<CacheFileReport>,
    pub backup_total_bytes: u64,
    pub storage: Vec<StoragePathReport>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoragePathReport {
    pub label: String,
    pub path: PathBuf,
    pub mount_source: Option<String>,
    pub filesystem_type: Option<String>,
    pub classification: StorageMountClassification,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StorageMountClassification {
    Local,
    Network,
    Virtual,
    Unknown,
}

impl StorageMountClassification {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Network => "network",
            Self::Virtual => "virtual",
            Self::Unknown => "unknown",
        }
    }
}

/// Outcome of [`prune_cache_backups`]. Partial failures land in
/// `failed_files` instead of short-circuiting the prune so the caller can
/// surface a per-file summary (the doctor row uses both halves), but
/// callers that previously pattern-matched `Err(io::ErrorKind::…)` on the
/// old signature would silently miss those failures — hence `#[must_use]`
/// and [`Self::errored`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateCacheStats {
    pub schema_version: Option<u64>,
    pub tool_receipts: usize,
    pub read_snapshots: usize,
    pub mcp_tool_cache_entries: usize,
    pub observations: usize,
    pub compaction_checkpoints: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphCacheStats {
    pub schema_version: Option<u64>,
    pub graph_partitions: usize,
    pub resolver_entries: usize,
    pub import_graph_present: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[must_use = "CachePruneReport carries partial failures in `failed_files`; ignore only intentionally"]
pub struct CachePruneReport {
    pub removed_files: Vec<CacheFileReport>,
    pub removed_bytes: u64,
    pub failed_files: Vec<(PathBuf, String)>,
}

impl CachePruneReport {
    /// `Some` iff at least one backup failed to delete. Lets callers test
    /// for partial failure without re-checking the `failed_files` field.
    pub fn errored(&self) -> Option<&[(PathBuf, String)]> {
        (!self.failed_files.is_empty()).then_some(self.failed_files.as_slice())
    }
}

pub fn cache_diagnostics(
    workspace_root: impl AsRef<Path>,
    cache_root: Option<&Path>,
) -> Result<CacheDiagnostics> {
    cache_diagnostics_with_session_dir(workspace_root, cache_root, None)
}

pub fn cache_diagnostics_with_session_dir(
    workspace_root: impl AsRef<Path>,
    cache_root: Option<&Path>,
    session_log_dir: Option<&Path>,
) -> Result<CacheDiagnostics> {
    let workspace_root = workspace_root.as_ref();
    let cache_dir = cache_dir_path(workspace_root, cache_root);
    let state = cache_file_report(state_path(workspace_root, cache_root));
    let graph_v3 = graph_v3_dir_path(workspace_root, cache_root);
    let legacy_graph = graph_path(workspace_root, cache_root);
    let graph = if graph_v3.exists() {
        cache_tree_report(graph_v3.clone())
    } else {
        cache_file_report(legacy_graph.clone())
    };
    let state_stats = state.exists.then(|| state_cache_stats(&state.path));
    let graph_stats = graph.exists.then(|| {
        if graph_v3.exists() {
            graph_v3_cache_stats(&graph_v3)
        } else {
            graph_cache_stats(&legacy_graph)
        }
    });
    let backups = cache_backups(&cache_dir)?;
    let backup_total_bytes = backups.iter().map(|file| file.size_bytes).sum();
    let session_dir = session_dir_path(workspace_root, cache_root, session_log_dir);
    let storage = storage_reports([
        ("cache", cache_dir.as_path()),
        ("sessions", session_dir.as_path()),
        ("state.redb", state.path.as_path()),
        ("graph.v3", graph_v3.as_path()),
        ("legacy_graph.redb", legacy_graph.as_path()),
    ]);
    Ok(CacheDiagnostics {
        cache_dir,
        state,
        graph,
        state_stats,
        graph_stats,
        backups,
        backup_total_bytes,
        storage,
    })
}

pub fn prune_cache_backups(
    workspace_root: impl AsRef<Path>,
    cache_root: Option<&Path>,
) -> Result<CachePruneReport> {
    let diagnostics = cache_diagnostics(workspace_root, cache_root)?;
    let mut removed_files = Vec::new();
    let mut failed_files = Vec::new();
    let mut removed_bytes = 0;
    for backup in diagnostics.backups {
        match fs_util::remove_file(&backup.path) {
            Ok(()) => {
                removed_bytes += backup.size_bytes;
                removed_files.push(backup);
            }
            Err(SqueezyError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => failed_files.push((backup.path.clone(), err.to_string())),
        }
    }
    Ok(CachePruneReport {
        removed_files,
        removed_bytes,
        failed_files,
    })
}

/// Bootstrap a freshly created state store at the target schema version.
fn bootstrap_store(workspace_root: &Path, cache_root: Option<&Path>) -> Result<()> {
    let path = state_path(workspace_root, cache_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let database = open_database(&path)?;
    initialize_schema(&database)
}

fn state_cache_stats(path: &Path) -> StateCacheStats {
    match open_database(path) {
        Ok(database) => StateCacheStats {
            schema_version: current_schema_version(&database).ok().flatten(),
            tool_receipts: table_entry_count(&database, TOOL_RECEIPTS).unwrap_or(0),
            read_snapshots: table_entry_count(&database, READ_SNAPSHOTS).unwrap_or(0),
            mcp_tool_cache_entries: table_entry_count(&database, MCP_TOOL_CACHE).unwrap_or(0),
            observations: table_entry_count(&database, OBSERVATIONS).unwrap_or(0),
            compaction_checkpoints: table_entry_count(&database, COMPACTION_CHECKPOINTS)
                .unwrap_or(0),
            error: None,
        },
        Err(error) => StateCacheStats {
            schema_version: None,
            tool_receipts: 0,
            read_snapshots: 0,
            mcp_tool_cache_entries: 0,
            observations: 0,
            compaction_checkpoints: 0,
            error: Some(error.to_string()),
        },
    }
}

fn graph_cache_stats(path: &Path) -> GraphCacheStats {
    match ReadOnlyDatabase::open(path).map_err(store_error) {
        Ok(database) => GraphCacheStats {
            schema_version: current_schema_version(&database).ok().flatten(),
            graph_partitions: table_entry_count(&database, GRAPH_PARTITIONS).unwrap_or(0),
            resolver_entries: table_entry_count(&database, RESOLVER_SNAPSHOT_PER_FILE).unwrap_or(0),
            import_graph_present: table_has_key(
                &database,
                RESOLVER_IMPORT_GRAPH,
                "resolver_import_graph",
            )
            .unwrap_or(false),
            error: None,
        },
        Err(error) => GraphCacheStats {
            schema_version: None,
            graph_partitions: 0,
            resolver_entries: 0,
            import_graph_present: false,
            error: Some(error.to_string()),
        },
    }
}

fn graph_v3_cache_stats(root: &Path) -> GraphCacheStats {
    let manifest_path = root.join(GRAPH_MANIFEST_FILE_NAME);
    let manifest = match ReadOnlyDatabase::open(&manifest_path).map_err(store_error) {
        Ok(database) => database,
        Err(error) => {
            return GraphCacheStats {
                schema_version: None,
                graph_partitions: 0,
                resolver_entries: 0,
                import_graph_present: false,
                error: Some(error.to_string()),
            };
        }
    };
    let schema_version = current_schema_version(&manifest).ok().flatten();
    let placements = graph_v3_file_placements_for_stats(&manifest).unwrap_or_default();
    let mut graph_partitions = 0usize;
    let mut resolver_entries = 0usize;
    let mut shard_keys = placements
        .iter()
        .map(|(_, key)| key.clone())
        .collect::<Vec<_>>();
    shard_keys.sort();
    shard_keys.dedup();
    for key in shard_keys {
        let shard = root
            .join(GRAPH_SHARDS_DIR_NAME)
            .join(key.language.as_str())
            .join(format!("{:02}.redb", key.bucket));
        let Ok(database) = ReadOnlyDatabase::open(&shard).map_err(store_error) else {
            continue;
        };
        graph_partitions = graph_partitions
            .saturating_add(table_entry_count(&database, GRAPH_PARTITIONS).unwrap_or(0));
        resolver_entries = resolver_entries
            .saturating_add(table_entry_count(&database, RESOLVER_SNAPSHOT_PER_FILE).unwrap_or(0));
    }
    let global = root.join(GRAPH_GLOBAL_FILE_NAME);
    let import_graph_present = ReadOnlyDatabase::open(&global)
        .map_err(store_error)
        .ok()
        .and_then(|database| {
            table_has_key(&database, RESOLVER_IMPORT_GRAPH, "resolver_import_graph").ok()
        })
        .unwrap_or(false);
    GraphCacheStats {
        schema_version,
        graph_partitions,
        resolver_entries,
        import_graph_present,
        error: None,
    }
}

fn graph_v3_file_placements_for_stats(
    manifest: &impl ReadableDatabase,
) -> Result<Vec<(FileId, GraphShardKey)>> {
    let read = manifest.begin_read().map_err(store_error)?;
    let table = match read.open_table(GRAPH_FILE_SHARDS) {
        Ok(table) => table,
        Err(_) => return Ok(Vec::new()),
    };
    let mut placements = Vec::new();
    for entry in table.iter().map_err(store_error)? {
        let (key, value) = entry.map_err(store_error)?;
        let placement: GraphShardPlacement = decode(value.value())?;
        placements.push((
            FileId::new(key.value().to_string()),
            GraphShardKey::from(placement),
        ));
    }
    Ok(placements)
}

fn table_entry_count(
    database: &impl ReadableDatabase,
    definition: TableDefinition<&str, &[u8]>,
) -> Result<usize> {
    let read = database.begin_read().map_err(store_error)?;
    let table = match read.open_table(definition) {
        Ok(table) => table,
        Err(_) => return Ok(0),
    };
    // redb 4.x exposes a constant-time `len()` on every readable table via
    // [`ReadableTableMetadata`]. Falling back to iteration would do a full
    // table scan for what cache diagnostics treat as a one-line summary —
    // noticeable on graph caches with tens of thousands of partitions.
    table.len().map(|len| len as usize).map_err(store_error)
}

fn table_has_key(
    database: &impl ReadableDatabase,
    definition: TableDefinition<&str, &[u8]>,
    key: &str,
) -> Result<bool> {
    let read = database.begin_read().map_err(store_error)?;
    let table = match read.open_table(definition) {
        Ok(table) => table,
        Err(_) => return Ok(false),
    };
    table
        .get(key)
        .map(|value| value.is_some())
        .map_err(store_error)
}

pub fn cache_dir_path(workspace_root: &Path, cache_root: Option<&Path>) -> PathBuf {
    match cache_root {
        Some(path) if is_xdg_cache_root(path) => xdg_cache_dir_path(workspace_root),
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => workspace_root.join(path),
        None => workspace_root.join(".squeezy").join("cache"),
    }
}

pub fn session_dir_path(
    workspace_root: &Path,
    cache_root: Option<&Path>,
    session_log_dir: Option<&Path>,
) -> PathBuf {
    if let Some(path) = session_log_dir {
        return resolve_workspace_path(workspace_root, path);
    }
    if let Some(root) = cache_root {
        return cache_dir_path(workspace_root, Some(root)).join("sessions");
    }
    workspace_root.join(".squeezy").join("sessions")
}

pub fn state_path(workspace_root: &Path, cache_root: Option<&Path>) -> PathBuf {
    cache_dir_path(workspace_root, cache_root).join(STATE_FILE_NAME)
}

pub fn graph_path(workspace_root: &Path, cache_root: Option<&Path>) -> PathBuf {
    cache_dir_path(workspace_root, cache_root).join(GRAPH_FILE_NAME)
}

pub fn graph_v3_dir_path(workspace_root: &Path, cache_root: Option<&Path>) -> PathBuf {
    cache_dir_path(workspace_root, cache_root)
        .join(GRAPH_DIR_NAME)
        .join(GRAPH_V3_DIR_NAME)
}

pub fn graph_manifest_path(workspace_root: &Path, cache_root: Option<&Path>) -> PathBuf {
    graph_v3_dir_path(workspace_root, cache_root).join(GRAPH_MANIFEST_FILE_NAME)
}

pub fn graph_global_path(workspace_root: &Path, cache_root: Option<&Path>) -> PathBuf {
    graph_v3_dir_path(workspace_root, cache_root).join(GRAPH_GLOBAL_FILE_NAME)
}

fn graph_manifest_path_from_legacy_path(legacy_path: &Path) -> PathBuf {
    legacy_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(GRAPH_DIR_NAME)
        .join(GRAPH_V3_DIR_NAME)
        .join(GRAPH_MANIFEST_FILE_NAME)
}

pub(crate) fn open_database(path: &Path) -> Result<Database> {
    Database::create(path).map_err(store_error)
}

fn open_graph_v3_database(path: &Path) -> Result<Database> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let initial = open_database(path)?;
    match current_schema_version(&initial)? {
        Some(GRAPH_V3_SCHEMA_VERSION) => Ok(initial),
        Some(on_disk_version) => {
            drop(initial);
            let backup = backup_path(path, on_disk_version);
            match fs_util::rotate_file(path, &backup) {
                Ok(()) => sync_parent_dir(&backup)?,
                Err(error) => {
                    tracing::warn!(
                        target: "squeezy::store",
                        path = %path.display(),
                        error = %error,
                        "graph v3 database schema mismatch; rotation failed, replacing cache file",
                    );
                    let _ = fs::remove_file(path);
                }
            }
            let database = open_database(path)?;
            initialize_graph_v3_schema(&database)?;
            Ok(database)
        }
        None => {
            initialize_graph_v3_schema(&initial)?;
            Ok(initial)
        }
    }
}

fn ensure_graph_manifest(database: &Database) -> Result<()> {
    let write = database.begin_write().map_err(store_error)?;
    {
        let mut table = write.open_table(GRAPH_MANIFEST).map_err(store_error)?;
        if read_table_json::<GraphManifest, _>(&table, "manifest")?.is_none() {
            insert_json(&mut table, "manifest", &default_graph_manifest())?;
        }
    }
    write.commit().map_err(store_error)
}

fn default_graph_manifest() -> GraphManifest {
    GraphManifest {
        schema_version: GRAPH_V3_SCHEMA_VERSION,
        graph_format_version: GRAPH_V3_SCHEMA_VERSION,
        active_generation: 0,
        shard_count: GRAPH_SHARD_COUNT,
    }
}

pub(crate) fn initialize_schema(database: &Database) -> Result<()> {
    let write = database.begin_write().map_err(store_error)?;
    {
        let mut meta = write.open_table(META).map_err(store_error)?;
        insert_json(&mut meta, "schema_version", &SCHEMA_VERSION)?;
    }
    write.open_table(TOOL_RECEIPTS).map_err(store_error)?;
    write.open_table(READ_SNAPSHOTS).map_err(store_error)?;
    write.open_table(MCP_TOOL_CACHE).map_err(store_error)?;
    write.open_table(OBSERVATIONS).map_err(store_error)?;
    write.open_table(OBSERVATION_INDEX).map_err(store_error)?;
    write
        .open_table(COMPACTION_CHECKPOINTS)
        .map_err(store_error)?;
    write.commit().map_err(store_error)
}

fn initialize_graph_v3_schema(database: &Database) -> Result<()> {
    let write = database.begin_write().map_err(store_error)?;
    {
        let mut meta = write.open_table(META).map_err(store_error)?;
        insert_json(&mut meta, "schema_version", &GRAPH_V3_SCHEMA_VERSION)?;
    }
    write.open_table(GRAPH_MANIFEST).map_err(store_error)?;
    write.open_table(GRAPH_FILE_SHARDS).map_err(store_error)?;
    write.open_table(GRAPH_PARTITIONS).map_err(store_error)?;
    write
        .open_table(RESOLVER_SNAPSHOT_PER_FILE)
        .map_err(store_error)?;
    write
        .open_table(RESOLVER_IMPORT_GRAPH)
        .map_err(store_error)?;
    write.commit().map_err(store_error)
}

pub(crate) fn current_schema_version(database: &impl ReadableDatabase) -> Result<Option<u64>> {
    let read = database.begin_read().map_err(store_error)?;
    let table = match read.open_table(META) {
        Ok(table) => table,
        Err(_) => return Ok(None),
    };
    read_table_json(&table, "schema_version")
}

fn copy_state_tables(from: &Path, to: &Path) -> Result<()> {
    let source = open_database(from)?;
    let destination = open_database(to)?;
    for table in [
        TOOL_RECEIPTS,
        READ_SNAPSHOTS,
        MCP_TOOL_CACHE,
        OBSERVATIONS,
        OBSERVATION_INDEX,
        COMPACTION_CHECKPOINTS,
    ] {
        copy_table(&source, &destination, table)?;
    }
    Ok(())
}

fn copy_table(
    source: &Database,
    destination: &Database,
    definition: TableDefinition<&str, &[u8]>,
) -> Result<()> {
    let read = source.begin_read().map_err(store_error)?;
    let source_table = match read.open_table(definition) {
        Ok(table) => table,
        Err(_) => return Ok(()),
    };
    let write = destination.begin_write().map_err(store_error)?;
    {
        let mut destination_table = write.open_table(definition).map_err(store_error)?;
        for entry in source_table.iter().map_err(store_error)? {
            let (key, value) = entry.map_err(store_error)?;
            destination_table
                .insert(key.value(), value.value())
                .map_err(store_error)?;
        }
    }
    write.commit().map_err(store_error)
}

fn cache_file_report(path: PathBuf) -> CacheFileReport {
    match fs::metadata(&path) {
        Ok(metadata) => CacheFileReport {
            path,
            exists: true,
            size_bytes: metadata.len(),
            modified_unix_ms: metadata
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis()),
        },
        Err(_) => CacheFileReport {
            path,
            exists: false,
            size_bytes: 0,
            modified_unix_ms: None,
        },
    }
}

fn cache_tree_report(path: PathBuf) -> CacheFileReport {
    match fs::metadata(&path) {
        Ok(metadata) if metadata.is_dir() => CacheFileReport {
            size_bytes: dir_size_bytes(&path).unwrap_or(0),
            modified_unix_ms: metadata
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis()),
            path,
            exists: true,
        },
        _ => cache_file_report(path),
    }
}

fn dir_size_bytes(path: &Path) -> io::Result<u64> {
    let mut total = 0u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            total = total.saturating_add(dir_size_bytes(&entry.path())?);
        } else {
            total = total.saturating_add(metadata.len());
        }
    }
    Ok(total)
}

fn cache_backups(cache_dir: &Path) -> Result<Vec<CacheFileReport>> {
    let entries = match fs::read_dir(cache_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    let mut backups = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.ends_with(".redb.bak") {
            backups.push(cache_file_report(path));
        }
    }
    backups.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(backups)
}

fn oversized_state_needs_fast_rotate(path: &Path) -> Result<bool> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err.into()),
    };
    Ok(metadata.len() > OVERSIZED_STATE_FAST_ROTATE_BYTES)
}

fn backup_path(path: &Path, on_disk_version: u64) -> PathBuf {
    backup_path_with_label(path, &format!("schema-{on_disk_version}"))
}

fn backup_path_with_label(path: &Path, label: &str) -> PathBuf {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("store");
    let suffix = format!("{stem}-{label}-{}.redb.bak", unix_millis());
    path.with_file_name(suffix)
}

pub(crate) fn is_xdg_cache_root(path: &Path) -> bool {
    // Case-sensitive sentinel: only literal "xdg" opts into XDG resolution.
    // `"XDG"`, `"Xdg"`, and `"xdg/"` are treated as ordinary relative paths,
    // matching the lowercase-only convention used by `CacheDurability::parse`.
    path == Path::new("xdg")
}

/// Resolve the on-disk location backing `[cache].root = "xdg"`.
///
/// Walks the standard XDG chain: `$XDG_CACHE_HOME` first, then
/// `$HOME/.cache`, and finally `<workspace_root>/.squeezy/cache` when
/// neither environment variable is set (typically only happens in sandboxes
/// or test runs that strip both env vars). The repo-stable
/// `squeezy/<repo-id>` suffix is appended in every branch so the resolved
/// path survives workspace re-canonicalizations.
fn xdg_cache_dir_path(workspace_root: &Path) -> PathBuf {
    let base = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(|| workspace_root.join(".squeezy").join("cache"));
    base.join("squeezy").join(repo_settings_id(workspace_root))
}

fn resolve_workspace_path(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

/// Atomically replace `path` with `bytes`.
///
/// Writes to a unique sibling temp file, calls `sync_all` on it, then
/// `rename`s it over the destination.  On Linux, also fsyncs the parent
/// directory so the directory entry survives a crash; on other platforms the
/// parent fsync is a no-op (APFS provides equivalent rename durability without
/// it).
///
/// Temp file naming pattern: `.{name}.{pid}.{stamp}.{counter}.tmp` (hidden,
/// per-process). The sibling advisory lock used by the global session index
/// follows the matching `.{name}.compact.lock` pattern. Both names are
/// dot-prefixed so they stay out of `ls` listings; grep on either suffix when
/// debugging a half-written state.
pub(crate) fn atomic_replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        // Newly-created session directories must themselves survive a crash:
        // a grandparent fsync after the freshly created directory ensures the
        // directory entry is durable before the file inside is renamed into
        // place, closing the gap between `create_dir_all` and `fs::rename`.
        let parent_existed = parent.exists();
        fs::create_dir_all(parent)?;
        if !parent_existed {
            sync_parent_dir(parent)?;
        }
    }
    let tmp = unique_tmp_path(path);
    let result = (|| -> io::Result<()> {
        {
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&tmp)?;
            use std::io::Write as _;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        fs::rename(&tmp, path)?;
        sync_parent_dir(path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn unique_tmp_path(path: &Path) -> PathBuf {
    let pid = std::process::id();
    let stamp = unix_millis();
    let counter = NEXT_TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    match path.file_name().and_then(|name| name.to_str()) {
        Some(name) => path.with_file_name(format!(".{name}.{pid}.{stamp}.{counter}.tmp")),
        None => path.with_extension(format!("{pid}.{stamp}.{counter}.tmp")),
    }
}

static NEXT_TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub(crate) fn sync_parent_dir(path: &Path) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let Some(parent) = path.parent() else {
            return Ok(());
        };
        let dir = fs::File::open(parent)?;
        match dir.sync_all() {
            Ok(()) => Ok(()),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::InvalidInput | io::ErrorKind::Unsupported
                ) =>
            {
                tracing::warn!(
                    target: "squeezy::store",
                    path = %parent.display(),
                    error = %error,
                    "filesystem rejected directory fsync after atomic rename",
                );
                Ok(())
            }
            Err(error) => Err(error),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        Ok(())
    }
}

fn storage_reports<'a>(
    paths: impl IntoIterator<Item = (&'a str, &'a Path)>,
) -> Vec<StoragePathReport> {
    paths
        .into_iter()
        .map(|(label, path)| storage_report(label, path))
        .collect()
}

fn storage_report(label: &str, path: &Path) -> StoragePathReport {
    let mount = mount_for_path(path);
    let filesystem_type = mount.as_ref().map(|mount| mount.fs_type.clone());
    let mount_source = mount.as_ref().map(|mount| mount.source.clone());
    let classification = filesystem_type
        .as_deref()
        .map(classify_filesystem)
        .unwrap_or(StorageMountClassification::Unknown);
    let warning = storage_warning(label, classification, filesystem_type.as_deref());
    StoragePathReport {
        label: label.to_string(),
        path: path.to_path_buf(),
        mount_source,
        filesystem_type,
        classification,
        warning,
    }
}

/// Suggested config key to relocate when `label` lands on a remote/volatile mount.
/// `sessions` paths come from `[session].log_dir`; every other label
/// (`cache`, `state.redb`, `graph.redb`) is governed by `[cache].root`.
pub fn storage_relocation_hint(label: &str) -> &'static str {
    match label {
        "sessions" => "[session].log_dir",
        _ => "[cache].root",
    }
}

fn storage_warning(
    label: &str,
    classification: StorageMountClassification,
    filesystem_type: Option<&str>,
) -> Option<String> {
    let hint = storage_relocation_hint(label);
    match classification {
        StorageMountClassification::Network => Some(format!(
            "{} filesystems can surprise redb locking, mmap, rename, or fsync; move {hint} to a local SSD path",
            filesystem_type.unwrap_or("network")
        )),
        StorageMountClassification::Virtual => Some(format!(
            "{} filesystems can make cache locking or fsync slower or less durable; move {hint} to a local SSD path",
            filesystem_type.unwrap_or("virtual")
        )),
        StorageMountClassification::Local | StorageMountClassification::Unknown => None,
    }
}

fn classify_filesystem(fs_type: &str) -> StorageMountClassification {
    let fs_type = fs_type.to_ascii_lowercase();
    match fs_type.as_str() {
        "nfs" | "nfs4" | "cifs" | "smb" | "smb2" | "smb3" | "sshfs" | "9p" | "afs" | "ceph"
        | "glusterfs" | "davfs" => StorageMountClassification::Network,
        "fuse" | "fuseblk" | "fuse.sshfs" | "overlay" | "overlayfs" | "aufs" | "unionfs"
        | "virtiofs" | "proc" => StorageMountClassification::Virtual,
        "ext2" | "ext3" | "ext4" | "xfs" | "btrfs" | "apfs" | "hfs" | "hfsplus" | "zfs"
        | "f2fs" | "ufs" => StorageMountClassification::Local,
        // tmpfs is RAM-backed and volatile: contents are lost on reboot/unmount.
        // Classify it alongside virtual/container filesystems so the durability
        // warning fires when cache or session paths land on a tmpfs mount.
        "tmpfs" => StorageMountClassification::Virtual,
        _ => StorageMountClassification::Unknown,
    }
}

#[derive(Debug, Clone)]
struct MountEntry {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    mount_point: PathBuf,
    fs_type: String,
    source: String,
}

fn mount_for_path(path: &Path) -> Option<MountEntry> {
    #[cfg(target_os = "linux")]
    {
        let entries = parse_linux_mountinfo(&fs::read_to_string("/proc/self/mountinfo").ok()?);
        let canonical = path
            .canonicalize()
            .or_else(|_| {
                path.parent()
                    .map(Path::canonicalize)
                    .unwrap_or_else(|| Ok(path.to_path_buf()))
            })
            .unwrap_or_else(|_| path.to_path_buf());
        entries
            .into_iter()
            .filter(|entry| canonical.starts_with(&entry.mount_point))
            .max_by_key(|entry| entry.mount_point.as_os_str().len())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        None
    }
}

#[cfg(target_os = "linux")]
fn parse_linux_mountinfo(contents: &str) -> Vec<MountEntry> {
    contents
        .lines()
        .filter_map(parse_linux_mountinfo_line)
        .collect()
}

#[cfg(target_os = "linux")]
fn parse_linux_mountinfo_line(line: &str) -> Option<MountEntry> {
    // Per `proc(5)`, the variable-length optional-fields tail between
    // `mount_point` and the `" - "` separator is consumed by `split_once`
    // (we never count those fields directly); the post-separator side begins
    // unconditionally with `fs_type` then `mount_source`.
    let (pre, post) = line.split_once(" - ")?;
    let mut pre_fields = pre.split_whitespace();
    let _mount_id = pre_fields.next()?;
    let _parent_id = pre_fields.next()?;
    let _major_minor = pre_fields.next()?;
    let _root = pre_fields.next()?;
    let mount_point = unescape_mountinfo_path(pre_fields.next()?);
    let mut post_fields = post.split_whitespace();
    let fs_type = post_fields.next()?.to_string();
    let source = post_fields
        .next()
        .map(unescape_mountinfo_path)
        .unwrap_or_default();
    Some(MountEntry {
        mount_point: PathBuf::from(mount_point),
        fs_type,
        source,
    })
}

#[cfg(target_os = "linux")]
fn unescape_mountinfo_path(value: &str) -> String {
    // Accumulate raw bytes so multi-byte UTF-8 sequences encoded as consecutive
    // octal escapes (e.g. \303\251 for 'é') are decoded correctly.
    let mut raw: Vec<u8> = Vec::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            let mut octal = String::new();
            for _ in 0..3 {
                match chars.peek().copied() {
                    Some(next) if next.is_ascii_digit() => {
                        octal.push(next);
                        chars.next();
                    }
                    _ => break,
                }
            }
            if octal.len() == 3
                && let Ok(byte) = u8::from_str_radix(&octal, 8)
            {
                raw.push(byte);
                continue;
            }
            // Not a valid octal escape — emit the backslash and any digits we consumed.
            raw.push(b'\\');
            raw.extend_from_slice(octal.as_bytes());
        } else {
            // ASCII-only char in mountinfo field names; push its UTF-8 byte(s).
            let mut buf = [0u8; 4];
            raw.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
        }
    }
    String::from_utf8_lossy(&raw).into_owned()
}

fn insert_json<T: Serialize>(
    table: &mut redb::Table<'_, &str, &[u8]>,
    key: &str,
    value: &T,
) -> Result<()> {
    let encoded = encode(value)?;
    table.insert(key, encoded.as_slice()).map_err(store_error)?;
    Ok(())
}

fn read_table_json<T: DeserializeOwned, K: AsRef<str>>(
    table: &impl ReadableTable<&'static str, &'static [u8]>,
    key: K,
) -> Result<Option<T>> {
    let Some(value) = table.get(key.as_ref()).map_err(store_error)? else {
        return Ok(None);
    };
    decode(value.value()).map(Some)
}

fn clear_table(
    write: &redb::WriteTransaction,
    definition: TableDefinition<&str, &[u8]>,
) -> Result<()> {
    let mut table = write.open_table(definition).map_err(store_error)?;
    let keys = table
        .iter()
        .map_err(store_error)?
        .map(|entry| entry.map(|(key, _)| key.value().to_string()))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(store_error)?;
    for key in keys {
        table.remove(key.as_str()).map_err(store_error)?;
    }
    Ok(())
}

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value)
        .map_err(|err| SqueezyError::Tool(format!("store encode failed: {err}")))
}

fn decode<T: DeserializeOwned>(value: &[u8]) -> Result<T> {
    serde_json::from_slice(value)
        .map_err(|err| SqueezyError::Tool(format!("store decode failed: {err}")))
}

/// Compressed codec for the large per-repo graph tables (`GRAPH_PARTITIONS`,
/// `RESOLVER_SNAPSHOT_PER_FILE`, `RESOLVER_IMPORT_GRAPH`). Partition values are
/// highly repetitive JSON (field names, identifiers, paths, keywords repeated
/// across thousands of symbols), so DEFLATE shrinks them by ~an order of
/// magnitude on disk and cuts the bytes redb must read back at warm start. The
/// `meta` table (which holds the `graph_metadata` format-version gate) and
/// every state.redb table stay on the plain-JSON `encode`/`decode` so the
/// version check is always readable and resumable sessions are never
/// invalidated by a graph-codec change. Bumping `graph_format_version` rebuilds
/// any pre-compression cache, so a reader never inflates plain-JSON bytes.
fn encode_graph<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    use std::io::Write as _;
    let json = serde_json::to_vec(value)
        .map_err(|err| SqueezyError::Tool(format!("store graph encode failed: {err}")))?;
    let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder
        .write_all(&json)
        .map_err(|err| SqueezyError::Tool(format!("store graph compress failed: {err}")))?;
    encoder
        .finish()
        .map_err(|err| SqueezyError::Tool(format!("store graph compress failed: {err}")))
}

fn decode_graph<T: DeserializeOwned>(value: &[u8]) -> Result<T> {
    use std::io::Read as _;
    let mut json = Vec::new();
    flate2::read::DeflateDecoder::new(value)
        .read_to_end(&mut json)
        .map_err(|err| SqueezyError::Tool(format!("store graph decompress failed: {err}")))?;
    serde_json::from_slice(&json)
        .map_err(|err| SqueezyError::Tool(format!("store graph decode failed: {err}")))
}

fn read_graph<T: DeserializeOwned, K: AsRef<str>>(
    table: &impl ReadableTable<&'static str, &'static [u8]>,
    key: K,
) -> Result<Option<T>> {
    let Some(value) = table.get(key.as_ref()).map_err(store_error)? else {
        return Ok(None);
    };
    decode_graph(value.value()).map(Some)
}

fn encode_versioned_graph_payload(generation: u64, payload: &[u8]) -> Result<Vec<u8>> {
    encode_graph(&VersionedGraphPayload {
        generation,
        payload: payload.to_vec(),
    })
}

fn read_versioned_graph<T: DeserializeOwned, K: AsRef<str>>(
    table: &impl ReadableTable<&'static str, &'static [u8]>,
    key: K,
    active_generation: u64,
) -> Result<Option<T>> {
    let Some(value) = table.get(key.as_ref()).map_err(store_error)? else {
        return Ok(None);
    };
    match decode_graph::<VersionedGraphPayload>(value.value()) {
        Ok(wrapper) if wrapper.generation <= active_generation => {
            decode_graph(wrapper.payload.as_slice()).map(Some)
        }
        Ok(_) => Ok(None),
        Err(_) => decode_graph(value.value()).map(Some),
    }
}

fn legacy_database(path: &Path) -> Option<ReadOnlyDatabase> {
    if !path.exists() {
        return None;
    }
    ReadOnlyDatabase::open(path).ok()
}

fn legacy_graph_metadata(path: &Path) -> Result<Option<GraphStoreMetadata>> {
    let Some(database) = legacy_database(path) else {
        return Ok(None);
    };
    let read = database.begin_read().map_err(store_error)?;
    let table = match read.open_table(META) {
        Ok(table) => table,
        Err(_) => return Ok(None),
    };
    read_table_json(&table, "graph_metadata")
}

fn legacy_graph_partition<T: DeserializeOwned>(path: &Path, file_id: &FileId) -> Result<Option<T>> {
    let Some(database) = legacy_database(path) else {
        return Ok(None);
    };
    let read = database.begin_read().map_err(store_error)?;
    let table = match read.open_table(GRAPH_PARTITIONS) {
        Ok(table) => table,
        Err(_) => return Ok(None),
    };
    read_graph(&table, file_id.0.as_str())
}

fn legacy_graph_partition_entries<T: DeserializeOwned>(path: &Path) -> Result<Vec<(FileId, T)>> {
    let Some(database) = legacy_database(path) else {
        return Ok(Vec::new());
    };
    let read = database.begin_read().map_err(store_error)?;
    let table = match read.open_table(GRAPH_PARTITIONS) {
        Ok(table) => table,
        Err(_) => return Ok(Vec::new()),
    };
    let mut entries = Vec::new();
    for entry in table.iter().map_err(store_error)? {
        let (key, value) = entry.map_err(store_error)?;
        let decoded: T = decode_graph(value.value())?;
        entries.push((FileId::new(key.value().to_string()), decoded));
    }
    Ok(entries)
}

fn legacy_resolver_entry<T: DeserializeOwned>(path: &Path, file_id: &FileId) -> Result<Option<T>> {
    let Some(database) = legacy_database(path) else {
        return Ok(None);
    };
    let read = database.begin_read().map_err(store_error)?;
    let table = match read.open_table(RESOLVER_SNAPSHOT_PER_FILE) {
        Ok(table) => table,
        Err(_) => return Ok(None),
    };
    read_graph(&table, file_id.0.as_str())
}

fn legacy_import_graph<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let Some(database) = legacy_database(path) else {
        return Ok(None);
    };
    let read = database.begin_read().map_err(store_error)?;
    let table = match read.open_table(RESOLVER_IMPORT_GRAPH) {
        Ok(table) => table,
        Err(_) => return Ok(None),
    };
    read_graph(&table, "resolver_import_graph")
}

fn language_shard_name(language: LanguageKind) -> &'static str {
    match language {
        LanguageKind::C => "c",
        LanguageKind::CSharp => "csharp",
        LanguageKind::Cpp => "cpp",
        LanguageKind::Dart => "dart",
        LanguageKind::Go => "go",
        LanguageKind::Java => "java",
        LanguageKind::JavaScript => "javascript",
        LanguageKind::Jsx => "jsx",
        LanguageKind::Kotlin => "kotlin",
        LanguageKind::Php => "php",
        LanguageKind::Python => "python",
        LanguageKind::Ruby => "ruby",
        LanguageKind::Rust => "rust",
        LanguageKind::Scala => "scala",
        LanguageKind::Swift => "swift",
        LanguageKind::TypeScript => "typescript",
        LanguageKind::Tsx => "tsx",
        LanguageKind::Unsupported => "unsupported",
        LanguageKind::Unknown => "unknown",
    }
}

fn store_error(error: impl std::fmt::Display) -> SqueezyError {
    // The Windows storage hint mentions file locks / AV / sync clients, which
    // is appropriate for IO-shaped failures (sharing violation, access denied,
    // etc.) and misleading for JSON decode, schema migration, or other
    // redb-internal errors. `std::io::Error`'s `Display` impl always includes
    // the `(os error N)` suffix, so we gate the hint on that marker — IO
    // errors keep the explanation, structured-data errors stay terse.
    let text = error.to_string();
    let formatted = if text.contains("os error") {
        fs_util::windows_storage_hint(&text)
    } else {
        text
    };
    SqueezyError::Tool(format!("store error: {formatted}"))
}

fn receipt_key(tool_name: &str, stable_output_sha256: &str) -> String {
    format!("{tool_name}\0{stable_output_sha256}")
}

/// Composite key for read snapshots. Keys are `<path>\0<start_byte>\0<end_byte>`
/// with zero-padded byte offsets so the lexicographic ordering of redb keys
/// matches the natural numeric ordering of `(start_byte, end_byte)` within a
/// given path. This lets multiple windows of the same file coexist instead of
/// the most recent read clobbering older ones.
fn read_snapshot_key(path: &str, start_byte: u64, end_byte: u64) -> String {
    format!("{path}\0{start_byte:020}\0{end_byte:020}")
}

/// Prefix used to scan every snapshot belonging to `path`.
fn read_snapshot_key_prefix(path: &str) -> String {
    format!("{path}\0")
}

fn observation_tokens(observation: &Observation) -> BTreeSet<String> {
    let mut tokens = tokenize(&observation.text);
    tokens.extend(tokenize(&observation.source));
    for tag in &observation.tags {
        tokens.extend(tokenize(tag));
    }
    tokens.insert(format!("{:?}", observation.kind).to_ascii_lowercase());
    tokens
}

fn observation_matches(observation: &Observation, query_tokens: &BTreeSet<String>) -> bool {
    if query_tokens.is_empty() {
        return true;
    }
    let tokens = observation_tokens(observation);
    query_tokens.iter().all(|token| tokens.contains(token))
}

fn tokenize(text: &str) -> BTreeSet<String> {
    text.split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
        .filter_map(|token| {
            let token = token.trim().to_ascii_lowercase();
            (!token.is_empty()).then_some(token)
        })
        .collect()
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn unix_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}
