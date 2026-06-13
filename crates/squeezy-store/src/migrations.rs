//! Centralised forward-only migrations for the squeezy workspace state.
//!
//! Until this module existed, schema bootstrap lived inline in
//! [`crate::SqueezyStore::open`] as a single call to `initialize_schema`.
//! That worked while there was exactly one migration step (creating the
//! v1 redb tables and stamping `schema_version = 1`), but does not scale:
//! every new migration would have to thread through the same call site,
//! and there is no shared concept of "what version are we at and what
//! migrations still need to run".
//!
//! This module introduces:
//!
//! * [`Migration`] — a trait every forward migration implements, exposing
//!   its target [`Migration::version`] and an idempotent [`Migration::migrate`].
//! * [`MigrationRegistry`] — an ordered collection of migrations that
//!   knows how to run every migration whose `version()` is strictly
//!   greater than a supplied current version.
//! * [`run_migrations`] — the public orchestrator. Reads the current
//!   on-disk schema version stamped at `<cwd>/.squeezy/cache/state.redb`
//!   (treating "no file" or "no `schema_version` entry" as version 0),
//!   then runs every registered migration in ascending version order.
//!
//! New migrations register themselves in [`default_registry`] and only
//! need to implement the trait — the orchestrator handles version
//! gating, ordering, and the no-op case where the store is already at
//! the target version.

use std::{fs, path::Path};

use redb::TableDefinition;
use squeezy_core::Result;

use crate::{current_schema_version, open_database, state_path};

/// Tables added by [`V2AddResolverTables`]. Duplicated here so the
/// migration is self-contained even if the table constants in
/// [`crate`] move; the redb-layer constants drive the runtime accessors.
const RESOLVER_SNAPSHOT_PER_FILE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("resolver_snapshot_per_file");
const RESOLVER_IMPORT_GRAPH: TableDefinition<&str, &[u8]> =
    TableDefinition::new("resolver_import_graph");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");

/// A single forward migration step.
///
/// Migrations are strictly forward-only: each implementation produces a
/// well-defined [`Migration::version`], and the orchestrator runs every
/// registered migration whose `version()` is strictly greater than the
/// schema version currently stamped on disk. Two migrations cannot share
/// a `version()`; the registry enforces this at registration time.
pub trait Migration: Send + Sync {
    /// Target schema version after [`Self::migrate`] runs successfully.
    fn version(&self) -> u64;

    /// Apply this migration to the workspace rooted at `cwd`.
    ///
    /// Implementations must be idempotent so that a partial failure can
    /// be retried safely: re-running a successful migration should
    /// produce the same on-disk state.
    fn migrate(&self, cwd: &Path) -> Result<()>;
}

/// Ordered registry of [`Migration`]s.
///
/// Migrations are kept sorted by [`Migration::version`] so [`Self::run`]
/// can apply them in ascending order without re-sorting per call.
/// Registration order is irrelevant; the registry sorts on insert.
pub struct MigrationRegistry {
    migrations: Vec<Box<dyn Migration>>,
}

impl Default for MigrationRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl MigrationRegistry {
    pub fn new() -> Self {
        Self {
            migrations: Vec::new(),
        }
    }

