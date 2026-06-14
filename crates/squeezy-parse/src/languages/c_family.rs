use std::collections::{HashMap, HashSet};

use crate::languages::rust::*;
use crate::*;

pub(crate) fn extract_c_family(file: FileRecord, source: &str, tree: &Tree) -> ParsedFile {
    let mut ctx = ExtractContext::new(file.clone(), source);
    let root = tree.root_node();
    record_parse_error_diagnostics(root, &mut ctx);

    visit_c_family_node(root, &mut ctx, None, None);
    dedup_c_family_facts(&mut ctx);
    collapse_c_family_function_decls(&mut ctx);

    ParsedFile {
        file,
        package: None,
        symbols: ctx.symbols,
        imports: ctx.imports,
        calls: ctx.calls,
        references: ctx.references,
        body_hits: ctx.body_hits,
        unsupported: None,
        diagnostics: ctx.diagnostics,
        changed_ranges: Vec::new(),
    }
}

pub(crate) fn dedup_c_family_facts(ctx: &mut ExtractContext<'_>) {
    type ImportKey = (String, Option<String>, String, Option<String>, bool);
    type CallKey = (String, Option<String>, String, u32, u32);
    type ReferenceKey = (String, Option<String>, String, ReferenceKind, u32, u32);

    let mut symbols: HashSet<String> = HashSet::with_capacity(ctx.symbols.len());
    ctx.symbols
        .retain(|symbol| symbols.insert(symbol.id.0.clone()));

    let mut imports: HashSet<ImportKey> = HashSet::with_capacity(ctx.imports.len());
    ctx.imports.retain(|import| {
        imports.insert((
            import.file_id.0.clone(),
            import.owner_id.as_ref().map(|id| id.0.clone()),
            import.path.clone(),
            import.alias.clone(),
            import.is_reexport,
        ))
    });

    let mut calls: HashSet<CallKey> = HashSet::with_capacity(ctx.calls.len());
    ctx.calls.retain(|call| {
        calls.insert((
            call.file_id.0.clone(),
            call.caller_id.as_ref().map(|id| id.0.clone()),
            call.target_text.clone(),
            call.span.start_byte,
            call.span.end_byte,
        ))
    });

    let mut references: HashSet<ReferenceKey> = HashSet::with_capacity(ctx.references.len());
    ctx.references.retain(|reference| {
        references.insert((
            reference.file_id.0.clone(),
            reference.owner_id.as_ref().map(|id| id.0.clone()),
            reference.text.clone(),
            reference.kind,
            reference.span.start_byte,
            reference.span.end_byte,
        ))
    });
}

/// Collapse `(file, parent, kind, name)` Function/Method symbol pairs that
/// have one forward declaration and one definition into a single symbol.
/// Tree-sitter sees the forward declaration as `declaration` and the
/// definition as `function_definition`; both create independent
/// `ParsedSymbol`s with distinct spans, but clang's AST oracle reports
/// only one canonical declaration in the main translation unit. Keeping
/// the definition (or, if there is no definition, the declaration with
/// the widest signature) keeps the symbol set aligned with clang and
/// preserves the most useful span for downstream queries.
pub(crate) fn collapse_c_family_function_decls(ctx: &mut ExtractContext<'_>) {
    type FunctionGroupKey = (String, Option<String>, SymbolKind, String);
    let mut groups: HashMap<FunctionGroupKey, Vec<usize>> = HashMap::new();
    for (index, symbol) in ctx.symbols.iter().enumerate() {
        if !matches!(symbol.kind, SymbolKind::Function | SymbolKind::Method) {
            continue;
        }
        groups
            .entry((
                symbol.file_id.0.clone(),
                symbol.parent_id.as_ref().map(|id| id.0.clone()),
                symbol.kind,
                symbol.name.clone(),
            ))
            .or_default()
            .push(index);
    }

    let mut drop_indexes: HashSet<usize> = HashSet::new();
    for (_, indexes) in groups {
        if indexes.len() <= 1 {
            continue;
        }
        let preferred = pick_canonical_function_symbol(&indexes, &ctx.symbols);
        for index in indexes {
            if index != preferred {
                drop_indexes.insert(index);
            }
        }
    }

    if drop_indexes.is_empty() {
        return;
    }
    let mut index = 0;
    ctx.symbols.retain(|_| {
        let keep = !drop_indexes.contains(&index);
        index += 1;
        keep
    });
}

pub(crate) fn pick_canonical_function_symbol(indexes: &[usize], symbols: &[ParsedSymbol]) -> usize {
    let mut best = indexes[0];
    let mut best_score = -1i64;
    for index in indexes {
        let symbol = &symbols[*index];
        let mut score = 0i64;
        if symbol.body_span.is_some() {
            score += 1_000;
        }
        score += symbol.signature.len() as i64;
        if score > best_score {
            best_score = score;
            best = *index;
        }
    }
    best
}

pub(crate) fn visit_c_family_node(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
    owner_symbol: Option<&SymbolId>,
) {
    if node.is_missing() {
        record_missing_node_diagnostic(node, ctx);
        return;
    }

    let kind = node.kind();
    if kind == "preproc_include" {
        extract_c_include(node, ctx, owner_symbol.cloned());
    } else if matches!(kind, "using_declaration" | "using_directive") {
        extract_c_using(node, ctx, owner_symbol.cloned());
    } else if kind == "friend_declaration" {
        extract_c_family_friend(node, ctx, parent_symbol.map(|(id, _)| id.clone()));
    }

    if let Some(symbol) = c_family_symbol_from_node(node, ctx, parent_symbol) {
        extract_c_family_symbol_facts(node, &symbol, ctx);
        let symbol_pair = (symbol.id.clone(), symbol.kind);
        let next_parent_owned = if c_family_symbol_can_own_children(symbol.kind) {
            Some(symbol_pair)
        } else {
            None
        };
        let next_owner_owned = if symbol.body_span.is_some() {
            Some(symbol.id.clone())
        } else {
            None
        };
        ctx.symbols.push(symbol);
        let next_parent = next_parent_owned.as_ref().or(parent_symbol);
        let next_owner = next_owner_owned.as_ref().or(owner_symbol);
        visit_c_family_children(node, ctx, next_parent, next_owner);
        return;
    }

    if kind == "call_expression" {
        extract_c_family_call(node, ctx, owner_symbol.cloned());
    } else if matches!(kind, "preproc_call" | "preproc_function_def") {
        extract_c_macro_call(node, ctx, owner_symbol.cloned());
    } else if let Some(reference_kind) = c_family_reference_kind(node) {
        extract_c_family_reference(node, reference_kind, ctx, owner_symbol.cloned());
    } else if is_c_family_literal(kind) {
        extract_body_hit(node, BodyHitKind::Literal, ctx, owner_symbol.cloned());
    }

    visit_c_family_children(node, ctx, parent_symbol, owner_symbol);
}

