use std::{
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use squeezy_core::{
    Confidence, EdgeKind, FileId, Freshness, LanguageKind, Provenance, Result, SourceSpan,
    SymbolId, SymbolKind,
};
use squeezy_parse::{
    BodyHit, BodyHitKind, ParsedCall, ParsedCallKind, ParsedFile, ParsedImport, ParsedReference,
    ParsedSymbol, ReferenceKind, RustParser, edge_kind_for_call,
};
use squeezy_workspace::{CrawlOptions, FileRecord, WorkspaceCrawler};

pub const CRATE_NAME: &str = "squeezy-graph";

pub fn crate_name() -> &'static str {
    CRATE_NAME
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphSymbol {
    pub id: SymbolId,
    pub file_id: FileId,
    pub parent_id: Option<SymbolId>,
    pub name: String,
    pub kind: SymbolKind,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirtyAnnotation {
    pub status: String,
    pub ranges: Vec<DirtyRange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirtyRange {
    pub start_line: u32,
    pub end_line: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphStats {
    pub files: usize,
    pub symbols: usize,
    pub edges: usize,
    pub body_hits: usize,
    pub references: usize,
    pub calls: usize,
}

#[derive(Debug, Clone)]
pub struct SemanticGraph {
    pub files: HashMap<FileId, FileRecord>,
    pub symbols: HashMap<SymbolId, GraphSymbol>,
    edges: Vec<GraphEdge>,
    imports: Vec<ParsedImport>,
    calls: Vec<ParsedCall>,
    references: Vec<ParsedReference>,
    body_hits: Vec<BodyHit>,
    symbols_by_name: HashMap<String, Vec<SymbolId>>,
    symbol_signature_lower: HashMap<SymbolId, String>,
    signature_trigram_index: HashMap<[u8; 3], Vec<SymbolId>>,
    body_hit_text_lower: Vec<String>,
    body_hit_trigram_index: HashMap<[u8; 3], Vec<usize>>,
    references_by_text: HashMap<String, Vec<usize>>,
    children_by_parent: HashMap<SymbolId, Vec<SymbolId>>,
    edges_by_from: HashMap<SymbolId, Vec<usize>>,
    edges_by_to: HashMap<SymbolId, Vec<usize>>,
}

impl SemanticGraph {
    pub fn empty() -> Self {
        Self {
            files: HashMap::new(),
            symbols: HashMap::new(),
            edges: Vec::new(),
            imports: Vec::new(),
            calls: Vec::new(),
            references: Vec::new(),
            body_hits: Vec::new(),
            symbols_by_name: HashMap::new(),
            symbol_signature_lower: HashMap::new(),
            signature_trigram_index: HashMap::new(),
            body_hit_text_lower: Vec::new(),
            body_hit_trigram_index: HashMap::new(),
            references_by_text: HashMap::new(),
            children_by_parent: HashMap::new(),
            edges_by_from: HashMap::new(),
            edges_by_to: HashMap::new(),
        }
    }

    pub fn from_parsed(files: Vec<ParsedFile>) -> Self {
        let mut graph = Self::empty();
        for file in files {
            graph.insert_parsed_file(file);
        }
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
        self.rebuild_semantic_edges();
        self.rebuild_indexes();
    }

    pub fn remove_file(&mut self, file_id: &FileId) {
        self.remove_file_data(file_id);
        self.rebuild_semantic_edges();
        self.rebuild_indexes();
    }

    fn remove_file_data(&mut self, file_id: &FileId) {
        self.files.remove(file_id);
        self.symbols.retain(|_, symbol| &symbol.file_id != file_id);
        self.imports.retain(|import| &import.file_id != file_id);
        self.calls.retain(|call| &call.file_id != file_id);
        self.references
            .retain(|reference| &reference.file_id != file_id);
        self.body_hits.retain(|hit| &hit.file_id != file_id);
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
        }
    }

    pub fn edges(&self) -> &[GraphEdge] {
        &self.edges
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

    fn rebuild_semantic_edges(&mut self) {
        self.edges.retain(|edge| {
            edge.kind == EdgeKind::Contains
                && self.symbols.contains_key(&edge.from)
                && edge
                    .to
                    .as_ref()
                    .map(|to| self.symbols.contains_key(to))
                    .unwrap_or(true)
        });
        self.rebuild_indexes();

        let imports = self.imports.clone();
        let calls = self.calls.clone();
        let references = self.references.clone();
        self.add_import_edges(&imports);
        self.add_call_edges(&calls);
        self.add_reference_edges(&references);
    }

    fn add_import_edges(&mut self, imports: &[ParsedImport]) {
        for import in imports {
            let file_symbol_id = file_symbol_id(&import.file_id);
            let from = import
                .owner_id
                .clone()
                .unwrap_or_else(|| file_symbol_id.clone());
            let target_name = import
                .alias
                .clone()
                .unwrap_or_else(|| last_path_segment(&import.path));
            let candidates = self.symbols_by_name_or_scan(&target_name);
            let (to, confidence) = match candidates.as_slice() {
                [only] if !import.is_glob => (Some(only.clone()), Confidence::ImportResolved),
                [] if import.is_glob => (None, Confidence::CandidateSet),
                [] => (None, Confidence::External),
                _ => (None, Confidence::CandidateSet),
            };
            self.edges.push(GraphEdge {
                from,
                to,
                target_text: import.path.clone(),
                kind: if import.is_reexport {
                    EdgeKind::Reexports
                } else {
                    EdgeKind::Imports
                },
                span: Some(import.span),
                confidence,
                freshness: Freshness::Fresh,
                provenance: import.provenance.clone(),
            });
        }
    }

    fn add_call_edges(&mut self, calls: &[ParsedCall]) {
        for call in calls {
            let file_symbol_id = file_symbol_id(&call.file_id);
            let from = call
                .caller_id
                .clone()
                .unwrap_or_else(|| file_symbol_id.clone());
            let (to, confidence, rank_reason) = self.resolve_call(call, &from);
            self.edges.push(GraphEdge {
                from,
                to,
                target_text: call.target_text.clone(),
                kind: edge_kind_for_call(call.kind),
                span: Some(call.span),
                confidence,
                freshness: Freshness::Fresh,
                provenance: Provenance::new(
                    call.provenance.source.clone(),
                    format!("{}; rank={rank_reason}", call.provenance.reason),
                ),
            });
        }
    }

    fn add_reference_edges(&mut self, references: &[ParsedReference]) {
        for reference in references {
            let file_symbol_id = file_symbol_id(&reference.file_id);
            let from = reference
                .owner_id
                .clone()
                .unwrap_or_else(|| file_symbol_id.clone());
            let candidates = self.symbols_by_name_or_scan(&last_path_segment(&reference.text));
            let (to, confidence) = match candidates.as_slice() {
                [only] => (Some(only.clone()), Confidence::Heuristic),
                [] => (None, Confidence::External),
                _ => (None, Confidence::CandidateSet),
            };
            self.edges.push(GraphEdge {
                from,
                to,
                target_text: reference.text.clone(),
                kind: EdgeKind::References,
                span: Some(reference.span),
                confidence,
                freshness: Freshness::Fresh,
                provenance: reference.provenance.clone(),
            });
        }
    }

    fn resolve_call(
        &self,
        call: &ParsedCall,
        caller_id: &SymbolId,
    ) -> (Option<SymbolId>, Confidence, &'static str) {
        if call.kind == ParsedCallKind::Macro {
            let candidates = self
                .symbols_by_name_or_scan(&call.name)
                .into_iter()
                .filter(|id| {
                    self.symbols
                        .get(id)
                        .map(|symbol| symbol.kind == SymbolKind::Macro)
                        .unwrap_or(false)
                })
                .collect::<Vec<_>>();
            return match candidates.as_slice() {
                [only] => (Some(only.clone()), Confidence::ExactSyntax, "macro exact"),
                [] => (None, Confidence::MacroOpaque, "macro opaque"),
                _ => (None, Confidence::CandidateSet, "macro candidate set"),
            };
        }

        if call.kind == ParsedCallKind::Direct
            && let Some(callee) = self.import_alias_direct_call(caller_id, call)
        {
            return (Some(callee), Confidence::ImportResolved, "import alias");
        }

        let is_base_call =
            call.kind == ParsedCallKind::Method && call.receiver.as_deref() == Some("base");

        if call.kind == ParsedCallKind::Method
            && !is_base_call
            && let Some(callee) = self.same_impl_method(caller_id, &call.name)
        {
            return (Some(callee), Confidence::ExactSyntax, "same class or impl");
        }

        if call.kind == ParsedCallKind::Method
            && let Some(callee) = self.inherited_python_method(caller_id, call)
        {
            return (Some(callee), Confidence::Heuristic, "inherited class");
        }

        let candidates = self
            .symbols_by_name_or_scan(&call.name)
            .into_iter()
            .filter(|id| {
                self.symbols
                    .get(id)
                    .map(|symbol| {
                        matches!(
                            symbol.kind,
                            SymbolKind::Class
                                | SymbolKind::Function
                                | SymbolKind::Method
                                | SymbolKind::Test
                        )
                    })
                    .unwrap_or(false)
            })
            .collect::<Vec<_>>();

        if let Some(id) = self.qualified_direct_call(&candidates, caller_id, call) {
            return (Some(id), Confidence::Heuristic, "qualified syntax");
        }

        if call.kind == ParsedCallKind::Method {
            if let Some(id) = self.python_receiver_alias_method(caller_id, call) {
                return (Some(id), Confidence::Heuristic, "constructor alias");
            }
            if let Some(id) = self.python_module_qualified_call(&candidates, caller_id, call) {
                return (Some(id), Confidence::ImportResolved, "imported module");
            }
            return match candidates.as_slice() {
                [] => (None, Confidence::External, "method external"),
                _ => (None, Confidence::CandidateSet, "method candidate set"),
            };
        }

        if call.receiver.is_some() {
            return match candidates.as_slice() {
                [] => (None, Confidence::External, "receiver external"),
                _ => (None, Confidence::CandidateSet, "receiver candidate set"),
            };
        }

        if let Some(id) = self.same_file_direct_call(&candidates, caller_id, call) {
            return (Some(id), Confidence::ExactSyntax, "same file");
        }
        if let Some(id) = self.imported_direct_call(&candidates, caller_id, call) {
            return (Some(id), Confidence::ImportResolved, "explicit import");
        }
        if let Some(id) = self.package_local_direct_call(&candidates, caller_id) {
            return (Some(id), Confidence::Heuristic, "package local");
        }
        match candidates.as_slice() {
            [] => (None, Confidence::External, "external"),
            _ => (None, Confidence::CandidateSet, "candidate set"),
        }
    }

    fn qualified_direct_call(
        &self,
        candidates: &[SymbolId],
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        if call.kind != ParsedCallKind::Direct || !call.target_text.contains("::") {
            return None;
        }
        self.same_impl_qualified_call(candidates, caller_id, call)
            .or_else(|| self.associated_function_call(candidates, call))
            .or_else(|| self.module_qualified_call(candidates, caller_id, call))
    }

    fn same_impl_qualified_call(
        &self,
        candidates: &[SymbolId],
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        if call.receiver.as_deref() != Some("Self") {
            return None;
        }
        let caller = self.symbols.get(caller_id)?;
        let impl_id = if caller.kind == SymbolKind::Impl {
            caller.id.clone()
        } else {
            caller.parent_id.clone()?
        };
        self.symbols
            .get(&impl_id)
            .filter(|symbol| symbol.kind == SymbolKind::Impl)?;
        single_symbol(
            candidates
                .iter()
                .filter_map(|id| self.symbols.get(id))
                .filter(|symbol| symbol.parent_id.as_ref() == Some(&impl_id))
                .map(|symbol| symbol.id.clone()),
        )
    }

    fn associated_function_call(
        &self,
        candidates: &[SymbolId],
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        let receiver = call.receiver.as_deref()?;
        if path_starts_with_external_root(receiver) {
            return None;
        }
        let type_name = last_path_segment(receiver);
        if !type_name
            .chars()
            .next()
            .map(|ch| ch.is_ascii_uppercase())
            .unwrap_or(false)
        {
            return None;
        }

        single_symbol(
            candidates
                .iter()
                .filter_map(|id| self.symbols.get(id))
                .filter(|symbol| {
                    matches!(symbol.kind, SymbolKind::Function | SymbolKind::Method)
                        && symbol
                            .parent_id
                            .as_ref()
                            .and_then(|parent_id| self.symbols.get(parent_id))
                            .map(|parent| {
                                parent.kind == SymbolKind::Impl
                                    && impl_header_matches_type(&parent.name, &type_name)
                            })
                            .unwrap_or(false)
                })
                .map(|symbol| symbol.id.clone()),
        )
    }

    fn module_qualified_call(
        &self,
        candidates: &[SymbolId],
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        let receiver = call.receiver.as_deref()?;
        if path_starts_with_external_root(receiver) {
            return None;
        }
        let caller = self.symbols.get(caller_id)?;
        let receiver_paths = self.receiver_module_paths(caller, receiver);
        if receiver_paths.is_empty() {
            return None;
        }

        single_symbol(
            candidates
                .iter()
                .filter_map(|id| self.symbols.get(id))
                .filter(|symbol| matches!(symbol.kind, SymbolKind::Function | SymbolKind::Test))
                .filter(|symbol| is_free_function_like(symbol))
                .filter(|symbol| {
                    let module_path = self.module_path_for_symbol(symbol);
                    receiver_paths.iter().any(|path| path == &module_path)
                })
                .map(|symbol| symbol.id.clone()),
        )
    }

    fn receiver_module_paths(&self, caller: &GraphSymbol, receiver: &str) -> Vec<Vec<String>> {
        let receiver_segments = path_segments(receiver);
        if receiver_segments.is_empty() {
            return Vec::new();
        }
        let mut paths = BTreeSet::new();
        if receiver_segments.first().map(String::as_str) == Some("crate") {
            paths.insert(receiver_segments.clone());
        }
        if let Some(caller_file) = self.files.get(&caller.file_id) {
            let caller_module = module_path_for_file(&caller_file.relative_path);
            let mut child = caller_module.clone();
            child.extend(receiver_segments.clone());
            if child.first().map(String::as_str) == Some("crate") {
                paths.insert(child);
            }
            let mut relative = caller_module;
            relative.pop();
            relative.extend(receiver_segments.clone());
            if relative.first().map(String::as_str) == Some("crate") {
                paths.insert(relative);
            }
        }
        for import in self
            .imports
            .iter()
            .filter(|import| self.import_visible_from_symbol(import, caller))
        {
            let alias_or_name = import
                .alias
                .clone()
                .unwrap_or_else(|| last_path_segment(&import.path));
            if alias_or_name == receiver_segments[0] {
                let mut import_segments = path_segments(&import.path);
                import_segments.extend(receiver_segments.iter().skip(1).cloned());
                if import_segments.first().map(String::as_str) == Some("crate") {
                    paths.insert(import_segments);
                }
            }
        }
        paths.into_iter().collect()
    }

    fn module_path_for_symbol(&self, symbol: &GraphSymbol) -> Vec<String> {
        let mut path = self
            .files
            .get(&symbol.file_id)
            .map(|file| module_path_for_file(&file.relative_path))
            .unwrap_or_else(|| vec!["crate".to_string()]);
        let mut modules = Vec::new();
        let mut parent_id = symbol.parent_id.as_ref();
        while let Some(id) = parent_id {
            let Some(parent) = self.symbols.get(id) else {
                break;
            };
            if parent.kind == SymbolKind::Module {
                modules.push(parent.name.clone());
            }
            parent_id = parent.parent_id.as_ref();
        }
        modules.reverse();
        path.extend(modules);
        path
    }

    fn same_file_direct_call(
        &self,
        candidates: &[SymbolId],
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        if call.target_text.contains("::") {
            return None;
        }
        let caller = self.symbols.get(caller_id)?;
        let mut same_file = candidates
            .iter()
            .filter_map(|id| self.symbols.get(id))
            .filter(|symbol| {
                symbol.file_id == caller.file_id
                    && matches!(
                        symbol.kind,
                        SymbolKind::Class | SymbolKind::Function | SymbolKind::Test
                    )
            })
            .map(|symbol| symbol.id.clone())
            .collect::<Vec<_>>();
        same_file.sort_by(|left, right| left.0.cmp(&right.0));
        same_file.dedup();
        match same_file.as_slice() {
            [only] => Some(only.clone()),
            _ => None,
        }
    }

    fn imported_direct_call(
        &self,
        candidates: &[SymbolId],
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        if call.receiver.is_some() {
            return None;
        }
        let caller = self.symbols.get(caller_id)?;
        let mut imported_candidates = candidates
            .iter()
            .filter_map(|id| self.symbols.get(id))
            .filter(|symbol| self.symbol_is_imported_as(caller, symbol, &call.name))
            .map(|symbol| symbol.id.clone())
            .collect::<Vec<_>>();
        imported_candidates.sort_by(|left, right| left.0.cmp(&right.0));
        imported_candidates.dedup();
        match imported_candidates.as_slice() {
            [only] => Some(only.clone()),
            _ => None,
        }
    }

    fn import_alias_direct_call(
        &self,
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        if call.kind != ParsedCallKind::Direct || call.receiver.is_some() {
            return None;
        }
        let caller = self.symbols.get(caller_id)?;
        let candidates = self
            .imports
            .iter()
            .filter(|import| self.import_visible_from_symbol(import, caller))
            .filter(|import| import.span.start_byte <= call.span.start_byte)
            .filter(|import| import.alias.as_deref() == Some(call.name.as_str()))
            .flat_map(|import| {
                let target_name = last_path_segment(&import.path);
                self.symbols_by_name_or_scan(&target_name)
                    .into_iter()
                    .filter_map(|id| self.symbols.get(&id))
                    .filter(|symbol| {
                        matches!(
                            symbol.kind,
                            SymbolKind::Class | SymbolKind::Function | SymbolKind::Test
                        ) && self.import_matches_symbol(import, symbol)
                    })
                    .map(|symbol| symbol.id.clone())
                    .collect::<Vec<_>>()
            });
        single_symbol(candidates)
    }

    fn package_local_direct_call(
        &self,
        candidates: &[SymbolId],
        caller_id: &SymbolId,
    ) -> Option<SymbolId> {
        let caller = self.symbols.get(caller_id)?;
        let caller_file = self.files.get(&caller.file_id)?;
        single_symbol(
            candidates
                .iter()
                .filter_map(|id| self.symbols.get(id))
                .filter(|symbol| {
                    self.files
                        .get(&symbol.file_id)
                        .map(|file| {
                            package_key(&file.relative_path)
                                == package_key(&caller_file.relative_path)
                        })
                        .unwrap_or(false)
                })
                .filter(|symbol| {
                    matches!(
                        symbol.kind,
                        SymbolKind::Class | SymbolKind::Function | SymbolKind::Test
                    )
                })
                .map(|symbol| symbol.id.clone()),
        )
    }

    fn symbol_is_imported_as(
        &self,
        caller: &GraphSymbol,
        symbol: &GraphSymbol,
        name: &str,
    ) -> bool {
        self.imports
            .iter()
            .filter(|import| self.import_visible_from_symbol(import, caller))
            .filter(|import| {
                import
                    .alias
                    .as_deref()
                    .map(|alias| alias == name)
                    .unwrap_or_else(|| last_path_segment(&import.path) == name)
            })
            .any(|import| self.import_matches_symbol(import, symbol))
    }

    fn import_matches_symbol(&self, import: &ParsedImport, symbol: &GraphSymbol) -> bool {
        if last_path_segment(&import.path) != symbol.name {
            return false;
        }
        let Some(file) = self.files.get(&symbol.file_id) else {
            return true;
        };
        if file.language != squeezy_core::LanguageKind::Python {
            return true;
        }
        let import_segments = python_path_segments(&import.path);
        if import_segments.len() <= 1 {
            return true;
        }
        let import_module = &import_segments[..import_segments.len() - 1];
        let symbol_module = python_module_path_for_file(&file.relative_path);
        path_segments_suffix_match(import_module, &symbol_module)
    }

    fn import_visible_from_symbol(&self, import: &ParsedImport, caller: &GraphSymbol) -> bool {
        if import.file_id != caller.file_id {
            return false;
        }
        let Some(owner_id) = &import.owner_id else {
            return true;
        };
        owner_id == &caller.id || self.symbol_is_descendant_of(&caller.id, owner_id)
    }

    fn import_visible_from_reference(
        &self,
        import: &ParsedImport,
        reference: &ParsedReference,
    ) -> bool {
        if import.file_id != reference.file_id {
            return false;
        }
        let Some(owner_id) = &import.owner_id else {
            return true;
        };
        reference
            .owner_id
            .as_ref()
            .map(|reference_owner| {
                reference_owner == owner_id
                    || self.symbol_is_descendant_of(reference_owner, owner_id)
            })
            .unwrap_or(false)
    }

    fn symbol_is_descendant_of(&self, child_id: &SymbolId, ancestor_id: &SymbolId) -> bool {
        let mut current = Some(child_id.clone());
        while let Some(id) = current {
            if &id == ancestor_id {
                return true;
            }
            current = self
                .symbols
                .get(&id)
                .and_then(|symbol| symbol.parent_id.clone());
        }
        false
    }

    fn inherited_python_method(&self, caller_id: &SymbolId, call: &ParsedCall) -> Option<SymbolId> {
        // Receivers that imply "look up the inheritance chain":
        //   Python: `self.foo()`, `cls.foo()`
        //   C#:     `this.Foo()`, `base.Foo()`
        // For `base.Foo()` we want to skip the caller's own type and go
        // directly to its bases, since the override on the current type
        // would otherwise shadow the parent definition.
        let receiver = call.receiver.as_deref()?;
        let skip_self = match receiver {
            "self" | "cls" | "this" => false,
            "base" => true,
            _ => return None,
        };
        let class_id = self.python_class_for_caller(caller_id)?;
        if !skip_self && let Some(method) = self.python_method_on_class(&class_id, &call.name) {
            return Some(method);
        }
        self.python_method_in_bases(&class_id, &call.name, 0)
    }

    fn python_receiver_alias_method(
        &self,
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        let receiver = call.receiver.as_deref()?;
        if matches!(receiver, "self" | "cls") {
            return None;
        }
        let caller = self.symbols.get(caller_id)?;
        let class = self.python_class_for_alias(caller, receiver, Some(call.span.start_byte))?;
        self.python_method_on_class(&class.id, &call.name)
    }

    fn python_module_qualified_call(
        &self,
        candidates: &[SymbolId],
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        let receiver = call.receiver.as_deref()?;
        let caller = self.symbols.get(caller_id)?;
        let receiver_paths = self.python_receiver_module_paths(caller, receiver);
        if receiver_paths.is_empty() {
            return None;
        }
        single_symbol(
            candidates
                .iter()
                .filter_map(|id| self.symbols.get(id))
                .filter(|symbol| matches!(symbol.kind, SymbolKind::Function | SymbolKind::Test))
                .filter(|symbol| is_free_function_like(symbol))
                .filter(|symbol| {
                    self.files
                        .get(&symbol.file_id)
                        .map(|file| {
                            let module_path = python_module_path_for_file(&file.relative_path);
                            receiver_paths.iter().any(|path| path == &module_path)
                        })
                        .unwrap_or(false)
                })
                .map(|symbol| symbol.id.clone()),
        )
    }

    fn python_class_for_alias(
        &self,
        caller: &GraphSymbol,
        alias: &str,
        before_byte: Option<u32>,
    ) -> Option<GraphSymbol> {
        self.python_class_for_alias_in_scope(caller, alias, before_byte, 0)
    }

    fn python_class_for_alias_in_scope(
        &self,
        caller: &GraphSymbol,
        alias: &str,
        before_byte: Option<u32>,
        depth: usize,
    ) -> Option<GraphSymbol> {
        if depth > 4 {
            return None;
        }
        let latest = self
            .imports
            .iter()
            .filter(|import| self.import_visible_from_symbol(import, caller))
            .filter(|import| import.alias.as_deref() == Some(alias))
            .filter(|import| {
                before_byte
                    .map(|byte| import.span.start_byte <= byte)
                    .unwrap_or(true)
            })
            .max_by_key(|import| import.span.start_byte)?;
        let target_name = last_path_segment(&latest.path);
        if let Some(class) = single_symbol(
            self.symbols_by_name_or_scan(&target_name)
                .into_iter()
                .filter_map(|id| self.symbols.get(&id))
                .filter(|symbol| {
                    symbol.kind == SymbolKind::Class && self.import_matches_symbol(latest, symbol)
                })
                .map(|symbol| symbol.id.clone()),
        )
        .and_then(|id| self.symbols.get(&id).cloned())
        {
            return Some(class);
        }
        self.python_class_for_alias_in_scope(caller, &target_name, before_byte, depth + 1)
    }

    fn python_receiver_module_paths(
        &self,
        caller: &GraphSymbol,
        receiver: &str,
    ) -> Vec<Vec<String>> {
        let receiver_segments = python_path_segments(receiver);
        if receiver_segments.is_empty() {
            return Vec::new();
        }
        let mut paths = BTreeSet::new();
        for import in self
            .imports
            .iter()
            .filter(|import| self.import_visible_from_symbol(import, caller))
        {
            let import_segments = python_path_segments(&import.path);
            if import.alias.as_deref() == Some(receiver) {
                paths.insert(import_segments);
                continue;
            }
            if import.path == receiver {
                paths.insert(import_segments);
                continue;
            }
            if import_segments
                .first()
                .map(|segment| segment == &receiver_segments[0])
                .unwrap_or(false)
            {
                let mut resolved = import_segments.clone();
                if receiver_segments.len() > 1 {
                    resolved.extend(receiver_segments.iter().skip(1).cloned());
                }
                paths.insert(resolved);
            }
        }
        paths.into_iter().collect()
    }

    fn python_class_for_caller(&self, caller_id: &SymbolId) -> Option<SymbolId> {
        let caller = self.symbols.get(caller_id)?;
        if is_class_like_kind(caller.kind) {
            return Some(caller.id.clone());
        }
        let mut current = caller.parent_id.clone();
        while let Some(id) = current {
            let symbol = self.symbols.get(&id)?;
            if is_class_like_kind(symbol.kind) {
                return Some(symbol.id.clone());
            }
            current = symbol.parent_id.clone();
        }
        None
    }

    fn python_method_in_bases(
        &self,
        class_id: &SymbolId,
        method_name: &str,
        depth: usize,
    ) -> Option<SymbolId> {
        if depth > 8 {
            return None;
        }
        let class = self.symbols.get(class_id)?;
        for base in class
            .attributes
            .iter()
            .filter_map(|attribute| attribute.strip_prefix("base:"))
        {
            let base_ids = self.python_class_candidates_for_name_in_file(&class.file_id, base);
            for base_id in base_ids {
                if let Some(method) = self.python_method_on_class(&base_id, method_name) {
                    return Some(method);
                }
                if let Some(method) = self.python_method_in_bases(&base_id, method_name, depth + 1)
                {
                    return Some(method);
                }
            }
        }
        None
    }

    fn python_class_candidates_for_name_in_file(
        &self,
        file_id: &FileId,
        name: &str,
    ) -> Vec<SymbolId> {
        let direct_name = last_path_segment(name);
        let mut class_ids = self
            .symbols_by_name_or_scan(&direct_name)
            .into_iter()
            .filter_map(|id| self.symbols.get(&id))
            .filter(|symbol| is_class_like_kind(symbol.kind))
            .map(|symbol| symbol.id.clone())
            .collect::<Vec<_>>();

        class_ids.extend(
            self.imports
                .iter()
                .filter(|import| import.file_id == *file_id)
                .filter(|import| import.alias.as_deref() == Some(name))
                .flat_map(|import| {
                    let target_name = last_path_segment(&import.path);
                    self.symbols_by_name_or_scan(&target_name)
                        .into_iter()
                        .filter_map(|id| self.symbols.get(&id))
                        .filter(|symbol| {
                            is_class_like_kind(symbol.kind)
                                && self.import_matches_symbol(import, symbol)
                        })
                        .map(|symbol| symbol.id.clone())
                        .collect::<Vec<_>>()
                }),
        );

        class_ids.sort_by(|left, right| left.0.cmp(&right.0));
        class_ids.dedup();
        class_ids
    }

    fn python_method_on_class(&self, class_id: &SymbolId, method_name: &str) -> Option<SymbolId> {
        single_symbol(
            self.children_by_parent
                .get(class_id)?
                .iter()
                .filter_map(|child_id| self.symbols.get(child_id))
                .filter(|symbol| symbol.kind == SymbolKind::Method && symbol.name == method_name)
                .map(|symbol| symbol.id.clone()),
        )
    }

    fn python_method_on_class_or_bases(
        &self,
        class_id: &SymbolId,
        method_name: &str,
    ) -> Option<SymbolId> {
        self.python_method_on_class(class_id, method_name)
            .or_else(|| self.python_method_in_bases(class_id, method_name, 0))
    }

    fn same_impl_method(&self, caller_id: &SymbolId, method_name: &str) -> Option<SymbolId> {
        let caller = self.symbols.get(caller_id)?;
        let impl_id = if caller.kind == SymbolKind::Impl {
            caller.id.clone()
        } else {
            caller.parent_id.clone()?
        };
        let parent = self.symbols.get(&impl_id)?;
        if !matches!(
            parent.kind,
            // Records / structs / classes / interfaces / impl blocks all
            // declare instance methods that resolve to siblings via
            // `this`/`self`/`Self`. Keeping `Impl` and `Trait` here covers
            // Rust; adding `Struct` covers C# records and C# structs whose
            // siblings need to be reachable for `this.Foo()` resolution.
            SymbolKind::Class | SymbolKind::Impl | SymbolKind::Trait | SymbolKind::Struct
        ) {
            return None;
        }
        self.children_by_parent
            .get(&impl_id)?
            .iter()
            .find(|child_id| {
                self.symbols
                    .get(*child_id)
                    .map(|symbol| symbol.name == method_name)
                    .unwrap_or(false)
            })
            .cloned()
    }

    fn edge_hit(&self, edge_index: usize) -> Option<CallEdgeHit> {
        let edge = self.edges.get(edge_index)?.clone();
        Some(CallEdgeHit {
            caller: self.symbols.get(&edge.from).cloned(),
            callee: edge
                .to
                .as_ref()
                .and_then(|id| self.symbols.get(id))
                .cloned(),
            edge,
        })
    }

    fn reference_hit(&self, reference: &ParsedReference, confidence: Confidence) -> ReferenceHit {
        ReferenceHit {
            owner: reference
                .owner_id
                .as_ref()
                .and_then(|id| self.symbols.get(id))
                .cloned(),
            reference: reference.clone(),
            confidence,
        }
    }

    fn reference_candidate_indexes(&self, text: &str) -> Vec<usize> {
        let mut indexes = BTreeSet::new();
        if let Some(exact) = self.references_by_text.get(text) {
            indexes.extend(exact.iter().copied());
        }
        let suffix = format!("::{text}");
        if let Some(segment) = self.references_by_text.get(&suffix) {
            indexes.extend(segment.iter().copied());
        }
        let dot_suffix = format!(".{text}");
        indexes.extend(
            self.references
                .iter()
                .enumerate()
                .filter(|(_, reference)| reference.text.ends_with(&dot_suffix))
                .map(|(index, _)| index),
        );
        if text.contains("::") {
            indexes.extend(
                self.references
                    .iter()
                    .enumerate()
                    .filter(|(_, reference)| {
                        reference.text == text || reference.text.ends_with(&suffix)
                    })
                    .map(|(index, _)| index),
            );
        }
        indexes.into_iter().collect()
    }

    fn reference_candidate_indexes_for_symbol(&self, symbol: &GraphSymbol) -> Vec<usize> {
        let mut indexes = BTreeSet::new();
        indexes.extend(self.reference_candidate_indexes(&symbol.name));
        for alias in self
            .imports
            .iter()
            .filter(|import| self.import_matches_symbol(import, symbol))
            .filter_map(|import| import.alias.as_deref())
        {
            indexes.extend(self.reference_candidate_indexes(alias));
        }
        indexes.into_iter().collect()
    }

    fn reference_binding_confidence(
        &self,
        symbol: &GraphSymbol,
        reference: &ParsedReference,
    ) -> Option<Confidence> {
        if !reference_text_matches_symbol(reference, symbol)
            && !self.reference_alias_matches_symbol(symbol, reference)
            && !self.reference_qualifier_matches_symbol(symbol, reference)
        {
            return None;
        }
        if path_starts_with_external_root(&reference.text)
            || self.reference_has_external_scope_prefix(reference)
        {
            return None;
        }
        if constructor_reference_can_bind_symbol(reference, symbol)
            && self.reference_has_uppercase_scope_prefix(reference)
        {
            return None;
        }
        if self.reference_is_symbol_declaration(symbol, reference) {
            return None;
        }
        if !self.reference_is_in_symbol_package(symbol, reference) {
            return None;
        }
        if self.python_property_reference_matches(symbol, reference) {
            return Some(Confidence::Heuristic);
        }
        if self.reference_alias_matches_symbol(symbol, reference)
            && reference_kind_can_bind_symbol(reference, symbol)
        {
            return Some(Confidence::ImportResolved);
        }
        if self.reference_is_impl_method_declaration_for_trait(symbol, reference) {
            return Some(Confidence::Heuristic);
        }
        if self.associated_type_reference_matches_symbol(symbol, reference) {
            return Some(Confidence::Heuristic);
        }
        if let Some(edge) = self.call_edge_for_reference(reference) {
            return self.edge_binding_confidence(symbol, edge);
        }
        if self.imported_reference_matches_symbol(symbol, reference) {
            return Some(Confidence::ImportResolved);
        }
        if let Some(edge) = self.semantic_edge_for_reference(reference) {
            if edge.kind == EdgeKind::References
                && !reference_kind_can_bind_symbol(reference, symbol)
            {
                return None;
            }
            if let Some(confidence) = self.edge_binding_confidence(symbol, edge) {
                return Some(confidence);
            }
            if !matches!(edge.kind, EdgeKind::Imports | EdgeKind::Reexports) {
                return None;
            }
        }
        if self.scoped_type_qualifier_matches_symbol(symbol, reference) {
            return Some(Confidence::Heuristic);
        }
        if self.qualified_reference_matches_symbol(symbol, reference) {
            return Some(Confidence::Heuristic);
        }
        if self.can_bind_loose_reference(symbol, reference) {
            return Some(Confidence::Heuristic);
        }
        None
    }

    fn reference_alias_matches_symbol(
        &self,
        symbol: &GraphSymbol,
        reference: &ParsedReference,
    ) -> bool {
        self.imports
            .iter()
            .filter(|import| self.import_visible_from_reference(import, reference))
            .filter(|import| import.alias.as_deref() == Some(reference.text.as_str()))
            .any(|import| self.import_matches_symbol(import, symbol))
    }

    fn python_property_reference_matches(
        &self,
        symbol: &GraphSymbol,
        reference: &ParsedReference,
    ) -> bool {
        if symbol.kind != SymbolKind::Method
            || reference.kind != ReferenceKind::Field
            || !symbol
                .attributes
                .iter()
                .any(|attribute| attribute == "python:property")
            || last_path_segment(&reference.text) != symbol.name
        {
            return false;
        }
        let Some(receiver) = receiver_from_dotted_reference(&reference.text) else {
            return false;
        };
        let Some(owner_id) = &reference.owner_id else {
            return false;
        };
        let Some(owner) = self.symbols.get(owner_id) else {
            return false;
        };
        if matches!(receiver.as_str(), "self" | "cls") {
            return self
                .python_class_for_caller(owner_id)
                .and_then(|class_id| self.python_method_on_class_or_bases(&class_id, &symbol.name))
                .map(|method_id| method_id == symbol.id)
                .unwrap_or(false);
        }
        self.python_class_for_alias(owner, &receiver, Some(reference.span.start_byte))
            .and_then(|class| self.python_method_on_class_or_bases(&class.id, &symbol.name))
            .map(|method_id| method_id == symbol.id)
            .unwrap_or(false)
    }

    fn edge_binding_confidence(
        &self,
        symbol: &GraphSymbol,
        edge: &GraphEdge,
    ) -> Option<Confidence> {
        let to = edge.to.as_ref()?;
        if to == &symbol.id || self.impl_method_implements_trait_method(to, symbol) {
            Some(edge.confidence)
        } else {
            None
        }
    }

    fn reference_is_in_symbol_package(
        &self,
        symbol: &GraphSymbol,
        reference: &ParsedReference,
    ) -> bool {
        let Some(symbol_file) = self.files.get(&symbol.file_id) else {
            return false;
        };
        let Some(reference_file) = self.files.get(&reference.file_id) else {
            return false;
        };
        if matches!(
            (symbol_file.language, reference_file.language),
            (LanguageKind::Python, LanguageKind::Python)
        ) {
            return true;
        }
        package_key(&symbol_file.relative_path) == package_key(&reference_file.relative_path)
    }

    fn imported_reference_matches_symbol(
        &self,
        symbol: &GraphSymbol,
        reference: &ParsedReference,
    ) -> bool {
        let reference_name = last_path_segment(&reference.text);
        if self
            .imports
            .iter()
            .filter(|import| import.file_id == reference.file_id)
            .any(|import| {
                if import.is_glob
                    || !import.span.contains_byte(reference.span.start_byte)
                    || !import.span.contains_byte(reference.span.end_byte)
                {
                    return false;
                }
                let alias_or_name = import
                    .alias
                    .as_deref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| last_path_segment(&import.path));
                // `extract_import` records the whole `use_declaration` span on
                // every flattened import, so multi-item brace groups whose last
                // segments collide (e.g. `use crate::{a::Foo, b::Foo}`) would
                // otherwise let an inner-segment identifier bind to every
                // colliding import. Require an alias-text match, a full-path
                // match on the reference text, or no last-segment collisions
                // within the same import span before allowing the inside-span
                // shortcut.
                let collisions = self
                    .imports
                    .iter()
                    .filter(|other| {
                        other.file_id == import.file_id
                            && other.span == import.span
                            && last_path_segment(&other.path) == symbol.name
                    })
                    .count();
                let alias_match = import.alias.as_deref() == Some(reference.text.as_str());
                let full_path_match = reference.text == import.path;
                (alias_match || full_path_match || collisions <= 1)
                    && (reference_name == symbol.name || reference_name == alias_or_name)
                    && last_path_segment(&import.path) == symbol.name
                    && self.import_module_matches_symbol(import, symbol)
            })
        {
            return true;
        }
        if !reference_kind_can_bind_symbol(reference, symbol) {
            return false;
        }
        self.imports
            .iter()
            .filter(|import| import.file_id == reference.file_id)
            .any(|import| {
                if path_starts_with_external_root(&import.path) {
                    return false;
                }
                let alias_or_name = import
                    .alias
                    .as_deref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| last_path_segment(&import.path));
                if import.is_glob {
                    return reference_name == symbol.name
                        && self.import_module_matches_symbol(import, symbol);
                }
                alias_or_name == reference_name
                    && last_path_segment(&import.path) == symbol.name
                    && self.import_module_matches_symbol(import, symbol)
            })
    }

    fn reference_qualifier_matches_symbol(
        &self,
        symbol: &GraphSymbol,
        reference: &ParsedReference,
    ) -> bool {
        if !matches!(reference.kind, ReferenceKind::Path) || !is_type_like_symbol(symbol.kind) {
            return false;
        }
        path_segments(&reference.text)
            .first()
            .map(|segment| segment == &symbol.name)
            .unwrap_or(false)
    }

    fn scoped_type_qualifier_matches_symbol(
        &self,
        symbol: &GraphSymbol,
        reference: &ParsedReference,
    ) -> bool {
        if !self.reference_qualifier_matches_symbol(symbol, reference) {
            return false;
        }
        if path_starts_with_external_root(&reference.text) {
            return false;
        }
        self.symbol_is_in_reference_scope(symbol, reference)
    }

    fn symbol_is_in_reference_scope(
        &self,
        symbol: &GraphSymbol,
        reference: &ParsedReference,
    ) -> bool {
        if symbol.file_id == reference.file_id {
            return true;
        }
        self.imports
            .iter()
            .filter(|import| import.file_id == reference.file_id)
            .any(|import| {
                let alias_or_name = import
                    .alias
                    .as_deref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| last_path_segment(&import.path));
                alias_or_name == symbol.name
                    && last_path_segment(&import.path) == symbol.name
                    && self.import_module_matches_symbol(import, symbol)
            })
    }

    fn import_module_matches_symbol(&self, import: &ParsedImport, symbol: &GraphSymbol) -> bool {
        let mut import_path = path_segments(&import.path);
        if import.is_glob {
            if import_path.last().map(String::as_str) == Some("*") {
                import_path.pop();
            }
        } else {
            import_path.pop();
        }
        if import_path.is_empty() {
            return false;
        }
        import_path == self.module_path_for_symbol(symbol)
    }

    fn qualified_reference_matches_symbol(
        &self,
        symbol: &GraphSymbol,
        reference: &ParsedReference,
    ) -> bool {
        if !reference.text.contains("::") || !reference_kind_can_bind_symbol(reference, symbol) {
            return false;
        }
        let mut reference_path = path_segments(&reference.text);
        if reference_path.pop().as_deref() != Some(symbol.name.as_str()) {
            return false;
        }
        if reference_path.is_empty() || path_starts_with_external_root(&reference.text) {
            return false;
        }
        if reference_path.first().map(String::as_str) == Some("crate") {
            return reference_path == self.module_path_for_symbol(symbol);
        }
        let Some(owner) = reference
            .owner_id
            .as_ref()
            .and_then(|id| self.symbols.get(id))
        else {
            return false;
        };
        self.receiver_module_paths(owner, &reference_path.join("::"))
            .into_iter()
            .any(|path| path == self.module_path_for_symbol(symbol))
    }

    fn impl_method_implements_trait_method(
        &self,
        impl_method_id: &SymbolId,
        trait_method: &GraphSymbol,
    ) -> bool {
        if !matches!(trait_method.kind, SymbolKind::Function | SymbolKind::Method) {
            return false;
        }
        let Some(trait_symbol) = trait_method
            .parent_id
            .as_ref()
            .and_then(|id| self.symbols.get(id))
            .filter(|symbol| symbol.kind == SymbolKind::Trait)
        else {
            return false;
        };
        let Some(impl_method) = self.symbols.get(impl_method_id) else {
            return false;
        };
        if impl_method.name != trait_method.name
            || !matches!(impl_method.kind, SymbolKind::Function | SymbolKind::Method)
        {
            return false;
        }
        let Some(impl_parent) = impl_method
            .parent_id
            .as_ref()
            .and_then(|id| self.symbols.get(id))
            .filter(|parent| parent.kind == SymbolKind::Impl)
        else {
            return false;
        };
        // Name match alone is ambiguous when two traits in different modules
        // share a name. Require that the impl header's trait path resolves
        // back to the trait we're testing against.
        impl_header_implements_trait(&impl_parent.name, &trait_symbol.name)
            && self.impl_header_trait_resolves_to(&impl_parent.name, trait_symbol, impl_parent)
    }

    fn impl_header_trait_resolves_to(
        &self,
        header: &str,
        trait_symbol: &GraphSymbol,
        impl_anchor: &GraphSymbol,
    ) -> bool {
        let Some((trait_part, _)) = header.split_once(" for ") else {
            return false;
        };
        let trait_part = trait_part
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != ':');
        let segments = path_segments(trait_part);
        if segments.last().map(String::as_str) != Some(trait_symbol.name.as_str()) {
            return false;
        }
        if segments.len() == 1 {
            if trait_symbol.file_id == impl_anchor.file_id {
                return true;
            }
            return self
                .imports
                .iter()
                .filter(|import| import.file_id == impl_anchor.file_id)
                .any(|import| {
                    let alias_or_name = import
                        .alias
                        .as_deref()
                        .map(str::to_string)
                        .unwrap_or_else(|| last_path_segment(&import.path));
                    alias_or_name == trait_symbol.name
                        && self.import_module_matches_symbol(import, trait_symbol)
                });
        }
        if segments.first().map(String::as_str) == Some("crate") {
            let mut expected = self.module_path_for_symbol(trait_symbol);
            expected.push(trait_symbol.name.clone());
            return segments == expected;
        }
        let receiver = segments[..segments.len() - 1].join("::");
        self.receiver_module_paths(impl_anchor, &receiver)
            .into_iter()
            .any(|receiver_path| receiver_path == self.module_path_for_symbol(trait_symbol))
    }

    fn reference_is_impl_method_declaration_for_trait(
        &self,
        trait_method: &GraphSymbol,
        reference: &ParsedReference,
    ) -> bool {
        let Some(owner) = reference
            .owner_id
            .as_ref()
            .and_then(|id| self.symbols.get(id))
        else {
            return false;
        };
        if !self.impl_method_implements_trait_method(&owner.id, trait_method)
            || !self.reference_is_symbol_declaration(owner, reference)
        {
            return false;
        }
        !self.symbol_or_ancestors_have_cfg_attribute(owner)
    }

    fn associated_type_reference_matches_symbol(
        &self,
        symbol: &GraphSymbol,
        reference: &ParsedReference,
    ) -> bool {
        if symbol.kind != SymbolKind::TypeAlias || last_path_segment(&reference.text) != symbol.name
        {
            return false;
        }
        let Some(symbol_owner) = symbol
            .parent_id
            .as_ref()
            .and_then(|id| self.symbols.get(id))
        else {
            return false;
        };
        if symbol_owner.kind != SymbolKind::Trait {
            return false;
        }

        let segments = path_segments(&reference.text);
        if segments.len() < 2 || segments.last() != Some(&symbol.name) {
            return false;
        }
        let qualifier = &segments[..segments.len() - 1];
        if qualifier.len() == 1 && qualifier[0] == "Self" {
            return self
                .reference_owner_trait(reference)
                .map(|owner| owner.id == symbol_owner.id)
                .unwrap_or(false);
        }
        self.trait_path_matches_symbol(qualifier, symbol_owner, reference)
    }

    fn reference_owner_trait(&self, reference: &ParsedReference) -> Option<&GraphSymbol> {
        let mut current = reference
            .owner_id
            .as_ref()
            .and_then(|id| self.symbols.get(id));
        while let Some(symbol) = current {
            match symbol.kind {
                SymbolKind::Trait => return Some(symbol),
                SymbolKind::Impl => return None,
                _ => {
                    current = symbol
                        .parent_id
                        .as_ref()
                        .and_then(|id| self.symbols.get(id));
                }
            }
        }
        None
    }

    fn trait_path_matches_symbol(
        &self,
        path: &[String],
        trait_symbol: &GraphSymbol,
        reference: &ParsedReference,
    ) -> bool {
        if path.last().map(String::as_str) != Some(trait_symbol.name.as_str()) {
            return false;
        }
        if path.len() == 1 {
            return self.symbol_is_in_reference_scope(trait_symbol, reference);
        }
        if path.first().map(String::as_str) == Some("crate") {
            let mut expected = self.module_path_for_symbol(trait_symbol);
            expected.push(trait_symbol.name.clone());
            return path == expected;
        }
        let Some(owner) = reference
            .owner_id
            .as_ref()
            .and_then(|id| self.symbols.get(id))
        else {
            return false;
        };
        let receiver = path[..path.len() - 1].join("::");
        self.receiver_module_paths(owner, &receiver)
            .into_iter()
            .any(|receiver_path| receiver_path == self.module_path_for_symbol(trait_symbol))
    }

    fn symbol_or_ancestors_have_cfg_attribute(&self, symbol: &GraphSymbol) -> bool {
        if has_cfg_attribute(symbol) || self.symbol_has_leading_cfg_attribute(symbol) {
            return true;
        }
        let mut parent_id = symbol.parent_id.as_ref();
        while let Some(id) = parent_id {
            let Some(parent) = self.symbols.get(id) else {
                break;
            };
            if has_cfg_attribute(parent) || self.symbol_has_leading_cfg_attribute(parent) {
                return true;
            }
            parent_id = parent.parent_id.as_ref();
        }
        false
    }

    fn symbol_has_leading_cfg_attribute(&self, symbol: &GraphSymbol) -> bool {
        let Some(file) = self.files.get(&symbol.file_id) else {
            return false;
        };
        let Ok(source) = std::fs::read_to_string(&file.path) else {
            return false;
        };
        let Some(prefix) = source.get(..symbol.span.start_byte as usize) else {
            return false;
        };
        // Walk lines backward, tolerating doc/line comments and multi-line
        // attributes that close with `)]` or `]` on a continuation line.
        let mut in_multiline_attribute = false;
        for line in prefix.lines().rev() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.starts_with("//") {
                continue;
            }
            if trimmed.starts_with("/*") && trimmed.ends_with("*/") {
                continue;
            }
            if in_multiline_attribute {
                if trimmed.starts_with("#[") || trimmed.starts_with("#![") {
                    in_multiline_attribute = false;
                    if attribute_text_is_cfg(trimmed) {
                        return true;
                    }
                }
                continue;
            }
            if trimmed.starts_with("#[") || trimmed.starts_with("#![") {
                if attribute_text_is_cfg(trimmed) {
                    return true;
                }
                continue;
            }
            if trimmed.ends_with(']') {
                in_multiline_attribute = true;
                continue;
            }
            break;
        }
        false
    }

    fn reference_is_symbol_declaration(
        &self,
        symbol: &GraphSymbol,
        reference: &ParsedReference,
    ) -> bool {
        if symbol.file_id != reference.file_id {
            return false;
        }
        let signature_end = symbol
            .body_span
            .map(|span| span.start_byte)
            .unwrap_or(symbol.span.end_byte);
        if !(symbol.span.start_byte <= reference.span.start_byte
            && reference.span.end_byte <= signature_end)
        {
            return false;
        }
        let Some(file) = self.files.get(&symbol.file_id) else {
            return false;
        };
        let Ok(source) = std::fs::read_to_string(&file.path) else {
            return false;
        };
        let Some(signature) = source.get(symbol.span.start_byte as usize..signature_end as usize)
        else {
            return false;
        };
        find_identifier(signature, &symbol.name)
            .map(|offset| reference.span.start_byte == symbol.span.start_byte + offset as u32)
            .unwrap_or(false)
    }

    fn reference_has_external_scope_prefix(&self, reference: &ParsedReference) -> bool {
        if reference.text.contains("::") {
            return false;
        }
        let Some(file) = self.files.get(&reference.file_id) else {
            return false;
        };
        let Ok(source) = std::fs::read_to_string(&file.path) else {
            return false;
        };
        let Some(prefix) = source.get(..reference.span.start_byte as usize) else {
            return false;
        };
        let scope = prefix
            .chars()
            .rev()
            .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_' || *ch == ':')
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        let scope = scope.trim_end_matches("::");
        !scope.is_empty() && path_starts_with_external_root(scope)
    }

    fn reference_has_uppercase_scope_prefix(&self, reference: &ParsedReference) -> bool {
        if reference.text.contains("::") {
            return false;
        }
        let Some(file) = self.files.get(&reference.file_id) else {
            return false;
        };
        let Ok(source) = std::fs::read_to_string(&file.path) else {
            return false;
        };
        let Some(prefix) = source.get(..reference.span.start_byte as usize) else {
            return false;
        };
        let scope = prefix
            .chars()
            .rev()
            .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_' || *ch == ':')
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        let scope = scope.trim_end_matches("::");
        !scope.is_empty()
            && scope
                .rsplit("::")
                .next()
                .unwrap_or_default()
                .chars()
                .next()
                .map(|ch| ch.is_ascii_uppercase())
                .unwrap_or(false)
    }

    fn semantic_edge_for_reference(&self, reference: &ParsedReference) -> Option<&GraphEdge> {
        let from = reference
            .owner_id
            .clone()
            .unwrap_or_else(|| file_symbol_id(&reference.file_id));
        self.edges_by_from
            .get(&from)?
            .iter()
            .filter_map(|index| self.edges.get(*index))
            .filter(|edge| {
                matches!(
                    edge.kind,
                    EdgeKind::References | EdgeKind::Imports | EdgeKind::Reexports
                ) && edge
                    .span
                    .map(|span| span.contains_byte(reference.span.start_byte))
                    .unwrap_or(false)
            })
            .min_by_key(|edge| {
                edge.span
                    .map(|span| span.end_byte.saturating_sub(span.start_byte))
                    .unwrap_or(u32::MAX)
            })
    }

    fn call_edge_for_reference(&self, reference: &ParsedReference) -> Option<&GraphEdge> {
        let from = reference
            .owner_id
            .clone()
            .unwrap_or_else(|| file_symbol_id(&reference.file_id));
        self.edges_by_from
            .get(&from)?
            .iter()
            .filter_map(|index| self.edges.get(*index))
            .filter(|edge| {
                matches!(edge.kind, EdgeKind::Calls | EdgeKind::InvokesMacro)
                    && edge
                        .span
                        .map(|span| {
                            span.contains_byte(reference.span.start_byte)
                                && span.contains_byte(reference.span.end_byte)
                        })
                        .unwrap_or(false)
                    && last_path_segment(&edge.target_text) == last_path_segment(&reference.text)
            })
            .min_by_key(|edge| {
                edge.span
                    .map(|span| span.end_byte.saturating_sub(span.start_byte))
                    .unwrap_or(u32::MAX)
            })
    }

    fn can_bind_loose_reference(&self, symbol: &GraphSymbol, reference: &ParsedReference) -> bool {
        if !reference_kind_can_bind_symbol(reference, symbol) {
            return false;
        }
        let candidates = self.symbols_by_name_or_scan(&symbol.name);
        let same_file_candidates = candidates
            .iter()
            .filter_map(|id| self.symbols.get(id))
            .filter(|candidate| {
                candidate.file_id == reference.file_id
                    && reference_kind_can_bind_symbol(reference, candidate)
            })
            .collect::<Vec<_>>();
        same_file_candidates.len() == 1 && same_file_candidates[0].id == symbol.id
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

        self.body_hit_text_lower = self
            .body_hits
            .iter()
            .map(|hit| hit.text.to_lowercase())
            .collect();
        for (index, lower) in self.body_hit_text_lower.iter().enumerate() {
            for trigram in unique_trigrams(lower) {
                self.body_hit_trigram_index
                    .entry(trigram)
                    .or_default()
                    .push(index);
            }
        }

        for (index, reference) in self.references.iter().enumerate() {
            self.references_by_text
                .entry(reference.text.clone())
                .or_default()
                .push(index);
            self.references_by_text
                .entry(format!("::{}", last_path_segment(&reference.text)))
                .or_default()
                .push(index);
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
    pub duration_ms: u128,
    pub files_seen: usize,
    pub bytes_seen: u64,
    pub bytes_reparsed: u64,
    pub language: LanguageReport,
    pub stats: GraphStats,
    pub skipped_due_to_interval: bool,
    pub budget_exhausted: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LanguageReport {
    pub csharp_files: usize,
    pub python_files: usize,
    pub rust_files: usize,
    pub supported_files: usize,
    pub unsupported_files: usize,
    pub unknown_files: usize,
}

pub struct GraphManager {
    root: PathBuf,
    crawler: WorkspaceCrawler,
    parser: RustParser,
    graph: SemanticGraph,
    config: RefreshConfig,
    last_refresh: Instant,
    build_report: GraphBuildReport,
    pending_changed_paths: HashSet<PathBuf>,
}

impl GraphManager {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_config(root, RefreshConfig::default())
    }

    pub fn open_with_config(root: impl AsRef<Path>, config: RefreshConfig) -> Result<Self> {
        let started = Instant::now();
        let root = root.as_ref().to_path_buf();
        let crawler = WorkspaceCrawler::new(CrawlOptions::default());
        let snapshot = crawler.crawl(&root)?;
        let mut parser = RustParser::new()?;
        let bytes_seen = snapshot.files.iter().map(|file| file.size_bytes).sum();
        let language = language_report(&snapshot.files);
        let (parsed, parse_summary) = parser.parse_records(&snapshot.files)?;
        let graph = SemanticGraph::from_parsed(parsed);
        let build_report = GraphBuildReport {
            duration_ms: started.elapsed().as_millis(),
            files_seen: snapshot.files.len(),
            parsed_files: parse_summary.parsed_files,
            unsupported_files: parse_summary.unsupported_files,
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
                duration_ms: 0,
                files_seen: self.graph.files.len(),
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
                duration_ms: started.elapsed().as_millis(),
                files_seen: self.graph.files.len(),
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
        let bytes_seen = snapshot.files.iter().map(|file| file.size_bytes).sum();
        let language = language_report(&snapshot.files);
        let current = snapshot
            .files
            .iter()
            .map(|record| (record.id.clone(), record.clone()))
            .collect::<HashMap<_, _>>();
        let old_ids = self.graph.files.keys().cloned().collect::<HashSet<_>>();
        let current_ids = current.keys().cloned().collect::<HashSet<_>>();

        let removed_files = old_ids
            .difference(&current_ids)
            .cloned()
            .collect::<Vec<_>>();
        let mut changed_records = current
            .values()
            .filter(|record| {
                self.graph
                    .files
                    .get(&record.id)
                    .map(|old| old.hash != record.hash || old.language != record.language)
                    .unwrap_or(true)
            })
            .cloned()
            .collect::<Vec<_>>();
        changed_records.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));

        let mut reparsed_files = 0;
        let mut bytes_reparsed = 0;
        let mut budget_exhausted = false;
        for file_id in &removed_files {
            self.graph.remove_file(file_id);
        }
        let changed_files = changed_records
            .iter()
            .map(|record| record.id.clone())
            .collect::<Vec<_>>();

        let mut parsed_files = Vec::new();
        for record in changed_records {
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
            self.graph.replace_files(parsed_files);
        }

        self.pending_changed_paths.clear();
        self.last_refresh = Instant::now();
        Ok(RefreshReport {
            refreshed: reparsed_files > 0 || !removed_files.is_empty(),
            changed_files,
            removed_files,
            reparsed_files,
            duration_ms: started.elapsed().as_millis(),
            files_seen,
            bytes_seen,
            bytes_reparsed,
            language,
            stats: self.graph.stats(),
            skipped_due_to_interval: false,
            budget_exhausted,
        })
    }
}

