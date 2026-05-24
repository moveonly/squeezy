use std::{
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

pub mod backend;
mod languages;
mod references;
mod resolution;

use serde::{Deserialize, Serialize};
use squeezy_core::{
    Confidence, ContentHash, EdgeKind, FileId, Freshness, LanguageFamily, LanguageKind, Provenance,
    Result, SourceSpan, SymbolId, SymbolKind,
};
use squeezy_parse::{
    BodyHit, BodyHitKind, LanguageParser, ParsedCall, ParsedCallKind, ParsedFile, ParsedImport,
    ParsedReference, ParsedSymbol, ReferenceKind, edge_kind_for_call,
};
use squeezy_store::{GraphStoreMetadata, GraphWriteBatch, SqueezyStore};
use squeezy_workspace::{CrawlOptions, FileRecord, IndexCoverage, WorkspaceCrawler};

use crate::languages::{
    csharp::{
        csharp_import_matches_symbol, dotnet_configured_source_facts, dotnet_dependency_facts,
        dotnet_project_metadata_provider, dotnet_source_root_facts, dotnet_target_facts,
    },
    java::{
        java_build_metadata_provider, java_configured_source_facts, java_dependency_facts,
        java_paths_signature, java_source_root_facts,
    },
    js_ts::{JsTsResolver, is_js_ts_language},
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
    pub signature: String,
    pub visibility: Option<String>,
    pub docs: Vec<String>,
    pub attributes: Vec<String>,
    pub provenance: Provenance,
    pub confidence: Confidence,
    pub freshness: Freshness,
    pub dirty: Option<DirtyAnnotation>,
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
            signature: symbol.signature,
            visibility: symbol.visibility,
            docs: symbol.docs,
            attributes: symbol.attributes,
            provenance: symbol.provenance,
            confidence: symbol.confidence,
            freshness: symbol.freshness,
            dirty: None,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LanguageFact {
    Java(JavaProjectFact),
    Dotnet(DotnetProjectFact),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphStats {
    pub files: usize,
    pub symbols: usize,
    pub edges: usize,
    pub body_hits: usize,
    pub references: usize,
    pub calls: usize,
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
    java_project_facts_cache: HashMap<FileId, CachedJavaProjectFacts>,
    java_project_facts_cache_java_paths_signature: u64,
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
    /// Indices into [`Self::imports`] grouped by the file that introduced
    /// them. `import_visible_from_symbol` only ever returns true when the
    /// import shares a file with the caller, so resolving an alias or
    /// reference no longer needs to scan every import in the workspace.
    imports_by_file: HashMap<FileId, Vec<usize>>,
    java_package_by_file: HashMap<FileId, Vec<String>>,
    js_ts_resolver: JsTsResolver,
}

#[derive(Debug, Clone)]
struct CachedJavaProjectFacts {
    hash: ContentHash,
    java_paths_signature: u64,
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
            java_project_facts_cache: HashMap::new(),
            java_project_facts_cache_java_paths_signature: 0,
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
            imports_by_file: HashMap::new(),
            java_package_by_file: HashMap::new(),
            js_ts_resolver: JsTsResolver::default(),
        }
    }

    pub fn from_parsed(files: Vec<ParsedFile>) -> Self {
        let mut graph = Self::empty();
        for file in files {
            graph.insert_parsed_file(file);
        }
        graph.rebuild_java_project_facts();
        graph.rebuild_dotnet_project_facts();
        graph.rebuild_semantic_edges();
        graph.rebuild_indexes();
        graph
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
        self.rebuild_semantic_edges();
        self.rebuild_indexes();
    }

    pub fn remove_file(&mut self, file_id: &FileId) {
        self.remove_file_data(file_id);
        self.rebuild_java_project_facts();
        self.rebuild_dotnet_project_facts();
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
        GraphStats {
            files: self.files.len(),
            symbols: self.symbols.len(),
            edges: self.edges.len(),
            body_hits: self.body_hits.len(),
            references: self.references.len(),
            calls: self.calls.len(),
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
            .collect()
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
        let mut hits = self
            .reference_candidate_indexes(text)
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
        let Some(symbol) = self.symbols.get(symbol_id) else {
            return Vec::new();
        };
        let mut hits = self
            .reference_candidate_indexes_for_symbol(symbol)
            .into_iter()
            .filter_map(|index| self.references.get(index))
            .filter_map(|reference| {
                self.reference_binding_confidence(symbol, reference)
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
        self.files.insert(file.file.id.clone(), file.file.clone());
        if let Some(package) = &file.package {
            self.packages.insert(file.file.id.clone(), package.clone());
        }
        if file.unsupported.is_some() {
            return;
        }

        let file_symbol = file_symbol(&file.file);
        let file_symbol_id = file_symbol.id.clone();
        self.symbols.insert(file_symbol_id.clone(), file_symbol);

        for symbol in file.symbols {
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
            });
            self.symbols.insert(symbol.id.clone(), symbol);
        }

        self.imports.extend(file.imports.clone());
        self.calls.extend(file.calls.clone());
        self.references.extend(file.references.clone());
        self.body_hits.extend(file.body_hits.clone());
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
                let java_path_refs = java_paths.iter().map(String::as_str).collect::<Vec<_>>();
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
        self.rebuild_import_indexes();

        for symbol in self.symbols.values() {
            self.symbols_by_name
                .entry(symbol.name.clone())
                .or_default()
                .push(symbol.id.clone());
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
            let leaf = last_path_segment(&reference.text);
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
        self.rebuild_import_indexes();

        for symbol in self.symbols.values() {
            self.symbols_by_name
                .entry(symbol.name.clone())
                .or_default()
                .push(symbol.id.clone());
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
    }

    fn rebuild_import_indexes(&mut self) {
        self.imports_by_file.clear();
        self.java_package_by_file.clear();
        for (index, import) in self.imports.iter().enumerate() {
            self.imports_by_file
                .entry(import.file_id.clone())
                .or_default()
                .push(index);
            if import.alias.as_deref() == Some("__java_package__") {
                let segments = path_segments(&import.path);
                if !segments.is_empty() {
                    self.java_package_by_file
                        .insert(import.file_id.clone(), segments);
                }
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
            CandidateSet::Indexes(ids) => ids
                .into_iter()
                .filter_map(|id| self.symbols.get(&id))
                .collect(),
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
                .into_iter()
                .filter_map(|index| {
                    let lower = self.body_hit_text_lower.get(index)?;
                    lower
                        .contains(needle)
                        .then(|| self.body_hits.get(index))
                        .flatten()
                })
                .collect(),
        }
    }
}

enum CandidateSet<T> {
    All,
    None,
    Indexes(Vec<T>),
}

fn rarest_indexed_trigram<T: Clone>(
    needle: &str,
    index: &HashMap<[u8; 3], Vec<T>>,
) -> CandidateSet<T> {
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

    best.map(|candidates| CandidateSet::Indexes(candidates.clone()))
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
pub struct LanguageReport {
    pub c_files: usize,
    pub csharp_files: usize,
    pub cpp_files: usize,
    pub go_files: usize,
    pub java_files: usize,
    pub javascript_files: usize,
    pub jsx_files: usize,
    pub python_files: usize,
    pub rust_files: usize,
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
    store: Option<Arc<SqueezyStore>>,
    store_metadata: Option<GraphStoreMetadata>,
    last_refresh: Instant,
    build_report: GraphBuildReport,
    pending_changed_paths: HashSet<PathBuf>,
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

    /// Open a `GraphManager` that uses its own private [`SqueezyStore`] under
    /// `<workspace_root>/.squeezy/cache` (or `cache_root` when overridden).
    /// Production callers that already hold a shared `Arc<SqueezyStore>`
    /// should use [`open_with_store`] instead: redb forbids two `Database`
    /// handles on the same file, so opening another one here against an
    /// agent-owned store would fail silently and disable graph persistence.
    pub fn open_persistent_with_crawl_options(
        root: impl AsRef<Path>,
        config: RefreshConfig,
        crawl_options: CrawlOptions,
        cache_root: Option<PathBuf>,
    ) -> Result<Self> {
        let root_path = root.as_ref().to_path_buf();
        let store = SqueezyStore::open(&root_path, cache_root.as_deref())
            .ok()
            .map(Arc::new);
        Self::open_with_optional_store(root_path, config, crawl_options, store)
    }

    /// Open a `GraphManager` against an already-open [`SqueezyStore`]. Pass
    /// `None` to disable persistence. Sharing one handle keeps the agent and
    /// the tool registry from racing to acquire redb's exclusive file lock.
    pub fn open_with_store(
        root: impl AsRef<Path>,
        config: RefreshConfig,
        crawl_options: CrawlOptions,
        store: Option<Arc<SqueezyStore>>,
    ) -> Result<Self> {
        Self::open_with_optional_store(root, config, crawl_options, store)
    }

    fn open_with_optional_store(
        root: impl AsRef<Path>,
        config: RefreshConfig,
        crawl_options: CrawlOptions,
        store: Option<Arc<SqueezyStore>>,
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
            pending_changed_paths: HashSet::new(),
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

    pub fn record_changed_path(&mut self, path: impl Into<PathBuf>) {
        self.pending_changed_paths.insert(path.into());
    }

    pub fn record_changed_paths(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        self.pending_changed_paths.extend(paths);
    }

    pub fn refresh_before_query(&mut self) -> Result<RefreshReport> {
        if self.pending_changed_paths.is_empty()
            && self.last_refresh.elapsed() < self.config.idle_refresh_interval
        {
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
        let pending_changed_paths = self.pending_changed_paths.clone();
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
            self.graph.remove_file(file_id);
            graph_batch.remove_partition(file_id);
        }
        for file_id in &unsupported_removed_files {
            self.graph.files.remove(file_id);
            graph_batch.remove_partition(file_id);
        }
        for record in unsupported_changed_records {
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
        } else if metadata_refresh_needed {
            self.graph.rebuild_java_project_facts();
            self.graph.rebuild_dotnet_project_facts();
            self.graph.rebuild_semantic_edges();
            self.graph.rebuild_indexes();
        }
        if let Some(store) = self.store.as_deref()
            && !graph_batch.is_empty()
        {
            let _ = store.apply_graph_batch(&graph_batch);
        }

        self.pending_changed_paths.clear();
        self.last_refresh = Instant::now();
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
    store: Option<&SqueezyStore>,
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
            LanguageKind::Unsupported => report.unsupported_files += 1,
            LanguageKind::Unknown => report.unknown_files += 1,
        }
    }
    report
}

fn line_ranges_intersect(start: u32, end: u32, dirty: DirtyRange) -> bool {
    start <= dirty.end_line && dirty.start_line <= end
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
        signature: file.relative_path.clone(),
        visibility: None,
        docs: Vec::new(),
        attributes: Vec::new(),
        provenance: Provenance::new("squeezy-workspace", "workspace file record"),
        confidence: Confidence::ExactSyntax,
        freshness: file.freshness,
        dirty: None,
    }
}

fn file_symbol_id(file_id: &FileId) -> SymbolId {
    SymbolId::new(format!("file:{}", file_id.0))
}

fn last_path_segment(path: &str) -> String {
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
        .to_string()
}

fn reference_text_matches_symbol(reference: &ParsedReference, symbol: &GraphSymbol) -> bool {
    reference.text == symbol.name || last_path_segment(&reference.text) == symbol.name
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
        || last_path_segment(&reference.text) != symbol.name
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
fn receiver_split(text: &str, delimiter: char) -> Option<String> {
    let (_, rest) = text.rsplit_once(delimiter)?;
    let rest = rest.trim();
    if rest.is_empty() {
        None
    } else {
        Some(rest.to_string())
    }
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

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
