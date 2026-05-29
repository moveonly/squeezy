//! Persistent resolver-cache types. The squeezy-store schema V2 tables
//! `RESOLVER_SNAPSHOT_PER_FILE` and `RESOLVER_IMPORT_GRAPH` hold values of
//! the shapes declared here. The graph layer owns the types because the
//! store layer does not depend on parsed-file or cross-file structures.
//!
//! No consumer yet writes through these tables; the read consumer in
//! [`crate::GraphManager::open_with_optional_store`] lands in Item 2 PR-2.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use squeezy_core::FileId;

use crate::cross_file::{ExportTable, ImportList, SupertypeList};

/// File-level fingerprint used by the resolver cache to decide whether a
/// snapshot is still authoritative without rehashing the source. Both
/// fields are already on `squeezy_workspace::FileRecord`; the store
/// rounds them to its own ordering and ignores the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFingerprint {
    pub modified_unix_millis: u128,
    pub size_bytes: u64,
}

/// Snapshot of the builder's per-file state. `symbols_by_name_local` is
/// the subset of [`crate::SemanticGraph::symbols_by_name`] entries owned
/// by the file; sorted into a `BTreeMap` so serialisation is
/// deterministic (so the content fingerprint of a snapshot is stable
/// across runs).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuilderSnapshot {
    pub symbols_by_name_local: BTreeMap<String, Vec<String>>,
}

/// One entry in `RESOLVER_SNAPSHOT_PER_FILE`. Per-file derivatives the
/// resolver layer would otherwise have to recompute on every process
/// start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolverFileEntry {
    pub fingerprint: FileFingerprint,
    pub exports: ExportTable,
    pub imports: ImportList,
    pub supertypes: SupertypeList,
    pub builder_snapshot: BuilderSnapshot,
}

/// Single-blob persistent shape of the file-level import adjacency graph
/// that the phased scheduler discovers. Stored under one key in
/// `RESOLVER_IMPORT_GRAPH` so a warm-start load is one table get.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolverSnapshot {
    pub imports_by_file: BTreeMap<String, Vec<String>>,
    pub importers_by_file: BTreeMap<String, Vec<String>>,
}

impl ResolverSnapshot {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_edge(&mut self, importer: &FileId, target: &FileId) {
        self.imports_by_file
            .entry(importer.0.clone())
            .or_default()
            .push(target.0.clone());
        self.importers_by_file
            .entry(target.0.clone())
            .or_default()
            .push(importer.0.clone());
    }
}
