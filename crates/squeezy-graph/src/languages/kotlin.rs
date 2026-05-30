// Kotlin-specific graph helpers.
//
// Mirrors `crates/squeezy-graph/src/languages/java.rs` in shape but keeps the
// Kotlin chain (package marker, import classification, owner-path, source-root
// detection, member resolution) self-contained — no `pub(crate)` symbol from
// `java.rs` is re-exported here. Future Java refactors must not break Kotlin.
//
// Out of scope for this PR (TODOs reference the spec):
//   - companion-object owner-path collapsing for member resolution beyond
//     "child reparented to host class" (extractor side)
//   - delegated-property accessor binding (spec §4g)
//   - `inline reified` type-parameter modeling (spec §4d)
//   - sealed-class child enumeration helper (spec §4f, phase 2)

use crate::*;

impl SemanticGraph {
    pub(crate) fn kotlin_package_for_file(&self, file_id: &FileId) -> Option<Vec<String>> {
        self.kotlin_package_by_file.get(file_id).cloned()
    }

    /// Whether `import` matches `symbol` for the Kotlin classification of
    /// the import kind. Mirrors `java_import_matches_symbol` but without
    /// the Java-only `static` form (Kotlin has no `import static`).
    pub(crate) fn kotlin_import_matches_symbol(
        &self,
        import: &ParsedImport,
        symbol: &GraphSymbol,
    ) -> bool {
        let mut import_segments = path_segments(&import.path);
        let last_segment_is_glob = import_segments
            .last()
            .map(|segment| segment == "*")
            .unwrap_or(false);
        if last_segment_is_glob {
            import_segments.pop();
        }
        let Some(package) = self.kotlin_package_for_file(&symbol.file_id) else {
            return false;
        };

        // Wildcard import (`import a.b.*`). Matches every top-level symbol
        // (class/object/function/property/typealias) whose package equals
        // `import_segments`, plus any nested type whose owner-class chain
        // begins below `import_segments`.
        if import.is_glob {
            if import_segments == package && kotlin_symbol_is_top_level(symbol) {
                return true;
            }
            return self.kotlin_symbol_owner_path(symbol) == import_segments;
        }

        // Named import. The path's leaf names the target symbol's *original*
        // name; the alias is only the locally-bound name and never appears
        // in the package + class chain of the target symbol.
        if last_path_segment(&import.path) != symbol.name {
            return false;
        }
        import_segments.pop();
        // Owner path = package + any class chain. For a top-level symbol the
        // owner path is the package itself.
        let owner_path = self.kotlin_symbol_owner_path(symbol);
        owner_path == import_segments
    }

    pub(crate) fn kotlin_symbol_owner_path(&self, symbol: &GraphSymbol) -> Vec<String> {
        let mut path = self
            .kotlin_package_for_file(&symbol.file_id)
            .unwrap_or_default();
        let mut chain = Vec::new();
        let mut parent_id = symbol.parent_id.as_ref();
        while let Some(id) = parent_id {
            let Some(parent) = self.symbols.get(id) else {
                break;
            };
            if matches!(
                parent.kind,
                SymbolKind::Class | SymbolKind::Trait | SymbolKind::Enum | SymbolKind::Struct,
            ) && !parent
                .attributes
                .iter()
                .any(|attribute| attribute == "kotlin:companion")
            {
                chain.push(parent.name.clone());
            }
            parent_id = parent.parent_id.as_ref();
        }
        chain.reverse();
        path.extend(chain);
        path
    }