fn language_report<'a>(records: impl IntoIterator<Item = &'a FileRecord>) -> LanguageReport {
    let mut report = LanguageReport::default();
    for record in records {
        match record.language {
            LanguageKind::CSharp => {
                report.csharp_files += 1;
                report.supported_files += 1;
            }
            LanguageKind::Python => {
                report.python_files += 1;
                report.supported_files += 1;
            }
            LanguageKind::Rust => {
                report.rust_files += 1;
                report.supported_files += 1;
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
    let segment = path
        .rsplit("::")
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
        SymbolKind::Struct
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
        SymbolKind::Class | SymbolKind::Struct | SymbolKind::Trait | SymbolKind::Enum
    )
}

fn constructor_reference_can_bind_symbol(
    reference: &ParsedReference,
    symbol: &GraphSymbol,
) -> bool {
    if symbol.kind != SymbolKind::Struct
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

fn path_starts_with_external_root(path: &str) -> bool {
    path.split("::")
        .next()
        .map(str::trim)
        .map(|root| matches!(root, "std" | "core" | "alloc" | "proc_macro"))
        .unwrap_or(false)
}

fn path_segments(path: &str) -> Vec<String> {
    path.split("::")
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

fn python_path_segments(path: &str) -> Vec<String> {
    path.split('.')
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .map(|segment| segment.trim_end_matches(".*").to_string())
        .filter(|segment| !segment.is_empty())
        .collect()
}

fn receiver_from_dotted_reference(path: &str) -> Option<String> {
    path.rsplit_once('.')
        .map(|(receiver, _)| receiver.trim().to_string())
        .filter(|receiver| !receiver.is_empty())
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

fn python_module_path_for_file(path: &str) -> Vec<String> {
    let path = path
        .trim_end_matches(".py")
        .trim_end_matches("/__init__")
        .trim_start_matches("src/");
    path.split('/')
        .filter(|segment| {
            !segment.is_empty()
                && *segment != "__init__"
                && *segment != "tests"
                && *segment != "test"
        })
        .map(ToString::to_string)
        .collect()
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
