use crate::*;

/// Sentinel alias on a `library` directive (mirrors Dart extractor constant).
const DART_LIBRARY_ALIAS: &str = "__dart_library__";
/// Sentinel alias on a `part 'other.dart';` directive in the host library.
const DART_PART_ALIAS: &str = "__dart_part__";
/// Sentinel alias on a `part of 'main.dart';` (or `part of name;`) directive in
/// a part file.
const DART_PART_OF_ALIAS: &str = "__dart_part_of__";

const DART_ANCESTOR_DEPTH_CAP: usize = 8;

impl SemanticGraph {
    pub(crate) fn caller_is_dart(&self, caller_id: &SymbolId) -> bool {
        self.symbols
            .get(caller_id)
            .and_then(|caller| self.files.get(&caller.file_id))
            .map(|file| file.language == LanguageKind::Dart)
            .unwrap_or(false)
    }

    /// Resolve a Dart method/identifier call that lives inside a class/mixin
    /// body. Walks the host class's `mixin:` / `base:` / `iface:` ancestor
    /// chain (depth cap §4b) looking for a Method/Function with the same name.
    pub(crate) fn dart_inherited_method(
        &self,
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        if !self.caller_is_dart(caller_id) {
            return None;
        }
        // Apply only when the receiver is empty (bare `foo(...)`), `this`,
        // `self`, or `super`. Other receivers go through name-resolution paths
        // and module-prefix dispatch.
        let receiver = call.receiver.as_deref();
        let allow = matches!(receiver, None | Some("this") | Some("super"));
        if !allow {
            return None;
        }
        let class_id = self.dart_class_for_caller(caller_id)?;
        let skip_self = receiver == Some("super");
        if !skip_self && let Some(method) = self.dart_method_on_class(&class_id, &call.name) {
            return Some(method);
        }
        self.dart_method_in_ancestors(&class_id, &call.name, 0)
    }

    /// Find a method/getter/setter named `method_name` declared directly on
    /// `class_id` (or its part-file siblings — Dart libraries can span files
    /// via `part`).
    pub(crate) fn dart_method_on_class(
        &self,
        class_id: &SymbolId,
        method_name: &str,
    ) -> Option<SymbolId> {
        single_symbol(
            self.children_by_parent
                .get(class_id)
                .into_iter()
                .flatten()
                .filter_map(|child_id| self.symbols.get(child_id))
                .filter(|symbol| {
                    matches!(
                        symbol.kind,
                        SymbolKind::Method | SymbolKind::Function | SymbolKind::Test
                    ) && symbol.name == method_name
                })
                .map(|symbol| symbol.id.clone()),
        )
    }

    /// Walk the Dart ancestor chain (mixin -> extends -> implements -> on)
    /// looking for a method named `method_name`. Caps at depth 8 (§4b) and
    /// dedupes ancestors via a visited-set: on a wide diamond hierarchy with
    /// many same-named classes (Flutter's `State`/`Widget`/`Element` trees),
    /// every ancestor name resolves to a fan-out of global candidates, and
    /// without dedup the recursion re-expands shared ancestors along every
    /// path — combinatorial blow-up that made the initial graph build on a
    /// large Dart workspace run for minutes and never finish. The visited-set
    /// only prunes redundant re-traversal; an ancestor's subtree is identical
    /// regardless of the path that reaches it, so the first match in Dart's
    /// resolution order is unchanged and resolution stays correct.
    pub(crate) fn dart_method_in_ancestors(
        &self,
        class_id: &SymbolId,
        method_name: &str,
        depth: usize,
    ) -> Option<SymbolId> {
        let mut visited = std::collections::HashSet::new();
        visited.insert(class_id.clone());
        self.dart_method_in_ancestors_visited(class_id, method_name, depth, &mut visited)
    }

