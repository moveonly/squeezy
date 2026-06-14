use crate::languages::rust::*;
use crate::*;

/// Sentinel alias on the synthetic `ParsedImport` emitted from a `library`
/// directive. The graph resolver uses this to recover the host library name
/// for a file (mirrors Java's `__java_package__` trick).
pub(crate) const DART_LIBRARY_ALIAS: &str = "__dart_library__";

/// Sentinel alias on the synthetic `ParsedImport` emitted from a `part`
/// directive in the *host* library. The graph resolver follows this to attach
/// each part file's top-level symbols to the host's library symbol.
pub(crate) const DART_PART_ALIAS: &str = "__dart_part__";

/// Sentinel alias on the synthetic `ParsedImport` emitted from a `part of`
/// directive in a part file. The graph resolver follows this to re-parent
/// the part file's top-level symbols onto the host library.
pub(crate) const DART_PART_OF_ALIAS: &str = "__dart_part_of__";

pub(crate) fn extract_dart(file: FileRecord, source: &str, tree: &Tree) -> ParsedFile {
    let mut ctx = ExtractContext::new(file.clone(), source);
    let root = tree.root_node();
    record_parse_error_diagnostics(root, &mut ctx);

    visit_dart_node(root, &mut ctx, None, None);
    dedup_dart_facts(&mut ctx);

    let package = ctx
        .imports
        .iter()
        .find(|import| import.alias.as_deref() == Some(DART_LIBRARY_ALIAS))
        .map(|import| import.path.clone());

    ParsedFile {
        file,
        package,
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

fn visit_dart_node(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<(SymbolId, SymbolKind)>,
    owner_symbol: Option<SymbolId>,
) {
    if node.is_missing() {
        record_missing_node_diagnostic(node, ctx);
        return;
    }

    let kind = node.kind();
    match kind {
        "library_name" => extract_dart_library_directive(node, ctx),
        "library_import" => extract_dart_import_export(node, ctx, /*is_reexport=*/ false),
        "library_export" => extract_dart_import_export(node, ctx, /*is_reexport=*/ true),
        "part_directive" => extract_dart_part(node, ctx),
        "part_of_directive" => extract_dart_part_of(node, ctx),
        _ => {}
    }

    // class_member is a transparent wrapper around either a method_declaration
    // or a declaration; descend without emitting.
    if kind == "class_member" {
        visit_dart_children(node, ctx, parent_symbol, owner_symbol);
        return;
    }

    // declaration (no body) is a class member whose actual shape is in a
    // sub-node. Unwrap and visit the body-less symbol.
    if kind == "declaration" {
        let symbols = dart_symbols_from_declaration(node, ctx, parent_symbol.as_ref());
        if !symbols.is_empty() {
            for symbol in &symbols {
                extract_dart_symbol_facts(node, symbol, ctx);
                ctx.symbols.push(symbol.clone());
            }
            // Descend so any initializer expressions are scanned for
            // calls/references attributed to the parent (no body of our own).
            visit_dart_children(node, ctx, parent_symbol, owner_symbol);
            return;
        }
    }

    // method_declaration wraps method_signature + function_body.
    if kind == "method_declaration"
        && let Some(symbol) = dart_symbol_from_method_declaration(node, ctx, parent_symbol.as_ref())
    {
        extract_dart_symbol_facts(node, &symbol, ctx);
        let next_parent = Some((symbol.id.clone(), symbol.kind));
        let next_owner = if symbol.body_span.is_some() {
            Some(symbol.id.clone())
        } else {
            owner_symbol.clone()
        };
        ctx.symbols.push(symbol);
        visit_dart_children(node, ctx, next_parent, next_owner);
        return;
    }

    if let Some(symbol) = dart_top_level_symbol_from_node(node, ctx, parent_symbol.as_ref()) {
        extract_dart_symbol_facts(node, &symbol, ctx);
        let next_parent = Some((symbol.id.clone(), symbol.kind));
        let next_owner = if symbol.body_span.is_some() {
            Some(symbol.id.clone())
        } else {
            owner_symbol.clone()
        };
        // Synthesize children for the extension type representation field,
        // and emit enum constants as Const children before descending.
        match node.kind() {
            "extension_type_declaration" => {
                if let Some(field) = dart_extension_type_representation_field(node, ctx, &symbol) {
                    ctx.symbols.push(field);
                }
            }
            "enum_declaration" => {
                let constants = dart_enum_constant_symbols(node, ctx, &symbol);
                for constant in constants {
                    ctx.symbols.push(constant);
                }
            }
            _ => {}
        }
        ctx.symbols.push(symbol);
        visit_dart_children(node, ctx, next_parent, next_owner);
        return;
    }

    // Top-level variable declarations may emit multiple symbols.
    if kind == "top_level_variable_declaration" {
        let symbols = dart_top_level_variable_symbols(node, ctx, parent_symbol.as_ref());
        if !symbols.is_empty() {
            for symbol in symbols {
                ctx.symbols.push(symbol);
            }
            visit_dart_children(node, ctx, parent_symbol, owner_symbol);
            return;
        }
    }

    match kind {
        "call_expression" => {
            extract_dart_call(node, ctx, owner_symbol.clone());
            // `test('desc', () {...})` / `testWidgets(...)` / `group(...)` are
            // the standard Dart/Flutter test idioms. Lower a matching call into
            // a `Test` symbol so `kind=test` queries surface it and the calls
            // inside its closure attribute to the test (impact / affected_tests).
            if let Some(test_symbol) =
                dart_test_symbol_from_call(node, ctx, parent_symbol.as_ref())
            {
                let next_parent = Some((test_symbol.id.clone(), test_symbol.kind));
                let next_owner = Some(test_symbol.id.clone());
                ctx.symbols.push(test_symbol);
                visit_dart_children(node, ctx, next_parent, next_owner);
                return;
            }
            visit_dart_children(node, ctx, parent_symbol, owner_symbol);
            return;
        }
        "new_expression" | "constructor_invocation" | "const_object_expression" => {
            extract_dart_object_creation(node, ctx, owner_symbol.clone());
            visit_dart_children(node, ctx, parent_symbol, owner_symbol);
            return;
        }
        "type_identifier" => {
            extract_dart_reference(node, ReferenceKind::Type, ctx, owner_symbol.clone());
        }
        "qualified" => {
            extract_dart_reference(node, ReferenceKind::Path, ctx, owner_symbol.clone());
        }
        "identifier" if !dart_identifier_is_declaration_name(node) => {
            extract_dart_reference(node, ReferenceKind::Identifier, ctx, owner_symbol.clone());
        }
        "annotation" => {
            extract_dart_annotation_reference(node, ctx, owner_symbol.clone());
        }
        kind if is_dart_literal(kind) => {
            extract_body_hit(node, BodyHitKind::Literal, ctx, owner_symbol.clone());
        }
        _ => {}
    }

    visit_dart_children(node, ctx, parent_symbol, owner_symbol);
}

fn visit_dart_children(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<(SymbolId, SymbolKind)>,
    owner_symbol: Option<SymbolId>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit_dart_node(child, ctx, parent_symbol.clone(), owner_symbol.clone());
    }
}

fn dart_top_level_symbol_from_node(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> Option<ParsedSymbol> {
    let kind = match node.kind() {
        "class_declaration" => SymbolKind::Class,
        "mixin_declaration" => SymbolKind::Trait,
        "extension_declaration" => SymbolKind::Class,
        "extension_type_declaration" => SymbolKind::Class,
        "enum_declaration" => SymbolKind::Enum,
        "type_alias" => SymbolKind::TypeAlias,
        "function_declaration" => SymbolKind::Function,
        _ => return None,
    };

    let (name, name_span, anonymous) = match node.kind() {
        "extension_declaration" => match dart_node_name(node, ctx.source) {
            Some(value) => (value.0, value.1, false),
            None => {
                let span = span_from_node(node);
                let synthetic = format!("__ext_{}_{}", span.start.line, span.start.column);
                (synthetic, span, true)
            }
        },
        "extension_type_declaration" => dart_extension_type_name(node, ctx.source)?,
        "function_declaration" => {
            let signature = node.child_by_field_name("signature")?;
            let (text, span) = dart_node_name(signature, ctx.source)?;
            (text, span, false)
        }
        _ => {
            let (text, span) = dart_node_name(node, ctx.source)?;
            (text, span, false)
        }
    };
    if name.is_empty() {
        return None;
    }

    let body = dart_symbol_body(node);
    let span = span_from_node(node);
    let body_span = body.map(span_from_node);
    let signature_span = signature_span_from_nodes(node, body);
    let signature = signature_text(node, body, ctx.source);
    let parent_id = parent_symbol.map(|(id, _)| id.clone());
    let id = symbol_id(&ctx.file, parent_id.as_ref(), kind, &name, span);

    let mut attributes = Vec::new();
    dart_collect_modifier_attributes(node, ctx.source, &mut attributes);
    match node.kind() {
        "extension_declaration" => {
            attributes.push("dart:extension".to_string());
            if anonymous {
                attributes.push("dart:anonymous-extension".to_string());
            }
        }
        "extension_type_declaration" => {
            attributes.push("dart:extension-type".to_string());
        }
        "type_alias" => {
            attributes.push("dart:typedef".to_string());
        }
        _ => {}
    }
    // class / mixin / extension_type ancestor attributes
    if matches!(
        node.kind(),
        "class_declaration" | "mixin_declaration" | "extension_type_declaration"
    ) {
        for base in dart_superclass_names(node, ctx.source) {
            attributes.push(format!("base:{base}"));
        }
        for mixin in dart_mixin_names(node, ctx.source) {
            attributes.push(format!("mixin:{mixin}"));
        }
        for iface in dart_interfaces_names(node, ctx.source) {
            attributes.push(format!("iface:{iface}"));
        }
        if node.kind() == "mixin_declaration" {
            for on_name in dart_mixin_on_names(node, ctx.source) {
                attributes.push(format!("mixin-on:{on_name}"));
            }
        }
    }
    // enum_declaration also accepts `with` mixins / `implements` interfaces
    if node.kind() == "enum_declaration" {
        for mixin in dart_mixin_names(node, ctx.source) {
            attributes.push(format!("mixin:{mixin}"));
        }
        for iface in dart_interfaces_names(node, ctx.source) {
            attributes.push(format!("iface:{iface}"));
        }
    }
    if node.kind() == "function_declaration"
        && let Some(signature) = node.child_by_field_name("signature")
    {
        dart_collect_async_attribute(node, &signature, &mut attributes);
    }
    if matches!(kind, SymbolKind::Function)
        && let Some(body_node) = body
    {
        dart_collect_local_type_attributes(body_node, ctx.source, &mut attributes);
    }
    attributes.sort();
    attributes.dedup();

    let confidence = if anonymous {
        Confidence::Partial
    } else {
        Confidence::ExactSyntax
    };

    let language_identity = match node.kind() {
        "extension_declaration" => dart_extension_on_type(node, ctx.source),
        "extension_type_declaration" => dart_extension_type_representation_type(node, ctx.source),
        _ => None,
    };

    let arity = if matches!(kind, SymbolKind::Function) {
        node.child_by_field_name("signature")
            .and_then(|sig| sig.child_by_field_name("parameters"))
            .map(|params| u8::try_from(dart_param_count(params)).unwrap_or(u8::MAX))
    } else {
        None
    };

    Some(ParsedSymbol {
        id,
        file_id: ctx.file.id.clone(),
        parent_id,
        name,
        kind,
        language_identity,
        span,
        body_span,
        signature_span,
        signature,
        visibility: dart_visibility_for_name_span(name_span, ctx.source),
        docs: dart_docs_for_node(node, ctx.source),
        attributes,
        provenance: Provenance::new("tree-sitter-dart", format!("{} declaration", node.kind())),
        confidence,
        freshness: Freshness::Fresh,
        arity,
    })
}

fn dart_symbol_from_method_declaration(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> Option<ParsedSymbol> {
    let signature = node.child_by_field_name("signature")?;
    let mut cursor = signature.walk();
    let inner = signature
        .named_children(&mut cursor)
        .find(|child| dart_is_method_inner_signature(child.kind()))?;
    dart_symbol_from_signature(node, inner, ctx, parent_symbol, /*has_body=*/ true)
}

fn dart_symbols_from_declaration(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> Vec<ParsedSymbol> {
    let mut cursor = node.walk();
    let mut field_type: Option<String> = None;
    let mut symbols = Vec::new();
    for child in node.named_children(&mut cursor) {
        let child_kind = child.kind();
        if child_kind == "type" {
            field_type = node_text(child, ctx.source)
                .ok()
                .map(|text| dart_strip_type_args(text.trim()).to_string())
                .filter(|text| !text.is_empty());
            continue;
        }
        if dart_is_method_inner_signature(child_kind)
            && let Some(symbol) = dart_symbol_from_signature(
                node,
                child,
                ctx,
                parent_symbol,
                /*has_body=*/ false,
            )
        {
            symbols.push(symbol);
        }
        if matches!(
            child_kind,
            "initialized_identifier_list" | "static_final_declaration_list" | "identifier_list"
        ) {
            let mut field_cursor = child.walk();
            for declarator in child.named_children(&mut field_cursor) {
                if let Some(symbol) = dart_field_symbol_from_declarator(
                    node,
                    declarator,
                    field_type.as_deref(),
                    ctx,
                    parent_symbol,
                ) {
                    symbols.push(symbol);
                }
            }
        }
    }
    symbols
}

fn dart_symbol_from_signature(
    decl_node: Node<'_>,
    signature_node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
    has_body: bool,
) -> Option<ParsedSymbol> {
    let inner_kind = signature_node.kind();
    let mut name = dart_signature_name(signature_node, ctx.source)?;
    if name.is_empty() {
        return None;
    }

    let mut attributes = Vec::new();
    dart_collect_modifier_attributes(decl_node, ctx.source, &mut attributes);

    // Disambiguate the symbol kind. Functions at file scope become Function,
    // everything else is a Method-shaped construct.
    let parent_kind = parent_symbol.map(|(_, kind)| *kind);
    let symbol_kind = match inner_kind {
        "function_signature" => {
            if parent_kind.is_some() {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            }
        }
        "method_signature" => SymbolKind::Method,
        "getter_signature" => {
            attributes.push("dart:getter".to_string());
            // No SymbolKind::Variable; the closest Dart-getter shape is a
            // zero-arg method whose dispatch the resolver special-cases via
            // the attribute. SymbolKind::Method keeps signature search and
            // hierarchy queries working without inventing a new kind.
            SymbolKind::Method
        }
        "setter_signature" => {
            attributes.push("dart:setter".to_string());
            SymbolKind::Method
        }
        "constructor_signature" | "constant_constructor_signature" => {
            attributes.push("dart:constructor".to_string());
            SymbolKind::Method
        }
        "factory_constructor_signature" | "redirecting_factory_constructor_signature" => {
            attributes.push("dart:constructor".to_string());
            attributes.push("dart:factory".to_string());
            SymbolKind::Method
        }
        "operator_signature" => {
            attributes.push("dart:operator".to_string());
            SymbolKind::Method
        }
        _ => return None,
    };

    // For named constructors keep the dotted name (`Foo.named`).
    if matches!(
        inner_kind,
        "constructor_signature"
            | "constant_constructor_signature"
            | "factory_constructor_signature"
            | "redirecting_factory_constructor_signature"
    ) {
        let dotted = dart_constructor_dotted_name(signature_node, ctx.source);
        if let Some(dotted) = dotted {
            name = dotted;
        }
    }

    if has_body {
        dart_collect_async_attribute(decl_node, &signature_node, &mut attributes);
    }

    let body = if has_body {
        decl_node.child_by_field_name("body")
    } else {
        None
    };
    if let Some(body_node) = body {
        dart_collect_local_type_attributes(body_node, ctx.source, &mut attributes);
    }
    attributes.sort();
    attributes.dedup();

    let span = span_from_node(decl_node);
    let body_span = body.map(span_from_node);
    let signature_span = signature_span_from_nodes(decl_node, body);
    let signature_text_value = signature_text(decl_node, body, ctx.source);
    let parent_id = parent_symbol.map(|(id, _)| id.clone());
    let id = symbol_id(&ctx.file, parent_id.as_ref(), symbol_kind, &name, span);

    let arity = dart_signature_arity(signature_node, symbol_kind);

    // Inherit language_identity from enclosing extension/extension-type when
    // applicable; the parent symbol carries it, but synthesized member
    // dispatch reads it from the method itself. We let the graph resolver
    // walk up via parent_id, so we leave None here.
    Some(ParsedSymbol {
        id,
        file_id: ctx.file.id.clone(),
        parent_id,
        name,
        kind: symbol_kind,
        language_identity: None,
        span,
        body_span,
        signature_span,
        signature: signature_text_value,
        visibility: None,
        docs: dart_docs_for_node(decl_node, ctx.source),
        attributes,
        provenance: Provenance::new("tree-sitter-dart", format!("{inner_kind} declaration")),
        confidence: Confidence::ExactSyntax,
        freshness: Freshness::Fresh,
        arity,
    })
}

fn dart_field_symbol_from_declarator(
    decl_node: Node<'_>,
    declarator: Node<'_>,
    field_type: Option<&str>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> Option<ParsedSymbol> {
    let name_node = match declarator.kind() {
        "initialized_identifier" | "static_final_declaration" => {
            let mut walker = declarator.walk();
            declarator
                .named_children(&mut walker)
                .find(|child| child.kind() == "identifier")?
        }
        "identifier" => declarator,
        _ => return None,
    };
    let name = node_text(name_node, ctx.source).ok()?.trim().to_string();
    if name.is_empty() {
        return None;
    }
    let span = span_from_node(declarator);
    let parent_id = parent_symbol.map(|(id, _)| id.clone());
    let id = symbol_id(
        &ctx.file,
        parent_id.as_ref(),
        SymbolKind::Field,
        &name,
        span,
    );
    let mut attributes = Vec::new();
    dart_collect_modifier_attributes(decl_node, ctx.source, &mut attributes);
    if let Some(field_type) = field_type {
        attributes.push(format!("type:{field_type}"));
    }
    attributes.sort();
    attributes.dedup();
    Some(ParsedSymbol {
        id,
        file_id: ctx.file.id.clone(),
        parent_id,
        name,
        kind: SymbolKind::Field,
        language_identity: None,
        span,
        body_span: None,
        signature_span: None,
        signature: signature_text(decl_node, None, ctx.source),
        visibility: dart_visibility_for_name_span(span_from_node(name_node), ctx.source),
        docs: dart_docs_for_node(decl_node, ctx.source),
        attributes,
        provenance: Provenance::new("tree-sitter-dart", "field declaration"),
        confidence: Confidence::ExactSyntax,
        freshness: Freshness::Fresh,
        arity: None,
    })
}

fn dart_top_level_variable_symbols(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> Vec<ParsedSymbol> {
    let mut cursor = node.walk();
    let mut field_type: Option<String> = None;
    let mut symbols = Vec::new();
    let raw_modifier = node
        .child_by_field_name("modifier")
        .and_then(|m| node_text(m, ctx.source).ok())
        .map(|text| text.trim().to_string());
    let is_const = raw_modifier.as_deref() == Some("const");
    let is_final = raw_modifier.as_deref() == Some("final");
    for child in node.named_children(&mut cursor) {
        let child_kind = child.kind();
        if child_kind == "type" {
            field_type = node_text(child, ctx.source)
                .ok()
                .map(|text| dart_strip_type_args(text.trim()).to_string())
                .filter(|text| !text.is_empty());
            continue;
        }
        if matches!(
            child_kind,
            "initialized_identifier_list" | "static_final_declaration_list" | "identifier_list"
        ) {
            let mut field_cursor = child.walk();
            for declarator in child.named_children(&mut field_cursor) {
                let Some(name_node) = (match declarator.kind() {
                    "initialized_identifier" | "static_final_declaration" => {
                        let mut walker = declarator.walk();
                        declarator
                            .named_children(&mut walker)
                            .find(|c| c.kind() == "identifier")
                    }
                    "identifier" => Some(declarator),
                    _ => None,
                }) else {
                    continue;
                };
                let name = match node_text(name_node, ctx.source) {
                    Ok(text) if !text.trim().is_empty() => text.trim().to_string(),
                    _ => continue,
                };
                let span = span_from_node(declarator);
                let kind = if is_const || is_final {
                    if dart_rhs_is_call_expression(declarator) {
                        // const/final with call-shaped rhs still classifies as
                        // Const declaration but with Partial confidence (spec
                        // §5).
                        SymbolKind::Const
                    } else {
                        SymbolKind::Const
                    }
                } else {
                    // No `Variable` symbol kind in squeezy-core; the closest
                    // analogue for a non-const top-level binding is `Const`
                    // (mirrors Ruby's top-level Const heuristic; the
                    // graph resolver does not currently distinguish mutable
                    // top-levels from immutable ones).
                    SymbolKind::Const
                };
                let parent_id = parent_symbol.map(|(id, _)| id.clone());
                let id = symbol_id(&ctx.file, parent_id.as_ref(), kind, &name, span);
                let mut attributes = Vec::new();
                if let Some(ref field_type) = field_type {
                    attributes.push(format!("type:{field_type}"));
                }
                if let Some(modifier) = &raw_modifier {
                    attributes.push(format!("dart:{modifier}"));
                }
                attributes.sort();
                attributes.dedup();
                let confidence = if dart_rhs_is_call_expression(declarator) {
                    Confidence::Partial
                } else {
                    Confidence::ExactSyntax
                };
                symbols.push(ParsedSymbol {
                    id,
                    file_id: ctx.file.id.clone(),
                    parent_id,
                    name,
                    kind,
                    language_identity: None,
                    span,
                    body_span: None,
                    signature_span: None,
                    signature: signature_text(node, None, ctx.source),
                    visibility: dart_visibility_for_name_span(
                        span_from_node(name_node),
                        ctx.source,
                    ),
                    docs: dart_docs_for_node(node, ctx.source),
                    attributes,
                    provenance: Provenance::new("tree-sitter-dart", "top_level_variable"),
                    confidence,
                    freshness: Freshness::Fresh,
                    arity: None,
                });
            }
        }
    }
    symbols
}

fn dart_extension_type_representation_field(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent: &ParsedSymbol,
) -> Option<ParsedSymbol> {
    let representation = node.child_by_field_name("representation")?;
    let mut name = None;
    let mut field_type = None;
    let mut cursor = representation.walk();
    for child in representation.named_children(&mut cursor) {
        match child.kind() {
            "type" => {
                field_type = node_text(child, ctx.source)
                    .ok()
                    .map(|t| t.trim().to_string());
            }
            "identifier" => {
                name = node_text(child, ctx.source)
                    .ok()
                    .map(|t| t.trim().to_string());
            }
            _ => {}
        }
    }
    let name = name?;
    if name.is_empty() {
        return None;
    }
    let span = span_from_node(representation);
    let id = symbol_id(&ctx.file, Some(&parent.id), SymbolKind::Field, &name, span);
    let mut attributes = vec!["dart:representation".to_string()];
    if let Some(field_type) = field_type {
        attributes.push(format!("type:{field_type}"));
    }
    Some(ParsedSymbol {
        id,
        file_id: ctx.file.id.clone(),
        parent_id: Some(parent.id.clone()),
        name,
        kind: SymbolKind::Field,
        language_identity: None,
        span,
        body_span: None,
        signature_span: None,
        signature: signature_text(representation, None, ctx.source),
        visibility: None,
        docs: Vec::new(),
        attributes,
        provenance: Provenance::new("tree-sitter-dart", "extension_type representation"),
        confidence: Confidence::ExactSyntax,
        freshness: Freshness::Fresh,
        arity: None,
    })
}

fn dart_enum_constant_symbols(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent: &ParsedSymbol,
) -> Vec<ParsedSymbol> {
    let Some(body) = node.child_by_field_name("body") else {
        return Vec::new();
    };
    let mut cursor = body.walk();
    let mut symbols = Vec::new();
    for child in body.named_children(&mut cursor) {
        if child.kind() != "enum_constant" {
            continue;
        }
        let mut inner = child.walk();
        let name_node = child
            .named_children(&mut inner)
            .find(|grand| grand.kind() == "identifier");
        let Some(name_node) = name_node else { continue };
        let Ok(name_text) = node_text(name_node, ctx.source) else {
            continue;
        };
        let name = name_text.trim().to_string();
        if name.is_empty() {
            continue;
        }
        let span = span_from_node(child);
        let id = symbol_id(&ctx.file, Some(&parent.id), SymbolKind::Const, &name, span);
        symbols.push(ParsedSymbol {
            id,
            file_id: ctx.file.id.clone(),
            parent_id: Some(parent.id.clone()),
            name,
            kind: SymbolKind::Const,
            language_identity: None,
            span,
            body_span: None,
            signature_span: None,
            signature: signature_text(child, None, ctx.source),
            visibility: None,
            docs: Vec::new(),
            attributes: vec!["dart:enum-constant".to_string()],
            provenance: Provenance::new("tree-sitter-dart", "enum_constant"),
            confidence: Confidence::ExactSyntax,
            freshness: Freshness::Fresh,
            arity: None,
        });
    }
    symbols
}

fn dart_signature_name(node: Node<'_>, source: &str) -> Option<String> {
    match node.kind() {
        "function_signature" | "method_signature" | "getter_signature" | "setter_signature" => node
            .child_by_field_name("name")
            .and_then(|name| node_text(name, source).ok())
            .map(|text| text.trim().to_string()),
        "constructor_signature"
        | "constant_constructor_signature"
        | "factory_constructor_signature"
        | "redirecting_factory_constructor_signature" => {
            // First identifier child is the class name; if there's a dot
            // followed by another identifier, that's the named constructor.
            let mut cursor = node.walk();
            let mut idents = node
                .named_children(&mut cursor)
                .filter(|child| child.kind() == "identifier");
            let first = idents.next()?;
            let first_text = node_text(first, source).ok()?.trim().to_string();
            Some(first_text)
        }
        "operator_signature" => {
            // Use full source slice as the symbol name token after `operator`.
            let raw = node_text(node, source).ok()?.trim();
            raw.split_once("operator")
                .map(|(_, rest)| rest.split('(').next().unwrap_or("").trim().to_string())
                .filter(|name| !name.is_empty())
        }
        _ => None,
    }
}

fn dart_constructor_dotted_name(node: Node<'_>, source: &str) -> Option<String> {
    let mut cursor = node.walk();
    let idents: Vec<_> = node
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "identifier")
        .filter_map(|child| node_text(child, source).ok().map(|t| t.trim().to_string()))
        .filter(|text| !text.is_empty())
        .collect();
    if idents.is_empty() {
        return None;
    }
    if idents.len() == 1 {
        return Some(idents[0].clone());
    }
    // First is class name, subsequent identifiers are the named constructor.
    Some(format!("{}.{}", idents[0], idents[1..].join(".")))
}

fn dart_signature_arity(node: Node<'_>, kind: SymbolKind) -> Option<u8> {
    if !matches!(kind, SymbolKind::Function | SymbolKind::Method) {
        return None;
    }
    let mut params_node = node.child_by_field_name("parameters");
    if params_node.is_none() {
        // operator_signature / constant_constructor_signature stash the
        // formal_parameter_list as a named child rather than a labelled field.
        let mut cursor = node.walk();
        params_node = node
            .named_children(&mut cursor)
            .find(|child| child.kind() == "formal_parameter_list");
    }
    let params = params_node?;
    Some(u8::try_from(dart_param_count(params)).unwrap_or(u8::MAX))
}

fn dart_param_count(params: Node<'_>) -> usize {
    if params.kind() != "formal_parameter_list" {
        return named_child_count(params);
    }
    let mut count = 0;
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        match child.kind() {
            "formal_parameter" => count += 1,
            "optional_formal_parameters" | "named_formal_parameters" => {
                let mut inner = child.walk();
                count += child
                    .named_children(&mut inner)
                    .filter(|inner_child| inner_child.kind() == "formal_parameter")
                    .count();
            }
            _ => {}
        }
    }
    count
}

fn dart_symbol_body(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("body")
}

fn dart_node_name(node: Node<'_>, source: &str) -> Option<(String, SourceSpan)> {
    let name = node.child_by_field_name("name")?;
    let raw = node_text(name, source).ok()?.trim().to_string();
    if raw.is_empty() {
        return None;
    }
    Some((raw, span_from_node(name)))
}

fn dart_extension_type_name(node: Node<'_>, source: &str) -> Option<(String, SourceSpan, bool)> {
    let name = node.child_by_field_name("name")?;
    let raw = if name.kind() == "extension_type_name" {
        // extension_type_name -> identifier
        let mut cursor = name.walk();
        let ident = name
            .named_children(&mut cursor)
            .find(|child| child.kind() == "identifier")?;
        node_text(ident, source).ok()?.trim().to_string()
    } else {
        node_text(name, source).ok()?.trim().to_string()
    };
    if raw.is_empty() {
        return None;
    }
    Some((raw, span_from_node(name), false))
}

fn dart_extension_on_type(node: Node<'_>, source: &str) -> Option<String> {
    // `extension X on T { ... }` parses with `class` field holding `T`.
    let on_type = node.child_by_field_name("class")?;
    let raw = node_text(on_type, source).ok()?.trim().to_string();
    if raw.is_empty() {
        return None;
    }
    Some(dart_strip_type_args(&raw).to_string())
}

fn dart_extension_type_representation_type(node: Node<'_>, source: &str) -> Option<String> {
    let rep = node.child_by_field_name("representation")?;
    let mut cursor = rep.walk();
    let type_node = rep
        .named_children(&mut cursor)
        .find(|child| child.kind() == "type")?;
    let raw = node_text(type_node, source).ok()?.trim().to_string();
    if raw.is_empty() {
        return None;
    }
    Some(dart_strip_type_args(&raw).to_string())
}

/// Walk a Dart function/method body and emit `dart-local:<name>:<type>`
/// attributes for typed-or-inferable local variables. The graph resolver
/// reads these to dispatch `receiver.method(...)` calls when the receiver
/// is a body-local whose static type is known (typed declaration, or a
/// `final/var name = Constructor()` shape).
fn dart_collect_local_type_attributes(body: Node<'_>, source: &str, attributes: &mut Vec<String>) {
    let mut stack = vec![body];
    while let Some(node) = stack.pop() {
        if node.kind() == "local_variable_declaration" {
            dart_collect_locals_from_declaration(node, source, attributes);
            // Don't descend: variable RHS expressions can't introduce new
            // locals on the enclosing function in a way that affects
            // dispatch.
            continue;
        }
        // Explicitly-typed pattern bindings introduce locals just like a plain
        // declaration: `if (obj case Foo x)` / `switch (v) { case Foo x: }` /
        // `final (Foo a, Bar b) = pair;`. Record them so `x.method()` dispatch
        // can resolve the receiver's type. We keep descending — a pattern can
        // nest more patterns (record/list/object) that bind further locals.
        match node.kind() {
            "variable_pattern" => dart_collect_locals_from_variable_pattern(node, source, attributes),
            "cast_pattern" => dart_collect_locals_from_cast_pattern(node, source, attributes),
            _ => {}
        }
        // Skip descending into nested closures / nested function
        // declarations so their locals don't leak onto the enclosing
        // symbol's attribute set.
        if matches!(
            node.kind(),
            "function_expression"
                | "function_declaration"
                | "method_declaration"
                | "local_function_declaration"
        ) && node.id() != body.id()
        {
            continue;
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
}

/// `variable_pattern` = `<final|var|[final] Type> name`. Only the explicitly
/// typed form (`Foo x`, `final Foo x`) carries a `type` child and is resolvable;
/// `var x` / `final x` have no static type to attach.
fn dart_collect_locals_from_variable_pattern(
    node: Node<'_>,
    source: &str,
    attributes: &mut Vec<String>,
) {
    let Some(name) = node
        .child_by_field_name("name")
        .and_then(|name| node_text(name, source).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
    else {
        return;
    };
    let mut cursor = node.walk();
    let Some(ty) = node
        .named_children(&mut cursor)
        .find(|child| child.kind() == "type")
        .and_then(|type_node| dart_pattern_type_name(type_node, source))
    else {
        return;
    };
    attributes.push(format!("dart-local:{name}:{ty}"));
}

/// `cast_pattern` = `<primary_pattern> as Type`. The cast `type` is the static
/// type of the bound variable, so `var x as Foo` makes `x` a `Foo`.
fn dart_collect_locals_from_cast_pattern(
    node: Node<'_>,
    source: &str,
    attributes: &mut Vec<String>,
) {
    let Some(ty) = node
        .child_by_field_name("type")
        .and_then(|type_node| dart_pattern_type_name(type_node, source))
    else {
        return;
    };
    // The bound name is the first sub-pattern: a `variable_pattern` (`var x`)
    // or a bare `identifier`. Nested cast targets are emitted by the walker.
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let name = match child.kind() {
            "variable_pattern" => child
                .child_by_field_name("name")
                .and_then(|name| node_text(name, source).ok()),
            "identifier" => node_text(child, source).ok(),
            _ => None,
        };
        if let Some(name) = name
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty())
        {
            attributes.push(format!("dart-local:{name}:{ty}"));
            return;
        }
    }
}

/// Normalise a pattern `type` node to a bare leaf type name, dropping generic
/// arguments and a trailing nullable `?` (mirrors local-declaration handling).
fn dart_pattern_type_name(type_node: Node<'_>, source: &str) -> Option<String> {
    node_text(type_node, source)
        .ok()
        .map(|text| dart_strip_type_args(text.trim()).to_string())
        .map(|text| text.trim_end_matches('?').trim().to_string())
        .filter(|text| !text.is_empty() && text != "?")
}

fn dart_collect_locals_from_declaration(
    decl: Node<'_>,
    source: &str,
    attributes: &mut Vec<String>,
) {
    let mut decl_cursor = decl.walk();
    let Some(init) = decl
        .named_children(&mut decl_cursor)
        .find(|child| child.kind() == "initialized_variable_definition")
    else {
        return;
    };
    let mut explicit_type: Option<String> = None;
    let mut current_name: Option<String> = None;
    let mut current_has_rhs = false;
    let mut cursor = init.walk();
    let children: Vec<Node<'_>> = init.named_children(&mut cursor).collect();
    for child in children {
        match child.kind() {
            "type" => {
                explicit_type = node_text(child, source)
                    .ok()
                    .map(|text| dart_strip_type_args(text.trim()).to_string())
                    .map(|text| text.trim_end_matches('?').to_string())
                    .filter(|text| !text.is_empty() && text != "?");
            }
            "identifier" => {
                if let Some(prev_name) = current_name.take()
                    && !current_has_rhs
                    && let Some(ty) = explicit_type.as_deref()
                {
                    attributes.push(format!("dart-local:{prev_name}:{ty}"));
                }
                current_name = node_text(child, source)
                    .ok()
                    .map(|text| text.trim().to_string())
                    .filter(|text| !text.is_empty());
                current_has_rhs = false;
            }
            "initialized_identifier" => {
                if let Some(prev_name) = current_name.take()
                    && !current_has_rhs
                    && let Some(ty) = explicit_type.as_deref()
                {
                    attributes.push(format!("dart-local:{prev_name}:{ty}"));
                }
                let mut inner_cursor = child.walk();
                let mut ident: Option<Node<'_>> = None;
                let mut rhs: Option<Node<'_>> = None;
                for ic in child.named_children(&mut inner_cursor) {
                    if ic.kind() == "identifier" && ident.is_none() {
                        ident = Some(ic);
                    } else if !matches!(ic.kind(), "metadata" | "comment" | "block_comment") {
                        rhs = Some(ic);
                    }
                }
                let name = ident
                    .and_then(|n| node_text(n, source).ok())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                let inferred = rhs.and_then(|n| dart_infer_expression_type(n, source));
                let ty = explicit_type.clone().or(inferred);
                if let (Some(name), Some(ty)) = (name, ty) {
                    attributes.push(format!("dart-local:{name}:{ty}"));
                }
                current_name = None;
                current_has_rhs = false;
            }
            other if !matches!(other, "metadata" | "comment" | "block_comment") => {
                if let Some(name) = current_name.take() {
                    current_has_rhs = true;
                    let ty = explicit_type
                        .clone()
                        .or_else(|| dart_infer_expression_type(child, source));
                    if let Some(ty) = ty {
                        attributes.push(format!("dart-local:{name}:{ty}"));
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(name) = current_name
        && !current_has_rhs
        && let Some(ty) = explicit_type
    {
        attributes.push(format!("dart-local:{name}:{ty}"));
    }
}

/// Best-effort static type for a simple Dart RHS expression on a
/// local-variable declaration. Recognises constructor calls
/// (`Foo()`, `prefix.Foo()`) and literal forms.
fn dart_infer_expression_type(node: Node<'_>, source: &str) -> Option<String> {
    match node.kind() {
        "call_expression" => {
            let func = node.child_by_field_name("function")?;
            match func.kind() {
                "identifier" => node_text(func, source)
                    .ok()
                    .map(|text| text.trim().to_string())
                    .filter(|text| dart_looks_like_type_name(text)),
                "member_expression" => {
                    let property = func.child_by_field_name("property")?;
                    node_text(property, source)
                        .ok()
                        .map(|text| text.trim().to_string())
                        .filter(|text| dart_looks_like_type_name(text))
                }
                _ => None,
            }
        }
        "string_literal" => Some("String".to_string()),
        "decimal_integer_literal" | "hex_integer_literal" => Some("int".to_string()),
        "decimal_floating_point_literal" => Some("double".to_string()),
        "true" | "false" => Some("bool".to_string()),
        "list_literal" => Some("List".to_string()),
        "set_or_map_literal" => Some("Map".to_string()),
        _ => None,
    }
}

fn dart_looks_like_type_name(text: &str) -> bool {
    let bare = dart_strip_type_args(text);
    let first = bare.chars().next();
    first.map(|c| c.is_ascii_uppercase()).unwrap_or(false)
        && bare.chars().all(|c| c.is_alphanumeric() || c == '_')
}

fn dart_superclass_names(node: Node<'_>, source: &str) -> Vec<String> {
    let Some(superclass) = node.child_by_field_name("superclass") else {
        return Vec::new();
    };
    let mut names = Vec::new();
    let mut cursor = superclass.walk();
    for child in superclass.named_children(&mut cursor) {
        if child.kind() == "type"
            && let Ok(text) = node_text(child, source)
            && let Some(name) = dart_type_name(text)
        {
            names.push(name);
        }
    }
    names.sort();
    names.dedup();
    names
}

fn dart_mixin_names(node: Node<'_>, source: &str) -> Vec<String> {
    // For class_declaration / extension_type_declaration, mixins live inside
    // `superclass` field as a `mixins` child. For mixin_declaration there is
    // no `with`, so this returns empty. For enum_declaration mixins are a
    // direct child of kind `mixins`.
    let mut names = Vec::new();
    if let Some(superclass) = node.child_by_field_name("superclass") {
        let mut cursor = superclass.walk();
        for child in superclass.named_children(&mut cursor) {
            if child.kind() == "mixins" {
                let mut inner = child.walk();
                for type_node in child.named_children(&mut inner) {
                    if type_node.kind() == "type"
                        && let Ok(text) = node_text(type_node, source)
                        && let Some(name) = dart_type_name(text)
                    {
                        names.push(name);
                    }
                }
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "mixins" {
            let mut inner = child.walk();
            for type_node in child.named_children(&mut inner) {
                if type_node.kind() == "type"
                    && let Ok(text) = node_text(type_node, source)
                    && let Some(name) = dart_type_name(text)
                {
                    names.push(name);
                }
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

fn dart_interfaces_names(node: Node<'_>, source: &str) -> Vec<String> {
    let Some(interfaces) = node.child_by_field_name("interfaces") else {
        return Vec::new();
    };
    let mut names = Vec::new();
    let mut cursor = interfaces.walk();
    for child in interfaces.named_children(&mut cursor) {
        if child.kind() == "type"
            && let Ok(text) = node_text(child, source)
            && let Some(name) = dart_type_name(text)
        {
            names.push(name);
        }
    }
    names.sort();
    names.dedup();
    names
}

fn dart_mixin_on_names(node: Node<'_>, source: &str) -> Vec<String> {
    // `mixin M on Foo, Bar {}` produces a direct `type` child after the name
    // (no field). Walk named children and collect every `type` node that's
    // not inside `body`/`interfaces`/`type_parameters`.
    let mut names = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "type"
            && let Ok(text) = node_text(child, source)
            && let Some(name) = dart_type_name(text)
        {
            names.push(name);
        }
    }
    names.sort();
    names.dedup();
    names
}

fn dart_type_name(text: &str) -> Option<String> {
    let trimmed = dart_strip_type_args(text.trim());
    if trimmed.is_empty() {
        return None;
    }
    let mut last = trimmed
        .rsplit('.')
        .next()
        .unwrap_or(trimmed)
        .trim()
        .trim_end_matches('?')
        .to_string();
    if last.is_empty() || is_dart_keyword(&last) {
        return None;
    }
    // Drop any leading prefix like `<` left over from generic expressions.
    while last.starts_with(|c: char| !c.is_alphanumeric() && c != '_') {
        last.remove(0);
        if last.is_empty() {
            return None;
        }
    }
    Some(last)
}

fn dart_strip_type_args(text: &str) -> &str {
    text.split('<').next().unwrap_or(text).trim()
}

fn dart_visibility_for_name_span(span: SourceSpan, source: &str) -> Option<String> {
    let bytes = source.as_bytes();
    let start = span.start_byte as usize;
    if start >= bytes.len() {
        return None;
    }
    if bytes[start] == b'_' {
        Some("private".to_string())
    } else {
        Some("public".to_string())
    }
}

fn dart_collect_modifier_attributes(node: Node<'_>, source: &str, attributes: &mut Vec<String>) {
    // tree-sitter-dart 0.2 represents `static`, `abstract`, `external`, `sealed`,
    // `final`, `late`, `covariant`, `base` as anonymous keyword tokens that
    // appear as unnamed children of the declaration. Walk all *non-named*
    // children to capture the modifiers that the grammar exposes as bare
    // keyword tokens.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.is_named() {
            continue;
        }
        let Ok(text) = node_text(child, source) else {
            continue;
        };
        let trimmed = text.trim();
        match trimmed {
            "static" => attributes.push("dart:static".to_string()),
            "abstract" => attributes.push("dart:abstract".to_string()),
            "external" => attributes.push("dart:external".to_string()),
            "sealed" => attributes.push("dart:sealed".to_string()),
            "final" => attributes.push("dart:final".to_string()),
            "late" => attributes.push("dart:late".to_string()),
            "covariant" => attributes.push("dart:covariant".to_string()),
            "base" => attributes.push("dart:base".to_string()),
            "interface" => attributes.push("dart:interface".to_string()),
            "mixin" if node.kind() == "class_declaration" => {
                attributes.push("dart:mixin-class".to_string());
            }
            "augment" => attributes.push("dart:augment".to_string()),
            _ => {}
        }
    }
}

fn dart_collect_async_attribute(
    decl_node: Node<'_>,
    _signature_node: &Node<'_>,
    attributes: &mut Vec<String>,
) {
    let Some(body) = decl_node.child_by_field_name("body") else {
        return;
    };
    // `function_body` shape: `async {...}` / `async* {...}` / `sync* {...}` /
    // `=> ...` / plain block. The async/sync markers appear as anonymous
    // children of the body.
    let mut cursor = body.walk();
    let mut saw_async = false;
    let mut saw_sync = false;
    let mut saw_star = false;
    for child in body.children(&mut cursor) {
        if child.is_named() {
            continue;
        }
        match child.kind() {
            "async" => saw_async = true,
            "sync" => saw_sync = true,
            "*" => saw_star = true,
            _ => {}
        }
    }
    match (saw_async, saw_sync, saw_star) {
        (true, _, true) => attributes.push("dart:async-star".to_string()),
        (true, _, false) => attributes.push("dart:async".to_string()),
        (false, true, true) => attributes.push("dart:sync-star".to_string()),
        _ => {}
    }
}

fn dart_docs_for_node(node: Node<'_>, source: &str) -> Vec<String> {
    let mut docs = Vec::new();
    let Some(mut previous) = node.prev_named_sibling() else {
        return docs;
    };
    while matches!(
        previous.kind(),
        "comment" | "block_comment" | "documentation_block_comment" | "line_comment"
    ) {
        if let Ok(text) = node_text(previous, source) {
            let trimmed = text
                .trim()
                .trim_start_matches("///")
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
        let Some(prev) = previous.prev_named_sibling() else {
            break;
        };
        previous = prev;
    }
    docs.reverse();
    docs
}

fn dart_is_method_inner_signature(kind: &str) -> bool {
    matches!(
        kind,
        "function_signature"
            | "method_signature"
            | "getter_signature"
            | "setter_signature"
            | "constructor_signature"
            | "constant_constructor_signature"
            | "factory_constructor_signature"
            | "redirecting_factory_constructor_signature"
            | "operator_signature"
    )
}

fn extract_dart_symbol_facts(node: Node<'_>, symbol: &ParsedSymbol, ctx: &mut ExtractContext<'_>) {
    if matches!(
        symbol.kind,
        SymbolKind::Class | SymbolKind::Trait | SymbolKind::Enum
    ) {
        for type_name in dart_superclass_names(node, ctx.source) {
            ctx.references.push(ParsedReference {
                file_id: ctx.file.id.clone(),
                owner_id: Some(symbol.id.clone()),
                text: type_name.clone(),
                kind: ReferenceKind::Type,
                span: symbol.span,
                provenance: Provenance::new("tree-sitter-dart", "superclass reference"),
            });
            ctx.body_hits.push(BodyHit {
                file_id: ctx.file.id.clone(),
                owner_id: Some(symbol.id.clone()),
                text: type_name,
                kind: BodyHitKind::Type,
                span: symbol.span,
            });
        }
        for mixin in dart_mixin_names(node, ctx.source) {
            ctx.references.push(ParsedReference {
                file_id: ctx.file.id.clone(),
                owner_id: Some(symbol.id.clone()),
                text: mixin,
                kind: ReferenceKind::Type,
                span: symbol.span,
                provenance: Provenance::new("tree-sitter-dart", "mixin reference"),
            });
        }
        for iface in dart_interfaces_names(node, ctx.source) {
            ctx.references.push(ParsedReference {
                file_id: ctx.file.id.clone(),
                owner_id: Some(symbol.id.clone()),
                text: iface,
                kind: ReferenceKind::Type,
                span: symbol.span,
                provenance: Provenance::new("tree-sitter-dart", "interface reference"),
            });
        }
    }
    if symbol.kind == SymbolKind::Trait && node.kind() == "mixin_declaration" {
        for on_name in dart_mixin_on_names(node, ctx.source) {
            ctx.references.push(ParsedReference {
                file_id: ctx.file.id.clone(),
                owner_id: Some(symbol.id.clone()),
                text: on_name,
                kind: ReferenceKind::Type,
                span: symbol.span,
                provenance: Provenance::new("tree-sitter-dart", "mixin on reference"),
            });
        }
    }
}

fn extract_dart_library_directive(node: Node<'_>, ctx: &mut ExtractContext<'_>) {
    let mut cursor = node.walk();
    let dotted = node
        .named_children(&mut cursor)
        .find(|child| child.kind() == "dotted_identifier_list");
    let path = match dotted {
        Some(node) => node_text(node, ctx.source)
            .map(|text| text.trim().to_string())
            .unwrap_or_default(),
        None => {
            // `library 'pkg/lib.dart';` form: the URI is the only child.
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .find(|child| child.kind() == "uri")
                .and_then(|uri| dart_uri_string(uri, ctx.source))
                .unwrap_or_default()
        }
    };
    if path.is_empty() {
        return;
    }
    ctx.imports.push(ParsedImport {
        file_id: ctx.file.id.clone(),
        owner_id: None,
        path,
        alias: Some(DART_LIBRARY_ALIAS.to_string()),
        is_glob: false,
        is_reexport: true,
        is_static: false,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-dart", "library directive"),
        kind: ImportKind::Unspecified,
        imported_name: None,
        is_global: false,
    });
}

fn extract_dart_import_export(node: Node<'_>, ctx: &mut ExtractContext<'_>, is_reexport: bool) {
    // The library_import / library_export wrapper contains an
    // import_specification (or configurable_uri for exports). The import
    // specification holds the URI, optional `as` alias, and combinator(s)
    // (`show`/`hide`).
    let (uri_node, alias, combinators, alt_uris, is_deferred) = if node.kind() == "library_import" {
        let mut cursor = node.walk();
        let spec = node
            .named_children(&mut cursor)
            .find(|child| child.kind() == "import_specification");
        let Some(spec) = spec else { return };
        let uri = spec.child_by_field_name("uri");
        let alias = spec
            .child_by_field_name("alias")
            .and_then(|alias| node_text(alias, ctx.source).ok())
            .map(|text| text.trim().to_string());
        // `import '...' deferred as p;` carries a `deferred` keyword token
        // (anonymous in the grammar) before the `as` alias. Scan all children,
        // not just named ones, so we can tell a lazy-loaded library boundary
        // apart from an eager prefixed import.
        let is_deferred = {
            let mut all = spec.walk();
            spec.children(&mut all).any(|child| child.kind() == "deferred")
        };
        let mut combinators = Vec::new();
        let mut alt_uris = Vec::new();
        let mut spec_cursor = spec.walk();
        for child in spec.named_children(&mut spec_cursor) {
            if child.kind() == "combinator" {
                combinators.push(dart_combinator_clause(child, ctx.source));
            }
        }
        if let Some(uri_node) = uri
            && uri_node.kind() == "configurable_uri"
        {
            let mut cursor = uri_node.walk();
            for child in uri_node.named_children(&mut cursor) {
                if child.kind() == "configuration_uri"
                    && let Some(uri_string) = dart_configuration_uri_target(child, ctx.source)
                {
                    alt_uris.push(uri_string);
                }
            }
        }
        (uri, alias, combinators, alt_uris, is_deferred)
    } else {
        // library_export: configurable_uri is a direct child, combinator(s) too.
        let uri = node.child_by_field_name("uri").or_else(|| {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .find(|child| child.kind() == "configurable_uri" || child.kind() == "uri")
        });
        let mut combinators = Vec::new();
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "combinator" {
                combinators.push(dart_combinator_clause(child, ctx.source));
            }
        }
        (uri, None, combinators, Vec::new(), false)
    };

    let Some(uri_node) = uri_node else { return };
    let Some(path) = dart_uri_text(uri_node, ctx.source) else {
        return;
    };
    if path.is_empty() {
        return;
    }

    let mut show_names = Vec::new();
    let mut hide_names = Vec::new();
    for (kw, names) in combinators {
        match kw.as_str() {
            "show" => show_names.extend(names),
            "hide" => hide_names.extend(names),
            _ => {}
        }
    }

    let alias_clone = alias.clone();
    let prefix_kind = if alias.is_some() {
        ImportKind::Namespace
    } else {
        ImportKind::Named
    };
    ctx.imports.push(ParsedImport {
        file_id: ctx.file.id.clone(),
        owner_id: None,
        path: path.clone(),
        alias: alias_clone,
        is_glob: alias.is_some(),
        is_reexport,
        is_static: false,
        span: span_from_node(node),
        provenance: Provenance::new(
            "tree-sitter-dart",
            if is_reexport {
                "export directive"
            } else if is_deferred {
                "deferred import directive"
            } else {
                "import directive"
            },
        ),
        kind: prefix_kind,
        imported_name: None,
        is_global: false,
    });

    for name in show_names {
        if hide_names.contains(&name) {
            continue;
        }
        ctx.imports.push(ParsedImport {
            file_id: ctx.file.id.clone(),
            owner_id: None,
            path: format!("{path}.{name}"),
            alias: None,
            is_glob: false,
            is_reexport,
            is_static: false,
            span: span_from_node(node),
            provenance: Provenance::new("tree-sitter-dart", "import show combinator"),
            kind: ImportKind::Named,
            imported_name: Some(name),
            is_global: false,
        });
    }

    for alt in alt_uris {
        ctx.imports.push(ParsedImport {
            file_id: ctx.file.id.clone(),
            owner_id: None,
            path: alt,
            alias: None,
            is_glob: false,
            is_reexport,
            is_static: false,
            span: span_from_node(node),
            provenance: Provenance::new("tree-sitter-dart", "import conditional alternate"),
            kind: ImportKind::Named,
            imported_name: None,
            is_global: false,
        });
    }
}

fn extract_dart_part(node: Node<'_>, ctx: &mut ExtractContext<'_>) {
    let Some(uri_node) = node.child_by_field_name("uri") else {
        return;
    };
    let Some(path) = dart_uri_text(uri_node, ctx.source) else {
        return;
    };
    if path.is_empty() {
        return;
    }
    ctx.imports.push(ParsedImport {
        file_id: ctx.file.id.clone(),
        owner_id: None,
        path,
        alias: Some(DART_PART_ALIAS.to_string()),
        is_glob: true,
        is_reexport: true,
        is_static: false,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-dart", "part directive"),
        kind: ImportKind::Wildcard,
        imported_name: None,
        is_global: false,
    });
}

fn extract_dart_part_of(node: Node<'_>, ctx: &mut ExtractContext<'_>) {
    let mut cursor = node.walk();
    let mut path = String::new();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "uri" => {
                if let Some(value) = dart_uri_string(child, ctx.source) {
                    path = value;
                    break;
                }
            }
            "dotted_identifier_list" => {
                if let Ok(value) = node_text(child, ctx.source) {
                    path = value.trim().to_string();
                }
            }
            _ => {}
        }
    }
    if path.is_empty() {
        return;
    }
    ctx.imports.push(ParsedImport {
        file_id: ctx.file.id.clone(),
        owner_id: None,
        path,
        alias: Some(DART_PART_OF_ALIAS.to_string()),
        is_glob: false,
        is_reexport: true,
        is_static: true,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-dart", "part-of directive"),
        kind: ImportKind::Unspecified,
        imported_name: None,
        is_global: false,
    });
}

fn dart_uri_text(node: Node<'_>, source: &str) -> Option<String> {
    match node.kind() {
        "uri" => dart_uri_string(node, source),
        "configurable_uri" => {
            // The primary URI is the first child of kind `uri`.
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .find(|child| child.kind() == "uri")
                .and_then(|uri| dart_uri_string(uri, source))
        }
        _ => None,
    }
}

fn dart_uri_string(node: Node<'_>, source: &str) -> Option<String> {
    // uri -> string_literal -> string_literal_*_quotes -> template_chars_*
    let raw = node_text(node, source).ok()?.trim().to_string();
    // Strip surrounding quotes.
    let trimmed = raw
        .trim_start_matches(['"', '\''])
        .trim_end_matches(['"', '\''])
        .to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn dart_configuration_uri_target(node: Node<'_>, source: &str) -> Option<String> {
    // configuration_uri -> ... `if (cond) 'target'` with a `uri` child holding the
    // alternate.
    let mut cursor = node.walk();
    let uri = node
        .named_children(&mut cursor)
        .find(|child| child.kind() == "uri")?;
    dart_uri_string(uri, source)
}

fn dart_combinator_clause(node: Node<'_>, source: &str) -> (String, Vec<String>) {
    // combinator -> first non-named child is `show`/`hide` keyword; the
    // identifier children name the imported/hidden symbols.
    let mut keyword = String::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named()
            && let Ok(text) = node_text(child, source)
        {
            let trimmed = text.trim();
            if trimmed == "show" || trimmed == "hide" {
                keyword = trimmed.to_string();
                break;
            }
        }
    }
    let mut names = Vec::new();
    let mut id_cursor = node.walk();
    for child in node.named_children(&mut id_cursor) {
        if child.kind() == "identifier"
            && let Ok(text) = node_text(child, source)
        {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                names.push(trimmed.to_string());
            }
        }
    }
    (keyword, names)
}

/// Recognise a `package:test` / `flutter_test` test-declaration idiom call and
/// build a `Test` symbol for it. Matches `test(...)`, `testWidgets(...)` and
/// `group(...)` invoked as a plain function (no receiver) whose first argument
/// is a string literal description and which passes a closure body. The symbol
/// is named after the description so `kind=test` results read naturally.
fn dart_test_symbol_from_call(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> Option<ParsedSymbol> {
    let function_node = node.child_by_field_name("function")?;
    // Only bare-identifier calls: `test(...)`, not `foo.test(...)` (which would
    // be an unrelated method on some object).
    if function_node.kind() != "identifier" {
        return None;
    }
    let func_name = node_text(function_node, ctx.source).ok()?.trim().to_string();
    if !matches!(func_name.as_str(), "test" | "testWidgets" | "group") {
        return None;
    }
    let arguments = node.child_by_field_name("arguments")?;
    let mut cursor = arguments.walk();
    let mut description: Option<String> = None;
    let mut has_closure = false;
    for child in arguments.named_children(&mut cursor) {
        match child.kind() {
            kind if is_dart_string_literal(kind) && description.is_none() => {
                description = dart_uri_string(child, ctx.source);
            }
            "function_expression" => has_closure = true,
            _ => {}
        }
    }
    // The closure argument is what makes this a test/group body rather than an
    // arbitrary call that merely happens to be named `test`.
    if !has_closure {
        return None;
    }
    let description = description?;
    let span = span_from_node(node);
    let body_span = node
        .child_by_field_name("arguments")
        .map(span_from_node)
        .or(Some(span));
    let parent_id = parent_symbol.map(|(id, _)| id.clone());
    // Disambiguate same-named tests within a file by call-site byte offset
    // (folded into `symbol_id`); keep the display name human-readable.
    let id = symbol_id(&ctx.file, parent_id.as_ref(), SymbolKind::Test, &description, span);
    Some(ParsedSymbol {
        id,
        file_id: ctx.file.id.clone(),
        parent_id,
        name: description,
        kind: SymbolKind::Test,
        language_identity: None,
        span,
        body_span,
        signature_span: None,
        signature: String::new(),
        visibility: Some("public".to_string()),
        docs: Vec::new(),
        attributes: vec!["dart:test".to_string(), format!("dart:test-idiom:{func_name}")],
        provenance: Provenance::new("tree-sitter-dart", format!("{func_name} idiom")),
        confidence: Confidence::Heuristic,
        freshness: Freshness::Fresh,
        arity: None,
    })
}

/// True for any Dart string-literal node kind (plain, raw, single/double,
/// single/multi-line). Used to spot a test description argument.
fn is_dart_string_literal(kind: &str) -> bool {
    kind == "string_literal" || (kind.contains("string_literal") && kind.contains("quotes"))
}

fn extract_dart_call(node: Node<'_>, ctx: &mut ExtractContext<'_>, owner_id: Option<SymbolId>) {
    let Some(function_node) = node.child_by_field_name("function") else {
        return;
    };
    let raw = node_text(node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    let target_text = node_text(function_node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if target_text.is_empty() {
        return;
    }
    let name = match function_node.kind() {
        "member_expression" => function_node
            .child_by_field_name("property")
            .and_then(|prop| node_text(prop, ctx.source).ok())
            .map(|text| text.trim().to_string())
            .unwrap_or_else(|| last_path_segment(&target_text)),
        _ => last_path_segment(&target_text),
    };
    let receiver = match function_node.kind() {
        "member_expression" => function_node
            .child_by_field_name("object")
            .and_then(|obj| node_text(obj, ctx.source).ok())
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty()),
        _ => receiver_from_method_text(&raw, &name),
    };
    let arity = node
        .child_by_field_name("arguments")
        .map(|args| dart_arg_count(args))
        .unwrap_or(0);
    let kind = if receiver.is_some()
        || function_node.kind() == "member_expression"
        || target_text.contains('.')
    {
        ParsedCallKind::Method
    } else {
        ParsedCallKind::Direct
    };
    let confidence = if matches!(kind, ParsedCallKind::Method) {
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
        provenance: Provenance::new("tree-sitter-dart", node.kind()),
        confidence,
    });
    extract_body_hit(node, BodyHitKind::Call, ctx, owner_id);
}

fn extract_dart_object_creation(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let type_node = node.child_by_field_name("type");
    let constructor_node = node.child_by_field_name("constructor");
    let target_text = match (type_node, constructor_node) {
        (Some(ty), Some(ctor)) => format!(
            "{}.{}",
            node_text(ty, ctx.source).unwrap_or_default().trim(),
            node_text(ctor, ctx.source).unwrap_or_default().trim()
        ),
        (Some(ty), None) => node_text(ty, ctx.source)
            .unwrap_or_default()
            .trim()
            .to_string(),
        _ => {
            let raw = node_text(node, ctx.source).unwrap_or_default();
            java_object_type_from_text(raw)
        }
    };
    if target_text.is_empty() {
        return;
    }
    let arity = node
        .child_by_field_name("arguments")
        .map(|args| dart_arg_count(args))
        .unwrap_or(0);
    let name = last_path_segment(&target_text);
    ctx.calls.push(ParsedCall {
        file_id: ctx.file.id.clone(),
        caller_id: owner_id.clone(),
        name,
        target_text: target_text.clone(),
        receiver: receiver_from_direct_call(&target_text),
        arity,
        kind: ParsedCallKind::Direct,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-dart", node.kind()),
        confidence: Confidence::Heuristic,
    });
    ctx.body_hits.push(BodyHit {
        file_id: ctx.file.id.clone(),
        owner_id,
        text: target_text,
        kind: BodyHitKind::Call,
        span: span_from_node(node),
    });
}

fn dart_arg_count(args: Node<'_>) -> usize {
    let mut cursor = args.walk();
    let mut count = 0;
    for child in args.named_children(&mut cursor) {
        // arguments may include named_argument / argument_definition nodes;
        // count anything that is not a structural label as one call argument.
        if !matches!(child.kind(), "comment" | "block_comment") {
            count += 1;
        }
    }
    count
}

fn extract_dart_reference(
    node: Node<'_>,
    kind: ReferenceKind,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let text = node_text(node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if text.is_empty() || is_dart_keyword(&text) {
        return;
    }
    let body_kind = match kind {
        ReferenceKind::Type => BodyHitKind::Type,
        ReferenceKind::Path => BodyHitKind::Path,
        ReferenceKind::Identifier => BodyHitKind::Identifier,
        ReferenceKind::Field => BodyHitKind::Identifier,
        ReferenceKind::Attribute => BodyHitKind::Attribute,
    };
    ctx.references.push(ParsedReference {
        file_id: ctx.file.id.clone(),
        owner_id: owner_id.clone(),
        text: text.clone(),
        kind,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-dart", format!("{} reference", node.kind())),
    });
    ctx.body_hits.push(BodyHit {
        file_id: ctx.file.id.clone(),
        owner_id,
        text,
        kind: body_kind,
        span: span_from_node(node),
    });
}

fn extract_dart_annotation_reference(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let mut cursor = node.walk();
    let identifier = node
        .named_children(&mut cursor)
        .find(|child| matches!(child.kind(), "identifier" | "qualified"));
    let Some(identifier) = identifier else {
        return;
    };
    let Ok(text) = node_text(identifier, ctx.source) else {
        return;
    };
    let trimmed = text.trim();
    if trimmed.is_empty() || is_dart_keyword(trimmed) {
        return;
    }
    let span = span_from_node(identifier);
    ctx.references.push(ParsedReference {
        file_id: ctx.file.id.clone(),
        owner_id: owner_id.clone(),
        text: trimmed.to_string(),
        kind: ReferenceKind::Attribute,
        span,
        provenance: Provenance::new("tree-sitter-dart", "annotation"),
    });
    ctx.body_hits.push(BodyHit {
        file_id: ctx.file.id.clone(),
        owner_id,
        text: trimmed.to_string(),
        kind: BodyHitKind::Attribute,
        span,
    });
}

fn dart_identifier_is_declaration_name(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    let parent_kind = parent.kind();
    if matches!(
        parent_kind,
        "class_declaration"
            | "mixin_declaration"
            | "extension_declaration"
            | "extension_type_declaration"
            | "extension_type_name"
            | "enum_declaration"
            | "function_signature"
            | "method_signature"
            | "getter_signature"
            | "setter_signature"
            | "function_declaration"
            | "method_declaration"
            | "type_alias"
            | "constructor_signature"
            | "constant_constructor_signature"
            | "factory_constructor_signature"
            | "redirecting_factory_constructor_signature"
            | "operator_signature"
            | "library_name"
            | "import_specification"
            | "initialized_identifier"
            | "static_final_declaration"
    ) && parent
        .child_by_field_name("name")
        .map(|name| name == node)
        .unwrap_or(false)
    {
        return true;
    }
    // The combinator clause's identifiers are not real references.
    if parent_kind == "combinator" {
        return true;
    }
    // The dotted_identifier_list in library directives is captured via
    // the directive itself; suppress per-identifier references.
    if parent_kind == "dotted_identifier_list" {
        return true;
    }
    // Identifier children of enum_constant declare the constant name.
    if parent_kind == "enum_constant" {
        return true;
    }
    // Identifier children of constructor_signature (named ctor part) and
    // factory_constructor_signature.
    if matches!(
        parent_kind,
        "constructor_signature"
            | "constant_constructor_signature"
            | "factory_constructor_signature"
            | "redirecting_factory_constructor_signature"
    ) {
        return true;
    }
    // formal_parameter / constructor_param introduce parameter bindings, not
    // references.
    if matches!(
        parent_kind,
        "formal_parameter" | "constructor_param" | "super_formal_parameter"
    ) && parent
        .named_children(&mut parent.walk())
        .filter(|child| child.kind() == "identifier")
        .last()
        .map(|last| last == node)
        .unwrap_or(false)
    {
        return true;
    }
    false
}

fn is_dart_literal(kind: &str) -> bool {
    matches!(
        kind,
        "string_literal"
            | "raw_string_literal_double_quotes"
            | "raw_string_literal_double_quotes_multiple"
            | "raw_string_literal_single_quotes"
            | "raw_string_literal_single_quotes_multiple"
            | "string_literal_double_quotes"
            | "string_literal_double_quotes_multiple"
            | "string_literal_single_quotes"
            | "string_literal_single_quotes_multiple"
            | "decimal_integer_literal"
            | "decimal_floating_point_literal"
            | "hex_integer_literal"
            | "true"
            | "false"
            | "null_literal"
            | "symbol_literal"
            | "list_literal"
            | "set_or_map_literal"
            | "record_literal"
    )
}

fn is_dart_keyword(text: &str) -> bool {
    matches!(
        text,
        "abstract"
            | "as"
            | "assert"
            | "async"
            | "await"
            | "base"
            | "break"
            | "case"
            | "catch"
            | "class"
            | "const"
            | "continue"
            | "covariant"
            | "default"
            | "deferred"
            | "do"
            | "dynamic"
            | "else"
            | "enum"
            | "export"
            | "extends"
            | "extension"
            | "external"
            | "factory"
            | "false"
            | "final"
            | "finally"
            | "for"
            | "Function"
            | "get"
            | "hide"
            | "if"
            | "implements"
            | "import"
            | "in"
            | "interface"
            | "is"
            | "late"
            | "library"
            | "mixin"
            | "new"
            | "null"
            | "of"
            | "on"
            | "operator"
            | "part"
            | "rethrow"
            | "return"
            | "sealed"
            | "set"
            | "show"
            | "static"
            | "super"
            | "switch"
            | "sync"
            | "this"
            | "throw"
            | "true"
            | "try"
            | "typedef"
            | "var"
            | "void"
            | "when"
            | "while"
            | "with"
            | "yield"
    )
}

fn dart_rhs_is_call_expression(declarator: Node<'_>) -> bool {
    let mut cursor = declarator.walk();
    declarator.named_children(&mut cursor).any(|child| {
        matches!(
            child.kind(),
            "call_expression" | "new_expression" | "constructor_invocation"
        )
    })
}

fn dedup_dart_facts(ctx: &mut ExtractContext<'_>) {
    let mut references: HashSet<(u32, ReferenceKind, String)> = HashSet::new();
    ctx.references.retain(|reference| {
        references.insert((
            reference.span.start_byte,
            reference.kind,
            reference.text.clone(),
        ))
    });
    let mut body_hits: HashSet<(u32, BodyHitKind, String)> = HashSet::new();
    ctx.body_hits
        .retain(|hit| body_hits.insert((hit.span.start_byte, hit.kind, hit.text.clone())));
    let mut calls: HashSet<(u32, String)> = HashSet::new();
    ctx.calls
        .retain(|call| calls.insert((call.span.start_byte, call.target_text.clone())));
}
