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
//! * `graph.redb` - semantic graph partitions and resolver-cache snapshots.

use std::{
    collections::BTreeSet,
    env, fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use squeezy_core::{FileId, Result, SqueezyError, repo_settings_id};

pub mod migrations;
pub mod repo_profile;
pub mod reports;
pub mod sessions;

pub use migrations::{
    Migration, MigrationRegistry, default_registry, run_migrations, run_registry,
};
pub use repo_profile::*;
pub use reports::*;
pub use sessions::*;

pub const CRATE_NAME: &str = "squeezy-store";
pub const SCHEMA_VERSION: u64 = 3;
pub const GRAPH_SCHEMA_VERSION: u64 = 1;
pub const STATE_FILE_NAME: &str = "state.redb";
pub const GRAPH_FILE_NAME: &str = "graph.redb";

const OVERSIZED_STATE_FAST_ROTATE_BYTES: u64 = 256 * 1024 * 1024;

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;

const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
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
}

#[derive(Debug)]
pub struct GraphStore {
    path: PathBuf,
    database: Database,
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
            fs::rename(&path, &backup)?;
            sync_parent_dir(&backup)?;
            bootstrap_store(workspace_root, cache_root)?;
            tracing::warn!(
                target: "squeezy::store",
                threshold_bytes = OVERSIZED_STATE_FAST_ROTATE_BYTES,
                backup = %backup.display(),
                "state.redb exceeded the split-cache threshold; existing cache backed up without opening redb",
            );
            let database = open_database(&path)?;
            return Ok(Self { path, database });
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
                fs::rename(&path, &backup)?;
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
                open_database(&path)?
            }
            None => {
                drop(initial);
                bootstrap_store(workspace_root, cache_root)?;
                open_database(&path)?
            }
        };
        Ok(Self { path, database })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn set_graph_metadata(&self, metadata: &GraphStoreMetadata) -> Result<()> {
        self.graph_store()?.set_graph_metadata(metadata)
    }

    pub fn graph_metadata(&self) -> Result<Option<GraphStoreMetadata>> {
        self.graph_store()?.graph_metadata()
    }

    pub fn put_graph_partition<T: Serialize>(&self, file_id: &FileId, partition: &T) -> Result<()> {
        self.graph_store()?.put_graph_partition(file_id, partition)
    }

    pub fn graph_partition<T: DeserializeOwned>(&self, file_id: &FileId) -> Result<Option<T>> {
        self.graph_store()?.graph_partition(file_id)
    }

    pub fn remove_graph_partition(&self, file_id: &FileId) -> Result<()> {
        self.graph_store()?.remove_graph_partition(file_id)
    }

    pub fn clear_graph_partitions(&self) -> Result<()> {
        self.graph_store()?.clear_graph_partitions()
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
        self.graph_store()?.apply_graph_batch(batch)
    }

    /// Upsert a per-file resolver snapshot into the V2 resolver cache.
    /// Callers should fingerprint the file (modified-time + size) into the
    /// stored value so a later open can decide whether the snapshot is
    /// still authoritative.
    pub fn put_resolver_entry<T: Serialize>(&self, file_id: &FileId, entry: &T) -> Result<()> {
        self.graph_store()?.put_resolver_entry(file_id, entry)
    }

    pub fn resolver_entry<T: DeserializeOwned>(&self, file_id: &FileId) -> Result<Option<T>> {
        self.graph_store()?.resolver_entry(file_id)
    }

    pub fn resolver_entries_for<T: DeserializeOwned>(
        &self,
        file_ids: &[FileId],
    ) -> Result<Vec<(FileId, T)>> {
        self.graph_store()?.resolver_entries_for(file_ids)
    }

    pub fn remove_resolver_entry(&self, file_id: &FileId) -> Result<()> {
        self.graph_store()?.remove_resolver_entry(file_id)
    }

    pub fn clear_resolver_entries(&self) -> Result<()> {
        self.graph_store()?.clear_resolver_entries()
    }

    /// Replace the persisted file-level import adjacency blob. Stored under
    /// one key so reading on warm-start is a single table get.
    pub fn put_import_graph<T: Serialize>(&self, graph: &T) -> Result<()> {
        self.graph_store()?.put_import_graph(graph)
    }

    pub fn import_graph<T: DeserializeOwned>(&self) -> Result<Option<T>> {
        self.graph_store()?.import_graph()
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

    fn graph_store(&self) -> Result<GraphStore> {
        GraphStore::open_path(self.path.with_file_name(GRAPH_FILE_NAME))
    }
}

