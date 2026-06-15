//! Foundation types for the phased cross-file resolver.
//!
//! Squeezy's call resolver currently runs a single pass over every parsed
//! file. This module provides the per-file [`ExportTable`] / [`ImportList`] /
//! [`SupertypeList`] structures and the [`PathResolver`] trait that the phased
//! pipeline will consume. The single-pass
//! [`crate::resolution::SemanticGraph::resolve_call`] still drives resolution;
//! the types are populated during every rebuild and persisted through the
//! resolver-cache so that per-language flips can read a ready table on the
//! first refresh without a one-time backfill. Moving the actual resolver onto
//! these structures to eliminate whole-workspace scans is the next planned
//! step.
//!
//! Boundary: this module owns resolver inputs and path-resolution contracts.
//! It should not mutate graph indexes or emit final semantic edges; those
//! side effects stay in `resolution` until a language is explicitly flipped to
//! the phased pipeline.

pub mod scheduler;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};
use squeezy_core::{EdgeKind, FileId, SymbolId};

use crate::SemanticGraph;

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

/// Inheritance-style edge kinds the ancestor walker consults. Order matters
/// — PHP method resolution looks up the trait ancestors first (in declaration
/// order), then the `Extends` parent, then the `Implements` interfaces. The
/// generic walker in [`SemanticGraph::walk_inheritance_ancestors`] enumerates
/// in this same priority so a per-language method lookup can pick the first
/// hit and match PHP's runtime semantics. Java's `extends` / `implements` and
/// C#'s `base` walks already work because they only ever see the
/// `Extends`/`Implements` slots — adding `UsesTrait` does not change their
/// behavior when no trait edges are present.
const ANCESTOR_EDGE_KINDS: [EdgeKind; 3] =
    [EdgeKind::UsesTrait, EdgeKind::Extends, EdgeKind::Implements];

impl SemanticGraph {
    /// Breadth-first walk of the inheritance-style ancestors of `start`,
    /// following `UsesTrait` / `Extends` / `Implements` edges in that
    /// declaration order. Returns every reachable ancestor symbol id in
    /// visit order with `start` excluded.
    ///
    /// PHP semantics drive the per-class ordering: traits (in declaration
    /// order) shadow the `Extends` parent, which shadows `Implements`
    /// interfaces. Walking traits before extends in BFS lets a caller pick
    /// the first hit and match the language's actual method-resolution
    /// order. Java and C# never emit `UsesTrait` edges, so the walker is a
    /// no-op extension for them and they keep the existing
    /// `Extends`/`Implements` behavior.
    ///
    /// Cycle safety: trait usage can be diamond-shaped (`trait A { use B; }
    /// trait B { use A; }`), and language extractors may emit cyclic
    /// `Extends` chains for malformed code. The walker tracks visited
    /// symbol ids in a `HashSet`, so each ancestor is enumerated exactly
    /// once and a cycle terminates the walk for that branch.
    ///
    /// This consults [`Self::ancestor_edges_by_from`] rather than the main
    /// `edges_by_from` index because [`SemanticGraph::rebuild_semantic_edges`]
    /// pushes the new type edges (including `UsesTrait`) BEFORE
    /// `add_call_edges` runs but only refreshes `edges_by_from` afterward in
    /// `rebuild_indexes`. The dedicated index — built by
    /// [`Self::build_ancestor_edge_index`] right after the type edges are
    /// pushed — keeps the walker correct from call-resolution time onward and
    /// turns each BFS node from an O(total edges) scan into an O(out-degree)
    /// lookup.
    pub(crate) fn walk_inheritance_ancestors(&self, start: &SymbolId) -> Vec<SymbolId> {
        let mut visited: HashSet<SymbolId> = HashSet::new();
        visited.insert(start.clone());
        let mut order = Vec::new();
        let mut queue: VecDeque<SymbolId> = VecDeque::new();
        queue.push_back(start.clone());
        while let Some(current) = queue.pop_front() {
            // The index already groups targets by edge kind in
            // `ANCESTOR_EDGE_KINDS` priority, so enqueueing each bucket in
            // turn preserves PHP method-resolution order.
            let Some(by_kind) = self.ancestor_edges_by_from.get(&current) else {
                continue;
            };
            for targets in by_kind {
                for target in targets {
                    if visited.insert(target.clone()) {
                        order.push(target.clone());
                        queue.push_back(target.clone());
                    }
                }
            }
        }
        order
    }

    /// Build the transient [`Self::ancestor_edges_by_from`] index from a single
    /// O(total edges) pass, grouping each inheritance edge's target by
    /// `from` symbol and `ANCESTOR_EDGE_KINDS` priority. Called once per
    /// `rebuild_semantic_edges` after the type edges are pushed so the PHP
    /// ancestor walk never rescans the whole edge vector.
    pub(crate) fn build_ancestor_edge_index(&mut self) {
        let mut index: HashMap<SymbolId, [Vec<SymbolId>; 3]> = HashMap::new();
        for edge in &self.edges {
            let Some(kind_index) = ANCESTOR_EDGE_KINDS
                .iter()
                .position(|kind| *kind == edge.kind)
            else {
                continue;
            };
            let Some(target) = &edge.to else {
                continue;
            };
            index.entry(edge.from.clone()).or_default()[kind_index].push(target.clone());
        }
        self.ancestor_edges_by_from = index;
    }
}
