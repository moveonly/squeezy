//! Swift graph-side helpers: module imports, extension-method receiver
//! matching, protocol conformance walking. Mirrors the Java helper
//! structure in `crates/squeezy-graph/src/languages/java.rs`.
//!
//! Spec: `docs/internal/lang-specs/swift.md`.

use crate::*;

impl SemanticGraph {
    /// Returns the SwiftPM module name for `file_id`, if the file's
    /// `ParsedFile::package` was set by the extractor (paths under
    /// `Sources/<Module>/...` or `Tests/<Module>/...`).
    pub(crate) fn swift_module_for_file(&self, file_id: &FileId) -> Option<&str> {
        self.packages.get(file_id).map(String::as_str)
    }

    /// Returns true when a `ParsedImport` from a Swift file's `import M`
    /// statement could resolve `symbol` — i.e. when `symbol` is declared
    /// inside the SwiftPM module `M`. Swift imports are coarse-grained at
    /// the module level (or `import struct M.T` selecting a specific
    /// member); both shapes are handled here.
    pub(crate) fn swift_import_matches_symbol(
        &self,
        import: &ParsedImport,
        symbol: &GraphSymbol,
    ) -> bool {
        let Some(module) = self.swift_module_for_file(&symbol.file_id) else {
            return false;
        };
        let path = import.path.as_str();
        if path == module {
            // Bare `import M` — symbol's module matches.
            return true;
        }
        // `import struct M.T` etc. We strip the kind in the extractor, so
        // the path here is `M.T`. The leaf must equal the symbol name and
        // the prefix must equal the symbol's module.
        if let Some((prefix, leaf)) = path.rsplit_once('.')
            && prefix == module
            && leaf == symbol.name
        {
            return true;
        }
        false
    }

    /// Walk every owner `class`/`struct`/`enum`/`trait` ancestor a symbol
    /// has, returning the chain bottom-up plus the SwiftPM module name as
    /// the root. Used by `swift_import_matches_symbol` for nested types
    /// and by extension receiver matching.
    #[allow(dead_code)]
    pub(crate) fn swift_symbol_owner_path(&self, symbol: &GraphSymbol) -> Vec<String> {
        let mut path: Vec<String> = self
            .swift_module_for_file(&symbol.file_id)
            .map(|m| vec![m.to_string()])
            .unwrap_or_default();
        let mut chain: Vec<String> = Vec::new();
        let mut parent_id = symbol.parent_id.as_ref();
        while let Some(id) = parent_id {
            let Some(parent) = self.symbols.get(id) else {
                break;
            };
            if matches!(
                parent.kind,
                SymbolKind::Class | SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Trait
            ) {
                chain.push(parent.name.clone());
            }
            parent_id = parent.parent_id.as_ref();
        }
        chain.reverse();
        path.extend(chain);
        path
    }

    /// Spec gotcha (a): extension members emit at file scope with
    /// `language_identity = <ExtendedType>`. When resolving `foo.bar()`
    /// where `foo: Foo`, the receiver-method lookup also checks members
    /// whose `language_identity` is `Foo`. Returns the receiver-method
    /// symbol id when exactly one candidate matches.
    pub(crate) fn swift_extension_receiver_method(
        &self,
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        let receiver = call.receiver.as_deref()?;
        if matches!(receiver, "self" | "super") || receiver.contains(' ') || receiver.contains('(')
        {
            return None;
        }
        let caller = self.symbols.get(caller_id)?;
        let caller_file = self.files.get(&caller.file_id)?;
        if caller_file.language != LanguageKind::Swift {
            return None;
        }

        // Heuristic: bound types for the receiver come from either a
        // direct symbol named `receiver` whose type:T attribute is set,
        // or from the caller's enclosing type when `receiver == "self"`.
        // We support the first form to mirror Java's
        // `java_receiver_field_method`.
        let receiver_type = self.swift_receiver_type_name(caller_id, receiver);

        let candidates: Vec<SymbolId> = self
            .find_symbol_by_name(&call.name)
            .into_iter()
            .filter(|s| s.kind == SymbolKind::Method)
            .filter(|s| match (&receiver_type, &s.language_identity) {
                (Some(ty), Some(id)) => ty == id,
                _ => false,
            })
            .map(|s| s.id)
            .collect();
        single_swift_symbol(candidates)
    }

    fn swift_receiver_type_name(&self, caller_id: &SymbolId, receiver: &str) -> Option<String> {
        // Look for a stored property `let receiver: T` on the caller's
        // enclosing type. If found, return T's leaf name (already stored
        // in `type:` attribute by the extractor).
        let class_id = self.swift_owner_type_for_caller(caller_id)?;
        let field = self
            .children_by_parent
            .get(&class_id)?
            .iter()
            .find_map(|child_id| {
                self.symbols
                    .get(child_id)
                    .filter(|s| s.kind == SymbolKind::Field && s.name == receiver)
            })?;
        field
            .attributes
            .iter()
            .find_map(|attribute| attribute.strip_prefix("type:"))
            .map(str::to_string)
    }

    fn swift_owner_type_for_caller(&self, caller_id: &SymbolId) -> Option<SymbolId> {
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

fn single_swift_symbol(mut ids: Vec<SymbolId>) -> Option<SymbolId> {
    ids.sort_by(|left, right| left.0.cmp(&right.0));
    ids.dedup();
    if ids.len() == 1 { ids.pop() } else { None }
}