impl GraphStore {
    pub fn open(workspace_root: impl AsRef<Path>, cache_root: Option<&Path>) -> Result<Self> {
        let path = graph_path(workspace_root.as_ref(), cache_root);
        Self::open_path(path)
    }

    pub fn open_path(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let initial = open_database(&path)?;
        let database = match current_schema_version(&initial)? {
            Some(GRAPH_SCHEMA_VERSION) => initial,
            Some(on_disk_version) => {
                drop(initial);
                let backup = backup_path(&path, on_disk_version);
                fs::rename(&path, &backup)?;
                sync_parent_dir(&backup)?;
                tracing::warn!(
                    target: "squeezy::store",
                    on_disk_version,
                    schema_version = GRAPH_SCHEMA_VERSION,
                    backup = %backup.display(),
                    "graph.redb schema mismatch; existing graph cache backed up and reinitialised",
                );
                bootstrap_graph_store(&path)?;
                open_database(&path)?
            }
            None => {
                drop(initial);
                bootstrap_graph_store(&path)?;
                open_database(&path)?
            }
        };
        Ok(Self { path, database })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn set_graph_metadata(&self, metadata: &GraphStoreMetadata) -> Result<()> {
        let write = self.begin_write()?;
        {
            let mut meta = write.open_table(META).map_err(store_error)?;
            insert_json(&mut meta, "graph_metadata", metadata)?;
        }
        write.commit().map_err(store_error)
    }

    pub fn graph_metadata(&self) -> Result<Option<GraphStoreMetadata>> {
        let read = self.database.begin_read().map_err(store_error)?;
        let table = match read.open_table(META) {
            Ok(table) => table,
            Err(_) => return Ok(None),
        };
        read_table_json(&table, "graph_metadata")
    }

    pub fn put_graph_partition<T: Serialize>(&self, file_id: &FileId, partition: &T) -> Result<()> {
        let write = self.begin_write()?;
        {
            let mut table = write.open_table(GRAPH_PARTITIONS).map_err(store_error)?;
            insert_json(&mut table, file_id.0.as_str(), partition)?;
        }
        write.commit().map_err(store_error)
    }

    pub fn graph_partition<T: DeserializeOwned>(&self, file_id: &FileId) -> Result<Option<T>> {
        let read = self.database.begin_read().map_err(store_error)?;
        let table = match read.open_table(GRAPH_PARTITIONS) {
            Ok(table) => table,
            Err(_) => return Ok(None),
        };
        read_table_json(&table, file_id.0.as_str())
    }

    pub fn remove_graph_partition(&self, file_id: &FileId) -> Result<()> {
        let write = self.begin_write()?;
        {
            let mut table = write.open_table(GRAPH_PARTITIONS).map_err(store_error)?;
            table.remove(file_id.0.as_str()).map_err(store_error)?;
        }
        write.commit().map_err(store_error)
    }

    pub fn clear_graph_partitions(&self) -> Result<()> {
        let write = self.begin_write()?;
        {
            clear_table(&write, GRAPH_PARTITIONS)?;
        }
        write.commit().map_err(store_error)
    }

    pub fn apply_graph_batch(&self, batch: &GraphWriteBatch) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let write = self.begin_write()?;
        if let Some(metadata) = &batch.metadata {
            let mut meta = write.open_table(META).map_err(store_error)?;
            insert_json(&mut meta, "graph_metadata", metadata)?;
        }
        if !batch.upserts.is_empty() || !batch.removals.is_empty() {
            let mut table = write.open_table(GRAPH_PARTITIONS).map_err(store_error)?;
            for (key, value) in &batch.upserts {
                table
                    .insert(key.as_str(), value.as_slice())
                    .map_err(store_error)?;
            }
            for key in &batch.removals {
                table.remove(key.as_str()).map_err(store_error)?;
            }
        }
        if !batch.resolver_upserts.is_empty() || !batch.resolver_removals.is_empty() {
            let mut table = write
                .open_table(RESOLVER_SNAPSHOT_PER_FILE)
                .map_err(store_error)?;
            for (key, value) in &batch.resolver_upserts {
                table
                    .insert(key.as_str(), value.as_slice())
                    .map_err(store_error)?;
            }
            for key in &batch.resolver_removals {
                table.remove(key.as_str()).map_err(store_error)?;
            }
        }
        if let Some(import_graph) = &batch.import_graph {
            let mut table = write
                .open_table(RESOLVER_IMPORT_GRAPH)
                .map_err(store_error)?;
            table
                .insert("resolver_import_graph", import_graph.as_slice())
                .map_err(store_error)?;
        }
        write.commit().map_err(store_error)
    }

