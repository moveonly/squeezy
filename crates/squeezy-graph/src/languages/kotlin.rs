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
//
// Filled in by langs/kotlin-deferred (extractor-side, see
// `crates/squeezy-parse/src/languages/kotlin.rs`):
//   - delegated-property accessor binding (spec §4g): delegate target is
//     emitted as a `ParsedCall` whose `caller_id` is the property symbol.
//   - sealed-class child enumeration (spec §4f): nested children of a
//     `sealed` parent emit a Type `ParsedReference` to the parent, so
//     `references_to_symbol(Parent)` already returns the sibling set
//     without a dedicated resolver helper.
//   - `inline reified` type-parameter modeling (spec §4d): each reified
//     type-parameter name lands in `language_identity` so a future
//     resolver can match call-site type arguments against the function.

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
        if matches!(receiver, "this" | "super") {
            return None;
        }
        let caller = self.symbols.get(caller_id)?;
        let caller_file = self.files.get(&caller.file_id)?;
        if caller_file.language != LanguageKind::Kotlin {
            return None;
        }

        // Infer the receiver's *type name*. A literal receiver (`"x".foo()`,
        // `42.bar()`) binds to its built-in type; a property of the caller's
        // enclosing class with a `type:T` attribute uses T; otherwise fall
        // back to the raw receiver text (covers cases like `Foo.bar()` where
        // the receiver is itself a type name).
        let receiver_type = kotlin_literal_receiver_type(receiver)
            .map(str::to_string)
            .or_else(|| {
                if receiver.contains(' ') {
                    None
                } else {
                    self.kotlin_receiver_type_name(caller_id, receiver)
                }
            })
            .unwrap_or_else(|| {
                if receiver.contains(' ') {
                    String::new()
                } else {
                    last_path_segment(receiver)
                }
            });
        if receiver_type.is_empty() {
            return None;
        }

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
                            // §4d: extension+reified language_identity is
                            // shaped `<receiver>;reified:<params>`. Match
                            // only on the receiver half so the existing
                            // extension-function routing keeps working
                            // alongside reified-type-arg matching.
                            let receiver_half = identity
                                .split_once(';')
                                .map(|(head, _)| head)
                                .unwrap_or(identity);
                            receiver_half == receiver_type
                                || last_path_segment(receiver_half) == receiver_type
                        })
                        .unwrap_or(false)
            })
            .filter(|symbol| self.kotlin_symbol_visible_to_caller(symbol, caller, caller_file))
            .map(|symbol| symbol.id.clone());

        single_symbol(matches)
    }

    /// Resolve `receiver.method()` where `receiver` is a property of the
    /// caller's enclosing class with a declared type, binding to an instance
    /// method declared on that type or one of its supertype ancestors.
    ///
    /// This is the Kotlin analogue of `java_receiver_field_method`: it infers
    /// the receiver's type from the field's `type:T` attribute, resolves `T`
    /// to a class in the caller's file scope, and looks up the method on that
    /// class or its `base:` ancestors so inherited methods resolve.
    pub(crate) fn kotlin_receiver_field_method(
        &self,
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        let receiver = call.receiver.as_deref()?;
        if matches!(receiver, "this" | "super") || receiver.contains(' ') || receiver.contains('(')
        {
            return None;
        }
        let caller = self.symbols.get(caller_id)?;
        let caller_file = self.files.get(&caller.file_id)?;
        if caller_file.language != LanguageKind::Kotlin {
            return None;
        }
        let type_name = self.kotlin_receiver_type_name(caller_id, receiver)?;
        let class_id = self
            .kotlin_class_candidates_for_name_in_file(&caller.file_id, &type_name)
            .first()?
            .clone();
        self.kotlin_method_on_class_or_ancestors(&class_id, &call.name)
    }

    /// Class/object symbols named `name` that are visible from `file_id`
    /// (same file, same package, or imported). Mirrors
    /// `java_class_candidates_for_name_in_file` for the Kotlin family.
    fn kotlin_class_candidates_for_name_in_file(
        &self,
        file_id: &FileId,
        name: &str,
    ) -> Vec<SymbolId> {
        let direct_name = last_path_segment(name);
        let mut class_ids = self
            .symbols_by_name_or_scan(&direct_name)
            .into_iter()
            .filter_map(|id| self.symbols.get(&id))
            .filter(|symbol| {
                matches!(
                    symbol.kind,
                    SymbolKind::Class | SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Trait
                )
            })
            .filter(|symbol| {
                symbol.file_id == *file_id
                    || self.kotlin_package_for_file(&symbol.file_id)
                        == self.kotlin_package_for_file(file_id)
                    || self
                        .imports_for_file(file_id)
                        .any(|import| self.kotlin_import_matches_symbol(import, symbol))
            })
            .map(|symbol| symbol.id.clone())
            .collect::<Vec<_>>();
        class_ids.sort_by(|left, right| left.0.cmp(&right.0));
        class_ids.dedup();
        class_ids
    }

    /// Single method named `method_name` declared directly on `class_id`.
    fn kotlin_method_on_class(&self, class_id: &SymbolId, method_name: &str) -> Option<SymbolId> {
        single_symbol(
            self.children_by_parent
                .get(class_id)?
                .iter()
                .filter_map(|child_id| self.symbols.get(child_id))
                .filter(|symbol| symbol.kind == SymbolKind::Method && symbol.name == method_name)
                .map(|symbol| symbol.id.clone()),
        )
    }

    /// Look up `method_name` on `class_id`, falling back to its `base:`
    /// supertype ancestors. Kotlin records inheritance as `base:<name>`
    /// attributes (no `Extends`/`Implements` graph edges), so we resolve each
    /// base name to a class in the same file scope and recurse, with a
    /// visited-set bounding cyclic / diamond hierarchies.
    fn kotlin_method_on_class_or_ancestors(
        &self,
        class_id: &SymbolId,
        method_name: &str,
    ) -> Option<SymbolId> {
        let mut visited = std::collections::HashSet::new();
        visited.insert(class_id.clone());
        self.kotlin_method_on_class_or_ancestors_visited(class_id, method_name, &mut visited)
    }

    fn kotlin_method_on_class_or_ancestors_visited(
        &self,
        class_id: &SymbolId,
        method_name: &str,
        visited: &mut std::collections::HashSet<SymbolId>,
    ) -> Option<SymbolId> {
        if let Some(method) = self.kotlin_method_on_class(class_id, method_name) {
            return Some(method);
        }
        let class = self.symbols.get(class_id)?;
        let class_file_id = class.file_id.clone();
        let base_names = class
            .attributes
            .iter()
            .filter_map(|attribute| attribute.strip_prefix("base:"))
            .map(str::to_string)
            .collect::<Vec<_>>();
        for base_name in base_names {
            for ancestor_id in
                self.kotlin_class_candidates_for_name_in_file(&class_file_id, &base_name)
            {
                if !visited.insert(ancestor_id.clone()) {
                    continue;
                }
                if let Some(method) = self.kotlin_method_on_class_or_ancestors_visited(
                    &ancestor_id,
                    method_name,
                    visited,
                ) {
                    return Some(method);
                }
            }
        }
        None
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

/// Map a literal-shaped extension-call receiver to the Kotlin built-in
/// type the extractor records as `language_identity` for that extension
/// (`String`, `Int`, `Long`, `Float`, `Double`, `Boolean`, `Char`). The
/// receiver text arrives raw from tree-sitter (e.g. `"hello"`, `42`,
/// `1.5f`, `true`, `'x'`); we pattern-match on the literal form. Returns
/// `None` if the receiver does not look like a literal.
fn kotlin_literal_receiver_type(receiver: &str) -> Option<&'static str> {
    let trimmed = receiver.trim();
    if trimmed.is_empty() {
        return None;
    }
    // String literals: regular `"…"`, raw `"""…"""`, or any
    // string-template shape — anything starting with `"` is a `String`.
    if trimmed.starts_with('"') {
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
    // Numeric literals. Trailing-letter suffix wins; otherwise a `.` makes
    // it a `Double`, else `Int`.
    let first = trimmed.chars().next()?;
    if first.is_ascii_digit() || (first == '-' && trimmed.len() > 1) {
        let last = trimmed.chars().last()?;
        if last == 'L' {
            return Some("Long");
        }
        if last == 'f' || last == 'F' {
            return Some("Float");
        }
        if trimmed.contains('.') {
            return Some("Double");
        }
        return Some("Int");
    }
    None
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
/// `benchmarks/specs/kotlin-smoke-queries.json` and consumed by
/// `SemanticGraph::rebuild_kotlin_project_facts`.
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
pub(crate) fn kotlin_build_metadata_provider(file: &FileRecord) -> Option<&'static str> {
    match file.relative_path.as_str() {
        "pom.xml" => Some("maven"),
        "build.gradle" | "build.gradle.kts" | "settings.gradle" | "settings.gradle.kts" => {
            Some("gradle")
        }
        _ => None,
    }
}

/// FNV-1a fingerprint over the Kotlin source path set; mirrors
/// `java_paths_signature` so the rebuild cache invalidates whenever Kotlin
/// source layout changes.
pub(crate) fn kotlin_paths_signature(paths: &[String]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x00000100000001b3;
    let mut hash = FNV_OFFSET;
    for path in paths {
        for byte in path.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        hash ^= 0x00;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Configured-source extraction from the build script. Gradle's
/// `srcDir(...)` lines parse the same way whether the DSL is Groovy or
/// Kotlin, and Maven's `<sourceDirectory>` tags are XML-only, so both
/// providers reuse the existing Java helpers.
pub(crate) fn kotlin_configured_source_facts(
    provider: &str,
    source: &str,
) -> Vec<(&'static str, String, &'static str)> {
    match provider {
        "maven" => super::java::maven_configured_source_facts(source),
        "gradle" => super::java::gradle_configured_source_facts(source),
        _ => Vec::new(),
    }
}

/// Dependency-coordinate extraction. Gradle and Maven coordinate shapes are
/// language-agnostic at the build-metadata layer, so reuse the existing Java
/// extractors verbatim.
pub(crate) fn kotlin_dependency_facts(provider: &str, source: &str) -> Vec<String> {
    match provider {
        "maven" => super::java::maven_dependency_facts(source),
        "gradle" => super::java::gradle_dependency_facts(source),
        _ => Vec::new(),
    }
}

#[cfg(test)]
#[path = "kotlin_tests.rs"]
mod tests;
