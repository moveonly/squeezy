use crate::*;

impl SemanticGraph {
    pub(crate) fn caller_is_python(&self, caller_id: &SymbolId) -> bool {
        self.symbols
            .get(caller_id)
            .and_then(|caller| self.files.get(&caller.file_id))
            .map(|file| file.language == squeezy_core::LanguageKind::Python)
            .unwrap_or(false)
    }

    pub(crate) fn inherited_python_method(
        &self,
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Option<SymbolId> {
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

    pub(crate) fn python_receiver_alias_method(
        &self,
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        let receiver = call.receiver.as_deref()?;
        if matches!(receiver, "self" | "cls") {
            return None;
        }
        if !self.caller_is_python(caller_id) {
            return None;
        }
        let caller = self.symbols.get(caller_id)?;
        let class = self.python_class_for_alias(caller, receiver, Some(call.span.start_byte))?;
        self.python_method_on_class(&class.id, &call.name)
    }

    /// ORM manager dispatch (Django / SQLAlchemy-style): resolve a call whose
    /// receiver is a model class's manager attribute back to a method on that
    /// model class. For `User.objects.filter(...)` tree-sitter records the call
    /// as `name=filter`, `receiver="User.objects"`; we strip the allow-listed
    /// manager segment (`objects` / `query`), resolve the leading `User` to a
    /// model class in scope, and look the method up on that class (or its
    /// bases). Precision is kept high by the manager allow-list and by reusing
    /// the scope-aware class resolver — we never bind to an arbitrary same-named
    /// method, and decline outright when the receiver is not a recognised
    /// `<Class>.<manager>` chain or the head does not resolve to a class.
    pub(crate) fn python_manager_dispatch_call(
        &self,
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        if !self.caller_is_python(caller_id) {
            return None;
        }
        let receiver = call.receiver.as_deref()?;
        // Split the receiver into `<head>.<manager>` and require the trailing
        // segment to be an allow-listed manager attribute. A bare `objects`
        // with no class head (`objects.filter()`) is too ambiguous to bind.
        let (head, manager) = receiver.rsplit_once('.')?;
        if !is_python_manager_attribute(manager) {
            return None;
        }
        let head = head.trim();
        if head.is_empty() {
            return None;
        }
        let class_name = last_path_segment(head);
        let caller = self.symbols.get(caller_id)?;

        // Resolve `<head>` to a model class. `self`/`cls` managers point at the
        // enclosing class; an imported/aliased name resolves through the same
        // scope-aware path the rest of the Python resolver uses; otherwise fall
        // back to a same-file (or unambiguous workspace) class declaration.
        let class_id = if matches!(head, "self" | "cls") {
            self.python_class_for_caller(caller_id)?
        } else if let Some(class) =
            self.python_class_for_alias(caller, &class_name, Some(call.span.start_byte))
        {
            class.id
        } else {
            single_symbol(
                self.python_class_candidates_for_name_in_file(&caller.file_id, &class_name)
                    .into_iter(),
            )?
        };

        self.python_method_on_class_or_bases(&class_id, &call.name)
    }

    /// Framework route / registry decorator dispatch.
    ///
    /// A handler decorated with an allow-listed registry decorator
    /// (`@app.route`, `@router.get`, `@blueprint.post`, …) is invoked by the
    /// framework, never by name, so the call graph would otherwise show the
    /// handler as an unreferenced leaf. The decorator's call-site is the
    /// registrar (`app` / `router` / a registry class). We resolve that
    /// registrar to a workspace symbol and return it as the edge source so the
    /// orchestrator can record a `registrar -> handler` edge.
    ///
    /// Precision is held high by two gates:
    ///   * the handler must carry the parser's `framework:web-route` marker (or
    ///     a `route:` attribute), i.e. the decorator already matched the
    ///     parser's framework allow-list; and
    ///   * the registrar must resolve to a *single* workspace symbol — a
    ///     registry class brought into scope (`@MyRouter.get`, where `MyRouter`
    ///     is a class), or, for a bare decorator (`@register`), a unique
    ///     workspace function/class of that name.
    ///
    /// We deliberately decline the common instance-registrar case
    /// (`router = APIRouter()` then `@router.get(...)`): the parser records no
    /// symbol for the `router` variable, so there is no resolvable call-site and
    /// inventing one would be a guess. Returning `None` keeps the heuristic
    /// allow-list-driven and bounded.
    pub(crate) fn python_route_decorator_registrar(
        &self,
        handler_id: &SymbolId,
    ) -> Option<SymbolId> {
        let handler = self.symbols.get(handler_id)?;
        if !matches!(handler.kind, SymbolKind::Function | SymbolKind::Method) {
            return None;
        }
        if !self.caller_is_python(handler_id) {
            return None;
        }
        // Gate on the parser's framework marker so we only act on decorators the
        // parser already classified as a web route via its own allow-list.
        let is_route = handler
            .attributes
            .iter()
            .any(|attribute| attribute == "framework:web-route" || attribute.starts_with("route:"));
        if !is_route {
            return None;
        }

        // Find the raw decorator attribute (the verbatim `@…` text the parser
        // preserves) whose leaf is an allow-listed route/registry verb.
        let target = handler
            .attributes
            .iter()
            .filter_map(|attribute| python_decorator_target(attribute))
            .find(|target| is_python_route_decorator_leaf(last_path_segment(target)))?;

        match target.rsplit_once('.') {
            // `@Receiver.verb` — only resolvable when `Receiver` is a registry
            // *class* in scope. Dispatch the verb to a method on that class so
            // the edge points at real, workspace-defined code.
            Some((receiver, verb)) => {
                let receiver = receiver.trim();
                if receiver.is_empty() || matches!(receiver, "self" | "cls") {
                    return None;
                }
                let receiver_name = last_path_segment(receiver);
                let class_id = single_symbol(
                    self.python_class_candidates_for_name_in_file(&handler.file_id, &receiver_name)
                        .into_iter(),
                )?;
                self.python_method_on_class_or_bases(&class_id, verb)
            }
            // Bare `@register` — resolve to a unique workspace function/class.
            None => {
                let name = target.trim();
                if name.is_empty() {
                    return None;
                }
                single_symbol(self.symbols_by_name_or_scan(name).into_iter().filter(|id| {
                    self.symbols
                        .get(id)
                        .map(|symbol| {
                            matches!(symbol.kind, SymbolKind::Function | SymbolKind::Class)
                        })
                        .unwrap_or(false)
                }))
            }
        }
    }

    pub(crate) fn python_module_qualified_call(
        &self,
        candidates: &[SymbolId],
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        let receiver = call.receiver.as_deref()?;
        if !self.caller_is_python(caller_id) {
            return None;
        }
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

    pub(crate) fn python_class_for_alias(
        &self,
        caller: &GraphSymbol,
        alias: &str,
        before_byte: Option<u32>,
    ) -> Option<GraphSymbol> {
        self.python_class_for_alias_in_scope(caller, alias, before_byte, 0)
    }

    pub(crate) fn python_class_for_alias_in_scope(
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

    pub(crate) fn python_receiver_module_paths(
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
            .imports_for_file(&caller.file_id)
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

    pub(crate) fn python_class_for_caller(&self, caller_id: &SymbolId) -> Option<SymbolId> {
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

    pub(crate) fn python_method_in_bases(
        &self,
        class_id: &SymbolId,
        method_name: &str,
        depth: usize,
    ) -> Option<SymbolId> {
        let mut visited = std::collections::HashSet::new();
        visited.insert(class_id.clone());
        self.python_method_in_bases_visited(class_id, method_name, depth, &mut visited)
    }

    /// Walk the `base:` attribute chain looking for `method_name`, deduping
    /// ancestors via a visited-set. Each `base:` name resolves to a fan-out of
    /// global class candidates, and on a wide diamond hierarchy with many
    /// identically-named classes (a framework's `Model`/`Controller`/`Builder`
    /// trees) the recursion re-expands shared ancestors along every path —
    /// combinatorial blow-up that dominated the cold build's call resolution.
    /// The visited-set only prunes redundant re-traversal: an ancestor's
    /// subtree is identical no matter how it is reached, so the first match in
    /// declaration order is unchanged and resolution stays correct.
    fn python_method_in_bases_visited(
        &self,
        class_id: &SymbolId,
        method_name: &str,
        depth: usize,
        visited: &mut std::collections::HashSet<SymbolId>,
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
                if !visited.insert(base_id.clone()) {
                    continue;
                }
                if let Some(method) = self.python_method_on_class(&base_id, method_name) {
                    return Some(method);
                }
                if let Some(method) =
                    self.python_method_in_bases_visited(&base_id, method_name, depth + 1, visited)
                {
                    return Some(method);
                }
            }
        }
        None
    }

    /// Resolve a `base:` ancestor name to Python class-like symbols, scoping to
    /// the file the subclass lives in: same-file declarations and any class
    /// brought into scope by a matching import (`from pkg import Base`, or an
    /// aliased `import pkg.Base as B`). This prevents `Child(Base)` from binding
    /// to an unrelated same-named `Base` defined in another module. Only when
    /// the leaf name resolves to no in-scope candidate at all do we fall back to
    /// a global name match, and even then only when that match is unambiguous
    /// (a single class-like symbol workspace-wide) — mirroring the scope-aware
    /// JS/TS resolver.
    pub(crate) fn python_class_candidates_for_name_in_file(
        &self,
        file_id: &FileId,
        name: &str,
    ) -> Vec<SymbolId> {
        let direct_name = last_path_segment(name);
        let global = self
            .symbols_by_name_or_scan(&direct_name)
            .into_iter()
            .filter_map(|id| self.symbols.get(&id))
            .filter(|symbol| is_class_like_kind(symbol.kind))
            .collect::<Vec<_>>();

        let mut class_ids = global
            .iter()
            .filter(|symbol| {
                symbol.file_id == *file_id || self.python_class_is_in_scope(file_id, name, symbol)
            })
            .map(|symbol| symbol.id.clone())
            .collect::<Vec<_>>();

        // No in-scope declaration: fall back to a global name match, but only
        // when it is unambiguous so we never silently bind to one of several
        // unrelated same-named classes.
        if class_ids.is_empty() && global.len() == 1 {
            class_ids.push(global[0].id.clone());
        }

        class_ids.sort_by(|left, right| left.0.cmp(&right.0));
        class_ids.dedup();
        class_ids
    }

    /// True when an import visible in `file_id` brings `symbol` into scope under
    /// `name` — either as an explicit alias (`import pkg.Base as name`) or by the
    /// import path's leaf (`from pkg import Base`), with the import's module
    /// suffix matching the symbol's file (via [`Self::import_matches_symbol`]).
    fn python_class_is_in_scope(&self, file_id: &FileId, name: &str, symbol: &GraphSymbol) -> bool {
        self.imports_for_file(file_id)
            .filter(|import| !is_package_marker(import))
            .filter(|import| {
                import
                    .alias
                    .as_deref()
                    .map(|alias| alias == name)
                    .unwrap_or_else(|| last_path_segment_str(&import.path) == name)
            })
            .any(|import| self.import_matches_symbol(import, symbol))
    }

    pub(crate) fn python_method_on_class(
        &self,
        class_id: &SymbolId,
        method_name: &str,
    ) -> Option<SymbolId> {
        single_symbol(
            self.children_by_parent
                .get(class_id)?
                .iter()
                .filter_map(|child_id| self.symbols.get(child_id))
                .filter(|symbol| symbol.kind == SymbolKind::Method && symbol.name == method_name)
                .map(|symbol| symbol.id.clone()),
        )
    }

    pub(crate) fn python_method_on_class_or_bases(
        &self,
        class_id: &SymbolId,
        method_name: &str,
    ) -> Option<SymbolId> {
        self.python_method_on_class(class_id, method_name)
            .or_else(|| self.python_method_in_bases(class_id, method_name, 0))
    }

    /// For C/C++ `bar()` calls from inside a method of class `Foo`, prefer
    /// `Foo::bar` over a same-name free function in the same file. The Rust
    /// path uses `same_impl_method` for receiver-less self/this calls
    /// (`ParsedCallKind::Method` with `self`/`this`), but tree-sitter-cpp
    /// classifies receiver-less calls as `Direct`, so without this lookup
    /// the call resolver falls through to `same_file_direct_call` (which
    /// filters out `Method`) and the call becomes `CandidateSet`.
    pub(crate) fn python_property_reference_matches(
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
}

/// Allow-listed ORM "manager" attributes that stand in front of a model class
/// in a query expression (`Model.objects.filter(...)`,
/// `Model.query.get(...)`). Kept deliberately small so the manager-dispatch
/// heuristic only fires on the well-known Django (`objects`) and
/// Flask-SQLAlchemy (`query`) idioms and never on an arbitrary attribute
/// access that happens to be followed by a method call.
pub(crate) fn is_python_manager_attribute(attribute: &str) -> bool {
    matches!(attribute.trim(), "objects" | "query")
}

/// Extract the dotted callee path from a raw Python decorator attribute, or
/// `None` when the attribute is not a verbatim `@…` decorator (the parser also
/// stores derived markers like `route:GET` / `framework:web-route` in the same
/// list). `@router.get("/x")` -> `router.get`; `@register` -> `register`.
pub(crate) fn python_decorator_target(attribute: &str) -> Option<String> {
    let trimmed = attribute.trim();
    let body = trimmed.strip_prefix('@')?;
    let callee = body
        .split('(')
        .next()
        .unwrap_or(body)
        .trim()
        .trim_end_matches('.');
    if callee.is_empty() {
        return None;
    }
    Some(callee.to_string())
}

/// Allow-listed route/registry decorator verbs. Matches the HTTP-method and
/// `route`/`add_*_route` decorators recognised by the parser's framework
/// heuristic (Flask `@app.route`, FastAPI/Starlette `@router.get`, etc.) plus a
/// small set of registry verbs, so this resolver only fires on the same idioms.
pub(crate) fn is_python_route_decorator_leaf(leaf: impl AsRef<str>) -> bool {
    matches!(
        leaf.as_ref(),
        "route"
            | "get"
            | "post"
            | "put"
            | "patch"
            | "delete"
            | "options"
            | "head"
            | "websocket"
            | "register"
            | "add_url_rule"
            | "add_api_route"
    )
}

pub(crate) fn python_path_segments(path: &str) -> Vec<String> {
    path.split('.')
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .map(|segment| segment.trim_end_matches(".*").to_string())
        .filter(|segment| !segment.is_empty())
        .collect()
}

pub(crate) fn python_module_path_for_file(path: &str) -> Vec<String> {
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
