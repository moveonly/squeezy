use crate::*;

impl SemanticGraph {
    pub(crate) fn go_import_matches_symbol(
        &self,
        import: &ParsedImport,
        symbol: &GraphSymbol,
    ) -> bool {
        let Some(file) = self.files.get(&symbol.file_id) else {
            return true;
        };
        let import_leaf = import
            .alias
            .as_deref()
            .filter(|alias| *alias != "_")
            .map(str::to_string)
            .unwrap_or_else(|| last_path_segment(&import.path));
        let symbol_package = self
            .packages
            .get(&symbol.file_id)
            .cloned()
            .unwrap_or_else(|| go_package_name_from_path(&file.relative_path));
        import_leaf == symbol_package || last_path_segment(&import.path) == symbol_package
    }

    pub(crate) fn go_package_qualified_call(
        &self,
        candidates: &[SymbolId],
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        let receiver = call.receiver.as_deref()?;
        if receiver.contains('.') || receiver.contains('/') {
            return None;
        }
        let caller = self.symbols.get(caller_id)?;
        if !matches!(
            self.files.get(&caller.file_id).map(|file| file.language),
            Some(squeezy_core::LanguageKind::Go),
        ) {
            return None;
        }
        let imports = self
            .imports_for_file(&caller.file_id)
            .filter(|import| self.import_visible_from_symbol(import, caller))
            .filter(|import| {
                import
                    .alias
                    .as_deref()
                    .filter(|alias| *alias != "_")
                    .map(|alias| alias == receiver)
                    .unwrap_or_else(|| last_path_segment(&import.path) == receiver)
            })
            .collect::<Vec<_>>();
        if imports.is_empty() {
            return None;
        }
        single_symbol(
            candidates
                .iter()
                .filter_map(|id| self.symbols.get(id))
                .filter(|symbol| matches!(symbol.kind, SymbolKind::Function | SymbolKind::Test))
                .filter(|symbol| is_free_function_like(symbol))
                .filter(|symbol| {
                    imports
                        .iter()
                        .any(|import| self.go_import_matches_symbol(import, symbol))
                })
                .map(|symbol| symbol.id.clone()),
        )
    }

    pub(crate) fn caller_is_go(&self, caller_id: &SymbolId) -> bool {
        self.symbols
            .get(caller_id)
            .and_then(|caller| self.files.get(&caller.file_id))
            .map(|file| file.language == squeezy_core::LanguageKind::Go)
            .unwrap_or(false)
    }

    /// Resolve `recv.Method(...)` where `recv` is a local/param/field whose
    /// static type `T` we can infer from the caller's scope, by binding
    /// `call.name` against the methods reparented under type `T`. Go methods are
    /// attached as children of their receiver-type symbol and stamped
    /// `go:receiver:<Type>`, so a single matching value-or-pointer receiver
    /// method in the same package resolves the call. Declines on any ambiguity
    /// (multiple type symbols or multiple matching methods) via `single_symbol`.
    pub(crate) fn go_receiver_method_call(
        &self,
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        if !self.caller_is_go(caller_id) {
            return None;
        }
        let receiver = call.receiver.as_deref()?;
        // A package-qualified or path-y receiver (`pkg.Fn`, `a.b`) is handled by
        // the import-aware path, not type-directed dispatch.
        if is_self_receiver(Some(receiver))
            || receiver.contains('.')
            || receiver.contains('/')
            || !is_go_ident(receiver)
        {
            return None;
        }
        let receiver_type = self.go_receiver_static_type(caller_id, receiver)?;
        let caller = self.symbols.get(caller_id)?;
        let type_id = self.go_type_symbol_in_scope(&caller.file_id, &receiver_type)?;
        self.go_method_on_type(&type_id, &call.name)
    }

    /// Best-effort static type of receiver identifier `receiver` inside a Go
    /// caller. Resolution order: the method's own receiver/typed parameters (in
    /// the signature), then `go-local:<name>:<Type>` attributes stamped by the
    /// parser for typed body locals, then a field of the caller's receiver type.
    pub(crate) fn go_receiver_static_type(
        &self,
        caller_id: &SymbolId,
        receiver: &str,
    ) -> Option<String> {
        let caller = self.symbols.get(caller_id)?;
        // Receiver / typed parameters: `func (g *Greeter) f(other Greeter)` —
        // the token immediately following the receiver name in the signature is
        // its type (with a leading `*` for pointer receivers/params).
        if let Some(ty) = go_type_after_identifier(&caller.signature, receiver) {
            return Some(ty);
        }
        // Parser-stamped body locals (`var x T`, `x := T{...}`, `x := &T{...}`).
        if let Some(ty) = caller
            .attributes
            .iter()
            .filter_map(|attr| attr.strip_prefix("go-local:"))
            .find_map(|attr| attr.strip_prefix(receiver)?.strip_prefix(':'))
        {
            return Some(go_leaf_type_token(ty.trim()).to_string());
        }
        // A field of the caller's enclosing receiver type whose name matches.
        let owner_type = self.go_owner_type_for_caller(caller_id)?;
        let field = self
            .children_by_parent
            .get(&owner_type)
            .into_iter()
            .flatten()
            .filter_map(|child_id| self.symbols.get(child_id))
            .find(|symbol| symbol.kind == SymbolKind::Field && symbol.name == receiver)?;
        // Field symbols carry the full `name Type` declaration text in their
        // signature; the type token follows the field name.
        go_type_after_identifier(&field.signature, receiver)
    }

