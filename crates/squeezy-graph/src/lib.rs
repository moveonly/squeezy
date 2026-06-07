use std::{
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub mod affected;
pub mod backend;
pub mod cross_file;
mod languages;
mod references;
mod resolution;
pub mod resolver_cache;
pub mod watcher;

use serde::{Deserialize, Serialize};
use squeezy_core::{
    Confidence, ContentHash, EdgeKind, FileId, Freshness, LanguageFamily, LanguageKind, Provenance,
    Result, SourceSpan, SqueezyError, SymbolId, SymbolKind,
};
use squeezy_parse::{
    BodyHit, BodyHitKind, LanguageParser, ParsedCall, ParsedCallKind, ParsedFile, ParsedImport,
    ParsedReference, ParsedSymbol, ReferenceKind, edge_kind_for_call,
};
use squeezy_store::{GraphStore, GraphStoreMetadata, GraphWriteBatch};
use squeezy_workspace::{CrawlOptions, FileRecord, IndexCoverage, WorkspaceCrawler};

use crate::languages::{
    csharp::{
        csharp_import_matches_symbol, dotnet_configured_source_facts, dotnet_dependency_facts,
        dotnet_project_metadata_provider, dotnet_source_root_facts, dotnet_target_facts,
    },
    java::{
        java_build_metadata_provider, java_configured_source_facts, java_dependency_facts,
        java_paths_signature, java_source_root_facts, symbol_is_top_level_for_imports,
    },
    js_ts::{JsTsResolver, is_js_ts_language},
    kotlin::{
        kotlin_build_metadata_provider, kotlin_configured_source_facts, kotlin_dependency_facts,
        kotlin_paths_signature, kotlin_source_root_facts,
    },
    python::{python_module_path_for_file, python_path_segments},
};

pub const CRATE_NAME: &str = "squeezy-graph";
const BODY_HIT_TRIGRAM_INDEX_MAX_HITS: usize = 100_000;
const GRAPH_FORMAT_VERSION: u64 = 1;

pub fn crate_name() -> &'static str {
    CRATE_NAME
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphSymbol {
    pub id: SymbolId,
    pub file_id: FileId,
    pub parent_id: Option<SymbolId>,
    pub name: String,
    pub kind: SymbolKind,
    pub language_identity: Option<String>,
    pub span: SourceSpan,
    pub body_span: Option<SourceSpan>,
    /// Mirror of [`ParsedSymbol::signature_span`]: the declaration-header byte
    /// range (symbol start up to body start). `read_slice` with
    /// `span_kind=signature` reads this when present so a signature read
    /// excludes the body, falling back to the full `span` when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature_span: Option<SourceSpan>,
    pub signature: String,
    pub visibility: Option<String>,
    pub docs: Vec<String>,
    pub attributes: Vec<String>,
    pub provenance: Provenance,
    pub confidence: Confidence,
    pub freshness: Freshness,
    pub dirty: Option<DirtyAnnotation>,
    /// Mirror of [`ParsedSymbol::arity`]; populated when the parser was
    /// able to count fixed positional parameters for the symbol's kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arity: Option<u8>,
    /// `true` when the symbol came from a parsed workspace file. `false`
    /// reserves space for external stubs and parse-failed files which the
    /// resolver should not treat as authoritative candidates by default.
    /// Defaults to `true` for back-compat with persisted JSON snapshots
    /// produced before this field existed.
    #[serde(default = "default_scanned")]
    pub scanned: bool,
}

fn default_scanned() -> bool {
    true
}

impl From<ParsedSymbol> for GraphSymbol {
    fn from(symbol: ParsedSymbol) -> Self {
        Self {
            id: symbol.id,
            file_id: symbol.file_id,
            parent_id: symbol.parent_id,
            name: symbol.name,
            kind: symbol.kind,
            language_identity: symbol.language_identity,
            span: symbol.span,
            body_span: symbol.body_span,
            signature_span: symbol.signature_span,
            signature: symbol.signature,
            visibility: symbol.visibility,
            docs: symbol.docs,
            attributes: symbol.attributes,
            provenance: symbol.provenance,
            confidence: symbol.confidence,
            freshness: symbol.freshness,
            dirty: None,
            arity: symbol.arity,
            scanned: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirtyAnnotation {
    pub status: String,
    pub ranges: Vec<DirtyRange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirtyRange {
    pub start_line: u32,
    pub end_line: u32,
}

/// Maximum number of candidate symbols retained on a `Confidence::CandidateSet`
/// edge. Caps the per-edge payload so token budgets stay predictable when
/// rendered into evidence packets.
pub const MAX_EDGE_CANDIDATES: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphEdge {
    pub from: SymbolId,
    pub to: Option<SymbolId>,
    pub target_text: String,
    pub kind: EdgeKind,
    pub span: Option<SourceSpan>,
    pub confidence: Confidence,
    pub freshness: Freshness,
    pub provenance: Provenance,
    /// For `Confidence::CandidateSet` edges, the disambiguation set the graph
    /// already enumerated during resolution. Empty for every other
    /// confidence. Capped at [`MAX_EDGE_CANDIDATES`] entries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<SymbolId>,
}

/// Provenance metadata for a batch of cargo-derived facts.
///
/// Captured once per `refresh_compiler_facts` run so downstream consumers can
/// tell which exact shell invocation produced the metadata and diagnostics,
/// along with the cargo/rustc versions seen at capture time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CargoFactProvenance {
    pub command: String,
    pub cargo_version: Option<String>,
    pub rustc_version: Option<String>,
    pub captured_unix_millis: u64,
}

/// Kind of cargo-derived node stored in the compiler fact cache.
///
/// Sourced from `cargo metadata --format-version=1 --no-deps`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CargoFactNodeKind {
    Workspace,
    Package,
    Target,
    Feature,
}

/// A single node in the cargo compiler-fact cache (workspace, package, target,
/// or feature) sourced from `cargo metadata`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CargoFactNode {
    pub id: String,
    pub kind: CargoFactNodeKind,
    pub name: String,
    pub package_id: Option<String>,
    pub manifest_path: Option<String>,
    pub source_path: Option<String>,
    pub target_kinds: Vec<String>,
    pub provenance: Provenance,
}

/// A single compiler diagnostic surfaced from
/// `cargo check --message-format=json` and aligned to a file/span in the graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CargoDiagnostic {
    pub level: String,
    pub message: String,
    pub code: Option<String>,
    pub file_id: Option<FileId>,
    pub span: Option<SourceSpan>,
    pub label: Option<String>,
    pub package_id: Option<String>,
    pub target_name: Option<String>,
    pub provenance: Provenance,
}

/// Freshness verdict for the cached cargo compiler facts.
///
/// `Fresh` means the inputs that drive cargo's output (manifests, lockfile,
/// configs, toolchain files, and Rust sources) have not changed since the
/// facts were captured. `Stale` records the hashes plus a short reason list
/// so callers can decide whether to refresh.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CargoFactFreshness {
    pub status: Freshness,
    pub input_fingerprint: ContentHash,
    pub current_fingerprint: ContentHash,
    pub stale_reasons: Vec<String>,
}

/// One diagnostic plus the freshness verdict for the batch it belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CargoDiagnosticHit {
    pub diagnostic: CargoDiagnostic,
    pub freshness: CargoFactFreshness,
}

/// Counts plus freshness for the cargo compiler-fact cache, intended for
/// concise summary output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CargoFactsSummary {
    pub workspaces: usize,
    pub packages: usize,
    pub targets: usize,
    pub features: usize,
    pub diagnostics: usize,
    pub freshness: Option<CargoFactFreshness>,
}

/// Result of a `refresh_compiler_facts` call: the summary plus whether the
/// caller asked for (and successfully loaded) `cargo check` diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CargoFactRefreshReport {
    pub summary: CargoFactsSummary,
    pub diagnostics_loaded: bool,
}

