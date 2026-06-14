use crate::*;

impl SemanticGraph {
    pub(crate) fn unresolved_js_ts_imported_direct_call(
        &self,
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> bool {
        if call.kind != ParsedCallKind::Direct || call.receiver.is_some() {
            return false;
        }
        let Some(caller) = self.symbols.get(caller_id) else {
            return false;
        };
        if !self
            .files
            .get(&caller.file_id)
            .map(|file| is_js_ts_language(file.language))
            .unwrap_or(false)
        {
            return false;
        }
        self.imports_for_file(&caller.file_id)
            .filter(|import| self.import_visible_from_symbol(import, caller))
            .filter(|import| import.span.start_byte <= call.span.start_byte)
            .any(|import| {
                import
                    .alias
                    .as_deref()
                    .map(|alias| alias == call.name)
                    .unwrap_or_else(|| last_path_segment(&import.path) == call.name)
            })
    }

    /// Return `true` when a JS/TS `import` statement is a plausible source
    /// for `symbol`. Uses [`JsTsResolver`] to expand the import's module
    /// specifier into candidate file paths and compares them against the
    /// module-path variants of the symbol's declaring file (e.g. `foo.ts`,
    /// `foo/index.ts`, `foo.js`). A bare import of `'./foo'` in the caller
    /// correctly matches any declaration in a recognised variant of that
    /// module — without this check every cross-file JS/TS call that lacks a
    /// matching named-binding falls back to a `CandidateSet`.
    pub(crate) fn js_ts_import_matches_symbol(
        &self,
        import: &ParsedImport,
        symbol: &GraphSymbol,
    ) -> bool {
        let Some(symbol_file) = self.files.get(&symbol.file_id) else {
            return false;
        };
        let Some(module) = js_ts_import_module_part(import) else {
            return false;
        };
        let import_file = self.files.get(&import.file_id);
        let module_candidates = self.js_ts_resolver.module_candidates(module, import_file);
        if module_candidates.is_empty() {
            return false;
        }
        let symbol_modules = js_ts_file_module_variants(&symbol_file.relative_path);
        module_candidates
            .iter()
            .any(|candidate| symbol_modules.contains(candidate))
    }

    /// Bug #14: resolve a JS/TS `this.foo()` / `super.foo()` call to a method
    /// inherited from the caller class's `extends`/`implements` ancestors.
    ///
    /// The JS/TS parser records inheritance as queryable `base:`/`iface:`
    /// attributes on the class symbol (mirroring C#, Java, Dart, Python), but
    /// unlike C#/PHP it never lowers them to `Extends`/`Implements` edges, so
    /// the edge-driven `walk_inheritance_ancestors` would find nothing. We walk
    /// the attribute chain directly — exactly like the Dart resolver — resolving
    /// each ancestor name to a JS/TS class/interface symbol and looking for the
    /// method there.
    ///
    /// Receivers handled: `this`/`self` (own class first, then ancestors) and
    /// `super` (skip the own class, go straight to ancestors). Any other
    /// receiver belongs to a value of some other type and is left to the
    /// type-directed rules.
    pub(crate) fn inherited_js_ts_method(
        &self,
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        if !self.caller_is_js_ts(caller_id) {
            return None;
        }
        let receiver = call.receiver.as_deref()?;
        let skip_self = match receiver {
            "this" | "self" => false,
            "super" => true,
            _ => return None,
        };
        let class_id = self.js_ts_class_for_caller(caller_id)?;
        if !skip_self && let Some(method) = self.js_ts_method_on_class(&class_id, &call.name) {
            return Some(method);
        }
        self.js_ts_method_in_ancestors(&class_id, &call.name, 0)
    }

    fn caller_is_js_ts(&self, caller_id: &SymbolId) -> bool {
        self.symbols
            .get(caller_id)
            .and_then(|caller| self.files.get(&caller.file_id))
            .map(|file| is_js_ts_language(file.language))
            .unwrap_or(false)
    }

    /// Climb the caller's `parent_id` chain to the enclosing class/interface.
    fn js_ts_class_for_caller(&self, caller_id: &SymbolId) -> Option<SymbolId> {
        let mut current = self.symbols.get(caller_id)?;
        loop {
            if matches!(current.kind, SymbolKind::Class | SymbolKind::Interface) {
                return Some(current.id.clone());
            }
            let parent_id = current.parent_id.as_ref()?;
            current = self.symbols.get(parent_id)?;
        }
    }

    /// A method named `method_name` declared directly on `class_id`.
    fn js_ts_method_on_class(&self, class_id: &SymbolId, method_name: &str) -> Option<SymbolId> {
        single_symbol(
            self.children_by_parent
                .get(class_id)
                .into_iter()
                .flatten()
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

    /// Walk the class's `base:`/`iface:` ancestor chain (depth-capped, with a
    /// visited-set to keep a diamond hierarchy from re-expanding shared
    /// ancestors) looking for `method_name`. `base:` ancestors take priority
    /// over `iface:` ones, matching JS/TS single-inheritance + interface
    /// semantics.
    fn js_ts_method_in_ancestors(
        &self,
        class_id: &SymbolId,
        method_name: &str,
        depth: usize,
    ) -> Option<SymbolId> {
        let mut visited = std::collections::HashSet::new();
        visited.insert(class_id.clone());
        self.js_ts_method_in_ancestors_visited(class_id, method_name, depth, &mut visited)
    }

    fn js_ts_method_in_ancestors_visited(
        &self,
        class_id: &SymbolId,
        method_name: &str,
        depth: usize,
        visited: &mut std::collections::HashSet<SymbolId>,
    ) -> Option<SymbolId> {
        const JS_TS_ANCESTOR_DEPTH_CAP: usize = 8;
        if depth >= JS_TS_ANCESTOR_DEPTH_CAP {
            return None;
        }
        let class = self.symbols.get(class_id)?;
        let file_id = class.file_id.clone();
        let bases = class
            .attributes
            .iter()
            .filter_map(|attr| attr.strip_prefix("base:"));
        let ifaces = class
            .attributes
            .iter()
            .filter_map(|attr| attr.strip_prefix("iface:"));
        for name in bases.chain(ifaces) {
            for ancestor_id in self.js_ts_class_candidates_for_name_in_file(&file_id, name) {
                if !visited.insert(ancestor_id.clone()) {
                    continue;
                }
                if let Some(method) = self.js_ts_method_on_class(&ancestor_id, method_name) {
                    return Some(method);
                }
                if let Some(method) = self.js_ts_method_in_ancestors_visited(
                    &ancestor_id,
                    method_name,
                    depth + 1,
                    visited,
                ) {
                    return Some(method);
                }
            }
        }
        None
    }

    /// Resolve a `base:`/`iface:` ancestor name to JS/TS class/interface
    /// symbols, scoping to the calling file: same-file declarations and any
    /// declaration brought into scope by a matching import. This avoids binding
    /// to an unrelated same-named class in another module.
    fn js_ts_class_candidates_for_name_in_file(
        &self,
        file_id: &FileId,
        name: &str,
    ) -> Vec<SymbolId> {
        let leaf = last_path_segment(name);
        let mut ids = self
            .symbols_by_name_or_scan(&leaf)
            .into_iter()
            .filter_map(|id| self.symbols.get(&id))
            .filter(|symbol| matches!(symbol.kind, SymbolKind::Class | SymbolKind::Interface))
            .filter(|symbol| {
                self.files
                    .get(&symbol.file_id)
                    .map(|file| is_js_ts_language(file.language))
                    .unwrap_or(false)
            })
            .filter(|symbol| {
                symbol.file_id == *file_id
                    || self
                        .imports_for_file(file_id)
                        .filter(|import| !crate::is_package_marker_alias(import.alias.as_deref()))
                        .filter(|import| {
                            import
                                .alias
                                .as_deref()
                                .unwrap_or_else(|| last_path_segment_str(&import.path))
                                == leaf
                        })
                        .any(|import| self.js_ts_import_matches_symbol(import, symbol))
            })
            .map(|symbol| symbol.id.clone())
            .collect::<Vec<_>>();
        ids.sort_by(|left, right| left.0.cmp(&right.0));
        ids.dedup();
        ids
    }

    /// LANGUAGE-ROI #8: resolve a TS `obj.method()` call by reading the
    /// TypeScript type annotation on the receiver `obj` (a typed parameter,
    /// typed body local, or class field) and scoping the lookup to the
    /// annotated class/interface and its `extends`/`implements` ancestors.
    ///
    /// This complements [`Self::inherited_js_ts_method`] (which only handles
    /// the `this`/`super` receivers): any *named* receiver with a static type
    /// in source is now bound to the concrete method instead of decaying to a
    /// `CandidateSet`. We deliberately stay conservative — only a single,
    /// unambiguous class/interface declaration for the annotated type name is
    /// accepted (via [`single_symbol`]), so a same-named type in another module
    /// can never hijack the edge.
    ///
    /// Decline conditions:
    /// * caller is not a JS/TS file,
    /// * the receiver is `this`/`super`/`self` (handled elsewhere), is not a
    ///   plain identifier (chained / call / index receivers carry no usable
    ///   annotation), or has no recoverable TS type annotation,
    /// * the annotated type name resolves to a built-in/primitive,
    /// * the type name resolves to zero or more-than-one in-scope
    ///   class/interface declaration (ambiguous), or
    /// * neither the class nor its ancestor chain declares `call.name`.
    pub(crate) fn ts_receiver_typed_method(
        &self,
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        if !self.caller_is_js_ts(caller_id) {
            return None;
        }
        let receiver = call.receiver.as_deref()?;
        if matches!(receiver, "this" | "super" | "self") || !is_simple_js_ts_identifier(receiver) {
            return None;
        }
        let type_name = self.ts_receiver_annotated_type(caller_id, receiver)?;
        let caller = self.symbols.get(caller_id)?;
        // Require exactly one in-scope class/interface for the annotated type:
        // a same-named type in another module would otherwise forge a wrong
        // edge, so decline on ambiguity rather than guess.
        let candidates = self.js_ts_class_candidates_for_name_in_file(&caller.file_id, &type_name);
        let class_id = single_symbol(candidates.into_iter())?;
        if let Some(method) = self.js_ts_method_on_class(&class_id, &call.name) {
            return Some(method);
        }
        self.js_ts_method_in_ancestors(&class_id, &call.name, 0)
    }

    /// Best-effort static type name of a named receiver inside a JS/TS caller,
    /// derived from TypeScript type annotations only (no inference).
    ///
    /// Resolution order, narrowest scope first:
    /// 1. the caller's own signature header — a typed parameter
    ///    (`method(obj: Foo)`) or a typed local visible in the header,
    /// 2. a typed body local of the caller (`const obj: Foo = ...`), recorded
    ///    as a child `Const`/`Field`/`Static` symbol whose `signature` carries
    ///    the annotation,
    /// 3. a field on the caller's enclosing class (`private obj: Foo;`).
    ///
    /// Returns the bare (capitalised, non-builtin) type name, or `None` when no
    /// usable annotation is found.
    fn ts_receiver_annotated_type(&self, caller_id: &SymbolId, receiver: &str) -> Option<String> {
        let caller = self.symbols.get(caller_id)?;
        // 1. Typed parameter / header-visible local on the caller itself.
        if let Some(ty) = js_ts_annotated_type_in_signature(&caller.signature, receiver) {
            return Some(ty);
        }
        // 2. Typed body local declared inside the caller.
        if let Some(ty) = self
            .children_by_parent
            .get(caller_id)
            .into_iter()
            .flatten()
            .filter_map(|child_id| self.symbols.get(child_id))
            .filter(|symbol| {
                matches!(
                    symbol.kind,
                    SymbolKind::Const | SymbolKind::Field | SymbolKind::Static
                ) && symbol.name == receiver
            })
            .find_map(|symbol| js_ts_annotated_type_in_signature(&symbol.signature, receiver))
        {
            return Some(ty);
        }
        // 3. A field on the caller's enclosing class.
        let class_id = self.js_ts_class_for_caller(caller_id)?;
        self.children_by_parent
            .get(&class_id)
            .into_iter()
            .flatten()
            .filter_map(|child_id| self.symbols.get(child_id))
            .filter(|symbol| symbol.kind == SymbolKind::Field && symbol.name == receiver)
            .find_map(|symbol| js_ts_annotated_type_in_signature(&symbol.signature, receiver))
    }
}

/// `true` when `text` is a single bare JS/TS identifier (no member access,
/// call, index, whitespace, or other expression syntax). Chained or
/// computed receivers carry no usable single annotation, so they decline.
fn is_simple_js_ts_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first == '$' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch == '$' || ch.is_ascii_alphanumeric())
}

/// Find the TypeScript type annotation attached to `name` inside a declaration
/// header/signature, e.g. the `Foo` in `method(name: Foo)`, `name?: Foo`, or
/// `const name: Foo = ...`. Returns the bare capitalised, non-builtin type
/// name, declining unions/intersections (ambiguous) and primitives.
fn js_ts_annotated_type_in_signature(signature: &str, name: &str) -> Option<String> {
    let mut search_from = 0;
    while let Some(rel) = signature[search_from..].find(name) {
        let start = search_from + rel;
        let end = start + name.len();
        search_from = end;
        // Whole-identifier match only (so `userName` does not match `user`).
        let before = signature[..start].chars().next_back();
        let after_ident = signature[end..].chars();
        let bounded_before = before
            .map(|ch| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '$'))
            .unwrap_or(true);
        if !bounded_before {
            continue;
        }
        // After the identifier allow an optional `?`, then whitespace, then the
        // `:` that introduces the annotation. Anything else (`,`, `)`, `=`, a
        // `.` for member access) means this occurrence is not a binding site.
        let mut rest = after_ident.as_str().trim_start();
        rest = rest.strip_prefix('?').map(str::trim_start).unwrap_or(rest);
        let Some(annotation) = rest.strip_prefix(':') else {
            continue;
        };
        // Reject union / intersection types: the receiver could be any member,
        // so binding to one is unsound — leave it for the candidate set.
        let head = annotation
            .split(['=', ';', ',', ')', '{', '\n'])
            .next()
            .unwrap_or(annotation);
        if head.contains('|') || head.contains('&') {
            return None;
        }
        if let Some(type_name) = js_ts_type_name_from_annotation_str(head) {
            return Some(type_name);
        }
    }
    None
}