    /// Walk up `parent_id` to the caller's enclosing Struct/Interface/TypeAlias
    /// (the receiver type a method is reparented under).
    fn go_owner_type_for_caller(&self, caller_id: &SymbolId) -> Option<SymbolId> {
        let mut current = self.symbols.get(caller_id)?;
        loop {
            if matches!(
                current.kind,
                SymbolKind::Struct | SymbolKind::Interface | SymbolKind::TypeAlias
            ) {
                return Some(current.id.clone());
            }
            let parent_id = current.parent_id.as_ref()?;
            current = self.symbols.get(parent_id)?;
        }
    }

    /// Resolve type name `name` to the unique Struct/Interface/TypeAlias symbol
    /// in the caller's package. Go has no cross-file forward declarations to
    /// worry about: a type and its callers share a package, and same-package
    /// files share the directory. We scope to symbols whose package matches the
    /// caller's, falling back to a globally-unique match, and decline on
    /// ambiguity so an unrelated package's same-named type cannot hijack the
    /// edge.
    fn go_type_symbol_in_scope(&self, file_id: &FileId, name: &str) -> Option<SymbolId> {
        let caller_package = self.packages.get(file_id);
        let type_symbols = self
            .symbols_by_name
            .get(name)
            .into_iter()
            .flatten()
            .filter_map(|id| self.symbols.get(id))
            .filter(|symbol| {
                matches!(
                    symbol.kind,
                    SymbolKind::Struct | SymbolKind::Interface | SymbolKind::TypeAlias
                )
            })
            .filter(|symbol| {
                self.files
                    .get(&symbol.file_id)
                    .map(|file| file.language == squeezy_core::LanguageKind::Go)
                    .unwrap_or(false)
            })
            .collect::<Vec<_>>();
        let same_package = type_symbols
            .iter()
            .filter(|symbol| {
                caller_package.is_some() && self.packages.get(&symbol.file_id) == caller_package
            })
            .map(|symbol| symbol.id.clone())
            .collect::<Vec<_>>();
        if let Some(id) = single_symbol(same_package.into_iter()) {
            return Some(id);
        }
        single_symbol(type_symbols.into_iter().map(|symbol| symbol.id.clone()))
    }

    /// Bind a method named `method_name` declared on type `type_id`. Methods are
    /// reparented under their receiver type, so they are this type's children;
    /// value- and pointer-receiver methods are indistinguishable here, which is
    /// exactly what Go's method set rules want for the common case. Declines on
    /// ambiguity (overload-like duplicates) via `single_symbol`.
    fn go_method_on_type(&self, type_id: &SymbolId, method_name: &str) -> Option<SymbolId> {
        single_symbol(
            self.children_by_parent
                .get(type_id)
                .into_iter()
                .flatten()
                .filter_map(|child_id| self.symbols.get(child_id))
                .filter(|symbol| {
                    matches!(symbol.kind, SymbolKind::Method | SymbolKind::Test)
                        && symbol.name == method_name
                })
                .map(|symbol| symbol.id.clone()),
        )
    }
}

/// Given a Go signature/declaration text and an identifier `name`, return the
/// leaf type token that immediately follows `name` (the Go `name Type` order),
/// stripping a leading `*` and a `pkg.`/generic suffix. Returns `None` when the
/// following token is not a plain type identifier (e.g. `name func(...)`).
fn go_type_after_identifier(text: &str, name: &str) -> Option<String> {
    let idx = find_identifier(text, name)?;
    let after = text[idx + name.len()..].trim_start();
    // The receiver/param/field type is the next whitespace-delimited token; for
    // a comma-grouped param list (`a, b T`) we'd see a comma first — bail, as we
    // cannot tell which type binds without fuller parsing.
    let token = after
        .split(|ch: char| ch.is_whitespace() || ch == ')' || ch == ',' || ch == '{')
        .next()
        .unwrap_or_default();
    let leaf = go_leaf_type_token(token);
    is_go_ident(leaf).then(|| leaf.to_string())
}

/// Minimal Go identifier check for resolver-side text (receiver names, type
/// tokens already extracted from parsed signatures). Mirrors the parser's
/// `is_go_identifier` rule without pulling in the keyword table: a leaf type or
/// receiver name we read from a parsed declaration is never a bare keyword.
fn is_go_ident(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_alphabetic()) && chars.all(|ch| ch == '_' || ch.is_alphanumeric())
}

/// Reduce a Go type expression token to its leaf type name: drop leading `*`
/// and `&`, a `pkg.` qualifier, and any generic-argument suffix (`Foo[T]`).
fn go_leaf_type_token(token: &str) -> &str {
    let token = token.trim_start_matches(['*', '&']).trim();
    let token = token.split('[').next().unwrap_or(token);
    token.rsplit('.').next().unwrap_or(token).trim()
}

pub(crate) fn go_package_name_from_path(path: &str) -> String {
    path.rsplit('/')
        .next()
        .unwrap_or(path)
        .trim_end_matches(".go")
        .to_string()
}