/// Aggregate cargo compiler facts held on the in-memory semantic graph.
///
/// Produced by `cargo metadata --format-version=1 --no-deps` and optionally
/// `cargo check --message-format=json`. `input_fingerprint` is hashed from the
/// graph's view of Rust sources, manifests, lockfile, cargo config files, and
/// toolchain files at capture time, so later edits can be detected without
/// re-running cargo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CargoCompilerFacts {
    pub workspace_root: Option<String>,
    pub target_directory: Option<String>,
    pub nodes: Vec<CargoFactNode>,
    pub diagnostics: Vec<CargoDiagnostic>,
    pub provenance: CargoFactProvenance,
    pub input_fingerprint: ContentHash,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HierarchyNode {
    pub id: SymbolId,
    pub name: String,
    pub kind: SymbolKind,
    pub span: SourceSpan,
    pub freshness: Freshness,
    pub children: Vec<HierarchyNode>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureQuery {
    pub text: String,
    pub kind: Option<SymbolKind>,
    pub visibility: Option<String>,
    pub attribute: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodySearchQuery {
    pub text: String,
    pub owner_kind: Option<SymbolKind>,
    pub hit_kind: Option<BodyHitKind>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodySearchHit {
    pub owner: Option<GraphSymbol>,
    pub hit: BodyHit,
    pub confidence: Confidence,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceHit {
    pub owner: Option<GraphSymbol>,
    pub reference: ParsedReference,
    pub confidence: Confidence,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallEdgeHit {
    pub caller: Option<GraphSymbol>,
    pub callee: Option<GraphSymbol>,
    pub edge: GraphEdge,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JavaProjectFact {
    pub provider: String,
    pub kind: String,
    pub value: String,
    pub source_file: FileId,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DotnetProjectFact {
    pub provider: String,
    pub kind: String,
    pub value: String,
    pub source_file: FileId,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KotlinProjectFact {
    pub provider: String,
    pub kind: String,
    pub value: String,
    pub source_file: FileId,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LanguageFact {
    Java(JavaProjectFact),
    Dotnet(DotnetProjectFact),
    Kotlin(KotlinProjectFact),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphStats {
    pub files: usize,
    pub symbols: usize,
    pub edges: usize,
    pub body_hits: usize,
    pub references: usize,
    pub calls: usize,
    pub cargo_workspaces: usize,
    pub cargo_packages: usize,
    pub cargo_targets: usize,
    pub cargo_features: usize,
    pub cargo_diagnostics: usize,
    pub body_hit_trigram_indexed: bool,
    pub body_hit_trigram_terms: usize,
    pub reference_index_terms: usize,
}

#[derive(Debug, Clone)]
pub struct SemanticGraph {
    pub files: HashMap<FileId, FileRecord>,
    pub symbols: HashMap<SymbolId, GraphSymbol>,
    packages: HashMap<FileId, String>,
    edges: Vec<GraphEdge>,
    imports: Vec<ParsedImport>,
    calls: Vec<ParsedCall>,
    references: Vec<ParsedReference>,
    body_hits: Vec<BodyHit>,
    java_project_facts: Vec<JavaProjectFact>,
    dotnet_project_facts: Vec<DotnetProjectFact>,
    kotlin_project_facts: Vec<KotlinProjectFact>,
    cargo_facts: Option<CargoCompilerFacts>,
    java_project_facts_cache: HashMap<FileId, CachedJavaProjectFacts>,
    java_project_facts_cache_java_paths_signature: u64,
    kotlin_project_facts_cache: HashMap<FileId, CachedKotlinProjectFacts>,
    kotlin_project_facts_cache_kotlin_paths_signature: u64,
    symbols_by_name: HashMap<String, Vec<SymbolId>>,
    symbol_signature_lower: HashMap<SymbolId, String>,
    signature_trigram_index: HashMap<[u8; 3], Vec<SymbolId>>,
    body_hit_text_lower: Vec<String>,
    body_hit_trigram_index: HashMap<[u8; 3], Vec<usize>>,
    body_hit_trigram_indexed: bool,
    references_by_text: HashMap<String, Vec<usize>>,
    children_by_parent: HashMap<SymbolId, Vec<SymbolId>>,
    edges_by_from: HashMap<SymbolId, Vec<usize>>,
    edges_by_to: HashMap<SymbolId, Vec<usize>>,
    /// Transient from-index restricted to the inheritance edge kinds
    /// (`UsesTrait` / `Extends` / `Implements`, in that priority order) that
    /// [`SemanticGraph::walk_inheritance_ancestors`] consults. Built once per
    /// `rebuild_semantic_edges` right after the type edges are pushed, so the
    /// PHP ancestor walk does an O(out-degree) lookup per BFS node instead of
    /// rescanning the whole edge vector. The main `edges_by_from` index is
    /// stale during the call-resolution phase, hence the dedicated map.
    ancestor_edges_by_from: HashMap<SymbolId, [Vec<SymbolId>; 3]>,
    /// Indices into [`Self::imports`] grouped by the file that introduced
    /// them. `import_visible_from_symbol` only ever returns true when the
    /// import shares a file with the caller, so resolving an alias or
    /// reference no longer needs to scan every import in the workspace.
    imports_by_file: HashMap<FileId, Vec<usize>>,
    /// Aliased imports grouped by the last path segment of the import (the
    /// target symbol name). `reference_candidate_indexes_for_symbol` only
    /// pulls aliases from imports whose target leaf equals the symbol's
    /// name, so this turns a workspace-wide import scan into an O(matches)
    /// hash lookup. Glob aliased imports (whose leaf would be `*`) live in
    /// `wildcard_aliased_imports` instead.
    imports_by_alias_target: HashMap<String, Vec<usize>>,
    /// Aliased imports whose path leaf is a wildcard (e.g. JS/TS
    /// `import * as M from 'mod'`). These do not bucket by target name, so
    /// they are scanned alongside the by-target hit list.
    wildcard_aliased_imports: Vec<usize>,
    java_package_by_file: HashMap<FileId, Vec<String>>,
    kotlin_package_by_file: HashMap<FileId, Vec<String>>,
    scala_package_by_file: HashMap<FileId, Vec<String>>,
    js_ts_resolver: JsTsResolver,
    /// Parallel index of `(file, name, arity) -> symbol` so the resolver
    /// can disambiguate overloaded callees by exact positional-parameter
    /// count when the AST already gave us that information. The phased
    /// resolver consumes this; the legacy single-pass path does not yet
    /// read from it (Item 5 PR-2).
    arity_index: HashMap<(FileId, String, u8), SymbolId>,
    /// Reverse import edge: which files import the key. Populated from
    /// `imports_by_file` plus per-language path resolution; used by
    /// affected-set incremental refresh (Item 3 PR-2).
    importers_by_file: HashMap<FileId, Vec<FileId>>,
    /// Per-file [`cross_file::ResolverSlot`] holding exports / imports /
    /// supertypes for the phased pipeline. Populated even before any
    /// resolver phase consumes it so the per-language flip does not need
    /// a one-time backfill.
    resolver_slots: cross_file::ResolverSlots,
    /// Class-like symbols grouped by `language_identity`, the key languages
    /// use to stitch a single logical type split across declarations (Rust
    /// `impl` blocks, C# `partial` classes, Dart `part` files). Only symbols
    /// whose `language_identity` is set and whose kind is class-like are
    /// indexed. [`Self::method_on_class_or_partials`] reads this to enumerate
    /// a class's partials in O(partials) instead of scanning every symbol in
    /// the workspace per resolved call — the dominant cost of the cold build
    /// on large repos, where it turned call resolution quadratic in symbols.
    symbols_by_language_identity: HashMap<String, Vec<SymbolId>>,
}

#[derive(Debug, Clone)]
struct CachedJavaProjectFacts {
    hash: ContentHash,
    java_paths_signature: u64,
    dependency_values: Vec<String>,
    configured_source_facts: Vec<(&'static str, String, &'static str)>,
    source_root_facts: Vec<(&'static str, String, &'static str)>,
}

#[derive(Debug, Clone)]
struct CachedKotlinProjectFacts {
    hash: ContentHash,
    kotlin_paths_signature: u64,
    dependency_values: Vec<String>,
    configured_source_facts: Vec<(&'static str, String, &'static str)>,
    source_root_facts: Vec<(&'static str, String, &'static str)>,
}

impl SemanticGraph {
    #[allow(dead_code)]
    fn lang_ext(&self, family: LanguageFamily) -> &'static dyn backend::LanguageGraphExt {
        backend::extension_for_family(family)
            .unwrap_or_else(|| panic!("missing graph extension for {family:?}"))
    }

    #[allow(dead_code)]
    fn lang_ext_for_kind(
        &self,
        kind: LanguageKind,
    ) -> Option<&'static dyn backend::LanguageGraphExt> {
        LanguageFamily::of(kind).map(|family| self.lang_ext(family))
    }

    pub fn empty() -> Self {
        Self {
            files: HashMap::new(),
            symbols: HashMap::new(),
            packages: HashMap::new(),
            edges: Vec::new(),
            imports: Vec::new(),
            calls: Vec::new(),
            references: Vec::new(),
            body_hits: Vec::new(),
            java_project_facts: Vec::new(),
            dotnet_project_facts: Vec::new(),
            kotlin_project_facts: Vec::new(),
            cargo_facts: None,
            java_project_facts_cache: HashMap::new(),
            java_project_facts_cache_java_paths_signature: 0,
            kotlin_project_facts_cache: HashMap::new(),
            kotlin_project_facts_cache_kotlin_paths_signature: 0,
            symbols_by_name: HashMap::new(),
            symbol_signature_lower: HashMap::new(),
            signature_trigram_index: HashMap::new(),
            body_hit_text_lower: Vec::new(),
            body_hit_trigram_index: HashMap::new(),
            body_hit_trigram_indexed: true,
            references_by_text: HashMap::new(),
            children_by_parent: HashMap::new(),
            edges_by_from: HashMap::new(),
            edges_by_to: HashMap::new(),
            ancestor_edges_by_from: HashMap::new(),
            imports_by_file: HashMap::new(),
            imports_by_alias_target: HashMap::new(),
            wildcard_aliased_imports: Vec::new(),
            java_package_by_file: HashMap::new(),
            kotlin_package_by_file: HashMap::new(),
            scala_package_by_file: HashMap::new(),
            js_ts_resolver: JsTsResolver::default(),
            arity_index: HashMap::new(),
            importers_by_file: HashMap::new(),
            resolver_slots: cross_file::ResolverSlots::new(),
            symbols_by_language_identity: HashMap::new(),
        }
    }

    pub fn from_parsed(files: Vec<ParsedFile>) -> Self {
        let mut graph = Self::empty();
        graph.reserve_parsed_capacity(&files);
        for file in files {
            graph.insert_parsed_file(file);
        }
        graph.rebuild_java_project_facts();
        graph.rebuild_dotnet_project_facts();
        graph.rebuild_kotlin_project_facts();
        graph.rebuild_semantic_edges();
        graph.rebuild_indexes();
        graph
    }

    fn reserve_parsed_capacity(&mut self, files: &[ParsedFile]) {
        let file_count = files.len();
        let symbol_count = files.iter().map(|file| file.symbols.len()).sum::<usize>();
        self.files.reserve(file_count);
        self.symbols.reserve(file_count + symbol_count);
        self.edges.reserve(symbol_count);
        self.imports
            .reserve(files.iter().map(|file| file.imports.len()).sum());
        self.calls
            .reserve(files.iter().map(|file| file.calls.len()).sum());
        self.references
            .reserve(files.iter().map(|file| file.references.len()).sum());
        self.body_hits
            .reserve(files.iter().map(|file| file.body_hits.len()).sum());
    }

    pub fn replace_file(&mut self, file: ParsedFile) {
        self.replace_files(vec![file]);
    }

    pub fn replace_files(&mut self, files: Vec<ParsedFile>) {
        for file in files {
            self.remove_file_data(&file.file.id);
            self.insert_parsed_file(file);
        }
        self.rebuild_java_project_facts();
        self.rebuild_dotnet_project_facts();
        self.rebuild_kotlin_project_facts();
        self.rebuild_semantic_edges();
        self.rebuild_indexes();
    }

    pub fn remove_file(&mut self, file_id: &FileId) {
        self.remove_file_data(file_id);
        self.rebuild_java_project_facts();
        self.rebuild_dotnet_project_facts();
        self.rebuild_kotlin_project_facts();
        self.rebuild_semantic_edges();
        self.rebuild_indexes();
    }

    fn remove_file_data(&mut self, file_id: &FileId) {
        self.files.remove(file_id);
        self.packages.remove(file_id);
        self.symbols.retain(|_, symbol| &symbol.file_id != file_id);
        self.imports.retain(|import| &import.file_id != file_id);
        self.calls.retain(|call| &call.file_id != file_id);
        self.references
            .retain(|reference| &reference.file_id != file_id);
        self.body_hits.retain(|hit| &hit.file_id != file_id);
        self.java_project_facts
            .retain(|fact| &fact.source_file != file_id);
        self.dotnet_project_facts
            .retain(|fact| &fact.source_file != file_id);
        self.kotlin_project_facts
            .retain(|fact| &fact.source_file != file_id);
        self.edges.retain(|edge| {
            self.symbols.contains_key(&edge.from)
                && edge
                    .to
                    .as_ref()
                    .map(|to| self.symbols.contains_key(to))
                    .unwrap_or(true)
        });
    }

    pub fn stats(&self) -> GraphStats {
        let cargo = self.cargo_facts_summary();
        GraphStats {
            files: self.files.len(),
            symbols: self.symbols.len(),
            edges: self.edges.len(),
            body_hits: self.body_hits.len(),
            references: self.references.len(),
            calls: self.calls.len(),
            cargo_workspaces: cargo.workspaces,
            cargo_packages: cargo.packages,
            cargo_targets: cargo.targets,
            cargo_features: cargo.features,
            cargo_diagnostics: cargo.diagnostics,
            body_hit_trigram_indexed: self.body_hit_trigram_indexed,
            body_hit_trigram_terms: self.body_hit_trigram_index.len(),
            reference_index_terms: self.references_by_text.len(),
        }
    }

    pub fn edges(&self) -> &[GraphEdge] {
        &self.edges
    }

    pub fn java_project_facts(&self) -> &[JavaProjectFact] {
        &self.java_project_facts
    }

    pub fn dotnet_project_facts(&self) -> &[DotnetProjectFact] {
        &self.dotnet_project_facts
    }

    pub fn kotlin_project_facts(&self) -> &[KotlinProjectFact] {
        &self.kotlin_project_facts
    }

    pub fn cargo_facts(&self) -> Option<&CargoCompilerFacts> {
        self.cargo_facts.as_ref()
    }

    pub fn cargo_facts_summary(&self) -> CargoFactsSummary {
        let Some(facts) = &self.cargo_facts else {
            return CargoFactsSummary::default();
        };
        let mut summary = CargoFactsSummary {
            diagnostics: facts.diagnostics.len(),
            freshness: Some(self.cargo_fact_freshness_for(facts)),
            ..CargoFactsSummary::default()
        };
        for node in &facts.nodes {
            match node.kind {
                CargoFactNodeKind::Workspace => summary.workspaces += 1,
                CargoFactNodeKind::Package => summary.packages += 1,
                CargoFactNodeKind::Target => summary.targets += 1,
                CargoFactNodeKind::Feature => summary.features += 1,
            }
        }
        summary
    }

    pub fn refresh_cargo_facts_from_json(
        &mut self,
        metadata_json: &str,
        diagnostics_json: Option<&str>,
        provenance: CargoFactProvenance,
        root: &Path,
    ) -> Result<CargoFactRefreshReport> {
        let mut facts = parse_cargo_metadata(metadata_json, &provenance, root)?;
        if let Some(diagnostics_json) = diagnostics_json {
            facts.diagnostics.extend(parse_cargo_diagnostics(
                diagnostics_json,
                &provenance,
                root,
            )?);
        }
        facts.input_fingerprint = self.cargo_fact_input_fingerprint();
        self.cargo_facts = Some(facts);
        Ok(CargoFactRefreshReport {
            summary: self.cargo_facts_summary(),
            diagnostics_loaded: diagnostics_json.is_some(),
        })
    }

    pub fn cargo_diagnostics_for_symbol(&self, symbol: &GraphSymbol) -> Vec<CargoDiagnosticHit> {
        let Some(facts) = &self.cargo_facts else {
            return Vec::new();
        };
        let freshness = self.cargo_fact_freshness_for(facts);
        let symbol_span = symbol.body_span.unwrap_or(symbol.span);
        let mut hits = facts
            .diagnostics
            .iter()
            .filter(|diagnostic| {
                let Some(file_id) = &diagnostic.file_id else {
                    return false;
                };
                if file_id != &symbol.file_id {
                    return false;
                }
                if symbol.kind == SymbolKind::File {
                    return true;
                }
                diagnostic
                    .span
                    .map(|span| spans_intersect(symbol_span, span))
                    .unwrap_or(false)
            })
            .cloned()
            .map(|diagnostic| CargoDiagnosticHit {
                diagnostic,
                freshness: freshness.clone(),
            })
            .collect::<Vec<_>>();
        hits.sort_by(|left, right| {
            left.diagnostic
                .span
                .map(|span| span.start_byte)
                .cmp(&right.diagnostic.span.map(|span| span.start_byte))
                .then(left.diagnostic.message.cmp(&right.diagnostic.message))
        });
        hits
    }

    pub fn language_facts(&self) -> Vec<LanguageFact> {
        self.java_project_facts
            .iter()
            .cloned()
            .map(LanguageFact::Java)
            .chain(
                self.dotnet_project_facts
                    .iter()
                    .cloned()
                    .map(LanguageFact::Dotnet),
            )
            .chain(
                self.kotlin_project_facts
                    .iter()
                    .cloned()
                    .map(LanguageFact::Kotlin),
            )
            .collect()
    }

    fn cargo_fact_freshness_for(&self, facts: &CargoCompilerFacts) -> CargoFactFreshness {
        let current_fingerprint = self.cargo_fact_input_fingerprint();
        let mut stale_reasons = Vec::new();
        if current_fingerprint != facts.input_fingerprint {
            stale_reasons.push(
                "Cargo manifest, lockfile, config, or Rust source inputs changed".to_string(),
            );
        }
        CargoFactFreshness {
            status: if stale_reasons.is_empty() {
                Freshness::Fresh
            } else {
                Freshness::Stale
            },
            input_fingerprint: facts.input_fingerprint.clone(),
            current_fingerprint,
            stale_reasons,
        }
    }

    fn cargo_fact_input_fingerprint(&self) -> ContentHash {
        let mut entries = self
            .files
            .values()
            .filter(|file| {
                file.language == LanguageKind::Rust || is_cargo_fact_input_path(&file.relative_path)
            })
            .map(|file| {
                format!(
                    "{}\t{}\t{}",
                    file.relative_path,
                    file.hash.0,
                    file.language.display_name()
                )
            })
            .collect::<Vec<_>>();
        entries.sort();
        ContentHash::new(squeezy_workspace::stable_content_hash(
            entries.join("\n").as_bytes(),
        ))
    }

    pub fn hierarchy(&self, root: Option<&SymbolId>, max_depth: usize) -> Vec<HierarchyNode> {
        let roots = match root {
            Some(root) => vec![root.clone()],
            None => self
                .symbols
                .values()
                .filter(|symbol| symbol.kind == SymbolKind::File)
                .map(|symbol| symbol.id.clone())
                .collect::<Vec<_>>(),
        };

        let mut nodes = roots
            .iter()
            .filter_map(|id| self.hierarchy_node(id, max_depth))
            .collect::<Vec<_>>();
        nodes.sort_by(|left, right| left.name.cmp(&right.name));
        nodes
    }

    pub fn signature_search(&self, query: &SignatureQuery) -> Vec<GraphSymbol> {
        let needle = query.text.to_lowercase();
        let visibility = query.visibility.as_deref();
        let attribute = query.attribute.as_deref();
        let candidates = self.signature_candidates(&needle);
        let mut matches = candidates
            .into_iter()
            .filter(|symbol| {
                query.kind.map(|kind| symbol.kind == kind).unwrap_or(true)
                    && self
                        .symbol_signature_lower
                        .get(&symbol.id)
                        .map(|signature| signature.contains(&needle))
                        .unwrap_or(false)
                    && visibility
                        .map(|visibility| symbol.visibility.as_deref() == Some(visibility))
                        .unwrap_or(true)
                    && attribute
                        .map(|attribute| {
                            symbol
                                .attributes
                                .iter()
                                .any(|existing| existing.contains(attribute))
                        })
                        .unwrap_or(true)
            })
            .cloned()
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| {
            left.file_id
                .0
                .cmp(&right.file_id.0)
                .then(left.span.start_byte.cmp(&right.span.start_byte))
        });
        matches
    }

    pub fn body_search(&self, query: &BodySearchQuery) -> Vec<BodySearchHit> {
        let needle = query.text.to_lowercase();
        let candidates = self.body_hit_candidates(&needle);
        let mut hits = candidates
            .into_iter()
            .filter(|hit| query.hit_kind.map(|kind| hit.kind == kind).unwrap_or(true))
            .filter_map(|hit| {
                let owner = hit
                    .owner_id
                    .as_ref()
                    .and_then(|id| self.symbols.get(id))
                    .cloned();
                if query
                    .owner_kind
                    .map(|kind| owner.as_ref().map(|owner| owner.kind) == Some(kind))
                    .unwrap_or(true)
                {
                    Some(BodySearchHit {
                        owner,
                        hit: hit.clone(),
                        confidence: Confidence::Heuristic,
                    })
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        hits.sort_by(|left, right| {
            left.hit
                .file_id
                .0
                .cmp(&right.hit.file_id.0)
                .then(left.hit.span.start_byte.cmp(&right.hit.span.start_byte))
        });
        hits
    }

    pub fn reference_search(&self, text: &str) -> Vec<ReferenceHit> {
        let mut indexes = self
            .reference_candidate_indexes(text)
            .into_iter()
            .collect::<BTreeSet<_>>();
        // Alias-aware lookup: when `text` is the local alias of an aliased
        // import (e.g. Kotlin / Python / JS-TS `import x.Target as Friendly`),
        // also surface references to the original `Target` so a search for
        // the alias finds the underlying name. The reverse direction —
        // `references_to_symbol` — already does this via
        // `reference_candidate_indexes_for_symbol`; this is the symmetric
        // forward path.
        for import in &self.imports {
            if import.alias.as_deref() != Some(text) {
                continue;
            }
            let leaf = last_path_segment(&import.path);
            if leaf.is_empty() || leaf == "*" || leaf == text {
                continue;
            }
            indexes.extend(self.reference_candidate_indexes(&leaf));
        }
        let mut hits = indexes
            .into_iter()
            .filter_map(|index| self.references.get(index))
            .map(|reference| self.reference_hit(reference, Confidence::Heuristic))
            .collect::<Vec<_>>();
        hits.sort_by(|left, right| {
            left.reference
                .file_id
                .0
                .cmp(&right.reference.file_id.0)
                .then(
                    left.reference
                        .span
                        .start_byte
                        .cmp(&right.reference.span.start_byte),
                )
        });
        hits
    }

    pub fn references_to_symbol(&self, symbol_id: &SymbolId) -> Vec<ReferenceHit> {
        // One source cache per query so the binding pass reads each referenced
        // file at most once instead of once per candidate reference in it.
        let mut sources = references::SourceCache::default();
        self.references_to_symbol_with_cache(symbol_id, &mut sources)
    }

    pub(crate) fn references_to_symbol_with_cache(
        &self,
        symbol_id: &SymbolId,
        sources: &mut references::SourceCache,
    ) -> Vec<ReferenceHit> {
        let Some(symbol) = self.symbols.get(symbol_id) else {
            return Vec::new();
        };
        let mut hits = self
            .reference_candidate_indexes_for_symbol(symbol)
            .into_iter()
            .filter_map(|index| self.references.get(index))
            .filter_map(|reference| {
                self.reference_binding_confidence(symbol, reference, sources)
                    .map(|confidence| self.reference_hit(reference, confidence))
            })
            .collect::<Vec<_>>();
        hits.sort_by(|left, right| {
            left.reference
                .file_id
                .0
                .cmp(&right.reference.file_id.0)
                .then(
                    left.reference
                        .span
                        .start_byte
                        .cmp(&right.reference.span.start_byte),
                )
        });
        hits.dedup_by(|left, right| {
            left.reference.file_id == right.reference.file_id
                && left.reference.span == right.reference.span
        });
        hits
    }

    pub fn annotate_dirty_ranges(&mut self, dirty: &HashMap<FileId, DirtyAnnotation>) {
        for symbol in self.symbols.values_mut() {
            symbol.dirty = None;
            let Some(annotation) = dirty.get(&symbol.file_id) else {
                continue;
            };
            if symbol.kind == SymbolKind::File
                || annotation.ranges.iter().any(|range| {
                    line_ranges_intersect(symbol.span.start.line, symbol.span.end.line, *range)
                })
            {
                symbol.dirty = Some(annotation.clone());
            }
        }
    }

    pub fn dirty_symbols(&self) -> Vec<GraphSymbol> {
        let mut symbols = self
            .symbols
            .values()
            .filter(|symbol| symbol.dirty.is_some() && symbol.kind != SymbolKind::File)
            .cloned()
            .collect::<Vec<_>>();
        symbols.sort_by(|left, right| {
            left.file_id
                .0
                .cmp(&right.file_id.0)
                .then(left.span.start_byte.cmp(&right.span.start_byte))
        });
        symbols
    }

    pub fn callees(&self, caller: &SymbolId) -> Vec<CallEdgeHit> {
        self.edges_by_from
            .get(caller)
            .into_iter()
            .flatten()
            .filter_map(|edge_index| self.edge_hit(*edge_index))
            .filter(|hit| matches!(hit.edge.kind, EdgeKind::Calls | EdgeKind::InvokesMacro))
            .collect()
    }

    pub fn callers(&self, callee: &SymbolId) -> Vec<CallEdgeHit> {
        self.edges_by_to
            .get(callee)
            .into_iter()
            .flatten()
            .filter_map(|edge_index| self.edge_hit(*edge_index))
            .filter(|hit| matches!(hit.edge.kind, EdgeKind::Calls | EdgeKind::InvokesMacro))
            .collect()
    }

    pub fn call_chain(
        &self,
        from: &SymbolId,
        to: &SymbolId,
        max_depth: usize,
    ) -> Option<Vec<SymbolId>> {
        let mut queue = VecDeque::from([(from.clone(), vec![from.clone()])]);
        let mut seen = HashSet::from([from.clone()]);

        while let Some((current, path)) = queue.pop_front() {
            if &current == to {
                return Some(path);
            }
            // `path.len()` counts nodes, so the chain `from -> ... -> current`
            // has `path.len() - 1` edges. Stop expanding once that edge count
            // would exceed `max_depth`, keeping this bound aligned with the BFS
            // call-graph listing (which gates on `depth >= max_depth`). Using
            // `>` here (not `>=` or `> max_depth + 1`) is deliberate: a target
            // reachable in exactly `max_depth` edges is found, but never one
            // edge further than the BFS listing would reach.
            if path.len() > max_depth {
                continue;
            }
            for edge_hit in self.callees(&current) {
                let Some(next) = edge_hit.edge.to else {
                    continue;
                };
                if seen.insert(next.clone()) {
                    let mut next_path = path.clone();
                    next_path.push(next.clone());
                    queue.push_back((next, next_path));
                }
            }
        }
        None
    }

    pub fn find_symbol_by_name(&self, name: &str) -> Vec<GraphSymbol> {
        self.symbols_by_name
            .get(name)
            .into_iter()
            .flatten()
            .filter_map(|id| self.symbols.get(id))
            .cloned()
            .collect()
    }

    fn insert_parsed_file(&mut self, file: ParsedFile) {
        let ParsedFile {
            file,
            package,
            symbols,
            imports,
            calls,
            references,
            body_hits,
            unsupported,
            ..
        } = file;
        let file_id = file.id.clone();
        let file_symbol = unsupported.is_none().then(|| file_symbol(&file));
        self.files.insert(file_id.clone(), file);
        if let Some(package) = package {
            self.packages.insert(file_id.clone(), package);
        }
        if unsupported.is_some() {
            return;
        }

        let Some(file_symbol) = file_symbol else {
            return;
        };
        let file_symbol_id = file_symbol.id.clone();
        self.symbols.insert(file_symbol_id.clone(), file_symbol);

        for symbol in symbols {
            let symbol = GraphSymbol::from(symbol);
            let parent_id = symbol
                .parent_id
                .clone()
                .unwrap_or_else(|| file_symbol_id.clone());
            self.edges.push(GraphEdge {
                from: parent_id,
                to: Some(symbol.id.clone()),
                target_text: symbol.name.clone(),
                kind: EdgeKind::Contains,
                span: Some(symbol.span),
                confidence: Confidence::ExactSyntax,
                freshness: symbol.freshness,
                provenance: Provenance::new("squeezy-graph", "contains edge from parser hierarchy"),
                candidates: Vec::new(),
            });
            self.symbols.insert(symbol.id.clone(), symbol);
        }

        self.imports.extend(imports);
        self.calls.extend(calls);
        self.references.extend(references);
        self.body_hits.extend(body_hits);
    }

    fn rebuild_java_project_facts(&mut self) {
        self.java_project_facts.clear();
        let metadata_files = self
            .files
            .values()
            .filter_map(|file| {
                java_build_metadata_provider(file).map(|provider| (provider, file.clone()))
            })
            .collect::<Vec<_>>();
        if metadata_files.is_empty() {
            self.java_project_facts_cache.clear();
            self.java_project_facts_cache_java_paths_signature = 0;
            return;
        }

        let mut java_paths = self
            .files
            .values()
            .filter(|file| file.language == LanguageKind::Java)
            .map(|file| file.relative_path.clone())
            .collect::<Vec<_>>();
        java_paths.sort();
        let java_paths_sig = java_paths_signature(&java_paths);
        self.java_project_facts_cache_java_paths_signature = java_paths_sig;

        let metadata_ids = metadata_files
            .iter()
            .map(|(_, file)| file.id.clone())
            .collect::<HashSet<_>>();
        self.java_project_facts_cache
            .retain(|file_id, _| metadata_ids.contains(file_id));

        let java_path_refs = java_paths.iter().map(String::as_str).collect::<Vec<_>>();
        let mut dedup = BTreeSet::new();
        for (provider, file) in metadata_files {
            let cache_hit = self
                .java_project_facts_cache
                .get(&file.id)
                .map(|entry| {
                    entry.hash == file.hash && entry.java_paths_signature == java_paths_sig
                })
                .unwrap_or(false);

            if !cache_hit {
                let source_root_facts = java_source_root_facts(provider, &java_path_refs);
                let (dependency_values, configured_source_facts) =
                    if let Ok(source) = std::fs::read_to_string(&file.path) {
                        (
                            java_dependency_facts(provider, &source),
                            java_configured_source_facts(provider, &source),
                        )
                    } else {
                        (Vec::new(), Vec::new())
                    };
                self.java_project_facts_cache.insert(
                    file.id.clone(),
                    CachedJavaProjectFacts {
                        hash: file.hash.clone(),
                        java_paths_signature: java_paths_sig,
                        dependency_values,
                        configured_source_facts,
                        source_root_facts,
                    },
                );
            }

            let entry = self
                .java_project_facts_cache
                .get(&file.id)
                .expect("just inserted")
                .clone();
            for (kind, value, reason) in entry.source_root_facts {
                self.push_java_project_fact(&mut dedup, provider, kind, value, &file, reason);
            }
            for value in entry.dependency_values {
                self.push_java_project_fact(
                    &mut dedup,
                    provider,
                    "dependency",
                    value,
                    &file,
                    "build dependency coordinate",
                );
            }
            for (kind, value, reason) in entry.configured_source_facts {
                self.push_java_project_fact(&mut dedup, provider, kind, value, &file, reason);
            }
        }

        self.java_project_facts.sort_by(|left, right| {
            left.provider
                .cmp(&right.provider)
                .then(left.kind.cmp(&right.kind))
                .then(left.value.cmp(&right.value))
        });
    }

    fn rebuild_dotnet_project_facts(&mut self) {
        self.dotnet_project_facts.clear();
        let metadata_files = self
            .files
            .values()
            .filter_map(|file| {
                dotnet_project_metadata_provider(file).map(|provider| (provider, file.clone()))
            })
            .collect::<Vec<_>>();
        if metadata_files.is_empty() {
            return;
        }

        let mut csharp_paths = self
            .files
            .values()
            .filter(|file| file.language == LanguageKind::CSharp)
            .map(|file| file.relative_path.clone())
            .collect::<Vec<_>>();
        csharp_paths.sort();
        let csharp_path_refs = csharp_paths.iter().map(String::as_str).collect::<Vec<_>>();

        let mut dedup = BTreeSet::new();
        for (provider, file) in metadata_files {
            self.push_dotnet_project_fact(
                &mut dedup,
                provider,
                "metadata_file",
                file.relative_path.clone(),
                &file,
                ".NET project metadata file",
            );
            for (kind, value, reason) in dotnet_source_root_facts(provider, &csharp_path_refs) {
                self.push_dotnet_project_fact(&mut dedup, provider, kind, value, &file, reason);
            }
            let Ok(source) = std::fs::read_to_string(&file.path) else {
                continue;
            };
            for (kind, value, reason) in dotnet_target_facts(provider, &source) {
                self.push_dotnet_project_fact(&mut dedup, provider, kind, value, &file, reason);
            }
            for value in dotnet_dependency_facts(provider, &source) {
                self.push_dotnet_project_fact(
                    &mut dedup,
                    provider,
                    "dependency",
                    value,
                    &file,
                    ".NET dependency coordinate",
                );
            }
            for (kind, value, reason) in dotnet_configured_source_facts(provider, &source) {
                self.push_dotnet_project_fact(&mut dedup, provider, kind, value, &file, reason);
            }
        }

        self.dotnet_project_facts.sort_by(|left, right| {
            left.provider
                .cmp(&right.provider)
                .then(left.kind.cmp(&right.kind))
                .then(left.value.cmp(&right.value))
        });
    }

    fn push_java_project_fact(
        &mut self,
        dedup: &mut BTreeSet<(String, String, String, String)>,
        provider: &str,
        kind: &str,
        value: String,
        file: &FileRecord,
        reason: &str,
    ) {
        if value.is_empty()
            || !dedup.insert((
                provider.to_string(),
                kind.to_string(),
                value.clone(),
                file.id.0.clone(),
            ))
        {
            return;
        }
        self.java_project_facts.push(JavaProjectFact {
            provider: provider.to_string(),
            kind: kind.to_string(),
            value,
            source_file: file.id.clone(),
            provenance: Provenance::new(provider, reason),
        });
    }

    fn push_dotnet_project_fact(
        &mut self,
        dedup: &mut BTreeSet<(String, String, String, String)>,
        provider: &str,
        kind: &str,
        value: String,
        file: &FileRecord,
        reason: &str,
    ) {
        if value.is_empty()
            || !dedup.insert((
                provider.to_string(),
                kind.to_string(),
                value.clone(),
                file.id.0.clone(),
            ))
        {
            return;
        }
        self.dotnet_project_facts.push(DotnetProjectFact {
            provider: provider.to_string(),
            kind: kind.to_string(),
            value,
            source_file: file.id.clone(),
            provenance: Provenance::new(provider, reason),
        });
    }

    fn rebuild_kotlin_project_facts(&mut self) {
        self.kotlin_project_facts.clear();
        let metadata_files = self
            .files
            .values()
            .filter_map(|file| {
                kotlin_build_metadata_provider(file).map(|provider| (provider, file.clone()))
            })
            .collect::<Vec<_>>();
        if metadata_files.is_empty() {
            self.kotlin_project_facts_cache.clear();
            self.kotlin_project_facts_cache_kotlin_paths_signature = 0;
            return;
        }

        let mut kotlin_paths = self
            .files
            .values()
            .filter(|file| file.language == LanguageKind::Kotlin)
            .map(|file| file.relative_path.clone())
            .collect::<Vec<_>>();
        kotlin_paths.sort();
        let kotlin_paths_sig = kotlin_paths_signature(&kotlin_paths);
        self.kotlin_project_facts_cache_kotlin_paths_signature = kotlin_paths_sig;

        let metadata_ids = metadata_files
            .iter()
            .map(|(_, file)| file.id.clone())
            .collect::<HashSet<_>>();
        self.kotlin_project_facts_cache
            .retain(|file_id, _| metadata_ids.contains(file_id));

        let kotlin_path_refs = kotlin_paths.iter().map(String::as_str).collect::<Vec<_>>();
        let mut dedup = BTreeSet::new();
        for (provider, file) in metadata_files {
            let cache_hit = self
                .kotlin_project_facts_cache
                .get(&file.id)
                .map(|entry| {
                    entry.hash == file.hash && entry.kotlin_paths_signature == kotlin_paths_sig
                })
                .unwrap_or(false);

            if !cache_hit {
                let source_root_facts = kotlin_source_root_facts(provider, &kotlin_path_refs);
                let (dependency_values, configured_source_facts) =
                    if let Ok(source) = std::fs::read_to_string(&file.path) {
                        (
                            kotlin_dependency_facts(provider, &source),
                            kotlin_configured_source_facts(provider, &source),
                        )
                    } else {
                        (Vec::new(), Vec::new())
                    };
                self.kotlin_project_facts_cache.insert(
                    file.id.clone(),
                    CachedKotlinProjectFacts {
                        hash: file.hash.clone(),
                        kotlin_paths_signature: kotlin_paths_sig,
                        dependency_values,
                        configured_source_facts,
                        source_root_facts,
                    },
                );
            }

            let entry = self
                .kotlin_project_facts_cache
                .get(&file.id)
                .expect("just inserted")
                .clone();
            for (kind, value, reason) in entry.source_root_facts {
                self.push_kotlin_project_fact(&mut dedup, provider, kind, value, &file, reason);
            }
            for value in entry.dependency_values {
                self.push_kotlin_project_fact(
                    &mut dedup,
                    provider,
                    "dependency",
                    value,
                    &file,
                    "build dependency coordinate",
                );
            }
            for (kind, value, reason) in entry.configured_source_facts {
                self.push_kotlin_project_fact(&mut dedup, provider, kind, value, &file, reason);
            }
        }

        self.kotlin_project_facts.sort_by(|left, right| {
            left.provider
                .cmp(&right.provider)
                .then(left.kind.cmp(&right.kind))
                .then(left.value.cmp(&right.value))
        });
    }

    fn push_kotlin_project_fact(
        &mut self,
        dedup: &mut BTreeSet<(String, String, String, String)>,
        provider: &str,
        kind: &str,
        value: String,
        file: &FileRecord,
        reason: &str,
    ) {
        if value.is_empty()
            || !dedup.insert((
                provider.to_string(),
                kind.to_string(),
                value.clone(),
                file.id.0.clone(),
            ))
        {
            return;
        }
        self.kotlin_project_facts.push(KotlinProjectFact {
            provider: provider.to_string(),
            kind: kind.to_string(),
            value,
            source_file: file.id.clone(),
            provenance: Provenance::new(provider, reason),
        });
    }

    fn hierarchy_node(&self, id: &SymbolId, max_depth: usize) -> Option<HierarchyNode> {
        let symbol = self.symbols.get(id)?;
        let children = if max_depth == 0 {
            Vec::new()
        } else {
            let mut children = self
                .children_by_parent
                .get(id)
                .into_iter()
                .flatten()
                .filter_map(|child_id| self.hierarchy_node(child_id, max_depth - 1))
                .collect::<Vec<_>>();
            children.sort_by(|left, right| {
                left.span
                    .start_byte
                    .cmp(&right.span.start_byte)
                    .then(left.name.cmp(&right.name))
            });
            children
        };

        Some(HierarchyNode {
            id: symbol.id.clone(),
            name: symbol.name.clone(),
            kind: symbol.kind,
            span: symbol.span,
            freshness: symbol.freshness,
            children,
        })
    }

    fn rebuild_indexes(&mut self) {
        self.symbols_by_name.clear();
        self.symbol_signature_lower.clear();
        self.signature_trigram_index.clear();
        self.body_hit_text_lower.clear();
        self.body_hit_trigram_index.clear();
        self.references_by_text.clear();
        self.children_by_parent.clear();
        self.edges_by_from.clear();
        self.edges_by_to.clear();
        self.symbols_by_language_identity.clear();
        self.rebuild_import_indexes();

        self.symbols_by_name.reserve(self.symbols.len());
        self.symbol_signature_lower.reserve(self.symbols.len());
        self.body_hit_text_lower.reserve(self.body_hits.len());
        self.references_by_text.reserve(self.references.len());
        self.children_by_parent.reserve(self.symbols.len());
        self.edges_by_from.reserve(self.edges.len());
        self.edges_by_to.reserve(self.edges.len());

        for symbol in self.symbols.values() {
            self.symbols_by_name
                .entry(symbol.name.clone())
                .or_default()
                .push(symbol.id.clone());
            if let Some(identity) = &symbol.language_identity
                && is_class_like_kind(symbol.kind)
            {
                self.symbols_by_language_identity
                    .entry(identity.clone())
                    .or_default()
                    .push(symbol.id.clone());
            }
            let lower = symbol.signature.to_lowercase();
            for trigram in unique_trigrams(&lower) {
                self.signature_trigram_index
                    .entry(trigram)
                    .or_default()
                    .push(symbol.id.clone());
            }
            self.symbol_signature_lower.insert(symbol.id.clone(), lower);
        }

        // Body hits can dominate huge Java/Go repositories. Build the trigram
        // index only when total hit volume is small enough; otherwise
        // body_search falls back to a direct scan so cold graph builds stay
        // cheap on million-hit corpora.
        self.body_hit_text_lower = self
            .body_hits
            .iter()
            .map(|hit| hit.text.to_lowercase())
            .collect();
        self.body_hit_trigram_indexed = self.body_hits.len() <= BODY_HIT_TRIGRAM_INDEX_MAX_HITS;
        if self.body_hit_trigram_indexed {
            for (index, lower) in self.body_hit_text_lower.iter().enumerate() {
                for trigram in unique_trigrams(lower) {
                    self.body_hit_trigram_index
                        .entry(trigram)
                        .or_default()
                        .push(index);
                }
            }
        }

        for (index, reference) in self.references.iter().enumerate() {
            self.references_by_text
                .entry(reference.text.clone())
                .or_default()
                .push(index);
            let leaf = last_path_segment_str(&reference.text);
            // Suffix keys let `reference_candidate_indexes` answer
            // dotted/arrow/scoped lookups in O(matches) instead of scanning
            // every reference. We populate them eagerly here because index
            // construction is already O(refs).
            self.references_by_text
                .entry(format!("::{leaf}"))
                .or_default()
                .push(index);
            if let Some(rest) = receiver_split(&reference.text, '.') {
                self.references_by_text
                    .entry(format!(".{rest}"))
                    .or_default()
                    .push(index);
            }
            if let Some(rest) = receiver_split(&reference.text, '>') {
                // `->field` lookups; we key on `->leaf` matching the
                // pre-arrow text via `receiver_split('>') == "leaf"`.
                self.references_by_text
                    .entry(format!("->{rest}"))
                    .or_default()
                    .push(index);
            }
            for segment in path_segments(&reference.text) {
                self.references_by_text
                    .entry(segment)
                    .or_default()
                    .push(index);
            }
        }

        for edge in &self.edges {
            if edge.kind == EdgeKind::Contains
                && let Some(to) = &edge.to
            {
                self.children_by_parent
                    .entry(edge.from.clone())
                    .or_default()
                    .push(to.clone());
            }
        }

        for (index, edge) in self.edges.iter().enumerate() {
            self.edges_by_from
                .entry(edge.from.clone())
                .or_default()
                .push(index);
            if let Some(to) = &edge.to {
                self.edges_by_to.entry(to.clone()).or_default().push(index);
            }
        }
    }

    fn rebuild_resolution_indexes(&mut self) {
        self.symbols_by_name.clear();
        self.children_by_parent.clear();
        self.arity_index.clear();
        self.symbols_by_language_identity.clear();
        self.rebuild_import_indexes();

        self.symbols_by_name.reserve(self.symbols.len());
        self.children_by_parent.reserve(self.symbols.len());
        self.arity_index.reserve(self.symbols.len());

        for symbol in self.symbols.values() {
            self.symbols_by_name
                .entry(symbol.name.clone())
                .or_default()
                .push(symbol.id.clone());
            if let Some(arity) = symbol.arity {
                self.arity_index.insert(
                    (symbol.file_id.clone(), symbol.name.clone(), arity),
                    symbol.id.clone(),
                );
            }
            if let Some(identity) = &symbol.language_identity
                && is_class_like_kind(symbol.kind)
            {
                self.symbols_by_language_identity
                    .entry(identity.clone())
                    .or_default()
                    .push(symbol.id.clone());
            }
        }

        for edge in &self.edges {
            if edge.kind == EdgeKind::Contains
                && let Some(to) = &edge.to
            {
                self.children_by_parent
                    .entry(edge.from.clone())
                    .or_default()
                    .push(to.clone());
            }
        }

        self.rebuild_resolver_slots();
        self.rebuild_importers_by_file();
    }

    /// Populate per-file [`cross_file::ResolverSlot`] entries. The phased
    /// resolver does not yet consume these; the populate step exists so
    /// the per-language flip can read a ready table on the first refresh
    /// instead of paying a one-time backfill.
    fn rebuild_resolver_slots(&mut self) {
        self.resolver_slots.clear();
        for file_id in self.files.keys() {
            self.resolver_slots
                .insert(file_id.clone(), cross_file::ResolverSlot::default());
        }
        for symbol in self.symbols.values() {
            let Some(slot) = self.resolver_slots.get_mut(&symbol.file_id) else {
                continue;
            };
            if symbol_is_exported(symbol) {
                slot.exports.insert(cross_file::ExportEntry {
                    name: symbol.name.clone(),
                    kind: cross_file::ExportKind::Named,
                    symbol: Some(symbol.id.clone()),
                    source: None,
                });
            }
        }
        for import in &self.imports {
            if crate::is_package_marker_alias(import.alias.as_deref()) {
                continue;
            }
            let Some(slot) = self.resolver_slots.get_mut(&import.file_id) else {
                continue;
            };
            slot.imports.push(cross_file::ImportEntry {
                path: import.path.clone(),
                imported_name: import.imported_name.clone(),
                alias: import.alias.clone(),
                source_file: None,
            });
        }
    }

    /// Populate the reverse-import index (target file → files that import it)
    /// used by incremental affected-set propagation.
    ///
    /// Bug #9: the index must point at the file the import *actually resolves
    /// to*, not at every file that merely declares a same-leaf symbol. An
    /// `import a/b/Thing` must not attach an unrelated `c/d/Thing`. Two kinds of
    /// imports need different treatment:
    ///
    /// * Symbol-naming imports (Java/Python/Go dotted paths, JS named imports,
    ///   …) name a symbol leaf. We resolve those through the symbol index,
    ///   narrowing each candidate with [`Self::import_matches_symbol`] — the
    ///   same per-language visibility check the call resolver uses (module /
    ///   package suffix).
    /// * Path-naming imports (Dart `package:foo/a/b/thing.dart`, C `#include
    ///   "a/b/thing.h"`, JS-relative `./a/b/thing`) name a *file*, so their leaf
    ///   is a filename rather than a symbol; resolving them through the symbol
    ///   index would miss them entirely. We resolve those directly to the file
    ///   whose workspace path matches the import's directory+stem suffix.
    ///
    /// Glob imports (`import a.b.*`, `from pkg import *`) name no leaf symbol;
    /// they are handled in a single bounded pass that tests each glob against
    /// the top-level symbols.
    ///
    /// Remaining gap: this still relies on `import_matches_symbol` / path-suffix
    /// matching rather than a fully wired module-resolution pipeline, so
    /// re-export chains and config path-mappings beyond what those model are not
    /// yet followed.
    fn rebuild_importers_by_file(&mut self) {
        self.importers_by_file.clear();
        // Compute first into a local map so we can borrow `self` immutably
        // while walking imports without mutating `importers_by_file` in
        // the same loop.
        let mut updates: HashMap<FileId, Vec<FileId>> = HashMap::new();
        let mut attach = |target: &FileId, importer: &FileId| {
            if target == importer {
                return;
            }
            updates
                .entry(target.clone())
                .or_default()
                .push(importer.clone());
        };

        let mut glob_imports: Vec<&ParsedImport> = Vec::new();
        for import in &self.imports {
            if crate::is_package_marker_alias(import.alias.as_deref()) {
                continue;
            }
            if import.is_glob {
                glob_imports.push(import);
                continue;
            }

            let mut attached_any = false;
            // Bug #13: the reverse-import index must point at the symbol the
            // import actually targets — the import path's leaf — not the local
            // alias. `import { Thing as T }` targets the symbol named `Thing`;
            // keying on `T` would attach the importer to no file (or the wrong
            // one), so affected-file propagation would miss edits to `Thing`.
            let target_name = last_path_segment_str(&import.path);
            for symbol_id in self.symbols_by_name.get(target_name).into_iter().flatten() {
                let Some(symbol) = self.symbols.get(symbol_id) else {
                    continue;
                };
                if symbol.file_id == import.file_id {
                    continue;
                }
                if !self.import_matches_symbol(import, symbol) {
                    continue;
                }
                if !import_path_dir_matches_file(&import.path, &symbol.file_id, &self.files) {
                    continue;
                }
                attach(&symbol.file_id, &import.file_id);
                attached_any = true;
            }

            // Path-naming imports: the leaf is a filename, not a symbol, so the
            // symbol scan above found nothing. Resolve the import path's
            // directory+stem suffix directly against file paths.
            if !attached_any && let Some(suffix) = import_path_filesystem_suffix(&import.path) {
                for (target_id, file) in &self.files {
                    if *target_id == import.file_id {
                        continue;
                    }
                    if file_path_matches_import_suffix(&file.relative_path, &suffix) {
                        attach(target_id, &import.file_id);
                    }
                }
            }
        }

        // Glob imports: one bounded pass over every symbol, testing each
        // top-level symbol against every glob import. Globs are typically few,
        // so this stays O(symbols × globs) without a per-glob full rescan.
        if !glob_imports.is_empty() {
            for symbol in self.symbols.values() {
                if !symbol_is_top_level_for_imports(symbol) {
                    continue;
                }
                for import in &glob_imports {
                    if symbol.file_id == import.file_id {
                        continue;
                    }
                    if self.import_matches_symbol(import, symbol) {
                        attach(&symbol.file_id, &import.file_id);
                    }
                }
            }
        }

        for (target, mut importers) in updates {
            importers.sort_by(|left, right| left.0.cmp(&right.0));
            importers.dedup();
            self.importers_by_file.insert(target, importers);
        }
    }

    fn rebuild_import_indexes(&mut self) {
        self.imports_by_file.clear();
        self.imports_by_alias_target.clear();
        self.wildcard_aliased_imports.clear();
        self.java_package_by_file.clear();
        self.kotlin_package_by_file.clear();
        self.scala_package_by_file.clear();
        self.imports_by_file.reserve(self.imports.len());
        self.imports_by_alias_target.reserve(self.imports.len());
        for (index, import) in self.imports.iter().enumerate() {
            self.imports_by_file
                .entry(import.file_id.clone())
                .or_default()
                .push(index);
            if crate::is_package_marker_alias(import.alias.as_deref()) {
                let segments = path_segments(&import.path);
                if !segments.is_empty() {
                    let alias = import.alias.as_deref().unwrap_or_default();
                    match alias {
                        "__scala_package__" => {
                            self.scala_package_by_file
                                .insert(import.file_id.clone(), segments);
                        }
                        "__kotlin_package__" => {
                            self.kotlin_package_by_file
                                .insert(import.file_id.clone(), segments);
                        }
                        _ => {
                            self.java_package_by_file
                                .insert(import.file_id.clone(), segments);
                        }
                    }
                }
                // Package markers never name a target symbol; they live only
                // in the by-file index. Skip both alias-target buckets.
                continue;
            }
            if import.alias.is_none() {
                continue;
            }
            let leaf = last_path_segment_str(&import.path);
            if leaf == "*" || leaf.is_empty() {
                self.wildcard_aliased_imports.push(index);
            } else {
                self.imports_by_alias_target
                    .entry(leaf.to_string())
                    .or_default()
                    .push(index);
            }
        }
    }

    fn imports_for_file(&self, file_id: &FileId) -> impl Iterator<Item = &ParsedImport> {
        self.imports_by_file
            .get(file_id)
            .map(|indexes| indexes.as_slice())
            .unwrap_or(&[])
            .iter()
            .filter_map(|index| self.imports.get(*index))
    }

    fn symbols_by_name_or_scan(&self, name: &str) -> Vec<SymbolId> {
        self.symbols_by_name.get(name).cloned().unwrap_or_default()
    }

    fn signature_candidates(&self, needle: &str) -> Vec<&GraphSymbol> {
        match rarest_indexed_trigram(needle, &self.signature_trigram_index) {
            CandidateSet::All => self.symbols.values().collect(),
            CandidateSet::None => Vec::new(),
            CandidateSet::Indexes(ids) => {
                ids.iter().filter_map(|id| self.symbols.get(id)).collect()
            }
        }
    }

    fn body_hit_candidates(&self, needle: &str) -> Vec<&BodyHit> {
        if !self.body_hit_trigram_indexed {
            return self
                .body_hits
                .iter()
                .zip(self.body_hit_text_lower.iter())
                .filter(|(_, lower)| lower.contains(needle))
                .map(|(hit, _)| hit)
                .collect();
        }
        match rarest_indexed_trigram(needle, &self.body_hit_trigram_index) {
            CandidateSet::All => self
                .body_hits
                .iter()
                .zip(self.body_hit_text_lower.iter())
                .filter(|(_, lower)| lower.contains(needle))
                .map(|(hit, _)| hit)
                .collect(),
            CandidateSet::None => Vec::new(),
            CandidateSet::Indexes(indexes) => indexes
                .iter()
                .filter_map(|index| {
                    let lower = self.body_hit_text_lower.get(*index)?;
                    lower
                        .contains(needle)
                        .then(|| self.body_hits.get(*index))
                        .flatten()
                })
                .collect(),
        }
    }
}

enum CandidateSet<'a, T> {
    All,
    None,
    Indexes(&'a [T]),
}

fn rarest_indexed_trigram<'a, T>(
    needle: &str,
    index: &'a HashMap<[u8; 3], Vec<T>>,
) -> CandidateSet<'a, T> {
    let trigrams = unique_trigrams(needle);
    if trigrams.is_empty() {
        return CandidateSet::All;
    }

    let mut best = None;
    for trigram in trigrams {
        let Some(candidates) = index.get(&trigram) else {
            return CandidateSet::None;
        };
        if best
            .as_ref()
            .map(|current: &&Vec<T>| candidates.len() < current.len())
            .unwrap_or(true)
        {
            best = Some(candidates);
        }
    }

    best.map(|candidates| CandidateSet::Indexes(candidates.as_slice()))
        .unwrap_or(CandidateSet::All)
}

fn unique_trigrams(text: &str) -> BTreeSet<[u8; 3]> {
    let bytes = text.as_bytes();
    if bytes.len() < 3 {
        return BTreeSet::new();
    }
    bytes
        .windows(3)
        .map(|window| [window[0], window[1], window[2]])
        .collect()
}

#[derive(Debug, Clone)]
pub struct RefreshConfig {
    pub debounce: Duration,
    pub idle_refresh_interval: Duration,
    pub per_tool_refresh_budget: Duration,
}

impl Default for RefreshConfig {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(500),
            idle_refresh_interval: Duration::from_secs(15),
            per_tool_refresh_budget: Duration::from_millis(250),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphBuildReport {
    pub duration_ms: u128,
    pub files_seen: usize,
    pub parsed_files: usize,
    pub unsupported_files: usize,
    pub persisted_files_loaded: usize,
    pub persisted_files_missed: usize,
    pub persistence_rebuilt: bool,
    pub excluded_files: usize,
    pub excluded_dirs: usize,
    pub excluded_bytes: u64,
    pub coverage: IndexCoverage,
    pub bytes_seen: u64,
    pub language: LanguageReport,
    pub stats: GraphStats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshReport {
    pub refreshed: bool,
    pub changed_files: Vec<FileId>,
    pub removed_files: Vec<FileId>,
    pub reparsed_files: usize,
    pub changed_paths_from_events: usize,
    pub changed_paths_from_polling: usize,
    pub unchanged_event_paths: usize,
    pub duration_ms: u128,
    pub files_seen: usize,
    pub excluded_files: usize,
    pub excluded_dirs: usize,
    pub excluded_bytes: u64,
    pub coverage: IndexCoverage,
    pub bytes_seen: u64,
    pub bytes_reparsed: u64,
    pub language: LanguageReport,
    pub stats: GraphStats,
    pub skipped_due_to_interval: bool,
    pub budget_exhausted: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WatcherMode {
    #[default]
    Disabled,
    Native,
    PollingFallback,
}

impl WatcherMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Native => "native",
            Self::PollingFallback => "polling_fallback",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatcherStatus {
    pub mode: WatcherMode,
    pub backend: &'static str,
    pub fallback_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LanguageReport {
    pub c_files: usize,
    pub csharp_files: usize,
    pub cpp_files: usize,
    pub dart_files: usize,
    pub go_files: usize,
    pub java_files: usize,
    pub javascript_files: usize,
    pub jsx_files: usize,
    pub kotlin_files: usize,
    pub php_files: usize,
    pub python_files: usize,
    pub ruby_files: usize,
    pub rust_files: usize,
    pub scala_files: usize,
    pub swift_files: usize,
    pub typescript_files: usize,
    pub tsx_files: usize,
    pub supported_files: usize,
    pub unsupported_files: usize,
    pub unknown_files: usize,
}

pub struct GraphManager {
    root: PathBuf,
    crawler: WorkspaceCrawler,
    parser: LanguageParser,
    graph: SemanticGraph,
    config: RefreshConfig,
    store: Option<Arc<GraphStore>>,
    store_metadata: Option<GraphStoreMetadata>,
    last_refresh: Instant,
    build_report: GraphBuildReport,
    /// Paths the next `refresh_now` should treat as authoritatively changed.
    /// Wrapped in `Arc<Mutex<>>` so a background [`watcher::FileWatcher`]
    /// can push paths from its own thread; writers push, the single reader
    /// drains during refresh, so a plain mutex is sufficient.
    pending_changed_paths: Arc<Mutex<HashSet<PathBuf>>>,
    /// Optional running file-system watcher whose lifetime is tied to this
    /// manager. `RAII` drop stops it. `None` for one-shot CLI callers that
    /// open the graph without watching the filesystem.
    _watcher: Option<watcher::FileWatcher>,
    watcher_status: WatcherStatus,
}

impl GraphManager {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_config(root, RefreshConfig::default())
    }

    pub fn open_with_config(root: impl AsRef<Path>, config: RefreshConfig) -> Result<Self> {
        Self::open_with_crawl_options(root, config, CrawlOptions::default())
    }

    pub fn open_with_crawl_options(
        root: impl AsRef<Path>,
        config: RefreshConfig,
        crawl_options: CrawlOptions,
    ) -> Result<Self> {
        Self::open_with_optional_store(root, config, crawl_options, None)
    }

    /// Open a `GraphManager` that uses its own private [`GraphStore`] under
    /// `<workspace_root>/.squeezy/cache` (or `cache_root` when overridden).
    pub fn open_persistent_with_crawl_options(
        root: impl AsRef<Path>,
        config: RefreshConfig,
        crawl_options: CrawlOptions,
        cache_root: Option<PathBuf>,
    ) -> Result<Self> {
        let root_path = root.as_ref().to_path_buf();
        let store = GraphStore::open(&root_path, cache_root.as_deref())
            .ok()
            .map(Arc::new);
        Self::open_with_optional_store(root_path, config, crawl_options, store)
    }

    /// Open a `GraphManager` against an already-open [`GraphStore`]. Pass
    /// `None` to disable persistence.
    pub fn open_with_store(
        root: impl AsRef<Path>,
        config: RefreshConfig,
        crawl_options: CrawlOptions,
        store: Option<Arc<GraphStore>>,
    ) -> Result<Self> {
        Self::open_with_optional_store(root, config, crawl_options, store)
    }

    /// Open a `GraphManager` and attach a background
    /// [`watcher::FileWatcher`] so the workspace's file changes accumulate
    /// in `pending_changed_paths` without polling. The next
    /// `refresh_before_query` drains them. Long-lived processes (agent,
    /// TUI) should prefer this constructor; one-shot CLI invocations
    /// should not, because the OS watch tear-down adds startup cost they
    /// will never amortise.
    pub fn open_watching(
        root: impl AsRef<Path>,
        config: RefreshConfig,
        crawl_options: CrawlOptions,
        store: Option<Arc<GraphStore>>,
        watcher_config: watcher::WatcherConfig,
    ) -> Result<Self> {
        let mut manager = Self::open_with_optional_store(root, config, crawl_options, store)?;
        let handle = Arc::clone(&manager.pending_changed_paths);
        let watcher_result = watcher::FileWatcher::start(watcher_config.clone(), move |batch| {
            if let Ok(mut paths) = handle.lock() {
                for path in batch.modified.into_iter().chain(batch.removed) {
                    paths.insert(path);
                }
            }
        });
        let (file_watcher, watcher_status) = match watcher_result {
            Ok(file_watcher) => (
                file_watcher,
                WatcherStatus {
                    mode: WatcherMode::Native,
                    backend: watcher::native_backend_name(),
                    fallback_reason: None,
                },
            ),
            Err(native_err) => {
                let fallback_reason = native_err.to_string();
                let handle = Arc::clone(&manager.pending_changed_paths);
                let file_watcher =
                    watcher::FileWatcher::start_polling(watcher_config, move |batch| {
                        if let Ok(mut paths) = handle.lock() {
                            for path in batch.modified.into_iter().chain(batch.removed) {
                                paths.insert(path);
                            }
                        }
                    })
                    .map_err(|poll_err| {
                        SqueezyError::Tool(format!(
                            "watcher: native backend failed ({fallback_reason}); polling fallback failed ({poll_err})"
                        ))
                    })?;
                (
                    file_watcher,
                    WatcherStatus {
                        mode: WatcherMode::PollingFallback,
                        backend: watcher::polling_backend_name(),
                        fallback_reason: Some(fallback_reason),
                    },
                )
            }
        };
        manager._watcher = Some(file_watcher);
        manager.watcher_status = watcher_status;
        Ok(manager)
    }

    fn open_with_optional_store(
        root: impl AsRef<Path>,
        config: RefreshConfig,
        crawl_options: CrawlOptions,
        store: Option<Arc<GraphStore>>,
    ) -> Result<Self> {
        let started = Instant::now();
        let root = root.as_ref().to_path_buf();
        let store_metadata = store
            .as_ref()
            .map(|_| graph_store_metadata(&root, &crawl_options));
        let crawler = WorkspaceCrawler::new(crawl_options);
        let snapshot = crawler.crawl(&root)?;
        let mut parser = LanguageParser::new()?;
        let bytes_seen = snapshot.files.iter().map(|file| file.size_bytes).sum();
        let language = language_report(&snapshot.files);
        let loaded =
            load_persisted_partitions(store.as_deref(), store_metadata.as_ref(), &snapshot.files)?;
        let (parsed_missed, parse_summary) = parser.parse_records(&loaded.missed_records)?;
        if let (Some(store), Some(metadata)) = (store.as_deref(), store_metadata.as_ref()) {
            // Coalesce the cold-build writes into one redb transaction so the
            // first crawl pays a single fsync rather than one per parsed file.
            let mut batch = GraphWriteBatch::new();
            batch.set_metadata(metadata.clone());
            for parsed in &parsed_missed {
                batch.upsert_partition(&parsed.file.id, parsed)?;
            }
            store.apply_graph_batch(&batch)?;
        }
        let graph = SemanticGraph::from_parsed(merge_parsed_by_snapshot_order(
            &snapshot.files,
            loaded.parsed,
            parsed_missed,
        ));
        let build_report = GraphBuildReport {
            duration_ms: started.elapsed().as_millis(),
            files_seen: snapshot.files.len(),
            parsed_files: parse_summary.parsed_files,
            unsupported_files: graph
                .files
                .values()
                .filter(|file| file.language == LanguageKind::Unsupported)
                .count(),
            persisted_files_loaded: loaded.loaded_files,
            persisted_files_missed: loaded.missed_records.len(),
            persistence_rebuilt: loaded.rebuilt,
            excluded_files: snapshot.coverage.skipped_files,
            excluded_dirs: snapshot.coverage.skipped_dirs,
            excluded_bytes: snapshot.coverage.skipped_bytes,
            coverage: snapshot.coverage.clone(),
            bytes_seen,
            language,
            stats: graph.stats(),
        };
        Ok(Self {
            root,
            crawler,
            parser,
            graph,
            config,
            store,
            store_metadata,
            last_refresh: Instant::now(),
            build_report,
            pending_changed_paths: Arc::new(Mutex::new(HashSet::new())),
            _watcher: None,
            watcher_status: WatcherStatus {
                mode: WatcherMode::Disabled,
                backend: "none",
                fallback_reason: None,
            },
        })
    }

    pub fn graph(&self) -> &SemanticGraph {
        &self.graph
    }

    pub fn graph_mut(&mut self) -> &mut SemanticGraph {
        &mut self.graph
    }

    pub fn build_report(&self) -> &GraphBuildReport {
        &self.build_report
    }

    /// Per-language file counts derived from the current graph state.
    /// Cheap (linear in `graph.files`), no parsing — safe to poll from
    /// the TUI tick loop to drive a live status-line item.
    pub fn current_language_report(&self) -> LanguageReport {
        language_report(self.graph.files.values())
    }

    /// `true` when the file watcher has queued changes the next
    /// `refresh_before_query` will pick up. Lets callers (e.g. the TUI
    /// language-summary poller) decide whether a refresh is worth the
    /// lock contention.
    pub fn has_pending_changes(&self) -> bool {
        self.pending_changed_paths
            .lock()
            .map(|paths| !paths.is_empty())
            .unwrap_or(false)
    }

    /// Number of changed paths still queued for the next refresh. After a
    /// budget-exhausted `refresh_before_query`, the unprocessed paths stay in
    /// this set; callers (e.g. squeezy-tools' graph payload) surface the count
    /// so the model learns some changed files were not yet reparsed.
    pub fn pending_changed_count(&self) -> usize {
        self.pending_changed_paths
            .lock()
            .map(|paths| paths.len())
            .unwrap_or(0)
    }

    pub fn watcher_status(&self) -> WatcherStatus {
        self.watcher_status.clone()
    }

    pub fn record_changed_path(&mut self, path: impl Into<PathBuf>) {
        if let Ok(mut paths) = self.pending_changed_paths.lock() {
            paths.insert(path.into());
        }
    }

    pub fn record_changed_paths(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        if let Ok(mut set) = self.pending_changed_paths.lock() {
            set.extend(paths);
        }
    }

    /// Borrow a clone of the `Arc<Mutex<_>>` so a background watcher
    /// thread can push paths into the pending set without holding `&mut
    /// self`. The next `refresh_before_query` drains the set.
    pub fn pending_changed_paths_handle(&self) -> Arc<Mutex<HashSet<PathBuf>>> {
        Arc::clone(&self.pending_changed_paths)
    }

    /// Best-effort write of the V2 resolver-cache rows. Per-file entries
    /// carry the workspace-side fingerprint that future warm-start reads
    /// will compare against. The single-blob import adjacency is mirrored
    /// from [`SemanticGraph::importers_by_file`]. Failures are swallowed
    /// so persistence errors cannot poison the in-memory graph.
    fn persist_resolver_cache(&self, store: &GraphStore) {
        for (file_id, file) in &self.graph.files {
            let Some(slot) = self.graph.resolver_slots.get(file_id) else {
                continue;
            };
            let entry = resolver_cache::ResolverFileEntry {
                fingerprint: resolver_cache::FileFingerprint {
                    modified_unix_millis: file.modified_unix_millis,
                    size_bytes: file.size_bytes,
                },
                exports: slot.exports.clone(),
                imports: slot.imports.clone(),
                supertypes: slot.supertypes.clone(),
                builder_snapshot: resolver_cache::BuilderSnapshot::default(),
            };
            let _ = store.put_resolver_entry(file_id, &entry);
        }
        let mut snapshot = resolver_cache::ResolverSnapshot::new();
        for (target, importers) in &self.graph.importers_by_file {
            for importer in importers {
                snapshot.record_edge(importer, target);
            }
        }
        let _ = store.put_import_graph(&snapshot);
    }

    pub fn refresh_before_query(&mut self) -> Result<RefreshReport> {
        let pending_empty = self
            .pending_changed_paths
            .lock()
            .map(|paths| paths.is_empty())
            .unwrap_or(true);
        if pending_empty && self.last_refresh.elapsed() < self.config.idle_refresh_interval {
            return Ok(RefreshReport {
                refreshed: false,
                changed_files: Vec::new(),
                removed_files: Vec::new(),
                reparsed_files: 0,
                changed_paths_from_events: 0,
                changed_paths_from_polling: 0,
                unchanged_event_paths: 0,
                duration_ms: 0,
                files_seen: self.graph.files.len(),
                excluded_files: self.build_report.excluded_files,
                excluded_dirs: self.build_report.excluded_dirs,
                excluded_bytes: self.build_report.excluded_bytes,
                coverage: self.build_report.coverage.clone(),
                bytes_seen: self.graph.files.values().map(|file| file.size_bytes).sum(),
                bytes_reparsed: 0,
                language: language_report(self.graph.files.values()),
                stats: self.graph.stats(),
                skipped_due_to_interval: true,
                budget_exhausted: false,
            });
        }
        self.refresh_now()
    }

    pub fn refresh_now(&mut self) -> Result<RefreshReport> {
        let started = Instant::now();
        if self.last_refresh.elapsed() < self.config.debounce {
            return Ok(RefreshReport {
                refreshed: false,
                changed_files: Vec::new(),
                removed_files: Vec::new(),
                reparsed_files: 0,
                changed_paths_from_events: 0,
                changed_paths_from_polling: 0,
                unchanged_event_paths: 0,
                duration_ms: started.elapsed().as_millis(),
                files_seen: self.graph.files.len(),
                excluded_files: self.build_report.excluded_files,
                excluded_dirs: self.build_report.excluded_dirs,
                excluded_bytes: self.build_report.excluded_bytes,
                coverage: self.build_report.coverage.clone(),
                bytes_seen: self.graph.files.values().map(|file| file.size_bytes).sum(),
                bytes_reparsed: 0,
                language: language_report(self.graph.files.values()),
                stats: self.graph.stats(),
                skipped_due_to_interval: true,
                budget_exhausted: false,
            });
        }

        let snapshot = self.crawler.crawl(&self.root)?;
        let files_seen = snapshot.files.len();
        let coverage = snapshot.coverage.clone();
        let bytes_seen = snapshot.files.iter().map(|file| file.size_bytes).sum();
        let language = language_report(&snapshot.files);
        let current = snapshot
            .files
            .iter()
            .map(|record| (record.id.clone(), record.clone()))
            .collect::<HashMap<_, _>>();
        let old_ids = self.graph.files.keys().cloned().collect::<HashSet<_>>();
        let current_ids = current.keys().cloned().collect::<HashSet<_>>();

        let removed_files_all = old_ids
            .difference(&current_ids)
            .cloned()
            .collect::<Vec<_>>();
        let removed_files = removed_files_all
            .iter()
            .filter(|id| {
                self.graph
                    .files
                    .get(*id)
                    .map(|old| old.language != LanguageKind::Unsupported)
                    .unwrap_or(true)
            })
            .cloned()
            .collect::<Vec<_>>();
        let unsupported_removed_files = removed_files_all
            .iter()
            .filter(|id| {
                self.graph
                    .files
                    .get(*id)
                    .map(|old| old.language == LanguageKind::Unsupported)
                    .unwrap_or(false)
            })
            .cloned()
            .collect::<Vec<_>>();
        let metadata_removed_files = unsupported_removed_files
            .iter()
            .filter(|id| {
                self.graph
                    .files
                    .get(*id)
                    .map(unsupported_record_affects_graph)
                    .unwrap_or(false)
            })
            .cloned()
            .collect::<Vec<_>>();
        let pending_changed_paths = self
            .pending_changed_paths
            .lock()
            .map(|paths| paths.clone())
            .unwrap_or_default();
        let mut supported_changed_records = current
            .values()
            .filter(|record| record.language != LanguageKind::Unsupported)
            .filter(|record| {
                self.graph
                    .files
                    .get(&record.id)
                    .map(|old| old.hash != record.hash || old.language != record.language)
                    .unwrap_or(true)
            })
            .cloned()
            .collect::<Vec<_>>();
        let unsupported_changed_records = current
            .values()
            .filter(|record| {
                record.language == LanguageKind::Unsupported
                    && self
                        .graph
                        .files
                        .get(&record.id)
                        .map(|old| old.hash != record.hash || old.language != record.language)
                        .unwrap_or(true)
            })
            .cloned()
            .collect::<Vec<_>>();
        let metadata_changed_records = unsupported_changed_records
            .iter()
            .filter(|record| unsupported_record_affects_graph(record))
            .cloned()
            .collect::<Vec<_>>();
        let metadata_refresh_needed =
            !metadata_changed_records.is_empty() || !metadata_removed_files.is_empty();
        let mut changed_records = supported_changed_records.clone();
        changed_records.extend(metadata_changed_records.iter().cloned());
        changed_records.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        supported_changed_records
            .sort_by(|left, right| left.relative_path.cmp(&right.relative_path));

        let mut reparsed_files = 0;
        let mut bytes_reparsed = 0;
        let mut budget_exhausted = false;
        let changed_files = changed_records
            .iter()
            .map(|record| record.id.clone())
            .collect::<Vec<_>>();
        let changed_paths_from_events = changed_records
            .iter()
            .filter(|record| {
                pending_changed_paths
                    .iter()
                    .any(|path| paths_match(path, &record.path))
            })
            .count();
        let changed_paths_from_polling = changed_records
            .len()
            .saturating_sub(changed_paths_from_events);
        let event_changed_or_removed = pending_changed_paths
            .iter()
            .filter(|path| {
                changed_records
                    .iter()
                    .any(|record| paths_match(path, &record.path))
                    || removed_files_all.iter().any(|id| {
                        self.graph
                            .files
                            .get(id)
                            .map(|old| paths_match(path, &old.path))
                            .unwrap_or(false)
                    })
            })
            .count();
        let unchanged_event_paths = pending_changed_paths
            .len()
            .saturating_sub(event_changed_or_removed);
        // Accumulate persistence side effects across the refresh and flush in
        // a single redb transaction at the end. Refreshes can touch dozens of
        // files (large saves, branch switches) and a per-file commit pays one
        // fsync each, which dominates wall-clock cost.
        let mut graph_batch = GraphWriteBatch::new();
        for file_id in &removed_files {
            self.graph.remove_file_data(file_id);
            graph_batch.remove_partition(file_id);
        }
        for file_id in &unsupported_removed_files {
            self.graph.files.remove(file_id);
            graph_batch.remove_partition(file_id);
        }
        for record in unsupported_changed_records {
            // A file that flipped from a supported language to unsupported
            // still has its old symbols/edges/calls/references/packages/facts
            // in the graph. Purge all derived data for the file before
            // recording the unsupported placeholder, otherwise the stale rows
            // remain queryable and poison every downstream tool.
            self.graph.remove_file_data(&record.id);
            self.graph.files.insert(record.id.clone(), record.clone());
        }

        let mut parsed_files = Vec::new();
        for record in supported_changed_records {
            if started.elapsed() > self.config.per_tool_refresh_budget {
                budget_exhausted = true;
                break;
            }
            bytes_reparsed += record.size_bytes;
            let parsed = self.parser.parse_record(&record)?;
            parsed_files.push(parsed);
            reparsed_files += 1;
        }
        if !parsed_files.is_empty() {
            if self.store.is_some() {
                if let Some(metadata) = &self.store_metadata {
                    graph_batch.set_metadata(metadata.clone());
                }
                for parsed in &parsed_files {
                    if let Err(err) = graph_batch.upsert_partition(&parsed.file.id, parsed) {
                        // Encoding failure cannot poison the in-memory graph
                        // update; skip persistence for the offending file and
                        // keep going. The warm-start path will re-parse it
                        // next time.
                        let _ = err;
                    }
                }
            }
            self.graph.replace_files(parsed_files);
        } else if metadata_refresh_needed || !removed_files.is_empty() {
            self.graph.rebuild_java_project_facts();
            self.graph.rebuild_dotnet_project_facts();
            self.graph.rebuild_kotlin_project_facts();
            self.graph.rebuild_semantic_edges();
            self.graph.rebuild_indexes();
        }
        if let Some(store) = self.store.as_deref()
            && !graph_batch.is_empty()
        {
            let _ = store.apply_graph_batch(&graph_batch);
        }
        // Persist the resolver-cache rows for every file the rebuild
        // touched. Best-effort: encoding or write failure must not poison
        // the in-memory graph update; the warm-start path will fall back
        // to a full rebuild when it cannot find an entry.
        if let Some(store) = self.store.as_deref() {
            self.persist_resolver_cache(store);
        }

        // Only declare the pending set fully drained when we actually parsed
        // every changed file. If the per-refresh budget broke the loop early,
        // some changed paths were never reparsed; clearing the set and
        // advancing `last_refresh` here would let the next query skip refresh
        // for the whole idle interval and serve stale data for those files.
        // Leave the pending paths queued (already-parsed ones become cheap
        // no-ops on the next pass) and leave `last_refresh` untouched so the
        // next `refresh_before_query` still picks them up immediately.
        if !budget_exhausted {
            if let Ok(mut paths) = self.pending_changed_paths.lock() {
                paths.clear();
            }
            self.last_refresh = Instant::now();
        }
        Ok(RefreshReport {
            refreshed: reparsed_files > 0 || !removed_files.is_empty() || metadata_refresh_needed,
            changed_files,
            removed_files,
            reparsed_files,
            changed_paths_from_events,
            changed_paths_from_polling,
            unchanged_event_paths,
            duration_ms: started.elapsed().as_millis(),
            files_seen,
            excluded_files: coverage.skipped_files,
            excluded_dirs: coverage.skipped_dirs,
            excluded_bytes: coverage.skipped_bytes,
            coverage,
            bytes_seen,
            bytes_reparsed,
            language,
            stats: self.graph.stats(),
            skipped_due_to_interval: false,
            budget_exhausted,
        })
    }
}

struct LoadedPartitions {
    parsed: Vec<ParsedFile>,
    missed_records: Vec<FileRecord>,
    loaded_files: usize,
    rebuilt: bool,
}

fn load_persisted_partitions(
    store: Option<&GraphStore>,
    expected_metadata: Option<&GraphStoreMetadata>,
    records: &[FileRecord],
) -> Result<LoadedPartitions> {
    let Some(store) = store else {
        return Ok(LoadedPartitions {
            parsed: Vec::new(),
            missed_records: records.to_vec(),
            loaded_files: 0,
            rebuilt: false,
        });
    };
    let metadata_matches = match (store.graph_metadata()?, expected_metadata) {
        (Some(existing), Some(expected)) => existing == *expected,
        (None, _) => false,
        (_, None) => false,
    };
    if !metadata_matches {
        store.clear_graph_partitions()?;
        return Ok(LoadedPartitions {
            parsed: Vec::new(),
            missed_records: records.to_vec(),
            loaded_files: 0,
            rebuilt: true,
        });
    }

    let mut parsed = Vec::new();
    let mut missed_records = Vec::new();
    for record in records {
        match store.graph_partition::<ParsedFile>(&record.id)? {
            Some(mut persisted) if persisted_partition_matches(&persisted, record) => {
                persisted.file = record.clone();
                parsed.push(persisted);
            }
            _ => missed_records.push(record.clone()),
        }
    }
    let loaded_files = parsed.len();
    Ok(LoadedPartitions {
        parsed,
        missed_records,
        loaded_files,
        rebuilt: false,
    })
}

fn persisted_partition_matches(parsed: &ParsedFile, record: &FileRecord) -> bool {
    parsed.file.id == record.id
        && parsed.file.relative_path == record.relative_path
        && parsed.file.hash == record.hash
        && parsed.file.language == record.language
}

fn merge_parsed_by_snapshot_order(
    records: &[FileRecord],
    loaded: Vec<ParsedFile>,
    parsed_missed: Vec<ParsedFile>,
) -> Vec<ParsedFile> {
    let mut by_id = loaded
        .into_iter()
        .chain(parsed_missed)
        .map(|parsed| (parsed.file.id.clone(), parsed))
        .collect::<HashMap<_, _>>();
    records
        .iter()
        .filter_map(|record| by_id.remove(&record.id))
        .collect()
}

fn graph_store_metadata(root: &Path, crawl_options: &CrawlOptions) -> GraphStoreMetadata {
    let crawl_options_json = serde_json::json!({
        "include_hidden": crawl_options.include_hidden,
        "max_file_bytes": crawl_options.max_file_bytes,
        "require_indexing_signal": crawl_options.require_indexing_signal,
        "include": crawl_options.policy.include,
        "exclude": crawl_options.policy.exclude,
        "include_classes": crawl_options.policy.include_classes,
        "exclude_classes": crawl_options.policy.exclude_classes,
    });
    let language_registry_version = LanguageFamily::all()
        .iter()
        .map(|family| {
            let kinds = family
                .kinds()
                .iter()
                .map(|kind| kind.display_name())
                .collect::<Vec<_>>()
                .join(",");
            format!("{}:{kinds}", family.id())
        })
        .collect::<Vec<_>>()
        .join("|");
    GraphStoreMetadata {
        workspace_root: root.display().to_string(),
        crawl_options_hash: squeezy_workspace::stable_content_hash(
            crawl_options_json.to_string().as_bytes(),
        ),
        language_registry_version,
        graph_format_version: GRAPH_FORMAT_VERSION,
    }
}

fn unsupported_record_affects_graph(record: &FileRecord) -> bool {
    dotnet_project_metadata_provider(record).is_some()
        || java_build_metadata_provider(record).is_some()
        || record.relative_path.ends_with("tsconfig.json")
        || record.relative_path.ends_with("package.json")
}

fn language_report<'a>(records: impl IntoIterator<Item = &'a FileRecord>) -> LanguageReport {
    let mut report = LanguageReport::default();
    for record in records {
        if LanguageFamily::of(record.language).is_some() {
            report.supported_files += 1;
        }
        match record.language {
            LanguageKind::C => {
                report.c_files += 1;
            }
            LanguageKind::CSharp => {
                report.csharp_files += 1;
            }
            LanguageKind::Cpp => {
                report.cpp_files += 1;
            }
            LanguageKind::Java => {
                report.java_files += 1;
            }
            LanguageKind::JavaScript => {
                report.javascript_files += 1;
            }
            LanguageKind::Jsx => {
                report.jsx_files += 1;
            }
            LanguageKind::Python => {
                report.python_files += 1;
            }
            LanguageKind::Go => {
                report.go_files += 1;
            }
            LanguageKind::Rust => {
                report.rust_files += 1;
            }
            LanguageKind::TypeScript => {
                report.typescript_files += 1;
            }
            LanguageKind::Tsx => {
                report.tsx_files += 1;
            }
            LanguageKind::Ruby => {
                report.ruby_files += 1;
            }
            LanguageKind::Php => {
                report.php_files += 1;
            }
            LanguageKind::Kotlin => {
                report.kotlin_files += 1;
            }
            LanguageKind::Swift => {
                report.swift_files += 1;
            }
            LanguageKind::Scala => {
                report.scala_files += 1;
            }
            LanguageKind::Dart => {
                report.dart_files += 1;
            }
            LanguageKind::Unsupported => report.unsupported_files += 1,
            LanguageKind::Unknown => report.unknown_files += 1,
        }
    }
    report
}

fn line_ranges_intersect(start: u32, end: u32, dirty: DirtyRange) -> bool {
    start <= dirty.end_line && dirty.start_line <= end
}

fn spans_intersect(left: SourceSpan, right: SourceSpan) -> bool {
    left.start_byte <= right.end_byte && right.start_byte <= left.end_byte
}

fn is_cargo_fact_input_path(relative_path: &str) -> bool {
    // Files whose basename matters anywhere in the workspace tree. This is a
    // basename match (not `ends_with`) so `foo/NotCargo.toml` does not get
    // mistaken for a Cargo manifest.
    const BASENAME_INPUTS: &[&str] = &[
        "Cargo.toml",
        "Cargo.lock",
        "build.rs",
        "rust-toolchain",
        "rust-toolchain.toml",
    ];
    // Trailing path segments that indicate a cargo config file. Cargo honors
    // these at the workspace root and inside any ancestor of a target, so we
    // accept them at arbitrary depth (notably nested in member crates).
    const SUFFIX_INPUTS: &[&str] = &[".cargo/config", ".cargo/config.toml"];
    let basename = relative_path.rsplit('/').next().unwrap_or(relative_path);
    if BASENAME_INPUTS.contains(&basename) {
        return true;
    }
    SUFFIX_INPUTS
        .iter()
        .any(|suffix| relative_path == *suffix || relative_path.ends_with(&format!("/{suffix}")))
}

#[derive(Debug, Deserialize)]
struct CargoMetadataJson {
    packages: Vec<CargoMetadataPackageJson>,
    workspace_members: Vec<String>,
    workspace_root: Option<String>,
    target_directory: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CargoMetadataPackageJson {
    id: String,
    name: String,
    manifest_path: Option<String>,
    targets: Vec<CargoMetadataTargetJson>,
    #[serde(default)]
    features: HashMap<String, Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct CargoMetadataTargetJson {
    name: String,
    kind: Vec<String>,
    src_path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CargoMessageJson {
    reason: String,
    package_id: Option<String>,
    target: Option<CargoMessageTargetJson>,
    message: Option<RustcMessageJson>,
}

#[derive(Debug, Deserialize)]
struct CargoMessageTargetJson {
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RustcMessageJson {
    message: String,
    level: String,
    code: Option<RustcCodeJson>,
    #[serde(default)]
    spans: Vec<RustcSpanJson>,
}

#[derive(Debug, Deserialize)]
struct RustcCodeJson {
    code: String,
}

#[derive(Debug, Deserialize)]
struct RustcSpanJson {
    file_name: String,
    byte_start: u32,
    byte_end: u32,
    line_start: u32,
    line_end: u32,
    column_start: u32,
    column_end: u32,
    is_primary: bool,
    label: Option<String>,
}

fn parse_cargo_metadata(
    metadata_json: &str,
    provenance: &CargoFactProvenance,
    root: &Path,
) -> Result<CargoCompilerFacts> {
    let metadata = serde_json::from_str::<CargoMetadataJson>(metadata_json).map_err(|err| {
        SqueezyError::Graph(format!("failed to parse cargo metadata JSON: {err}"))
    })?;
    let mut nodes = Vec::new();
    nodes.push(CargoFactNode {
        id: "cargo:workspace".to_string(),
        kind: CargoFactNodeKind::Workspace,
        name: normalize_optional_cargo_path(root, metadata.workspace_root.as_deref())
            .unwrap_or_else(|| ".".to_string()),
        package_id: None,
        manifest_path: None,
        source_path: None,
        target_kinds: Vec::new(),
        provenance: Provenance::new("cargo metadata", provenance.command.clone()),
    });

    let workspace_members = metadata
        .workspace_members
        .into_iter()
        .collect::<HashSet<_>>();
    for package in metadata.packages {
        // Defensive: cargo always emits `workspace_members` in practice, even
        // for single-package crates. Treat an empty set as "no filtering"
        // rather than dropping every package on the floor.
        if !workspace_members.is_empty() && !workspace_members.contains(&package.id) {
            continue;
        }
        let manifest_path = normalize_optional_cargo_path(root, package.manifest_path.as_deref());
        nodes.push(CargoFactNode {
            id: format!("cargo:package:{}", package.id),
            kind: CargoFactNodeKind::Package,
            name: package.name.clone(),
            package_id: Some(package.id.clone()),
            manifest_path: manifest_path.clone(),
            source_path: None,
            target_kinds: Vec::new(),
            provenance: Provenance::new("cargo metadata", "workspace package"),
        });
        for target in package.targets {
            nodes.push(CargoFactNode {
                id: format!("cargo:target:{}:{}", package.id, target.name),
                kind: CargoFactNodeKind::Target,
                name: target.name,
                package_id: Some(package.id.clone()),
                manifest_path: manifest_path.clone(),
                source_path: normalize_optional_cargo_path(root, target.src_path.as_deref()),
                target_kinds: target.kind,
                provenance: Provenance::new("cargo metadata", "package target"),
            });
        }
        for feature in package.features.keys() {
            nodes.push(CargoFactNode {
                id: format!("cargo:feature:{}:{feature}", package.id),
                kind: CargoFactNodeKind::Feature,
                name: feature.clone(),
                package_id: Some(package.id.clone()),
                manifest_path: manifest_path.clone(),
                source_path: None,
                target_kinds: Vec::new(),
                provenance: Provenance::new("cargo metadata", "package feature"),
            });
        }
    }
    nodes.sort_by(|left, right| left.id.cmp(&right.id));

    Ok(CargoCompilerFacts {
        workspace_root: normalize_optional_cargo_path(root, metadata.workspace_root.as_deref()),
        target_directory: normalize_optional_cargo_path(root, metadata.target_directory.as_deref()),
        nodes,
        diagnostics: Vec::new(),
        provenance: provenance.clone(),
        input_fingerprint: ContentHash::new(""),
    })
}

fn parse_cargo_diagnostics(
    diagnostics_json: &str,
    provenance: &CargoFactProvenance,
    root: &Path,
) -> Result<Vec<CargoDiagnostic>> {
    let mut diagnostics = Vec::new();
    for line in diagnostics_json.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_str::<CargoMessageJson>(line) else {
            continue;
        };
        if event.reason != "compiler-message" {
            continue;
        }
        let Some(message) = event.message else {
            continue;
        };
        let spans = primary_or_first_spans(&message.spans);
        if spans.is_empty() {
            diagnostics.push(CargoDiagnostic {
                level: message.level,
                message: message.message,
                code: message.code.map(|code| code.code),
                file_id: None,
                span: None,
                label: None,
                package_id: event.package_id,
                target_name: event.target.and_then(|target| target.name),
                provenance: Provenance::new("cargo check", provenance.command.clone()),
            });
            continue;
        }
        for span in spans {
            // Rustc reports lines and columns as 1-indexed; the rest of the
            // graph stores 0-indexed tree-sitter coordinates. Normalize here so
            // diagnostic spans line up with the owning symbol's span convention.
            let line_start = span.line_start.saturating_sub(1);
            let line_end = span.line_end.saturating_sub(1);
            let column_start = span.column_start.saturating_sub(1);
            let column_end = span.column_end.saturating_sub(1);
            diagnostics.push(CargoDiagnostic {
                level: message.level.clone(),
                message: message.message.clone(),
                code: message.code.as_ref().map(|code| code.code.clone()),
                file_id: normalize_cargo_file_id(root, &span.file_name).map(FileId::new),
                span: Some(SourceSpan::new(
                    span.byte_start,
                    span.byte_end,
                    squeezy_core::SourcePoint::new(line_start, column_start),
                    squeezy_core::SourcePoint::new(line_end, column_end),
                )),
                label: span.label.clone(),
                package_id: event.package_id.clone(),
                target_name: event.target.as_ref().and_then(|target| target.name.clone()),
                provenance: Provenance::new("cargo check", provenance.command.clone()),
            });
        }
    }
    diagnostics.sort_by(|left, right| {
        left.file_id
            .as_ref()
            .map(|id| id.0.as_str())
            .cmp(&right.file_id.as_ref().map(|id| id.0.as_str()))
            .then(
                left.span
                    .map(|span| span.start_byte)
                    .cmp(&right.span.map(|span| span.start_byte)),
            )
            .then(left.message.cmp(&right.message))
    });
    Ok(diagnostics)
}

fn primary_or_first_spans(spans: &[RustcSpanJson]) -> Vec<&RustcSpanJson> {
    let primary = spans
        .iter()
        .filter(|span| span.is_primary)
        .collect::<Vec<_>>();
    if primary.is_empty() {
        spans.first().into_iter().collect()
    } else {
        primary
    }
}

fn normalize_optional_cargo_path(root: &Path, path: Option<&str>) -> Option<String> {
    path.and_then(|path| normalize_cargo_file_id(root, path))
}

fn normalize_cargo_file_id(root: &Path, path: &str) -> Option<String> {
    if path.starts_with('<') {
        return None;
    }
    let path = Path::new(path);
    let relative = if path.is_absolute() {
        path.strip_prefix(root).ok()?.to_path_buf()
    } else {
        path.to_path_buf()
    };
    let normalized = relative
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => Some(part.to_string_lossy().to_string()),
            std::path::Component::CurDir => None,
            _ => Some(component.as_os_str().to_string_lossy().to_string()),
        })
        .collect::<Vec<_>>()
        .join("/");
    (!normalized.is_empty()).then_some(normalized)
}

fn file_symbol(file: &FileRecord) -> GraphSymbol {
    GraphSymbol {
        id: file_symbol_id(&file.id),
        file_id: file.id.clone(),
        parent_id: None,
        name: file.relative_path.clone(),
        kind: SymbolKind::File,
        language_identity: None,
        span: SourceSpan::new(
            0,
            0,
            squeezy_core::SourcePoint::new(0, 0),
            squeezy_core::SourcePoint::new(0, 0),
        ),
        body_span: None,
        signature_span: None,
        signature: file.relative_path.clone(),
        visibility: None,
        docs: Vec::new(),
        attributes: Vec::new(),
        provenance: Provenance::new("squeezy-workspace", "workspace file record"),
        confidence: Confidence::ExactSyntax,
        freshness: file.freshness,
        dirty: None,
        arity: None,
        scanned: true,
    }
}

fn file_symbol_id(file_id: &FileId) -> SymbolId {
    SymbolId::new(format!("file:{}", file_id.0))
}

/// Heuristic for whether a symbol should be considered exported for the
/// purposes of cross-file [`cross_file::ExportTable`]. Squeezy's visibility
/// labels vary by language ("pub", "public", `null` for Python module
/// scope, etc.); we treat the symbol as exported when the visibility
/// string is missing or anything other than `private`/`protected`. Anchor
/// the rule here so language-specific tightenings (Item 1 PR-2..5) land
/// in one place.
fn symbol_is_exported(symbol: &GraphSymbol) -> bool {
    if !matches!(
        symbol.kind,
        SymbolKind::Class
            | SymbolKind::Function
            | SymbolKind::Method
            | SymbolKind::Interface
            | SymbolKind::Struct
            | SymbolKind::Enum
            | SymbolKind::Trait
            | SymbolKind::TypeAlias
            | SymbolKind::Const
            | SymbolKind::Macro
            | SymbolKind::Test
            | SymbolKind::Module
    ) {
        return false;
    }
    !matches!(
        symbol.visibility.as_deref(),
        Some("private") | Some("protected") | Some("internal")
    )
}

fn last_path_segment(path: &str) -> String {
    last_path_segment_str(path).to_string()
}

/// Reverse-import narrowing guard (Bug #9) for path-like imports.
///
/// Languages with a dedicated `import_matches_symbol` matcher (Java/Kotlin/
/// Scala/JS-TS/Swift/Python/Go) already validate module/package visibility, so
/// this guard leaves them untouched. The remaining languages fall through
/// `import_matches_symbol`'s default leaf-only check, so an `import a/b/Thing`
/// would still attach an unrelated `c/d/Thing` that merely shares the leaf.
/// When such an import path carries a directory portion (`#include
/// "a/b/thing.h"`, Dart `package:foo/service.dart`, `./base`), require the
/// candidate symbol's file path (extension-stripped) to actually end with the
/// import's directory+stem suffix. Imports with no directory separator
/// (dotted/bare module ids) pass through.
fn import_path_dir_matches_file(
    import_path: &str,
    symbol_file_id: &FileId,
    files: &HashMap<FileId, FileRecord>,
) -> bool {
    let Some(file) = files.get(symbol_file_id) else {
        return true;
    };
    if !language_uses_default_import_match(file.language) {
        return true;
    }
    let Some(suffix) = import_path_filesystem_suffix(import_path) else {
        return true;
    };
    file_path_matches_import_suffix(&file.relative_path, &suffix)
}

/// True when a workspace-relative file path is the resolution target of a
/// path-like import whose directory+stem suffix is `suffix` (both already
/// extension-stripped). Accepts an exact suffix match or a `dir/index` /
/// `dir/mod` directory-style resolution.
fn file_path_matches_import_suffix(relative_path: &str, suffix: &str) -> bool {
    let file_stem = strip_source_extension(relative_path);
    let file_stem = file_stem.trim_start_matches("./");
    file_stem == suffix
        || file_stem.ends_with(&format!("/{suffix}"))
        // Some ecosystems point an import at a directory's index/mod file.
        || file_stem.ends_with(&format!("/{suffix}/index"))
        || file_stem.ends_with(&format!("/{suffix}/mod"))
}

/// True for languages that fall through to `import_matches_symbol`'s default
/// leaf-name-only branch (no dedicated per-language matcher). These are the
/// only ones the path-suffix guard above tightens.
fn language_uses_default_import_match(language: squeezy_core::LanguageKind) -> bool {
    use squeezy_core::LanguageKind as L;
    !matches!(
        language,
        L::Java | L::Kotlin | L::Scala | L::Swift | L::Python | L::Go
    ) && !is_js_ts_language(language)
}

/// If `import_path` is a filesystem-style path with a directory component,
/// return its normalized `dir/stem` suffix (leading `./`, `../`, `package:pkg/`
/// and trailing extension stripped). Returns `None` for dotted/bare imports.
fn import_path_filesystem_suffix(import_path: &str) -> Option<String> {
    let mut path = import_path.trim();
    // Drop a `package:<pkg>/` URI prefix, keeping the in-package path.
    if let Some(rest) = path.strip_prefix("package:") {
        path = rest.split_once('/').map(|(_, rest)| rest).unwrap_or(rest);
    }
    let path = path.trim_start_matches("./");
    if !path.contains('/') {
        return None;
    }
    let normalized = strip_source_extension(path);
    let normalized = normalized.trim_start_matches('/');
    // Re-check after extension stripping that a directory component survives;
    // a bare `foo.h` would not, but `a/b/foo.h` does.
    if !normalized.contains('/') {
        return None;
    }
    Some(normalized.to_string())
}

fn strip_source_extension(path: &str) -> &str {
    match path.rsplit_once('.') {
        Some((stem, ext)) if !ext.contains('/') && !stem.is_empty() => stem,
        _ => path,
    }
}

fn last_path_segment_str(path: &str) -> &str {
    // Strip C/C++ pointer-arrow access (`runner->id` → `id`) before the
    // other separators so dotted/scoped reference text from the C-family
    // path collapses to the symbol leaf the same way Rust/Python does.
    let segment = path
        .rsplit("->")
        .next()
        .unwrap_or(path)
        .rsplit("::")
        .next()
        .unwrap_or(path)
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .rsplit('.')
        .next()
        .unwrap_or(path);
    segment
        .split('<')
        .next()
        .unwrap_or(segment)
        .trim()
        .trim_end_matches('!')
        .trim_end_matches("::*")
}

fn reference_text_matches_symbol(reference: &ParsedReference, symbol: &GraphSymbol) -> bool {
    reference.text == symbol.name || last_path_segment_str(&reference.text) == symbol.name.as_str()
}

fn reference_kind_can_bind_symbol(reference: &ParsedReference, symbol: &GraphSymbol) -> bool {
    if constructor_reference_can_bind_symbol(reference, symbol) {
        return true;
    }
    match symbol.kind {
        SymbolKind::Class
        | SymbolKind::Interface
        | SymbolKind::Struct
        | SymbolKind::Enum
        | SymbolKind::Union
        | SymbolKind::Trait
        | SymbolKind::TypeAlias => matches!(reference.kind, ReferenceKind::Type),
        SymbolKind::Const | SymbolKind::Static | SymbolKind::Module => {
            matches!(
                reference.kind,
                ReferenceKind::Identifier | ReferenceKind::Path
            )
        }
        SymbolKind::Macro => false,
        SymbolKind::Method => matches!(reference.kind, ReferenceKind::Field),
        SymbolKind::Function | SymbolKind::Test => false,
        SymbolKind::Field => {
            matches!(
                reference.kind,
                ReferenceKind::Field | ReferenceKind::Identifier
            )
        }
        SymbolKind::Crate
        | SymbolKind::File
        | SymbolKind::Impl
        | SymbolKind::Variant
        | SymbolKind::Unknown => false,
    }
}

fn is_type_like_symbol(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class
            | SymbolKind::Struct
            | SymbolKind::Interface
            | SymbolKind::Enum
            | SymbolKind::Union
            | SymbolKind::Trait
            | SymbolKind::TypeAlias
            | SymbolKind::Module
    )
}

/// Returns true when `kind` denotes a container that hosts instance methods —
/// the "class-like" types used by Python class lookups, C# class/struct/record
/// member resolution, and similar self/this/base method calls.
fn is_class_like_kind(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class
            | SymbolKind::Struct
            | SymbolKind::Interface
            | SymbolKind::Trait
            | SymbolKind::Enum
    )
}

fn constructor_reference_can_bind_symbol(
    reference: &ParsedReference,
    symbol: &GraphSymbol,
) -> bool {
    if !matches!(symbol.kind, SymbolKind::Class | SymbolKind::Struct)
        || !matches!(
            reference.kind,
            ReferenceKind::Identifier | ReferenceKind::Path
        )
        || last_path_segment_str(&reference.text) != symbol.name.as_str()
        || matches!(symbol.name.as_str(), "None" | "Some" | "Ok" | "Err")
        || !symbol
            .name
            .chars()
            .next()
            .map(|ch| ch.is_ascii_uppercase())
            .unwrap_or(false)
    {
        return false;
    }
    if !reference.text.contains("::") {
        return true;
    }
    path_segments(&reference.text)
        .first()
        .map(|segment| {
            matches!(segment.as_str(), "crate" | "self" | "super")
                || segment
                    .chars()
                    .next()
                    .map(|ch| ch.is_ascii_lowercase() || ch == '_')
                    .unwrap_or(false)
        })
        .unwrap_or(false)
}

fn path_starts_with_external_root(path: &str, language: LanguageKind) -> bool {
    let first_segment = match language {
        LanguageKind::Rust => path.split("::").next().unwrap_or(path).trim(),
        LanguageKind::Go | LanguageKind::CSharp => path
            .split([':', '.', '/'])
            .find(|segment| !segment.trim().is_empty())
            .unwrap_or(path)
            .trim(),
        LanguageKind::Java => path
            .split([':', '.'])
            .find(|segment| !segment.trim().is_empty())
            .unwrap_or(path)
            .trim(),
        // C/C++ does not project "external" symbols through a path prefix
        // (cross-TU references go through `#include` instead, which is
        // handled by `c_family_include_direct_call`). JS/TS/JSX/TSX use
        // module path resolution through `JsTsResolver` rather than syntactic
        // external roots. Python/Unknown/Unsupported also have no syntactic
        // external roots Squeezy can pattern-match on.
        LanguageKind::C
        | LanguageKind::Cpp
        | LanguageKind::JavaScript
        | LanguageKind::Jsx
        | LanguageKind::TypeScript
        | LanguageKind::Tsx
        | LanguageKind::Python
        | LanguageKind::Ruby
        | LanguageKind::Php
        | LanguageKind::Kotlin
        | LanguageKind::Swift
        | LanguageKind::Scala
        | LanguageKind::Dart
        | LanguageKind::Unknown
        | LanguageKind::Unsupported => return false,
    };
    let externals: &[&str] = match language {
        LanguageKind::Rust => &["std", "core", "alloc", "proc_macro"],
        LanguageKind::Go => &[
            "fmt", "context", "errors", "io", "net", "os", "strings", "sync", "time",
        ],
        LanguageKind::Java => &["java", "javax", "jakarta"],
        LanguageKind::CSharp => &[
            // Top-level BCL / NuGet roots whose members live outside the
            // workspace graph; binding heuristics should treat them as
            // external rather than searching for matching local symbols.
            "System",
            "Microsoft",
            "Windows",
            "Azure",
            "Newtonsoft",
            "Xunit",
            "NUnit",
            "MsTest",
            "FluentAssertions",
        ],
        _ => &[],
    };
    externals.contains(&first_segment)
}

/// Returns true when `alias` is one of the package-marker sentinel aliases
/// that language extractors stash on file-level `ParsedImport`s to encode
/// the file's package path without inventing a dedicated field. These
/// imports must be filtered out of regular import-resolution traversals.
pub(crate) fn is_package_marker_alias(alias: Option<&str>) -> bool {
    matches!(
        alias,
        Some("__java_package__") | Some("__kotlin_package__") | Some("__scala_package__"),
    )
}

fn path_segments(path: &str) -> Vec<String> {
    path.split("::")
        .flat_map(|segment| segment.split('/'))
        .flat_map(|segment| segment.split('.'))
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            segment
                .trim_end_matches('!')
                .trim_end_matches("::*")
                .to_string()
        })
        .filter(|segment| !segment.is_empty())
        .collect()
}

fn receiver_from_dotted_reference(path: &str) -> Option<String> {
    path.rsplit_once('.')
        .map(|(receiver, _)| receiver.trim().to_string())
        .filter(|receiver| !receiver.is_empty())
}

/// Returns the trailing identifier after the last `delimiter` character in
/// the text, if any. Used by index construction to derive dotted/arrow
/// suffix keys without scanning the reference text repeatedly at query
/// time.
fn receiver_split(text: &str, delimiter: char) -> Option<&str> {
    let (_, rest) = text.rsplit_once(delimiter)?;
    let rest = rest.trim();
    if rest.is_empty() { None } else { Some(rest) }
}

fn module_path_for_file(path: &str) -> Vec<String> {
    let mut path = path.trim_end_matches(".rs");
    if let Some((_, rest)) = path.split_once("/src/") {
        path = rest;
    } else if let Some(rest) = path.strip_prefix("src/") {
        path = rest;
    } else if let Some(rest) = path.strip_prefix("crates/") {
        let mut parts = rest.splitn(2, '/');
        let _package = parts.next();
        if let Some(rest) = parts.next() {
            path = rest;
        }
    }
    if let Some(rest) = path.strip_suffix("/mod") {
        path = rest;
    }

    let mut segments = vec!["crate".to_string()];
    segments.extend(
        path.split('/')
            .filter(|segment| *segment != "lib" && *segment != "main" && !segment.is_empty())
            .map(ToString::to_string),
    );
    segments
}

fn paths_match(left: &Path, right: &Path) -> bool {
    left == right
        || std::fs::canonicalize(left)
            .ok()
            .zip(std::fs::canonicalize(right).ok())
            .map(|(left, right)| left == right)
            .unwrap_or(false)
}

fn path_segments_suffix_match(left: &[String], right: &[String]) -> bool {
    left == right || left.ends_with(right) || right.ends_with(left)
}

fn is_free_function_like(symbol: &GraphSymbol) -> bool {
    symbol
        .parent_id
        .as_ref()
        .map(|id| id.0.starts_with("file:") || id.0.contains("::module:"))
        .unwrap_or(true)
}

fn impl_header_matches_type(header: &str, type_name: &str) -> bool {
    let own_type = header
        .split_once(" for ")
        .map(|(_, target)| target)
        .unwrap_or(header)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_');
    last_path_segment(own_type) == type_name
}

fn impl_header_implements_trait(header: &str, trait_name: &str) -> bool {
    let Some((trait_part, _)) = header.split_once(" for ") else {
        return false;
    };
    let trait_part = trait_part
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != ':');
    last_path_segment(trait_part) == trait_name
}

fn attribute_text_is_cfg(text: &str) -> bool {
    let trimmed = text.trim_start();
    let body = trimmed
        .strip_prefix("#![")
        .or_else(|| trimmed.strip_prefix("#["));
    let Some(body) = body else { return false };
    let head: String = body
        .trim_start()
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
        .collect();
    head == "cfg" || head == "cfg_attr"
}

fn has_cfg_attribute(symbol: &GraphSymbol) -> bool {
    symbol
        .attributes
        .iter()
        .any(|attr| attr.contains("#[cfg(") || attr.contains("#[cfg_attr("))
}

/// True when a method-call receiver names the caller's *own* instance/type
/// rather than some other value — i.e. it is one of the language-level "self"
/// keywords. Self-receiver calls (`self.foo()`, `this.foo()`) legitimately
/// dispatch into the caller's own class/impl; a call with any *other* explicit
/// receiver (`b.foo()`) must NOT be allowed to short-circuit to the caller's
/// own type, or a method named `foo` on the caller's class will swallow a call
/// that actually targets `b`'s type.
fn is_self_receiver(receiver: Option<&str>) -> bool {
    matches!(
        receiver,
        Some("self") | Some("this") | Some("cls") | Some("Self")
    )
}

fn single_symbol(symbols: impl Iterator<Item = SymbolId>) -> Option<SymbolId> {
    let mut symbols = symbols.collect::<Vec<_>>();
    symbols.sort_by(|left, right| left.0.cmp(&right.0));
    symbols.dedup();
    match symbols.as_slice() {
        [only] => Some(only.clone()),
        _ => None,
    }
}

fn find_identifier(text: &str, name: &str) -> Option<usize> {
    text.match_indices(name)
        .find(|(index, _)| {
            let before = text[..*index].chars().next_back();
            let after = text[*index + name.len()..].chars().next();
            !before
                .map(|ch| ch.is_ascii_alphanumeric() || ch == '_')
                .unwrap_or(false)
                && !after
                    .map(|ch| ch.is_ascii_alphanumeric() || ch == '_')
                    .unwrap_or(false)
        })
        .map(|(index, _)| index)
}

/// Matches a `#include "path"` value against a workspace file's relative
/// path. We allow three shapes:
///   * exact match on the bracketed string (rare; only when the include
///     uses a project-rooted path)
///   * suffix match aligned on a `/` boundary (`utils/runner.h` vs
///     `src/utils/runner.h`)
///   * basename match (`runner.h` vs `src/runner.h`) when the include
///     has no directory component
fn include_path_matches_file(include: &str, relative_path: &str) -> bool {
    let include =
        include.trim_matches(|ch: char| ch.is_whitespace() || ch == '"' || ch == '<' || ch == '>');
    if include.is_empty() {
        return false;
    }
    if include == relative_path {
        return true;
    }
    if !include.contains('/') {
        return relative_path
            .rsplit('/')
            .next()
            .map(|name| name == include)
            .unwrap_or(false);
    }
    if relative_path.ends_with(include) {
        let prefix_len = relative_path.len() - include.len();
        if prefix_len == 0 || &relative_path[prefix_len - 1..prefix_len] == "/" {
            return true;
        }
    }
    false
}

/// True when `relative_path` lives in the same directory (or a sibling
/// translation-unit directory) as the file referenced by the include.
/// Lets `#include "runner.h"` from `src/consumer.c` resolve to
/// `src/runner.c`'s function definitions, matching how C projects keep
/// declaration/definition pairs next to each other.
fn file_shares_include_root(include: &str, relative_path: &str) -> bool {
    if include_path_matches_file(include, relative_path) {
        return true;
    }
    let include =
        include.trim_matches(|ch: char| ch.is_whitespace() || ch == '"' || ch == '<' || ch == '>');
    if include.is_empty() {
        return false;
    }
    // Strip the bracketed-include extension to derive the file stem (e.g.
    // `runner.h` → `runner`). Then check whether `relative_path` has a
    // matching stem in the same directory shape.
    let include_basename = include.rsplit('/').next().unwrap_or(include);
    let Some(include_stem) = include_basename.rsplit_once('.').map(|(stem, _)| stem) else {
        return false;
    };
    let file_basename = relative_path.rsplit('/').next().unwrap_or(relative_path);
    let Some(file_stem) = file_basename.rsplit_once('.').map(|(stem, _)| stem) else {
        return false;
    };
    if include_stem != file_stem {
        return false;
    }
    // Same stem, different extension. Make sure they share the same
    // directory prefix so we don't bind `a/runner.h` → `b/runner.c`.
    let include_dir = include.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
    let file_dir = relative_path
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .unwrap_or("");
    if include_dir.is_empty() {
        return true;
    }
    file_dir.ends_with(include_dir)
}

/// `single_symbol` over an Iterator of `SymbolId` without sorting (we
/// already collected and don't need stable ordering for uniqueness — only
/// that exactly one distinct value remains).
fn single_unique<I: IntoIterator<Item = SymbolId>>(iter: I) -> Option<SymbolId> {
    let mut seen: Option<SymbolId> = None;
    for id in iter {
        match &seen {
            Some(existing) if existing == &id => continue,
            Some(_) => return None,
            None => seen = Some(id),
        }
    }
    seen
}

/// Pseudo-imports synthesised by language extractors to communicate the
/// file-level package binding (e.g. `__java_package__`, `__kotlin_package__`).
/// These never name a target symbol and must be skipped by the cross-file
/// resolver, the importers index, and any code that maps imports to
/// candidate symbols by name.
pub(crate) fn is_package_marker(import: &ParsedImport) -> bool {
    is_package_marker_alias(import.alias.as_deref())
}

fn package_key(path: &str) -> String {
    let mut parts = path.split('/');
    match (parts.next(), parts.next()) {
        (Some("crates"), Some(name)) => format!("crates/{name}"),
        (Some(first), Some(_)) if !matches!(first, "src" | "tests" | "benches" | "examples") => {
            first.to_string()
        }
        (Some("build.rs"), None) => ".".to_string(),
        _ => ".".to_string(),
    }
}

/// For a workspace-relative Rust source path, return the crate
/// identifier that appears in code (kebab-case crate name turned into
/// underscores). Returns `None` for paths outside `crates/<name>/`.
pub(crate) fn crate_underscore_alias_for_relative_path(path: &str) -> Option<String> {
    let mut parts = path.split('/');
    if parts.next() != Some("crates") {
        return None;
    }
    let crate_dir = parts.next()?;
    if crate_dir.is_empty() {
        return None;
    }
    Some(crate_dir.replace('-', "_"))
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