    /// Register `migration`.
    ///
    /// Panics if another migration with the same [`Migration::version`]
    /// has already been registered. Two migrations cannot share a
    /// version because the runner uses it as the on-disk state stamp;
    /// detecting the collision at registration time keeps the failure
    /// loud and immediate rather than letting it manifest as silently
    /// skipped migration steps at runtime.
    pub fn register<M: Migration + 'static>(&mut self, migration: M) -> &mut Self {
        let version = migration.version();
        assert!(
            !self.migrations.iter().any(|m| m.version() == version),
            "duplicate migration version: {version}",
        );
        self.migrations.push(Box::new(migration));
        self.migrations.sort_by_key(|m| m.version());
        self
    }

    /// Highest target version any registered migration produces. Returns
    /// `0` when the registry is empty.
    pub fn target_version(&self) -> u64 {
        self.migrations
            .iter()
            .map(|m| m.version())
            .max()
            .unwrap_or(0)
    }

    /// Number of registered migrations.
    pub fn len(&self) -> usize {
        self.migrations.len()
    }

    /// Whether the registry has no migrations registered.
    pub fn is_empty(&self) -> bool {
        self.migrations.is_empty()
    }

    /// Run every migration whose [`Migration::version`] is strictly
    /// greater than `current_version`, in ascending order, against
    /// `cwd`. Returns the number of migrations that ran.
    ///
    /// Migrations are run sequentially; a failure short-circuits the
    /// run and propagates the error to the caller. Earlier migrations
    /// that already succeeded are not rolled back — each migration is
    /// expected to leave the store in a usable state regardless of
    /// whether a later migration also succeeds.
    pub fn run(&self, cwd: &Path, current_version: u64) -> Result<usize> {
        let mut applied = 0;
        for migration in &self.migrations {
            if migration.version() > current_version {
                migration.migrate(cwd)?;
                applied += 1;
            }
        }
        Ok(applied)
    }
}

/// Build the migration registry shipped with the crate.
///
/// Every new migration should be added here. The order in which they
/// appear in source is irrelevant; the registry sorts by
/// [`Migration::version`] before running.
pub fn default_registry() -> MigrationRegistry {
    let mut registry = MigrationRegistry::new();
    registry.register(InitializeStoreSchemaV1);
    registry.register(V2AddResolverTables);
    registry.register(V3SplitGraphCache);
    registry
}

/// Run every registered migration in [`default_registry`] whose
/// `version()` is strictly greater than the schema version currently
/// stamped at `<cwd>/.squeezy/cache/state.redb`.
///
/// A missing state file or a redb without a `schema_version` entry is
/// treated as version 0, so a brand-new workspace ends up with every
/// migration applied in order and the store left at
/// [`crate::SCHEMA_VERSION`].
///
/// Returns `Ok(())` even when no migration ran; the no-op case is the
/// common path on every `SqueezyStore::open` after the first.
pub fn run_migrations(cwd: &Path) -> Result<()> {
    run_registry(&default_registry(), cwd).map(|_| ())
}

/// Lower-level entry point used by tests and the small number of
/// callers that need to compose their own [`MigrationRegistry`]. Reads
/// the current on-disk schema version (or 0 when none is present) and
/// delegates to [`MigrationRegistry::run`], returning the number of
/// migrations applied.
pub fn run_registry(registry: &MigrationRegistry, cwd: &Path) -> Result<usize> {
    let current = current_store_schema_version(cwd)?.unwrap_or(0);
    registry.run(cwd, current)
}

/// Read the schema version currently stamped at the default workspace
/// state path. Returns `Ok(None)` when the redb file is absent or has
/// no `schema_version` entry yet.
fn current_store_schema_version(cwd: &Path) -> Result<Option<u64>> {
    let path = state_path(cwd, None);
    if !path.exists() {
        return Ok(None);
    }
    let database = open_database(&path)?;
    current_schema_version(&database)
}

/// First migration: create the redb tables and stamp `schema_version = 1`
/// on a fresh workspace. Equivalent to the previous inline
/// `initialize_schema` call in `SqueezyStore::open`, lifted into the
/// registry so subsequent migrations can sit next to it without
/// touching the store open path.
pub struct InitializeStoreSchemaV1;

impl Migration for InitializeStoreSchemaV1 {
    fn version(&self) -> u64 {
        1
    }