/// Extract a bare class/interface type name from a TS annotation fragment
/// (e.g. `Foo`, ` Foo<T>`, `ns.Foo`), returning `None` for primitives,
/// lowercase-leading names, and non-identifiers. Mirrors the parser's
/// `js_ts_type_name_from_annotation` so resolver and extractor agree on which
/// annotations are bindable.
fn js_ts_type_name_from_annotation_str(annotation: &str) -> Option<String> {
    let text = annotation
        .split(['=', ';', ',', ')', '(', '[', ']', '{', '}', '<', '|', '&'])
        .next()
        .unwrap_or(annotation)
        .trim()
        .trim_start_matches("readonly ")
        .trim();
    if text.is_empty()
        || matches!(
            text,
            "any"
                | "bigint"
                | "boolean"
                | "false"
                | "never"
                | "null"
                | "number"
                | "object"
                | "string"
                | "symbol"
                | "true"
                | "undefined"
                | "unknown"
                | "void"
        )
    {
        return None;
    }
    let name = last_path_segment(text);
    if is_simple_js_ts_identifier(&name)
        && name
            .chars()
            .next()
            .map(|ch| ch.is_ascii_uppercase())
            .unwrap_or(false)
    {
        Some(name)
    } else {
        None
    }
}

pub(crate) fn is_js_ts_language(language: LanguageKind) -> bool {
    matches!(
        language,
        LanguageKind::JavaScript | LanguageKind::Jsx | LanguageKind::TypeScript | LanguageKind::Tsx
    )
}

