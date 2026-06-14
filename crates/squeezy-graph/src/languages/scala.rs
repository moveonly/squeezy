use crate::languages::java::symbol_is_top_level_for_imports;
use crate::*;

const SCALA_IMPORT_GIVEN_ALIAS: &str = "__scala_import_given__";

impl SemanticGraph {
    pub(crate) fn scala_package_for_file(&self, file_id: &FileId) -> Option<Vec<String>> {
        self.scala_package_by_file.get(file_id).cloned()
    }

    pub(crate) fn scala_import_matches_symbol(
        &self,
        import: &ParsedImport,
        symbol: &GraphSymbol,
    ) -> bool {
        if crate::is_package_marker_alias(import.alias.as_deref()) {
            return false;
        }
        let mut import_segments = path_segments(&import.path);
        let last_is_glob = import_segments
            .last()
            .map(|segment| segment == "*")
            .unwrap_or(false);
        if last_is_glob {
            import_segments.pop();
        }
        let Some(package) = self.scala_package_for_file(&symbol.file_id) else {
            return false;
        };

        // Wildcard import of a package (`import a.b.*`) or a given-only
        // wildcard (`import a.b.given`). Matches top-level symbols in that
        // package and members of a companion-object scope expressed via the
        // owner-class chain (mirrors Java glob behavior).
        if import.is_glob {
            return (import_segments == package && symbol_is_top_level_for_imports(symbol))
                || self.scala_symbol_owner_path(symbol) == import_segments;
        }

        // Plain or given-named import (e.g. `import a.b.C` or
        // `import a.b.{given Ordering[Int]}`). After popping the leaf the
        // remainder must equal the symbol's package, and the leaf must match
        // the symbol name (possibly under its renamed alias).
        let leaf = last_path_segment(&import.path);
        let target_name = match import.alias.as_deref() {
            Some(SCALA_IMPORT_GIVEN_ALIAS) | None => leaf.clone(),
            Some(alias) => alias.to_string(),
        };
        // The alias path is the consumer-facing name; the symbol on the
        // exporter side keeps its declared name in `leaf`.
        if leaf != symbol.name && target_name != symbol.name {
            return false;
        }
        import_segments.pop();
        let owner_path = self.scala_symbol_owner_path(symbol);
        owner_path == import_segments
    }

    pub(crate) fn scala_symbol_owner_path(&self, symbol: &GraphSymbol) -> Vec<String> {
        // Walk the parent chain collecting class/object/trait/enum names so a
        // member-import path like `a.b.Outer` can match a nested type.
        let mut path = self
            .scala_package_for_file(&symbol.file_id)
            .unwrap_or_default();
        let mut chain = Vec::new();
        let mut parent_id = symbol.parent_id.as_ref();
        while let Some(id) = parent_id {
            let Some(parent) = self.symbols.get(id) else {
                break;
            };
            if matches!(
                parent.kind,
                SymbolKind::Class
                    | SymbolKind::Trait
                    | SymbolKind::Enum
                    | SymbolKind::Struct
                    | SymbolKind::Module
            ) {
                chain.push(parent.name.clone());
            }
            parent_id = parent.parent_id.as_ref();
        }
        chain.reverse();
        path.extend(chain);
        path
    }

    /// Look up the companion object of a Scala class/trait/enum/struct by
    /// scanning sibling top-level symbols in the same file. Returns the
    /// `Class` symbol carrying the `scala:object` attribute matching `name`.
    pub(crate) fn scala_companion_object_for(
        &self,
        class_symbol: &GraphSymbol,
    ) -> Option<SymbolId> {
        let candidates = self.symbols_by_name.get(&class_symbol.name)?;
        for candidate_id in candidates {
            let Some(candidate) = self.symbols.get(candidate_id) else {
                continue;
            };
            if candidate.file_id != class_symbol.file_id {
                continue;
            }
            if candidate.id == class_symbol.id {
                continue;
            }
            if candidate.kind != SymbolKind::Class {
                continue;
            }
            if candidate
                .attributes
                .iter()
                .any(|attribute| attribute == "scala:object")
            {
                return Some(candidate.id.clone());
            }
        }
        None
    }

