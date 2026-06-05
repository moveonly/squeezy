use crate::*;

/// Per-query memoized file reader. The binding pass inspects the bytes
/// preceding several candidate references, and many candidates share a file.
/// Reading each file at most once per [`SemanticGraph::references_to_symbol`]
/// call collapses what was O(references) blocking `read_to_string` calls into
/// O(distinct files). The cache lives for a single query, so it never serves
/// stale bytes across queries.
#[derive(Default)]
pub(crate) struct SourceCache {
    files: HashMap<FileId, Option<String>>,
    reads: usize,
}

impl SourceCache {
    /// Return the source text for `file_id`, reading it from disk on first
    /// access and reusing the cached copy (or cached failure) afterwards.
    pub(crate) fn source(&mut self, graph: &SemanticGraph, file_id: &FileId) -> Option<&str> {
        let reads = &mut self.reads;
        self.files
            .entry(file_id.clone())
            .or_insert_with(|| {
                *reads += 1;
                let path = &graph.files.get(file_id)?.path;
                std::fs::read_to_string(path).ok()
            })
            .as_deref()
    }

    /// Number of distinct `read_to_string` attempts made so far. Used by the
    /// regression test to prove each referenced file is read at most once.
    #[cfg(test)]
    pub(crate) fn reads(&self) -> usize {
        self.reads
    }
}

