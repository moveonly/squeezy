//! Foundation types for the phased cross-file resolver.
//!
//! Squeezy's call resolver runs a single pass over every parsed file. The
//! work in this module sets up the structures the phased pipeline needs —
//! per-file [`ExportTable`] / [`ImportList`] / [`SupertypeList`] plus a
//! [`PathResolver`] trait per language — without flipping any existing call
//! site to consume them. The single-pass [`crate::resolution::SemanticGraph::resolve_call`]
//! continues to drive resolution; the types below are populated and ready
//! for the per-language flips that follow.

pub mod scheduler;

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};
use squeezy_core::{FileId, SymbolId};

/// Shape of an exported binding. Captures whether a name leaves a module as
/// the default export, a named binding, a star re-export, etc., so the
/// phased resolver can route the look-up through the matching rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ExportKind {
    Named,
    Default,
    ReExport,
    Star,
    ModuleAlias,
}

/// One entry in a file's [`ExportTable`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportEntry {
    pub name: String,
    pub kind: ExportKind,
    pub symbol: Option<SymbolId>,
    /// `Some(file)` for re-exports — the file whose export this entry forwards.
    pub source: Option<FileId>,
}

/// Per-file table of exported bindings keyed by their externally visible name.
///
/// `BTreeMap` so the serialised form is deterministic across runs; the
/// persistent fingerprint cache (Item 2) relies on stable ordering for
/// content-addressed snapshots.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportTable {
    pub entries: BTreeMap<String, ExportEntry>,
}

impl ExportTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, entry: ExportEntry) {
        self.entries.insert(entry.name.clone(), entry);
    }

    pub fn get(&self, name: &str) -> Option<&ExportEntry> {
        self.entries.get(name)
    }
}

/// One resolved entry in a file's [`ImportList`]. `source_file` is `None`
/// while the importer cannot be matched to a workspace file (external
/// package, unresolved path, etc.).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportEntry {
    pub path: String,
    pub imported_name: Option<String>,
    pub alias: Option<String>,
    pub source_file: Option<FileId>,
}

/// Per-file list of imports with the resolved source file attached.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportList {
    pub entries: Vec<ImportEntry>,
}

impl ImportList {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, entry: ImportEntry) {
        self.entries.push(entry);
    }

    pub fn iter(&self) -> impl Iterator<Item = &ImportEntry> {
        self.entries.iter()
    }
}

/// Per-class direct supertypes (extends / implements / bases) used by the
/// nested-type inheritance chain in Java, Python, and C#.
///
/// `SymbolId` is keyed by `HashMap` because `squeezy_core::SymbolId` does
/// not implement `Ord`; the inner `BTreeSet` keeps the supertype list
/// deterministic for the persistent fingerprint cache.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupertypeList {
    pub by_symbol: HashMap<SymbolId, BTreeSet<String>>,
}

impl SupertypeList {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, symbol: SymbolId, supertype: impl Into<String>) {
        self.by_symbol
            .entry(symbol)
            .or_default()
            .insert(supertype.into());
    }

    pub fn supertypes(&self, symbol: &SymbolId) -> impl Iterator<Item = &String> {
        self.by_symbol
            .get(symbol)
            .into_iter()
            .flat_map(|set| set.iter())
    }
}

/// Trait implemented by per-language module path resolvers. Generalises the
/// existing [`crate::languages::js_ts::JsTsResolver`] so Java / Python / C#
/// / Rust can plug into the phased pipeline with the same interface.
///
/// `Input` and `Output` are intentionally associated types — Java's path
/// resolver needs `(package, file-relative-path)` context that JS/TS does
/// not, and forcing a shared concrete type now would push optional fields
/// onto every implementation. The phased scheduler holds an `Arc<dyn
/// PathResolver<Input = …, Output = …>>` per language family.
pub trait PathResolver: Send + Sync {
    type Input;
    type Output;

    fn resolve(&self, input: Self::Input) -> Option<Self::Output>;
}

/// Numbered strongly-connected component id used by the scheduler. The
/// scheduler computes SCCs once per `rebuild_semantic_edges` and assigns
/// each file a stable id in the topological order so the fixpoint inside
/// an SCC can iterate without re-discovering the component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SccId(pub u32);

/// Per-file slot held on `SemanticGraph` for the phased resolver. The
/// foundation types are populated even before any resolver phase consumes
/// them so a one-time backfill is not needed when the per-language flip
/// lands.
#[derive(Debug, Clone, Default)]
pub struct ResolverSlot {
    pub exports: ExportTable,
    pub imports: ImportList,
    pub supertypes: SupertypeList,
}

/// Map keyed by `FileId` holding a [`ResolverSlot`] for each scanned file.
pub type ResolverSlots = HashMap<FileId, ResolverSlot>;