pub(crate) fn visit_c_family_children(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
    owner_symbol: Option<&SymbolId>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit_c_family_node(child, ctx, parent_symbol, owner_symbol);
    }
}

pub(crate) fn c_family_symbol_from_node(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> Option<ParsedSymbol> {
    let mut kind = match node.kind() {
        "namespace_definition" => SymbolKind::Module,
        "class_specifier" => SymbolKind::Class,
        "struct_specifier" => SymbolKind::Struct,
        "union_specifier" => SymbolKind::Union,
        "enum_specifier" => SymbolKind::Enum,
        "enumerator" => SymbolKind::Variant,
        "function_definition" => SymbolKind::Function,
        "declaration" if c_declaration_is_function(node) => SymbolKind::Function,
        "declaration" if c_family_is_global_value_declaration(node) => {
            if c_family_declaration_is_const(node, ctx.source) {
                SymbolKind::Const
            } else {
                SymbolKind::Static
            }
        }
        "field_declaration" if c_declaration_is_function(node) => SymbolKind::Function,
        "field_declaration" => SymbolKind::Field,
        "type_definition" | "alias_declaration" => SymbolKind::TypeAlias,
        "preproc_def" | "preproc_function_def" => SymbolKind::Macro,
        _ => return None,
    };
    if kind == SymbolKind::Function
        && parent_symbol
            .map(|(_, parent_kind)| c_family_symbol_can_own_members(*parent_kind))
            .unwrap_or(false)
    {
        kind = SymbolKind::Method;
    }
    if kind == SymbolKind::Function
        && c_family_function_declarator_qualifier(node, ctx.source)
            .as_deref()
            .map(qualifier_is_type_like)
            .unwrap_or(false)
    {
        kind = SymbolKind::Method;
    }

    let mut name = c_family_symbol_name(node, kind, ctx.source)?;
    if name.is_empty() {
        return None;
    }

    // gtest / Boost.Test macros (`TEST(Suite, Name) { … }`,
    // `BOOST_AUTO_TEST_CASE(name) { … }`) parse as `function_definition` whose
    // declarator name is the macro and whose arguments are the suite/case
    // identifiers. Reclassify them as `Test` and name them after the macro
    // arguments so `decl_search kind=test` and the test-impact queries see
    // them. Only top-level (non-member) definitions qualify; a method literally
    // called `TEST` should stay a method.
    let mut test_macro = false;
    if kind == SymbolKind::Function
        && node.kind() == "function_definition"
        && c_family_is_test_macro_name(&name)
        && let Some(test_name) = c_family_test_macro_label(node, &name, ctx.source)
    {
        name = test_name;
        kind = SymbolKind::Test;
        test_macro = true;
    }

    let body = c_family_body_node(node);
    let span = span_from_node(node);
    let body_span = body.map(span_from_node);
    let signature_span = signature_span_from_nodes(node, body);
    let signature = signature_text(node, body, ctx.source);
    let parent_id = parent_symbol.map(|(id, _)| id.clone());
    let mut attributes = c_family_attributes_for_node(node, kind, &signature);
    attributes.extend(c_family_base_class_attributes(node, kind, ctx.source));
    if matches!(kind, SymbolKind::Function | SymbolKind::Method)
        && let Some(role) = c_family_special_member_role(node, &name, ctx.source)
    {
        attributes.push(role.to_string());
    }
    if kind == SymbolKind::Field {
        attributes.extend(c_family_bitfield_attributes(node, ctx.source));
    }
    if matches!(kind, SymbolKind::Function | SymbolKind::Method)
        && c_family_has_c_linkage(node, ctx.source)
    {
        attributes.push("c++:c-linkage".to_string());
    }
    if test_macro {
        attributes.push("c-family:test".to_string());
    }
    attributes.sort();
    attributes.dedup();
    let confidence = c_family_symbol_confidence(node, &attributes);
    Some(ParsedSymbol {
        id: symbol_id(&ctx.file, parent_id.as_ref(), kind, &name, span),
        file_id: ctx.file.id.clone(),
        parent_id,
        name,
        kind,
        language_identity: None,
        span,
        body_span,
        signature_span,
        signature,
        visibility: c_family_visibility_text(node, ctx.source),
        docs: Vec::new(),
        attributes,
        provenance: Provenance::new(
            c_family_parser_name(ctx.file.language),
            format!("{} declaration", node.kind()),
        ),
        confidence,
        freshness: Freshness::Fresh,
        arity: None,
    })
}

pub(crate) fn extract_c_family_symbol_facts(
    node: Node<'_>,
    symbol: &ParsedSymbol,
    ctx: &mut ExtractContext<'_>,
) {
    if matches!(symbol.kind, SymbolKind::Class | SymbolKind::Struct)
        && let Some(bases) = node.child_by_field_name("superclasses")
    {
        let mut cursor = bases.walk();
        for base in bases.named_children(&mut cursor) {
            // The clause interleaves `access_specifier` (public/private/
            // protected) nodes with the base types; skip them so we never
            // record a bogus `public`/`private` type reference.
            if base.kind() == "access_specifier" {
                continue;
            }
            if let Ok(text) = node_text(base, ctx.source) {
                let name = c_family_last_name(text);
                if !name.is_empty() {
                    ctx.references.push(ParsedReference {
                        file_id: ctx.file.id.clone(),
                        owner_id: Some(symbol.id.clone()),
                        text: name,
                        kind: ReferenceKind::Type,
                        span: span_from_node(base),
                        provenance: Provenance::new(
                            c_family_parser_name(ctx.file.language),
                            "base class reference",
                        ),
                    });
                }
            }
        }
    }

    // `enum class E : uint8_t {…}` carries its underlying integral type in the
    // `base` field. Record it as a Type reference (skipping the built-in
    // integer spellings) so the enum's dependency on a user-defined alias is
    // visible to reference queries.
    if symbol.kind == SymbolKind::Enum
        && let Some(base) = node.child_by_field_name("base")
        && let Ok(text) = node_text(base, ctx.source)
    {
        let name = c_family_last_name(text);
        if !name.is_empty() && !c_family_builtin_type(&name) {
            ctx.references.push(ParsedReference {
                file_id: ctx.file.id.clone(),
                owner_id: Some(symbol.id.clone()),
                text: name,
                kind: ReferenceKind::Type,
                span: span_from_node(base),
                provenance: Provenance::new(
                    c_family_parser_name(ctx.file.language),
                    "enum underlying type",
                ),
            });
        }
    }

    if matches!(
        symbol.kind,
        SymbolKind::Function
            | SymbolKind::Method
            | SymbolKind::Field
            | SymbolKind::TypeAlias
            | SymbolKind::Const
            | SymbolKind::Static
    ) {
        for type_name in c_family_type_names_from_signature(&symbol.signature) {
            ctx.references.push(ParsedReference {
                file_id: ctx.file.id.clone(),
                owner_id: Some(symbol.id.clone()),
                text: type_name.clone(),
                kind: ReferenceKind::Type,
                span: symbol.span,
                provenance: Provenance::new(
                    c_family_parser_name(ctx.file.language),
                    "signature type reference",
                ),
            });
            ctx.body_hits.push(BodyHit {
                file_id: ctx.file.id.clone(),
                owner_id: Some(symbol.id.clone()),
                text: type_name,
                kind: BodyHitKind::Type,
                span: symbol.span,
            });
        }
    }
}

pub(crate) fn extract_c_include(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let raw = node_text(node, ctx.source).unwrap_or_default();
    let Some(path) = c_include_path(raw) else {
        return;
    };
    // `<...>` includes resolve against the toolchain's system header search
    // path, never the workspace, while `"..."` includes are local first.
    // tree-sitter exposes the distinction as the `path` node kind
    // (`system_lib_string` vs `string_literal`); a text-opener fallback keeps
    // the classification working if the field is ever absent. The provenance
    // reason carries the origin (both keep the `include directive` substring the
    // resolver already filters on) so the include resolver can skip
    // workspace path-matching for system headers without overloading
    // `is_global` (which is reserved for C# global using).
    let reason = if c_include_is_system(node, raw) {
        "system include directive"
    } else {
        "include directive"
    };
    // `#include "x.h"` exposes every declaration in `x.h` to the including
    // file the same way Rust's `use module::*;` does. Marking the import as
    // a glob lets `add_import_edges` and the call resolver consult the
    // include for cross-TU lookups without inventing a name match.
    ctx.imports.push(ParsedImport {
        file_id: ctx.file.id.clone(),
        owner_id,
        path,
        alias: None,
        is_glob: true,
        is_reexport: false,
        is_static: false,
        span: span_from_node(node),
        provenance: Provenance::new(c_family_parser_name(ctx.file.language), reason),
        kind: ImportKind::Wildcard,
        imported_name: None,
        is_global: false,
    });
}

/// True for a `<...>` system include. Prefers the `path` node kind
/// (`system_lib_string`) and falls back to the raw opening delimiter.
pub(crate) fn c_include_is_system(node: Node<'_>, raw: &str) -> bool {
    if let Some(path) = node.child_by_field_name("path") {
        return path.kind() == "system_lib_string";
    }
    let trimmed = raw.trim();
    trimmed
        .find(['"', '<'])
        .map(|index| trimmed.as_bytes()[index] == b'<')
        .unwrap_or(false)
}

pub(crate) fn c_include_path(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let start = raw.find(['"', '<'])?;
    let opener = raw.as_bytes()[start] as char;
    let closer = if opener == '"' { '"' } else { '>' };
    let rest = &raw[start + opener.len_utf8()..];
    let end = rest.find(closer)?;
    let path = rest[..end].trim();
    if path.is_empty() {
        None
    } else {
        Some(path.to_string())
    }
}

/// Index `using ns::Name;` (declaration) and `using namespace ns;`
/// (directive) so cross-namespace references and calls in real C++ code can
/// resolve via the same import machinery that handles Rust `use`. Plain
/// `using` aliases like `using It = Vec::iterator;` are folded into Squeezy
/// as type-alias symbols by the symbol path, so we only emit imports for
/// the namespace-scoping forms here.
pub(crate) fn extract_c_using(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let raw = node_text(node, ctx.source).unwrap_or_default();
    let trimmed = raw.trim().trim_end_matches(';').trim();
    let Some(rest) = trimmed.strip_prefix("using") else {
        return;
    };
    let rest = rest.trim();
    let is_namespace = rest.starts_with("namespace");
    let body = if is_namespace {
        rest.trim_start_matches("namespace").trim()
    } else {
        rest
    };
    if body.is_empty() || body.contains('=') {
        return;
    }
    let path = body.replace([' ', '\t', '\n'], "");
    if path.is_empty() {
        return;
    }
    let kind = if is_namespace {
        ImportKind::Wildcard
    } else {
        ImportKind::Named
    };
    let imported_name = if is_namespace {
        None
    } else {
        Some(last_path_segment(&path))
    };
    ctx.imports.push(ParsedImport {
        file_id: ctx.file.id.clone(),
        owner_id,
        path,
        alias: None,
        is_glob: is_namespace,
        is_reexport: false,
        is_static: false,
        span: span_from_node(node),
        provenance: Provenance::new(
            c_family_parser_name(ctx.file.language),
            if is_namespace {
                "using namespace directive"
            } else {
                "using declaration"
            },
        ),
        kind,
        imported_name,
        is_global: false,
    });
}

/// Record a C++ `friend` grant on the enclosing class.
///
/// tree-sitter-cpp models `friend class Bar;` / `friend Bar;` as a
/// `friend_declaration` whose named child is a `type_identifier`,
/// `qualified_identifier`, or `template_type`; friend *functions*
/// (`friend void f();`) wrap a `declaration` / `function_definition` that the
/// regular visitor still descends into. We emit a `Type` reference for the
/// granted name tagged with a `friend grant` provenance so the access grant is
/// distinguishable from an ordinary type mention. The reference is owned by the
/// granting class so "who does Foo befriend" is answerable.
pub(crate) fn extract_c_family_friend(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if !matches!(
            child.kind(),
            "type_identifier" | "qualified_identifier" | "template_type"
        ) {
            continue;
        }
        let Ok(text) = node_text(child, ctx.source) else {
            continue;
        };
        let name = c_family_last_name(text);
        if name.is_empty() {
            continue;
        }
        ctx.references.push(ParsedReference {
            file_id: ctx.file.id.clone(),
            owner_id: owner_id.clone(),
            text: name,
            kind: ReferenceKind::Type,
            span: span_from_node(child),
            provenance: Provenance::new(c_family_parser_name(ctx.file.language), "friend grant"),
        });
    }
}

