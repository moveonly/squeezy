use crate::*;

impl SemanticGraph {
    pub(crate) fn edge_hit(&self, edge_index: usize) -> Option<CallEdgeHit> {
        let edge = self.edges.get(edge_index)?.clone();
        Some(CallEdgeHit {
            caller: self.symbols.get(&edge.from).cloned(),
            callee: edge
                .to
                .as_ref()
                .and_then(|id| self.symbols.get(id))
                .cloned(),
            edge,
        })
    }

    pub(crate) fn reference_hit(
        &self,
        reference: &ParsedReference,
        confidence: Confidence,
    ) -> ReferenceHit {
        ReferenceHit {
            owner: reference
                .owner_id
                .as_ref()
                .and_then(|id| self.symbols.get(id))
                .cloned(),
            reference: reference.clone(),
            confidence,
        }
    }

    pub(crate) fn reference_candidate_indexes(&self, text: &str) -> Vec<usize> {
        let mut indexes = BTreeSet::new();
        if let Some(exact) = self.references_by_text.get(text) {
            indexes.extend(exact.iter().copied());
        }
        let colon_suffix = format!("::{text}");
        if let Some(segment) = self.references_by_text.get(&colon_suffix) {
            indexes.extend(segment.iter().copied());
        }
        let dot_suffix = format!(".{text}");
        if let Some(segment) = self.references_by_text.get(&dot_suffix) {
            indexes.extend(segment.iter().copied());
        }
        let arrow_suffix = format!("->{text}");
        if let Some(segment) = self.references_by_text.get(&arrow_suffix) {
            indexes.extend(segment.iter().copied());
        }
        indexes.into_iter().collect()
    }

    pub(crate) fn reference_candidate_indexes_for_symbol(
        &self,
        symbol: &GraphSymbol,
    ) -> Vec<usize> {
        let mut indexes = BTreeSet::new();
        indexes.extend(self.reference_candidate_indexes(&symbol.name));
        // The `imports_by_alias_target` inverted index keys aliased imports by
        // `last_path_segment(import.path)`. For every non-glob aliased import
        // that resolves to `symbol`, the import's leaf equals `symbol.name`,
        // so a single hash lookup yields the candidate set. Glob aliased
        // imports live in `wildcard_aliased_imports` and are scanned as a
        // separate (small) bucket.
        let by_target = self
            .imports_by_alias_target
            .get(&symbol.name)
            .map(|indexes| indexes.as_slice())
            .unwrap_or(&[]);
        for index in by_target.iter().chain(self.wildcard_aliased_imports.iter()) {
            let Some(import) = self.imports.get(*index) else {
                continue;
            };
            let Some(alias) = import.alias.as_deref() else {
                continue;
            };
            if !self.import_matches_symbol(import, symbol) {
                continue;
            }
            indexes.extend(self.reference_candidate_indexes(alias));
        }
        indexes.into_iter().collect()
    }

    pub(crate) fn reference_binding_confidence(
        &self,
        symbol: &GraphSymbol,
        reference: &ParsedReference,
    ) -> Option<Confidence> {
        if !reference_text_matches_symbol(reference, symbol)
            && !self.reference_alias_matches_symbol(symbol, reference)
            && !self.reference_qualifier_matches_symbol(symbol, reference)
        {
            return None;
        }
        if path_starts_with_external_root(&reference.text, self.reference_language(reference))
            || self.reference_has_external_scope_prefix(reference)
        {
            return None;
        }
        if constructor_reference_can_bind_symbol(reference, symbol)
            && self.reference_has_uppercase_scope_prefix(reference)
        {
            return None;
        }
        if self.reference_is_symbol_declaration(symbol, reference) {
            return None;
        }
        if !self.reference_is_in_symbol_package(symbol, reference) {
            return None;
        }
        if self.python_property_reference_matches(symbol, reference) {
            return Some(Confidence::Heuristic);
        }
        if self.reference_alias_matches_symbol(symbol, reference)
            && reference_kind_can_bind_symbol(reference, symbol)
        {
            return Some(Confidence::ImportResolved);
        }
        if self.reference_is_impl_method_declaration_for_trait(symbol, reference) {
            return Some(Confidence::Heuristic);
        }
        if self.associated_type_reference_matches_symbol(symbol, reference) {
            return Some(Confidence::Heuristic);
        }
        if let Some(edge) = self.call_edge_for_reference(reference) {
            return self.edge_binding_confidence(symbol, edge);
        }
        if self.imported_reference_matches_symbol(symbol, reference) {
            return Some(Confidence::ImportResolved);
        }
        if let Some(edge) = self.semantic_edge_for_reference(reference) {
            if edge.kind == EdgeKind::References
                && !reference_kind_can_bind_symbol(reference, symbol)
            {
                return None;
            }
            if let Some(confidence) = self.edge_binding_confidence(symbol, edge) {
                return Some(confidence);
            }
            if !matches!(edge.kind, EdgeKind::Imports | EdgeKind::Reexports) {
                return None;
            }
        }
        if self.scoped_type_qualifier_matches_symbol(symbol, reference) {
            return Some(Confidence::Heuristic);
        }
        if self.qualified_reference_matches_symbol(symbol, reference) {
            return Some(Confidence::Heuristic);
        }
        if self.can_bind_loose_reference(symbol, reference) {
            return Some(Confidence::Heuristic);
        }
        None
    }