    /// Resolve an unqualified call to a method on the receiver's companion
    /// object when the call expression's receiver names a sibling class. The
    /// resolver uses this to land `Greeter.default` style calls where
    /// `Greeter` is declared as `class Greeter` paired with `object Greeter`.
    pub(crate) fn scala_companion_method(
        &self,
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        let receiver = call.receiver.as_deref()?;
        let caller = self.symbols.get(caller_id)?;
        let caller_file = self.files.get(&caller.file_id)?;
        if caller_file.language != LanguageKind::Scala {
            return None;
        }
        let caller_package = self.scala_package_for_file(&caller.file_id);
        let class_candidates = self.symbols_by_name.get(receiver)?;
        let matches = class_candidates
            .iter()
            .filter_map(|class_id| self.symbols.get(class_id))
            .filter(|class_symbol| {
                matches!(
                    class_symbol.kind,
                    SymbolKind::Class | SymbolKind::Trait | SymbolKind::Enum | SymbolKind::Struct
                )
            })
            // Only consider receiver classes that are actually in scope at the
            // call site: declared in the caller's file, in the caller's
            // package, or brought in by a matching import. This mirrors
            // `scala_top_level_def` / `kotlin_companion_member_call` and stops
            // an unrelated same-named class in another package from binding.
            .filter(|class_symbol| {
                class_symbol.file_id == caller.file_id
                    || self.scala_package_for_file(&class_symbol.file_id) == caller_package
                    || self
                        .imports_for_file(&caller.file_id)
                        .any(|import| self.scala_import_matches_symbol(import, class_symbol))
            })
            .filter_map(|class_symbol| self.scala_companion_object_for(class_symbol))
            .flat_map(|object_id| {
                self.children_by_parent
                    .get(&object_id)
                    .into_iter()
                    .flatten()
                    .filter_map(|child_id| self.symbols.get(child_id))
                    .filter(|symbol| {
                        matches!(
                            symbol.kind,
                            SymbolKind::Method | SymbolKind::Function | SymbolKind::Const
                        ) && symbol.name == call.name
                    })
                    .map(|symbol| symbol.id.clone())
                    .collect::<Vec<_>>()
            });
        single_symbol(matches)
    }

    /// Resolve an extension-method invocation by matching the receiver's
    /// language identity against the extension function's declared receiver
    /// type. Tree-sitter encodes the receiver type in `language_identity`
    /// when the extension target is monomorphic.
    pub(crate) fn scala_extension_method(
        &self,
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        let caller = self.symbols.get(caller_id)?;
        let caller_file = self.files.get(&caller.file_id)?;
        if caller_file.language != LanguageKind::Scala {
            return None;
        }
        let candidates = self.symbols_by_name.get(&call.name)?;
        // Require a positive receiver-type match before binding. When the
        // receiver type cannot be inferred we decline rather than guessing,
        // mirroring Swift's `swift_extension_receiver_method` and Kotlin's
        // `kotlin_extension_function_call`. Otherwise any unrelated
        // `extension (x: T) def foo` would hijack a regular `obj.foo()`.
        let receiver_type = self.scala_receiver_type_name(caller_id, call)?;
        let matches = candidates
            .iter()
            .filter_map(|candidate_id| self.symbols.get(candidate_id))
            .filter(|candidate| {
                candidate
                    .attributes
                    .iter()
                    .any(|attribute| attribute == "scala:extension")
                    && candidate.language_identity.as_deref() == Some(receiver_type.as_str())
            })
            .map(|candidate| candidate.id.clone());
        // Use `single_symbol` so ambiguous matches do not silently bind to
        // the first symbol by insertion order.
        single_symbol(matches)
    }

    /// Top-level-def lookup: a Scala 3 `def`/`val`/`given` declared at file
    /// scope can be called unqualified from any file that imports the same
    /// package (analogous to Java static imports). Returns the matching
    /// symbol when the caller's file shares the package and the call name
    /// matches the top-level symbol's name.
    pub(crate) fn scala_top_level_def(
        &self,
        candidates: &[SymbolId],
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        if call.receiver.is_some() {
            return None;
        }
        let caller = self.symbols.get(caller_id)?;
        let caller_file = self.files.get(&caller.file_id)?;
        if caller_file.language != LanguageKind::Scala {
            return None;
        }
        let caller_package = self.scala_package_for_file(&caller_file.id);
        for candidate_id in candidates {
            let Some(candidate) = self.symbols.get(candidate_id) else {
                continue;
            };
            if !matches!(
                candidate.kind,
                SymbolKind::Function | SymbolKind::Const | SymbolKind::Static
            ) {
                continue;
            }
            if !symbol_is_top_level_for_imports(candidate) {
                continue;
            }
            if candidate.name != call.name {
                continue;
            }
            let candidate_package = self.scala_package_for_file(&candidate.file_id);
            if candidate_package == caller_package {
                return Some(candidate.id.clone());
            }
        }
        None
    }