    pub fn put_resolver_entry<T: Serialize>(&self, file_id: &FileId, entry: &T) -> Result<()> {
        let mut batch = GraphWriteBatch::new();
        batch.upsert_resolver_entry(file_id, entry)?;
        self.apply_graph_batch(&batch)
    }

    pub fn resolver_entry<T: DeserializeOwned>(&self, file_id: &FileId) -> Result<Option<T>> {
        let read = self.database.begin_read().map_err(store_error)?;
        let table = match read.open_table(RESOLVER_SNAPSHOT_PER_FILE) {
            Ok(table) => table,
            Err(_) => return Ok(None),
        };
        read_table_json(&table, file_id.0.as_str())
    }

    pub fn resolver_entries_for<T: DeserializeOwned>(
        &self,
        file_ids: &[FileId],
    ) -> Result<Vec<(FileId, T)>> {
        let read = self.database.begin_read().map_err(store_error)?;
        let table = match read.open_table(RESOLVER_SNAPSHOT_PER_FILE) {
            Ok(table) => table,
            Err(_) => return Ok(Vec::new()),
        };
        let mut out = Vec::with_capacity(file_ids.len());
        for file_id in file_ids {
            if let Some(value) = read_table_json(&table, file_id.0.as_str())? {
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
        let write = self.begin_write()?;
        {
            clear_table(&write, RESOLVER_SNAPSHOT_PER_FILE)?;
        }
        write.commit().map_err(store_error)
    }

    pub fn put_import_graph<T: Serialize>(&self, graph: &T) -> Result<()> {
        let mut batch = GraphWriteBatch::new();
        batch.set_import_graph(graph)?;
        self.apply_graph_batch(&batch)
    }

    pub fn import_graph<T: DeserializeOwned>(&self) -> Result<Option<T>> {
        let read = self.database.begin_read().map_err(store_error)?;
        let table = match read.open_table(RESOLVER_IMPORT_GRAPH) {
            Ok(table) => table,
            Err(_) => return Ok(None),
        };
        read_table_json(&table, "resolver_import_graph")
    }

    fn begin_write(&self) -> Result<redb::WriteTransaction> {
        self.database.begin_write().map_err(store_error)
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
    upserts: Vec<(String, Vec<u8>)>,
    removals: Vec<String>,
    resolver_upserts: Vec<(String, Vec<u8>)>,
    resolver_removals: Vec<String>,
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
        let encoded = encode(partition)?;
        self.upserts.push((file_id.0.clone(), encoded));
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
        let encoded = encode(entry)?;
        self.resolver_upserts.push((file_id.0.clone(), encoded));
        Ok(())
    }

    pub fn remove_resolver_entry(&mut self, file_id: &FileId) {
        self.resolver_removals.push(file_id.0.clone());
    }

    pub fn set_import_graph<T: Serialize>(&mut self, graph: &T) -> Result<()> {
        self.import_graph = Some(encode(graph)?);
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
        self.upserts.len()
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachePruneReport {
    pub removed_files: Vec<CacheFileReport>,
    pub removed_bytes: u64,
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
    let graph = cache_file_report(graph_path(workspace_root, cache_root));
    let backups = cache_backups(&cache_dir)?;
    let backup_total_bytes = backups.iter().map(|file| file.size_bytes).sum();
    let session_dir = session_dir_path(workspace_root, cache_root, session_log_dir);
    let storage = storage_reports([
        ("cache", cache_dir.as_path()),
        ("sessions", session_dir.as_path()),
        ("state.redb", state.path.as_path()),
        ("graph.redb", graph.path.as_path()),
    ]);
    Ok(CacheDiagnostics {
        cache_dir,
        state,
        graph,
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
    let mut removed_bytes = 0;
    for backup in diagnostics.backups {
        match fs::remove_file(&backup.path) {
            Ok(()) => {
                removed_bytes += backup.size_bytes;
                removed_files.push(backup);
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(CachePruneReport {
        removed_files,
        removed_bytes,
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

fn bootstrap_graph_store(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let database = open_database(path)?;
    initialize_graph_schema(&database)
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

pub(crate) fn open_database(path: &Path) -> Result<Database> {
    Database::create(path).map_err(store_error)
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

pub(crate) fn initialize_graph_schema(database: &Database) -> Result<()> {
    let write = database.begin_write().map_err(store_error)?;
    {
        let mut meta = write.open_table(META).map_err(store_error)?;
        insert_json(&mut meta, "schema_version", &GRAPH_SCHEMA_VERSION)?;
    }
    write.open_table(GRAPH_PARTITIONS).map_err(store_error)?;
    write
        .open_table(RESOLVER_SNAPSHOT_PER_FILE)
        .map_err(store_error)?;
    write
        .open_table(RESOLVER_IMPORT_GRAPH)
        .map_err(store_error)?;
    write.commit().map_err(store_error)
}

pub(crate) fn current_schema_version(database: &Database) -> Result<Option<u64>> {
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
    let suffix = format!("{label}-{}.redb.bak", unix_millis());
    path.with_file_name(suffix)
}

pub(crate) fn is_xdg_cache_root(path: &Path) -> bool {
    path == Path::new("xdg")
}

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

pub(crate) fn atomic_replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
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
        return match dir.sync_all() {
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
        };
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
    let warning = storage_warning(classification, filesystem_type.as_deref());
    StoragePathReport {
        label: label.to_string(),
        path: path.to_path_buf(),
        mount_source,
        filesystem_type,
        classification,
        warning,
    }
}

fn storage_warning(
    classification: StorageMountClassification,
    filesystem_type: Option<&str>,
) -> Option<String> {
    match classification {
        StorageMountClassification::Network => Some(format!(
            "{} filesystems can surprise redb locking, mmap, rename, or fsync; move [cache].root to a local SSD path",
            filesystem_type.unwrap_or("network")
        )),
        StorageMountClassification::Virtual => Some(format!(
            "{} filesystems can make cache locking or fsync slower or less durable; prefer a local SSD cache path",
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
        | "virtiofs" => StorageMountClassification::Virtual,
        "ext2" | "ext3" | "ext4" | "xfs" | "btrfs" | "apfs" | "hfs" | "hfsplus" | "zfs"
        | "tmpfs" | "f2fs" | "ufs" => StorageMountClassification::Local,
        _ => StorageMountClassification::Unknown,
    }
}

#[derive(Debug, Clone)]
struct MountEntry {
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
            .max_by_key(|entry| entry.mount_point.to_string_lossy().len())
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
    let (pre, post) = line.split_once(" - ")?;
    let mut pre_fields = pre.split_whitespace();
    let _mount_id = pre_fields.next()?;
    let _parent_id = pre_fields.next()?;
    let _major_minor = pre_fields.next()?;
    let _root = pre_fields.next()?;
    let mount_point = unescape_mountinfo_path(pre_fields.next()?);
    let mut post_fields = post.split_whitespace();
    let fs_type = post_fields.next()?.to_string();
    let source = post_fields.next().unwrap_or("").to_string();
    Some(MountEntry {
        mount_point: PathBuf::from(mount_point),
        fs_type,
        source,
    })
}

#[cfg(target_os = "linux")]
fn unescape_mountinfo_path(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
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
                out.push(byte as char);
                continue;
            }
            out.push(ch);
            out.push_str(&octal);
        } else {
            out.push(ch);
        }
    }
    out
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

fn store_error(error: impl std::fmt::Display) -> SqueezyError {
    SqueezyError::Tool(format!("store error: {error}"))
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
