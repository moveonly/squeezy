use crate::*;

impl SemanticGraph {
    /// Walk every PHP class/interface/trait/enum and synthesize Extends /
    /// Implements / UsesTrait edges from the `base:<leaf>` and
    /// `uses_trait:<leaf>` attributes the extractor stamped onto the symbol.
    /// The shape mirrors C# and Java for base types: a single base candidate
    /// becomes an `Extends`/`Implements` edge; zero matches stay `External`;
    /// multiple matches become a `CandidateSet`. Trait inclusion
    /// (`use TraitA;` inside a class body) follows the same candidate-set
    /// machinery but always lands as `EdgeKind::UsesTrait`.
    pub(crate) fn add_php_type_edges(&mut self) {
        let symbols = self
            .symbols
            .values()
            .filter(|symbol| self.symbol_is_php_type(symbol))
            .cloned()
            .collect::<Vec<_>>();
        for symbol in symbols {
            let bases = symbol
                .attributes
                .iter()
                .filter_map(|attribute| attribute.strip_prefix("base:"))
                .map(str::to_string)
                .collect::<Vec<_>>();
            for base in bases {
                let candidates = self.php_type_candidates_for_name_in_file(&symbol.file_id, &base);
                let (to, confidence, edge_candidates) = match candidates.as_slice() {
                    [only] => (Some(only.clone()), Confidence::Heuristic, Vec::new()),
                    [] => (None, Confidence::External, Vec::new()),
                    _ => (
                        None,
                        Confidence::CandidateSet,
                        candidates
                            .iter()
                            .take(MAX_EDGE_CANDIDATES)
                            .cloned()
                            .collect(),
                    ),
                };
                let kind = to
                    .as_ref()
                    .and_then(|id| self.symbols.get(id))
                    .map(|target| {
                        if target.kind == SymbolKind::Interface {
                            EdgeKind::Implements
                        } else {
                            EdgeKind::Extends
                        }
                    })
                    .unwrap_or(EdgeKind::Extends);
                self.edges.push(GraphEdge {
                    from: symbol.id.clone(),
                    to,
                    target_text: base,
                    kind,
                    span: Some(symbol.span),
                    confidence,
                    freshness: Freshness::Fresh,
                    provenance: Provenance::new("tree-sitter-php", "base type edge"),
                    candidates: edge_candidates,
                });
            }

            let traits = symbol
                .attributes
                .iter()
                .filter_map(|attribute| attribute.strip_prefix("uses_trait:"))
                .map(str::to_string)
                .collect::<Vec<_>>();
            for trait_name in traits {
                let candidates =
                    self.php_type_candidates_for_name_in_file(&symbol.file_id, &trait_name);
                let (to, confidence, edge_candidates) = match candidates.as_slice() {
                    [only] => (Some(only.clone()), Confidence::Heuristic, Vec::new()),
                    [] => (None, Confidence::External, Vec::new()),
                    _ => (
                        None,
                        Confidence::CandidateSet,
                        candidates
                            .iter()
                            .take(MAX_EDGE_CANDIDATES)
                            .cloned()
                            .collect(),
                    ),
                };
                self.edges.push(GraphEdge {
                    from: symbol.id.clone(),
                    to,
                    target_text: trait_name,
                    kind: EdgeKind::UsesTrait,
                    span: Some(symbol.span),
                    confidence,
                    freshness: Freshness::Fresh,
                    provenance: Provenance::new("tree-sitter-php", "trait include edge"),
                    candidates: edge_candidates,
                });
            }
        }
    }

    fn symbol_is_php_type(&self, symbol: &GraphSymbol) -> bool {
        self.files
            .get(&symbol.file_id)
            .map(|file| file.language == LanguageKind::Php)
            .unwrap_or(false)
            && matches!(
                symbol.kind,
                SymbolKind::Class
                    | SymbolKind::Struct
                    | SymbolKind::Interface
                    | SymbolKind::Trait
                    | SymbolKind::Enum
                    | SymbolKind::TypeAlias
            )
    }

    /// True when the call's owner sits in a PHP file. Mirrors the gating used
    /// by [`SemanticGraph::caller_is_python`] so the call resolver can dispatch
    /// to PHP-specific inheritance walks without re-querying the file table.
    pub(crate) fn caller_is_php(&self, caller_id: &SymbolId) -> bool {
        self.symbols
            .get(caller_id)
            .and_then(|caller| self.files.get(&caller.file_id))
            .map(|file| file.language == LanguageKind::Php)
            .unwrap_or(false)
    }

