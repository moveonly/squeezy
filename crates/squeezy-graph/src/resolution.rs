use crate::*;

impl SemanticGraph {
    pub(crate) fn rebuild_semantic_edges(&mut self) {
        self.edges.retain(|edge| {
            edge.kind == EdgeKind::Contains
                && self.symbols.contains_key(&edge.from)
                && edge
                    .to
                    .as_ref()
                    .map(|to| self.symbols.contains_key(to))
                    .unwrap_or(true)
        });
        self.rebuild_resolution_indexes();
        // Incrementally update the JS/TS module-resolution table: configs
        // (tsconfig.json / package.json) whose `ContentHash` matches the
        // cached entry are reused, only changed/added/removed configs are
        // re-parsed. In a TS monorepo a single file save no longer pays
        // the cost of rebuilding the entire workspace path map.
        self.js_ts_resolver.update_from_files(&self.files);
        self.add_csharp_type_edges();
        self.add_php_type_edges();

        // Move-out, mutate, move-back. Each builder iterates a single
        // field's data while writing edges, and the borrow checker won't
        // let us hold an immutable iterator over `self.X` while passing
        // `&mut self` to push edges. The original code cloned all three
        // collections up-front; we use `mem::take` so the clone-equivalent
        // cost is paid only on the data each builder actually iterates.
        //
        // IMPORTANT: `add_call_edges` and `add_reference_edges` use the
        // resolver, which reads `self.imports` (Python alias lookups,
        // C/C++ include-aware lookups, Java import resolution, Rust
        // glob/use resolution). We must restore `self.imports` between
        // phases so the resolver sees it.
        let imports = std::mem::take(&mut self.imports);
        self.add_import_edges(&imports);
        self.imports = imports;

        let calls = std::mem::take(&mut self.calls);
        self.add_call_edges(&calls);
        self.calls = calls;

        let references = std::mem::take(&mut self.references);
        self.add_reference_edges(&references);
        self.references = references;
    }