    fn dart_method_in_ancestors_visited(
        &self,
        class_id: &SymbolId,
        method_name: &str,
        depth: usize,
        visited: &mut std::collections::HashSet<SymbolId>,
    ) -> Option<SymbolId> {
        if depth >= DART_ANCESTOR_DEPTH_CAP {
            return None;
        }
        let class = self.symbols.get(class_id)?;
        let (mixins, bases, ifaces, ons) = dart_ancestor_attributes(class);
        // Dart resolution order: mixin chain right-to-left, then superclass,
        // then implements, then `on` constraints.
        for name in mixins
            .iter()
            .rev()
            .chain(bases.iter())
            .chain(ifaces.iter())
            .chain(ons.iter())
        {
            for candidate_class_id in self.dart_class_symbols_by_name(name) {
                // Skip ancestors already expanded on another path: their
                // subtree is identical no matter how we reach them, so a
                // re-walk only burns time (exponentially, on a diamond).
                if !visited.insert(candidate_class_id.clone()) {
                    continue;
                }
                if let Some(method) = self.dart_method_on_class(&candidate_class_id, method_name) {
                    return Some(method);
                }
                if let Some(method) = self.dart_method_in_ancestors_visited(
                    &candidate_class_id,
                    method_name,
                    depth + 1,
                    visited,
                ) {
                    return Some(method);
                }
            }
        }
        None
    }

    /// Find the enclosing class/mixin/extension/extension-type symbol for a
    /// caller (walks up `parent_id`).
    pub(crate) fn dart_class_for_caller(&self, caller_id: &SymbolId) -> Option<SymbolId> {
        let mut current = self.symbols.get(caller_id)?;
        loop {
            if matches!(
                current.kind,
                SymbolKind::Class
                    | SymbolKind::Trait
                    | SymbolKind::Enum
                    | SymbolKind::Struct
                    | SymbolKind::Interface
            ) {
                return Some(current.id.clone());
            }
            let parent_id = current.parent_id.as_ref()?;
            current = self.symbols.get(parent_id)?;
        }
    }

    /// Lookup class-like Dart symbols by name (used for ancestor walks).
    pub(crate) fn dart_class_symbols_by_name(&self, name: &str) -> Vec<SymbolId> {
        self.symbols_by_name
            .get(name)
            .into_iter()
            .flatten()
            .filter(|id| {
                self.symbols
                    .get(*id)
                    .and_then(|symbol| self.files.get(&symbol.file_id).map(|file| (symbol, file)))
                    .map(|(symbol, file)| {
                        file.language == LanguageKind::Dart
                            && matches!(
                                symbol.kind,
                                SymbolKind::Class
                                    | SymbolKind::Trait
                                    | SymbolKind::Enum
                                    | SymbolKind::Struct
                                    | SymbolKind::Interface
                            )
                    })
                    .unwrap_or(false)
            })
            .cloned()
            .collect()
    }

    /// Dart extension dispatch: when a call is `receiver.method(...)`, search
    /// for an `extension X on T` whose method matches and whose `T` is the
    /// receiver's static type (best-effort, requires the receiver to be a
    /// locally-resolvable variable whose declared type is known).
    pub(crate) fn dart_extension_method_call(
        &self,
        candidates: &[SymbolId],
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        if !self.caller_is_dart(caller_id) {
            return None;
        }
        let receiver_text = call.receiver.as_deref()?;
        if matches!(receiver_text, "this" | "super" | "self") {
            return None;
        }
        let receiver_type = self.dart_receiver_static_type(caller_id, receiver_text)?;
        single_symbol(
            candidates
                .iter()
                .filter_map(|id| self.symbols.get(id))
                .filter(|symbol| {
                    matches!(
                        symbol.kind,
                        SymbolKind::Method | SymbolKind::Function | SymbolKind::Test
                    ) && symbol.name == call.name
                })
                .filter(|symbol| {
                    let parent_id = symbol.parent_id.as_ref();
                    parent_id
                        .and_then(|id| self.symbols.get(id))
                        .map(|parent| {
                            parent.language_identity.as_deref() == Some(receiver_type.as_str())
                                && parent
                                    .attributes
                                    .iter()
                                    .any(|attr| attr == "dart:extension")
                        })
                        .unwrap_or(false)
                })
                .map(|symbol| symbol.id.clone()),
        )
    }

    /// Resolve a Dart prefix import: when a call is `prefix.foo(...)` and
    /// `prefix` is declared via `import 'pkg.dart' as prefix;`, restrict the
    /// candidate list to symbols whose file matches the import path suffix.
    pub(crate) fn dart_import_prefix_method_call(
        &self,
        candidates: &[SymbolId],
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        if !self.caller_is_dart(caller_id) {
            return None;
        }
        let receiver = call.receiver.as_deref()?;
        let caller = self.symbols.get(caller_id)?;
        let prefix_import = self.imports_for_file(&caller.file_id).find(|import| {
            import.alias.as_deref() == Some(receiver)
                && import.alias.as_deref() != Some(DART_LIBRARY_ALIAS)
                && import.alias.as_deref() != Some(DART_PART_ALIAS)
                && import.alias.as_deref() != Some(DART_PART_OF_ALIAS)
        })?;
        let path = prefix_import.path.clone();
        let path_suffix = dart_path_suffix(&path);
        single_symbol(
            candidates
                .iter()
                .filter_map(|id| self.symbols.get(id))
                .filter(|symbol| symbol.name == call.name)
                .filter(|symbol| {
                    self.files
                        .get(&symbol.file_id)
                        .map(|file| {
                            file.language == LanguageKind::Dart
                                && file.relative_path.ends_with(&path_suffix)
                        })
                        .unwrap_or(false)
                })
                .map(|symbol| symbol.id.clone()),
        )
    }

