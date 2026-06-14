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
pub use references::SourceCache;
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
use squeezy_workspace::{
    CompiledIndexingPolicy, CrawlOptions, FileRecord, IndexCoverage, IndexingDecision,
    PathConflict, PriorFileMeta, PriorFileMetadata, VCS_AND_CACHE_DIR_NAMES, WorkspaceCrawler,
    filesystem_paths_match,
};
use tracing::{error, warn};

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
// v2: graph partitions and resolver-cache rows are now stored DEFLATE-compressed
// (squeezy-store `encode_graph`/`decode_graph`) instead of plain JSON. Bumping
// this invalidates any v1 plain-JSON cache via the metadata gate so it is wiped
// and rebuilt in the compressed format — a reader never inflates plain bytes.
const GRAPH_FORMAT_VERSION: u64 = 2;

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
    /// Raw path string from the compiler span, populated only when `file_id`
    /// normalization failed (i.e. `file_id` is `None` and the span had a
    /// non-empty, non-`<…>` file path). Lets callers report the raw path and
    /// workspace root so users can diagnose container, symlink, or bind-mount
    /// path-spelling mismatches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_path: Option<String>,
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

/// Result of [`SemanticGraph::compute_impact`]: files, symbols, and test
/// symbols reachable from a set of changed files through reverse-import
/// propagation.
#[derive(Debug, Clone, Default)]
pub struct ImpactSet {
    /// All files reachable from the changed set through reverse-import edges.
    pub affected_files: HashSet<FileId>,
    /// All non-file symbols whose declaring file is in `affected_files`.
    pub affected_symbols: Vec<GraphSymbol>,
    /// Subset of `affected_symbols` that are test functions or carry a
    /// `TestOf` edge to a symbol in the affected set.
    pub affected_tests: Vec<GraphSymbol>,
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
    /// Number of path pairs whose lowercase spellings collide. Non-zero on
    /// Windows when a checkout produces two differently-cased spellings for
    /// the same logical file. Surfaced in graph-status output so operators
    /// can detect Windows casing-drift artefacts.
    pub case_collision_count: usize,
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
    /// Fingerprint of the `body_hits` text used to build the current
    /// `body_hit_text_lower` / `body_hit_trigram_index`. `rebuild_indexes`
    /// recomputes this from `body_hits` and, when it matches, skips the
    /// per-hit `to_lowercase()` re-allocation and the trigram rebuild — the
    /// dominant index-rebuild cost on body-hit-heavy repos when a refresh did
    /// not actually change any body hit. `None` forces a rebuild.
    body_hits_fingerprint: Option<u64>,
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
    /// Aliased imports grouped by their **local alias** (the in-file binding
    /// name), the dual of [`Self::imports_by_alias_target`]. `reference_search`
    /// uses this to answer "which imports bind the local name `text`?" without
    /// scanning every import in the workspace.
    imports_by_alias: HashMap<String, Vec<usize>>,
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
    /// count when the AST already gave us that information. Populated on
    /// every rebuild; the phased resolver will consume this when it replaces
    /// the single-pass path.
    arity_index: HashMap<(FileId, String, u8), SymbolId>,
    /// Reverse import edge: which files import the key. Populated from
    /// `imports_by_file` plus per-language path resolution; used by
    /// affected-set computation and [`SemanticGraph::compute_impact`].
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
    /// Lowercase slash-normalized relative path → `FileId` for O(1)
    /// case-insensitive lookups. Populated by `rebuild_indexes` from every
    /// indexed file. On case-insensitive filesystems (Windows) this index
    /// lets path-filter helpers skip linear scans and avoids redundant
    /// `canonicalize` calls during watcher event reconciliation.
    pub(crate) files_by_normalized_id: HashMap<String, FileId>,
    /// Case-collision log: pairs of `FileId` strings whose lowercase forms
    /// are equal. Populated during `rebuild_indexes`; empty on well-formed
    /// repositories, non-empty on Windows when a checkout leaves two
    /// differently-cased spellings for the same logical path.
    pub(crate) case_collisions: Vec<(String, String)>,
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
            body_hits_fingerprint: None,
            references_by_text: HashMap::new(),
            children_by_parent: HashMap::new(),
            edges_by_from: HashMap::new(),
            edges_by_to: HashMap::new(),
            ancestor_edges_by_from: HashMap::new(),
            imports_by_file: HashMap::new(),
            imports_by_alias_target: HashMap::new(),
            imports_by_alias: HashMap::new(),
            wildcard_aliased_imports: Vec::new(),
            java_package_by_file: HashMap::new(),
            kotlin_package_by_file: HashMap::new(),
            scala_package_by_file: HashMap::new(),
            js_ts_resolver: JsTsResolver::default(),
            arity_index: HashMap::new(),
            importers_by_file: HashMap::new(),
            resolver_slots: cross_file::ResolverSlots::new(),
            symbols_by_language_identity: HashMap::new(),
            files_by_normalized_id: HashMap::new(),
            case_collisions: Vec::new(),
        }
    }

    pub fn from_parsed(files: Vec<ParsedFile>) -> Self {
        Self::from_parsed_with_resolver_cache(files, None, &[]).0
    }

    fn from_parsed_with_resolver_cache(
        files: Vec<ParsedFile>,
        store: Option<&GraphStore>,
        records: &[FileRecord],
    ) -> (Self, ResolverCacheLoadReport) {
        let mut graph = Self::empty();
        graph.reserve_parsed_capacity(&files);
        for file in files {
            graph.insert_parsed_file(file);
        }
        graph.rebuild_java_project_facts();
        graph.rebuild_dotnet_project_facts();
        graph.rebuild_kotlin_project_facts();
        let resolver_cache = load_resolver_cache(store, &mut graph, records).unwrap_or({
            ResolverCacheLoadReport {
                entries_loaded: 0,
                entries_missed: records.len(),
                import_graph_loaded: false,
            }
        });
        graph.rebuild_semantic_edges_with_cached_resolver(
            resolver_cache.entries_missed == 0 && resolver_cache.import_graph_loaded,
        );
        graph.rebuild_indexes();
        (graph, resolver_cache)
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
        // Batch the per-file purge into a single pass. The previous loop called
        // `remove_file_data` once per file, and each call did its own full
        // retain over symbols/imports/calls/references/body_hits/facts plus a
        // full edge retain — O(files_changed × total_rows). Collecting the
        // changed ids once and retaining a single time makes it O(total_rows).
        let changed: HashSet<FileId> = files.iter().map(|file| file.file.id.clone()).collect();
        self.remove_files_data(&changed);
        for file in files {
            self.insert_parsed_file(file);
        }
        self.rebuild_java_project_facts();
        self.rebuild_dotnet_project_facts();
        self.rebuild_kotlin_project_facts();
        self.rebuild_semantic_edges();
        self.rebuild_indexes();
    }

    /// Purge all derived data for every file in `file_ids` in a single retain
    /// pass per collection. Semantically equivalent to calling
    /// [`Self::remove_file_data`] once per id, but it scans each row vector
    /// once instead of once per file.
    fn remove_files_data(&mut self, file_ids: &HashSet<FileId>) {
        if file_ids.is_empty() {
            return;
        }
        self.files.retain(|id, _| !file_ids.contains(id));
        self.packages.retain(|id, _| !file_ids.contains(id));
        self.symbols
            .retain(|_, symbol| !file_ids.contains(&symbol.file_id));
        self.imports
            .retain(|import| !file_ids.contains(&import.file_id));
        self.calls.retain(|call| !file_ids.contains(&call.file_id));
        self.references
            .retain(|reference| !file_ids.contains(&reference.file_id));
        // Keep `body_hit_text_lower` aligned with `body_hits` (same parallel
        // vectors as in `remove_file_data`).
        if self.body_hit_text_lower.len() == self.body_hits.len() {
            let mut keep = self
                .body_hits
                .iter()
                .map(|hit| !file_ids.contains(&hit.file_id));
            self.body_hit_text_lower
                .retain(|_| keep.next().unwrap_or(true));
        }
        self.body_hits
            .retain(|hit| !file_ids.contains(&hit.file_id));
        self.java_project_facts
            .retain(|fact| !file_ids.contains(&fact.source_file));
        self.dotnet_project_facts
            .retain(|fact| !file_ids.contains(&fact.source_file));
        self.kotlin_project_facts
            .retain(|fact| !file_ids.contains(&fact.source_file));
        // Edges survive only when both endpoints still have a symbol. The
        // symbol retain above already dropped every removed file's symbols, so
        // checking membership in the now-current `self.symbols` is correct.
        self.edges.retain(|edge| {
            self.symbols.contains_key(&edge.from)
                && edge
                    .to
                    .as_ref()
                    .map(|to| self.symbols.contains_key(to))
                    .unwrap_or(true)
        });
    }

    pub fn remove_file(&mut self, file_id: &FileId) {
        self.remove_file_data(file_id);
        self.rebuild_java_project_facts();
        self.rebuild_dotnet_project_facts();
        self.rebuild_kotlin_project_facts();
        self.rebuild_semantic_edges();
        self.rebuild_indexes();
    }

    /// Look up a `FileRecord` by a case-insensitive, backslash-normalized path.
    ///
    /// Returns the first indexed record whose lowercase slash-normalized id
    /// equals `lowercase(normalize_backslashes(query))`. This is used for
    /// Windows-friendly path resolution without requiring an exact-case match.
    pub fn find_file_case_insensitive(&self, query: &str) -> Option<&FileRecord> {
        let normalized = query.replace('\\', "/").to_ascii_lowercase();
        self.files_by_normalized_id
            .get(&normalized)
            .and_then(|id| self.files.get(id))
    }

    /// When an exact path lookup misses, check if only casing differs and
    /// return the indexed spelling for a user-facing hint.
    pub fn case_insensitive_match_hint(&self, query: &str) -> Option<&str> {
        let normalized = query.replace('\\', "/").to_ascii_lowercase();
        self.files_by_normalized_id
            .get(&normalized)
            .map(|id| id.0.as_str())
    }

    fn remove_file_data(&mut self, file_id: &FileId) {
        self.files.remove(file_id);
        self.packages.remove(file_id);
        self.symbols.retain(|_, symbol| &symbol.file_id != file_id);
        self.imports.retain(|import| &import.file_id != file_id);
        self.calls.retain(|call| &call.file_id != file_id);
        self.references
            .retain(|reference| &reference.file_id != file_id);
        // `body_hits` and `body_hit_text_lower` are parallel vectors addressed
        // by the same index from `body_hit_trigram_index`. Drop the lowercase
        // shadow in lockstep so the index does not point past the truncated
        // `body_hits` (a later rebuild repopulates both, but read paths between
        // here and that rebuild must still see aligned vectors). The lengths
        // can legitimately differ before the first `rebuild_indexes`, so guard
        // on equal length and otherwise leave the shadow for the rebuild.
        if self.body_hit_text_lower.len() == self.body_hits.len() {
            let mut keep = self.body_hits.iter().map(|hit| &hit.file_id != file_id);
            self.body_hit_text_lower
                .retain(|_| keep.next().unwrap_or(true));
        }
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
            case_collision_count: self.case_collisions.len(),
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

    /// Return the **containment** hierarchy rooted at `root` (or at all file
    /// symbols when `root` is `None`), following `Contains` edges up to
    /// `max_depth` levels. The result represents lexical nesting (file →
    /// module → class → method), **not** inheritance.
    ///
    /// For inheritance/subtype relationships use
    /// [`Self::inheritance_ancestors`] and [`Self::inheritance_direct_subtypes`].
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

    /// Capped variant of [`Self::hierarchy`] that selects the roots to expand
    /// **before** building any subtree, so a rootless repo map on a huge
    /// workspace never materialises the full containment forest just to throw
    /// most of it away.
    ///
    /// When `root` is `Some`, behaves like `hierarchy(Some(root), max_depth)`
    /// and the returned count is the number of resolved roots (0 or 1). When
    /// `root` is `None`, every `File` symbol is a candidate root: candidates are
    /// sorted by name, the first `max_roots` are expanded to `max_depth`, and
    /// the returned `usize` is the **total** candidate-root count (before the
    /// `max_roots` cap) so callers can report "showing N of M".
    pub fn hierarchy_capped(
        &self,
        root: Option<&SymbolId>,
        max_depth: usize,
        max_roots: usize,
    ) -> (Vec<HierarchyNode>, usize) {
        if let Some(root) = root {
            let nodes = self
                .hierarchy_node(root, max_depth)
                .into_iter()
                .collect::<Vec<_>>();
            let total = nodes.len();
            return (nodes, total);
        }

        // Collect file roots and sort by name first; expansion of each subtree
        // is the expensive part, so only the selected prefix is expanded.
        let mut file_roots = self
            .symbols
            .values()
            .filter(|symbol| symbol.kind == SymbolKind::File)
            .map(|symbol| (symbol.name.clone(), symbol.id.clone()))
            .collect::<Vec<_>>();
        file_roots.sort_by(|left, right| left.0.cmp(&right.0));
        let total_roots = file_roots.len();

        let nodes = file_roots
            .into_iter()
            .take(max_roots)
            .filter_map(|(_, id)| self.hierarchy_node(&id, max_depth))
            .collect::<Vec<_>>();
        (nodes, total_roots)
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
        for import in self
            .imports_by_alias
            .get(text)
            .into_iter()
            .flatten()
            .filter_map(|index| self.imports.get(*index))
        {
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

    pub fn references_to_symbol_with_cache(
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

    /// Iterate the graph edges whose source is `from`, in index order, using
    /// the `edges_by_from` index instead of scanning the full edge vector.
    /// Callers that only need a subset (a particular [`EdgeKind`], say) can
    /// filter the returned iterator without materialising an intermediate
    /// `Vec`.
    pub fn outgoing_edges(&self, from: &SymbolId) -> impl Iterator<Item = &GraphEdge> + '_ {
        self.edges_by_from
            .get(from)
            .into_iter()
            .flatten()
            .filter_map(|index| self.edges.get(*index))
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

    /// Borrow the `SymbolId`s indexed under `name` without allocating. Returns
    /// an empty slice when no symbol carries that name. Read-only callers that
    /// only iterate the ids (and look each up via [`Self::symbols`]) should
    /// prefer this over [`Self::symbols_by_name_or_scan`], which clones the
    /// whole vector on every call.
    pub fn symbols_by_name(&self, name: &str) -> &[SymbolId] {
        self.symbols_by_name
            .get(name)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Return all inheritance-style ancestors of `start` (i.e. symbols
    /// reachable via `UsesTrait`, `Extends`, and `Implements` edges in PHP
    /// method-resolution order) as a breadth-first list, excluding `start`
    /// itself.
    ///
    /// This is the public counterpart of the `pub(crate)` helper used by the
    /// call resolver. It exposes the same walk for tools that need an
    /// inheritance view of the type hierarchy rather than the containment view
    /// produced by [`Self::hierarchy`].
    pub fn inheritance_ancestors(&self, start: &SymbolId) -> Vec<GraphSymbol> {
        self.walk_inheritance_ancestors(start)
            .into_iter()
            .filter_map(|id| self.symbols.get(&id))
            .cloned()
            .collect()
    }

    /// Return the first-generation direct inheritors of `target`: symbols that
    /// carry a `UsesTrait`, `Extends`, or `Implements` edge pointing **to**
    /// `target`. One hop only; callers that need transitive descendants can
    /// recurse with [`Self::inheritance_ancestors`] on each result.
    pub fn inheritance_direct_subtypes(&self, target: &SymbolId) -> Vec<GraphSymbol> {
        self.edges_by_to
            .get(target)
            .into_iter()
            .flatten()
            .filter_map(|&edge_index| {
                let edge = self.edges.get(edge_index)?;
                if matches!(
                    edge.kind,
                    EdgeKind::UsesTrait | EdgeKind::Extends | EdgeKind::Implements
                ) {
                    self.symbols.get(&edge.from)
                } else {
                    None
                }
            })
            .cloned()
            .collect()
    }

    /// Compute the impact set for a set of changed file IDs. Returns all files
    /// reachable through reverse-import propagation starting from any file
    /// whose exports changed, together with the symbols in those files and any
    /// test symbols that cover them.
    ///
    /// The three input sets mirror the parameters of
    /// [`crate::affected::compute_affected`]: `changed` is every file observed
    /// to have changed in the last refresh; `propagating` is the subset whose
    /// exports actually changed (only those push downstream invalidation);
    /// `removed` contains deleted files (always treated as propagating).
    pub fn compute_impact(
        &self,
        changed: &HashSet<FileId>,
        propagating: &HashSet<FileId>,
        removed: &HashSet<FileId>,
    ) -> ImpactSet {
        let affected_files = crate::affected::compute_affected(
            changed,
            &self.importers_by_file,
            propagating,
            removed,
        );

        let affected_symbols: Vec<GraphSymbol> = self
            .symbols
            .values()
            .filter(|sym| sym.kind != SymbolKind::File && affected_files.contains(&sym.file_id))
            .cloned()
            .collect();

        let affected_tests: Vec<GraphSymbol> = affected_symbols
            .iter()
            .filter(|sym| {
                sym.attributes
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case("test"))
                    || self
                        .edges_by_from
                        .get(&sym.id)
                        .into_iter()
                        .flatten()
                        .any(|&ei| {
                            self.edges
                                .get(ei)
                                .map(|e| e.kind == EdgeKind::TestOf)
                                .unwrap_or(false)
                        })
            })
            .cloned()
            .collect();

        ImpactSet {
            affected_files,
            affected_symbols,
            affected_tests,
        }
    }

    /// Apply warm-start resolver-cache data loaded from persistence. For each
    /// file whose on-disk fingerprint (modified-time + size) matches the stored
    /// entry, replaces the in-memory [`cross_file::ResolverSlot`] with the
    /// richer persisted version. If a `ResolverSnapshot` is provided it
    /// replaces the in-memory `importers_by_file` map entirely.
    ///
    /// Called once per `GraphManager::open_with_optional_store` after
    /// `SemanticGraph::from_parsed`; the rebuilt-from-scratch slots and
    /// importers are overwritten only for files whose fingerprint still
    /// matches, so stale entries are never applied.
    #[allow(dead_code)]
    pub(crate) fn apply_warm_resolver_cache(
        &mut self,
        entries: Vec<(FileId, resolver_cache::ResolverFileEntry)>,
        snapshot: Option<resolver_cache::ResolverSnapshot>,
    ) {
        for (file_id, entry) in entries {
            let Some(file) = self.files.get(&file_id) else {
                continue;
            };
            if file.modified_unix_millis != entry.fingerprint.modified_unix_millis
                || file.size_bytes != entry.fingerprint.size_bytes
            {
                continue;
            }
            if let Some(slot) = self.resolver_slots.get_mut(&file_id) {
                slot.exports = entry.exports;
                slot.imports = entry.imports;
                slot.supertypes = entry.supertypes;
            }
        }
        if let Some(snap) = snapshot
            && (!snap.imports_by_file.is_empty() || !snap.importers_by_file.is_empty())
        {
            self.importers_by_file.clear();
            for (target_str, importer_strs) in &snap.importers_by_file {
                let target = FileId(target_str.clone());
                let importers: Vec<FileId> =
                    importer_strs.iter().map(|s| FileId(s.clone())).collect();
                if !importers.is_empty() {
                    self.importers_by_file.insert(target, importers);
                }
            }
        }
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
        self.references_by_text.clear();
        self.children_by_parent.clear();
        self.edges_by_from.clear();
        self.edges_by_to.clear();
        self.symbols_by_language_identity.clear();
        self.files_by_normalized_id.clear();
        self.case_collisions.clear();
        self.rebuild_import_indexes();

        // Decide up front whether the body-hit lowercase shadow + trigram index
        // can be reused. They are a pure function of the ordered `body_hits`
        // text, so an unchanged fingerprint (and a still-aligned shadow vector)
        // means the existing index is exact and the costly per-hit lowercasing
        // can be skipped entirely. Only clear them when we will actually rebuild.
        let body_fingerprint = body_hits_fingerprint(&self.body_hits);
        let body_hits_unchanged = self.body_hits_fingerprint == Some(body_fingerprint)
            && self.body_hit_text_lower.len() == self.body_hits.len();
        if !body_hits_unchanged {
            self.body_hit_text_lower.clear();
            self.body_hit_trigram_index.clear();
        }

        // Build a lowercase path → FileId index for O(1) case-insensitive
        // lookups and detect case collisions that indicate Windows casing drift.
        for file_id in self.files.keys() {
            let normalized = file_id.0.to_ascii_lowercase();
            if let Some(existing) = self.files_by_normalized_id.get(&normalized) {
                if existing != file_id {
                    self.case_collisions
                        .push((existing.0.clone(), file_id.0.clone()));
                }
            } else {
                self.files_by_normalized_id
                    .insert(normalized, file_id.clone());
            }
        }

        self.symbols_by_name.reserve(self.symbols.len());
        self.symbol_signature_lower.reserve(self.symbols.len());
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
        // cheap on million-hit corpora. Skip the whole section when the body
        // hits are byte-identical to the last rebuild (the common no-op /
        // metadata-only refresh): the existing shadow vector and trigram index
        // are already exact.
        if !body_hits_unchanged {
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
            self.body_hits_fingerprint = Some(body_fingerprint);
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

    pub(crate) fn rebuild_resolution_indexes_with_cached_resolver(
        &mut self,
        resolver_loaded: bool,
    ) {
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

        if !resolver_loaded {
            self.rebuild_resolver_slots();
            self.rebuild_importers_by_file();
        }
    }

    /// Populate per-file [`cross_file::ResolverSlot`] entries. Slots are
    /// persisted by [`GraphManager::extend_resolver_cache_batch`] and restored
    /// on warm starts by `load_resolver_cache`, so the per-language
    /// flip to the phased resolver can read a ready table immediately instead
    /// of paying a one-time backfill.
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
        self.imports_by_alias.clear();
        self.wildcard_aliased_imports.clear();
        self.java_package_by_file.clear();
        self.kotlin_package_by_file.clear();
        self.scala_package_by_file.clear();
        self.imports_by_file.reserve(self.imports.len());
        self.imports_by_alias_target.reserve(self.imports.len());
        self.imports_by_alias.reserve(self.imports.len());
        for (index, import) in self.imports.iter().enumerate() {
            self.imports_by_file
                .entry(import.file_id.clone())
                .or_default()
                .push(index);
            // Index every aliased import by its local alias so the forward
            // alias-aware `reference_search` can look up "imports binding `x`"
            // directly. Package markers are skipped: their alias is an internal
            // sentinel (`__java_package__`, etc.) that a real reference text
            // can never equal, so indexing them would only add dead entries.
            if !crate::is_package_marker_alias(import.alias.as_deref())
                && let Some(alias) = import.alias.as_deref()
            {
                self.imports_by_alias
                    .entry(alias.to_string())
                    .or_default()
                    .push(index);
            }
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

/// Fingerprint the ordered `body_hits` text so `rebuild_indexes` can detect a
/// no-op refresh and reuse the existing lowercase shadow + trigram index. The
/// fingerprint covers only what those indexes derive from: the hit count and
/// each hit's text in order. Used only for within-process equality, so the
/// non-deterministic `DefaultHasher` seed is fine.
fn body_hits_fingerprint(body_hits: &[BodyHit]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    body_hits.len().hash(&mut hasher);
    for hit in body_hits {
        hit.text.hash(&mut hasher);
    }
    hasher.finish()
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
    pub resolver_entries_loaded: usize,
    pub resolver_entries_missed: usize,
    pub resolver_import_graph_loaded: bool,
    pub excluded_files: usize,
    pub excluded_dirs: usize,
    pub excluded_bytes: u64,
    pub path_conflicts: Vec<PathConflict>,
    pub coverage: IndexCoverage,
    pub bytes_seen: u64,
    pub language: LanguageReport,
    pub stats: GraphStats,
    pub indexing_decision: IndexingDecision,
    pub freshness_mode: GraphFreshnessMode,
    pub freshness_fallback_reason: Option<String>,
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
    pub path_conflicts: Vec<PathConflict>,
    pub coverage: IndexCoverage,
    pub bytes_seen: u64,
    pub bytes_reparsed: u64,
    pub language: LanguageReport,
    pub stats: GraphStats,
    pub skipped_due_to_interval: bool,
    pub budget_exhausted: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphFreshnessMode {
    Watcher,
    #[default]
    Polling,
}

impl GraphFreshnessMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Watcher => "watcher",
            Self::Polling => "polling",
        }
    }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatcherStatus {
    pub mode: WatcherMode,
    pub backend: &'static str,
    pub fallback_reason: Option<String>,
}

impl Default for WatcherStatus {
    fn default() -> Self {
        Self {
            mode: WatcherMode::Disabled,
            backend: "none",
            fallback_reason: None,
        }
    }
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

/// Map the outcome of the native watcher's `start` (and a lazily-evaluated
/// polling fallback closure) to the watcher slot + status the
/// [`GraphManager`] should publish.
///
/// Extracted so the dual-failure → [`WatcherMode::Disabled`] path can be
/// unit-tested directly, without depending on platform-specific failure
/// modes (inotify watch budget exhaustion, FUSE/NFS refusal, etc.).
///
/// B1 contract: if both backends fail, return `None` + a Disabled status
/// that captures both reasons. Do NOT propagate an error — losing the
/// whole graph for the session because the watcher couldn't attach would
/// be a strict regression versus the pre-watcher constructor.
fn resolve_watcher_attachment<P>(
    native: Result<watcher::FileWatcher>,
    polling_start: P,
) -> (Option<watcher::FileWatcher>, WatcherStatus)
where
    P: FnOnce() -> Result<watcher::FileWatcher>,
{
    match native {
        Ok(file_watcher) => (
            Some(file_watcher),
            WatcherStatus {
                mode: WatcherMode::Native,
                backend: watcher::native_backend_name(),
                fallback_reason: None,
            },
        ),
        Err(native_err) => {
            let fallback_reason = native_err.to_string();
            warn!(
                reason = %fallback_reason,
                "native watcher failed; falling back to polling watcher"
            );
            match polling_start() {
                Ok(file_watcher) => (
                    Some(file_watcher),
                    WatcherStatus {
                        mode: WatcherMode::PollingFallback,
                        backend: watcher::polling_backend_name(),
                        fallback_reason: Some(fallback_reason),
                    },
                ),
                Err(poll_err) => {
                    let combined_reason = format!("native: {fallback_reason}; polling: {poll_err}");
                    error!(
                        reason = %combined_reason,
                        "native watcher failed and polling fallback failed; live refresh disabled for this session"
                    );
                    (
                        None,
                        WatcherStatus {
                            mode: WatcherMode::Disabled,
                            backend: "none",
                            fallback_reason: Some(combined_reason),
                        },
                    )
                }
            }
        }
    }
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
    /// open the graph without watching the filesystem, and also `None` when
    /// both the native and polling backends failed to start (see
    /// `open_watching`).
    #[allow(dead_code)]
    watcher: Option<watcher::FileWatcher>,
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
    ///
    /// If the store cannot be opened (redb schema mismatch, file-lock
    /// contention, I/O error, …) a `tracing::warn` is emitted and the manager
    /// falls back to an in-memory-only graph. Callers that need a hard
    /// failure on persistence errors should call [`Self::open_with_store`]
    /// directly and pass a pre-opened [`GraphStore`].
    pub fn open_persistent_with_crawl_options(
        root: impl AsRef<Path>,
        config: RefreshConfig,
        crawl_options: CrawlOptions,
        cache_root: Option<PathBuf>,
    ) -> Result<Self> {
        let root_path = root.as_ref().to_path_buf();
        let store = match GraphStore::open(&root_path, cache_root.as_deref()) {
            Ok(store) => Some(Arc::new(store)),
            Err(error) => {
                let cache_dir = squeezy_store::cache_dir_path(&root_path, cache_root.as_deref());
                tracing::warn!(
                    target: "squeezy::graph",
                    root = %root_path.display(),
                    cache_dir = %cache_dir.display(),
                    error = %error,
                    "graph persistence disabled: GraphStore::open failed; falling back to in-memory graph",
                );
                None
            }
        };
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
        let watcher_config = watcher_config.with_default_root(manager.root.clone());
        let handle = Arc::clone(&manager.pending_changed_paths);
        let watched_root = manager.root.clone();
        let policy = Arc::clone(manager.crawler.policy());
        let native_result = watcher::FileWatcher::start(watcher_config.clone(), move |batch| {
            if let Ok(mut paths) = handle.lock() {
                for path in batch.modified.into_iter().chain(batch.removed) {
                    if watcher_path_should_enqueue(&watched_root, &policy, &path) {
                        paths.insert(path);
                    }
                }
            }
        });
        let polling_start = || {
            let handle = Arc::clone(&manager.pending_changed_paths);
            let watched_root = manager.root.clone();
            let policy = Arc::clone(manager.crawler.policy());
            watcher::FileWatcher::start_polling(watcher_config, move |batch| {
                if let Ok(mut paths) = handle.lock() {
                    for path in batch.modified.into_iter().chain(batch.removed) {
                        if watcher_path_should_enqueue(&watched_root, &policy, &path) {
                            paths.insert(path);
                        }
                    }
                }
            })
        };
        let (file_watcher, watcher_status) =
            resolve_watcher_attachment(native_result, polling_start);
        manager.watcher = file_watcher;
        manager.build_report.freshness_mode = match watcher_status.mode {
            WatcherMode::Native => GraphFreshnessMode::Watcher,
            WatcherMode::PollingFallback | WatcherMode::Disabled => GraphFreshnessMode::Polling,
        };
        manager.build_report.freshness_fallback_reason = watcher_status.fallback_reason.clone();
        manager.watcher_status = watcher_status;
        Ok(manager)
    }

    pub fn mark_polling_fallback(&mut self, reason: impl Into<String>) {
        self.build_report.freshness_mode = GraphFreshnessMode::Polling;
        self.build_report.freshness_fallback_reason = Some(reason.into());
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
        let crawler = WorkspaceCrawler::try_new(crawl_options)?;
        // Warm start: reuse the persisted per-file fingerprints so the crawl
        // skips the full read+hash of files whose size and mtime are unchanged.
        let prior_fingerprints = load_prior_fingerprints(store.as_deref(), store_metadata.as_ref());
        let snapshot =
            crawler.crawl_with_prior(&root, &prior_metadata_view(&prior_fingerprints))?;
        warn_case_collisions(&snapshot.files);
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
        let (graph, resolver_cache) = SemanticGraph::from_parsed_with_resolver_cache(
            merge_parsed_by_snapshot_order(&snapshot.files, loaded.parsed, parsed_missed),
            store.as_deref(),
            &snapshot.files,
        );
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
            resolver_entries_loaded: resolver_cache.entries_loaded,
            resolver_entries_missed: resolver_cache.entries_missed,
            resolver_import_graph_loaded: resolver_cache.import_graph_loaded,
            excluded_files: snapshot.coverage.skipped_files,
            excluded_dirs: snapshot.coverage.skipped_dirs,
            excluded_bytes: snapshot.coverage.skipped_bytes,
            path_conflicts: snapshot.path_conflicts.clone(),
            coverage: snapshot.coverage.clone(),
            bytes_seen,
            language,
            stats: graph.stats(),
            indexing_decision: snapshot.indexing_decision.clone(),
            freshness_mode: GraphFreshnessMode::Polling,
            freshness_fallback_reason: None,
        };
        let manager = Self {
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
            watcher: None,
            watcher_status: WatcherStatus {
                mode: WatcherMode::Disabled,
                backend: "none",
                fallback_reason: None,
            },
        };
        if let Some(store) = manager.store.as_deref() {
            // Cold-build commits the partition/metadata batch above with
            // error propagation; the resolver-cache rows go in a separate,
            // best-effort batch so an encoding or write failure here cannot
            // poison the freshly-built graph.
            let mut batch = GraphWriteBatch::new();
            manager.extend_resolver_cache_batch(&mut batch, ResolverCacheScope::Full);
            if !batch.is_empty() {
                let _ = store.apply_graph_batch(&batch);
            }
        }
        Ok(manager)
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

    pub fn freshness_mode(&self) -> GraphFreshnessMode {
        self.build_report.freshness_mode
    }

    pub fn freshness_fallback_reason(&self) -> Option<&str> {
        self.build_report.freshness_fallback_reason.as_deref()
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

    pub fn watcher_status(&self) -> &WatcherStatus {
        &self.watcher_status
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

    /// Extend `batch` with the V2 resolver-cache rows. Per-file entries
    /// carry the workspace-side fingerprint that future warm-start reads
    /// will compare against. The single-blob import adjacency is mirrored
    /// from [`SemanticGraph::importers_by_file`]. Encoding failures are
    /// swallowed so persistence errors cannot poison the in-memory graph.
    ///
    /// Callers fold this into the same `GraphWriteBatch` they already
    /// stage partition/metadata changes onto, so the resulting redb
    /// commit covers metadata, partitions, and resolver cache in one
    /// fsync. Deletion of stale rows for removed files is handled by
    /// the caller via [`GraphWriteBatch::remove_resolver_entry`].
    ///
    /// `scope` controls how much is re-encoded:
    ///  - [`ResolverCacheScope::Full`] re-encodes every file entry and always
    ///    rewrites the import-graph blob (cold build / first persist).
    ///  - [`ResolverCacheScope::Incremental`] re-encodes only the changed
    ///    files' entries and rewrites the import-graph blob only when an import
    ///    edge actually changed — so a one-file refresh no longer pays the cost
    ///    of re-encoding every resolver row plus the whole adjacency blob.
    fn extend_resolver_cache_batch(&self, batch: &mut GraphWriteBatch, scope: ResolverCacheScope) {
        let upsert_entry = |file_id: &FileId, batch: &mut GraphWriteBatch| {
            let Some(file) = self.graph.files.get(file_id) else {
                return;
            };
            let Some(slot) = self.graph.resolver_slots.get(file_id) else {
                return;
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
            // Encoding failure: skip this file; warm-start will recompute.
            let _ = batch.upsert_resolver_entry(file_id, &entry);
        };

        let rewrite_import_graph = match scope {
            ResolverCacheScope::Full => {
                for file_id in self.graph.files.keys() {
                    upsert_entry(file_id, batch);
                }
                true
            }
            ResolverCacheScope::Incremental {
                changed,
                import_edges_changed,
            } => {
                for file_id in changed {
                    upsert_entry(file_id, batch);
                }
                import_edges_changed
            }
        };

        if rewrite_import_graph {
            let mut snapshot = resolver_cache::ResolverSnapshot::new();
            for (target, importers) in &self.graph.importers_by_file {
                for importer in importers {
                    snapshot.record_edge(importer, target);
                }
            }
            let _ = batch.set_import_graph(&snapshot);
        }
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
                path_conflicts: self.build_report.path_conflicts.clone(),
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
                path_conflicts: self.build_report.path_conflicts.clone(),
                coverage: self.build_report.coverage.clone(),
                bytes_seen: self.graph.files.values().map(|file| file.size_bytes).sum(),
                bytes_reparsed: 0,
                language: language_report(self.graph.files.values()),
                stats: self.graph.stats(),
                skipped_due_to_interval: true,
                budget_exhausted: false,
            });
        }

        // Incremental refresh re-crawls the whole workspace, but the vast
        // majority of files are unchanged between refreshes. Feed the prior
        // fingerprints in so unchanged files are stat-checked instead of fully
        // re-read+hashed, keeping refresh cost proportional to the change set
        // rather than the workspace size.
        let mut prior_fingerprints =
            load_prior_fingerprints(self.store.as_deref(), self.store_metadata.as_ref());
        // A path the watcher flagged as changed must be re-read+hashed even if
        // its size and mtime look unchanged: editors can rewrite a file with
        // identical length and a clamped/preserved mtime, and some filesystems
        // round mtime coarsely enough that a fast edit lands in the same tick.
        // Dropping these paths' prior fingerprints before the crawl forces the
        // size+mtime fast-path to fall through to a content hash for exactly
        // the flagged paths, without changing the crawler.
        let pending_before_crawl = self
            .pending_changed_paths
            .lock()
            .map(|paths| paths.clone())
            .unwrap_or_default();
        if !prior_fingerprints.is_empty() && !pending_before_crawl.is_empty() {
            let pending_relative_keys = relative_keys_for_paths(&self.root, &pending_before_crawl);
            if !pending_relative_keys.is_empty() {
                prior_fingerprints.retain(|key, _| !pending_relative_keys.contains(key));
            }
        }
        let snapshot = self
            .crawler
            .crawl_with_prior(&self.root, &prior_metadata_view(&prior_fingerprints))?;
        warn_case_collisions(&snapshot.files);
        let files_seen = snapshot.files.len();
        let coverage = snapshot.coverage.clone();
        let path_conflicts = snapshot.path_conflicts.clone();
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
        // Pre-compute canonical forms for all pending event paths once so the
        // matching loops below do not repeatedly call canonicalize on the same
        // event path. Policy-pruned paths skip canonicalization entirely.
        let pending_canonicals = PendingCanonicals::from_paths(
            &self.root,
            self.crawler.policy(),
            &pending_changed_paths,
        );
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
        // Pre-compute canonical paths for all changed (supported + metadata +
        // unsupported) and removed records once. The same Vec is reused for
        // both the `changed_paths_from_events` count and the
        // `event_changed_or_removed` count, avoiding double canonicalization.
        // Non-metadata unsupported files are included so that a watcher event
        // for a changed .txt/.json file is not falsely reported as unmatched.
        let all_changed_canonicals: Vec<(PathBuf, Option<PathBuf>)> = changed_records
            .iter()
            .chain(
                unsupported_changed_records
                    .iter()
                    .filter(|r| !metadata_changed_records.iter().any(|m| m.id == r.id)),
            )
            .map(|r| (r.path.clone(), std::fs::canonicalize(&r.path).ok()))
            .collect();
        let changed_paths_from_events = changed_records
            .iter()
            .filter(|record| {
                all_changed_canonicals
                    .iter()
                    .find(|(raw, _)| raw == &record.path)
                    .map(|(raw, rec_can)| pending_canonicals.matches_record(raw, rec_can.as_ref()))
                    .unwrap_or(false)
            })
            .count();
        let changed_paths_from_polling = changed_records
            .len()
            .saturating_sub(changed_paths_from_events);
        let event_changed_or_removed = {
            let removed_canonicals: Vec<(PathBuf, Option<PathBuf>)> = removed_files_all
                .iter()
                .filter_map(|id| self.graph.files.get(id))
                .map(|old| (old.path.clone(), std::fs::canonicalize(&old.path).ok()))
                .collect();
            let path_pair_matches =
                |raw: &PathBuf, canon: &Option<PathBuf>, vec: &[(PathBuf, Option<PathBuf>)]| {
                    vec.iter().any(|(rec_raw, rec_can)| {
                        raw == rec_raw
                            || canon
                                .as_ref()
                                .zip(rec_can.as_ref())
                                .map(|(l, r)| l == r)
                                .unwrap_or(false)
                    })
                };
            pending_canonicals
                .entries
                .iter()
                .filter(|(raw, canon)| {
                    path_pair_matches(raw, canon, &all_changed_canonicals)
                        || path_pair_matches(raw, canon, &removed_canonicals)
                })
                .count()
        };
        let unchanged_event_paths = pending_canonicals
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
            // Drop the matching resolver-cache row so deleted files do
            // not accumulate as dead weight in `RESOLVER_SNAPSHOT_PER_FILE`
            // — the per-file partition removal above already covers
            // `GRAPH_PARTITIONS`, but the resolver row would otherwise
            // outlive its file and over-count
            // `cache_diagnostics.resolver_entries`.
            graph_batch.remove_resolver_entry(file_id);
        }
        for file_id in &unsupported_removed_files {
            self.graph.files.remove(file_id);
            graph_batch.remove_partition(file_id);
            // Best-effort: unsupported files rarely carry resolver rows, but
            // when they do (e.g. a file flipped from supported to unsupported
            // on a previous run) the row would otherwise leak.
            graph_batch.remove_resolver_entry(file_id);
        }
        // Track whether a supported->Unsupported flip purged derived data via
        // `remove_file_data`. When that is the only change in a refresh (no
        // reparsed files, no metadata change, no removals) the index rebuild
        // below would otherwise be skipped, leaving `edges_by_from`/
        // `symbols_by_name`/etc. pointing at the just-purged symbols.
        let mut unsupported_purged = false;
        for record in unsupported_changed_records {
            // A file that flipped from a supported language to unsupported
            // still has its old symbols/edges/calls/references/packages/facts
            // in the graph. Purge all derived data for the file before
            // recording the unsupported placeholder, otherwise the stale rows
            // remain queryable and poison every downstream tool.
            let was_supported = self
                .graph
                .files
                .get(&record.id)
                .map(|old| old.language != LanguageKind::Unsupported)
                .unwrap_or(false);
            self.graph.remove_file_data(&record.id);
            self.graph.files.insert(record.id.clone(), record.clone());
            if was_supported {
                unsupported_purged = true;
            }
        }

        // Reparse changed records in budget-bounded chunks. `supported_changed_records`
        // is already sorted by relative path, so chunking preserves a
        // deterministic processing order. Each chunk goes through
        // `parse_records`, which fans the work across worker threads once the
        // chunk is large enough — far cheaper than the previous one-record-at-a-
        // time loop on a multi-file save or branch switch. The budget is
        // re-checked between chunks so a long refresh still yields, just at
        // chunk granularity instead of per file.
        const REPARSE_CHUNK_SIZE: usize = 64;
        let mut parsed_files = Vec::with_capacity(supported_changed_records.len());
        for chunk in supported_changed_records.chunks(REPARSE_CHUNK_SIZE) {
            if started.elapsed() > self.config.per_tool_refresh_budget {
                budget_exhausted = true;
                break;
            }
            let (mut parsed_chunk, _summary) = self.parser.parse_records(chunk)?;
            bytes_reparsed += chunk.iter().map(|record| record.size_bytes).sum::<u64>();
            reparsed_files += parsed_chunk.len();
            parsed_files.append(&mut parsed_chunk);
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
        } else if metadata_refresh_needed || !removed_files.is_empty() || unsupported_purged {
            self.graph.rebuild_java_project_facts();
            self.graph.rebuild_dotnet_project_facts();
            self.graph.rebuild_kotlin_project_facts();
            self.graph.rebuild_semantic_edges();
            self.graph.rebuild_indexes();
        }
        // Fold the resolver-cache rows (per-file entries + import-graph
        // blob) into the same batch as the partition/metadata changes so a
        // refresh that touches one or two files pays a single redb fsync
        // instead of one for partitions and a second for the resolver
        // cache. Best-effort: encoding or write failure must not poison
        // the in-memory graph update; the warm-start path will fall back
        // to a full rebuild when it cannot find an entry.
        if let Some(store) = self.store.as_deref() {
            // Only re-encode the resolver rows for files this refresh touched.
            // The import-graph blob is regenerated from `importers_by_file`,
            // which only changes when the semantic edges were rebuilt (a reparse,
            // a removal, or a metadata/unsupported-flip change), so gate the
            // blob rewrite on exactly those conditions.
            let changed_set: HashSet<FileId> = changed_files.iter().cloned().collect();
            let import_edges_changed = reparsed_files > 0
                || !removed_files.is_empty()
                || metadata_refresh_needed
                || unsupported_purged;
            self.extend_resolver_cache_batch(
                &mut graph_batch,
                ResolverCacheScope::Incremental {
                    changed: &changed_set,
                    import_edges_changed,
                },
            );
            if !graph_batch.is_empty() {
                let _ = store.apply_graph_batch(&graph_batch);
            }
        }

        // Only declare the pending set fully drained when we actually parsed
        // every changed file. If the per-refresh budget broke the loop early,
        // some changed paths were never reparsed; clearing the set and
        // advancing `last_refresh` here would let the next query skip refresh
        // for the whole idle interval and serve stale data for those files.
        // Leave the pending paths queued (already-parsed ones become cheap
        // no-ops on the next pass) and leave `last_refresh` untouched so the
        // next `refresh_before_query` still picks them up immediately.
        //
        // Drain by set-difference, not a blanket `clear()`: the watcher runs on
        // a background thread and can push new paths into the set while this
        // refresh crawls/parses. Those concurrent events arrived *after* the
        // snapshot we processed, so clearing everything would silently swallow
        // them. Remove only the paths we observed at the start of this refresh
        // and leave anything newer queued for the next pass.
        if !budget_exhausted {
            let mut live_set_remaining = false;
            if let Ok(mut paths) = self.pending_changed_paths.lock() {
                paths.retain(|p| !pending_before_crawl.contains(p));
                live_set_remaining = !paths.is_empty();
            }
            // Advancing `last_refresh` gates both the idle-interval skip and the
            // debounce skip. Only advance when no concurrent events remain;
            // otherwise the next `refresh_now` would be debounce-suppressed even
            // though there is fresh work waiting in the live set.
            if !live_set_remaining {
                self.last_refresh = Instant::now();
            }
        }
        self.build_report.path_conflicts = path_conflicts.clone();
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
            path_conflicts,
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

fn watcher_path_should_enqueue(root: &Path, policy: &CompiledIndexingPolicy, path: &Path) -> bool {
    let relative = path.strip_prefix(root).unwrap_or(path);
    // Hard guard: always drop VCS metadata and Squeezy's own cache
    // (`VCS_AND_CACHE_DIR_NAMES`) regardless of policy so persisting the graph
    // can never self-trigger a refresh loop.
    if relative.components().any(|component| {
        let std::path::Component::Normal(name) = component else {
            return false;
        };
        let Some(name) = name.to_str() else {
            return false;
        };
        VCS_AND_CACHE_DIR_NAMES.contains(&name)
    }) {
        return false;
    }
    // Policy-aware prune: a watcher event under a default-pruned dir
    // (`target/`, `node_modules/`, `dist/`, …) is high-churn noise that the
    // crawl would skip anyway, so enqueuing it just burns refresh budget. Reuse
    // the crawler's keep/prune decision (`path_reason`) instead of a bare dir-
    // name check so an `include` glob that re-enables a subset is honoured: when
    // the policy would still index this path, `path_reason` returns `None` and
    // we keep it.
    let relative_str = relative.to_string_lossy();
    let relative_str = if relative_str.contains('\\') {
        relative_str.replace('\\', "/")
    } else {
        relative_str.into_owned()
    };
    if relative_str.is_empty() {
        return true;
    }
    policy.path_reason(&relative_str, false).is_none()
}

/// Scope for [`GraphManager::extend_resolver_cache_batch`]: how many
/// resolver-cache rows to re-encode and whether to rewrite the import-graph
/// blob.
enum ResolverCacheScope<'a> {
    /// Re-encode every file entry and rewrite the import-graph blob.
    Full,
    /// Re-encode only `changed` files' entries; rewrite the import-graph blob
    /// only when `import_edges_changed`.
    Incremental {
        changed: &'a HashSet<FileId>,
        import_edges_changed: bool,
    },
}

struct LoadedPartitions {
    parsed: Vec<ParsedFile>,
    missed_records: Vec<FileRecord>,
    loaded_files: usize,
    rebuilt: bool,
}

#[derive(Debug, Default)]
struct ResolverCacheLoadReport {
    entries_loaded: usize,
    entries_missed: usize,
    import_graph_loaded: bool,
}

fn load_resolver_cache(
    store: Option<&GraphStore>,
    graph: &mut SemanticGraph,
    records: &[FileRecord],
) -> Result<ResolverCacheLoadReport> {
    let Some(store) = store else {
        return Ok(ResolverCacheLoadReport {
            entries_loaded: 0,
            entries_missed: records.len(),
            import_graph_loaded: false,
        });
    };
    let ids = records
        .iter()
        .map(|record| record.id.clone())
        .collect::<Vec<_>>();
    let entries = store.resolver_entries_for::<resolver_cache::ResolverFileEntry>(&ids)?;
    let mut by_id = entries.into_iter().collect::<HashMap<_, _>>();

    // Pass 1: validate all entries without mutating the graph. If any entry is
    // missing or has a stale fingerprint the whole cache is unusable (because
    // rebuild_resolver_slots would clear resolver_slots immediately), so we bail
    // early rather than inserting matched entries that would be discarded anyway.
    let mut slots = Vec::with_capacity(records.len());
    let mut entries_missed: usize = 0;
    for record in records {
        let Some(entry) = by_id.remove(&record.id) else {
            entries_missed += 1;
            continue;
        };
        if entry.fingerprint.modified_unix_millis != record.modified_unix_millis
            || entry.fingerprint.size_bytes != record.size_bytes
        {
            entries_missed += 1;
            continue;
        }
        slots.push((
            record.id.clone(),
            cross_file::ResolverSlot {
                exports: entry.exports,
                imports: entry.imports,
                supertypes: entry.supertypes,
            },
        ));
    }

    if entries_missed > 0 {
        return Ok(ResolverCacheLoadReport {
            entries_loaded: 0,
            entries_missed,
            import_graph_loaded: false,
        });
    }

    // Check the import-graph blob BEFORE inserting slots. If it is absent
    // or unreadable, the caller's gate
    // (`entries_missed == 0 && import_graph_loaded`) would force a
    // `rebuild_resolver_slots()` that wipes anything we just inserted —
    // paying slot-population cost for nothing. Bail early instead.
    let Some(snapshot) = store.import_graph::<resolver_cache::ResolverSnapshot>()? else {
        return Ok(ResolverCacheLoadReport {
            entries_loaded: 0,
            entries_missed: 0,
            import_graph_loaded: false,
        });
    };

    // Pass 2: all entries matched and import-graph available — commit into the graph.
    let entries_loaded = slots.len();
    for (id, slot) in slots {
        graph.resolver_slots.insert(id, slot);
    }

    let known = graph.files.keys().cloned().collect::<HashSet<_>>();
    let mut importers_by_file = HashMap::new();
    for (target, importers) in snapshot.importers_by_file {
        let target = FileId::new(target);
        if !known.contains(&target) {
            continue;
        }
        let importers = importers
            .into_iter()
            .map(FileId::new)
            .filter(|id| known.contains(id) && id != &target)
            .collect::<Vec<_>>();
        if !importers.is_empty() {
            importers_by_file.insert(target, importers);
        }
    }
    graph.importers_by_file = importers_by_file;

    Ok(ResolverCacheLoadReport {
        entries_loaded,
        entries_missed: 0,
        import_graph_loaded: true,
    })
}

/// Per-file fingerprint deserialised from a persisted graph partition. Only
/// the fields the crawl's mtime/size fast-path needs are pulled out of the
/// stored `ParsedFile` JSON; serde ignores everything else, so reading these
/// is far cheaper than materialising the whole parse result.
#[derive(Deserialize)]
struct PersistedFingerprint {
    file: PersistedFingerprintFile,
}

#[derive(Deserialize)]
struct PersistedFingerprintFile {
    relative_path: String,
    hash: ContentHash,
    size_bytes: u64,
    modified_unix_millis: u128,
}

/// Owned prior-crawl fingerprints, keyed by relative path. Borrowed as a
/// [`PriorFileMetadata`] when handed to the crawl.
type PriorFingerprints = HashMap<String, (u64, u128, ContentHash)>;

/// Compute the set of crawl-relative path keys (forward-slash separated,
/// matching the workspace crawler's `relative_path` spelling) for a set of
/// absolute paths, relative to `root`. Each path is matched against both the
/// raw `root` and its canonical form so a `root` spelled differently from the
/// watcher's canonicalised paths (e.g. `/var` vs `/private/var` on macOS) still
/// produces a usable key. Paths that fall outside `root` are skipped.
fn relative_keys_for_paths(root: &Path, paths: &HashSet<PathBuf>) -> HashSet<String> {
    let canonical_root = root.canonicalize().ok();
    let to_key = |relative: &Path| -> String {
        let relative = relative.to_string_lossy();
        if relative.contains('\\') {
            relative.replace('\\', "/")
        } else {
            relative.into_owned()
        }
    };
    let mut keys = HashSet::with_capacity(paths.len());
    for path in paths {
        if let Ok(relative) = path.strip_prefix(root) {
            keys.insert(to_key(relative));
            continue;
        }
        // Fall back to matching against the canonical root, and to the
        // canonical form of the path itself, so symlinked roots still resolve.
        if let Some(canonical_root) = &canonical_root {
            if let Ok(relative) = path.strip_prefix(canonical_root) {
                keys.insert(to_key(relative));
                continue;
            }
            if let Ok(canonical_path) = path.canonicalize()
                && let Ok(relative) = canonical_path.strip_prefix(canonical_root)
            {
                keys.insert(to_key(relative));
            }
        }
    }
    keys
}

/// Load the prior crawl's per-file fingerprints so the next crawl can skip the
/// full read+hash of unchanged files (see
/// [`WorkspaceCrawler::crawl_with_prior`]).
///
/// Returns empty (forcing a from-scratch read+hash) when there is no store, no
/// persisted metadata, or the persisted metadata no longer matches the current
/// crawl options. The metadata gate mirrors [`load_persisted_partitions`]:
/// when it does not match, the persisted partitions are stale and about to be
/// cleared, so their fingerprints must not be trusted.
fn load_prior_fingerprints(
    store: Option<&GraphStore>,
    expected_metadata: Option<&GraphStoreMetadata>,
) -> PriorFingerprints {
    let Some(store) = store else {
        return PriorFingerprints::new();
    };
    let metadata_matches = match (store.graph_metadata(), expected_metadata) {
        (Ok(Some(existing)), Some(expected)) => existing == *expected,
        _ => false,
    };
    if !metadata_matches {
        return PriorFingerprints::new();
    }
    match store.graph_partition_entries::<PersistedFingerprint>() {
        Ok(entries) => entries
            .into_iter()
            .map(|(_, fingerprint)| {
                let file = fingerprint.file;
                (
                    file.relative_path,
                    (file.size_bytes, file.modified_unix_millis, file.hash),
                )
            })
            .collect(),
        // A read or decode failure here only costs us the fast-path; the crawl
        // still produces correct output by reading+hashing every file.
        Err(_) => PriorFingerprints::new(),
    }
}

/// Borrow the owned fingerprints as the map shape the crawl consumes.
fn prior_metadata_view(fingerprints: &PriorFingerprints) -> PriorFileMetadata<'_> {
    fingerprints
        .iter()
        .map(|(path, (size_bytes, modified_unix_millis, hash))| {
            (
                path.as_str(),
                PriorFileMeta {
                    size_bytes: *size_bytes,
                    modified_unix_millis: *modified_unix_millis,
                    hash,
                },
            )
        })
        .collect()
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
                // The per-element `file_id` is `#[serde(skip)]` in the persisted
                // partition (it is constant within a file and equals the redb
                // key), so restore it from the record here — this is the only
                // site that deserializes a full `ParsedFile` from the store, and
                // it runs before the partition is merged into the graph.
                backfill_element_file_ids(&mut persisted, &record.id);
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

/// Restore the `#[serde(skip)]` per-element `file_id` on a partition loaded from
/// the store. The id is constant within a file and equals the partition key, so
/// it is stamped from `file_id` rather than persisted on every symbol/import/
/// call/reference/body_hit. Fresh (in-memory) parses set it directly via the
/// extractors and never go through this path.
fn backfill_element_file_ids(parsed: &mut ParsedFile, file_id: &FileId) {
    for symbol in &mut parsed.symbols {
        symbol.file_id = file_id.clone();
    }
    for import in &mut parsed.imports {
        import.file_id = file_id.clone();
    }
    for call in &mut parsed.calls {
        call.file_id = file_id.clone();
    }
    for reference in &mut parsed.references {
        reference.file_id = file_id.clone();
    }
    for hit in &mut parsed.body_hits {
        hit.file_id = file_id.clone();
    }
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
        "languages": crawl_options.languages,
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
    let canonical_root = std::fs::canonicalize(root).ok();
    let canonical_root = canonical_root.as_deref();
    let metadata = serde_json::from_str::<CargoMetadataJson>(metadata_json).map_err(|err| {
        SqueezyError::Graph(format!("failed to parse cargo metadata JSON: {err}"))
    })?;
    let mut nodes = Vec::new();
    nodes.push(CargoFactNode {
        id: "cargo:workspace".to_string(),
        kind: CargoFactNodeKind::Workspace,
        name: normalize_optional_cargo_path(
            root,
            canonical_root,
            metadata.workspace_root.as_deref(),
        )
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
        let manifest_path =
            normalize_optional_cargo_path(root, canonical_root, package.manifest_path.as_deref());
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
                source_path: normalize_optional_cargo_path(
                    root,
                    canonical_root,
                    target.src_path.as_deref(),
                ),
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
        workspace_root: normalize_optional_cargo_path(
            root,
            canonical_root,
            metadata.workspace_root.as_deref(),
        ),
        target_directory: normalize_optional_cargo_path(
            root,
            canonical_root,
            metadata.target_directory.as_deref(),
        ),
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
    // Compute once so the fallback path in normalize_cargo_file_id does not
    // repeat the stat/readlink syscalls for every diagnostic span.
    let canonical_root = std::fs::canonicalize(root).ok();
    let canonical_root = canonical_root.as_deref();
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
                raw_path: None,
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
            let file_id =
                normalize_cargo_file_id(root, canonical_root, &span.file_name).map(FileId::new);
            // When the path cannot be mapped to a workspace-relative FileId,
            // keep the raw compiler path so callers can report both the raw
            // diagnostic path and the workspace root spelling, helping users
            // spot container, symlink, or bind-mount path-spelling mismatches.
            let raw_path = if file_id.is_none()
                && !span.file_name.is_empty()
                && !span.file_name.starts_with('<')
            {
                Some(span.file_name.clone())
            } else {
                None
            };
            diagnostics.push(CargoDiagnostic {
                level: message.level.clone(),
                message: message.message.clone(),
                code: message.code.as_ref().map(|code| code.code.clone()),
                file_id,
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
                raw_path,
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

fn normalize_optional_cargo_path(
    root: &Path,
    canonical_root: Option<&Path>,
    path: Option<&str>,
) -> Option<String> {
    path.and_then(|path| normalize_cargo_file_id(root, canonical_root, path))
}

/// Map a cargo compiler span path to a workspace-relative slash-joined string
/// suitable for use as a `FileId`.
///
/// `canonical_root` should be the pre-computed `canonicalize(root)` result,
/// passed in by the caller so that the fallback path does not repeat the
/// syscall once per diagnostic span.
fn normalize_cargo_file_id(
    root: &Path,
    canonical_root: Option<&Path>,
    path: &str,
) -> Option<String> {
    if path.starts_with('<') {
        return None;
    }
    let path = Path::new(path);
    let relative = if path.is_absolute() {
        // Fast path: exact prefix match (the common case).
        if let Ok(rel) = path.strip_prefix(root) {
            rel.to_path_buf()
        } else {
            #[cfg(windows)]
            if let Some(rel) = case_insensitive_relative_path(root, path) {
                return Some(
                    rel.components()
                        .filter_map(|component| match component {
                            std::path::Component::Normal(part) => {
                                Some(part.to_string_lossy().to_string())
                            }
                            std::path::Component::CurDir => None,
                            _ => Some(component.as_os_str().to_string_lossy().to_string()),
                        })
                        .collect::<Vec<_>>()
                        .join("/"),
                );
            }
            // Fallback: canonicalize the diagnostic path and retry against the
            // pre-computed canonical root. This handles symlinked workspaces,
            // bind-mounted build environments, and container setups where cargo
            // emits a different path spelling than the workspace root squeezy
            // was opened with.
            let cr = canonical_root?;
            let canonical_path = std::fs::canonicalize(path).ok()?;
            canonical_path.strip_prefix(cr).ok()?.to_path_buf()
        }
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

#[cfg(any(windows, test))]
fn case_insensitive_relative_path(root: &Path, path: &Path) -> Option<PathBuf> {
    // Use lowercase for comparison only, then slice the original
    // slash-normalized path so relative-path casing (e.g. `src/MyModule.cs`)
    // is preserved in the FileId.
    let path_str = path.to_string_lossy();
    let path_norm = path_str.replace('\\', "/");
    let root_str = root.to_string_lossy();
    let root_norm = root_str.replace('\\', "/");
    let path_lower = path_norm.to_ascii_lowercase();
    let root_lower = root_norm.to_ascii_lowercase();
    let root_prefix = if root_lower.ends_with('/') {
        root_lower.clone()
    } else {
        format!("{root_lower}/")
    };
    // Verify the match, then use the prefix length to slice the original
    // normalized path.
    path_lower.strip_prefix(&root_prefix)?;
    let remainder = &path_norm[root_prefix.len()..];
    Some(Path::new(remainder).to_path_buf())
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

/// Pre-computed canonical paths for a set of pending watcher event paths.
///
/// [`paths_match`] calls `canonicalize` twice per invocation. When matching
/// `N` pending paths against `M` record paths, the naive approach calls
/// `canonicalize` O(N×M) times. This struct computes each pending-path
/// canonical form once at construction and reuses it for all subsequent
/// `matches` calls, reducing the stat/readlink count to O(N + M).
struct PendingCanonicals {
    /// Each entry is `(raw_path, canonicalized_path_or_none)`.
    entries: Vec<(PathBuf, Option<PathBuf>)>,
}

impl PendingCanonicals {
    /// Build the canonical-path cache, but skip the `canonicalize` (a stat +
    /// readlink syscall) for any path the indexing policy would prune: those
    /// paths can never match a kept crawl record, so their canonical form is
    /// dead weight. Skipping them keeps the syscall count proportional to the
    /// paths that can actually match instead of to every churny build-dir event
    /// that slipped into the pending set.
    fn from_paths(root: &Path, policy: &CompiledIndexingPolicy, paths: &HashSet<PathBuf>) -> Self {
        let entries = paths
            .iter()
            .map(|p| {
                let canonical = if watcher_path_should_enqueue(root, policy, p) {
                    std::fs::canonicalize(p).ok()
                } else {
                    None
                };
                (p.clone(), canonical)
            })
            .collect();
        Self { entries }
    }

    /// Return `true` if `record_path` is covered by any pending event path.
    /// `record_canonical` is the caller-supplied canonical form of the record
    /// path (may be `None` if canonicalization failed, e.g. for a file that
    /// was deleted after the crawl).
    fn matches_record(&self, record_path: &Path, record_canonical: Option<&PathBuf>) -> bool {
        self.entries.iter().any(|(raw, canon)| {
            filesystem_paths_match(raw, record_path)
                || canon
                    .as_ref()
                    .zip(record_canonical)
                    .map(|(l, r)| l == r)
                    .unwrap_or(false)
        })
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
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

/// Extract the implementing-type leaf name from a trimmed Rust impl header.
/// `Trait for Concrete` -> `Concrete`, `Concrete<T>` -> `Concrete`. Returns an
/// empty string when no usable identifier can be parsed (e.g. a bare `impl`
/// header that survived trimming oddly). Mirrors the parsing in
/// [`impl_header_matches_type`].
fn impl_header_type_name(header: &str) -> String {
    let own_type = header
        .split_once(" for ")
        .map(|(_, target)| target)
        .unwrap_or(header)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != ':');
    last_path_segment(own_type)
}

/// Extract the trait leaf name from a trimmed Rust impl header, or `None` for
/// an inherent `impl Concrete` block with no trait. `Trait for Concrete` ->
/// `Some("Trait")`. Mirrors the parsing in [`impl_header_implements_trait`].
fn impl_header_trait_name(header: &str) -> Option<String> {
    let (trait_part, _) = header.split_once(" for ")?;
    let trait_part = trait_part
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != ':');
    let leaf = last_path_segment(trait_part);
    if leaf.is_empty() { None } else { Some(leaf) }
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

/// Detect indexed file paths that differ only in case. Linux filesystems
/// preserve exact case-sensitive identity, so both paths are indexed
/// correctly on ext4/xfs/btrfs. However, the same spellings may collide
/// on macOS/Windows or on Linux case-insensitive mounts, and persisted
/// cache entries keyed by the exact `FileId` string will be unusable after
/// a checkout on those platforms.
///
/// Returns one `[a, b]` pair for every distinct colliding combination so
/// an N-way collision yields C(N,2) pairs. Returning the pairs makes the
/// function testable without a tracing subscriber.
fn detect_case_collisions(files: &[squeezy_workspace::FileRecord]) -> Vec<[String; 2]> {
    let mut lower_to_paths: HashMap<String, Vec<&str>> = HashMap::with_capacity(files.len());
    for file in files {
        lower_to_paths
            .entry(file.relative_path.to_lowercase())
            .or_default()
            .push(&file.relative_path);
    }
    let mut collisions = Vec::new();
    for paths in lower_to_paths.values() {
        if paths.len() >= 2 {
            for i in 0..paths.len() {
                for j in (i + 1)..paths.len() {
                    collisions.push([paths[i].to_string(), paths[j].to_string()]);
                }
            }
        }
    }
    collisions
}

/// Emit a `tracing::warn!` for each case-colliding path pair detected by
/// [`detect_case_collisions`].
fn warn_case_collisions(files: &[squeezy_workspace::FileRecord]) {
    for [a, b] in detect_case_collisions(files) {
        tracing::warn!(
            "squeezy: case-collision: '{}' and '{}' fold to the same lowercase path; \
             cross-platform checkout or cached-entry reuse may be unreliable",
            a,
            b,
        );
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
