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
        let psr4 = self.php_psr4_map();
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
                    // PSR-4 acceptance: a candidate whose declared namespace and
                    // on-disk path obey the project's autoload map is reachable
                    // by a fully-qualified reference even without a same-namespace
                    // relationship or an explicit `use`, exactly as PHP's PSR-4
                    // autoloader would load it. Gated on the candidate actually
                    // sitting at its PSR-4 path so we never widen to leaf-name
                    // collisions across unrelated trees. When the reference is
                    // itself namespace-qualified (`new App\Service\Mailer`) the
                    // candidate's full dotted identity must equal the reference's,
                    // so a qualified name only binds the exact autoloaded class.
                    || (self.php_symbol_is_psr4_consistent(symbol, &psr4)
                        && php_qualified_reference_matches_symbol(name, symbol))
            })
            .map(|symbol| symbol.id.clone())
            .collect::<Vec<_>>();
        ids.sort_by(|left, right| left.0.cmp(&right.0));
        ids.dedup();
        ids
    }

    /// Build the project's PSR-4 autoload table (`namespace prefix -> source
    /// root directory`) for PHP candidate acceptance.
    ///
    /// The authoritative source for this table is `composer.json`'s
    /// `autoload.psr-4` / `autoload-dev.psr-4` maps. Parsing `composer.json`
    /// belongs in the project-facts layer (it is not a PHP source file fed to
    /// the parser), so when those facts are wired this method should consume
    /// them. Until then it derives the same `prefix -> root` table structurally
    /// from the workspace's own PHP type symbols: a class whose dotted identity
    /// is `Vendor.Pkg.Sub.Name` and whose file is `<root>/Sub/Name.php`
    /// witnesses the mapping `Vendor.Pkg -> <root>`. Only the longest matching
    /// (prefix, root) pair per prefix is retained, which is exactly how a PSR-4
    /// autoloader picks the most specific rule.
    fn php_psr4_map(&self) -> Psr4Map {
        let mut map = Psr4Map::default();
        for symbol in self.symbols.values() {
            if !self.symbol_is_php_type(symbol) {
                continue;
            }
            let Some(identity) = symbol.language_identity.as_deref() else {
                continue;
            };
            let Some(dotted) = identity.strip_prefix("T:") else {
                continue;
            };
            let Some(file) = self.files.get(&symbol.file_id) else {
                continue;
            };
            if let Some((prefix, root)) = php_psr4_entry_from_layout(dotted, &file.relative_path) {
                map.insert(prefix, root);
            }
        }
        map
    }

    /// True when `symbol`'s declared namespace and on-disk path are consistent
    /// with one of the project's PSR-4 autoload rules — i.e. the file lives at
    /// the path the autoloader would compute from the class's fully-qualified
    /// name. Returns false for symbols without a `T:` identity, without an
    /// indexed file, or whose layout no PSR-4 rule explains.
    pub(crate) fn php_symbol_is_psr4_consistent(
        &self,
        symbol: &GraphSymbol,
        psr4: &Psr4Map,
    ) -> bool {
        if !self.symbol_is_php_type(symbol) {
            return false;
        }
        let Some(identity) = symbol.language_identity.as_deref() else {
            return false;
        };
        let Some(dotted) = identity.strip_prefix("T:") else {
            return false;
        };
        let Some(file) = self.files.get(&symbol.file_id) else {
            return false;
        };
        psr4.is_consistent(dotted, &file.relative_path)
    }

    /// Resolve a PHP type-bearing call — `new ClassName(...)` object creation —
    /// to its declaring class/interface/trait/enum symbol, accepting candidates
    /// that are merely PSR-4-consistent (autoloadable) with the call site in
    /// addition to same-namespace / explicitly-`use`d types.
    ///
    /// The call resolver dispatches here for PHP callers on a
    /// [`ParsedCallKind::Direct`] call whose `target_text` names a type the
    /// generic single-name resolver did not already bind. Declines (returns
    /// `None`) for non-PHP callers, calls with an instance/scope receiver
    /// (`$x->m()`, `Foo::bar()` — those are method calls, not constructions),
    /// empty names, and whenever the relaxed candidate set is not exactly one
    /// symbol so the caller can fall back to a `CandidateSet` edge.
    pub(crate) fn php_type_candidate_for_reference(
        &self,
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        if !self.caller_is_php(caller_id) {
            return None;
        }
        if call.kind != ParsedCallKind::Direct || call.receiver.is_some() {
            return None;
        }
        let name = call.target_text.trim();
        if name.is_empty() {
            return None;
        }
        match self
            .php_type_candidates_for_name_in_file(&call.file_id, name)
            .as_slice()
        {
            [only] => Some(only.clone()),
            _ => None,
        }
    }
}

/// PSR-4 autoload table: dotted namespace prefix (`Vendor.Pkg`) → source root
/// directory (slash-relative, no trailing slash, e.g. `src`). Mirrors
/// `composer.json`'s `autoload.psr-4` entries, whose backslash prefixes and
/// trailing-slash roots are normalised to the dotted / slash forms squeezy
/// already uses for PHP identities and file ids.
#[derive(Debug, Default, Clone)]
pub(crate) struct Psr4Map {
    /// Kept sorted by descending prefix length so [`Self::is_consistent`] tries
    /// the most specific rule first, matching a real PSR-4 autoloader.
    entries: Vec<(String, String)>,
}

