//! Local persistence layer for Squeezy.
//!
//! This crate hosts independent on-disk stores that share little code beyond a
//! few small helpers, but live together because both are part of the local-state
//! surface (and so consumers can reach them through a single `squeezy-store`
//! dependency).
//!
//! * `repo_profile` - generated per-repo facts (`~/.squeezy/repos.toml`).
//! * `sessions` - per-session metadata and event logs.
//! * `state.redb` - graph partitions, receipt metadata, read snapshots
//!   (keyed by `(path, start_byte, end_byte)` so distinct windows of the same
//!   file do not overwrite each other), and observations.

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use squeezy_core::{FileId, Result, SqueezyError};

pub mod repo_profile;
pub mod reports;
pub mod sessions;

pub use repo_profile::*;
pub use reports::*;
pub use sessions::*;

pub const CRATE_NAME: &str = "squeezy-store";
pub const SCHEMA_VERSION: u64 = 1;

const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
const GRAPH_PARTITIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("graph_partitions");
const TOOL_RECEIPTS: TableDefinition<&str, &[u8]> = TableDefinition::new("tool_receipts");
const READ_SNAPSHOTS: TableDefinition<&str, &[u8]> = TableDefinition::new("read_snapshots");
const OBSERVATIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("observations");
const OBSERVATION_INDEX: TableDefinition<&str, &[u8]> = TableDefinition::new("observation_index");

pub fn crate_name() -> &'static str {
    CRATE_NAME
}

#[derive(Debug)]
pub struct SqueezyStore {
    path: PathBuf,
    database: Database,
}

impl SqueezyStore {
    pub fn open(workspace_root: impl AsRef<Path>, cache_root: Option<&Path>) -> Result<Self> {
        let path = state_path(workspace_root.as_ref(), cache_root);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut database = open_database(&path)?;
        match current_schema_version(&database)? {
            Some(SCHEMA_VERSION) => {}
            Some(old_version) => {
                drop(database);
                let backup = backup_path(&path, old_version);
                fs::rename(&path, &backup)?;
                database = open_database(&path)?;
                initialize_schema(&database)?;
            }
            None => initialize_schema(&database)?,
        }
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

    /// Apply a coherent set of graph changes (metadata + partition upserts and
    /// removals) inside a single redb write transaction. Callers should batch
    /// per-refresh churn through this rather than calling
    /// [`set_graph_metadata`], [`put_graph_partition`], or
    /// [`remove_graph_partition`] in a tight loop: each of those commits
    /// independently and pays a fresh fsync, which dominates wall-clock cost
    /// on a cold workspace crawl.
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
        write.commit().map_err(store_error)
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
        for entry in table.iter().map_err(store_error)? {
            let (key, value) = entry.map_err(store_error)?;
            if !key.value().starts_with(prefix.as_str()) {
                continue;
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

    pub fn is_empty(&self) -> bool {
        self.metadata.is_none() && self.upserts.is_empty() && self.removals.is_empty()
    }

    pub fn len(&self) -> usize {
        self.upserts.len() + self.removals.len()
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

fn state_path(workspace_root: &Path, cache_root: Option<&Path>) -> PathBuf {
    match cache_root {
        Some(path) if path.is_absolute() => path.join("state.redb"),
        Some(path) => workspace_root.join(path).join("state.redb"),
        None => workspace_root
            .join(".squeezy")
            .join("cache")
            .join("state.redb"),
    }
}

fn open_database(path: &Path) -> Result<Database> {
    Database::create(path).map_err(store_error)
}

fn initialize_schema(database: &Database) -> Result<()> {
    let write = database.begin_write().map_err(store_error)?;
    {
        let mut meta = write.open_table(META).map_err(store_error)?;
        insert_json(&mut meta, "schema_version", &SCHEMA_VERSION)?;
    }
    write.open_table(GRAPH_PARTITIONS).map_err(store_error)?;
    write.open_table(TOOL_RECEIPTS).map_err(store_error)?;
    write.open_table(READ_SNAPSHOTS).map_err(store_error)?;
    write.open_table(OBSERVATIONS).map_err(store_error)?;
    write.open_table(OBSERVATION_INDEX).map_err(store_error)?;
    write.commit().map_err(store_error)
}

fn current_schema_version(database: &Database) -> Result<Option<u64>> {
    let read = database.begin_read().map_err(store_error)?;
    let table = match read.open_table(META) {
        Ok(table) => table,
        Err(_) => return Ok(None),
    };
    read_table_json(&table, "schema_version")
}

fn backup_path(path: &Path, old_version: u64) -> PathBuf {
    let suffix = format!("schema-{old_version}-{}.redb.bak", unix_millis());
    path.with_file_name(suffix)
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