    /// Infer the *type name* of an extension-call receiver so
    /// `scala_extension_method` can match it against the extension function's
    /// declared receiver type (recorded in `language_identity`). The order of
    /// preference, from most to least certain, is:
    ///
    /// 1. A literal receiver (`"x".foo`, `42.foo`, `true.foo`) -> the
    ///    built-in type the extractor stores for that extension.
    /// 2. `this` -> the caller's enclosing class/trait/enum/struct name. (Bare
    ///    `super` is declined: the supertype is not recoverable here without
    ///    real type resolution, and guessing the subclass name would be
    ///    unsound.)
    /// 3. A `val`/`var`/`field`/constructor-param named like the receiver on
    ///    the caller's enclosing type whose `type:` attribute was recorded by
    ///    the extractor -> that declared type.
    ///
    /// Returns `None` when none of these apply so the (correctly hardened)
    /// matcher declines rather than guessing — preserving its soundness.
    pub(crate) fn scala_receiver_type_name(
        &self,
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Option<String> {
        let receiver = call.receiver.as_deref()?.trim();
        if receiver.is_empty() {
            return None;
        }

        // 1. Literal receivers map straight to their built-in type.
        if let Some(literal) = scala_literal_receiver_type(receiver) {
            return Some(literal.to_string());
        }

        // A receiver carrying spaces or call/index syntax is a non-trivial
        // expression we cannot type without real inference; decline.
        if receiver.contains(' ')
            || receiver.contains('(')
            || receiver.contains('[')
            || receiver.contains('.')
        {
            return None;
        }

        // 2. `this` resolves to the enclosing type's own name.
        if receiver == "this" {
            let owner_id = self.scala_owner_type_for_caller(caller_id)?;
            return self.symbols.get(&owner_id).map(|owner| owner.name.clone());
        }
        if receiver == "super" {
            return None;
        }

        // 3. A typed binding visible on the enclosing type. The extractor
        //    records constructor params and `val`/`var` members as `Field`
        //    children carrying a `type:T` attribute.
        self.scala_binding_type_name(caller_id, receiver)
    }

    /// Look up the declared type of a `val`/`var`/field/constructor-param named
    /// `receiver` on the caller's enclosing type, returning the `type:T`
    /// attribute value recorded by the extractor. Mirrors
    /// `swift_receiver_type_name` / `kotlin_receiver_type_name`.
    fn scala_binding_type_name(&self, caller_id: &SymbolId, receiver: &str) -> Option<String> {
        let owner_id = self.scala_owner_type_for_caller(caller_id)?;
        let binding = self
            .children_by_parent
            .get(&owner_id)?
            .iter()
            .find_map(|child_id| {
                self.symbols.get(child_id).filter(|symbol| {
                    matches!(
                        symbol.kind,
                        SymbolKind::Field | SymbolKind::Const | SymbolKind::Static
                    ) && symbol.name == receiver
                })
            })?;
        binding
            .attributes
            .iter()
            .find_map(|attribute| attribute.strip_prefix("type:"))
            .map(str::to_string)
    }

    /// Walk the parent chain from the caller to the nearest enclosing
    /// class/trait/enum/struct symbol. Mirrors `swift_owner_type_for_caller`
    /// and `kotlin_class_for_caller`.
    fn scala_owner_type_for_caller(&self, caller_id: &SymbolId) -> Option<SymbolId> {
        let caller = self.symbols.get(caller_id)?;
        if matches!(
            caller.kind,
            SymbolKind::Class | SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Trait
        ) {
            return Some(caller.id.clone());
        }
        let mut current = caller.parent_id.clone();
        while let Some(id) = current {
            let symbol = self.symbols.get(&id)?;
            if matches!(
                symbol.kind,
                SymbolKind::Class | SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Trait
            ) {
                return Some(symbol.id.clone());
            }
            current = symbol.parent_id.clone();
        }
        None
    }
}

/// Map a literal-shaped extension-call receiver to the Scala built-in type
/// the extractor records as `language_identity` for that extension. Mirrors
/// `kotlin_literal_receiver_type`: the receiver text arrives raw from
/// tree-sitter (e.g. `"hello"`, `s"…"`, `42`, `1.5`, `true`, `'x'`). Returns
/// `None` when the receiver is not a recognizable literal so the caller can
/// fall back to the typed-binding inference path rather than guessing.
fn scala_literal_receiver_type(receiver: &str) -> Option<&'static str> {
    let trimmed = receiver.trim();
    if trimmed.is_empty() {
        return None;
    }
    // String literals: plain `"…"`, triple-quoted, and interpolators
    // (`s"…"`, `f"…"`, `raw"…"`). Anything that ends as a string literal is a
    // `String`. Interpolator prefixes are alphanumeric identifiers followed
    // by a `"`.
    if trimmed.starts_with('"') {
        return Some("String");
    }
    if let Some(quote) = trimmed.find('"')
        && trimmed[..quote].chars().all(|c| c.is_ascii_alphanumeric())
        && !trimmed[..quote].is_empty()
    {
        return Some("String");
    }
    // Character literal: `'x'`.
    if trimmed.starts_with('\'') && trimmed.ends_with('\'') && trimmed.len() >= 2 {
        return Some("Char");
    }
    // Boolean literals.
    if trimmed == "true" || trimmed == "false" {
        return Some("Boolean");
    }
    // Numeric literals. A trailing type suffix wins; otherwise a `.` makes it
    // a `Double`, else `Int`.
    let first = trimmed.chars().next()?;
    if first.is_ascii_digit() || (first == '-' && trimmed.len() > 1) {
        let last = trimmed.chars().last()?;
        if last == 'L' || last == 'l' {
            return Some("Long");
        }
        if last == 'f' || last == 'F' {
            return Some("Float");
        }
        if last == 'd' || last == 'D' || trimmed.contains('.') {
            return Some("Double");
        }
        return Some("Int");
    }
    None
}