/// Incrementally maintained JS/TS module resolver.
///
/// Workspace-wide `tsconfig.json` / `package.json` files contribute path
/// mappings and package definitions. Re-parsing every config on every
/// `rebuild_semantic_edges` is O(n) per file save; the per-file caches
/// below let us reuse the derived state for configs whose `ContentHash`
/// is unchanged and only re-aggregate the flat lookup vectors when an
/// entry was added, removed, or rebuilt.
#[derive(Debug, Clone, Default)]
pub(crate) struct JsTsResolver {
    path_mappings: Vec<JsTsPathMapping>,
    packages: Vec<JsTsPackage>,
    tsconfig_entries: HashMap<FileId, JsTsTsconfigEntry>,
    package_entries: HashMap<FileId, JsTsPackageEntry>,
}

#[derive(Debug, Clone)]
pub(crate) struct JsTsPathMapping {
    config_dir: String,
    base_url: Option<String>,
    pattern: String,
    targets: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct JsTsPackage {
    root: String,
    name: String,
    exports: Vec<(String, String)>,
    main_entries: Vec<String>,
}

#[derive(Debug, Clone)]
struct JsTsTsconfigEntry {
    hash: ContentHash,
    relative_path: String,
    mappings: Vec<JsTsPathMapping>,
}

#[derive(Debug, Clone)]
struct JsTsPackageEntry {
    hash: ContentHash,
    relative_path: String,
    package: Option<JsTsPackage>,
}

/// Outcome of an incremental [`JsTsResolver::update_from_files`] pass.
///
/// `inserted` and `rebuilt` count freshly parsed configs (cache misses),
/// `reused` counts configs whose `ContentHash` matched the cache and were
/// skipped entirely. `removed` counts entries dropped because the file is
/// no longer in the workspace.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct JsTsResolverUpdate {
    pub inserted: usize,
    pub rebuilt: usize,
    pub reused: usize,
    pub removed: usize,
}