pub(crate) fn extract_c_family_call(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let Some(function_node) = node.child_by_field_name("function").or_else(|| {
        let mut cursor = node.walk();
        node.named_children(&mut cursor).next()
    }) else {
        return;
    };
    let target_text = node_text(function_node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if target_text.is_empty() {
        return;
    }
    let name = c_family_last_name(&target_text);
    if name.is_empty() {
        return;
    }
    let receiver = c_family_receiver_from_call_target(&target_text);
    let arity = node
        .child_by_field_name("arguments")
        .or_else(|| {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .find(|child| child.kind() == "argument_list")
        })
        .map(named_child_count)
        .unwrap_or_default();
    let kind = if receiver.is_some() {
        ParsedCallKind::Method
    } else {
        ParsedCallKind::Direct
    };
    let confidence = if c_family_call_is_macro_like(&name) {
        Confidence::MacroOpaque
    } else if receiver.is_some() || target_text.contains('<') {
        Confidence::CandidateSet
    } else {
        Confidence::Heuristic
    };
    ctx.calls.push(ParsedCall {
        file_id: ctx.file.id.clone(),
        caller_id: owner_id.clone(),
        name,
        target_text: target_text.clone(),
        receiver,
        arity,
        kind,
        span: span_from_node(node),
        provenance: Provenance::new(c_family_parser_name(ctx.file.language), "call_expression"),
        confidence,
    });
    extract_body_hit(node, BodyHitKind::Call, ctx, owner_id);
}

pub(crate) fn extract_c_macro_call(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let raw = node_text(node, ctx.source).unwrap_or_default();
    let name = raw
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .split('(')
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    if name.is_empty() {
        return;
    }
    ctx.calls.push(ParsedCall {
        file_id: ctx.file.id.clone(),
        caller_id: owner_id.clone(),
        name,
        target_text: raw.trim().to_string(),
        receiver: None,
        arity: 0,
        kind: ParsedCallKind::Macro,
        span: span_from_node(node),
        provenance: Provenance::new(
            c_family_parser_name(ctx.file.language),
            "preprocessor macro",
        ),
        confidence: Confidence::MacroOpaque,
    });
    extract_body_hit(node, BodyHitKind::Macro, ctx, owner_id);
}

pub(crate) fn extract_c_family_reference(
    node: Node<'_>,
    kind: ReferenceKind,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    if c_family_node_is_declaration_name(node) {
        return;
    }
    let text = node_text(node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if text.is_empty() || c_family_builtin_type(&text) {
        return;
    }
    let text = match kind {
        ReferenceKind::Path | ReferenceKind::Type | ReferenceKind::Field => {
            c_family_last_name(&text)
        }
        _ => text,
    };
    if text.is_empty() {
        return;
    }
    let body_kind = match kind {
        ReferenceKind::Identifier => BodyHitKind::Identifier,
        ReferenceKind::Type => BodyHitKind::Type,
        ReferenceKind::Path => BodyHitKind::Path,
        ReferenceKind::Field => BodyHitKind::Identifier,
        ReferenceKind::Attribute => BodyHitKind::Attribute,
    };
    ctx.references.push(ParsedReference {
        file_id: ctx.file.id.clone(),
        owner_id: owner_id.clone(),
        text: text.clone(),
        kind,
        span: span_from_node(node),
        provenance: Provenance::new(
            c_family_parser_name(ctx.file.language),
            format!("{} reference", node.kind()),
        ),
    });
    ctx.body_hits.push(BodyHit {
        file_id: ctx.file.id.clone(),
        owner_id,
        text,
        kind: body_kind,
        span: span_from_node(node),
    });
}

pub(crate) fn c_family_symbol_can_own_children(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class
            | SymbolKind::Struct
            | SymbolKind::Union
            | SymbolKind::Enum
            | SymbolKind::Module
    )
}

pub(crate) fn c_family_symbol_can_own_members(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class | SymbolKind::Struct | SymbolKind::Union
    )
}

