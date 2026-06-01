use crate::*;

impl SemanticGraph {
    pub(crate) fn c_family_include_direct_call(
        &self,
        candidates: &[SymbolId],
        caller_id: &SymbolId,
    ) -> Option<SymbolId> {
        let caller = self.symbols.get(caller_id)?;
        let caller_file = self.files.get(&caller.file_id)?;
        if !matches!(caller_file.language, LanguageKind::C | LanguageKind::Cpp) {
            return None;
        }
        let include_paths = self
            .imports_for_file(&caller.file_id)
            .filter(|import| import.provenance.reason.contains("include directive"))
            .map(|import| import.path.as_str())
            .collect::<Vec<_>>();
        if include_paths.is_empty() {
            return None;
        }

        // Gather candidate definitions/declarations that live in an
        // included header *or any file in the same package as an included
        // header* (e.g. `#include "runner.h"` + `runner.c` next to it).
        // Definitions (body_span.is_some()) beat declarations when both
        // exist — that's the canonical target the user actually wants to
        // jump to. We also let any same-workspace C/C++ symbol resolve so
        // a single-defining function still binds when the include only
        // declares it.
        let mut header_matches = Vec::new();
        let mut definitions = Vec::new();
        for symbol in candidates
            .iter()
            .filter_map(|id| self.symbols.get(id))
            .filter(|symbol| matches!(symbol.kind, SymbolKind::Function | SymbolKind::Method))
        {
            let Some(file) = self.files.get(&symbol.file_id) else {
                continue;
            };
            if !matches!(file.language, LanguageKind::C | LanguageKind::Cpp) {
                continue;
            }
            if include_paths
                .iter()
                .any(|include| include_path_matches_file(include, &file.relative_path))
            {
                header_matches.push(symbol.id.clone());
            }
            if symbol.body_span.is_some()
                && include_paths
                    .iter()
                    .any(|include| file_shares_include_root(include, &file.relative_path))
            {
                definitions.push(symbol.id.clone());
            }
        }

        if let Some(only) = single_unique(definitions) {
            return Some(only);
        }
        single_unique(header_matches)
    }
}
