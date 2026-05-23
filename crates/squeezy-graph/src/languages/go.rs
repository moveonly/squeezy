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
}

pub(crate) fn go_package_name_from_path(path: &str) -> String {
    path.rsplit('/')
        .next()
        .unwrap_or(path)
        .trim_end_matches(".go")
        .to_string()
}