pub(crate) fn c_family_parser_name(language: LanguageKind) -> &'static str {
    match language {
        LanguageKind::C => "tree-sitter-c",
        LanguageKind::Cpp => "tree-sitter-cpp",
        _ => "tree-sitter-c-family",
    }
}

pub(crate) fn c_declaration_is_function(node: Node<'_>) -> bool {
    node.child_by_field_name("declarator")
        .map(c_declarator_is_real_function)
        .unwrap_or(false)
}

/// True when a non-function `declaration` is a file- or namespace-scope value
/// (a global / static / const / extern variable) that deserves its own symbol.
///
/// Gated on the syntactic parent so locals inside function bodies, `for`-init
/// clauses, parameters, and class-member declarations never leak in: only the
/// translation unit, a namespace body (`declaration_list`), and an
/// `extern "C"` block (`linkage_specification`) host real globals. A bare
/// `declarator` is still required so forward type declarations (`struct Foo;`,
/// which have a type but no declarator) are left to the type-symbol path.
pub(crate) fn c_family_is_global_value_declaration(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if !matches!(
        parent.kind(),
        "translation_unit" | "declaration_list" | "linkage_specification"
    ) {
        return false;
    }
    node.child_by_field_name("declarator").is_some()
}

/// True when a declaration carries a `const`/`constexpr`/`consteval`/`constinit`
/// qualifier, so a global value should be classified as `Const` rather than
/// `Static`. tree-sitter surfaces `const`/`volatile` as `type_qualifier`
/// children and the others as keyword tokens, so a whole-token scan over the
/// pre-initializer head is the robust check.
pub(crate) fn c_family_declaration_is_const(node: Node<'_>, source: &str) -> bool {
    let Ok(text) = node_text(node, source) else {
        return false;
    };
    let head = text.split('=').next().unwrap_or(&text);
    ["const", "constexpr", "consteval", "constinit"]
        .iter()
        .any(|keyword| signature_has_keyword(head, keyword))
}

/// Returns true when the declarator describes a real function/method, not a
/// function pointer field/variable.
///
/// Tree-sitter wraps function pointers in a `function_declarator` whose own
/// `declarator` is a `parenthesized_declarator` containing a
/// `pointer_declarator` (`int (*cb)(int)`). Real functions wrap the
/// function_declarator around a plain identifier-like child
/// (`int helper(int)` → `function_declarator > identifier`). Clang's AST
/// oracle reports the first shape as `FieldDecl`, so we must keep them as
/// Squeezy `Field` symbols to avoid inflating FP against the oracle.
pub(crate) fn c_declarator_is_real_function(node: Node<'_>) -> bool {
    match node.kind() {
        "function_declarator" => node
            .child_by_field_name("declarator")
            .map(c_declarator_inner_is_function_name)
            .unwrap_or(false),
        "reference_declarator" | "init_declarator" => node
            .child_by_field_name("declarator")
            .or_else(|| first_named_child(node))
            .map(c_declarator_is_real_function)
            .unwrap_or(false),
        // `pointer_declarator`, `parenthesized_declarator`,
        // `array_declarator`, plain identifiers, anything else: not a
        // direct function declaration.
        _ => false,
    }
}

/// True when a function_declarator's inner declarator is a name-shaped node
/// (identifier, field_identifier, qualified_identifier, destructor_name,
/// operator_name). False for parenthesized/pointer declarators that signal
/// function pointers.
pub(crate) fn c_declarator_inner_is_function_name(node: Node<'_>) -> bool {
    match node.kind() {
        "identifier"
        | "field_identifier"
        | "type_identifier"
        | "qualified_identifier"
        | "namespace_identifier"
        | "destructor_name"
        | "operator_name"
        | "template_function" => true,
        // Reference declarators wrap a single inner declarator; rare but
        // legitimate for ref-qualified function returns. Recurse.
        "reference_declarator" => node
            .child_by_field_name("declarator")
            .or_else(|| first_named_child(node))
            .map(c_declarator_inner_is_function_name)
            .unwrap_or(false),
        _ => false,
    }
}