    /// Library identifier for a Dart file: either the dotted name from
    /// `library foo.bar;`, or the host file's identifier when the file is a
    /// part (`part of ...`).
    pub fn dart_library_for_file(&self, file_id: &FileId) -> Option<String> {
        if let Some(library) = self
            .imports_for_file(file_id)
            .find(|import| import.alias.as_deref() == Some(DART_LIBRARY_ALIAS))
            .map(|import| import.path.clone())
        {
            return Some(library);
        }
        let host_file_id = self.dart_host_file_for_part(file_id)?;
        if let Some(library) = self
            .imports_for_file(&host_file_id)
            .find(|import| import.alias.as_deref() == Some(DART_LIBRARY_ALIAS))
            .map(|import| import.path.clone())
        {
            return Some(library);
        }
        self.files
            .get(&host_file_id)
            .map(|file| file.relative_path.clone())
    }

    /// File id of the host library for a part file.
    pub fn dart_host_file_for_part(&self, file_id: &FileId) -> Option<FileId> {
        let part_of = self
            .imports_for_file(file_id)
            .find(|import| import.alias.as_deref() == Some(DART_PART_OF_ALIAS))?;
        let part_file = self.files.get(file_id)?;
        let host_path = if dart_uri_looks_like_path(&part_of.path) {
            dart_resolve_relative_path(&part_file.relative_path, &part_of.path)?
        } else {
            self.files
                .values()
                .filter(|file| file.language == LanguageKind::Dart)
                .find(|file| {
                    self.imports_for_file(&file.id).any(|import| {
                        import.alias.as_deref() == Some(DART_LIBRARY_ALIAS)
                            && import.path == part_of.path
                    })
                })
                .map(|file| file.relative_path.clone())?
        };
        let candidate_id = FileId::new(&host_path);
        if self.files.contains_key(&candidate_id) {
            return Some(candidate_id);
        }
        self.files
            .iter()
            .filter(|(_, file)| file.language == LanguageKind::Dart)
            .find(|(_, file)| {
                file.relative_path == host_path || file.relative_path.ends_with(&host_path)
            })
            .map(|(id, _)| id.clone())
    }

    /// Dart receiver-typed dispatch for `local.method(...)`: when the
    /// receiver is a body-local whose static type was recorded as a
    /// `dart-local:<name>:<type>` attribute, look up `method` on that
    /// class (and its `part` siblings via `dart_method_on_class`).
    pub(crate) fn dart_typed_local_method_call(
        &self,
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        if !self.caller_is_dart(caller_id) {
            return None;
        }
        let receiver_text = call.receiver.as_deref()?;
        if matches!(receiver_text, "this" | "super" | "self") {
            return None;
        }
        let receiver_type = self.dart_receiver_static_type(caller_id, receiver_text)?;
        let class_id = self
            .dart_class_symbols_by_name(&receiver_type)
            .into_iter()
            .next()?;
        if let Some(method) = self.dart_method_on_class(&class_id, &call.name) {
            return Some(method);
        }
        self.dart_method_in_ancestors(&class_id, &call.name, 0)
    }