impl JsTsResolverUpdate {
    pub(crate) fn changed(&self) -> bool {
        self.inserted + self.rebuilt + self.removed > 0
    }

    #[allow(dead_code)]
    pub(crate) fn parses(&self) -> usize {
        self.inserted + self.rebuilt
    }
}

impl JsTsResolver {
    /// Incrementally bring the resolver in sync with the workspace.
    ///
    /// Only configs whose `ContentHash` differs from the cached entry
    /// are re-parsed; unchanged configs reuse the previously derived
    /// `JsTsPathMapping` / `JsTsPackage` state. The flat lookup vectors
    /// are re-aggregated only when at least one entry was inserted,
    /// rebuilt, or removed.
    pub(crate) fn update_from_files(
        &mut self,
        files: &HashMap<FileId, FileRecord>,
    ) -> JsTsResolverUpdate {
        let mut update = JsTsResolverUpdate::default();

        let mut tsconfig_ids: HashSet<&FileId> = HashSet::new();
        let mut package_ids: HashSet<&FileId> = HashSet::new();
        for (file_id, file) in files {
            if file.relative_path.ends_with("tsconfig.json") {
                tsconfig_ids.insert(file_id);
            } else if file.relative_path.ends_with("package.json") {
                package_ids.insert(file_id);
            }
        }

        let drop_tsconfigs: Vec<FileId> = self
            .tsconfig_entries
            .keys()
            .filter(|id| !tsconfig_ids.contains(id))
            .cloned()
            .collect();
        for id in drop_tsconfigs {
            self.tsconfig_entries.remove(&id);
            update.removed += 1;
        }
        let drop_packages: Vec<FileId> = self
            .package_entries
            .keys()
            .filter(|id| !package_ids.contains(id))
            .cloned()
            .collect();
        for id in drop_packages {
            self.package_entries.remove(&id);
            update.removed += 1;
        }

        for id in tsconfig_ids {
            let file = &files[id];
            match self.tsconfig_entries.get(id) {
                Some(entry) if entry.hash == file.hash => update.reused += 1,
                Some(_) => {
                    let mappings = parse_tsconfig_mappings(file);
                    self.tsconfig_entries.insert(
                        id.clone(),
                        JsTsTsconfigEntry {
                            hash: file.hash.clone(),
                            relative_path: file.relative_path.clone(),
                            mappings,
                        },
                    );
                    update.rebuilt += 1;
                }
                None => {
                    let mappings = parse_tsconfig_mappings(file);
                    self.tsconfig_entries.insert(
                        id.clone(),
                        JsTsTsconfigEntry {
                            hash: file.hash.clone(),
                            relative_path: file.relative_path.clone(),
                            mappings,
                        },
                    );
                    update.inserted += 1;
                }
            }
        }
        for id in package_ids {
            let file = &files[id];
            match self.package_entries.get(id) {
                Some(entry) if entry.hash == file.hash => update.reused += 1,
                Some(_) => {
                    let package = parse_package_entry(file);
                    self.package_entries.insert(
                        id.clone(),
                        JsTsPackageEntry {
                            hash: file.hash.clone(),
                            relative_path: file.relative_path.clone(),
                            package,
                        },
                    );
                    update.rebuilt += 1;
                }
                None => {
                    let package = parse_package_entry(file);
                    self.package_entries.insert(
                        id.clone(),
                        JsTsPackageEntry {
                            hash: file.hash.clone(),
                            relative_path: file.relative_path.clone(),
                            package,
                        },
                    );
                    update.inserted += 1;
                }
            }
        }

        if update.changed() {
            self.rebuild_flat_views();
        }

        update
    }