pub(crate) fn first_named_child(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).next()
}

/// Recognise the gtest / Boost.Test macros that parse as a
/// `function_definition` because their arguments are bare identifiers. Catch2's
/// `TEST_CASE("name", "[tag]")` takes string-literal arguments and does not
/// parse as a function definition, so it is intentionally excluded here.
pub(crate) fn c_family_is_test_macro_name(name: &str) -> bool {
    matches!(
        name,
        "TEST"
            | "TEST_F"
            | "TEST_P"
            | "TYPED_TEST"
            | "TYPED_TEST_P"
            | "FRIEND_TEST"
            | "BOOST_AUTO_TEST_CASE"
            | "BOOST_FIXTURE_TEST_CASE"
            | "BOOST_AUTO_TEST_CASE_TEMPLATE"
    )
}

/// Build a readable label for a test-macro definition from the macro arguments.
///
/// The arguments live in the `function_declarator`'s `parameter_list`, where
/// tree-sitter parses each bare identifier as a type-only `parameter_declaration`
/// (`TEST(Suite, Name)` → `Suite`, `Name`). We join the identifier-shaped
/// arguments with `.` (`Suite.Name`), prefixed by the macro name so two cases
/// in different suites never collide (`TEST.Suite.Name`). Returns `None` when no
/// usable argument identifier is found so a bare `TEST() {}` is left a function.
pub(crate) fn c_family_test_macro_label(
    node: Node<'_>,
    macro_name: &str,
    source: &str,
) -> Option<String> {
    let declarator = node.child_by_field_name("declarator")?;
    if declarator.kind() != "function_declarator" {
        return None;
    }
    let parameters = declarator.child_by_field_name("parameters")?;
    let mut cursor = parameters.walk();
    let mut parts = vec![macro_name.to_string()];
    for param in parameters.named_children(&mut cursor) {
        if let Ok(text) = node_text(param, source) {
            let leaf = c_family_last_name(text);
            if !leaf.is_empty() {
                parts.push(leaf);
            }
        }
    }
    if parts.len() <= 1 {
        return None;
    }
    Some(parts.join("."))
}

/// Classify a C++ special member declaration (constructor, destructor,
/// operator, or conversion operator) by inspecting its declarator shape and,
/// for constructors, comparing the member name against the enclosing class.
///
/// - `operator_cast` declarator (`operator int()`) → conversion operator.
/// - `operator_name` declarator (`operator+`) → operator overload.
/// - `destructor_name` declarator or a leading `~` → destructor.
/// - member name equal to the enclosing `class`/`struct`/`union` name and not
///   an operator/destructor → constructor.
///
/// Returns the role attribute string, mirroring Dart's `dart:constructor` /
/// `dart:operator` markers so `decl_search` can filter C++ special members.
pub(crate) fn c_family_special_member_role(
    node: Node<'_>,
    name: &str,
    source: &str,
) -> Option<&'static str> {
    // Walk the declarator chain to the innermost name node so wrapped shapes
    // (`Foo()` inside `function_declarator`, `operator int()` as `operator_cast`)
    // are classified the same way.
    let mut declarator = node.child_by_field_name("declarator").unwrap_or(node);
    loop {
        match declarator.kind() {
            "operator_cast" => return Some("c++:conversion-operator"),
            "operator_name" => return Some("c++:operator"),
            "destructor_name" => return Some("c++:destructor"),
            "function_declarator" | "pointer_declarator" | "reference_declarator"
            | "parenthesized_declarator" | "init_declarator" => {
                match declarator
                    .child_by_field_name("declarator")
                    .or_else(|| first_named_child(declarator))
                {
                    Some(inner) if inner.id() != declarator.id() => declarator = inner,
                    _ => break,
                }
            }
            _ => break,
        }
    }

    if name.starts_with('~') {
        return Some("c++:destructor");
    }
    if name.starts_with("operator") {
        return Some("c++:operator");
    }

    // Out-of-line definition (`Foo::Foo()` / `Foo::~Foo()`): the declarator is
    // a `qualified_identifier` whose trailing qualifier names the class. A
    // qualifier leaf equal to the member name marks a constructor.
    if declarator.kind() == "qualified_identifier"
        && let Ok(text) = node_text(declarator, source)
    {
        let head = text.split('(').next().unwrap_or(text);
        if let Some((qualifier, _)) = head.rsplit_once("::") {
            let class = c_family_last_name(qualifier);
            if !class.is_empty() && class == name {
                return Some("c++:constructor");
            }
        }
    }

    // In-class constructor: bare member name matching the enclosing aggregate.
    match c_family_enclosing_class_name(node, source) {
        Some(class) if class == name => Some("c++:constructor"),
        _ => None,
    }
}

/// True when `node` sits inside an `extern "C"` block / declaration. C++ models
/// this as a `linkage_specification` whose `value` field is the string literal
/// `"C"`; `extern "C++"` and other linkages are left untagged. Walking
/// ancestors covers both `extern "C" void f();` and the braced
/// `extern "C" { … }` form.
pub(crate) fn c_family_has_c_linkage(node: Node<'_>, source: &str) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == "linkage_specification"
            && let Some(value) = parent.child_by_field_name("value")
            && let Ok(text) = node_text(value, source)
        {
            return text.trim().trim_matches('"').trim() == "C";
        }
        current = parent.parent();
    }
    false
}

/// The leaf name of the nearest enclosing `class`/`struct`/`union` specifier,
/// used to recognise in-class constructor declarations. Out-of-line
/// definitions (`Foo::Foo()`) are named via the qualified declarator and never
/// reach this path with a bare match, so they are handled by the declarator
/// chain plus name comparison above when the qualifier is stripped.
pub(crate) fn c_family_enclosing_class_name(node: Node<'_>, source: &str) -> Option<String> {
    let mut current = node.parent();
    while let Some(parent) = current {
        if matches!(
            parent.kind(),
            "class_specifier" | "struct_specifier" | "union_specifier"
        ) && let Some(name) = parent
            .child_by_field_name("name")
            .and_then(|name| node_text(name, source).ok())
            .map(c_family_last_name)
            .filter(|name| !name.is_empty())
        {
            return Some(name);
        }
        current = parent.parent();
    }
    None
}

pub(crate) fn c_family_symbol_name(
    node: Node<'_>,
    kind: SymbolKind,
    source: &str,
) -> Option<String> {
    match kind {
        SymbolKind::Macro => c_macro_definition_name(node, source),
        SymbolKind::TypeAlias => c_type_alias_name(node, source),
        SymbolKind::Field | SymbolKind::Const | SymbolKind::Static => {
            c_declarator_name(node, source)
        }
        SymbolKind::Function | SymbolKind::Method => node
            .child_by_field_name("declarator")
            .and_then(|declarator| c_declarator_name(declarator, source))
            .or_else(|| c_declarator_name(node, source)),
        _ => node
            .child_by_field_name("name")
            .and_then(|child| node_text(child, source).ok())
            .map(c_family_last_name)
            .or_else(|| c_named_child_text(node, source)),
    }
}