    /// Best-effort static type of a receiver identifier inside a Dart caller.
    /// Resolution order: caller signature (typed parameters), recorded
    /// `dart-local:<name>:<type>` attributes (from typed-or-inferable body
    /// locals), receiver-class fields with a `type:` attribute, then a
    /// literal-receiver heuristic (`'...'.method()` -> String, etc.).
    pub(crate) fn dart_receiver_static_type(
        &self,
        caller_id: &SymbolId,
        receiver: &str,
    ) -> Option<String> {
        let caller = self.symbols.get(caller_id)?;
        // Look for `<Type> <receiver>` or `<Type>? <receiver>` in the signature.
        let signature = &caller.signature;
        if let Some(idx) = signature.find(receiver) {
            // Walk backwards from the receiver name to find a type identifier.
            let before = &signature[..idx];
            let prev_token = before
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .next_back();
            if let Some(token) = prev_token
                && !token.is_empty()
                && token != "final"
                && token != "const"
                && token != "var"
                && token != "late"
            {
                return Some(token.to_string());
            }
        }
        // Look for a `dart-local:<receiver>:<type>` attribute recorded by
        // the parser for typed/inferable body locals.
        if let Some(ty) = caller
            .attributes
            .iter()
            .filter_map(|attr| attr.strip_prefix("dart-local:"))
            .find_map(|attr| attr.strip_prefix(receiver)?.strip_prefix(':'))
        {
            return Some(ty.trim().to_string());
        }
        // Literal-receiver heuristic: `'foo'.method()` has String receiver
        // type, `42.method()` has int, etc. The parser keeps the raw
        // literal text in `call.receiver`, so detect on first char.
        if let Some(ty) = dart_literal_receiver_type(receiver) {
            return Some(ty.to_string());
        }
        // Look for a Field on the receiver's class whose name matches.
        let class_id = self.dart_class_for_caller(caller_id)?;
        let field = self
            .children_by_parent
            .get(&class_id)
            .into_iter()
            .flatten()
            .filter_map(|child_id| self.symbols.get(child_id))
            .find(|symbol| symbol.kind == SymbolKind::Field && symbol.name == receiver)?;
        for attr in &field.attributes {
            if let Some(rest) = attr.strip_prefix("type:") {
                return Some(rest.trim().to_string());
            }
        }
        None
    }
}

/// Map a Dart literal-form receiver text (e.g. `'hi'`, `42`, `3.14`, `true`)
/// onto its built-in static type so extension dispatch can match
/// `extension X on T { ... }` against a literal call site.
fn dart_literal_receiver_type(receiver: &str) -> Option<&'static str> {
    let trimmed = receiver.trim();
    let first = trimmed.chars().next()?;
    match first {
        '\'' | '"' => Some("String"),
        't' if trimmed == "true" => Some("bool"),
        'f' if trimmed == "false" => Some("bool"),
        '0'..='9' => {
            if trimmed.contains('.') || trimmed.contains('e') || trimmed.contains('E') {
                Some("double")
            } else {
                Some("int")
            }
        }
        _ => None,
    }
}

fn dart_ancestor_attributes(symbol: &GraphSymbol) -> (Vec<&str>, Vec<&str>, Vec<&str>, Vec<&str>) {
    let mut mixins = Vec::new();
    let mut bases = Vec::new();
    let mut ifaces = Vec::new();
    let mut ons = Vec::new();
    for attr in &symbol.attributes {
        if let Some(rest) = attr.strip_prefix("mixin:") {
            mixins.push(rest);
        } else if let Some(rest) = attr.strip_prefix("base:") {
            bases.push(rest);
        } else if let Some(rest) = attr.strip_prefix("iface:") {
            ifaces.push(rest);
        } else if let Some(rest) = attr.strip_prefix("mixin-on:") {
            ons.push(rest);
        }
    }
    (mixins, bases, ifaces, ons)
}

fn dart_uri_looks_like_path(uri: &str) -> bool {
    uri.contains('/') || uri.ends_with(".dart") || uri.contains(".dart")
}

fn dart_path_suffix(path: &str) -> String {
    // Drop the `package:foo/` prefix to leave a workspace-relative-ish suffix
    // that's safe to match by `ends_with`.
    if let Some(stripped) = path.strip_prefix("package:") {
        let mut parts = stripped.splitn(2, '/');
        let _package = parts.next();
        if let Some(rest) = parts.next() {
            return rest.to_string();
        }
    }
    path.to_string()
}

/// Resolve a Dart relative URI (`'response.dart'` or `'../shared/foo.dart'`)
/// against a workspace-relative file path.
fn dart_resolve_relative_path(host_relative: &str, uri: &str) -> Option<String> {
    let host_dir: Vec<&str> = host_relative.split('/').collect();
    let dir_segments = &host_dir[..host_dir.len().saturating_sub(1)];
    let mut segments: Vec<String> = dir_segments.iter().map(|s| s.to_string()).collect();
    for part in uri.split('/') {
        match part {
            "" | "." => continue,
            ".." => {
                segments.pop();
            }
            other => segments.push(other.to_string()),
        }
    }
    Some(segments.join("/"))
}