    /// Resolve `someReceiver.foo()` against extension functions whose
    /// `language_identity` records the receiver type. The function must
    /// either live in the caller's file, be imported by the caller's file,
    /// or share the caller's package — i.e. Kotlin's normal lookup chain.
    pub(crate) fn kotlin_extension_function_call(
        &self,
        candidates: &[SymbolId],
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        let receiver = call.receiver.as_deref()?;
        if matches!(receiver, "this" | "super") || receiver.contains(' ') {
            return None;
        }
        let caller = self.symbols.get(caller_id)?;
        let caller_file = self.files.get(&caller.file_id)?;
        if caller_file.language != LanguageKind::Kotlin {
            return None;
        }

        // Infer the receiver's *type name*. If the receiver is a property of
        // the caller's enclosing class with a `type:T` attribute, use T;
        // otherwise fall back to the raw receiver text (covers cases like
        // `Foo.bar()` where the receiver is itself a type name).
        let receiver_type = self
            .kotlin_receiver_type_name(caller_id, receiver)
            .unwrap_or_else(|| last_path_segment(receiver));

        let matches = candidates
            .iter()
            .filter_map(|id| self.symbols.get(id))
            .filter(|symbol| {
                symbol
                    .attributes
                    .iter()
                    .any(|attribute| attribute == "kotlin:extension")
                    && symbol.name == call.name
                    && symbol
                        .language_identity
                        .as_deref()
                        .map(|identity| {
                            identity == receiver_type
                                || last_path_segment(identity) == receiver_type
                        })
                        .unwrap_or(false)
            })
            .filter(|symbol| self.kotlin_symbol_visible_to_caller(symbol, caller, caller_file))
            .map(|symbol| symbol.id.clone());

        single_symbol(matches)
    }

    /// Resolve `Host.member()` against companion-object members that the
    /// Kotlin extractor re-parents to the host class. The receiver text
    /// must equal the host class name as seen from the caller (after
    /// alias resolution).
    pub(crate) fn kotlin_companion_member_call(
        &self,
        candidates: &[SymbolId],
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        let receiver = call.receiver.as_deref()?;
        if matches!(receiver, "this" | "super") || receiver.contains(' ') {
            return None;
        }
        let caller = self.symbols.get(caller_id)?;
        let caller_file = self.files.get(&caller.file_id)?;
        if caller_file.language != LanguageKind::Kotlin {
            return None;
        }
        let receiver_name = last_path_segment(receiver);
        // Resolve alias imports so `Friendly.create()` matches
        // `import com.example.services.FriendlyGreeter as Friendly`.
        let resolved_host = self
            .kotlin_alias_target(&caller.file_id, &receiver_name)
            .unwrap_or(receiver_name);

        let matches = candidates
            .iter()
            .filter_map(|id| self.symbols.get(id))
            .filter(|symbol| symbol.name == call.name && symbol.kind == SymbolKind::Method)
            .filter(|symbol| {
                symbol
                    .attributes
                    .iter()
                    .any(|attribute| attribute == "kotlin:companion")
                    && symbol
                        .parent_id
                        .as_ref()
                        .and_then(|id| self.symbols.get(id))
                        .map(|parent| parent.name == resolved_host)
                        .unwrap_or(false)
            })
            .filter(|symbol| {
                // Visibility for a companion member is governed by the
                // visibility of its *host class* — the receiver of the
                // call. Resolve via the host symbol so a plain
                // `import a.b.Host` (no `.create`) is enough.
                let Some(host) = symbol
                    .parent_id
                    .as_ref()
                    .and_then(|id| self.symbols.get(id))
                else {
                    return false;
                };
                self.kotlin_symbol_visible_to_caller(host, caller, caller_file)
            })
            .map(|symbol| symbol.id.clone());

        single_symbol(matches)
    }

    fn kotlin_alias_target(&self, file_id: &FileId, alias: &str) -> Option<String> {
        self.imports_for_file(file_id)
            .filter(|import| import.alias.as_deref() == Some(alias))
            .map(|import| last_path_segment(&import.path))
            .next()
    }

    fn kotlin_receiver_type_name(&self, caller_id: &SymbolId, receiver: &str) -> Option<String> {
        let caller = self.symbols.get(caller_id)?;
        let class_id = self.kotlin_class_for_caller(caller_id)?;
        let _ = caller;
        let field = self
            .children_by_parent
            .get(&class_id)?
            .iter()
            .find_map(|id| {
                self.symbols
                    .get(id)
                    .filter(|symbol| symbol.kind == SymbolKind::Field && symbol.name == receiver)
            })?;
        field
            .attributes
            .iter()
            .find_map(|attribute| attribute.strip_prefix("type:"))
            .map(|s| s.to_string())
    }