/// Returns the qualifier prefix of a function declarator (e.g. `Foo::bar`
/// → `Some("Foo")`, `ns::free_function` → `Some("ns")`, `free` → `None`).
/// Used to distinguish out-of-line method definitions (`void Foo::bar()`)
/// from namespace-qualified free functions (`void ns::func()`) without a
/// second pass over the symbol table.
pub(crate) fn c_family_function_declarator_qualifier(
    node: Node<'_>,
    source: &str,
) -> Option<String> {
    let declarator = node.child_by_field_name("declarator")?;
    let text = node_text(declarator, source).ok()?;
    let head = text.split('(').next().unwrap_or(text).trim();
    let (qualifier, _) = head.rsplit_once("::")?;
    let qualifier = qualifier
        .trim()
        .trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != ':');
    if qualifier.is_empty() {
        None
    } else {
        Some(qualifier.to_string())
    }
}

/// Apply `looks_like_type_name` to the last segment of a `::`-qualifier.
/// Class names follow the type-name convention (uppercase initial or `_t`
/// suffix), namespace identifiers do not. This is a cheap heuristic; a
/// post-pass that walks symbols can later upgrade ambiguous cases.
pub(crate) fn qualifier_is_type_like(qualifier: &str) -> bool {
    let leaf = qualifier.rsplit("::").next().unwrap_or(qualifier).trim();
    !leaf.is_empty() && looks_like_type_name(leaf)
}

pub(crate) fn c_named_child_text(node: Node<'_>, source: &str) -> Option<String> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| {
            matches!(
                child.kind(),
                "identifier"
                    | "type_identifier"
                    | "field_identifier"
                    | "qualified_identifier"
                    | "namespace_identifier"
                    | "destructor_name"
                    | "operator_name"
            )
        })
        .and_then(|child| node_text(child, source).ok())
        .map(c_family_last_name)
}

pub(crate) fn c_declarator_name(node: Node<'_>, source: &str) -> Option<String> {
    if matches!(
        node.kind(),
        "identifier"
            | "field_identifier"
            | "type_identifier"
            | "qualified_identifier"
            | "namespace_identifier"
            | "destructor_name"
            | "operator_name"
    ) {
        return node_text(node, source).ok().map(c_family_last_name);
    }
    if let Some(name) = node
        .child_by_field_name("name")
        .and_then(|child| node_text(child, source).ok())
        .map(c_family_last_name)
        .filter(|name| !name.is_empty())
    {
        return Some(name);
    }
    if let Some(name) = node
        .child_by_field_name("declarator")
        .and_then(|child| c_declarator_name(child, source))
    {
        return Some(name);
    }
    let mut cursor = node.walk();
    let children = node.named_children(&mut cursor).collect::<Vec<_>>();
    for child in children.into_iter().rev() {
        if matches!(
            child.kind(),
            "parameter_list"
                | "field_declaration_list"
                | "argument_list"
                | "template_argument_list"
                | "template_parameter_list"
        ) {
            continue;
        }
        if let Some(name) = c_declarator_name(child, source).filter(|name| !name.is_empty()) {
            return Some(name);
        }
    }
    None
}

pub(crate) fn c_type_alias_name(node: Node<'_>, source: &str) -> Option<String> {
    node.child_by_field_name("declarator")
        .and_then(|child| c_declarator_name(child, source))
        .or_else(|| {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .filter(|child| matches!(child.kind(), "type_identifier" | "identifier"))
                .filter_map(|child| node_text(child, source).ok())
                .map(c_family_last_name)
                .last()
        })
}

pub(crate) fn c_macro_definition_name(node: Node<'_>, source: &str) -> Option<String> {
    node.child_by_field_name("name")
        .and_then(|child| node_text(child, source).ok())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .or_else(|| {
            let raw = node_text(node, source).ok()?;
            raw.split_whitespace()
                .nth(1)
                .and_then(|name| name.split('(').next())
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
        })
}

pub(crate) fn c_family_body_node(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("body").or_else(|| {
        let mut cursor = node.walk();
        node.named_children(&mut cursor).find(|child| {
            matches!(
                child.kind(),
                "compound_statement"
                    | "field_declaration_list"
                    | "enumerator_list"
                    | "declaration_list"
            )
        })
    })
}

pub(crate) fn c_family_attributes_for_node(
    node: Node<'_>,
    kind: SymbolKind,
    signature: &str,
) -> Vec<String> {
    let mut attributes = Vec::new();
    match node.kind() {
        "function_definition" => attributes.push("c-family:definition".to_string()),
        "declaration" if matches!(kind, SymbolKind::Function | SymbolKind::Method) => {
            attributes.push("c-family:declaration".to_string())
        }
        "field_declaration" if matches!(kind, SymbolKind::Function | SymbolKind::Method) => {
            attributes.push("c-family:declaration".to_string())
        }
        "field_declaration" => attributes.push("c-family:field".to_string()),
        "enumerator" => attributes.push("c-family:enum-variant".to_string()),
        "preproc_def" | "preproc_function_def" => {
            attributes.push("c-family:macro".to_string());
            attributes.push("preprocessor:opaque".to_string());
        }
        "template_declaration" => attributes.push("c++:template".to_string()),
        "enum_specifier" if c_family_enum_is_scoped(node) => {
            attributes.push("c++:scoped-enum".to_string())
        }
        _ => {}
    }

    let ancestors = c_family_ancestor_kinds(node);
    if ancestors.template {
        attributes.push("c++:template".to_string());
    }
    if ancestors.conditional {
        attributes.push("preprocessor:conditional".to_string());
    }

    // Function/method modifiers. The signature slice is already
    // start..body_start so the keyword scans never reach into the class body.
    if matches!(
        kind,
        SymbolKind::Function | SymbolKind::Method | SymbolKind::Field
    ) {
        c_family_collect_function_modifiers(signature, &mut attributes);
    }

    if matches!(
        kind,
        SymbolKind::Class | SymbolKind::Struct | SymbolKind::Union
    ) && c_family_is_template_specialization(node)
    {
        attributes.push("c++:template-specialization".to_string());
    }

    attributes
}

#[derive(Default)]
struct CFamilyAncestorFlags {
    template: bool,
    conditional: bool,
}

/// Single ancestor walk that records every kind we care about so attribute
/// extraction doesn't repeat the same parent walk for each flag.
fn c_family_ancestor_kinds(node: Node<'_>) -> CFamilyAncestorFlags {
    let mut flags = CFamilyAncestorFlags::default();
    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "template_declaration" => flags.template = true,
            "preproc_if" | "preproc_ifdef" | "preproc_ifndef" => flags.conditional = true,
            _ => {}
        }
        current = parent.parent();
    }
    flags
}

