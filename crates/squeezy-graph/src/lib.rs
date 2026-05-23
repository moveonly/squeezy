use std::{
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use squeezy_core::{
    Confidence, EdgeKind, FileId, Freshness, Provenance, Result, SourceSpan, SymbolId, SymbolKind,
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
        }
    }
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
            let (to, confidence) = self.resolve_call(call, &from);
            self.edges.push(GraphEdge {
                from,
                to,
                target_text: call.target_text.clone(),
                kind: edge_kind_for_call(call.kind),
                span: Some(call.span),
                confidence,
                freshness: Freshness::Fresh,
                provenance: call.provenance.clone(),
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
    ) -> (Option<SymbolId>, Confidence) {
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
                [only] => (Some(only.clone()), Confidence::ExactSyntax),
                [] => (None, Confidence::MacroOpaque),
                _ => (None, Confidence::CandidateSet),
            };
        }

        if call.kind == ParsedCallKind::Direct
            && let Some(callee) = self.import_alias_direct_call(caller_id, call)
        {
            return (Some(callee), Confidence::ImportResolved);
        }

        if call.kind == ParsedCallKind::Method
            && let Some(callee) = self.same_impl_method(caller_id, &call.name)
        {
            return (Some(callee), Confidence::ExactSyntax);
        }

        if call.kind == ParsedCallKind::Method
            && let Some(callee) = self.inherited_python_method(caller_id, call)
        {
            return (Some(callee), Confidence::Heuristic);
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
            return (Some(id), Confidence::Heuristic);
        }

        if call.kind == ParsedCallKind::Method {
            if let Some(id) = self.python_receiver_alias_method(caller_id, call) {
                return (Some(id), Confidence::Heuristic);
            }
            if let Some(id) = self.python_module_qualified_call(&candidates, caller_id, call) {
                return (Some(id), Confidence::ImportResolved);
            }
            return match candidates.as_slice() {
                [] => (None, Confidence::External),
                _ => (None, Confidence::CandidateSet),
            };
        }

        if call.receiver.is_some() {
            return match candidates.as_slice() {
                [] => (None, Confidence::External),
                _ => (None, Confidence::CandidateSet),
            };
        }

        if let Some(id) = self.same_file_direct_call(&candidates, caller_id, call) {
            return (Some(id), Confidence::ExactSyntax);
        }
        if let Some(id) = self.imported_direct_call(&candidates, caller_id, call) {
            return (Some(id), Confidence::ImportResolved);
        }
        if let Some(id) = self.package_local_direct_call(&candidates, caller_id) {
            return (Some(id), Confidence::Heuristic);
        }
        match candidates.as_slice() {
            [] => (None, Confidence::External),
            _ => (None, Confidence::CandidateSet),
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
                    self.files
                        .get(&symbol.file_id)
                        .map(|file| {
                            let module_path = module_path_for_file(&file.relative_path);
                            receiver_paths.iter().any(|path| path == &module_path)
                        })
                        .unwrap_or(false)
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
            .filter(|import| import.file_id == caller.file_id)
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
            .filter(|import| import.file_id == caller.file_id)
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
            .filter(|import| import.file_id == caller.file_id)
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

    fn inherited_python_method(&self, caller_id: &SymbolId, call: &ParsedCall) -> Option<SymbolId> {
        if !matches!(call.receiver.as_deref(), Some("self") | Some("cls")) {
            return None;
        }
        let class_id = self.python_class_for_caller(caller_id)?;
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
        let class = self.python_class_for_alias(caller, receiver)?;
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

    fn python_class_for_alias(&self, caller: &GraphSymbol, alias: &str) -> Option<GraphSymbol> {
        self.python_class_for_alias_in_file(&caller.file_id, alias, 0)
    }

    fn python_class_for_alias_in_file(
        &self,
        file_id: &FileId,
        alias: &str,
        depth: usize,
    ) -> Option<GraphSymbol> {
        if depth > 4 {
            return None;
        }
        let class_candidates = self
            .imports
            .iter()
            .filter(|import| import.file_id == *file_id)
            .filter(|import| import.alias.as_deref() == Some(alias))
            .flat_map(|import| {
                let target_name = last_path_segment(&import.path);
                self.symbols_by_name_or_scan(&target_name)
                    .into_iter()
                    .filter_map(|id| self.symbols.get(&id))
                    .filter(|symbol| {
                        symbol.kind == SymbolKind::Class
                            && self.import_matches_symbol(import, symbol)
                    })
                    .cloned()
                    .collect::<Vec<_>>()
            });
        if let Some(class) = single_symbol(class_candidates.map(|symbol| symbol.id))
            .and_then(|id| self.symbols.get(&id).cloned())
        {
            return Some(class);
        }
        self.imports
            .iter()
            .filter(|import| import.file_id == *file_id)
            .find(|import| import.alias.as_deref() == Some(alias))
            .and_then(|import| {
                self.python_class_for_alias_in_file(
                    file_id,
                    &last_path_segment(&import.path),
                    depth + 1,
                )
            })
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
            .filter(|import| import.file_id == caller.file_id)
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
        if caller.kind == SymbolKind::Class {
            return Some(caller.id.clone());
        }
        let mut current = caller.parent_id.clone();
        while let Some(id) = current {
            let symbol = self.symbols.get(&id)?;
            if symbol.kind == SymbolKind::Class {
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
            .filter(|symbol| symbol.kind == SymbolKind::Class)
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
                            symbol.kind == SymbolKind::Class
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

    fn same_impl_method(&self, caller_id: &SymbolId, method_name: &str) -> Option<SymbolId> {
        let caller = self.symbols.get(caller_id)?;
        let impl_id = if caller.kind == SymbolKind::Impl {
            caller.id.clone()
        } else {
            caller.parent_id.clone()?
        };
        let parent = self.symbols.get(&impl_id)?;
        if !matches!(parent.kind, SymbolKind::Class | SymbolKind::Impl) {
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
        {
            return None;
        }
        if path_starts_with_external_root(&reference.text)
            || self.reference_has_external_scope_prefix(reference)
        {
            return None;
        }
        if self.reference_is_symbol_declaration(symbol, reference) {
            return None;
        }
        if !self.reference_is_in_symbol_package(symbol, reference) {
            return None;
        }
        if self.reference_alias_matches_symbol(symbol, reference)
            && reference_kind_can_bind_symbol(reference.kind, symbol.kind)
        {
            return Some(Confidence::ImportResolved);
        }
        if let Some(edge) = self.call_edge_for_reference(reference) {
            return edge
                .to
                .as_ref()
                .filter(|to| *to == &symbol.id)
                .map(|_| edge.confidence);
        }
        if let Some(edge) = self.semantic_edge_for_reference(reference) {
            if edge.kind == EdgeKind::References
                && !reference_kind_can_bind_symbol(reference.kind, symbol.kind)
            {
                return None;
            }
            return edge
                .to
                .as_ref()
                .filter(|to| *to == &symbol.id)
                .map(|_| edge.confidence);
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
            .filter(|import| import.file_id == reference.file_id)
            .filter(|import| import.alias.as_deref() == Some(reference.text.as_str()))
            .any(|import| self.import_matches_symbol(import, symbol))
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
        package_key(&symbol_file.relative_path) == package_key(&reference_file.relative_path)
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
        if !reference_kind_can_bind_symbol(reference.kind, symbol.kind) {
            return false;
        }
        let candidates = self.symbols_by_name_or_scan(&symbol.name);
        let same_file_candidates = candidates
            .iter()
            .filter_map(|id| self.symbols.get(id))
            .filter(|candidate| {
                candidate.file_id == reference.file_id
                    && reference_kind_can_bind_symbol(reference.kind, candidate.kind)
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
pub struct RefreshReport {
    pub refreshed: bool,
    pub changed_files: Vec<FileId>,
    pub removed_files: Vec<FileId>,
    pub reparsed_files: usize,
    pub skipped_due_to_interval: bool,
    pub budget_exhausted: bool,
}

pub struct GraphManager {
    root: PathBuf,
    crawler: WorkspaceCrawler,
    parser: RustParser,
    graph: SemanticGraph,
    config: RefreshConfig,
    last_refresh: Instant,
    pending_changed_paths: HashSet<PathBuf>,
}

impl GraphManager {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_config(root, RefreshConfig::default())
    }

    pub fn open_with_config(root: impl AsRef<Path>, config: RefreshConfig) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let crawler = WorkspaceCrawler::new(CrawlOptions::default());
        let snapshot = crawler.crawl(&root)?;
        let mut parser = RustParser::new()?;
        let (parsed, _) = parser.parse_records(&snapshot.files)?;
        let graph = SemanticGraph::from_parsed(parsed);
        Ok(Self {
            root,
            crawler,
            parser,
            graph,
            config,
            last_refresh: Instant::now(),
            pending_changed_paths: HashSet::new(),
        })
    }

    pub fn graph(&self) -> &SemanticGraph {
        &self.graph
    }

    pub fn graph_mut(&mut self) -> &mut SemanticGraph {
        &mut self.graph
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
                skipped_due_to_interval: true,
                budget_exhausted: false,
            });
        }

        let snapshot = self.crawler.crawl(&self.root)?;
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
            skipped_due_to_interval: false,
            budget_exhausted,
        })
    }
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
            file.size_bytes.min(u64::from(u32::MAX)) as u32,
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
    }
}

fn file_symbol_id(file_id: &FileId) -> SymbolId {
    SymbolId::new(format!("file:{}", file_id.0))
}

fn last_path_segment(path: &str) -> String {
    path.rsplit("::")
        .next()
        .unwrap_or(path)
        .rsplit('.')
        .next()
        .unwrap_or(path)
        .trim()
        .trim_end_matches('!')
        .trim_end_matches("::*")
        .to_string()
}

fn reference_text_matches_symbol(reference: &ParsedReference, symbol: &GraphSymbol) -> bool {
    reference.text == symbol.name || last_path_segment(&reference.text) == symbol.name
}

fn reference_kind_can_bind_symbol(kind: ReferenceKind, symbol_kind: SymbolKind) -> bool {
    match symbol_kind {
        SymbolKind::Class
        | SymbolKind::Struct
        | SymbolKind::Enum
        | SymbolKind::Union
        | SymbolKind::Trait
        | SymbolKind::TypeAlias => matches!(kind, ReferenceKind::Type),
        SymbolKind::Const | SymbolKind::Static | SymbolKind::Module => {
            matches!(kind, ReferenceKind::Identifier | ReferenceKind::Path)
        }
        SymbolKind::Macro => false,
        SymbolKind::Function | SymbolKind::Method | SymbolKind::Test => false,
        SymbolKind::Crate
        | SymbolKind::File
        | SymbolKind::Impl
        | SymbolKind::Field
        | SymbolKind::Variant
        | SymbolKind::Unknown => false,
    }
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

fn module_path_for_file(path: &str) -> Vec<String> {
    let mut path = path.trim_end_matches(".rs");
    if let Some(rest) = path.strip_prefix("src/") {
        path = rest;
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
        _ => ".".to_string(),
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