    fn kotlin_class_for_caller(&self, caller_id: &SymbolId) -> Option<SymbolId> {
        let caller = self.symbols.get(caller_id)?;
        if matches!(
            caller.kind,
            SymbolKind::Class | SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Trait,
        ) {
            return Some(caller.id.clone());
        }
        let mut current = caller.parent_id.clone();
        while let Some(id) = current {
            let symbol = self.symbols.get(&id)?;
            if matches!(
                symbol.kind,
                SymbolKind::Class | SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Trait,
            ) {
                return Some(symbol.id.clone());
            }
            current = symbol.parent_id.clone();
        }
        None
    }

    fn kotlin_symbol_visible_to_caller(
        &self,
        symbol: &GraphSymbol,
        caller: &GraphSymbol,
        caller_file: &FileRecord,
    ) -> bool {
        symbol.file_id == caller.file_id
            || self.kotlin_package_for_file(&symbol.file_id)
                == self.kotlin_package_for_file(&caller.file_id)
            || self
                .imports_for_file(&caller_file.id)
                .any(|import| self.kotlin_import_matches_symbol(import, symbol))
    }
}

/// Whether `symbol` is a top-level (file-rooted) declaration, matching the
/// Java helper's logic but private to the Kotlin module to keep the family
/// self-contained.
fn kotlin_symbol_is_top_level(symbol: &GraphSymbol) -> bool {
    symbol
        .parent_id
        .as_ref()
        .map(|id| id.0.starts_with("file:"))
        .unwrap_or(true)
}

/// Source-root facts for a Kotlin Gradle layout (`src/<set>/kotlin/...`).
///
/// Wired into the bench harness via the `kotlin_project_facts` query kind in
/// `benchmarks/specs/kotlin-smoke-queries.json`; not yet consumed by the
/// in-process resolver, so the rebuild path stays Java-only until phase 2.
#[allow(dead_code)]
pub(crate) fn kotlin_source_root_facts(
    provider: &str,
    kotlin_paths: &[&str],
) -> Vec<(&'static str, String, &'static str)> {
    let mut roots = BTreeSet::new();
    for path in kotlin_paths {
        let segments = path.split('/').collect::<Vec<_>>();
        if segments.len() >= 4 && segments[0] == "src" && segments[2] == "kotlin" {
            let source_set = segments[1];
            roots.insert((source_set.to_string(), format!("src/{source_set}/kotlin")));
        }
        if let Some(root) = kotlin_generated_source_root(path) {
            roots.insert(("generated".to_string(), root));
        }
    }

    let mut facts = Vec::new();
    for (source_set, root) in roots {
        let kind = if source_set == "generated" {
            "generated_exclusion"
        } else if source_set.to_ascii_lowercase().contains("test") {
            "test_root"
        } else {
            "source_root"
        };
        let reason = if provider == "maven" {
            "Maven Kotlin source layout"
        } else {
            "Gradle Kotlin source layout"
        };
        facts.push((kind, format!("{source_set}:{root}"), reason));
    }
    facts
}

/// Generated-source markers that the indexer should treat as
/// `generated_exclusion`. The Kotlin set is a superset of Java's because
/// `src/generated/kotlin/...` is the conventional Kotlin layout, in
/// addition to the Gradle / Maven build paths shared with Java.
#[allow(dead_code)]
pub(crate) fn kotlin_generated_source_root(path: &str) -> Option<String> {
    for marker in [
        "target/generated-sources/",
        "build/generated/source/",
        "build/generated/",
        "generated-src/",
        "src/generated/kotlin/",
    ] {
        if path.starts_with(marker) {
            return Some(marker.trim_end_matches('/').to_string());
        }
    }
    None
}

/// Build-system provider detection mirrors Java's set since Kotlin shares
/// Gradle and Maven tooling. Wrapped under a Kotlin-named alias to keep the
/// family self-contained.
#[allow(dead_code)]
pub(crate) fn kotlin_build_metadata_provider(file: &FileRecord) -> Option<&'static str> {
    match file.relative_path.as_str() {
        "pom.xml" => Some("maven"),
        "build.gradle" | "build.gradle.kts" | "settings.gradle" | "settings.gradle.kts" => {
            Some("gradle")
        }
        _ => None,
    }
}

#[cfg(test)]
#[path = "kotlin_tests.rs"]
mod tests;
