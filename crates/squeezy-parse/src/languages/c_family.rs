use std::collections::{HashMap, HashSet};

use crate::*;

pub(crate) fn extract_c_family(file: FileRecord, source: &str, tree: &Tree) -> ParsedFile {
    let mut ctx = ExtractContext {
        file: file.clone(),
        source,
        symbols: Vec::new(),
        imports: Vec::new(),
        calls: Vec::new(),
        references: Vec::new(),
        body_hits: Vec::new(),
        diagnostics: Vec::new(),
        go_type_index: HashMap::new(),
    };
    let root = tree.root_node();
    if root.has_error() {
        ctx.diagnostics.push(ParseDiagnostic {
            message: "tree-sitter reported parse errors".to_string(),
            span: Some(span_from_node(root)),
            confidence: Confidence::Partial,
        });
    }

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

fn dedup_c_family_facts(ctx: &mut ExtractContext<'_>) {
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
fn collapse_c_family_function_decls(ctx: &mut ExtractContext<'_>) {
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

fn pick_canonical_function_symbol(indexes: &[usize], symbols: &[ParsedSymbol]) -> usize {
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

fn visit_c_family_node(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
    owner_symbol: Option<&SymbolId>,
) {
    if node.is_missing() {
        ctx.diagnostics.push(ParseDiagnostic {
            message: format!("missing {}", node.kind()),
            span: Some(span_from_node(node)),
            confidence: Confidence::Partial,
        });
        return;
    }

    let kind = node.kind();
    if kind == "preproc_include" {
        extract_c_include(node, ctx, owner_symbol.cloned());
    } else if matches!(kind, "using_declaration" | "using_directive") {
        extract_c_using(node, ctx, owner_symbol.cloned());
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

fn visit_c_family_children(
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

fn c_family_symbol_from_node(
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

    let name = c_family_symbol_name(node, kind, ctx.source)?;
    if name.is_empty() {
        return None;
    }
    let body = c_family_body_node(node);
    let span = span_from_node(node);
    let body_span = body.map(span_from_node);
    let signature = signature_text(node, body, ctx.source);
    let parent_id = parent_symbol.map(|(id, _)| id.clone());
    let mut attributes = c_family_attributes_for_node(node, kind, &signature);
    attributes.sort();
    attributes.dedup();
    let confidence = c_family_symbol_confidence(node, &attributes);
    Some(ParsedSymbol {
        id: symbol_id(&ctx.file, parent_id.as_ref(), kind, &name, span),
        file_id: ctx.file.id.clone(),
        parent_id,
        name,
        kind,
        span,
        body_span,
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
    })
}

fn extract_c_family_symbol_facts(
    node: Node<'_>,
    symbol: &ParsedSymbol,
    ctx: &mut ExtractContext<'_>,
) {
    if matches!(symbol.kind, SymbolKind::Class | SymbolKind::Struct)
        && let Some(bases) = node.child_by_field_name("superclasses")
    {
        let mut cursor = bases.walk();
        for base in bases.named_children(&mut cursor) {
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

    if matches!(
        symbol.kind,
        SymbolKind::Function | SymbolKind::Method | SymbolKind::Field | SymbolKind::TypeAlias
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

fn extract_c_include(node: Node<'_>, ctx: &mut ExtractContext<'_>, owner_id: Option<SymbolId>) {
    let raw = node_text(node, ctx.source).unwrap_or_default();
    let Some(path) = c_include_path(raw) else {
        return;
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
        provenance: Provenance::new(c_family_parser_name(ctx.file.language), "include directive"),
    });
}

fn c_include_path(raw: &str) -> Option<String> {
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
fn extract_c_using(node: Node<'_>, ctx: &mut ExtractContext<'_>, owner_id: Option<SymbolId>) {
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
    });
}

fn extract_c_family_call(node: Node<'_>, ctx: &mut ExtractContext<'_>, owner_id: Option<SymbolId>) {
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

fn extract_c_macro_call(node: Node<'_>, ctx: &mut ExtractContext<'_>, owner_id: Option<SymbolId>) {
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

fn extract_c_family_reference(
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

fn c_family_symbol_can_own_children(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class
            | SymbolKind::Struct
            | SymbolKind::Union
            | SymbolKind::Enum
            | SymbolKind::Module
    )
}

fn c_family_symbol_can_own_members(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class | SymbolKind::Struct | SymbolKind::Union
    )
}

fn c_family_parser_name(language: LanguageKind) -> &'static str {
    match language {
        LanguageKind::C => "tree-sitter-c",
        LanguageKind::Cpp => "tree-sitter-cpp",
        _ => "tree-sitter-c-family",
    }
}

fn c_declaration_is_function(node: Node<'_>) -> bool {
    node.child_by_field_name("declarator")
        .map(c_declarator_is_real_function)
        .unwrap_or(false)
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
fn c_declarator_is_real_function(node: Node<'_>) -> bool {
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
fn c_declarator_inner_is_function_name(node: Node<'_>) -> bool {
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

fn first_named_child(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).next()
}

fn c_family_symbol_name(node: Node<'_>, kind: SymbolKind, source: &str) -> Option<String> {
    match kind {
        SymbolKind::Macro => c_macro_definition_name(node, source),
        SymbolKind::TypeAlias => c_type_alias_name(node, source),
        SymbolKind::Field => c_declarator_name(node, source),
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
fn c_family_function_declarator_qualifier(node: Node<'_>, source: &str) -> Option<String> {
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
fn qualifier_is_type_like(qualifier: &str) -> bool {
    let leaf = qualifier.rsplit("::").next().unwrap_or(qualifier).trim();
    !leaf.is_empty() && looks_like_type_name(leaf)
}

fn c_named_child_text(node: Node<'_>, source: &str) -> Option<String> {
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

fn c_declarator_name(node: Node<'_>, source: &str) -> Option<String> {
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

fn c_type_alias_name(node: Node<'_>, source: &str) -> Option<String> {
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

fn c_macro_definition_name(node: Node<'_>, source: &str) -> Option<String> {
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

fn c_family_body_node(node: Node<'_>) -> Option<Node<'_>> {
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

fn c_family_attributes_for_node(node: Node<'_>, kind: SymbolKind, signature: &str) -> Vec<String> {
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
        _ => {}
    }

    let ancestors = c_family_ancestor_kinds(node);
    if ancestors.template {
        attributes.push("c++:template".to_string());
    }
    if ancestors.conditional {
        attributes.push("preprocessor:conditional".to_string());
    }

    // `virtual` is only meaningful for function/method symbols, and the
    // signature slice is already start..body_start so the search avoids
    // scanning the full class body.
    if matches!(
        kind,
        SymbolKind::Function | SymbolKind::Method | SymbolKind::Field
    ) && signature_has_keyword(signature, "virtual")
    {
        attributes.push("c++:virtual".to_string());
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

/// True when the given identifier appears as a whole-token keyword in the
/// signature slice. Avoids substring matches like `nonvirtual` or strings
/// embedded in default parameter values.
fn signature_has_keyword(signature: &str, keyword: &str) -> bool {
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
fn c_family_is_template_specialization(node: Node<'_>) -> bool {
    let Some(name) = node.child_by_field_name("name") else {
        return false;
    };
    name.kind() == "template_type"
}

fn c_family_symbol_confidence(node: Node<'_>, attributes: &[String]) -> Confidence {
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
fn c_family_visibility_text(node: Node<'_>, source: &str) -> Option<String> {
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

fn c_family_access_specifier_keyword(node: Node<'_>, source: &str) -> Option<String> {
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

fn c_family_aggregate_default_access(field_list: Node<'_>) -> Option<&'static str> {
    let parent = field_list.parent()?;
    match parent.kind() {
        "class_specifier" => Some("private"),
        "struct_specifier" | "union_specifier" => Some("public"),
        _ => None,
    }
}

fn c_family_reference_kind(node: Node<'_>) -> Option<ReferenceKind> {
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

fn c_family_node_is_declaration_name(node: Node<'_>) -> bool {
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

fn c_family_type_names_from_signature(signature: &str) -> Vec<String> {
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

fn c_family_last_name(path: &str) -> String {
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

fn c_family_receiver_from_call_target(target_text: &str) -> Option<String> {
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
fn c_family_call_is_macro_like(name: &str) -> bool {
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

fn c_family_builtin_type(text: &str) -> bool {
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

fn looks_like_type_name(name: &str) -> bool {
    name.chars()
        .next()
        .map(|ch| ch.is_ascii_uppercase())
        .unwrap_or(false)
        || name.ends_with("_t")
}

fn is_c_family_literal(kind: &str) -> bool {
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