/// Stamp the C++ function/method modifier attributes that survive in the
/// signature slice (`start..body_start`). Leading specifiers (`virtual`,
/// `static`, `explicit`, `constexpr`) and the `= delete` / `= default` clauses
/// are matched anywhere in the slice; trailing qualifiers (`const`, `override`,
/// `final`, `noexcept`) are matched only after the last `)` so a `const`
/// return type does not masquerade as a const member function.
pub(crate) fn c_family_collect_function_modifiers(signature: &str, attributes: &mut Vec<String>) {
    for (keyword, attribute) in [
        ("virtual", "c++:virtual"),
        ("static", "c++:static"),
        ("explicit", "c++:explicit"),
        ("constexpr", "c++:constexpr"),
    ] {
        if signature_has_keyword(signature, keyword) {
            attributes.push(attribute.to_string());
        }
    }

    // `= delete` / `= default`. The signature slice keeps the `= delete;`
    // tail because such declarations have no compound-statement body.
    if signature_has_keyword(signature, "delete") {
        attributes.push("c++:deleted".to_string());
    }
    if signature_has_keyword(signature, "default") {
        attributes.push("c++:defaulted".to_string());
    }

    // Trailing qualifiers live after the parameter list. Scan only the slice
    // following the last close-paren to avoid return-type `const`/`noexcept`.
    // A trailing return type (`-> const T`) starts at `->`, so truncate there
    // before scanning so its `const` is not mistaken for a const member.
    let mut trailing = signature
        .rfind(')')
        .map(|index| &signature[index + 1..])
        .unwrap_or("");
    if let Some(arrow) = trailing.find("->") {
        trailing = &trailing[..arrow];
    }
    for (keyword, attribute) in [
        ("const", "c++:const"),
        ("override", "c++:override"),
        ("final", "c++:final"),
        ("noexcept", "c++:noexcept"),
    ] {
        if signature_has_keyword(trailing, keyword) {
            attributes.push(attribute.to_string());
        }
    }
}

/// True when the given identifier appears as a whole-token keyword in the
/// signature slice. Avoids substring matches like `nonvirtual` or strings
/// embedded in default parameter values.
pub(crate) fn signature_has_keyword(signature: &str, keyword: &str) -> bool {
    for token in signature.split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_') {
        if token == keyword {
            return true;
        }
    }
    false
}

/// Detect `template<> class Foo<int> { … }` and `template<typename T> class
/// Foo<int, T> {}` shaped specializations. Tree-sitter-cpp represents these
/// as `class_specifier` whose `name` field is a `template_type` rather than a
/// `type_identifier`, with the explicit template arg list nested inside.
pub(crate) fn c_family_is_template_specialization(node: Node<'_>) -> bool {
    let Some(name) = node.child_by_field_name("name") else {
        return false;
    };
    name.kind() == "template_type"
}

/// Emit `c-family:bitfield` (and `c-family:bitfield-width:<n>` when the width is
/// an integer literal) for a `field_declaration` carrying a `bitfield_clause`
/// (`unsigned flags : 4;`). tree-sitter models the width as the clause's
/// `expression` child; non-literal widths (`: kBits`) still get the marker so
/// `decl_search attribute=c-family:bitfield` finds them.
pub(crate) fn c_family_bitfield_attributes(node: Node<'_>, source: &str) -> Vec<String> {
    let mut cursor = node.walk();
    let Some(clause) = node
        .children(&mut cursor)
        .find(|child| child.kind() == "bitfield_clause")
    else {
        return Vec::new();
    };
    let mut attributes = vec!["c-family:bitfield".to_string()];
    if let Some(width) = first_named_child(clause)
        .and_then(|expr| node_text(expr, source).ok())
        .map(str::trim)
        .filter(|width| !width.is_empty() && width.bytes().all(|byte| byte.is_ascii_digit()))
    {
        attributes.push(format!("c-family:bitfield-width:{width}"));
    }
    attributes
}

/// Detect a C++ scoped enum (`enum class E` / `enum struct E`). tree-sitter-cpp
/// emits the `class`/`struct` token as an anonymous child between the `enum`
/// keyword and the name field, so we scan the immediate children for it rather
/// than reading a field that does not exist.
pub(crate) fn c_family_enum_is_scoped(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| matches!(child.kind(), "class" | "struct"))
}

/// Emit `base:<Leaf>` inheritance attributes for each base class of a C++
/// `class_specifier` / `struct_specifier`.
///
/// tree-sitter-cpp models the base list as a single `base_class_clause` in the
/// `superclasses` field, whose named children interleave optional
/// `access_specifier` nodes with the base type nodes (`type_identifier`,
/// `qualified_identifier`, `template_type`, …); the `virtual` keyword is an
/// anonymous token. C++ has no syntactic interface/abstract distinction, so per
/// the generic lowering (which treats `base:` as Extends) every base — public,
/// protected, private, or virtual — is recorded as `base:`. The leaf type name
/// is taken (template args and `::` qualifiers stripped) to match how the
/// resolver looks symbols up. Mirrors the inheritance signal every other
/// extractor emits so `decl_search attribute=base:<Type>` and
/// `inheritance_hierarchy` work for C/C++.
fn c_family_base_class_attributes(node: Node<'_>, kind: SymbolKind, source: &str) -> Vec<String> {
    if !matches!(kind, SymbolKind::Class | SymbolKind::Struct) {
        return Vec::new();
    }
    let Some(bases) = node.child_by_field_name("superclasses") else {
        return Vec::new();
    };
    let mut attributes = Vec::new();
    let mut cursor = bases.walk();
    for base in bases.named_children(&mut cursor) {
        // The `access_specifier` (public/private/protected) is itself a named
        // child of the clause; skip it so only real base types are recorded.
        if base.kind() == "access_specifier" {
            continue;
        }
        let Ok(text) = node_text(base, source) else {
            continue;
        };
        let name = c_family_last_name(text);
        if !name.is_empty() {
            attributes.push(format!("base:{name}"));
        }
    }
    attributes
}

pub(crate) fn c_family_symbol_confidence(node: Node<'_>, attributes: &[String]) -> Confidence {
    if attributes
        .iter()
        .any(|attribute| attribute == "preprocessor:opaque")
    {
        return Confidence::MacroOpaque;
    }
    if attributes
        .iter()
        .any(|attribute| attribute == "preprocessor:conditional")
    {
        return Confidence::ConditionalUnknown;
    }
    if attributes
        .iter()
        .any(|attribute| attribute == "c++:template-specialization" || attribute == "c++:template")
    {
        return Confidence::Partial;
    }
    if node.kind() == "declaration" {
        return Confidence::Heuristic;
    }
    Confidence::ExactSyntax
}