impl Psr4Map {
    /// Insert one `prefix -> root` rule from a normalised composer entry or a
    /// structurally-derived witness. Duplicate prefixes keep the first root and
    /// the table stays ordered most-specific-first.
    pub(crate) fn insert(&mut self, prefix: String, root: String) {
        let prefix = prefix.trim_matches('.').to_string();
        let root = root.trim_matches('/').to_string();
        if prefix.is_empty() {
            return;
        }
        if self.entries.iter().any(|(existing_prefix, existing_root)| {
            *existing_prefix == prefix && *existing_root == root
        }) {
            return;
        }
        self.entries.push((prefix, root));
        self.entries
            .sort_by(|left, right| right.0.len().cmp(&left.0.len()).then(left.0.cmp(&right.0)));
    }

    /// True when the class whose dotted fully-qualified name is `dotted_identity`
    /// would be autoloaded from `relative_path` under one of the PSR-4 rules.
    pub(crate) fn is_consistent(&self, dotted_identity: &str, relative_path: &str) -> bool {
        self.entries
            .iter()
            .any(|(prefix, root)| psr4_path_matches(prefix, root, dotted_identity, relative_path))
    }
}

/// Infer the `(prefix, root)` PSR-4 rule witnessed by a single class whose
/// dotted identity is `dotted_identity` and whose file is `relative_path`.
///
/// PSR-4 maps the trailing namespace segments onto a directory path: a class
/// `A.B.C.Name` in `root/C/Name.php` witnesses `A.B -> root`. We peel the leaf
/// (class name = file stem) and as many trailing namespace segments as the path
/// directories agree with, then treat whatever namespace remains as the prefix
/// and the leftover leading directories as the root. Returns `None` when the
/// file stem disagrees with the class name (not PSR-4 layout) or there is no
/// namespace left to use as a prefix.
fn php_psr4_entry_from_layout(
    dotted_identity: &str,
    relative_path: &str,
) -> Option<(String, String)> {
    let ns_segments: Vec<&str> = dotted_identity
        .split('.')
        .filter(|segment| !segment.is_empty())
        .collect();
    if ns_segments.len() < 2 {
        // Need at least one prefix segment plus the class name.
        return None;
    }
    let class_name = *ns_segments.last()?;
    let path = relative_path.replace('\\', "/");
    let path = path
        .strip_suffix(".php")
        .or_else(|| path.strip_suffix(".PHP"))
        .unwrap_or(&path);
    let dir_segments: Vec<&str> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    let file_stem = *dir_segments.last()?;
    if file_stem != class_name {
        return None;
    }
    // Walk the namespace tail (excluding the class name) against the directory
    // tail (excluding the file stem); every aligned pair must match for the
    // layout to be PSR-4.
    let ns_tail = &ns_segments[..ns_segments.len() - 1];
    let dir_tail = &dir_segments[..dir_segments.len() - 1];
    let mut matched = 0usize;
    while matched < ns_tail.len()
        && matched < dir_tail.len()
        && ns_tail[ns_tail.len() - 1 - matched] == dir_tail[dir_tail.len() - 1 - matched]
    {
        matched += 1;
    }
    if matched == 0 {
        return None;
    }
    let prefix_segments = &ns_tail[..ns_tail.len() - matched];
    if prefix_segments.is_empty() {
        return None;
    }
    let root_segments = &dir_tail[..dir_tail.len() - matched];
    let prefix = prefix_segments.join(".");
    let root = root_segments.join("/");
    Some((prefix, root))
}

/// True when `relative_path` is exactly where a PSR-4 rule `prefix -> root`
/// would place the class whose dotted identity is `dotted_identity`.
fn psr4_path_matches(prefix: &str, root: &str, dotted_identity: &str, relative_path: &str) -> bool {
    let with_dot = format!("{prefix}.");
    let Some(suffix) = dotted_identity.strip_prefix(&with_dot) else {
        return false;
    };
    if suffix.is_empty() {
        return false;
    }
    let relative_subpath = suffix.replace('.', "/");
    let expected = if root.is_empty() {
        format!("{relative_subpath}.php")
    } else {
        format!("{root}/{relative_subpath}.php")
    };
    let actual = relative_path.replace('\\', "/");
    actual.eq_ignore_ascii_case(&expected)
}

/// Reconcile a caller's reference text with a candidate symbol's identity for
/// PSR-4 acceptance. A bare leaf reference (`Mailer`) matches any candidate the
/// leaf-name index already returned. A namespace-qualified reference
/// (`App\Service\Mailer`, optionally fully-qualified with a leading `\`) only
/// matches when the candidate's dotted `T:` identity equals the reference's
/// dotted form, so a qualified name binds the one autoloaded class it names.
fn php_qualified_reference_matches_symbol(reference: &str, symbol: &GraphSymbol) -> bool {
    let trimmed = reference.trim();
    if !trimmed.contains('\\') {
        return true;
    }
    let dotted_reference = trimmed
        .split('\\')
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join(".");
    if dotted_reference.is_empty() {
        return true;
    }
    let Some(identity) = symbol.language_identity.as_deref() else {
        return false;
    };
    let full_type_path = identity.strip_prefix("T:").unwrap_or(identity);
    full_type_path == dotted_reference
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
