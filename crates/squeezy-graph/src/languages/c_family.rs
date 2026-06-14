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

    /// Resolve a C/C++ member-method call (`obj.foo()`, `obj->foo()`) by
    /// inferring the static type of the receiver from the caller's parameter
    /// list or from a field of the enclosing class (and its base classes),
    /// then scoping the candidate methods to that class's hierarchy.
    ///
    /// Declines (returns `None`, letting resolution fall through to the
    /// candidate-set path) when:
    ///   * the caller is not C/C++,
    ///   * the receiver is absent, `this`/`super`/`self`, or not a plain
    ///     identifier (a chained/qualified/parenthesised receiver),
    ///   * the receiver type can't be inferred from params or fields,
    ///   * the type name resolves to more than one in-scope class
    ///     (`single_symbol` ambiguity), or
    ///   * the class hierarchy declares the method more than once
    ///     (`single_symbol` overload ambiguity).
    pub(crate) fn cpp_member_method_call(
        &self,
        caller_id: &SymbolId,
        call: &ParsedCall,
    ) -> Option<SymbolId> {
        if !self.caller_is_c_family(caller_id) {
            return None;
        }
        let receiver = call.receiver.as_deref()?;
        // Only plain-identifier receivers carry an inferable static type here.
        // `this`/`self`/`super` go through the same-class / inherited paths;
        // chained or qualified receivers (`a.b`, `Ns::T`, `f()`) aren't a
        // single local/param/field we can type from the signature.
        if matches!(receiver, "this" | "self" | "super")
            || !receiver
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
            || receiver.is_empty()
        {
            return None;
        }
        let receiver_type = self.cpp_receiver_type(caller_id, receiver)?;
        let caller = self.symbols.get(caller_id)?;
        let candidates =
            self.cpp_class_candidates_for_name_in_file(&caller.file_id, &receiver_type);
        let class_id = single_symbol(candidates.into_iter())?;
        self.cpp_method_on_class_or_bases(&class_id, &call.name)
    }

    /// True when the caller lives in a C or C++ source file.
    pub(crate) fn caller_is_c_family(&self, caller_id: &SymbolId) -> bool {
        self.symbols
            .get(caller_id)
            .and_then(|caller| self.files.get(&caller.file_id))
            .map(|file| matches!(file.language, LanguageKind::C | LanguageKind::Cpp))
            .unwrap_or(false)
    }

    /// Best-effort static type name of a receiver identifier inside a C/C++
    /// caller. Resolution order: the caller's parameter list (`Type recv`,
    /// `Type* recv`, `const Type& recv`), then a field named `recv` on the
    /// enclosing class or one of its base classes. The returned name is the
    /// final `::`-segment with pointer/reference/cv decoration stripped.
    pub(crate) fn cpp_receiver_type(&self, caller_id: &SymbolId, receiver: &str) -> Option<String> {
        let caller = self.symbols.get(caller_id)?;
        if let Some(ty) = cpp_param_type_in_signature(&caller.signature, receiver) {
            return Some(ty);
        }
        let class_id = self.cpp_class_for_caller(caller_id)?;
        self.cpp_field_type_on_class_or_bases(&class_id, receiver, 0)
    }

    /// Walk up `parent_id` from the caller to the enclosing class/struct/union.
    pub(crate) fn cpp_class_for_caller(&self, caller_id: &SymbolId) -> Option<SymbolId> {
        let mut current = self.symbols.get(caller_id)?;
        loop {
            if matches!(
                current.kind,
                SymbolKind::Class | SymbolKind::Struct | SymbolKind::Union
            ) {
                return Some(current.id.clone());
            }
            let parent_id = current.parent_id.as_ref()?;
            current = self.symbols.get(parent_id)?;
        }
    }

    /// Find the declared type of a field named `field_name` on `class_id` or,
    /// failing that, on any of its base classes (depth-capped, cycle-safe).
    fn cpp_field_type_on_class_or_bases(
        &self,
        class_id: &SymbolId,
        field_name: &str,
        depth: usize,
    ) -> Option<String> {
        if depth >= CPP_BASE_DEPTH_CAP {
            return None;
        }
        if let Some(field) = self
            .children_by_parent
            .get(class_id)
            .into_iter()
            .flatten()
            .filter_map(|child_id| self.symbols.get(child_id))
            .find(|symbol| symbol.kind == SymbolKind::Field && symbol.name == field_name)
            && let Some(ty) = cpp_field_type_from_signature(&field.signature, field_name)
        {
            return Some(ty);
        }
        let class = self.symbols.get(class_id)?;
        for base_name in cpp_base_class_names(&class.signature) {
            for base_id in self.cpp_class_candidates_for_name_in_file(&class.file_id, &base_name) {
                if let Some(ty) =
                    self.cpp_field_type_on_class_or_bases(&base_id, field_name, depth + 1)
                {
                    return Some(ty);
                }
            }
        }
        None
    }

    /// Resolve a type name to the C/C++ class/struct/union declaration(s)
    /// visible from `file_id`: declarations in the same file, in a file pulled
    /// in by an `#include` directive, or that shares the include translation
    /// unit root with one. Scoping keeps a same-named class in an unrelated
    /// header from hijacking the edge.
    pub(crate) fn cpp_class_candidates_for_name_in_file(
        &self,
        file_id: &FileId,
        name: &str,
    ) -> Vec<SymbolId> {
        let direct_name = last_path_segment(name);
        let include_paths = self
            .imports_for_file(file_id)
            .filter(|import| import.provenance.reason.contains("include directive"))
            .map(|import| import.path.clone())
            .collect::<Vec<_>>();
        let mut ids = self
            .symbols_by_name_or_scan(&direct_name)
            .into_iter()
            .filter_map(|id| self.symbols.get(&id))
            .filter(|symbol| {
                matches!(
                    symbol.kind,
                    SymbolKind::Class | SymbolKind::Struct | SymbolKind::Union
                )
            })
            .filter(|symbol| {
                self.files
                    .get(&symbol.file_id)
                    .map(|file| matches!(file.language, LanguageKind::C | LanguageKind::Cpp))
                    .unwrap_or(false)
            })
            .filter(|symbol| {
                symbol.file_id == *file_id
                    || self
                        .files
                        .get(&symbol.file_id)
                        .map(|file| {
                            include_paths.iter().any(|include| {
                                include_path_matches_file(include, &file.relative_path)
                                    || file_shares_include_root(include, &file.relative_path)
                            })
                        })
                        .unwrap_or(false)
            })
            .map(|symbol| symbol.id.clone())
            .collect::<Vec<_>>();
        ids.sort_by(|left, right| left.0.cmp(&right.0));
        ids.dedup();
        ids
    }

    /// Find a method named `method_name` declared directly on `class_id` or,
    /// failing that, on a base class (depth-capped, cycle-safe). Returns `None`
    /// when the name is overloaded within the searched scope so the caller can
    /// decline rather than forge an arbitrary edge.
    pub(crate) fn cpp_method_on_class_or_bases(
        &self,
        class_id: &SymbolId,
        method_name: &str,
    ) -> Option<SymbolId> {
        let mut visited = std::collections::HashSet::new();
        visited.insert(class_id.clone());
        self.cpp_method_on_class_or_bases_visited(class_id, method_name, 0, &mut visited)
    }

    fn cpp_method_on_class_or_bases_visited(
        &self,
        class_id: &SymbolId,
        method_name: &str,
        depth: usize,
        visited: &mut std::collections::HashSet<SymbolId>,
    ) -> Option<SymbolId> {
        if depth >= CPP_BASE_DEPTH_CAP {
            return None;
        }
        if let Some(method) = self.cpp_method_on_class(class_id, method_name) {
            return Some(method);
        }
        let class = self.symbols.get(class_id)?;
        let class_file_id = class.file_id.clone();
        for base_name in cpp_base_class_names(&class.signature) {
            for base_id in self.cpp_class_candidates_for_name_in_file(&class_file_id, &base_name) {
                if !visited.insert(base_id.clone()) {
                    continue;
                }
                if let Some(method) = self.cpp_method_on_class_or_bases_visited(
                    &base_id,
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

    /// A method named `method_name` declared directly on `class_id`. Declines
    /// (returns `None`) when the class declares the name more than once
    /// (overload set) so member-call resolution stays conservative.
    fn cpp_method_on_class(&self, class_id: &SymbolId, method_name: &str) -> Option<SymbolId> {
        single_symbol(
            self.children_by_parent
                .get(class_id)
                .into_iter()
                .flatten()
                .filter_map(|child_id| self.symbols.get(child_id))
                .filter(|symbol| symbol.kind == SymbolKind::Method && symbol.name == method_name)
                .map(|symbol| symbol.id.clone()),
        )
    }
}

/// Depth cap for the C++ base-class walk. Mirrors the other language ancestor
/// caps: deep diamond hierarchies are rare and a bounded walk keeps a
/// malformed cyclic `base` chain from running away even past the visited-set
/// guard.
const CPP_BASE_DEPTH_CAP: usize = 8;

/// Extract the static type of a parameter named `receiver` from a C/C++
/// function/method signature. Looks inside the (outermost) parameter list,
/// splits on top-level commas, and for a parameter whose trailing identifier
/// is `receiver` returns the preceding type token (final `::`-segment, with
/// `*`/`&`/cv-qualifiers stripped). Returns `None` for unnamed params,
/// builtin types, or when no parameter matches.
fn cpp_param_type_in_signature(signature: &str, receiver: &str) -> Option<String> {
    let open = signature.find('(')?;
    let rest = &signature[open + 1..];
    let close = matching_paren(rest)?;
    let params = &rest[..close];
    for param in split_top_level(params, ',') {
        let param = param.trim();
        if param.is_empty() {
            continue;
        }
        // The declared name is the last identifier token; everything before it
        // (minus decoration) is the type.
        let Some(name_idx) = last_identifier_index(param) else {
            continue;
        };
        let (type_part, name_part) = param.split_at(name_idx);
        if name_part != receiver {
            continue;
        }
        if let Some(ty) = cpp_clean_type(type_part) {
            return Some(ty);
        }
    }
    None
}

/// Extract the declared type from a field signature such as `Foo m_obj;`,
/// `Foo* m_obj`, or `const Foo& obj` for the field named `field_name`.
fn cpp_field_type_from_signature(signature: &str, field_name: &str) -> Option<String> {
    // Drop the trailing `;` / initializer / brace so only the
    // `<type> <name>` declarator head remains. We deliberately do NOT split on
    // `:` so a scoped type (`ns::Foo m_obj`) survives; a `: bitfield` width is
    // rare and the trailing-identifier logic tolerates it.
    let head = signature
        .split([';', '=', '{'])
        .next()
        .unwrap_or(signature)
        .trim();
    let idx = find_identifier(head, field_name)?;
    let type_part = &head[..idx];
    cpp_clean_type(type_part)
}

/// Reduce a raw type prefix (`const Foo&`, `Bar *`, `ns::Baz`) to its bare
/// final type identifier, returning `None` for empty / builtin / non-type
/// results so callers don't try to resolve `int`, `void`, etc. to a class.
fn cpp_clean_type(type_part: &str) -> Option<String> {
    let last = type_part
        .split(|ch: char| {
            !(ch.is_ascii_alphanumeric() || ch == '_' || ch == ':' || ch == '<' || ch == '>')
        })
        .filter(|token| !token.is_empty())
        .rfind(|token| !cpp_type_modifier(token))?;
    // Strip a template argument list: `vector<Foo>` -> `vector`.
    let bare = last.split('<').next().unwrap_or(last);
    let name = bare.rsplit("::").next().unwrap_or(bare).trim();
    if name.is_empty() || !looks_like_cpp_type(name) {
        return None;
    }
    Some(name.to_string())
}

/// Type-position keywords/qualifiers that are never the class name itself.
fn cpp_type_modifier(token: &str) -> bool {
    matches!(
        token,
        "const"
            | "volatile"
            | "mutable"
            | "static"
            | "register"
            | "struct"
            | "class"
            | "enum"
            | "union"
            | "typename"
            | "unsigned"
            | "signed"
    )
}

/// A name worth resolving to a class: UpperCamel, ends in `_t`, or contains a
/// namespace separator. Excludes obvious builtins so `int x` never tries to
/// resolve `int` to a class symbol.
fn looks_like_cpp_type(name: &str) -> bool {
    if matches!(
        name,
        "auto"
            | "bool"
            | "char"
            | "double"
            | "float"
            | "int"
            | "long"
            | "short"
            | "size_t"
            | "void"
            | "wchar_t"
    ) {
        return false;
    }
    name.chars()
        .next()
        .map(|ch| ch.is_ascii_uppercase())
        .unwrap_or(false)
        || name.ends_with("_t")
        || name.contains("::")
}

/// Base-class names from a class signature (`class D : public B, private C`).
/// Returns the bare final `::`-segment of each base, dropping access
/// specifiers and `virtual`. Empty when the class has no base list.
fn cpp_base_class_names(signature: &str) -> Vec<String> {
    let Some(colon) = signature.find(':') else {
        return Vec::new();
    };
    // Guard against `::` in the class name itself (out-of-line / qualified):
    // a base list begins with a single `:` not part of a `::` token.
    let bytes = signature.as_bytes();
    if bytes.get(colon + 1) == Some(&b':') || (colon > 0 && bytes[colon - 1] == b':') {
        // The first `:` is part of a `::`; scan for a standalone `:` after it.
        return cpp_base_class_names_after_scope(signature);
    }
    cpp_base_list_names(&signature[colon + 1..])
}

fn cpp_base_class_names_after_scope(signature: &str) -> Vec<String> {
    let chars: Vec<char> = signature.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == ':' {
            let prev = if i > 0 { chars[i - 1] } else { ' ' };
            let next = chars.get(i + 1).copied().unwrap_or(' ');
            if prev != ':' && next != ':' {
                let tail: String = chars[i + 1..].iter().collect();
                return cpp_base_list_names(&tail);
            }
        }
        i += 1;
    }
    Vec::new()
}

fn cpp_base_list_names(base_list: &str) -> Vec<String> {
    let mut names = Vec::new();
    for entry in split_top_level(base_list, ',') {
        let last = entry
            .split(|ch: char| {
                !(ch.is_ascii_alphanumeric() || ch == '_' || ch == ':' || ch == '<' || ch == '>')
            })
            .filter(|token| !token.is_empty())
            .rfind(|token| !matches!(*token, "public" | "private" | "protected" | "virtual"));
        if let Some(token) = last {
            let bare = token.split('<').next().unwrap_or(token);
            let name = bare.rsplit("::").next().unwrap_or(bare).trim();
            if !name.is_empty() {
                names.push(name.to_string());
            }
        }
    }
    names
}

/// Byte offset of the `)` that closes the `(` the slice starts just past,
/// accounting for nesting (`(int (*fn)(int))`). Returns `None` if unbalanced.
fn matching_paren(after_open: &str) -> Option<usize> {
    let mut depth = 1usize;
    for (idx, ch) in after_open.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(idx);
                }
            }
            _ => {}
        }
    }
    None
}