    pub(crate) fn reference_alias_matches_symbol(
        &self,
        symbol: &GraphSymbol,
        reference: &ParsedReference,
    ) -> bool {
        self.imports_for_file(&reference.file_id)
            .filter(|import| self.import_visible_from_reference(import, reference))
            .filter(|import| !is_package_marker(import))
            .filter(|import| import.alias.as_deref() == Some(reference.text.as_str()))
            .any(|import| self.import_matches_symbol(import, symbol))
    }

    pub(crate) fn edge_binding_confidence(
        &self,
        symbol: &GraphSymbol,
        edge: &GraphEdge,
    ) -> Option<Confidence> {
        let to = edge.to.as_ref()?;
        if to == &symbol.id || self.impl_method_implements_trait_method(to, symbol) {
            Some(edge.confidence)
        } else {
            None
        }
    }

    pub(crate) fn reference_language(&self, reference: &ParsedReference) -> LanguageKind {
        self.files
            .get(&reference.file_id)
            .map(|file| file.language)
            .unwrap_or(LanguageKind::Unknown)
    }

    pub(crate) fn reference_is_in_symbol_package(
        &self,
        symbol: &GraphSymbol,
        reference: &ParsedReference,
    ) -> bool {
        let Some(symbol_file) = self.files.get(&symbol.file_id) else {
            return false;
        };
        let Some(reference_file) = self.files.get(&reference.file_id) else {
            return false;
        };
        if matches!(
            (symbol_file.language, reference_file.language),
            (LanguageKind::Python, LanguageKind::Python)
        ) {
            return true;
        }
        if matches!(
            (symbol_file.language, reference_file.language),
            (LanguageKind::Java, LanguageKind::Java)
        ) {
            return self.java_package_for_file(&symbol.file_id)
                == self.java_package_for_file(&reference.file_id)
                || self.imported_reference_matches_symbol(symbol, reference);
        }
        if matches!(
            (symbol_file.language, reference_file.language),
            (LanguageKind::Go, LanguageKind::Go)
        ) {
            return self.packages.get(&symbol.file_id) == self.packages.get(&reference.file_id)
                && package_key(&symbol_file.relative_path)
                    == package_key(&reference_file.relative_path);
        }
        package_key(&symbol_file.relative_path) == package_key(&reference_file.relative_path)
    }

