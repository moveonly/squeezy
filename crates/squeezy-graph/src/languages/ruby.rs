//! Ruby graph resolver helpers.
//!
//! Mirrors the Python resolver shape: walk the inheritance chain rooted at
//! the caller's enclosing class, then mixins (Ruby MRO: `prepend` ->
//! self -> `include` -> superclass), capped at depth 8. See
//! `docs/internal/lang-specs/ruby.md` §4(g) for the contract.
use crate::*;

impl SemanticGraph {
    pub(crate) fn caller_is_ruby(&self, caller_id: &SymbolId) -> bool {
        self.symbols
            .get(caller_id)
            .and_then(|caller| self.files.get(&caller.file_id))
            .map(|file| file.language == squeezy_core::LanguageKind::Ruby)
            .unwrap_or(false)
    }

    /// Resolve `self.foo`/`bare_call`/`Class.method` Ruby dispatch by walking
    /// the host class -> superclass -> mixins chain.
    pub(crate) fn inherited_ruby_method(
        &self,
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        if !self.caller_is_ruby(caller_id) {
            return None;
        }
        let class_id = self.ruby_host_class_for_caller(caller_id)?;
        // For receiver-less calls inside a method body, look up the chain
        // starting at the caller's host class.
        if call.receiver.is_none() {
            return self.ruby_method_on_class_or_ancestors(&class_id, &call.name, 0);
        }
        let receiver = call.receiver.as_deref()?;
        if matches!(receiver, "self") {
            return self.ruby_method_on_class_or_ancestors(&class_id, &call.name, 0);
        }
        // `Foo.method` style: try resolving the receiver as a class name in
        // the caller's file (qualified or simple).
        let target_class =
            self.ruby_class_for_name_in_file(&self.symbols.get(caller_id)?.file_id, receiver)?;
        self.ruby_method_on_class_or_ancestors(&target_class, &call.name, 0)
    }

    fn ruby_host_class_for_caller(&self, caller_id: &SymbolId) -> Option<SymbolId> {
        let caller = self.symbols.get(caller_id)?;
        let mut current = Some(caller.clone());
        while let Some(symbol) = current {
            if matches!(symbol.kind, SymbolKind::Class | SymbolKind::Module) {
                return Some(symbol.id.clone());
            }
            current = symbol
                .parent_id
                .and_then(|id| self.symbols.get(&id).cloned());
        }
        None
    }

    fn ruby_class_for_name_in_file(&self, file_id: &FileId, name: &str) -> Option<SymbolId> {
        let leaf = last_path_segment(name);
        // Prefer a same-file class/module with this name (cheap match for
        // intra-file `Foo.method` style calls).
        for symbol in self.symbols.values() {
            if symbol.file_id == *file_id
                && matches!(symbol.kind, SymbolKind::Class | SymbolKind::Module)
                && symbol.name == leaf
            {
                return Some(symbol.id.clone());
            }
        }
        // Else scan the workspace for a Class/Module by leaf name (cross-file).
        single_symbol(
            self.symbols_by_name_or_scan(&leaf)
                .into_iter()
                .filter_map(|id| self.symbols.get(&id))
                .filter(|s| matches!(s.kind, SymbolKind::Class | SymbolKind::Module))
                .map(|s| s.id.clone()),
        )
    }

    /// Walk `class -> mixin:prepend:* -> class itself -> mixin:include:* ->
    /// base:*` looking for a Method named `name`. Capped at depth 8.
    fn ruby_method_on_class_or_ancestors(
        &self,
        class_id: &SymbolId,
        name: &str,
        depth: usize,
    ) -> Option<SymbolId> {
        if depth > 8 {
            return None;
        }
        let class = self.symbols.get(class_id)?;
        // `prepend` ancestors come *before* the class itself in Ruby MRO.
        for prepend in class
            .attributes
            .iter()
            .filter_map(|attr| attr.strip_prefix("mixin:prepend:"))
        {
            if let Some(ancestor) = self.ruby_class_for_name_in_file(&class.file_id, prepend)
                && let Some(method) =
                    self.ruby_method_on_class_or_ancestors(&ancestor, name, depth + 1)
            {
                return Some(method);
            }
        }
        if let Some(method) = self.python_method_on_class(class_id, name) {
            return Some(method);
        }
        // `include` ancestors come after the class itself.
        for include in class
            .attributes
            .iter()
            .filter_map(|attr| attr.strip_prefix("mixin:include:"))
        {
            if let Some(ancestor) = self.ruby_class_for_name_in_file(&class.file_id, include)
                && let Some(method) =
                    self.ruby_method_on_class_or_ancestors(&ancestor, name, depth + 1)
            {
                return Some(method);
            }
        }
        // `base:Foo` superclass.
        for base in class
            .attributes
            .iter()
            .filter_map(|attr| attr.strip_prefix("base:"))
        {
            if let Some(ancestor) = self.ruby_class_for_name_in_file(&class.file_id, base)
                && let Some(method) =
                    self.ruby_method_on_class_or_ancestors(&ancestor, name, depth + 1)
            {
                return Some(method);
            }
        }
        None
    }
}