/// Split `text` on `sep` while ignoring separators nested inside `()`, `<>`,
/// or `[]` — so a `,` inside `map<int, Foo>` or a default `f(a, b)` doesn't
/// break a parameter / base-list entry apart.
fn split_top_level(text: &str, sep: char) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth_paren = 0i32;
    let mut depth_angle = 0i32;
    let mut depth_square = 0i32;
    let mut start = 0usize;
    for (idx, ch) in text.char_indices() {
        match ch {
            '(' => depth_paren += 1,
            ')' => depth_paren -= 1,
            '<' => depth_angle += 1,
            '>' => depth_angle -= 1,
            '[' => depth_square += 1,
            ']' => depth_square -= 1,
            c if c == sep && depth_paren <= 0 && depth_angle <= 0 && depth_square <= 0 => {
                parts.push(text[start..idx].to_string());
                start = idx + c.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(text[start..].to_string());
    parts
}

/// Byte offset of the last identifier (`[A-Za-z_][A-Za-z0-9_]*`) token in a
/// declarator, skipping a trailing `[...]` array suffix. Used to peel the
/// parameter/field name off `Type name` so the remainder is the type.
fn last_identifier_index(decl: &str) -> Option<usize> {
    // Drop a trailing array dimension so `Foo arr[4]` peels `arr`, not `4`.
    let decl = decl.split('[').next().unwrap_or(decl).trim_end();
    let bytes = decl.as_bytes();
    let mut end = bytes.len();
    // Trim trailing non-identifier chars (`&`, `*`, spaces).
    while end > 0 && !is_ident_byte(bytes[end - 1]) {
        end -= 1;
    }
    if end == 0 {
        return None;
    }
    let mut start = end;
    while start > 0 && is_ident_byte(bytes[start - 1]) {
        start -= 1;
    }
    // A bare identifier with no preceding type isn't a `Type name` pair.
    if start == 0 {
        return None;
    }
    Some(start)
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}