    fn rebuild_flat_views(&mut self) {
        let mut tsconfigs: Vec<&JsTsTsconfigEntry> = self.tsconfig_entries.values().collect();
        tsconfigs.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
        self.path_mappings = tsconfigs
            .into_iter()
            .flat_map(|entry| entry.mappings.iter().cloned())
            .collect();

        let mut packages: Vec<&JsTsPackageEntry> = self.package_entries.values().collect();
        packages.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
        self.packages = packages
            .into_iter()
            .filter_map(|entry| entry.package.clone())
            .collect();
    }

    fn module_candidates(
        &self,
        module: &str,
        import_file: Option<&FileRecord>,
    ) -> BTreeSet<String> {
        let mut candidates = BTreeSet::new();
        if module.starts_with('.') {
            if let Some(import_file) = import_file {
                let base = parent_dir_string(&import_file.relative_path);
                insert_js_ts_module_variants(&mut candidates, &join_module_path(&base, module));
            }
            return candidates;
        }

        insert_js_ts_module_variants(&mut candidates, module);

        for mapping in &self.path_mappings {
            let Some(star) = match_js_ts_path_pattern(&mapping.pattern, module) else {
                continue;
            };
            for target in &mapping.targets {
                let replaced = target.replace('*', &star);
                let with_base = mapping
                    .base_url
                    .as_deref()
                    .map(|base| join_module_path(base, &replaced))
                    .unwrap_or_else(|| join_module_path(&mapping.config_dir, &replaced));
                insert_js_ts_module_variants(&mut candidates, &with_base);
            }
        }

        for package in &self.packages {
            let Some(subpath) = js_ts_package_subpath(&package.name, module) else {
                continue;
            };
            let package_subpath = subpath.unwrap_or_default();
            if package_subpath.is_empty() {
                insert_js_ts_module_variants(&mut candidates, &package.root);
                insert_js_ts_module_variants(
                    &mut candidates,
                    &join_module_path(&package.root, "src"),
                );
                insert_js_ts_module_variants(
                    &mut candidates,
                    &join_module_path(&package.root, "index"),
                );
                insert_js_ts_module_variants(
                    &mut candidates,
                    &join_module_path(&package.root, "src/index"),
                );
                for entry in &package.main_entries {
                    insert_js_ts_module_variants(
                        &mut candidates,
                        &join_module_path(&package.root, entry),
                    );
                }
            } else {
                insert_js_ts_module_variants(
                    &mut candidates,
                    &join_module_path(&package.root, &package_subpath),
                );
                insert_js_ts_module_variants(
                    &mut candidates,
                    &join_module_path(&join_module_path(&package.root, "src"), &package_subpath),
                );
            }
            let export_key = if package_subpath.is_empty() {
                ".".to_string()
            } else {
                format!("./{package_subpath}")
            };
            for (_, target) in package.exports.iter().filter(|(key, _)| key == &export_key) {
                insert_js_ts_module_variants(
                    &mut candidates,
                    &join_module_path(&package.root, target),
                );
            }
        }

        candidates
    }
}