    pub(crate) fn add_import_edges(&mut self, imports: &[ParsedImport]) {
        for import in imports {
            if import.alias.as_deref() == Some("__java_package__") {
                continue;
            }
            let file_symbol_id = file_symbol_id(&import.file_id);
            let from = import
                .owner_id
                .clone()
                .unwrap_or_else(|| file_symbol_id.clone());
            let target_name = import
                .alias
                .clone()
                .unwrap_or_else(|| last_path_segment(&import.path));
            let mut candidates = self.symbols_by_name_or_scan(&target_name);
            if self
                .files
                .get(&import.file_id)
                .map(|file| file.language == squeezy_core::LanguageKind::Java)
                .unwrap_or(false)
            {
                candidates.retain(|id| {
                    self.symbols
                        .get(id)
                        .map(|symbol| self.java_import_matches_symbol(import, symbol))
                        .unwrap_or(false)
                });
            }
            if self
                .files
                .get(&import.file_id)
                .map(|file| file.language == squeezy_core::LanguageKind::Kotlin)
                .unwrap_or(false)
            {
                candidates.retain(|id| {
                    self.symbols
                        .get(id)
                        .map(|symbol| self.kotlin_import_matches_symbol(import, symbol))
                        .unwrap_or(false)
                });
            }
            if self
                .files
                .get(&import.file_id)
                .map(|file| file.language == squeezy_core::LanguageKind::CSharp)
                .unwrap_or(false)
            {
                candidates.retain(|id| {
                    self.symbols
                        .get(id)
                        .map(|symbol| csharp_import_matches_symbol(import, symbol))
                        .unwrap_or(false)
                });
            }
            if self
                .files
                .get(&import.file_id)
                .map(|file| file.language == squeezy_core::LanguageKind::Php)
                .unwrap_or(false)
            {
                candidates.retain(|id| {
                    self.symbols
                        .get(id)
                        .map(|symbol| {
                            crate::languages::php::php_import_matches_symbol(import, symbol)
                        })
                        .unwrap_or(false)
                });
            }
            let (to, confidence, edge_candidates) = match candidates.as_slice() {
                [only] if !import.is_glob => {
                    (Some(only.clone()), Confidence::ImportResolved, Vec::new())
                }
                [] if import.is_glob => (None, Confidence::CandidateSet, Vec::new()),
                [] => (None, Confidence::External, Vec::new()),
                _ => (
                    None,
                    Confidence::CandidateSet,
                    self.rank_import_candidates(&candidates, &import.file_id)
                        .into_iter()
                        .take(MAX_EDGE_CANDIDATES)
                        .collect(),
                ),
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
                candidates: edge_candidates,
            });
        }
    }

    pub(crate) fn add_call_edges(&mut self, calls: &[ParsedCall]) {
        for call in calls {
            let file_symbol_id = file_symbol_id(&call.file_id);
            let from = call
                .caller_id
                .clone()
                .unwrap_or_else(|| file_symbol_id.clone());
            let (to, confidence, rank_reason, edge_candidates) = self.resolve_call(call, &from);
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
                candidates: edge_candidates,
            });
        }
    }

    pub(crate) fn add_reference_edges(&mut self, references: &[ParsedReference]) {
        for reference in references {
            if self.should_skip_reference_edge(reference) {
                continue;
            }
            let file_symbol_id = file_symbol_id(&reference.file_id);
            let from = reference
                .owner_id
                .clone()
                .unwrap_or_else(|| file_symbol_id.clone());
            let candidates = self.symbols_by_name_or_scan(&last_path_segment(&reference.text));
            let (to, confidence) = match candidates.as_slice() {
                [only] => (Some(only.clone()), Confidence::Heuristic),
                _ => continue,
            };
            if to.is_none() {
                continue;
            }
            self.edges.push(GraphEdge {
                from,
                to,
                target_text: reference.text.clone(),
                kind: EdgeKind::References,
                span: Some(reference.span),
                confidence,
                freshness: Freshness::Fresh,
                provenance: reference.provenance.clone(),
                candidates: Vec::new(),
            });
        }
    }

    pub(crate) fn should_skip_reference_edge(&self, reference: &ParsedReference) -> bool {
        self.files
            .get(&reference.file_id)
            .map(|file| {
                file.language == LanguageKind::Java
                    && matches!(
                        reference.kind,
                        ReferenceKind::Identifier | ReferenceKind::Field
                    )
            })
            .unwrap_or(false)
    }

    pub(crate) fn resolve_call(
        &self,
        call: &ParsedCall,
        caller_id: &SymbolId,
    ) -> (Option<SymbolId>, Confidence, &'static str, Vec<SymbolId>) {
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
                [only] => (
                    Some(only.clone()),
                    Confidence::ExactSyntax,
                    "macro exact",
                    Vec::new(),
                ),
                [] => (None, Confidence::MacroOpaque, "macro opaque", Vec::new()),
                _ => (
                    None,
                    Confidence::CandidateSet,
                    "macro candidate set",
                    self.rank_call_candidates(&candidates, caller_id, call)
                        .into_iter()
                        .take(MAX_EDGE_CANDIDATES)
                        .collect(),
                ),
            };
        }

        if call.kind == ParsedCallKind::Direct
            && let Some(callee) = self.import_alias_direct_call(caller_id, call)
        {
            return (
                Some(callee),
                Confidence::ImportResolved,
                "import alias",
                Vec::new(),
            );
        }

        let is_base_call =
            call.kind == ParsedCallKind::Method && call.receiver.as_deref() == Some("base");

        if call.kind == ParsedCallKind::Method
            && !is_base_call
            && let Some(callee) = self.same_impl_method(caller_id, &call.name)
        {
            return (
                Some(callee),
                Confidence::ExactSyntax,
                "same class or impl",
                Vec::new(),
            );
        }

        if call.kind == ParsedCallKind::Direct
            && call.receiver.is_none()
            && !call.target_text.contains("::")
            && let Some(callee) = self.same_class_direct_call(caller_id, &call.name)
        {
            return (
                Some(callee),
                Confidence::ExactSyntax,
                "same class",
                Vec::new(),
            );
        }

        if call.kind == ParsedCallKind::Method
            && let Some(callee) = self.inherited_python_method(caller_id, call)
        {
            return (
                Some(callee),
                Confidence::Heuristic,
                "inherited class",
                Vec::new(),
            );
        }

        if let Some(callee) = self.inherited_ruby_method(caller_id, call) {
            return (
                Some(callee),
                Confidence::Heuristic,
                "ruby ancestor",
                Vec::new(),
            );
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
            return (
                Some(id),
                Confidence::Heuristic,
                "qualified syntax",
                Vec::new(),
            );
        }

        if call.kind == ParsedCallKind::Method {
            if let Some(id) = self.java_static_imported_method(&candidates, caller_id, call) {
                return (
                    Some(id),
                    Confidence::ImportResolved,
                    "java static import",
                    Vec::new(),
                );
            }
            if let Some(id) = self.java_receiver_field_method(caller_id, call) {
                return (
                    Some(id),
                    Confidence::Heuristic,
                    "java field receiver",
                    Vec::new(),
                );
            }
            if let Some(id) = self.kotlin_extension_function_call(&candidates, caller_id, call) {
                return (
                    Some(id),
                    Confidence::Heuristic,
                    "kotlin extension receiver",
                    Vec::new(),
                );
            }
            if let Some(id) = self.kotlin_companion_member_call(&candidates, caller_id, call) {
                return (
                    Some(id),
                    Confidence::Heuristic,
                    "kotlin companion member",
                    Vec::new(),
                );
            }
            if let Some(id) = self.python_receiver_alias_method(caller_id, call) {
                return (
                    Some(id),
                    Confidence::Heuristic,
                    "constructor alias",
                    Vec::new(),
                );
            }
            if let Some(id) = self.python_module_qualified_call(&candidates, caller_id, call) {
                return (
                    Some(id),
                    Confidence::ImportResolved,
                    "imported module",
                    Vec::new(),
                );
            }
            if let Some(id) = self.go_package_qualified_call(&candidates, caller_id, call) {
                return (
                    Some(id),
                    Confidence::ImportResolved,
                    "go package import",
                    Vec::new(),
                );
            }
            if let Some(id) = self.arity_unique_candidate(&candidates, call) {
                return (Some(id), Confidence::Heuristic, "arity match", Vec::new());
            }
            return match candidates.as_slice() {
                [] => (None, Confidence::External, "method external", Vec::new()),
                _ => (
                    None,
                    Confidence::CandidateSet,
                    "method candidate set",
                    self.rank_call_candidates(&candidates, caller_id, call)
                        .into_iter()
                        .take(MAX_EDGE_CANDIDATES)
                        .collect(),
                ),
            };
        }

        if call.receiver.is_some() {
            if let Some(id) = self.arity_unique_candidate(&candidates, call) {
                return (Some(id), Confidence::Heuristic, "arity match", Vec::new());
            }
            return match candidates.as_slice() {
                [] => (None, Confidence::External, "receiver external", Vec::new()),
                _ => (
                    None,
                    Confidence::CandidateSet,
                    "receiver candidate set",
                    self.rank_call_candidates(&candidates, caller_id, call)
                        .into_iter()
                        .take(MAX_EDGE_CANDIDATES)
                        .collect(),
                ),
            };
        }

        if let Some(id) = self.same_file_direct_call(&candidates, caller_id, call) {
            return (Some(id), Confidence::ExactSyntax, "same file", Vec::new());
        }
        if let Some(id) = self.imported_direct_call(&candidates, caller_id, call) {
            return (
                Some(id),
                Confidence::ImportResolved,
                "explicit import",
                Vec::new(),
            );
        }
        if self.unresolved_js_ts_imported_direct_call(caller_id, call) {
            return match candidates.as_slice() {
                [] => (
                    None,
                    Confidence::External,
                    "unresolved imported symbol",
                    Vec::new(),
                ),
                _ => (
                    None,
                    Confidence::CandidateSet,
                    "unresolved imported symbol candidate set",
                    self.rank_call_candidates(&candidates, caller_id, call)
                        .into_iter()
                        .take(MAX_EDGE_CANDIDATES)
                        .collect(),
                ),
            };
        }
        if let Some(id) = self.c_family_include_direct_call(&candidates, caller_id) {
            return (
                Some(id),
                Confidence::ImportResolved,
                "include directive",
                Vec::new(),
            );
        }
        if let Some(id) = self.package_local_direct_call(&candidates, caller_id) {
            return (Some(id), Confidence::Heuristic, "package local", Vec::new());
        }
        if let Some(id) = self.arity_unique_candidate(&candidates, call) {
            return (Some(id), Confidence::Heuristic, "arity match", Vec::new());
        }
        match candidates.as_slice() {
            [] => (None, Confidence::External, "external", Vec::new()),
            _ => (
                None,
                Confidence::CandidateSet,
                "candidate set",
                self.rank_call_candidates(&candidates, caller_id, call)
                    .into_iter()
                    .take(MAX_EDGE_CANDIDATES)
                    .collect(),
            ),
        }
    }

    /// Item 5: when the candidate name+arity uniquely identifies one of
    /// the candidates, pick it. We only fire when more than one candidate
    /// is in play and exactly one matches the call's fixed-arity count;
    /// otherwise the rule is too eager and would mis-bind variadic/default
    /// arguments. Returns `None` when the call's arity does not fit in a
    /// `u8` (the parsed value is `usize`).
    pub(crate) fn arity_unique_candidate(
        &self,
        candidates: &[SymbolId],
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        if candidates.len() < 2 {
            return None;
        }
        let arity: u8 = call.arity.try_into().ok()?;
        let mut matches = candidates.iter().filter(|id| {
            self.symbols
                .get(id)
                .and_then(|symbol| symbol.arity)
                .map(|symbol_arity| symbol_arity == arity)
                .unwrap_or(false)
        });
        let first = matches.next()?.clone();
        if matches.next().is_some() {
            return None;
        }
        Some(first)
    }

    pub(crate) fn qualified_direct_call(
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

    pub(crate) fn same_impl_qualified_call(
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

    pub(crate) fn associated_function_call(
        &self,
        candidates: &[SymbolId],
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        let receiver = call.receiver.as_deref()?;
        if path_starts_with_external_root(receiver, LanguageKind::Rust) {
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

    pub(crate) fn module_qualified_call(
        &self,
        candidates: &[SymbolId],
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        let receiver = call.receiver.as_deref()?;
        if path_starts_with_external_root(receiver, LanguageKind::Rust) {
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

    pub(crate) fn receiver_module_paths(
        &self,
        caller: &GraphSymbol,
        receiver: &str,
    ) -> Vec<Vec<String>> {
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
            .imports_for_file(&caller.file_id)
            .filter(|import| self.import_visible_from_symbol(import, caller))
            .filter(|import| import.alias.as_deref() != Some("__java_package__"))
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

    pub(crate) fn module_path_for_symbol(&self, symbol: &GraphSymbol) -> Vec<String> {
        let mut path = self
            .files
            .get(&symbol.file_id)
            .map(|file| {
                if file.language == LanguageKind::Java {
                    self.java_package_for_file(&symbol.file_id)
                        .unwrap_or_else(|| module_path_for_file(&file.relative_path))
                } else {
                    module_path_for_file(&file.relative_path)
                }
            })
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

    pub(crate) fn same_file_direct_call(
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

    pub(crate) fn imported_direct_call(
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

    pub(crate) fn import_alias_direct_call(
        &self,
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        if call.kind != ParsedCallKind::Direct || call.receiver.is_some() {
            return None;
        }
        let caller = self.symbols.get(caller_id)?;
        let candidates = self
            .imports_for_file(&caller.file_id)
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
                            SymbolKind::Class
                                | SymbolKind::Function
                                | SymbolKind::Method
                                | SymbolKind::Test
                        ) && self.import_matches_symbol(import, symbol)
                    })
                    .map(|symbol| symbol.id.clone())
                    .collect::<Vec<_>>()
            });
        single_symbol(candidates)
    }

    pub(crate) fn package_local_direct_call(
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
                    if caller_file.language == LanguageKind::Java {
                        return self.java_package_for_file(&symbol.file_id)
                            == self.java_package_for_file(&caller.file_id);
                    }
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
                        SymbolKind::Class
                            | SymbolKind::Function
                            | SymbolKind::Method
                            | SymbolKind::Test
                    )
                })
                .map(|symbol| symbol.id.clone()),
        )
    }

    pub(crate) fn symbol_is_imported_as(
        &self,
        caller: &GraphSymbol,
        symbol: &GraphSymbol,
        name: &str,
    ) -> bool {
        self.imports_for_file(&caller.file_id)
            .filter(|import| self.import_visible_from_symbol(import, caller))
            .filter(|import| !is_package_marker(import))
            .filter(|import| {
                import
                    .alias
                    .as_deref()
                    .map(|alias| alias == name)
                    .unwrap_or_else(|| last_path_segment(&import.path) == name)
            })
            .any(|import| self.import_matches_symbol(import, symbol))
    }

    pub(crate) fn import_matches_symbol(
        &self,
        import: &ParsedImport,
        symbol: &GraphSymbol,
    ) -> bool {
        if is_package_marker(import) {
            return false;
        }
        let Some(file) = self.files.get(&symbol.file_id) else {
            if last_path_segment(&import.path) != symbol.name {
                return false;
            }
            return true;
        };
        if file.language == squeezy_core::LanguageKind::Java {
            return self.java_import_matches_symbol(import, symbol);
        }
        if file.language == squeezy_core::LanguageKind::Kotlin {
            return self.kotlin_import_matches_symbol(import, symbol);
        }
        if is_js_ts_language(file.language) {
            return self.js_ts_import_matches_symbol(import, symbol);
        }
        if last_path_segment(&import.path) != symbol.name {
            return false;
        }
        if file.language != squeezy_core::LanguageKind::Python
            && file.language != squeezy_core::LanguageKind::Go
        {
            return true;
        }
        if file.language == squeezy_core::LanguageKind::Go {
            return self.go_import_matches_symbol(import, symbol);
        }
        let import_segments = python_path_segments(&import.path);
        if import_segments.len() <= 1 {
            return true;
        }
        let import_module = &import_segments[..import_segments.len() - 1];
        let symbol_module = python_module_path_for_file(&file.relative_path);
        path_segments_suffix_match(import_module, &symbol_module)
    }

    pub(crate) fn import_visible_from_symbol(
        &self,
        import: &ParsedImport,
        caller: &GraphSymbol,
    ) -> bool {
        if import.file_id != caller.file_id {
            return false;
        }
        let Some(owner_id) = &import.owner_id else {
            return true;
        };
        owner_id == &caller.id || self.symbol_is_descendant_of(&caller.id, owner_id)
    }

    pub(crate) fn import_visible_from_reference(
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

    pub(crate) fn symbol_is_descendant_of(
        &self,
        child_id: &SymbolId,
        ancestor_id: &SymbolId,
    ) -> bool {
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

    pub(crate) fn same_class_direct_call(
        &self,
        caller_id: &SymbolId,
        method_name: &str,
    ) -> Option<SymbolId> {
        let caller = self.symbols.get(caller_id)?;
        if !matches!(caller.kind, SymbolKind::Method | SymbolKind::Function) {
            return None;
        }
        let parent_id = caller.parent_id.as_ref()?;
        let parent = self.symbols.get(parent_id)?;
        if !matches!(
            parent.kind,
            SymbolKind::Class | SymbolKind::Struct | SymbolKind::Union
        ) {
            return None;
        }
        self.method_on_class_or_partials(parent, method_name, Some(&caller.id))
    }

    pub(crate) fn same_impl_method(
        &self,
        caller_id: &SymbolId,
        method_name: &str,
    ) -> Option<SymbolId> {
        let caller = self.symbols.get(caller_id)?;
        let impl_id = if caller.kind == SymbolKind::Impl {
            caller.id.clone()
        } else {
            caller.parent_id.clone()?
        };
        let parent = self.symbols.get(&impl_id)?;
        if !matches!(
            parent.kind,
            SymbolKind::Class
                | SymbolKind::Impl
                | SymbolKind::Interface
                | SymbolKind::Struct
                | SymbolKind::Trait
        ) {
            return None;
        }
        self.method_on_class_or_partials(parent, method_name, Some(&caller.id))
    }

    pub(crate) fn method_on_class_or_partials(
        &self,
        parent: &GraphSymbol,
        method_name: &str,
        exclude: Option<&SymbolId>,
    ) -> Option<SymbolId> {
        let parent_ids = self
            .symbols
            .values()
            .filter(|symbol| {
                symbol.id == parent.id
                    || (symbol.language_identity.is_some()
                        && symbol.language_identity == parent.language_identity
                        && is_class_like_kind(symbol.kind))
            })
            .map(|symbol| symbol.id.clone())
            .collect::<Vec<_>>();
        single_symbol(parent_ids.iter().flat_map(|parent_id| {
            self.children_by_parent
                .get(parent_id)
                .into_iter()
                .flatten()
                .filter_map(|child_id| self.symbols.get(child_id))
                .filter(move |symbol| {
                    matches!(
                        symbol.kind,
                        SymbolKind::Method | SymbolKind::Function | SymbolKind::Test
                    ) && symbol.name == method_name
                        && exclude.map(|id| id != &symbol.id).unwrap_or(true)
                })
                .map(|symbol| symbol.id.clone())
        }))
    }

    /// Order a `CandidateSet` for a call so the most-likely callee comes first.
    ///
    /// `symbols_by_name_or_scan` returns matches in `HashMap` iteration order,
    /// so without an explicit rank the candidate list (and the truncated
    /// `MAX_EDGE_CANDIDATES` prefix) is non-deterministic across runs. The
    /// rank is a small set of cheap signals that hold across every supported
    /// language:
    ///
    /// 1. Same file as the caller — most calls bind locally even under
    ///    polymorphism.
    /// 2. Same package/crate/directory as the caller.
    /// 3. Receiver qualifier matches the candidate's enclosing impl/class
    ///    header (`obj.do_thing()` with a `Beta` receiver prefers
    ///    `Beta::do_thing` over `Alpha::do_thing`).
    ///
    /// `SymbolId` is the final tiebreaker so the order is fully deterministic.
    pub(crate) fn rank_call_candidates(
        &self,
        candidates: &[SymbolId],
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Vec<SymbolId> {
        let caller_symbol = self.symbols.get(caller_id);
        let caller_file = caller_symbol.map(|symbol| symbol.file_id.clone());
        let caller_package = caller_symbol
            .and_then(|symbol| self.files.get(&symbol.file_id))
            .map(|file| package_key(&file.relative_path));
        let receiver_qualifier = call
            .receiver
            .as_deref()
            .map(last_path_segment)
            .filter(|name| !name.is_empty());

        let mut ranked: Vec<(i32, SymbolId)> = candidates
            .iter()
            .map(|id| {
                let score = self.score_call_candidate(
                    id,
                    caller_file.as_ref(),
                    caller_package.as_deref(),
                    receiver_qualifier.as_deref(),
                );
                (score, id.clone())
            })
            .collect();
        // Higher score first; SymbolId ascending as the deterministic tiebreaker.
        ranked.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.0.cmp(&right.1.0)));
        ranked.into_iter().map(|(_, id)| id).collect()
    }

    /// Same idea as [`Self::rank_call_candidates`] but for import edges. Only
    /// the file/package signals apply — there is no receiver for an import.
    pub(crate) fn rank_import_candidates(
        &self,
        candidates: &[SymbolId],
        importer_file: &FileId,
    ) -> Vec<SymbolId> {
        let importer_package = self
            .files
            .get(importer_file)
            .map(|file| package_key(&file.relative_path));

        let mut ranked: Vec<(i32, SymbolId)> = candidates
            .iter()
            .map(|id| {
                let score = self.score_call_candidate(
                    id,
                    Some(importer_file),
                    importer_package.as_deref(),
                    None,
                );
                (score, id.clone())
            })
            .collect();
        ranked.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.0.cmp(&right.1.0)));
        ranked.into_iter().map(|(_, id)| id).collect()
    }

    fn score_call_candidate(
        &self,
        candidate_id: &SymbolId,
        caller_file: Option<&FileId>,
        caller_package: Option<&str>,
        receiver_qualifier: Option<&str>,
    ) -> i32 {
        let Some(candidate) = self.symbols.get(candidate_id) else {
            return 0;
        };
        let mut score = 0;
        if let Some(file_id) = caller_file
            && &candidate.file_id == file_id
        {
            score += 8;
        }
        if let Some(package) = caller_package
            && let Some(candidate_file) = self.files.get(&candidate.file_id)
            && package_key(&candidate_file.relative_path) == package
        {
            score += 4;
        }
        if let Some(qualifier) = receiver_qualifier
            && let Some(parent_id) = candidate.parent_id.as_ref()
            && let Some(parent) = self.symbols.get(parent_id)
        {
            // Direct match (e.g. parent class/struct/trait named `Beta`).
            if parent.name == qualifier {
                score += 4;
            } else if parent.kind == SymbolKind::Impl
                && impl_header_matches_type(&parent.name, qualifier)
            {
                // Rust impl headers carry the implementing type in their name
                // (e.g. `impl Trait for Concrete`).
                score += 4;
            }
        }
        score
    }
}