impl SemanticGraph {
    pub(crate) fn edge_hit(&self, edge_index: usize) -> Option<CallEdgeHit> {
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

    pub(crate) fn reference_hit(
        &self,
        reference: &ParsedReference,
        confidence: Confidence,
    ) -> ReferenceHit {
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

    pub(crate) fn reference_candidate_indexes(&self, text: &str) -> Vec<usize> {
        let mut indexes = BTreeSet::new();
        if let Some(exact) = self.references_by_text.get(text) {
            indexes.extend(exact.iter().copied());
        }
        let mut suffix = String::with_capacity(text.len() + 2);
        suffix.push_str("::");
        suffix.push_str(text);
        if let Some(segment) = self.references_by_text.get(&suffix) {
            indexes.extend(segment.iter().copied());
        }
        suffix.clear();
        suffix.push('.');
        suffix.push_str(text);
        if let Some(segment) = self.references_by_text.get(&suffix) {
            indexes.extend(segment.iter().copied());
        }
        suffix.clear();
        suffix.push_str("->");
        suffix.push_str(text);
        if let Some(segment) = self.references_by_text.get(&suffix) {
            indexes.extend(segment.iter().copied());
        }
        indexes.into_iter().collect()
    }

    pub(crate) fn reference_candidate_indexes_for_symbol(
        &self,
        symbol: &GraphSymbol,
    ) -> Vec<usize> {
        let mut indexes = BTreeSet::new();
        indexes.extend(self.reference_candidate_indexes(&symbol.name));
        // The `imports_by_alias_target` inverted index keys aliased imports by
        // `last_path_segment(import.path)`. For every non-glob aliased import
        // that resolves to `symbol`, the import's leaf equals `symbol.name`,
        // so a single hash lookup yields the candidate set. Glob aliased
        // imports live in `wildcard_aliased_imports` and are scanned as a
        // separate (small) bucket.
        let by_target = self
            .imports_by_alias_target
            .get(&symbol.name)
            .map(|indexes| indexes.as_slice())
            .unwrap_or(&[]);
        for index in by_target.iter().chain(self.wildcard_aliased_imports.iter()) {
            let Some(import) = self.imports.get(*index) else {
                continue;
            };
            let Some(alias) = import.alias.as_deref() else {
                continue;
            };
            if !self.import_matches_symbol(import, symbol) {
                continue;
            }
            indexes.extend(self.reference_candidate_indexes(alias));
        }
        indexes.into_iter().collect()
    }

    pub(crate) fn reference_binding_confidence(
        &self,
        symbol: &GraphSymbol,
        reference: &ParsedReference,
        sources: &mut SourceCache,
    ) -> Option<Confidence> {
        if !reference_text_matches_symbol(reference, symbol)
            && !self.reference_alias_matches_symbol(symbol, reference)
            && !self.reference_qualifier_matches_symbol(symbol, reference)
        {
            return None;
        }
        if path_starts_with_external_root(&reference.text, self.reference_language(reference))
            || self.reference_has_external_scope_prefix(reference, sources)
        {
            return None;
        }
        if constructor_reference_can_bind_symbol(reference, symbol)
            && self.reference_has_uppercase_scope_prefix(reference, sources)
        {
            return None;
        }
        if self.reference_is_symbol_declaration(symbol, reference, sources) {
            return None;
        }
        // Workspace-cross-crate qualified reference fallback runs
        // BEFORE the package-key gate because that gate rejects every
        // cross-crate reference by default. A reference of the shape
        // `<workspace-crate-alias>::Name` whose target is the unique
        // workspace symbol named `Name` is the standard Rust monorepo
        // cross-crate access pattern (e.g.
        // `impl squeezy_llm::LlmProvider for ...` from another
        // crate); without this fallback `reference_search` silently
        // misses every such call/impl site outside the symbol's own
        // crate.
        if self.workspace_cross_crate_qualified_match(symbol, reference) {
            return Some(Confidence::Heuristic);
        }
        if self.go_cross_package_method_match(symbol, reference) {
            return Some(Confidence::Heuristic);
        }
        if self.c_family_namespace_qualified_callable_match(symbol, reference) {
            return Some(Confidence::Heuristic);
        }
        if !self.reference_is_in_symbol_package(symbol, reference) {
            return None;
        }
        if self.python_property_reference_matches(symbol, reference) {
            return Some(Confidence::Heuristic);
        }
        if self.ruby_property_reference_matches(symbol, reference) {
            return Some(Confidence::Heuristic);
        }
        if self.reference_alias_matches_symbol(symbol, reference)
            && reference_kind_can_bind_symbol(reference, symbol)
        {
            return Some(Confidence::ImportResolved);
        }
        if self.reference_is_impl_method_declaration_for_trait(symbol, reference, sources) {
            return Some(Confidence::Heuristic);
        }
        if self.associated_type_reference_matches_symbol(symbol, reference) {
            return Some(Confidence::Heuristic);
        }
        // Self-crate qualified call check runs BEFORE call/semantic
        // edge resolution because both of those branches short-circuit
        // to None on the unresolved-call path (`edge.to = None`) and
        // on the `Function` rejection inside
        // `reference_kind_can_bind_symbol`. The check is conservative
        // (unique same-crate callable by name) so promoting it doesn't
        // override authoritative resolved-call bindings — those are
        // captured later for non-self-crate paths.
        if self.self_crate_qualified_callable_matches(symbol, reference) {
            return Some(Confidence::Heuristic);
        }
        if let Some(edge) = self.call_edge_for_reference(reference) {
            if let Some(confidence) = self.edge_binding_confidence(symbol, edge) {
                return Some(confidence);
            }
            // A call edge with `to = None` is unresolved (e.g. a C#
            // namespace-qualified static call that `resolve_call`
            // couldn't disambiguate). Fall through so the later
            // `semantic_edge_for_reference` branch — which DOES bind
            // via the `References` edge whose `to` is `Some(symbol.id)` —
            // gets a chance. Only short-circuit when the call edge is
            // authoritatively resolved to a different symbol.
            if edge.to.is_some() {
                return None;
            }
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

    /// Bind references like `<crate-name-in-underscores>::foo` that
    /// originate from inside `crates/<crate-name>/`. Tree-sitter emits
    /// a `ParsedReference` for the qualified path but `Calls`-edge
    /// resolution falls through when the receiver is the current
    /// crate's own name (a common Rust idiom: `mycrate::foo()` from
    /// another module of the same crate, often through a `pub use`
    /// re-export). The default qualified-reference rule rejects
    /// Functions outright via `reference_kind_can_bind_symbol`, so
    /// without this fallback `reference_search` silently misses the
    /// call site.
    ///
    /// Conservatively bound: only fires when the symbol is the
    /// unique workspace candidate of its name living in the same
    /// crate. With ambiguity we bail rather than risk a false bind.
    pub(crate) fn self_crate_qualified_callable_matches(
        &self,
        symbol: &GraphSymbol,
        reference: &ParsedReference,
    ) -> bool {
        if !matches!(
            symbol.kind,
            SymbolKind::Function | SymbolKind::Method | SymbolKind::Test
        ) {
            return false;
        }
        // The reference may be the whole qualified path
        // (`crate_alias::foo`, ReferenceKind::Path) or the bare leaf
        // (`foo`, ReferenceKind::Identifier) that the parser emits
        // alongside it. For the bare leaf, the qualifier lives in the
        // source bytes immediately before the reference span; consult
        // the same helper used by `reference_has_external_scope_prefix`.
        let qualified_first_segment: Option<String> = if reference.text.contains("::") {
            let segments = path_segments(&reference.text);
            if segments.last().map(String::as_str) != Some(symbol.name.as_str())
                || segments.len() < 2
            {
                return false;
            }
            segments.first().cloned()
        } else if reference.text == symbol.name {
            self.reference_source_scope_prefix_first_segment(reference)
        } else {
            return false;
        };
        let Some(first_segment) = qualified_first_segment else {
            return false;
        };
        let Some(reference_file) = self.files.get(&reference.file_id) else {
            return false;
        };
        let Some(crate_alias) =
            crate_underscore_alias_for_relative_path(&reference_file.relative_path)
        else {
            return false;
        };
        if first_segment != crate_alias {
            return false;
        }
        let Some(symbol_file) = self.files.get(&symbol.file_id) else {
            return false;
        };
        if package_key(&symbol_file.relative_path) != package_key(&reference_file.relative_path) {
            return false;
        }
        let mut same_crate_callable_count = 0u32;
        let mut symbol_seen = false;
        for id in self.symbols_by_name_or_scan(&symbol.name) {
            let Some(candidate) = self.symbols.get(&id) else {
                continue;
            };
            if !matches!(
                candidate.kind,
                SymbolKind::Function | SymbolKind::Method | SymbolKind::Test
            ) {
                continue;
            }
            let Some(candidate_file) = self.files.get(&candidate.file_id) else {
                continue;
            };
            if package_key(&candidate_file.relative_path)
                != package_key(&reference_file.relative_path)
            {
                continue;
            }
            same_crate_callable_count += 1;
            if candidate.id == symbol.id {
                symbol_seen = true;
            }
            if same_crate_callable_count > 1 {
                return false;
            }
        }
        symbol_seen && same_crate_callable_count == 1
    }

    /// Bind a `<workspace-crate-alias>::Name` reference from one
    /// workspace crate to a symbol named `Name` that lives in
    /// `crates/<workspace-crate-alias-kebab>/`. Mirrors
    /// [`Self::self_crate_qualified_callable_matches`] but for the
    /// cross-crate direction: a reference of this shape in
    /// `crates/other/` must bind to the symbol in the alias crate
    /// even though `reference_is_in_symbol_package` would otherwise
    /// reject the pair on package-key mismatch.
    ///
    /// Conservatism: requires the symbol to be the unique
    /// workspace-wide candidate of its name within the alias's crate
    /// (Function / Method / Test / Trait / Class / Struct / Enum /
    /// Union / TypeAlias / Const / Static / Module). Ambiguous names
    /// stay unresolved.
    pub(crate) fn workspace_cross_crate_qualified_match(
        &self,
        symbol: &GraphSymbol,
        reference: &ParsedReference,
    ) -> bool {
        if !matches!(
            symbol.kind,
            SymbolKind::Function
                | SymbolKind::Method
                | SymbolKind::Test
                | SymbolKind::Trait
                | SymbolKind::Class
                | SymbolKind::Struct
                | SymbolKind::Enum
                | SymbolKind::Union
                | SymbolKind::TypeAlias
                | SymbolKind::Const
                | SymbolKind::Static
                | SymbolKind::Module
        ) {
            return false;
        }
        // Pull the qualifying first segment from one of three places,
        // in order of confidence:
        //  1. The reference text itself (full `crate::Name` path).
        //  2. The source-byte scope prefix that immediately precedes
        //     the reference (covers the bare-leaf reference the
        //     parser emits alongside a scoped_identifier).
        //  3. A non-glob `use <crate>::Name` or `use <crate>::Name as
        //     <alias>` import that brings the symbol into scope as
        //     either `Name` or the alias the reference uses. This
        //     covers the very common bare call (`estimate_cost(...)`)
        //     after `use squeezy_llm::estimate_cost;`.
        let qualified_first_segment: Option<String> = if reference.text.contains("::") {
            let segments = path_segments(&reference.text);
            if segments.last().map(String::as_str) != Some(symbol.name.as_str())
                || segments.len() < 2
            {
                return false;
            }
            segments.first().cloned()
        } else if reference.text == symbol.name {
            self.reference_source_scope_prefix_first_segment(reference)
                .or_else(|| self.import_root_for_workspace_reference(symbol, reference))
        } else {
            // The reference text might be an alias (`use crate::foo as
            // bar; bar()`). Check imports for `bar` mapping to the
            // symbol via a workspace crate alias.
            self.import_root_for_workspace_reference(symbol, reference)
        };
        let Some(first_segment) = qualified_first_segment else {
            return false;
        };
        let Some(symbol_file) = self.files.get(&symbol.file_id) else {
            return false;
        };
        let Some(symbol_crate_alias) =
            crate_underscore_alias_for_relative_path(&symbol_file.relative_path)
        else {
            return false;
        };
        if first_segment != symbol_crate_alias {
            return false;
        }
        // Self-crate path is already handled by
        // `self_crate_qualified_callable_matches`; bail here so the
        // two helpers don't overlap.
        let Some(reference_file) = self.files.get(&reference.file_id) else {
            return false;
        };
        if package_key(&symbol_file.relative_path) == package_key(&reference_file.relative_path) {
            return false;
        }
        let symbol_crate_key = package_key(&symbol_file.relative_path);
        let mut candidates_in_symbol_crate = 0u32;
        let mut symbol_seen = false;
        for id in self.symbols_by_name_or_scan(&symbol.name) {
            let Some(candidate) = self.symbols.get(&id) else {
                continue;
            };
            if !matches!(
                candidate.kind,
                SymbolKind::Function
                    | SymbolKind::Method
                    | SymbolKind::Test
                    | SymbolKind::Trait
                    | SymbolKind::Class
                    | SymbolKind::Struct
                    | SymbolKind::Enum
                    | SymbolKind::Union
                    | SymbolKind::TypeAlias
                    | SymbolKind::Const
                    | SymbolKind::Static
                    | SymbolKind::Module
            ) {
                continue;
            }
            let Some(candidate_file) = self.files.get(&candidate.file_id) else {
                continue;
            };
            if package_key(&candidate_file.relative_path) != symbol_crate_key {
                continue;
            }
            candidates_in_symbol_crate += 1;
            if candidate.id == symbol.id {
                symbol_seen = true;
            }
            if candidates_in_symbol_crate > 1 {
                return false;
            }
        }
        symbol_seen && candidates_in_symbol_crate == 1
    }

    /// Bind Go cross-package method references like `cmd.VisitParents(...)`
    /// from a sibling Go package that imports the symbol's package.
    /// Tree-sitter emits a `Field`-kind reference with text equal to
    /// the method name; the receiver `cmd` is a variable typed by
    /// another package, which Squeezy doesn't track type-by-type.
    /// This helper accepts the binding when:
    ///   1. both files are Go,
    ///   2. the reference text matches the symbol name,
    ///   3. the reference's file imports a path whose leaf (or
    ///      alias) equals the symbol's package name, and
    ///   4. the symbol is the unique callable of its name in that
    ///      package.
    ///
    /// Mirrors the Rust [`Self::workspace_cross_crate_qualified_match`]
    /// but for Go's package + import shape.
    pub(crate) fn go_cross_package_method_match(
        &self,
        symbol: &GraphSymbol,
        reference: &ParsedReference,
    ) -> bool {
        if !matches!(
            symbol.kind,
            SymbolKind::Function | SymbolKind::Method | SymbolKind::Test,
        ) {
            return false;
        }
        let Some(symbol_file) = self.files.get(&symbol.file_id) else {
            return false;
        };
        let Some(reference_file) = self.files.get(&reference.file_id) else {
            return false;
        };
        if symbol_file.language != LanguageKind::Go || reference_file.language != LanguageKind::Go {
            return false;
        }
        // Same-package references already pass `reference_is_in_symbol_package`;
        // this helper only owns the cross-package case.
        if self.packages.get(&symbol.file_id) == self.packages.get(&reference.file_id) {
            return false;
        }
        if reference.text != symbol.name && last_path_segment(&reference.text) != symbol.name {
            return false;
        }
        let Some(symbol_package) = self.packages.get(&symbol.file_id).cloned() else {
            return false;
        };
        // Reference's file must import the symbol's package by name or
        // alias.
        let import_visible = self.imports_for_file(&reference.file_id).any(|import| {
            let import_leaf = import
                .alias
                .as_deref()
                .filter(|alias| *alias != "_")
                .map(str::to_string)
                .unwrap_or_else(|| last_path_segment(&import.path));
            import_leaf == symbol_package || last_path_segment(&import.path) == symbol_package
        });
        if !import_visible {
            return false;
        }
        // Symbol must be the unique callable by name within its package.
        let mut count = 0u32;
        let mut symbol_seen = false;
        for id in self.symbols_by_name_or_scan(&symbol.name) {
            let Some(candidate) = self.symbols.get(&id) else {
                continue;
            };
            if !matches!(
                candidate.kind,
                SymbolKind::Function | SymbolKind::Method | SymbolKind::Test,
            ) {
                continue;
            }
            let Some(candidate_file) = self.files.get(&candidate.file_id) else {
                continue;
            };
            if candidate_file.language != LanguageKind::Go {
                continue;
            }
            if self.packages.get(&candidate.file_id).cloned().as_deref()
                != Some(symbol_package.as_str())
            {
                continue;
            }
            count += 1;
            if candidate.id == symbol.id {
                symbol_seen = true;
            }
            if count > 1 {
                return false;
            }
        }
        symbol_seen && count == 1
    }

    /// Bind C/C++ namespace-qualified function calls like
    /// `details::os::localtime(...)`. The reference text after the
    /// tree-sitter `Path`-kind reduction is just `localtime`; the
    /// scope qualifier `details::os` sits in the source bytes
    /// preceding the reference span. Tail-match those scope segments
    /// against the symbol's enclosing-module chain (Squeezy stores
    /// C++ namespaces as `SymbolKind::Module` ancestors) and bind
    /// when the symbol is the unique callable of its name under that
    /// namespace.
    ///
    /// Mirrors [`Self::self_crate_qualified_callable_matches`] but
    /// keys off the namespace chain rather than the Rust crate
    /// underscore alias.
    pub(crate) fn c_family_namespace_qualified_callable_match(
        &self,
        symbol: &GraphSymbol,
        reference: &ParsedReference,
    ) -> bool {
        if !matches!(
            symbol.kind,
            SymbolKind::Function | SymbolKind::Method | SymbolKind::Test,
        ) {
            return false;
        }
        let Some(reference_file) = self.files.get(&reference.file_id) else {
            return false;
        };
        let Some(symbol_file) = self.files.get(&symbol.file_id) else {
            return false;
        };
        if !matches!(reference_file.language, LanguageKind::C | LanguageKind::Cpp) {
            return false;
        }
        if !matches!(symbol_file.language, LanguageKind::C | LanguageKind::Cpp) {
            return false;
        }
        // Reference text must match the symbol's bare name (or its
        // leaf when the parser produced a longer path).
        if reference.text != symbol.name && last_path_segment(&reference.text) != symbol.name {
            return false;
        }
        // Scope qualifier — what `details::os::` would look like
        // immediately before the reference start in the source.
        let Some(scope_prefix) = self.reference_source_scope_prefix(reference) else {
            return false;
        };
        let scope_segments: Vec<String> = scope_prefix
            .split("::")
            .filter(|segment| !segment.is_empty())
            .map(str::to_string)
            .collect();
        if scope_segments.is_empty() {
            return false;
        }
        // Symbol's enclosing namespace chain (innermost last).
        let symbol_namespaces = self.namespace_chain_for_symbol(symbol);
        if symbol_namespaces.is_empty() {
            return false;
        }
        // The scope tail must equal a suffix of the symbol's
        // namespace chain.
        if scope_segments.len() > symbol_namespaces.len() {
            return false;
        }
        let tail_start = symbol_namespaces.len() - scope_segments.len();
        if symbol_namespaces[tail_start..] != scope_segments[..] {
            return false;
        }
        // Conservatism: the symbol must be the unique workspace
        // candidate of its name living under the same namespace tail.
        let mut count = 0u32;
        let mut symbol_seen = false;
        for id in self.symbols_by_name_or_scan(&symbol.name) {
            let Some(candidate) = self.symbols.get(&id) else {
                continue;
            };
            if !matches!(
                candidate.kind,
                SymbolKind::Function | SymbolKind::Method | SymbolKind::Test,
            ) {
                continue;
            }
            let Some(candidate_file) = self.files.get(&candidate.file_id) else {
                continue;
            };
            if !matches!(candidate_file.language, LanguageKind::C | LanguageKind::Cpp) {
                continue;
            }
            let candidate_ns = self.namespace_chain_for_symbol(candidate);
            if candidate_ns.len() < scope_segments.len() {
                continue;
            }
            let candidate_tail_start = candidate_ns.len() - scope_segments.len();
            if candidate_ns[candidate_tail_start..] != scope_segments[..] {
                continue;
            }
            count += 1;
            if candidate.id == symbol.id {
                symbol_seen = true;
            }
            if count > 1 {
                return false;
            }
        }
        symbol_seen && count == 1
    }

    /// Walk a symbol's parent chain collecting consecutive
    /// `SymbolKind::Module` parents. Outermost namespace first,
    /// innermost last.
    fn namespace_chain_for_symbol(&self, symbol: &GraphSymbol) -> Vec<String> {
        let mut chain = Vec::new();
        let mut current = symbol.parent_id.clone();
        while let Some(id) = current {
            let Some(parent) = self.symbols.get(&id) else {
                break;
            };
            if parent.kind != SymbolKind::Module {
                break;
            }
            chain.push(parent.name.clone());
            current = parent.parent_id.clone();
        }
        chain.reverse();
        chain
    }

    /// Read the raw `::`-delimited scope qualifier sitting in the
    /// source bytes immediately before `reference`. Returns the
    /// qualifier with trailing `::` stripped, or `None` when no scope
    /// is present. The qualifier may contain multiple `::` segments
    /// (e.g. `details::os` for `details::os::localtime`).
    fn reference_source_scope_prefix(&self, reference: &ParsedReference) -> Option<String> {
        let file = self.files.get(&reference.file_id)?;
        let source = std::fs::read_to_string(&file.path).ok()?;
        // First try the source bytes COVERED BY the reference span.
        // Some parsers (e.g. tree-sitter-cpp for `scoped_identifier`)
        // keep the full qualified path in the span while reducing
        // `reference.text` to just the leaf identifier; the qualifier
        // then lives inside the span, not before it.
        if let Some(span_text) =
            source.get(reference.span.start_byte as usize..reference.span.end_byte as usize)
            && span_text.contains("::")
            && let Some((qualifier, _)) = span_text.trim().rsplit_once("::")
        {
            let qualifier = qualifier.trim();
            if !qualifier.is_empty() {
                return Some(qualifier.to_string());
            }
        }
        // Fallback: walk backward from the reference start byte and
        // pick up an identifier/`::` chain immediately preceding it.
        // Covers the `Foo.Bar` (dot-style) and Rust `crate::Bar`
        // qualified shapes.
        let prefix = source.get(..reference.span.start_byte as usize)?;
        let scope = prefix
            .chars()
            .rev()
            .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_' || *ch == ':')
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        let scope = scope.trim_end_matches("::");
        if scope.is_empty() {
            return None;
        }
        Some(scope.to_string())
    }

    /// For a bare-identifier reference whose enclosing file contains
    /// an explicit `use <crate>::Name [as <alias>]` import that names
    /// `symbol`, return the workspace crate's first path segment.
    /// `None` when no such import is present.
    fn import_root_for_workspace_reference(
        &self,
        symbol: &GraphSymbol,
        reference: &ParsedReference,
    ) -> Option<String> {
        for import in self.imports_for_file(&reference.file_id) {
            if import.is_glob || import.alias.as_deref() == Some("__java_package__") {
                continue;
            }
            // The leaf of the import path must name the symbol.
            if last_path_segment(&import.path) != symbol.name {
                continue;
            }
            // The reference text must either be the symbol name
            // (default) or the alias.
            let expected_name = import.alias.clone().unwrap_or_else(|| symbol.name.clone());
            if reference.text != expected_name {
                continue;
            }
            // First path segment is the workspace crate alias.
            let mut segments = path_segments(&import.path);
            if segments.len() < 2 {
                continue;
            }
            return Some(segments.swap_remove(0));
        }
        None
    }

    /// Read the source-byte scope prefix that immediately precedes a
    /// bare-identifier reference and return its first path segment.
    /// Returns `None` when no scope prefix is present or the scope can
    /// not be read.
    fn reference_source_scope_prefix_first_segment(
        &self,
        reference: &ParsedReference,
    ) -> Option<String> {
        let file = self.files.get(&reference.file_id)?;
        let source = std::fs::read_to_string(&file.path).ok()?;
        let prefix = source.get(..reference.span.start_byte as usize)?;
        let scope = prefix
            .chars()
            .rev()
            .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_' || *ch == ':')
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        let scope = scope.trim_end_matches("::");
        if scope.is_empty() {
            return None;
        }
        scope.split("::").next().map(str::to_string)
    }

    pub(crate) fn reference_alias_matches_symbol(
        &self,
        symbol: &GraphSymbol,
        reference: &ParsedReference,
    ) -> bool {
        self.imports_for_file(&reference.file_id)
            .filter(|import| self.import_visible_from_reference(import, reference))
            .filter(|import| !is_package_marker(import))
            .filter(|import| import.alias.as_deref() == Some(reference.text.as_str()))
            .any(|import| self.import_matches_symbol(import, symbol))
    }

    pub(crate) fn edge_binding_confidence(
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

    pub(crate) fn reference_language(&self, reference: &ParsedReference) -> LanguageKind {
        self.files
            .get(&reference.file_id)
            .map(|file| file.language)
            .unwrap_or(LanguageKind::Unknown)
    }

    pub(crate) fn reference_is_in_symbol_package(
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
        if matches!(
            (symbol_file.language, reference_file.language),
            (LanguageKind::Java, LanguageKind::Java)
        ) {
            return self.java_package_for_file(&symbol.file_id)
                == self.java_package_for_file(&reference.file_id)
                || self.imported_reference_matches_symbol(symbol, reference);
        }
        if matches!(
            (symbol_file.language, reference_file.language),
            (LanguageKind::Go, LanguageKind::Go)
        ) {
            return self.packages.get(&symbol.file_id) == self.packages.get(&reference.file_id)
                && package_key(&symbol_file.relative_path)
                    == package_key(&reference_file.relative_path);
        }
        package_key(&symbol_file.relative_path) == package_key(&reference_file.relative_path)
    }

    pub(crate) fn imported_reference_matches_symbol(
        &self,
        symbol: &GraphSymbol,
        reference: &ParsedReference,
    ) -> bool {
        let reference_name = last_path_segment_str(&reference.text);
        if self
            .imports_for_file(&reference.file_id)
            .filter(|import| !crate::is_package_marker_alias(import.alias.as_deref()))
            .any(|import| {
                // Half-open span containment: the reference must lie fully
                // inside the import span. A reference whose `end_byte` merely
                // touches the import's start boundary is not contained.
                if import.is_glob || !import.span.contains_span(reference.span) {
                    return false;
                }
                let alias_or_name = import
                    .alias
                    .as_deref()
                    .unwrap_or_else(|| last_path_segment_str(&import.path));
                let collisions = self
                    .imports_for_file(&import.file_id)
                    .filter(|other| {
                        other.span == import.span
                            && last_path_segment_str(&other.path) == symbol.name.as_str()
                    })
                    .count();
                let alias_match = import.alias.as_deref() == Some(reference.text.as_str());
                let full_path_match = reference.text == import.path;
                (alias_match || full_path_match || collisions <= 1)
                    && (reference_name == symbol.name.as_str() || reference_name == alias_or_name)
                    && last_path_segment_str(&import.path) == symbol.name.as_str()
                    && self.import_module_matches_symbol(import, symbol)
            })
        {
            return true;
        }
        if !reference_kind_can_bind_symbol(reference, symbol) {
            return false;
        }
        self.imports_for_file(&reference.file_id).any(|import| {
            if crate::is_package_marker_alias(import.alias.as_deref()) {
                return false;
            }
            if path_starts_with_external_root(&import.path, self.reference_language(reference)) {
                return false;
            }
            let alias_or_name = import
                .alias
                .as_deref()
                .unwrap_or_else(|| last_path_segment_str(&import.path));
            if import.is_glob {
                return reference_name == symbol.name.as_str()
                    && self.import_module_matches_symbol(import, symbol);
            }
            alias_or_name == reference_name
                && last_path_segment_str(&import.path) == symbol.name.as_str()
                && self.import_module_matches_symbol(import, symbol)
        })
    }

    pub(crate) fn reference_qualifier_matches_symbol(
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

    pub(crate) fn scoped_type_qualifier_matches_symbol(
        &self,
        symbol: &GraphSymbol,
        reference: &ParsedReference,
    ) -> bool {
        if !self.reference_qualifier_matches_symbol(symbol, reference) {
            return false;
        }
        if path_starts_with_external_root(&reference.text, self.reference_language(reference)) {
            return false;
        }
        self.symbol_is_in_reference_scope(symbol, reference)
    }

    pub(crate) fn symbol_is_in_reference_scope(
        &self,
        symbol: &GraphSymbol,
        reference: &ParsedReference,
    ) -> bool {
        if symbol.file_id == reference.file_id {
            return true;
        }
        self.imports_for_file(&reference.file_id)
            .filter(|import| !crate::is_package_marker_alias(import.alias.as_deref()))
            .any(|import| {
                let alias_or_name = import
                    .alias
                    .as_deref()
                    .unwrap_or_else(|| last_path_segment_str(&import.path));
                alias_or_name == symbol.name.as_str()
                    && last_path_segment_str(&import.path) == symbol.name.as_str()
                    && self.import_module_matches_symbol(import, symbol)
            })
    }

    pub(crate) fn import_module_matches_symbol(
        &self,
        import: &ParsedImport,
        symbol: &GraphSymbol,
    ) -> bool {
        let symbol_language = self.files.get(&symbol.file_id).map(|file| file.language);
        if symbol_language == Some(squeezy_core::LanguageKind::Java) {
            return self.java_import_matches_symbol(import, symbol);
        }
        if symbol_language == Some(squeezy_core::LanguageKind::Kotlin) {
            return self.kotlin_import_matches_symbol(import, symbol);
        }
        if symbol_language == Some(squeezy_core::LanguageKind::Scala) {
            return self.scala_import_matches_symbol(import, symbol);
        }
        if symbol_language.map(is_js_ts_language).unwrap_or(false) {
            return self.js_ts_import_matches_symbol(import, symbol);
        }
        if symbol_language == Some(squeezy_core::LanguageKind::Swift) {
            return self.swift_import_matches_symbol(import, symbol);
        }
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

    pub(crate) fn qualified_reference_matches_symbol(
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
        if reference_path.is_empty()
            || path_starts_with_external_root(&reference.text, self.reference_language(reference))
        {
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
        let symbol_module_path = self.module_path_for_symbol(symbol);
        self.receiver_module_paths(owner, &reference_path.join("::"))
            .into_iter()
            .any(|path| path == symbol_module_path)
    }

    pub(crate) fn impl_method_implements_trait_method(
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
        impl_header_implements_trait(&impl_parent.name, &trait_symbol.name)
            && self.impl_header_trait_resolves_to(&impl_parent.name, trait_symbol, impl_parent)
    }

    pub(crate) fn impl_header_trait_resolves_to(
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
                        .unwrap_or_else(|| last_path_segment_str(&import.path));
                    alias_or_name == trait_symbol.name.as_str()
                        && self.import_module_matches_symbol(import, trait_symbol)
                });
        }
        if segments.first().map(String::as_str) == Some("crate") {
            let mut expected = self.module_path_for_symbol(trait_symbol);
            expected.push(trait_symbol.name.clone());
            return segments == expected;
        }
        let receiver = segments[..segments.len() - 1].join("::");
        let trait_module_path = self.module_path_for_symbol(trait_symbol);
        self.receiver_module_paths(impl_anchor, &receiver)
            .into_iter()
            .any(|receiver_path| receiver_path == trait_module_path)
    }

    pub(crate) fn reference_is_impl_method_declaration_for_trait(
        &self,
        trait_method: &GraphSymbol,
        reference: &ParsedReference,
        sources: &mut SourceCache,
    ) -> bool {
        let Some(owner) = reference
            .owner_id
            .as_ref()
            .and_then(|id| self.symbols.get(id))
        else {
            return false;
        };
        if !self.impl_method_implements_trait_method(&owner.id, trait_method)
            || !self.reference_is_symbol_declaration(owner, reference, sources)
        {
            return false;
        }
        !self.symbol_or_ancestors_have_cfg_attribute(owner, sources)
    }

    pub(crate) fn associated_type_reference_matches_symbol(
        &self,
        symbol: &GraphSymbol,
        reference: &ParsedReference,
    ) -> bool {
        if symbol.kind != SymbolKind::TypeAlias
            || last_path_segment_str(&reference.text) != symbol.name.as_str()
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

    pub(crate) fn reference_owner_trait(
        &self,
        reference: &ParsedReference,
    ) -> Option<&GraphSymbol> {
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

    pub(crate) fn trait_path_matches_symbol(
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
        let trait_module_path = self.module_path_for_symbol(trait_symbol);
        self.receiver_module_paths(owner, &receiver)
            .into_iter()
            .any(|receiver_path| receiver_path == trait_module_path)
    }

    pub(crate) fn symbol_or_ancestors_have_cfg_attribute(
        &self,
        symbol: &GraphSymbol,
        sources: &mut SourceCache,
    ) -> bool {
        if has_cfg_attribute(symbol) || self.symbol_has_leading_cfg_attribute(symbol, sources) {
            return true;
        }
        let mut parent_id = symbol.parent_id.as_ref();
        while let Some(id) = parent_id {
            let Some(parent) = self.symbols.get(id) else {
                break;
            };
            if has_cfg_attribute(parent) || self.symbol_has_leading_cfg_attribute(parent, sources) {
                return true;
            }
            parent_id = parent.parent_id.as_ref();
        }
        false
    }

    pub(crate) fn symbol_has_leading_cfg_attribute(
        &self,
        symbol: &GraphSymbol,
        sources: &mut SourceCache,
    ) -> bool {
        let Some(source) = sources.source(self, &symbol.file_id) else {
            return false;
        };
        let Some(prefix) = source.get(..symbol.span.start_byte as usize) else {
            return false;
        };
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

    pub(crate) fn reference_is_symbol_declaration(
        &self,
        symbol: &GraphSymbol,
        reference: &ParsedReference,
        sources: &mut SourceCache,
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
        let Some(source) = sources.source(self, &symbol.file_id) else {
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

    pub(crate) fn reference_has_external_scope_prefix(
        &self,
        reference: &ParsedReference,
        sources: &mut SourceCache,
    ) -> bool {
        if reference.text.contains("::") {
            return false;
        }
        let language = self.reference_language(reference);
        let Some(source) = sources.source(self, &reference.file_id) else {
            return false;
        };
        let Some(prefix) = source.get(..reference.span.start_byte as usize) else {
            return false;
        };
        let Some(scope) = scope_prefix_before(prefix) else {
            return false;
        };
        !scope.is_empty() && path_starts_with_external_root(scope, language)
    }

    pub(crate) fn reference_has_uppercase_scope_prefix(
        &self,
        reference: &ParsedReference,
        sources: &mut SourceCache,
    ) -> bool {
        if reference.text.contains("::") {
            return false;
        }
        let Some(source) = sources.source(self, &reference.file_id) else {
            return false;
        };
        let Some(prefix) = source.get(..reference.span.start_byte as usize) else {
            return false;
        };
        let Some(scope) = scope_prefix_before(prefix) else {
            return false;
        };
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

    pub(crate) fn semantic_edge_for_reference(
        &self,
        reference: &ParsedReference,
    ) -> Option<&GraphEdge> {
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

    pub(crate) fn call_edge_for_reference(
        &self,
        reference: &ParsedReference,
    ) -> Option<&GraphEdge> {
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
                        // Half-open containment: the reference must sit fully
                        // inside the edge span, not merely touch its boundary.
                        .map(|span| span.contains_span(reference.span))
                        .unwrap_or(false)
                    && last_path_segment_str(&edge.target_text)
                        == last_path_segment_str(&reference.text)
            })
            .min_by_key(|edge| {
                edge.span
                    .map(|span| span.end_byte.saturating_sub(span.start_byte))
                    .unwrap_or(u32::MAX)
            })
    }

    pub(crate) fn can_bind_loose_reference(
        &self,
        symbol: &GraphSymbol,
        reference: &ParsedReference,
    ) -> bool {
        if !reference_kind_can_bind_symbol(reference, symbol) {
            return false;
        }
        let candidates = self.symbols_by_name_or_scan(&symbol.name);
        let mut same_file_candidates = candidates
            .iter()
            .filter_map(|id| self.symbols.get(id))
            .filter(|candidate| {
                candidate.file_id == reference.file_id
                    && reference_kind_can_bind_symbol(reference, candidate)
            });
        let Some(only) = same_file_candidates.next() else {
            return false;
        };
        same_file_candidates.next().is_none() && only.id == symbol.id
    }
}

fn scope_prefix_before(prefix: &str) -> Option<&str> {
    let bytes = prefix.as_bytes();
    let mut start = bytes.len();
    while start > 0 {
        let byte = bytes[start - 1];
        if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b':' {
            start -= 1;
        } else {
            break;
        }
    }
    let scope = prefix[start..].trim_end_matches("::");
    if scope.is_empty() { None } else { Some(scope) }
}

#[cfg(test)]
#[path = "references_tests.rs"]
mod tests;