fn parse_tsconfig_mappings(file: &FileRecord) -> Vec<JsTsPathMapping> {
    let Ok(raw) = std::fs::read_to_string(&file.path) else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Vec::new();
    };
    let Some(options) = json
        .get("compilerOptions")
        .and_then(|value| value.as_object())
    else {
        return Vec::new();
    };
    let config_dir = parent_dir_string(&file.relative_path);
    let base_url = options
        .get("baseUrl")
        .and_then(|value| value.as_str())
        .map(|value| js_ts_normalize_module_path(&join_module_path(&config_dir, value)));
    let mut mappings = Vec::new();
    if let Some(paths) = options.get("paths").and_then(|value| value.as_object()) {
        for (pattern, targets) in paths {
            let targets = targets
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|value| value.as_str())
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            if !targets.is_empty() {
                mappings.push(JsTsPathMapping {
                    config_dir: config_dir.clone(),
                    base_url: base_url.clone(),
                    pattern: pattern.clone(),
                    targets,
                });
            }
        }
    }
    if let Some(base_url) = base_url {
        mappings.push(JsTsPathMapping {
            config_dir,
            base_url: None,
            pattern: "*".to_string(),
            targets: vec![format!("{base_url}/*")],
        });
    }
    mappings
}

fn parse_package_entry(file: &FileRecord) -> Option<JsTsPackage> {
    let raw = std::fs::read_to_string(&file.path).ok()?;
    let json = serde_json::from_str::<serde_json::Value>(&raw).ok()?;
    let name = json.get("name").and_then(|value| value.as_str())?;
    let root = parent_dir_string(&file.relative_path);
    let mut main_entries = Vec::new();
    for field in ["types", "typings", "module", "main"] {
        if let Some(value) = json.get(field).and_then(|value| value.as_str()) {
            main_entries.push(value.to_string());
        }
    }
    let mut exports = Vec::new();
    if let Some(value) = json.get("exports") {
        collect_js_ts_exports(".", value, &mut exports);
    }
    Some(JsTsPackage {
        root,
        name: name.to_string(),
        exports,
        main_entries,
    })
}