/// Resolve the C++ access modifier for a class/struct member.
///
/// tree-sitter-cpp models `public:` / `private:` / `protected:` as keyword
/// children of an `access_specifier` named node. Walking prev_named_siblings
/// finds the closest preceding `access_specifier`; if none exists, the
/// containing aggregate's default applies (`struct` → public, `class` →
/// private, `union` → public). For non-member symbols we still fall back to
/// the leading-keyword scan so `static int g;` reports `static`.
pub(crate) fn c_family_visibility_text(node: Node<'_>, source: &str) -> Option<String> {
    let mut sibling = node.prev_named_sibling();
    while let Some(current) = sibling {
        if current.kind() == "access_specifier"
            && let Some(keyword) = c_family_access_specifier_keyword(current, source)
        {
            return Some(keyword);
        }
        sibling = current.prev_named_sibling();
    }

    if let Some(parent) = node.parent()
        && parent.kind() == "field_declaration_list"
        && let Some(default) = c_family_aggregate_default_access(parent)
    {
        return Some(default.to_string());
    }

    let raw = node_text(node, source).ok()?.trim_start();
    [
        "static",
        "extern",
        "inline",
        "public",
        "private",
        "protected",
    ]
    .into_iter()
    .find(|keyword| {
        raw.starts_with(*keyword)
            && raw
                .as_bytes()
                .get(keyword.len())
                .is_none_or(|byte| !((*byte as char).is_ascii_alphanumeric() || *byte == b'_'))
    })
    .map(str::to_string)
}

pub(crate) fn c_family_access_specifier_keyword(node: Node<'_>, source: &str) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if matches!(child.kind(), "public" | "private" | "protected") {
            return Some(child.kind().to_string());
        }
    }
    // Fallback: scan the text. `access_specifier` may report the keyword as
    // an anonymous child on some grammar versions.
    let raw = node_text(node, source).ok()?.trim();
    ["public", "private", "protected"]
        .into_iter()
        .find(|keyword| raw.starts_with(*keyword))
        .map(str::to_string)
}

pub(crate) fn c_family_aggregate_default_access(field_list: Node<'_>) -> Option<&'static str> {
    let parent = field_list.parent()?;
    match parent.kind() {
        "class_specifier" => Some("private"),
        "struct_specifier" | "union_specifier" => Some("public"),
        _ => None,
    }
}

pub(crate) fn c_family_reference_kind(node: Node<'_>) -> Option<ReferenceKind> {
    match node.kind() {
        "identifier" => Some(ReferenceKind::Identifier),
        "type_identifier" | "primitive_type" | "sized_type_specifier" => Some(ReferenceKind::Type),
        "qualified_identifier" | "scoped_identifier" | "namespace_identifier" => {
            Some(ReferenceKind::Path)
        }
        "field_identifier" => Some(ReferenceKind::Field),
        "attribute_specifier" | "attribute_declaration" => Some(ReferenceKind::Attribute),
        _ => None,
    }
}

pub(crate) fn c_family_node_is_declaration_name(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if parent
        .child_by_field_name("name")
        .map(|name| name.id() == node.id())
        .unwrap_or(false)
    {
        return true;
    }
    if matches!(
        parent.kind(),
        "function_declarator" | "pointer_declarator" | "reference_declarator" | "init_declarator"
    ) && parent
        .child_by_field_name("declarator")
        .map(|declarator| declarator.id() == node.id())
        .unwrap_or(false)
    {
        return true;
    }
    matches!(
        parent.kind(),
        "struct_specifier"
            | "class_specifier"
            | "union_specifier"
            | "enum_specifier"
            | "enumerator"
            | "type_definition"
            | "alias_declaration"
            | "namespace_definition"
            | "preproc_def"
            | "preproc_function_def"
    )
}

pub(crate) fn c_family_type_names_from_signature(signature: &str) -> Vec<String> {
    let mut names = Vec::new();
    for token in signature
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == ':' || ch == '~'))
    {
        let name = c_family_last_name(token);
        if name.is_empty() || c_family_builtin_type(&name) || !looks_like_type_name(&name) {
            continue;
        }
        names.push(name);
    }
    names.sort();
    names.dedup();
    names
}

pub(crate) fn c_family_last_name(path: &str) -> String {
    path.trim()
        .trim_matches(|ch: char| {
            matches!(
                ch,
                '&' | '*' | '(' | ')' | '[' | ']' | '{' | '}' | ';' | ',' | ':' | '<' | '>'
            )
        })
        .rsplit("::")
        .next()
        .unwrap_or(path)
        .rsplit("->")
        .next()
        .unwrap_or(path)
        .rsplit('.')
        .next()
        .unwrap_or(path)
        .trim()
        .trim_start_matches('~')
        .to_string()
}

pub(crate) fn c_family_receiver_from_call_target(target_text: &str) -> Option<String> {
    target_text
        .rsplit_once("::")
        .or_else(|| target_text.rsplit_once("->"))
        .or_else(|| target_text.rsplit_once('.'))
        .map(|(receiver, _)| receiver.trim().to_string())
        .filter(|receiver| !receiver.is_empty())
}

/// True when the call target reads like an all-caps preprocessor macro.
///
/// We're lenient: anything that contains zero lowercase ASCII letters and is
/// at least two characters long (so single-letter identifiers like `N`
/// don't fire) is treated as macro-like. This catches both `EXPECT_EQ` and
/// underscore-free names like `ASSERT`, `LOG`, and `CHECK`. The body
/// extractor still records the literal call site, so over-flagging only
/// widens the macro-opaque cone — it never invents calls.
pub(crate) fn c_family_call_is_macro_like(name: &str) -> bool {
    if name.len() < 2 {
        return false;
    }
    let mut has_alpha = false;
    for ch in name.chars() {
        if ch.is_ascii_lowercase() {
            return false;
        }
        if ch.is_ascii_alphabetic() {
            has_alpha = true;
        }
    }
    has_alpha
}

pub(crate) fn c_family_builtin_type(text: &str) -> bool {
    matches!(
        text,
        "auto"
            | "bool"
            | "char"
            | "const"
            | "double"
            | "extern"
            | "float"
            | "inline"
            | "int"
            | "long"
            | "mutable"
            | "register"
            | "restrict"
            | "short"
            | "signed"
            | "size_t"
            | "static"
            | "struct"
            | "template"
            | "typename"
            | "union"
            | "unsigned"
            | "void"
            | "volatile"
    )
}

pub(crate) fn looks_like_type_name(name: &str) -> bool {
    name.chars()
        .next()
        .map(|ch| ch.is_ascii_uppercase())
        .unwrap_or(false)
        || name.ends_with("_t")
}

pub(crate) fn is_c_family_literal(kind: &str) -> bool {
    matches!(
        kind,
        "string_literal"
            | "raw_string_literal"
            | "number_literal"
            | "char_literal"
            | "true"
            | "false"
            | "null"
            | "nullptr"
    )
}
