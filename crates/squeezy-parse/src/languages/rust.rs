use crate::languages::java::java_first_name_descendant;
use crate::*;

pub(crate) fn extract_rust(file: FileRecord, source: &str, tree: &Tree) -> ParsedFile {
    let mut ctx = ExtractContext::new(file.clone(), source);
    let root = tree.root_node();
    record_parse_error_diagnostics(root, &mut ctx);

    visit_node(root, &mut ctx, None, None);

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

pub(crate) fn visit_node(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<SymbolId>,
    owner_symbol: Option<SymbolId>,
) {
    if node.is_missing() {
        record_missing_node_diagnostic(node, ctx);
        return;
    }

    let kind = node.kind();
    if kind == "use_declaration" {
        extract_import(node, ctx, owner_symbol.clone());
    } else if kind == "extern_crate_declaration" {
        extract_extern_crate(node, ctx, owner_symbol.clone());
    }

    if let Some(symbol) = symbol_from_node(node, ctx, parent_symbol.clone()) {
        let next_parent = Some(symbol.id.clone());
        let next_owner = if symbol.body_span.is_some() {
            Some(symbol.id.clone())
        } else {
            owner_symbol.clone()
        };
        ctx.symbols.push(symbol);
        visit_children(node, ctx, next_parent, next_owner);
        return;
    }

    if kind == "call_expression" {
        extract_direct_call(node, ctx, owner_symbol.clone());
    } else if kind == "method_call_expression" {
        extract_method_call(node, ctx, owner_symbol.clone());
    } else if kind == "macro_invocation" {
        extract_macro_call(node, ctx, owner_symbol.clone());
    } else if let Some(reference_kind) = reference_kind(kind) {
        extract_reference(node, reference_kind, ctx, owner_symbol.clone());
    } else if is_literal(kind) {
        extract_body_hit(node, BodyHitKind::Literal, ctx, owner_symbol.clone());
    }

    visit_children(node, ctx, parent_symbol, owner_symbol);
}

pub(crate) fn visit_children(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<SymbolId>,
    owner_symbol: Option<SymbolId>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit_node(child, ctx, parent_symbol.clone(), owner_symbol.clone());
    }
}

pub(crate) fn symbol_from_node(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<SymbolId>,
) -> Option<ParsedSymbol> {
    let mut kind = match node.kind() {
        "mod_item" => SymbolKind::Module,
        "struct_item" => SymbolKind::Struct,
        "enum_item" => SymbolKind::Enum,
        "union_item" => SymbolKind::Union,
        "trait_item" => SymbolKind::Trait,
        "impl_item" => SymbolKind::Impl,
        "function_item" | "function_signature_item" => SymbolKind::Function,
        "const_item" => SymbolKind::Const,
        "static_item" => SymbolKind::Static,
        "type_item" | "associated_type" => SymbolKind::TypeAlias,
        "macro_definition" => SymbolKind::Macro,
        "field_declaration" => SymbolKind::Field,
        "enum_variant" => SymbolKind::Variant,
        _ => return None,
    };

    if kind == SymbolKind::Function
        && parent_symbol_is_impl_or_trait(&parent_symbol)
        && function_has_self_parameter(node, ctx.source)
    {
        kind = SymbolKind::Method;
    }

    let mut attributes = attributes_for_node(node, ctx.source);
    if kind == SymbolKind::Function && is_test_function(&attributes) {
        kind = SymbolKind::Test;
    }
    // Doc comments must be read from the literal `#[doc = ...]`/`///` attributes
    // before we append synthesized `derive:`/`cfg:` tokens, otherwise the
    // `attribute_path == "doc"` filter would have to skip the new tokens too.
    let docs = docs_from_attributes(&attributes);
    // Record the declared type of a struct field as a queryable `type:` attribute,
    // mirroring the Java/Kotlin field extractors so `decl_search attribute=type:X`
    // and field hierarchy listings expose it.
    if kind == SymbolKind::Field
        && let Some(field_type) = rust_field_type(node, ctx.source)
    {
        attributes.push(format!("type:{field_type}"));
    }
    // Normalize `#[derive(..)]`/`#[cfg(..)]` and other attribute items into
    // queryable tokens (`derive:Serialize`, `cfg:<predicate>`, `rust:attr:<path>`)
    // so `decl_search attribute=derive:X` and cfg filtering work, mirroring the
    // C#/Java/JS-TS normalized-attribute passes.
    attributes.extend(rust_semantic_attributes(&attributes));
    // Supertrait bounds (`trait Sub: Super`) live in the `bounds` field, not as
    // literal `#[..]` attributes; expose each as a `base:` token so Rust trait
    // hierarchies are queryable the same way Python/JS-TS class bases are.
    if kind == SymbolKind::Trait {
        attributes.extend(
            rust_supertrait_bases(node, ctx.source)
                .into_iter()
                .map(|base| format!("base:{base}")),
        );
    }
    // Record the impl's self-type and (optional) trait as queryable tokens so an
    // inheritance-edge builder can emit Implements/Extends/InherentImpl/TraitImpl
    // without re-parsing the impl header string on every query.
    if kind == SymbolKind::Impl {
        if let Some(self_type) = rust_impl_self_type(node, ctx.source) {
            attributes.push(format!("impl-for:{self_type}"));
        }
        if let Some(trait_name) = rust_impl_trait(node, ctx.source) {
            attributes.push(format!("impl-trait:{trait_name}"));
        }
    }
    attributes.sort();
    attributes.dedup();

    let name = symbol_name(node, kind, ctx.source)?;
    if kind == SymbolKind::Const && name == "_" {
        return None;
    }
    // Store the bare self-type on the Impl symbol's `language_identity` (as a
    // `T:<type>` token, matching the C# convention) so cross-file self-type
    // attribution and the partials index can read a stored identity instead of
    // re-running the impl-header string match on every query.
    let language_identity = if kind == SymbolKind::Impl {
        rust_impl_self_type(node, ctx.source).map(|self_type| format!("T:{self_type}"))
    } else {
        None
    };
    let body = node.child_by_field_name("body");
    let span = span_from_node(node);
    let body_span = body.map(span_from_node);
    let signature_span = signature_span_from_nodes(node, body);
    let signature = signature_text(node, body, ctx.source);
    let visibility = visibility_text(node, ctx.source);
    let id = symbol_id(&ctx.file, parent_symbol.as_ref(), kind, &name, span);
    let arity = if matches!(
        kind,
        SymbolKind::Function | SymbolKind::Method | SymbolKind::Test
    ) {
        node.child_by_field_name("parameters")
            .map(|params| u8::try_from(named_child_count(params)).unwrap_or(u8::MAX))
    } else {
        None
    };
    // cfg-gated items can be compiled out; downgrade their confidence to
    // ConditionalUnknown (mirroring C/C++ `preprocessor:conditional`) since a
    // single-config parse cannot know whether the symbol is actually present.
    let confidence = if attributes.iter().any(|attr| attr == "rust:conditional") {
        Confidence::ConditionalUnknown
    } else {
        Confidence::ExactSyntax
    };

    Some(ParsedSymbol {
        id,
        file_id: ctx.file.id.clone(),
        parent_id: parent_symbol,
        name,
        kind,
        language_identity,
        span,
        body_span,
        signature_span,
        signature,
        visibility,
        docs,
        attributes,
        provenance: Provenance::new("tree-sitter-rust", format!("{} declaration", node.kind())),
        confidence,
        freshness: Freshness::Fresh,
        arity,
    })
}