    pub(crate) fn imported_reference_matches_symbol(
        &self,
        symbol: &GraphSymbol,
        reference: &ParsedReference,
    ) -> bool {
        let reference_name = last_path_segment(&reference.text);
        if self
            .imports_for_file(&reference.file_id)
            .filter(|import| import.alias.as_deref() != Some("__java_package__"))
            .any(|import| {
                if import.is_glob
                    || !import.span.contains_byte(reference.span.start_byte)
                    || !import.span.contains_byte(reference.span.end_byte)
                {
                    return false;
                }
                let alias_or_name = import
                    .alias
                    .as_deref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| last_path_segment(&import.path));
                let collisions = self
                    .imports_for_file(&import.file_id)
                    .filter(|other| {
                        other.span == import.span && last_path_segment(&other.path) == symbol.name
                    })
                    .count();
                let alias_match = import.alias.as_deref() == Some(reference.text.as_str());
                let full_path_match = reference.text == import.path;
                (alias_match || full_path_match || collisions <= 1)
                    && (reference_name == symbol.name || reference_name == alias_or_name)
                    && last_path_segment(&import.path) == symbol.name
                    && self.import_module_matches_symbol(import, symbol)
            })
        {
            return true;
        }
        if !reference_kind_can_bind_symbol(reference, symbol) {
            return false;
        }
        self.imports_for_file(&reference.file_id).any(|import| {
            if import.alias.as_deref() == Some("__java_package__") {
                return false;
            }
            if path_starts_with_external_root(&import.path, self.reference_language(reference)) {
                return false;
            }
            let alias_or_name = import
                .alias
                .as_deref()
                .map(ToString::to_string)
                .unwrap_or_else(|| last_path_segment(&import.path));
            if import.is_glob {
                return reference_name == symbol.name
                    && self.import_module_matches_symbol(import, symbol);
            }
            alias_or_name == reference_name
                && last_path_segment(&import.path) == symbol.name
                && self.import_module_matches_symbol(import, symbol)
        })
    }

    pub(crate) fn reference_qualifier_matches_symbol(
        &self,
        symbol: &GraphSymbol,
        reference: &ParsedReference,
    ) -> bool {
        if !matches!(reference.kind, ReferenceKind::Path) || !is_type_like_symbol(symbol.kind) {
            return false;
        }
        path_segments(&reference.text)
            .first()
            .map(|segment| segment == &symbol.name)
            .unwrap_or(false)
    }

    pub(crate) fn scoped_type_qualifier_matches_symbol(
        &self,
        symbol: &GraphSymbol,
        reference: &ParsedReference,
    ) -> bool {
        if !self.reference_qualifier_matches_symbol(symbol, reference) {
            return false;
        }
        if path_starts_with_external_root(&reference.text, self.reference_language(reference)) {
            return false;
        }
        self.symbol_is_in_reference_scope(symbol, reference)
    }

    pub(crate) fn symbol_is_in_reference_scope(
        &self,
        symbol: &GraphSymbol,
        reference: &ParsedReference,
    ) -> bool {
        if symbol.file_id == reference.file_id {
            return true;
        }
        self.imports_for_file(&reference.file_id)
            .filter(|import| import.alias.as_deref() != Some("__java_package__"))
            .any(|import| {
                let alias_or_name = import
                    .alias
                    .as_deref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| last_path_segment(&import.path));
                alias_or_name == symbol.name
                    && last_path_segment(&import.path) == symbol.name
                    && self.import_module_matches_symbol(import, symbol)
            })
    }

    pub(crate) fn import_module_matches_symbol(
        &self,
        import: &ParsedImport,
        symbol: &GraphSymbol,
    ) -> bool {
        let symbol_language = self.files.get(&symbol.file_id).map(|file| file.language);
        if symbol_language == Some(squeezy_core::LanguageKind::Java) {
            return self.java_import_matches_symbol(import, symbol);
        }
        if symbol_language == Some(squeezy_core::LanguageKind::Kotlin) {
            return self.kotlin_import_matches_symbol(import, symbol);
        }
        if symbol_language.map(is_js_ts_language).unwrap_or(false) {
            return self.js_ts_import_matches_symbol(import, symbol);
        }
        if symbol_language == Some(squeezy_core::LanguageKind::Swift) {
            return self.swift_import_matches_symbol(import, symbol);
        }
        let mut import_path = path_segments(&import.path);
        if import.is_glob {
            if import_path.last().map(String::as_str) == Some("*") {
                import_path.pop();
            }
        } else {
            import_path.pop();
        }
        if import_path.is_empty() {
            return false;
        }
        import_path == self.module_path_for_symbol(symbol)
    }

    pub(crate) fn qualified_reference_matches_symbol(
        &self,
        symbol: &GraphSymbol,
        reference: &ParsedReference,
    ) -> bool {
        if !reference.text.contains("::") || !reference_kind_can_bind_symbol(reference, symbol) {
            return false;
        }
        let mut reference_path = path_segments(&reference.text);
        if reference_path.pop().as_deref() != Some(symbol.name.as_str()) {
            return false;
        }
        if reference_path.is_empty()
            || path_starts_with_external_root(&reference.text, self.reference_language(reference))
        {
            return false;
        }
        if reference_path.first().map(String::as_str) == Some("crate") {
            return reference_path == self.module_path_for_symbol(symbol);
        }
        let Some(owner) = reference
            .owner_id
            .as_ref()
            .and_then(|id| self.symbols.get(id))
        else {
            return false;
        };
        self.receiver_module_paths(owner, &reference_path.join("::"))
            .into_iter()
            .any(|path| path == self.module_path_for_symbol(symbol))
    }

    pub(crate) fn impl_method_implements_trait_method(
        &self,
        impl_method_id: &SymbolId,
        trait_method: &GraphSymbol,
    ) -> bool {
        if !matches!(trait_method.kind, SymbolKind::Function | SymbolKind::Method) {
            return false;
        }
        let Some(trait_symbol) = trait_method
            .parent_id
            .as_ref()
            .and_then(|id| self.symbols.get(id))
            .filter(|symbol| symbol.kind == SymbolKind::Trait)
        else {
            return false;
        };
        let Some(impl_method) = self.symbols.get(impl_method_id) else {
            return false;
        };
        if impl_method.name != trait_method.name
            || !matches!(impl_method.kind, SymbolKind::Function | SymbolKind::Method)
        {
            return false;
        }
        let Some(impl_parent) = impl_method
            .parent_id
            .as_ref()
            .and_then(|id| self.symbols.get(id))
            .filter(|parent| parent.kind == SymbolKind::Impl)
        else {
            return false;
        };
        impl_header_implements_trait(&impl_parent.name, &trait_symbol.name)
            && self.impl_header_trait_resolves_to(&impl_parent.name, trait_symbol, impl_parent)
    }

    pub(crate) fn impl_header_trait_resolves_to(
        &self,
        header: &str,
        trait_symbol: &GraphSymbol,
        impl_anchor: &GraphSymbol,
    ) -> bool {
        let Some((trait_part, _)) = header.split_once(" for ") else {
            return false;
        };
        let trait_part = trait_part
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != ':');
        let segments = path_segments(trait_part);
        if segments.last().map(String::as_str) != Some(trait_symbol.name.as_str()) {
            return false;
        }
        if segments.len() == 1 {
            if trait_symbol.file_id == impl_anchor.file_id {
                return true;
            }
            return self
                .imports
                .iter()
                .filter(|import| import.file_id == impl_anchor.file_id)
                .any(|import| {
                    let alias_or_name = import
                        .alias
                        .as_deref()
                        .map(str::to_string)
                        .unwrap_or_else(|| last_path_segment(&import.path));
                    alias_or_name == trait_symbol.name
                        && self.import_module_matches_symbol(import, trait_symbol)
                });
        }
        if segments.first().map(String::as_str) == Some("crate") {
            let mut expected = self.module_path_for_symbol(trait_symbol);
            expected.push(trait_symbol.name.clone());
            return segments == expected;
        }
        let receiver = segments[..segments.len() - 1].join("::");
        self.receiver_module_paths(impl_anchor, &receiver)
            .into_iter()
            .any(|receiver_path| receiver_path == self.module_path_for_symbol(trait_symbol))
    }

    pub(crate) fn reference_is_impl_method_declaration_for_trait(
        &self,
        trait_method: &GraphSymbol,
        reference: &ParsedReference,
    ) -> bool {
        let Some(owner) = reference
            .owner_id
            .as_ref()
            .and_then(|id| self.symbols.get(id))
        else {
            return false;
        };
        if !self.impl_method_implements_trait_method(&owner.id, trait_method)
            || !self.reference_is_symbol_declaration(owner, reference)
        {
            return false;
        }
        !self.symbol_or_ancestors_have_cfg_attribute(owner)
    }

    pub(crate) fn associated_type_reference_matches_symbol(
        &self,
        symbol: &GraphSymbol,
        reference: &ParsedReference,
    ) -> bool {
        if symbol.kind != SymbolKind::TypeAlias || last_path_segment(&reference.text) != symbol.name
        {
            return false;
        }
        let Some(symbol_owner) = symbol
            .parent_id
            .as_ref()
            .and_then(|id| self.symbols.get(id))
        else {
            return false;
        };
        if symbol_owner.kind != SymbolKind::Trait {
            return false;
        }

        let segments = path_segments(&reference.text);
        if segments.len() < 2 || segments.last() != Some(&symbol.name) {
            return false;
        }
        let qualifier = &segments[..segments.len() - 1];
        if qualifier.len() == 1 && qualifier[0] == "Self" {
            return self
                .reference_owner_trait(reference)
                .map(|owner| owner.id == symbol_owner.id)
                .unwrap_or(false);
        }
        self.trait_path_matches_symbol(qualifier, symbol_owner, reference)
    }

    pub(crate) fn reference_owner_trait(
        &self,
        reference: &ParsedReference,
    ) -> Option<&GraphSymbol> {
        let mut current = reference
            .owner_id
            .as_ref()
            .and_then(|id| self.symbols.get(id));
        while let Some(symbol) = current {
            match symbol.kind {
                SymbolKind::Trait => return Some(symbol),
                SymbolKind::Impl => return None,
                _ => {
                    current = symbol
                        .parent_id
                        .as_ref()
                        .and_then(|id| self.symbols.get(id));
                }
            }
        }
        None
    }

    pub(crate) fn trait_path_matches_symbol(
        &self,
        path: &[String],
        trait_symbol: &GraphSymbol,
        reference: &ParsedReference,
    ) -> bool {
        if path.last().map(String::as_str) != Some(trait_symbol.name.as_str()) {
            return false;
        }
        if path.len() == 1 {
            return self.symbol_is_in_reference_scope(trait_symbol, reference);
        }
        if path.first().map(String::as_str) == Some("crate") {
            let mut expected = self.module_path_for_symbol(trait_symbol);
            expected.push(trait_symbol.name.clone());
            return path == expected;
        }
        let Some(owner) = reference
            .owner_id
            .as_ref()
            .and_then(|id| self.symbols.get(id))
        else {
            return false;
        };
        let receiver = path[..path.len() - 1].join("::");
        self.receiver_module_paths(owner, &receiver)
            .into_iter()
            .any(|receiver_path| receiver_path == self.module_path_for_symbol(trait_symbol))
    }

    pub(crate) fn symbol_or_ancestors_have_cfg_attribute(&self, symbol: &GraphSymbol) -> bool {
        if has_cfg_attribute(symbol) || self.symbol_has_leading_cfg_attribute(symbol) {
            return true;
        }
        let mut parent_id = symbol.parent_id.as_ref();
        while let Some(id) = parent_id {
            let Some(parent) = self.symbols.get(id) else {
                break;
            };
            if has_cfg_attribute(parent) || self.symbol_has_leading_cfg_attribute(parent) {
                return true;
            }
            parent_id = parent.parent_id.as_ref();
        }
        false
    }

    pub(crate) fn symbol_has_leading_cfg_attribute(&self, symbol: &GraphSymbol) -> bool {
        let Some(file) = self.files.get(&symbol.file_id) else {
            return false;
        };
        let Ok(source) = std::fs::read_to_string(&file.path) else {
            return false;
        };
        let Some(prefix) = source.get(..symbol.span.start_byte as usize) else {
            return false;
        };
        let mut in_multiline_attribute = false;
        for line in prefix.lines().rev() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.starts_with("//") {
                continue;
            }
            if trimmed.starts_with("/*") && trimmed.ends_with("*/") {
                continue;
            }
            if in_multiline_attribute {
                if trimmed.starts_with("#[") || trimmed.starts_with("#![") {
                    in_multiline_attribute = false;
                    if attribute_text_is_cfg(trimmed) {
                        return true;
                    }
                }
                continue;
            }
            if trimmed.starts_with("#[") || trimmed.starts_with("#![") {
                if attribute_text_is_cfg(trimmed) {
                    return true;
                }
                continue;
            }
            if trimmed.ends_with(']') {
                in_multiline_attribute = true;
                continue;
            }
            break;
        }
        false
    }

    pub(crate) fn reference_is_symbol_declaration(
        &self,
        symbol: &GraphSymbol,
        reference: &ParsedReference,
    ) -> bool {
        if symbol.file_id != reference.file_id {
            return false;
        }
        let signature_end = symbol
            .body_span
            .map(|span| span.start_byte)
            .unwrap_or(symbol.span.end_byte);
        if !(symbol.span.start_byte <= reference.span.start_byte
            && reference.span.end_byte <= signature_end)
        {
            return false;
        }
        let Some(file) = self.files.get(&symbol.file_id) else {
            return false;
        };
        let Ok(source) = std::fs::read_to_string(&file.path) else {
            return false;
        };
        let Some(signature) = source.get(symbol.span.start_byte as usize..signature_end as usize)
        else {
            return false;
        };
        find_identifier(signature, &symbol.name)
            .map(|offset| reference.span.start_byte == symbol.span.start_byte + offset as u32)
            .unwrap_or(false)
    }

    pub(crate) fn reference_has_external_scope_prefix(&self, reference: &ParsedReference) -> bool {
        if reference.text.contains("::") {
            return false;
        }
        let Some(file) = self.files.get(&reference.file_id) else {
            return false;
        };
        let Ok(source) = std::fs::read_to_string(&file.path) else {
            return false;
        };
        let Some(prefix) = source.get(..reference.span.start_byte as usize) else {
            return false;
        };
        let scope = prefix
            .chars()
            .rev()
            .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_' || *ch == ':')
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        let scope = scope.trim_end_matches("::");
        !scope.is_empty()
            && path_starts_with_external_root(scope, self.reference_language(reference))
    }

    pub(crate) fn reference_has_uppercase_scope_prefix(&self, reference: &ParsedReference) -> bool {
        if reference.text.contains("::") {
            return false;
        }
        let Some(file) = self.files.get(&reference.file_id) else {
            return false;
        };
        let Ok(source) = std::fs::read_to_string(&file.path) else {
            return false;
        };
        let Some(prefix) = source.get(..reference.span.start_byte as usize) else {
            return false;
        };
        let scope = prefix
            .chars()
            .rev()
            .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_' || *ch == ':')
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        let scope = scope.trim_end_matches("::");
        !scope.is_empty()
            && scope
                .rsplit("::")
                .next()
                .unwrap_or_default()
                .chars()
                .next()
                .map(|ch| ch.is_ascii_uppercase())
                .unwrap_or(false)
    }

    pub(crate) fn semantic_edge_for_reference(
        &self,
        reference: &ParsedReference,
    ) -> Option<&GraphEdge> {
        let from = reference
            .owner_id
            .clone()
            .unwrap_or_else(|| file_symbol_id(&reference.file_id));
        self.edges_by_from
            .get(&from)?
            .iter()
            .filter_map(|index| self.edges.get(*index))
            .filter(|edge| {
                matches!(
                    edge.kind,
                    EdgeKind::References | EdgeKind::Imports | EdgeKind::Reexports
                ) && edge
                    .span
                    .map(|span| span.contains_byte(reference.span.start_byte))
                    .unwrap_or(false)
            })
            .min_by_key(|edge| {
                edge.span
                    .map(|span| span.end_byte.saturating_sub(span.start_byte))
                    .unwrap_or(u32::MAX)
            })
    }

    pub(crate) fn call_edge_for_reference(
        &self,
        reference: &ParsedReference,
    ) -> Option<&GraphEdge> {
        let from = reference
            .owner_id
            .clone()
            .unwrap_or_else(|| file_symbol_id(&reference.file_id));
        self.edges_by_from
            .get(&from)?
            .iter()
            .filter_map(|index| self.edges.get(*index))
            .filter(|edge| {
                matches!(edge.kind, EdgeKind::Calls | EdgeKind::InvokesMacro)
                    && edge
                        .span
                        .map(|span| {
                            span.contains_byte(reference.span.start_byte)
                                && span.contains_byte(reference.span.end_byte)
                        })
                        .unwrap_or(false)
                    && last_path_segment(&edge.target_text) == last_path_segment(&reference.text)
            })
            .min_by_key(|edge| {
                edge.span
                    .map(|span| span.end_byte.saturating_sub(span.start_byte))
                    .unwrap_or(u32::MAX)
            })
    }

    pub(crate) fn can_bind_loose_reference(
        &self,
        symbol: &GraphSymbol,
        reference: &ParsedReference,
    ) -> bool {
        if !reference_kind_can_bind_symbol(reference, symbol) {
            return false;
        }
        let candidates = self.symbols_by_name_or_scan(&symbol.name);
        let same_file_candidates = candidates
            .iter()
            .filter_map(|id| self.symbols.get(id))
            .filter(|candidate| {
                candidate.file_id == reference.file_id
                    && reference_kind_can_bind_symbol(reference, candidate)
            })
            .collect::<Vec<_>>();
        same_file_candidates.len() == 1 && same_file_candidates[0].id == symbol.id
    }
}