pub(crate) fn collect_js_ts_exports(
    key: &str,
    value: &serde_json::Value,
    out: &mut Vec<(String, String)>,
) {
    if let Some(target) = value.as_str() {
        out.push((key.to_string(), target.to_string()));
        return;
    }
    let Some(object) = value.as_object() else {
        return;
    };
    if key == "." {
        for (child_key, child_value) in object {
            if child_key == "." || child_key.starts_with("./") {
                collect_js_ts_exports(child_key, child_value, out);
            }
        }
    }
    for preferred in ["types", "import", "require", "default"] {
        if let Some(child) = object.get(preferred) {
            collect_js_ts_exports(key, child, out);
        }
    }
}

pub(crate) fn js_ts_import_module_part(import: &ParsedImport) -> Option<&str> {
    let path = if import.is_glob {
        import.path.strip_suffix(".*").unwrap_or(&import.path)
    } else {
        import
            .path
            .rsplit_once('.')
            .map(|(module, _)| module)
            .unwrap_or(&import.path)
    };
    Some(path).filter(|path| !path.is_empty())
}

pub(crate) fn js_ts_package_subpath(package_name: &str, module: &str) -> Option<Option<String>> {
    if module == package_name {
        return Some(None);
    }
    module
        .strip_prefix(package_name)
        .and_then(|rest| rest.strip_prefix('/'))
        .map(|rest| Some(rest.to_string()))
}

pub(crate) fn match_js_ts_path_pattern(pattern: &str, module: &str) -> Option<String> {
    let Some((prefix, suffix)) = pattern.split_once('*') else {
        return (pattern == module).then(String::new);
    };
    module
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_suffix(suffix))
        .map(ToString::to_string)
}

pub(crate) fn insert_js_ts_module_variants(candidates: &mut BTreeSet<String>, path: &str) {
    let normalized = js_ts_module_path_for_file(path);
    if normalized.is_empty() {
        return;
    }
    candidates.insert(normalized.clone());
    if !normalized.ends_with("/index") {
        candidates.insert(format!("{normalized}/index"));
    }
}

pub(crate) fn js_ts_file_module_variants(path: &str) -> BTreeSet<String> {
    let mut variants = BTreeSet::new();
    insert_js_ts_module_variants(&mut variants, path);
    variants
}

pub(crate) fn js_ts_module_path_for_file(path: &str) -> String {
    let without_ext = path
        .trim_end_matches(".jsx")
        .trim_end_matches(".tsx")
        .trim_end_matches(".mjs")
        .trim_end_matches(".cjs")
        .trim_end_matches(".mts")
        .trim_end_matches(".cts")
        .trim_end_matches(".js")
        .trim_end_matches(".ts")
        .trim_end_matches(".d");
    let normalized = js_ts_normalize_module_path(without_ext);
    normalized
        .strip_suffix("/index")
        .unwrap_or(&normalized)
        .to_string()
}

pub(crate) fn parent_dir_string(path: &str) -> String {
    path.rsplit_once('/')
        .map(|(dir, _)| dir.to_string())
        .unwrap_or_default()
}

pub(crate) fn join_module_path(base: &str, child: &str) -> String {
    if base.is_empty() {
        child.to_string()
    } else if child.is_empty() {
        base.to_string()
    } else {
        format!("{base}/{child}")
    }
}

pub(crate) fn js_ts_normalize_module_path(path: &str) -> String {
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}