pub(crate) fn python_symbol_from_node(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> Option<ParsedSymbol> {
    let mut kind = match node.kind() {
        "class_definition" => SymbolKind::Class,
        "function_definition" => SymbolKind::Function,
        _ => return None,
    };
    if kind == SymbolKind::Function
        && node.kind() != "variable_declarator"
        && parent_symbol
            .map(|(_, parent_kind)| *parent_kind == SymbolKind::Class)
            .unwrap_or(false)
    {
        kind = SymbolKind::Method;
    }

    let name = node
        .child_by_field_name("name")
        .and_then(|child| node_text(child, ctx.source).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())?;
    let body = node.child_by_field_name("body");
    let span = span_from_node(node);
    let body_span = body.map(span_from_node);
    let signature_span = signature_span_from_nodes(node, body);
    let signature = signature_text(node, body, ctx.source);
    let parent_id = parent_symbol.map(|(id, _)| id.clone());
    let id = symbol_id(&ctx.file, parent_id.as_ref(), kind, &name, span);
    let mut attributes = python_attributes_for_node(node, ctx.source);
    if kind == SymbolKind::Class {
        attributes.extend(
            python_class_bases(&signature)
                .into_iter()
                .map(|base| format!("base:{base}")),
        );
    }
    let docs = python_docs_for_node(node, ctx.source);
    attributes.sort();
    attributes.dedup();
    attributes.extend(python_test_attributes(&ctx.file.relative_path, kind, &name));
    attributes.sort();
    attributes.dedup();
    let arity = if matches!(
        kind,
        SymbolKind::Function | SymbolKind::Method | SymbolKind::Test
    ) {
        node.child_by_field_name("parameters")
            .map(|params| u8::try_from(named_child_count(params)).unwrap_or(u8::MAX))
    } else {
        None
    };

    Some(ParsedSymbol {
        id,
        file_id: ctx.file.id.clone(),
        parent_id,
        name,
        kind,
        language_identity: None,
        span,
        body_span,
        signature_span,
        signature,
        visibility: None,
        docs,
        attributes,
        provenance: Provenance::new("tree-sitter-python", format!("{} declaration", node.kind())),
        confidence: Confidence::ExactSyntax,
        freshness: Freshness::Fresh,
        arity,
    })
}

pub(crate) fn js_ts_symbol_from_node(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> Option<ParsedSymbol> {
    let mut kind = match node.kind() {
        "class_declaration" | "abstract_class_declaration" => SymbolKind::Class,
        "enum_declaration" => SymbolKind::Enum,
        "function"
        | "function_declaration"
        | "function_expression"
        | "function_signature"
        | "generator_function"
        | "generator_function_declaration" => SymbolKind::Function,
        "interface_declaration" => SymbolKind::Interface,
        "ambient_declaration"
        | "internal_module"
        | "module"
        | "module_declaration"
        | "namespace_declaration" => SymbolKind::Module,
        "method_definition" | "method_signature" | "abstract_method_signature" => {
            SymbolKind::Method
        }
        "public_field_definition" | "field_definition" | "property_signature" => SymbolKind::Field,
        "type_alias_declaration" => SymbolKind::TypeAlias,
        "variable_declarator" => {
            if js_ts_variable_is_for_loop_local(node) {
                return None;
            }
            js_ts_variable_symbol_kind(node, ctx.source)?
        }
        _ => return None,
    };
    if kind == SymbolKind::Function
        && parent_symbol
            .map(|(_, parent_kind)| *parent_kind == SymbolKind::Class)
            .unwrap_or(false)
    {
        kind = SymbolKind::Method;
    }
    if kind == SymbolKind::Field
        && js_ts_node_value_is_function_like(node)
        && (parent_symbol
            .map(|(_, parent_kind)| *parent_kind == SymbolKind::Class)
            .unwrap_or(false)
            || node
                .parent()
                .map(|parent| matches!(parent.kind(), "class_body"))
                .unwrap_or(false))
    {
        kind = SymbolKind::Method;
    }

    let name = js_ts_symbol_name(node, kind, ctx.source)?;
    let body = js_ts_symbol_body(node, kind);
    let span = span_from_node(node);
    let body_span = body.map(span_from_node);
    // Only anchor the signature header on a true brace/block `body` child. The
    // `value`/`right` fallback (arrow functions, expression-bodied members) and
    // `variable_declarator` symbols start the node at the binding name, so the
    // header would drop `const`/`let` or the parameter list — leave `None` there.
    let signature_span = signature_span_from_nodes(node, node.child_by_field_name("body"));
    let parent_id = parent_symbol.map(|(id, _)| id.clone());
    let id = symbol_id(&ctx.file, parent_id.as_ref(), kind, &name, span);
    let mut attributes = js_ts_attributes_for_symbol(node, kind, &name, ctx);
    attributes.sort();
    attributes.dedup();
    let arity = if matches!(kind, SymbolKind::Function | SymbolKind::Method) {
        js_ts_arity_for_node(node)
    } else {
        None
    };

    Some(ParsedSymbol {
        id,
        file_id: ctx.file.id.clone(),
        parent_id,
        name,
        kind,
        language_identity: None,
        span,
        body_span,
        signature_span,
        signature: signature_text(node, body, ctx.source),
        visibility: js_ts_visibility_text(node, ctx.source),
        docs: js_ts_docs_for_node(node, ctx.source),
        attributes,
        provenance: Provenance::new("tree-sitter-js-ts", format!("{} declaration", node.kind())),
        confidence: Confidence::ExactSyntax,
        freshness: Freshness::Fresh,
        arity,
    })
}

/// Count fixed positional parameters on a JS/TS function/method node. Tries
/// the `parameters` field first; falls back to scanning for a
/// `formal_parameters` child to cover variable declarators whose value is an
/// arrow function or function expression.
pub(crate) fn js_ts_arity_for_node(node: Node<'_>) -> Option<u8> {
    if let Some(params) = node.child_by_field_name("parameters") {
        return Some(u8::try_from(named_child_count(params)).unwrap_or(u8::MAX));
    }
    let value = node.child_by_field_name("value")?;
    let params = value.child_by_field_name("parameters")?;
    Some(u8::try_from(named_child_count(params)).unwrap_or(u8::MAX))
}

pub(crate) fn js_ts_variable_symbol_kind(node: Node<'_>, source: &str) -> Option<SymbolKind> {
    let value = node.child_by_field_name("value");
    let value_kind = value.map(|node| node.kind()).unwrap_or_default();
    if matches!(
        value_kind,
        "arrow_function" | "function" | "function_expression" | "generator_function"
    ) {
        return Some(SymbolKind::Function);
    }
    if matches!(value_kind, "class" | "class_expression") {
        return Some(SymbolKind::Class);
    }
    let _ = source;
    Some(SymbolKind::Const)
}

/// A `variable_declarator` introduced by a C-style `for (let i = 0; ...; ...)`
/// is anchored on a `lexical_declaration` whose parent is the enclosing
/// `for_statement`. Loop counters and similar locals are excluded from the
/// declaration set in both Squeezy and the JS/TS oracle so navigation does
/// not get flooded by `i`/`j`/`len` per loop site.
pub(crate) fn js_ts_variable_is_for_loop_local(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if !matches!(
        parent.kind(),
        "lexical_declaration" | "variable_declaration"
    ) {
        return false;
    }
    let Some(grand) = parent.parent() else {
        return false;
    };
    matches!(
        grand.kind(),
        "for_statement" | "for_in_statement" | "for_of_statement"
    )
}

pub(crate) fn js_ts_node_value_is_function_like(node: Node<'_>) -> bool {
    node.child_by_field_name("value")
        .map(|value| {
            matches!(
                value.kind(),
                "arrow_function" | "function" | "function_expression" | "generator_function"
            )
        })
        .unwrap_or(false)
}

pub(crate) fn js_ts_symbol_name(node: Node<'_>, kind: SymbolKind, source: &str) -> Option<String> {
    if kind == SymbolKind::Module {
        let raw_name = node
            .child_by_field_name("name")
            .and_then(|child| node_text(child, source).ok())?
            .trim()
            .to_string();
        if raw_name.starts_with(['"', '\'']) {
            return None;
        }
        return Some(js_ts_clean_property_name(&raw_name)).filter(|text| !text.is_empty());
    }
    if kind == SymbolKind::Method {
        if js_ts_method_is_accessor(node, source) {
            return None;
        }
        let name_node = node
            .child_by_field_name("name")
            .or_else(|| node.child_by_field_name("property"));
        if let Some(name) = name_node {
            let name = node_text(name, source)
                .ok()
                .map(js_ts_clean_property_name)
                .filter(|text| !text.is_empty())?;
            if name == "constructor" {
                return None;
            }
            return Some(name);
        }
    }
    if node.kind() == "variable_declarator" {
        return node
            .child_by_field_name("name")
            .and_then(|child| node_text(child, source).ok())
            .and_then(js_ts_binding_name);
    }
    // JavaScript field/method definitions expose the identifier as `property`,
    // while TypeScript uses `name`; falling back covers both grammars without
    // duplicating the rest of the lookup.
    node.child_by_field_name("name")
        .or_else(|| node.child_by_field_name("property"))
        .and_then(|child| node_text(child, source).ok())
        .map(js_ts_clean_property_name)
        .filter(|text| !text.is_empty())
}

pub(crate) fn js_ts_binding_name(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if is_js_ts_identifier(trimmed) {
        return Some(trimmed.to_string());
    }
    None
}

/// Tree-sitter parses a few constructs as flat keyword + identifier sequences
/// rather than `variable_declarator` or `module_declaration` wrappers, so the
/// regular declaration walk does not pick them up:
///
/// - `declare global { ... }` is an `ambient_declaration` whose `global`
///   segment is an anonymous keyword token rather than a named identifier
///   child. TypeScript treats it as a Module named `global`.
/// - `using x = expr` / `await using x = expr` (TC39 Stage 3) parse as an
///   `assignment_expression` whose first anonymous child is the `using`
///   keyword and whose `left` field is the binding identifier. TypeScript
///   treats these as ordinary `VariableDeclaration` Const symbols.
///
/// Synthesizing matching graph symbols keeps the JS/TS oracle from flagging
/// them as false negatives without forcing downstream consumers to special
/// case these surface syntaxes.
pub(crate) fn js_ts_synthetic_binding_symbol(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> Option<ParsedSymbol> {
    match node.kind() {
        "ambient_declaration" => js_ts_declare_global_symbol(node, ctx, parent_symbol),
        "assignment_expression" => js_ts_using_binding_symbol(node, ctx, parent_symbol),
        _ => None,
    }
}

pub(crate) fn js_ts_declare_global_symbol(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> Option<ParsedSymbol> {
    let mut cursor = node.walk();
    let mut declare_seen = false;
    for child in node.children(&mut cursor) {
        if child.is_named() {
            return None;
        }
        let text = node_text(child, ctx.source).ok()?.trim().to_string();
        if !declare_seen {
            if text == "declare" {
                declare_seen = true;
            } else {
                return None;
            }
            continue;
        }
        if text == "global" {
            let span = span_from_node(child);
            let parent_id = parent_symbol.map(|(id, _)| id.clone());
            let id = symbol_id(
                &ctx.file,
                parent_id.as_ref(),
                SymbolKind::Module,
                "global",
                span,
            );
            let attributes = vec![
                js_ts_language_tag(ctx.file.language),
                "declare:global".to_string(),
            ]
            .into_iter()
            .filter(|attr| !attr.is_empty())
            .collect();
            let body_span = {
                let mut walker = node.walk();
                node.children(&mut walker)
                    .find(|child| child.kind() == "statement_block")
                    .map(span_from_node)
            };
            return Some(ParsedSymbol {
                id,
                file_id: ctx.file.id.clone(),
                parent_id,
                name: "global".to_string(),
                kind: SymbolKind::Module,
                language_identity: None,
                span,
                body_span,
                // `span` is only the `global` keyword (not the full declaration),
                // so a header range anchored on it would be inconsistent; the
                // full-span fallback is already minimal here.
                signature_span: None,
                signature: "declare global".to_string(),
                visibility: None,
                docs: Vec::new(),
                attributes,
                provenance: Provenance::new(
                    "tree-sitter-js-ts",
                    "declare global module".to_string(),
                ),
                confidence: Confidence::ExactSyntax,
                freshness: Freshness::Fresh,
                arity: None,
            });
        }
        return None;
    }
    None
}

/// Recognize `using x = expr` and `await using x = expr` bindings. The
/// tree-sitter grammar (still pre-`using`) parses these as an assignment
/// expression with `using` as the first anonymous token; everything that
/// looks like a normal assignment is rejected so we never emit a Const for
/// `obj.foo = bar` or other reassignments.
pub(crate) fn js_ts_using_binding_symbol(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> Option<ParsedSymbol> {
    let mut cursor = node.walk();
    let mut first_anonymous_token: Option<String> = None;
    for child in node.children(&mut cursor) {
        if child.is_named() {
            break;
        }
        if let Ok(text) = node_text(child, ctx.source) {
            let token = text.trim();
            if !token.is_empty() {
                first_anonymous_token = Some(token.to_string());
                break;
            }
        }
    }
    if first_anonymous_token.as_deref() != Some("using") {
        return None;
    }
    let left = node.child_by_field_name("left")?;
    if left.kind() != "identifier" {
        return None;
    }
    let name = node_text(left, ctx.source).ok()?.trim().to_string();
    if name.is_empty() {
        return None;
    }
    let span = span_from_node(left);
    let parent_id = parent_symbol.map(|(id, _)| id.clone());
    let id = symbol_id(
        &ctx.file,
        parent_id.as_ref(),
        SymbolKind::Const,
        &name,
        span,
    );
    let attributes = vec![js_ts_language_tag(ctx.file.language), "using".to_string()]
        .into_iter()
        .filter(|attr| !attr.is_empty())
        .collect();
    Some(ParsedSymbol {
        id,
        file_id: ctx.file.id.clone(),
        parent_id,
        name,
        kind: SymbolKind::Const,
        language_identity: None,
        span,
        body_span: None,
        signature_span: None,
        signature: node_text(node, ctx.source).unwrap_or_default().to_string(),
        visibility: None,
        docs: Vec::new(),
        attributes,
        provenance: Provenance::new("tree-sitter-js-ts", "using declaration".to_string()),
        confidence: Confidence::ExactSyntax,
        freshness: Freshness::Fresh,
        arity: None,
    })
}

pub(crate) fn js_ts_language_tag(language: LanguageKind) -> String {
    match language {
        LanguageKind::JavaScript => "javascript".to_string(),
        LanguageKind::Jsx => "jsx".to_string(),
        LanguageKind::TypeScript => "typescript".to_string(),
        LanguageKind::Tsx => "tsx".to_string(),
        _ => String::new(),
    }
}

/// Detect `get foo()`/`set foo()` accessors on `method_definition` and
/// `method_signature` nodes. We look at the header (everything up to the
/// parameter list or method body) for a `get` or `set` keyword token that
/// is not the method's own name. Inspecting only the header avoids the old
/// substring scan that misfired on benign occurrences of " set " or " get "
/// inside comments or the method body itself.
pub(crate) fn js_ts_method_is_accessor(node: Node<'_>, source: &str) -> bool {
    let name_node = node.child_by_field_name("name");
    let header_end = node
        .child_by_field_name("parameters")
        .map(|params| params.start_byte())
        .or_else(|| {
            node.child_by_field_name("body")
                .map(|body| body.start_byte())
        })
        .unwrap_or(node.end_byte());
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.start_byte() >= header_end {
            break;
        }
        if matches!(name_node, Some(name) if name.id() == child.id()) {
            break;
        }
        if let Ok(text) = node_text(child, source) {
            let token = text.trim();
            if token == "get" || token == "set" {
                return true;
            }
        }
    }
    false
}

pub(crate) fn js_ts_clean_property_name(text: &str) -> String {
    let name = text.trim().trim_matches(['"', '\'']).to_string();
    if name.starts_with('#') || name.starts_with('[') || name.contains('[') || name.contains(']') {
        String::new()
    } else {
        name
    }
}

pub(crate) fn js_ts_symbol_body(node: Node<'_>, kind: SymbolKind) -> Option<Node<'_>> {
    node.child_by_field_name("body").or_else(|| {
        if matches!(kind, SymbolKind::Function | SymbolKind::Method) {
            node.child_by_field_name("value")
                .or_else(|| node.child_by_field_name("right"))
        } else {
            None
        }
    })
}

pub(crate) fn js_ts_attributes_for_symbol(
    node: Node<'_>,
    kind: SymbolKind,
    name: &str,
    ctx: &ExtractContext<'_>,
) -> Vec<String> {
    let mut attributes = Vec::new();
    match ctx.file.language {
        LanguageKind::JavaScript => attributes.push("javascript".to_string()),
        LanguageKind::Jsx => attributes.push("jsx".to_string()),
        LanguageKind::TypeScript => attributes.push("typescript".to_string()),
        LanguageKind::Tsx => attributes.push("tsx".to_string()),
        _ => {}
    }
    if matches!(ctx.file.language, LanguageKind::Jsx | LanguageKind::Tsx)
        && matches!(
            kind,
            SymbolKind::Function | SymbolKind::Method | SymbolKind::Class
        )
        && name
            .chars()
            .next()
            .map(|ch| ch.is_ascii_uppercase())
            .unwrap_or(false)
    {
        attributes.push("framework:component-like".to_string());
        attributes.push("jsx:component".to_string());
    }
    if js_ts_node_has_jsx_descendant(node) {
        attributes.push("jsx:returns-jsx".to_string());
    }
    // Record inheritance as queryable `base:`/`iface:` attributes (mirroring C#,
    // Java, Dart, Python). Without this the JS/TS heritage was only emitted as
    // type-reference edges, so `decl_search attribute=base:X` and the grep→graph
    // augment returned nothing for TS/JS — forcing the model into grep+read_file
    // storms to enumerate subclasses. Read only the header (start..body) so we
    // never scan the class body.
    if matches!(kind, SymbolKind::Class | SymbolKind::Interface) {
        let header_end = node
            .child_by_field_name("body")
            .map(|body| body.start_byte())
            .unwrap_or_else(|| node.end_byte());
        if let Some(header) = ctx.source.get(node.start_byte()..header_end) {
            let (extends, implements) = js_ts_heritage_split(header);
            for base in extends {
                attributes.push(format!("base:{base}"));
            }
            for iface in implements {
                attributes.push(format!("iface:{iface}"));
            }
        }
    }
    attributes.extend(js_ts_decorator_attributes(node, ctx.source));
    attributes
}

pub(crate) fn js_ts_decorator_attributes(node: Node<'_>, source: &str) -> Vec<String> {
    let Ok(raw) = node_text(node, source) else {
        return Vec::new();
    };
    raw.lines()
        .map(str::trim)
        .take_while(|line| line.starts_with('@'))
        .filter_map(|line| {
            let name = line
                .trim_start_matches('@')
                .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '.'))
                .next()
                .unwrap_or_default();
            (!name.is_empty()).then(|| format!("decorator:{name}"))
        })
        .collect()
}

pub(crate) fn js_ts_node_has_jsx_descendant(node: Node<'_>) -> bool {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if current.kind().starts_with("jsx_") {
            return true;
        }
        let mut cursor = current.walk();
        stack.extend(current.named_children(&mut cursor));
    }
    false
}

pub(crate) fn js_ts_visibility_text(node: Node<'_>, source: &str) -> Option<String> {
    let raw = node_text(node, source).ok()?.trim_start();
    ["public", "private", "protected", "readonly", "static"]
        .into_iter()
        .find(|keyword| raw.starts_with(*keyword))
        .map(str::to_string)
}

pub(crate) fn js_ts_docs_for_node(node: Node<'_>, source: &str) -> Vec<String> {
    let start = node.start_byte();
    let before = source.get(..start).unwrap_or_default();
    let Some(comment_start) = before.rfind("/**") else {
        return Vec::new();
    };
    let between = before[comment_start..].trim();
    if between.ends_with("*/") && between.lines().count() <= 20 {
        vec![between.to_string()]
    } else {
        Vec::new()
    }
}

pub(crate) fn extract_python_symbol_facts(
    node: Node<'_>,
    symbol: &ParsedSymbol,
    ctx: &mut ExtractContext<'_>,
) {
    if symbol.kind == SymbolKind::Class {
        for base in python_class_bases(&symbol.signature) {
            ctx.references.push(ParsedReference {
                file_id: ctx.file.id.clone(),
                owner_id: Some(symbol.id.clone()),
                text: base,
                kind: ReferenceKind::Type,
                span: symbol.span,
                provenance: Provenance::new("tree-sitter-python", "class base reference"),
            });
        }
    }

    if matches!(symbol.kind, SymbolKind::Function | SymbolKind::Method) {
        for annotation in python_type_annotations(&symbol.signature) {
            ctx.references.push(ParsedReference {
                file_id: ctx.file.id.clone(),
                owner_id: Some(symbol.id.clone()),
                text: annotation.clone(),
                kind: ReferenceKind::Type,
                span: symbol.span,
                provenance: Provenance::new("tree-sitter-python", "type annotation reference"),
            });
            ctx.body_hits.push(BodyHit {
                file_id: ctx.file.id.clone(),
                owner_id: Some(symbol.id.clone()),
                text: annotation,
                kind: BodyHitKind::Type,
                span: symbol.span,
            });
        }
    }

    let _ = node;
}

pub(crate) fn extract_js_ts_symbol_facts(
    node: Node<'_>,
    symbol: &ParsedSymbol,
    ctx: &mut ExtractContext<'_>,
) {
    if matches!(symbol.kind, SymbolKind::Class | SymbolKind::Interface) {
        for type_name in js_ts_extends_implements_names(&symbol.signature) {
            ctx.references.push(ParsedReference {
                file_id: ctx.file.id.clone(),
                owner_id: Some(symbol.id.clone()),
                text: type_name,
                kind: ReferenceKind::Type,
                span: symbol.span,
                provenance: Provenance::new("tree-sitter-js-ts", "heritage reference"),
            });
        }
    }
    for type_name in js_ts_type_reference_names(&symbol.signature) {
        ctx.references.push(ParsedReference {
            file_id: ctx.file.id.clone(),
            owner_id: Some(symbol.id.clone()),
            text: type_name.clone(),
            kind: ReferenceKind::Type,
            span: symbol.span,
            provenance: Provenance::new("tree-sitter-js-ts", "type annotation reference"),
        });
        ctx.body_hits.push(BodyHit {
            file_id: ctx.file.id.clone(),
            owner_id: Some(symbol.id.clone()),
            text: type_name,
            kind: BodyHitKind::Type,
            span: symbol.span,
        });
    }
    let _ = node;
}

pub(crate) fn js_ts_extends_implements_names(signature: &str) -> Vec<String> {
    let mut names = Vec::new();
    for keyword in ["extends", "implements"] {
        let Some((_, rest)) = signature.split_once(keyword) else {
            continue;
        };
        let before_body = rest
            .split_once('{')
            .map(|(before, _)| before)
            .unwrap_or(rest)
            .split_once(" from ")
            .map(|(before, _)| before)
            .unwrap_or(rest);
        names.extend(
            split_top_level_commas(before_body)
                .into_iter()
                .filter_map(|name| js_ts_type_name_from_annotation(&name)),
        );
    }
    names.sort();
    names.dedup();
    names
}

/// Strip balanced `<...>` generic groups from a class/interface header so a
/// `class X<T extends Base>` parameter constraint or a `Base<Y>` argument never
/// confuses the `extends`/`implements` keyword scan in [`js_ts_heritage_split`].
fn strip_angle_groups(header: &str) -> String {
    let mut out = String::with_capacity(header.len());
    let mut depth: u32 = 0;
    for ch in header.chars() {
        match ch {
            '<' => depth += 1,
            '>' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    out
}

/// Split a JS/TS class/interface declaration header into its `extends` targets
/// (superclass / extended interfaces -> `base:`) and `implements` targets
/// (-> `iface:`). The `extends` clause is cut at `implements` so a multi-target
/// `implements A, B` never leaks into the base list, and generic argument lists
/// are stripped first so `extends Base<T>` and `<T extends X>` resolve cleanly.
/// Keyword matching is space-delimited so an identifier such as `extendsFoo`
/// does not register as an `extends` clause.
pub(crate) fn js_ts_heritage_split(declaration: &str) -> (Vec<String>, Vec<String>) {
    let header = declaration
        .split_once('{')
        .map(|(before, _)| before)
        .unwrap_or(declaration);
    let cleaned = strip_angle_groups(header);
    // Pad so a clause at the very start still matches the space-delimited scan.
    let padded = format!(" {cleaned} ");
    let clause = |keyword: &str, stop: Option<&str>| -> Vec<String> {
        let Some((_, rest)) = padded.split_once(keyword) else {
            return Vec::new();
        };
        let scope = match stop {
            Some(stop_kw) => rest
                .split_once(stop_kw)
                .map(|(before, _)| before)
                .unwrap_or(rest),
            None => rest,
        };
        let mut names: Vec<String> = split_top_level_commas(scope)
            .into_iter()
            .filter_map(|name| js_ts_type_name_from_annotation(&name))
            .collect();
        names.sort();
        names.dedup();
        names
    };
    let extends = clause(" extends ", Some(" implements "));
    let implements = clause(" implements ", None);
    (extends, implements)
}

pub(crate) fn js_ts_type_reference_names(signature: &str) -> Vec<String> {
    let mut names = Vec::new();
    for segment in signature.split([':', '<', '|', '&']) {
        if let Some(name) = js_ts_type_name_from_annotation(segment) {
            names.push(name);
        }
    }
    names.sort();
    names.dedup();
    names
}

pub(crate) fn js_ts_type_name_from_annotation(annotation: &str) -> Option<String> {
    let text = annotation
        .split(['=', ';', ',', ')', '(', '[', ']', '{', '}'])
        .next()
        .unwrap_or(annotation)
        .trim()
        .trim_start_matches("readonly ")
        .trim();
    if text.is_empty()
        || matches!(
            text,
            "any"
                | "bigint"
                | "boolean"
                | "false"
                | "never"
                | "null"
                | "number"
                | "object"
                | "string"
                | "symbol"
                | "true"
                | "undefined"
                | "unknown"
                | "void"
        )
    {
        return None;
    }
    let name = last_path_segment(text);
    if is_js_ts_identifier(&name)
        && name
            .chars()
            .next()
            .map(|ch| ch.is_ascii_uppercase())
            .unwrap_or(false)
    {
        Some(name)
    } else {
        None
    }
}

pub(crate) fn java_attributes_for_node(node: Node<'_>, source: &str) -> Vec<String> {
    let mut attributes = Vec::new();
    let modifiers = java_modifiers_node(node);
    let Some(modifiers) = modifiers else {
        return attributes;
    };
    let mut cursor = modifiers.walk();
    for child in modifiers.named_children(&mut cursor) {
        let Some(annotation) = java_annotation_name(child, source) else {
            continue;
        };
        attributes.push(format!("java:annotation:{annotation}"));
        let leaf = annotation.rsplit('.').next().unwrap_or(annotation.as_str());
        match leaf {
            "Test" | "ParameterizedTest" => attributes.push("junit:test".to_string()),
            "Override" => attributes.push("java:override".to_string()),
            _ => {}
        }
    }
    attributes
}

pub(crate) fn java_modifiers_node(node: Node<'_>) -> Option<Node<'_>> {
    if let Some(modifiers) = node.child_by_field_name("modifiers") {
        return Some(modifiers);
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.kind() == "modifiers")
}

pub(crate) fn java_annotation_name(node: Node<'_>, source: &str) -> Option<String> {
    if !matches!(node.kind(), "marker_annotation" | "annotation") {
        return None;
    }
    let name_node = node
        .child_by_field_name("name")
        .or_else(|| java_first_name_descendant(node))?;
    let raw = node_text(name_node, source).ok()?.trim().to_string();
    if raw.is_empty() { None } else { Some(raw) }
}

pub(crate) fn java_visibility_text(node: Node<'_>, source: &str) -> Option<String> {
    let modifiers = java_modifiers_node(node)?;
    let mut cursor = modifiers.walk();
    for child in modifiers.children(&mut cursor) {
        let raw = node_text(child, source).unwrap_or_default().trim();
        if matches!(raw, "public" | "protected" | "private") {
            return Some(raw.to_string());
        }
    }
    None
}

pub(crate) fn java_docs_for_node(node: Node<'_>, source: &str) -> Vec<String> {
    let mut docs = Vec::new();
    let Some(mut previous) = node.prev_named_sibling() else {
        return docs;
    };
    while previous.kind() == "line_comment" || previous.kind() == "block_comment" {
        if let Ok(text) = node_text(previous, source) {
            let trimmed = text
                .trim()
                .trim_start_matches("/**")
                .trim_start_matches("/*")
                .trim_start_matches("//")
                .trim_end_matches("*/")
                .trim()
                .to_string();
            if !trimmed.is_empty() {
                docs.push(trimmed);
            }
        }
        let Some(next_previous) = previous.prev_named_sibling() else {
            break;
        };
        previous = next_previous;
    }
    docs.reverse();
    docs
}

pub(crate) fn collect_java_type_names(node: Node<'_>, source: &str, names: &mut Vec<String>) {
    if matches!(
        node.kind(),
        "type_identifier" | "scoped_type_identifier" | "generic_type"
    ) && let Ok(text) = node_text(node, source)
        && let Some(name) = java_type_name_from_text(text)
    {
        names.push(name);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_java_type_names(child, source, names);
    }
}

pub(crate) fn java_type_name_from_text(text: &str) -> Option<String> {
    let clean = text
        .split('<')
        .next()
        .unwrap_or(text)
        .trim()
        .trim_end_matches("[]")
        .to_string();
    if clean.is_empty() || is_java_keyword(&clean) {
        None
    } else {
        Some(clean)
    }
}

pub(crate) fn java_field_type(node: Node<'_>, source: &str) -> Option<String> {
    if let Some(child) = node.child_by_field_name("type")
        && let Ok(text) = node_text(child, source)
        && let Some(name) = java_type_name_from_text(text)
    {
        return Some(name);
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| {
            matches!(
                child.kind(),
                "type_identifier"
                    | "scoped_type_identifier"
                    | "generic_type"
                    | "array_type"
                    | "integral_type"
                    | "floating_point_type"
                    | "boolean_type"
                    | "void_type"
            )
        })
        .and_then(|child| node_text(child, source).ok())
        .and_then(java_type_name_from_text)
}

pub(crate) fn java_object_type_from_text(raw: &str) -> String {
    raw.split_once("new ")
        .map(|(_, rest)| rest)
        .unwrap_or(raw)
        .split('(')
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

pub(crate) fn is_java_test_symbol(
    relative_path: &str,
    kind: SymbolKind,
    name: &str,
    attributes: &[String],
) -> bool {
    matches!(kind, SymbolKind::Method | SymbolKind::Class)
        && (relative_path.contains("/test/")
            || relative_path.ends_with("Test.java")
            || name.ends_with("Test")
            || attributes.iter().any(|attribute| attribute == "junit:test"))
}

pub(crate) fn python_attributes_for_node(node: Node<'_>, source: &str) -> Vec<String> {
    let Some(parent) = node.parent() else {
        return Vec::new();
    };
    if parent.kind() != "decorated_definition" {
        return Vec::new();
    }
    let mut cursor = parent.walk();
    let mut attributes = parent
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "decorator")
        .filter_map(|child| node_text(child, source).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>();

    let mut semantic = attributes
        .iter()
        .flat_map(|attribute| python_semantic_attributes(attribute))
        .collect::<Vec<_>>();
    attributes.append(&mut semantic);
    attributes.sort();
    attributes.dedup();
    attributes
}

pub(crate) fn python_semantic_attributes(attribute: &str) -> Vec<String> {
    let trimmed = attribute.trim().trim_start_matches('@').trim();
    let target = trimmed
        .split('(')
        .next()
        .unwrap_or(trimmed)
        .trim()
        .trim_end_matches('.');
    let leaf = target.rsplit('.').next().unwrap_or(target);
    let mut attributes = Vec::new();
    match leaf {
        "property" | "staticmethod" | "classmethod" => {
            attributes.push(format!("python:{leaf}"));
        }
        "dataclass" => attributes.push("python:dataclass".to_string()),
        "fixture" => attributes.push("pytest:fixture".to_string()),
        "validator" | "field_validator" | "model_validator" => {
            attributes.push(format!("pydantic:{leaf}"));
        }
        "get" | "post" | "put" | "patch" | "delete" | "options" | "head" | "route" => {
            let receiver = target.rsplit_once('.').map(|(receiver, _)| receiver);
            if receiver
                .map(|receiver| {
                    matches!(
                        receiver.rsplit('.').next().unwrap_or(receiver),
                        "app" | "router" | "blueprint" | "bp"
                    )
                })
                .unwrap_or(false)
            {
                let method = leaf.to_ascii_uppercase();
                attributes.push(format!("route:{method}"));
                if let Some(path) = first_python_string_literal(attribute) {
                    attributes.push(format!("route:{method} {path}"));
                }
                attributes.push("framework:web-route".to_string());
            }
        }
        _ => {}
    }
    if target.contains("fastapi") || target.contains("APIRouter") {
        attributes.push("framework:fastapi".to_string());
    }
    if target.contains("flask") || target.contains("Blueprint") {
        attributes.push("framework:flask".to_string());
    }
    attributes
}

pub(crate) fn python_test_attributes(
    relative_path: &str,
    kind: SymbolKind,
    name: &str,
) -> Vec<String> {
    let file_name = relative_path.rsplit('/').next().unwrap_or(relative_path);
    let is_test_file = file_name.starts_with("test_") || file_name.ends_with("_test.py");
    match kind {
        SymbolKind::Function | SymbolKind::Method | SymbolKind::Test
            if is_test_file || name.starts_with("test_") =>
        {
            vec!["python:test".to_string(), "pytest:test".to_string()]
        }
        SymbolKind::Class if is_test_file || name.starts_with("Test") => {
            vec![
                "python:test-class".to_string(),
                "pytest:test-class".to_string(),
            ]
        }
        _ => Vec::new(),
    }
}

pub(crate) fn first_python_string_literal(text: &str) -> Option<String> {
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        let quote = match ch {
            '\'' | '"' => ch,
            _ => continue,
        };
        let mut value = String::new();
        let mut escaped = false;
        for ch in chars.by_ref() {
            if escaped {
                value.push(ch);
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote {
                return Some(value);
            } else {
                value.push(ch);
            }
        }
    }
    None
}

pub(crate) fn python_docs_for_node(node: Node<'_>, source: &str) -> Vec<String> {
    let Some(body) = node.child_by_field_name("body") else {
        return Vec::new();
    };
    let mut cursor = body.walk();
    let Some(first) = body.named_children(&mut cursor).next() else {
        return Vec::new();
    };
    let doc_node = if first.kind() == "expression_statement" {
        let mut first_cursor = first.walk();
        first
            .named_children(&mut first_cursor)
            .find(|child| child.kind() == "string")
    } else if first.kind() == "string" {
        Some(first)
    } else {
        None
    };
    doc_node
        .and_then(|node| node_text(node, source).ok())
        .map(|text| vec![text.trim().to_string()])
        .unwrap_or_default()
}

/// Returns true when the node lies inside a Python `@decorator(...)` head,
/// stopping at the enclosing function/class/lambda body.
pub(crate) fn python_node_is_inside_decorator(node: Node<'_>) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "decorator" => return true,
            "function_definition" | "class_definition" | "lambda" => return false,
            _ => current = parent.parent(),
        }
    }
    false
}

pub(crate) fn python_class_bases(signature: &str) -> Vec<String> {
    let Some(after_class) = signature.trim().strip_prefix("class ") else {
        return Vec::new();
    };
    let Some(open_index) = after_class.find('(') else {
        return Vec::new();
    };
    let Some(close_index) = matching_close_paren(after_class, open_index) else {
        return Vec::new();
    };
    split_top_level_commas(&after_class[open_index + 1..close_index])
        .into_iter()
        // Class headers admit keyword arguments (`metaclass=`, `total=`,
        // `frozen=`, ...); those are not base classes and `python_type_name_from_annotation`
        // would otherwise strip `metaclass=Meta` down to the keyword name
        // `"metaclass"` and silently drop `Meta`.
        .filter(|item| !item.contains('='))
        .filter_map(|base| python_type_name_from_annotation(&base))
        .collect()
}

pub(crate) fn python_type_annotations(signature: &str) -> Vec<String> {
    let mut annotations = Vec::new();
    if let Some(open_index) = signature.find('(')
        && let Some(close_index) = matching_close_paren(signature, open_index)
    {
        for parameter in split_top_level_commas(&signature[open_index + 1..close_index]) {
            if let Some((_, annotation)) = parameter.split_once(':')
                && let Some(name) = python_type_name_from_annotation(annotation)
            {
                annotations.push(name);
            }
        }
        let rest = &signature[close_index + 1..];
        if let Some((_, return_annotation)) = rest.split_once("->") {
            let return_annotation = return_annotation
                .split_once(':')
                .map(|(before, _)| before)
                .unwrap_or(return_annotation);
            if let Some(name) = python_type_name_from_annotation(return_annotation) {
                annotations.push(name);
            }
        }
    }
    annotations.sort();
    annotations.dedup();
    annotations
}

pub(crate) fn python_type_name_from_annotation(annotation: &str) -> Option<String> {
    let mut text = annotation
        .split('=')
        .next()
        .unwrap_or(annotation)
        .trim()
        .trim_matches(|ch: char| {
            matches!(
                ch,
                '\'' | '"' | '[' | ']' | '(' | ')' | '{' | '}' | ':' | ',' | ' '
            )
        })
        .trim_start_matches('*')
        .trim();
    if text.is_empty() {
        return None;
    }
    for separator in ['|', '[', ','] {
        if let Some((before, _)) = text.split_once(separator) {
            text = before.trim();
        }
    }
    if text.is_empty()
        || matches!(
            text,
            "None" | "Any" | "object" | "str" | "int" | "float" | "bool"
        )
    {
        return None;
    }
    Some(last_path_segment(text))
}

pub(crate) fn matching_close_paren(text: &str, open_index: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (index, ch) in text
        .char_indices()
        .skip_while(|(index, _)| *index < open_index)
    {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

pub(crate) fn split_top_level_commas(text: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut start = 0usize;
    let mut depth = 0usize;
    for (index, ch) in text.char_indices() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                let value = text[start..index].trim();
                if !value.is_empty() {
                    values.push(value.to_string());
                }
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }
    let value = text[start..].trim();
    if !value.is_empty() {
        values.push(value.to_string());
    }
    values
}

pub(crate) fn parent_symbol_is_impl_or_trait(parent_symbol: &Option<SymbolId>) -> bool {
    parent_symbol
        .as_ref()
        .map(|id| id.0.contains("::impl:") || id.0.contains("::trait:"))
        .unwrap_or(false)
}

pub(crate) fn function_has_self_parameter(node: Node<'_>, source: &str) -> bool {
    let Some(parameters) = node.child_by_field_name("parameters") else {
        return false;
    };
    let Ok(text) = node_text(parameters, source) else {
        return false;
    };
    let first = text
        .trim()
        .trim_start_matches('(')
        .trim_end_matches(')')
        .split(',')
        .next()
        .unwrap_or_default()
        .trim();
    let first = first.trim_start_matches("mut ").trim();

    first == "self"
        || first.starts_with("self:")
        || first.starts_with("&self")
        || first.starts_with("&mut self")
        || (first.starts_with('&') && first.contains(" self"))
        || first.starts_with("mut self:")
}

pub(crate) fn symbol_name(node: Node<'_>, kind: SymbolKind, source: &str) -> Option<String> {
    if kind == SymbolKind::Impl {
        return Some(impl_name(node, source));
    }

    node.child_by_field_name("name")
        .and_then(|child| node_text(child, source).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

pub(crate) fn impl_name(node: Node<'_>, source: &str) -> String {
    let raw = signature_text(node, node.child_by_field_name("body"), source);
    trim_impl_header(&collapse_whitespace(&raw))
}

/// Extract a queryable type name for a struct `field_declaration` from its
/// `type` field. Leading `&`/`&mut`/`*` and lifetimes are stripped and any
/// generic argument list is dropped so `Vec<String>` -> `Vec`, mirroring how the
/// Java/Kotlin extractors record `type:` for fields.
pub(crate) fn rust_field_type(node: Node<'_>, source: &str) -> Option<String> {
    let type_node = node.child_by_field_name("type")?;
    let raw = node_text(type_node, source).ok()?;
    let cleaned = rust_type_name_from_text(raw);
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

pub(crate) fn rust_type_name_from_text(text: &str) -> String {
    let normalized = collapse_whitespace(text);
    let mut rest = normalized.as_str();
    loop {
        let stripped = rest.trim_start_matches(['&', '*']).trim_start();
        let stripped = stripped
            .strip_prefix("mut ")
            .or_else(|| stripped.strip_prefix("const "))
            .or_else(|| stripped.strip_prefix("dyn "))
            .or_else(|| stripped.strip_prefix("impl "))
            .unwrap_or(stripped)
            .trim_start();
        // Drop a leading lifetime such as `'a ` so `&'a str` resolves to `str`.
        let stripped = if stripped.starts_with('\'') {
            stripped
                .split_once(char::is_whitespace)
                .map(|(_, after)| after.trim_start())
                .unwrap_or("")
        } else {
            stripped
        };
        if stripped == rest {
            break;
        }
        rest = stripped;
    }
    // Keep only the head up to any generic argument list, then take the final
    // path segment (`std::vec::Vec` -> `Vec`).
    let head = rest.split('<').next().unwrap_or(rest).trim();
    last_path_segment(head)
}

fn collapse_whitespace(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    for segment in text.split_whitespace() {
        if !normalized.is_empty() {
            normalized.push(' ');
        }
        normalized.push_str(segment);
    }
    normalized
}

pub(crate) fn trim_impl_header(raw: &str) -> String {
    let trimmed = raw.trim();
    let trimmed = trimmed.strip_prefix("unsafe ").unwrap_or(trimmed);
    let Some(rest) = trimmed.strip_prefix("impl") else {
        return trimmed.to_string();
    };
    let Some(next) = rest.chars().next() else {
        return trimmed.to_string();
    };
    if !next.is_whitespace() && next != '<' {
        return trimmed.to_string();
    }

    let mut rest = rest.trim_start();
    if rest.starts_with('<') {
        let mut depth = 0usize;
        let mut close_index = None;
        let mut previous = None;
        for (index, ch) in rest.char_indices() {
            match ch {
                '<' => depth += 1,
                '>' if previous != Some('-') => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        close_index = Some(index + ch.len_utf8());
                        break;
                    }
                }
                _ => {}
            }
            previous = Some(ch);
        }
        if let Some(index) = close_index {
            rest = rest[index..].trim_start();
        }
    }
    rest.split_once(" where ")
        .map(|(before, _)| before)
        .unwrap_or(rest)
        .trim_end_matches(',')
        .to_string()
}

pub(crate) fn symbol_id(
    file: &FileRecord,
    parent_id: Option<&SymbolId>,
    kind: SymbolKind,
    name: &str,
    span: SourceSpan,
) -> SymbolId {
    let kind_name = symbol_kind_name(kind);
    let mut safe_name = String::with_capacity(name.len());
    for ch in name.chars() {
        safe_name.push(if ch.is_ascii_alphanumeric() || ch == '_' {
            ch
        } else {
            '_'
        });
    }
    let base = parent_id
        .map(|id| id.0.clone())
        .unwrap_or_else(|| file.relative_path.clone());
    SymbolId::new(format!(
        "{base}::{kind_name}:{safe_name}@{}",
        span.start_byte
    ))
}

fn symbol_kind_name(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Class => "class",
        SymbolKind::Crate => "crate",
        SymbolKind::File => "file",
        SymbolKind::Interface => "interface",
        SymbolKind::Module => "module",
        SymbolKind::Struct => "struct",
        SymbolKind::Enum => "enum",
        SymbolKind::Union => "union",
        SymbolKind::Trait => "trait",
        SymbolKind::Impl => "impl",
        SymbolKind::Function => "function",
        SymbolKind::Method => "method",
        SymbolKind::Const => "const",
        SymbolKind::Static => "static",
        SymbolKind::TypeAlias => "typealias",
        SymbolKind::Field => "field",
        SymbolKind::Variant => "variant",
        SymbolKind::Macro => "macro",
        SymbolKind::Test => "test",
        SymbolKind::Unknown => "unknown",
    }
}

pub(crate) fn signature_text(node: Node<'_>, body: Option<Node<'_>>, source: &str) -> String {
    let start = node.start_byte();
    let end = body
        .map(|body| body.start_byte())
        .unwrap_or_else(|| node.end_byte());
    source
        .get(start..end)
        .unwrap_or_default()
        .trim()
        .trim_end_matches('=')
        .trim()
        .to_string()
}

pub(crate) fn visibility_text(node: Node<'_>, source: &str) -> Option<String> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(|child| child.kind() == "visibility_modifier")
        .and_then(|child| node_text(child, source).ok())
        .map(|text| text.trim().to_string())
}

pub(crate) fn attributes_for_node(node: Node<'_>, source: &str) -> Vec<String> {
    let mut attributes = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if (child.kind() == "attribute_item" || child.kind() == "inner_attribute_item")
            && let Ok(text) = node_text(child, source)
        {
            attributes.push(text.trim().to_string());
        }
    }
    attributes
}

pub(crate) fn docs_from_attributes(attributes: &[String]) -> Vec<String> {
    attributes
        .iter()
        .filter(|attr| attribute_path(attr).as_deref() == Some("doc"))
        .cloned()
        .collect()
}

pub(crate) fn is_test_function(attributes: &[String]) -> bool {
    attributes.iter().any(|attr| {
        attribute_path(attr)
            .and_then(|path| path.rsplit("::").next().map(str::to_string))
            .as_deref()
            == Some("test")
    })
}

/// Extract the attribute path (the identifier or `::`-separated path that
/// precedes any `(`, `=`, `]`, or `!`). Returns `None` for empty/unrecognized
/// inputs. The attribute text is expected to look like `#[<path>(...)]`,
/// `#[<path> = "..."]`, `#[<path>]`, or the inner-attribute form `#![...]`.
pub(crate) fn attribute_path(attribute: &str) -> Option<String> {
    let trimmed = attribute.trim_start();
    let body = trimmed
        .strip_prefix("#![")
        .or_else(|| trimmed.strip_prefix("#["))?;
    let body = body.trim_start();
    let path: String = body
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_' || *ch == ':')
        .collect();
    if path.is_empty() {
        None
    } else {
        Some(path.trim_end_matches(':').to_string())
    }
}

/// Normalize the raw `#[..]` attribute strings already collected for a symbol
/// into queryable tokens:
///
/// - `#[derive(Serialize, Clone)]` -> `derive:Serialize`, `derive:Clone`
/// - `#[cfg(feature = "x")]` / `#[cfg_attr(..)]` -> `cfg:<predicate>` plus a
///   `rust:conditional` marker used to downgrade the symbol confidence.
/// - any other attribute path -> `rust:attr:<path>` so the bare attribute name
///   is searchable without scanning the raw string.
///
/// The input is the raw attribute list (each element looks like `#[..]` or
/// `#![..]`); the returned tokens are appended to the symbol's attributes.
pub(crate) fn rust_semantic_attributes(raw_attributes: &[String]) -> Vec<String> {
    let mut tokens = Vec::new();
    for attribute in raw_attributes {
        let Some(path) = attribute_path(attribute) else {
            continue;
        };
        let leaf = path.rsplit("::").next().unwrap_or(path.as_str());
        match leaf {
            "derive" => {
                for name in rust_attribute_inner_paths(attribute) {
                    let trait_name = last_path_segment(&name);
                    if !trait_name.is_empty() {
                        tokens.push(format!("derive:{trait_name}"));
                    }
                }
            }
            "cfg" | "cfg_attr" => {
                tokens.push("rust:conditional".to_string());
                if let Some(predicate) = rust_attribute_arguments(attribute) {
                    let predicate = collapse_whitespace(&predicate);
                    if !predicate.is_empty() {
                        tokens.push(format!("cfg:{predicate}"));
                    }
                }
                tokens.push(format!("rust:attr:{path}"));
            }
            "doc" => {}
            _ => tokens.push(format!("rust:attr:{path}")),
        }
    }
    tokens
}

/// Return the comma-separated argument list inside an attribute's `(..)`, e.g.
/// `#[derive(Serialize, Clone)]` -> `"Serialize, Clone"`. Returns `None` for
/// attributes with no parenthesized arguments (`#[inline]`, `#[doc = "x"]`).
fn rust_attribute_arguments(attribute: &str) -> Option<String> {
    let open = attribute.find('(')?;
    let close = matching_close_paren(attribute, open)?;
    Some(attribute[open + 1..close].trim().to_string())
}

/// Split the parenthesized arguments of a `#[derive(..)]` attribute into the
/// individual trait paths, ignoring nested generics/commas.
fn rust_attribute_inner_paths(attribute: &str) -> Vec<String> {
    let Some(args) = rust_attribute_arguments(attribute) else {
        return Vec::new();
    };
    split_top_level_commas(&args)
        .into_iter()
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

/// The self-type of an `impl_item` (`impl Foo`, `impl Trait for Foo`), reduced
/// to its final path segment with generics/refs stripped.
pub(crate) fn rust_impl_self_type(node: Node<'_>, source: &str) -> Option<String> {
    let type_node = node.child_by_field_name("type")?;
    let raw = node_text(type_node, source).ok()?;
    let name = rust_type_name_from_text(raw);
    (!name.is_empty()).then_some(name)
}

/// The implemented trait of an `impl Trait for Type` block, reduced to its
/// final path segment. `None` for inherent impls (`impl Foo { .. }`).
pub(crate) fn rust_impl_trait(node: Node<'_>, source: &str) -> Option<String> {
    let trait_node = node.child_by_field_name("trait")?;
    let raw = node_text(trait_node, source).ok()?;
    let name = rust_type_name_from_text(raw);
    (!name.is_empty()).then_some(name)
}

/// Collect the supertrait names from a `trait_item`'s `bounds` field
/// (`trait Sub: Super + Other`). Lifetimes and `?Sized`-style markers are
/// dropped; each remaining bound is reduced to its final path segment.
pub(crate) fn rust_supertrait_bases(node: Node<'_>, source: &str) -> Vec<String> {
    let Some(bounds) = node.child_by_field_name("bounds") else {
        return Vec::new();
    };
    let mut bases = Vec::new();
    let mut cursor = bounds.walk();
    for child in bounds.named_children(&mut cursor) {
        if child.kind() == "lifetime" {
            continue;
        }
        let Ok(raw) = node_text(child, source) else {
            continue;
        };
        let trimmed = raw.trim().trim_start_matches('?').trim();
        let name = rust_type_name_from_text(trimmed);
        if !name.is_empty() {
            bases.push(name);
        }
    }
    bases
}

#[derive(Debug, Clone, PartialEq, Eq)]

struct ImportSpec {
    path: String,
    alias: Option<String>,
    is_glob: bool,
}

fn expand_use_declaration(raw: &str) -> Vec<ImportSpec> {
    let Some(tree) = strip_use_declaration(raw) else {
        return Vec::new();
    };
    expand_use_tree(tree)
        .into_iter()
        .filter(|import| !import.path.is_empty())
        .collect()
}

pub(crate) fn strip_use_declaration(raw: &str) -> Option<&str> {
    let mut text = raw.trim().trim_end_matches(';').trim();
    if let Some(rest) = text.strip_prefix("pub") {
        text = rest.trim_start();
        if text.starts_with('(')
            && let Some(close) = text.find(')')
        {
            text = text[close + 1..].trim_start();
        }
    }
    text.strip_prefix("use").map(str::trim)
}

fn expand_use_tree(tree: &str) -> Vec<ImportSpec> {
    let tree = tree.trim();
    if tree.is_empty() {
        return Vec::new();
    }
    if let Some((prefix, inner, suffix)) = split_top_level_braces(tree) {
        let prefix = prefix.trim_end_matches("::").trim();
        let suffix = suffix.trim_start_matches("::").trim();
        let mut imports = Vec::new();
        for item in split_top_level_use_commas(inner) {
            let item = item.trim();
            if item.is_empty() {
                continue;
            }
            let combined = join_use_segments(prefix, item, suffix);
            imports.extend(expand_use_tree(&combined));
        }
        return imports;
    }

    let (path, alias) = split_use_alias(tree);
    let path = path.trim().trim_end_matches(';').trim().to_string();
    if path.ends_with("::self") {
        return vec![ImportSpec {
            path: path.trim_end_matches("::self").to_string(),
            alias,
            is_glob: false,
        }];
    }
    vec![ImportSpec {
        is_glob: path.ends_with("::*"),
        path,
        alias,
    }]
}

pub(crate) fn split_use_alias(path: &str) -> (&str, Option<String>) {
    path.rsplit_once(" as ")
        .map(|(path, alias)| (path, Some(alias.trim().to_string())))
        .unwrap_or((path, None))
}

pub(crate) fn split_top_level_braces(text: &str) -> Option<(&str, &str, &str)> {
    let mut depth = 0usize;
    let mut start = None;
    for (index, ch) in text.char_indices() {
        match ch {
            '{' => {
                if depth == 0 {
                    start = Some(index);
                }
                depth += 1;
            }
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    let start = start?;
                    return Some((&text[..start], &text[start + 1..index], &text[index + 1..]));
                }
            }
            _ => {}
        }
    }
    None
}

pub(crate) fn split_top_level_use_commas(text: &str) -> Vec<&str> {
    let mut items = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (index, ch) in text.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                items.push(&text[start..index]);
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }
    items.push(&text[start..]);
    items
}

pub(crate) fn join_use_segments(prefix: &str, item: &str, suffix: &str) -> String {
    let item = if item == "self" { "" } else { item };
    let mut joined = String::with_capacity(prefix.len() + item.len() + suffix.len() + 4);
    let mut has_segment = false;
    for segment in [prefix, item, suffix] {
        let trimmed = segment.trim();
        if trimmed.is_empty() {
            continue;
        }
        if has_segment {
            joined.push_str("::");
        }
        joined.push_str(trimmed.trim_matches(':'));
        has_segment = true;
    }
    joined
}

pub(crate) fn extract_import(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let raw = node_text(node, ctx.source).unwrap_or_default();
    let is_reexport = raw.trim_start().starts_with("pub");
    for import in expand_use_declaration(raw) {
        let kind = if import.is_glob {
            ImportKind::Wildcard
        } else {
            ImportKind::Named
        };
        let imported_name = if import.is_glob {
            None
        } else {
            Some(last_path_segment(&import.path))
        };
        ctx.imports.push(ParsedImport {
            file_id: ctx.file.id.clone(),
            owner_id: owner_id.clone(),
            is_glob: import.is_glob,
            is_reexport,
            is_static: false,
            path: import.path,
            alias: import.alias,
            span: span_from_node(node),
            provenance: Provenance::new("tree-sitter-rust", "use declaration"),
            kind,
            imported_name,
            is_global: false,
        });
    }
}

/// Handle `extern crate foo;` / `extern crate foo as bar;`. The 2015-edition
/// and `no_std`/proc-macro syntax is parsed by tree-sitter as a dedicated
/// `extern_crate_declaration` node (distinct from `use_declaration`), so it is
/// never routed through `extract_import`. We emit a `Named` import for the crate
/// name, honoring the `as` alias, so the crate dependency is visible to the
/// import graph and `Imports` edge resolution.
pub(crate) fn extract_extern_crate(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let Some(name) = node
        .child_by_field_name("name")
        .and_then(|child| node_text(child, ctx.source).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
    else {
        return;
    };
    let alias = node
        .child_by_field_name("alias")
        .and_then(|child| node_text(child, ctx.source).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty() && text != "_");
    let is_reexport = node_text(node, ctx.source)
        .unwrap_or_default()
        .trim_start()
        .starts_with("pub");
    ctx.imports.push(ParsedImport {
        file_id: ctx.file.id.clone(),
        owner_id,
        is_glob: false,
        is_reexport,
        is_static: false,
        path: name.clone(),
        alias,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-rust", "extern crate declaration"),
        kind: ImportKind::Named,
        imported_name: Some(name),
        is_global: false,
    });
}

pub(crate) fn extract_direct_call(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let Some(function_node) = node.child_by_field_name("function") else {
        return;
    };
    let target_text = node_text(function_node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if target_text.is_empty() {
        return;
    }
    let name = last_path_segment(&target_text);
    let arity = node
        .child_by_field_name("arguments")
        .map(|arguments| named_child_count(arguments))
        .unwrap_or_default();

    ctx.calls.push(ParsedCall {
        file_id: ctx.file.id.clone(),
        caller_id: owner_id.clone(),
        name,
        target_text: target_text.clone(),
        receiver: receiver_from_direct_call(&target_text),
        arity,
        kind: ParsedCallKind::Direct,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-rust", "call_expression"),
        confidence: Confidence::Heuristic,
    });
    extract_body_hit(node, BodyHitKind::Call, ctx, owner_id);
}

pub(crate) fn extract_method_call(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let raw = node_text(node, ctx.source).unwrap_or_default();
    let name = node
        .child_by_field_name("name")
        .or_else(|| node.child_by_field_name("method"))
        .and_then(|child| node_text(child, ctx.source).ok())
        .map(|text| text.trim().to_string())
        .unwrap_or_else(|| method_name_from_text(raw));
    if name.is_empty() {
        return;
    }
    let receiver = node
        .child_by_field_name("receiver")
        .and_then(|child| node_text(child, ctx.source).ok())
        .map(|text| text.trim().to_string())
        .or_else(|| receiver_from_method_text(raw, &name));
    let arity = node
        .child_by_field_name("arguments")
        .map(|arguments| named_child_count(arguments))
        .unwrap_or_default();

    ctx.calls.push(ParsedCall {
        file_id: ctx.file.id.clone(),
        caller_id: owner_id.clone(),
        name,
        target_text: raw.trim().to_string(),
        receiver,
        arity,
        kind: ParsedCallKind::Method,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-rust", "method_call_expression"),
        confidence: Confidence::CandidateSet,
    });
    extract_body_hit(node, BodyHitKind::Call, ctx, owner_id);
}

pub(crate) fn extract_macro_call(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let raw = node_text(node, ctx.source).unwrap_or_default();
    let target = raw.split('!').next().unwrap_or_default().trim().to_string();
    if target.is_empty() {
        return;
    }

    ctx.calls.push(ParsedCall {
        file_id: ctx.file.id.clone(),
        caller_id: owner_id.clone(),
        name: last_path_segment(&target),
        target_text: raw.trim().to_string(),
        receiver: None,
        arity: 0,
        kind: ParsedCallKind::Macro,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-rust", "macro_invocation"),
        confidence: Confidence::MacroOpaque,
    });
    extract_body_hit(node, BodyHitKind::Macro, ctx, owner_id);
}

pub(crate) fn extract_reference(
    node: Node<'_>,
    kind: ReferenceKind,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let text = node_text(node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
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
        provenance: Provenance::new("tree-sitter-rust", format!("{} reference", node.kind())),
    });
    ctx.body_hits.push(BodyHit {
        file_id: ctx.file.id.clone(),
        owner_id,
        text,
        kind: body_kind,
        span: span_from_node(node),
    });
}