    fn migrate(&self, cwd: &Path) -> Result<()> {
        let path = state_path(cwd, None);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let database = open_database(&path)?;
        // Bootstrap a fresh store at v1 first; the v2 migration stamps
        // its own version + opens the new tables on top.
        let write = database.begin_write().map_err(store_error)?;
        {
            let mut meta = write
                .open_table(redb::TableDefinition::<&str, &[u8]>::new("meta"))
                .map_err(store_error)?;
            let encoded = serde_json::to_vec(&1u64).map_err(|err| {
                squeezy_core::SqueezyError::Tool(format!("store encode failed: {err}"))
            })?;
            meta.insert("schema_version", encoded.as_slice())
                .map_err(store_error)?;
        }
        for table in [
            "graph_partitions",
            "tool_receipts",
            "read_snapshots",
            "mcp_tool_cache",
            "observations",
            "observation_index",
            "compaction_checkpoints",
        ] {
            write
                .open_table(redb::TableDefinition::<&str, &[u8]>::new(table))
                .map_err(store_error)?;
        }
        write.commit().map_err(store_error)?;
        // Each V1 table above mirrors the tables opened by
        // `initialize_schema`, so this migration leaves the store at
        // exactly its declared `version()` (1). We deliberately do NOT call
        // `initialize_schema` here: it re-stamps `schema_version =
        // SCHEMA_VERSION`, which would jump the on-disk stamp past 1 and
        // break the per-version idempotency / resume contract. Tables added
        // in later schema versions belong in their own migration.
        Ok(())
    }
}

fn store_error(error: impl std::fmt::Display) -> squeezy_core::SqueezyError {
    squeezy_core::SqueezyError::Tool(format!("store error: {error}"))
}

/// Add the resolver-cache tables introduced by Item 2 PR-1. Idempotent —
/// opening a table inside a write transaction creates it on first run
/// and is a no-op on every subsequent run. Re-stamps the
/// `schema_version` key so a later `current_schema_version` read sees
/// the updated target.
pub struct V2AddResolverTables;

impl Migration for V2AddResolverTables {
    fn version(&self) -> u64 {
        2
    }

    fn migrate(&self, cwd: &Path) -> Result<()> {
        let path = state_path(cwd, None);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let database = open_database(&path)?;
        let write = database
            .begin_write()
            .map_err(|err| squeezy_core::SqueezyError::Tool(format!("store error: {err}")))?;
        write
            .open_table(RESOLVER_SNAPSHOT_PER_FILE)
            .map_err(|err| squeezy_core::SqueezyError::Tool(format!("store error: {err}")))?;
        write
            .open_table(RESOLVER_IMPORT_GRAPH)
            .map_err(|err| squeezy_core::SqueezyError::Tool(format!("store error: {err}")))?;
        {
            let mut meta = write
                .open_table(META)
                .map_err(|err| squeezy_core::SqueezyError::Tool(format!("store error: {err}")))?;
            let encoded = serde_json::to_vec(&2u64).map_err(|err| {
                squeezy_core::SqueezyError::Tool(format!("store encode failed: {err}"))
            })?;
            meta.insert("schema_version", encoded.as_slice())
                .map_err(|err| squeezy_core::SqueezyError::Tool(format!("store error: {err}")))?;
        }
        write
            .commit()
            .map_err(|err| squeezy_core::SqueezyError::Tool(format!("store error: {err}")))
    }
}

/// State schema v3 moves graph-only cache rows to `graph.redb`. The runtime
/// open path rewrites legacy files by rotating the old DB and copying only
/// non-graph tables into a fresh v3 state store; this migration keeps the
/// public registry target aligned for callers that still exercise the
/// forward-only registry directly.
pub struct V3SplitGraphCache;

impl Migration for V3SplitGraphCache {
    fn version(&self) -> u64 {
        3
    }

    fn migrate(&self, cwd: &Path) -> Result<()> {
        let path = state_path(cwd, None);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let database = open_database(&path)?;
        let write = database
            .begin_write()
            .map_err(|err| squeezy_core::SqueezyError::Tool(format!("store error: {err}")))?;
        {
            let mut meta = write
                .open_table(META)
                .map_err(|err| squeezy_core::SqueezyError::Tool(format!("store error: {err}")))?;
            let encoded = serde_json::to_vec(&3u64).map_err(|err| {
                squeezy_core::SqueezyError::Tool(format!("store encode failed: {err}"))
            })?;
            meta.insert("schema_version", encoded.as_slice())
                .map_err(|err| squeezy_core::SqueezyError::Tool(format!("store error: {err}")))?;
        }
        write
            .commit()
            .map_err(|err| squeezy_core::SqueezyError::Tool(format!("store error: {err}")))
    }
}

#[cfg(test)]
#[path = "migrations_tests.rs"]
mod tests;