    /// PHP method-resolution entry point used by the call resolver for
    /// `$this->method()`, `self::method()`, `static::method()` and
    /// `parent::method()`. Walks the caller's class up through trait
    /// inclusion, the `Extends` parent, and any `Implements` interface
    /// in PHP's actual lookup order: own class first, then the trait
    /// declarations (in declaration order), then the parent class, then
    /// the implemented interfaces.
    ///
    /// `parent::` skips the caller's own class but still consults its
    /// ancestors, matching the language's runtime behavior where the
    /// own class's override is intentionally bypassed.
    pub(crate) fn inherited_php_method(
        &self,
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        if !self.caller_is_php(caller_id) {
            return None;
        }
        let receiver = call.receiver.as_deref()?;
        // PHP's `$this` is the only object-receiver that implies the
        // caller's own class hierarchy. `self::`, `static::` and `parent::`
        // are scope receivers and follow the same lookup rule; the latter
        // intentionally skips the caller's own definition so a child class's
        // override no longer shadows the parent's implementation.
        let skip_self = match receiver {
            "$this" | "self" | "static" => false,
            "parent" => true,
            _ => return None,
        };
        let class_id = self.php_class_for_caller(caller_id)?;
        if !skip_self && let Some(method) = self.php_method_on_class(&class_id, &call.name) {
            return Some(method);
        }
        // The cross-file ancestor walker enumerates trait → extends →
        // implements in declaration order, so the first matching method is
        // the one PHP's method-resolution rules would actually bind to.
        for ancestor in self.walk_inheritance_ancestors(&class_id) {
            if let Some(method) = self.php_method_on_class(&ancestor, &call.name) {
                return Some(method);
            }
        }
        None
    }

    /// Climb the parent chain from a call's caller symbol to the enclosing
    /// PHP class/interface/trait/enum. Mirrors
    /// [`SemanticGraph::python_class_for_caller`] for the PHP family so the
    /// trait-aware walker can start from the right class.
    pub(crate) fn php_class_for_caller(&self, caller_id: &SymbolId) -> Option<SymbolId> {
        let caller = self.symbols.get(caller_id)?;
        if self.symbol_is_php_type(caller) {
            return Some(caller.id.clone());
        }
        let mut current = caller.parent_id.clone();
        while let Some(id) = current {
            let symbol = self.symbols.get(&id)?;
            if self.symbol_is_php_type(symbol) {
                return Some(symbol.id.clone());
            }
            current = symbol.parent_id.clone();
        }
        None
    }

    /// Single-symbol lookup of a method named `method_name` declared
    /// directly on the class/trait/interface identified by `class_id`.
    /// Returns `None` when the class has zero or multiple matches so the
    /// caller can fall back to the next ancestor candidate.
    pub(crate) fn php_method_on_class(
        &self,
        class_id: &SymbolId,
        method_name: &str,
    ) -> Option<SymbolId> {
        single_symbol(
            self.children_by_parent
                .get(class_id)?
                .iter()
                .filter_map(|child_id| self.symbols.get(child_id))
                .filter(|symbol| {
                    matches!(
                        symbol.kind,
                        SymbolKind::Method | SymbolKind::Function | SymbolKind::Test
                    ) && symbol.name == method_name
                })
                .map(|symbol| symbol.id.clone()),
        )
    }

    fn php_type_candidates_for_name_in_file(&self, file_id: &FileId, name: &str) -> Vec<SymbolId> {
        let direct_name = last_path_segment(name);
        let caller_namespace = self.packages.get(file_id);
        let mut ids = self
            .symbols_by_name_or_scan(&direct_name)
            .into_iter()
            .filter_map(|id| self.symbols.get(&id))
            .filter(|symbol| self.symbol_is_php_type(symbol))
            .filter(|symbol| {
                symbol.file_id == *file_id
                    || self.packages.get(&symbol.file_id) == caller_namespace
                    || self
                        .imports_for_file(file_id)
                        .any(|import| php_import_matches_symbol(import, symbol))
            })
            .map(|symbol| symbol.id.clone())
            .collect::<Vec<_>>();
        ids.sort_by(|left, right| left.0.cmp(&right.0));
        ids.dedup();
        ids
    }
}

/// True when an `use Foo\Bar [as Alias];` import matches a workspace symbol.
/// Mirrors the C# version: prefer aliases, then walk the symbol's stable
/// `T:Foo.Bar.Baz` identity backwards from the leaf so a `use Foo\Bar\Service;`
/// matches both `T:Foo.Bar.Service` and the namespace-only `T:Foo.Bar` form.
pub(crate) fn php_import_matches_symbol(import: &ParsedImport, symbol: &GraphSymbol) -> bool {
    if import.alias.as_deref() == Some(symbol.name.as_str()) {
        return true;
    }
    let Some(identity) = symbol.language_identity.as_deref() else {
        return false;
    };
    let full_type_path = identity.strip_prefix("T:").unwrap_or(identity);
    let suffix = format!(".{}", symbol.name);
    let namespace = full_type_path
        .strip_suffix(&suffix)
        .unwrap_or(full_type_path);
    import.path == namespace || import.path == full_type_path
}
