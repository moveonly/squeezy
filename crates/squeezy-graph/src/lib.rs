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
    ParsedSymbol, RustParser, edge_kind_for_call,
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
        let mut indexes = BTreeSet::new();
        if let Some(exact) = self.references_by_text.get(text) {
            indexes.extend(exact.iter().copied());
        }
        let suffix = format!("::{text}");
        if let Some(segment) = self.references_by_text.get(&suffix) {
            indexes.extend(segment.iter().copied());
        }
        let references = if text.contains("::") {
            self.references
                .iter()
                .filter(|reference| reference.text == text || reference.text.ends_with(&suffix))
                .collect::<Vec<_>>()
        } else {
            indexes
                .into_iter()
                .filter_map(|index| self.references.get(index))
                .collect::<Vec<_>>()
        };
        let mut hits = references
            .into_iter()
            .map(|reference| ReferenceHit {
                owner: reference
                    .owner_id
                    .as_ref()
                    .and_then(|id| self.symbols.get(id))
                    .cloned(),
                reference: reference.clone(),
                confidence: Confidence::Heuristic,
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

        if call.kind == ParsedCallKind::Method
            && let Some(callee) = self.same_impl_method(caller_id, &call.name)
        {
            return (Some(callee), Confidence::ExactSyntax);
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
                            SymbolKind::Function | SymbolKind::Method | SymbolKind::Test
                        )
                    })
                    .unwrap_or(false)
            })
            .collect::<Vec<_>>();

        match candidates.as_slice() {
            [only] if call.kind == ParsedCallKind::Direct => {
                (Some(only.clone()), Confidence::ExactSyntax)
            }
            [only] => (Some(only.clone()), Confidence::Heuristic),
            [] => (None, Confidence::External),
            _ => (None, Confidence::CandidateSet),
        }
    }

    fn same_impl_method(&self, caller_id: &SymbolId, method_name: &str) -> Option<SymbolId> {
        let caller = self.symbols.get(caller_id)?;
        let impl_id = if caller.kind == SymbolKind::Impl {
            caller.id.clone()
        } else {
            caller.parent_id.clone()?
        };
        let parent = self.symbols.get(&impl_id)?;
        if parent.kind != SymbolKind::Impl {
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
        .trim()
        .trim_end_matches("::*")
        .to_string()
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
