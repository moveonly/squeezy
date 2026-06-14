use crate::languages::common::visit_named_children_with_state;
use crate::languages::rust::*;
use crate::*;

pub(crate) fn extract_java(file: FileRecord, source: &str, tree: &Tree) -> ParsedFile {
    let mut ctx = ExtractContext::new(file.clone(), source);
    let root = tree.root_node();
    record_parse_error_diagnostics(root, &mut ctx);

    visit_java_node(root, &mut ctx, None, None);
    dedup_java_facts(&mut ctx);

    let package = ctx
        .imports
        .iter()
        .find(|import| import.alias.as_deref() == Some("__java_package__"))
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

pub(crate) fn visit_java_node(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<(SymbolId, SymbolKind)>,
    owner_symbol: Option<SymbolId>,
) {
    if node.is_missing() {
        record_missing_node_diagnostic(node, ctx);
        return;
    }

    match node.kind() {
        "package_declaration" => extract_java_package(node, ctx),
        "import_declaration" => extract_java_import(node, ctx, owner_symbol.clone()),
        _ => {}
    }

    if node.kind() == "module_declaration" {
        extract_java_module(node, ctx);
        return;
    }

    if node.kind() == "field_declaration" {
        let symbols = java_field_symbols_from_node(node, ctx, parent_symbol.as_ref());
        if !symbols.is_empty() {
            for symbol in symbols {
                ctx.symbols.push(symbol);
            }
            visit_java_children(node, ctx, parent_symbol, owner_symbol);
            return;
        }
    }

    if let Some(symbol) = java_symbol_from_node(node, ctx, parent_symbol.as_ref()) {
        let next_parent = Some((symbol.id.clone(), symbol.kind));
        let next_owner = if symbol.body_span.is_some() {
            Some(symbol.id.clone())
        } else {
            owner_symbol.clone()
        };
        ctx.symbols.push(symbol);
        if node.kind() == "record_declaration" {
            let components = java_record_component_symbols(node, ctx, next_parent.as_ref());
            ctx.symbols.extend(components);
        }
        visit_java_children(node, ctx, next_parent, next_owner);
        return;
    }

    match node.kind() {
        "method_invocation" => {
            extract_java_method_invocation(node, ctx, owner_symbol.clone());
            visit_java_children(node, ctx, parent_symbol, owner_symbol);
        }
        "object_creation_expression" => {
            extract_java_object_creation(node, ctx, owner_symbol.clone());
            if let Some(body) = java_first_child_of_kind(node, "class_body") {
                // Anonymous class (`new T(){ ... }`): promote it to a synthetic
                // Partial Class so the type, its base/interface relationship, and
                // its overridden members are visible and reparent correctly.
                let symbol = java_anonymous_class_symbol(node, ctx, parent_symbol.as_ref());
                let next_parent = Some((symbol.id.clone(), symbol.kind));
                let next_owner = Some(symbol.id.clone());
                ctx.symbols.push(symbol);
                // Recurse the constructor arguments under the outer owner, but
                // the class body under the synthetic anonymous class so its
                // members reparent there.
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    if child.id() == body.id() {
                        visit_java_children(child, ctx, next_parent.clone(), next_owner.clone());
                    } else {
                        visit_java_node(child, ctx, parent_symbol.clone(), owner_symbol.clone());
                    }
                }
            } else {
                visit_java_children(node, ctx, parent_symbol, owner_symbol);
            }
        }
        "method_reference" => {
            extract_java_method_reference(node, ctx, owner_symbol.clone());
            visit_java_children(node, ctx, parent_symbol, owner_symbol);
        }
        "explicit_constructor_invocation" => {
            extract_java_explicit_constructor_invocation(node, ctx, owner_symbol.clone());
            visit_java_children(node, ctx, parent_symbol, owner_symbol);
        }
        "identifier" => {}
        "type_identifier" | "scoped_type_identifier" => {
            extract_java_reference(node, ReferenceKind::Type, ctx, owner_symbol.clone())
        }
        "scoped_identifier" => {
            extract_java_reference(node, ReferenceKind::Path, ctx, owner_symbol.clone())
        }
        "field_access" => {
            extract_java_reference(node, ReferenceKind::Field, ctx, owner_symbol.clone())
        }
        "marker_annotation" | "annotation" => {
            extract_java_annotation_reference(node, ctx, owner_symbol)
        }
        kind if is_java_literal(kind) => {
            extract_body_hit(node, BodyHitKind::Literal, ctx, owner_symbol)
        }
        _ => visit_java_children(node, ctx, parent_symbol, owner_symbol),
    }
}

pub(crate) fn visit_java_children(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<(SymbolId, SymbolKind)>,
    owner_symbol: Option<SymbolId>,
) {
    visit_named_children_with_state(node, (parent_symbol, owner_symbol), |child, state| {
        let (parent_symbol, owner_symbol) = state;
        visit_java_node(child, ctx, parent_symbol, owner_symbol);
    });
}

pub(crate) fn java_symbol_from_node(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> Option<ParsedSymbol> {
    let kind = match node.kind() {
        "class_declaration" => SymbolKind::Class,
        "interface_declaration" | "annotation_type_declaration" => SymbolKind::Trait,
        "enum_declaration" => SymbolKind::Enum,
        "enum_constant" => SymbolKind::Variant,
        "record_declaration" => SymbolKind::Struct,
        "annotation_type_element_declaration" => SymbolKind::Method,
        "method_declaration" => SymbolKind::Method,
        "constructor_declaration" => SymbolKind::Method,
        _ => return None,
    };

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
    let arity = if matches!(kind, SymbolKind::Method) {
        node.child_by_field_name("parameters")
            .map(|params| u8::try_from(named_child_count(params)).unwrap_or(u8::MAX))
    } else {
        None
    };
    let mut attributes = java_modifier_attributes(node, ctx.source);
    if is_java_test_symbol(&ctx.file.relative_path, kind, &name, &attributes) {
        attributes.push("java:test".to_string());
    }
    if matches!(
        kind,
        SymbolKind::Class | SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Interface
    ) {
        attributes.extend(java_type_inheritance_attributes(node, ctx.source));
    }
    attributes.extend(java_generic_constraint_attributes(node, ctx.source));
    attributes.sort();
    attributes.dedup();

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
        visibility: java_visibility_text(node, ctx.source),
        docs: java_docs_for_node(node, ctx.source),
        attributes,
        provenance: Provenance::new("tree-sitter-java", format!("{} declaration", node.kind())),
        confidence: Confidence::ExactSyntax,
        freshness: Freshness::Fresh,
        arity,
    })
}

pub(crate) fn java_field_symbols_from_node(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> Vec<ParsedSymbol> {
    let mut attributes = java_modifier_attributes(node, ctx.source);
    if let Some(field_type) = java_field_type(node, ctx.source) {
        attributes.push(format!("type:{field_type}"));
    }
    attributes.sort();
    attributes.dedup();

    let visibility = java_visibility_text(node, ctx.source);
    let docs = java_docs_for_node(node, ctx.source);
    let signature = signature_text(node, None, ctx.source);
    let parent_id = parent_symbol.map(|(id, _)| id.clone());

    let mut symbols = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "variable_declarator" {
            continue;
        }
        let Some(name) = child
            .child_by_field_name("name")
            .and_then(|grandchild| node_text(grandchild, ctx.source).ok())
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty())
        else {
            continue;
        };
        let span = span_from_node(child);
        let id = symbol_id(
            &ctx.file,
            parent_id.as_ref(),
            SymbolKind::Field,
            &name,
            span,
        );
        symbols.push(ParsedSymbol {
            id,
            file_id: ctx.file.id.clone(),
            parent_id: parent_id.clone(),
            name,
            kind: SymbolKind::Field,
            language_identity: None,
            span,
            body_span: None,
            signature_span: None,
            signature: signature.clone(),
            visibility: visibility.clone(),
            docs: docs.clone(),
            attributes: attributes.clone(),
            provenance: Provenance::new("tree-sitter-java", "field_declaration declaration"),
            confidence: Confidence::ExactSyntax,
            freshness: Freshness::Fresh,
            arity: None,
        });
    }
    symbols
}

/// Positional record components (`record Point(int x, int y)`) are the
/// synthesized init-only properties of a record. The grammar exposes them as
/// `formal_parameter`s under the record's `parameters` field, but they are
/// otherwise invisible to decl/reference search and leave the record looking
/// empty. Emit a `Field` symbol per component (parented to the record, type
/// recorded as a `type:` attribute) so the component resolves like any field.
pub(crate) fn java_record_component_symbols(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> Vec<ParsedSymbol> {
    let Some(parameters) = node.child_by_field_name("parameters") else {
        return Vec::new();
    };
    let parent_id = parent_symbol.map(|(id, _)| id.clone());
    let mut symbols = Vec::new();
    let mut cursor = parameters.walk();
    for parameter in parameters.named_children(&mut cursor) {
        if parameter.kind() != "formal_parameter" {
            continue;
        }
        let Some(name) = parameter
            .child_by_field_name("name")
            .and_then(|child| node_text(child, ctx.source).ok())
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty())
        else {
            continue;
        };
        let mut attributes = Vec::new();
        if let Some(field_type) = java_field_type(parameter, ctx.source) {
            attributes.push(format!("type:{field_type}"));
        }
        attributes.push("java:record-component".to_string());
        attributes.sort();
        attributes.dedup();
        let span = span_from_node(parameter);
        let id = symbol_id(
            &ctx.file,
            parent_id.as_ref(),
            SymbolKind::Field,
            &name,
            span,
        );
        symbols.push(ParsedSymbol {
            id,
            file_id: ctx.file.id.clone(),
            parent_id: parent_id.clone(),
            name,
            kind: SymbolKind::Field,
            language_identity: None,
            span,
            body_span: None,
            signature_span: None,
            signature: node_text(parameter, ctx.source)
                .unwrap_or_default()
                .trim()
                .to_string(),
            visibility: None,
            docs: Vec::new(),
            attributes,
            provenance: Provenance::new("tree-sitter-java", "record component declaration"),
            confidence: Confidence::ExactSyntax,
            freshness: Freshness::Fresh,
            arity: None,
        });
    }
    symbols
}

/// A Java 9 (JPMS) module declaration in `module-info.java`. Emit a `Module`
/// symbol for the module itself and translate its directives into facts:
/// `requires <module>` becomes a (named) `Imports` fact and
/// `exports <package>` becomes a `Reexports` fact, so module-info contributes
/// both a symbol and JPMS dependency edges. `opens`/`provides`/`uses` are left
/// unmodelled for now.
pub(crate) fn extract_java_module(node: Node<'_>, ctx: &mut ExtractContext<'_>) {
    let Some(name) = node
        .child_by_field_name("name")
        .and_then(|child| node_text(child, ctx.source).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
    else {
        return;
    };
    let span = span_from_node(node);
    let body = node.child_by_field_name("body");
    let id = symbol_id(&ctx.file, None, SymbolKind::Module, &name, span);
    ctx.symbols.push(ParsedSymbol {
        id: id.clone(),
        file_id: ctx.file.id.clone(),
        parent_id: None,
        name,
        kind: SymbolKind::Module,
        language_identity: None,
        span,
        body_span: body.map(span_from_node),
        signature_span: signature_span_from_nodes(node, body),
        signature: signature_text(node, body, ctx.source),
        visibility: None,
        docs: java_docs_for_node(node, ctx.source),
        attributes: vec!["java:module".to_string()],
        provenance: Provenance::new("tree-sitter-java", "module declaration"),
        confidence: Confidence::ExactSyntax,
        freshness: Freshness::Fresh,
        arity: None,
    });

    let Some(body) = body else {
        return;
    };
    let mut cursor = body.walk();
    for directive in body.named_children(&mut cursor) {
        let (field, is_reexport, kind, provenance) = match directive.kind() {
            "requires_module_directive" => (
                "module",
                false,
                ImportKind::Named,
                "module requires directive",
            ),
            "exports_module_directive" => (
                "package",
                true,
                ImportKind::Unspecified,
                "module exports directive",
            ),
            _ => continue,
        };
        let Some(path) = directive
            .child_by_field_name(field)
            .and_then(|child| node_text(child, ctx.source).ok())
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty())
        else {
            continue;
        };
        let imported_name = Some(last_path_segment(&path));
        ctx.imports.push(ParsedImport {
            file_id: ctx.file.id.clone(),
            owner_id: Some(id.clone()),
            path,
            alias: None,
            is_glob: false,
            is_reexport,
            is_static: false,
            span: span_from_node(directive),
            provenance: Provenance::new("tree-sitter-java", provenance),
            kind,
            imported_name,
            is_global: false,
        });
    }
}

pub(crate) fn extract_java_package(node: Node<'_>, ctx: &mut ExtractContext<'_>) {
    let raw = node_text(node, ctx.source).unwrap_or_default();
    let Some(path) = raw
        .trim()
        .strip_prefix("package")
        .map(|text| text.trim().trim_end_matches(';').trim().to_string())
        .filter(|text| !text.is_empty())
    else {
        return;
    };
    ctx.imports.push(ParsedImport {
        file_id: ctx.file.id.clone(),
        owner_id: None,
        path,
        alias: Some("__java_package__".to_string()),
        is_glob: false,
        is_reexport: true,
        is_static: false,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-java", "package declaration"),
        kind: ImportKind::Unspecified,
        imported_name: None,
        is_global: false,
    });
}

pub(crate) fn extract_java_import(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let raw = node_text(node, ctx.source).unwrap_or_default();
    let Some(mut path) = raw
        .trim()
        .strip_prefix("import")
        .map(|text| text.trim().trim_end_matches(';').trim().to_string())
    else {
        return;
    };
    let is_static = path.strip_prefix("static ").is_some();
    if is_static {
        path = path.trim_start_matches("static ").trim().to_string();
    }
    if path.is_empty() {
        return;
    }
    let is_glob = path.ends_with(".*");
    let kind = if is_glob {
        ImportKind::Wildcard
    } else if is_static {
        ImportKind::Static
    } else {
        ImportKind::Named
    };
    let imported_name = if is_glob {
        None
    } else {
        Some(last_path_segment(&path))
    };
    ctx.imports.push(ParsedImport {
        file_id: ctx.file.id.clone(),
        owner_id,
        path,
        alias: None,
        is_glob,
        is_reexport: false,
        is_static,
        span: span_from_node(node),
        provenance: Provenance::new(
            "tree-sitter-java",
            if is_static {
                "static import declaration"
            } else {
                "import declaration"
            },
        ),
        kind,
        imported_name,
        is_global: false,
    });
}

pub(crate) fn extract_java_method_invocation(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let raw = node_text(node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if raw.is_empty() {
        return;
    }
    let name = node
        .child_by_field_name("name")
        .and_then(|child| node_text(child, ctx.source).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| method_name_from_text(&raw));
    if name.is_empty() {
        return;
    }
    let receiver = node
        .child_by_field_name("object")
        .or_else(|| node.child_by_field_name("receiver"))
        .and_then(|child| node_text(child, ctx.source).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .or_else(|| receiver_from_method_text(&raw, &name));
    let arity = node
        .child_by_field_name("arguments")
        .map(named_child_count)
        .unwrap_or_default();

    ctx.calls.push(ParsedCall {
        file_id: ctx.file.id.clone(),
        caller_id: owner_id.clone(),
        name,
        target_text: raw,
        receiver,
        arity,
        kind: ParsedCallKind::Method,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-java", "method_invocation"),
        confidence: Confidence::CandidateSet,
    });
    extract_body_hit(node, BodyHitKind::Call, ctx, owner_id);
}

/// An explicit constructor invocation is a `super(...)` or `this(...)`
/// delegation in a constructor body. Both were unhandled, so
/// constructor-to-superconstructor and constructor-to-overloaded-constructor
/// delegation produced no call edges and upstream/downstream flow on
/// constructors missed them. Emit a `Direct` `ParsedCall` named for the
/// enclosing class (the target type of both `super` and `this` delegation),
/// carrying the argument arity, so the resolver can bind it to a constructor.
pub(crate) fn extract_java_explicit_constructor_invocation(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let Some(class_name) = java_enclosing_type_name(node, ctx.source) else {
        return;
    };
    let receiver = node
        .child_by_field_name("constructor")
        .and_then(|child| node_text(child, ctx.source).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());
    let arity = node
        .child_by_field_name("arguments")
        .map(named_child_count)
        .unwrap_or_default();
    let target_text = node_text(node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    ctx.calls.push(ParsedCall {
        file_id: ctx.file.id.clone(),
        caller_id: owner_id.clone(),
        name: class_name,
        target_text,
        receiver,
        arity,
        kind: ParsedCallKind::Direct,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-java", "explicit_constructor_invocation"),
        confidence: Confidence::CandidateSet,
    });
    extract_body_hit(node, BodyHitKind::Call, ctx, owner_id);
}

/// Walk up to the nearest enclosing type declaration and return its declared
/// name. Used to name a `super(...)`/`this(...)` delegation after the class
/// whose constructor it targets.
fn java_enclosing_type_name(node: Node<'_>, source: &str) -> Option<String> {
    let mut current = node.parent();
    while let Some(parent) = current {
        if matches!(
            parent.kind(),
            "class_declaration"
                | "record_declaration"
                | "enum_declaration"
                | "interface_declaration"
        ) {
            return parent
                .child_by_field_name("name")
                .and_then(|child| node_text(child, source).ok())
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty());
        }
        current = parent.parent();
    }
    None
}

/// A method reference (`User::getName`, `Comparator::naturalOrder`,
/// `String::new`) names a method without invoking it. The grammar shapes it as
/// `<receiver> :: (type_arguments)? (identifier | new)`, where the receiver is
/// the first named child (a type/primary/super) and the method name is the
/// trailing token. Without this arm the name was dropped and only the receiver
/// type leaked as a `Type` reference, so idiomatic stream code
/// (`.map(User::getName)`) was missing from the call graph. Emit a `Method`
/// `ParsedCall` (`CandidateSet`) so the resolver matches it like any call.
pub(crate) fn extract_java_method_reference(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    // The method name is the last child token after `::`; it is `new` for a
    // constructor reference and an anonymous `identifier` otherwise.
    let mut cursor = node.walk();
    let name = node
        .children(&mut cursor)
        .filter(|child| matches!(child.kind(), "identifier" | "new"))
        .last()
        .and_then(|child| node_text(child, ctx.source).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());
    let Some(name) = name else {
        return;
    };
    let receiver = node
        .named_child(0)
        .and_then(|child| node_text(child, ctx.source).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());
    let target_text = node_text(node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    ctx.calls.push(ParsedCall {
        file_id: ctx.file.id.clone(),
        caller_id: owner_id.clone(),
        name,
        target_text,
        receiver,
        arity: 0,
        kind: ParsedCallKind::Method,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-java", "method_reference"),
        confidence: Confidence::CandidateSet,
    });
    extract_body_hit(node, BodyHitKind::Call, ctx, owner_id);
}

pub(crate) fn extract_java_object_creation(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let target_text = node
        .child_by_field_name("type")
        .and_then(|child| node_text(child, ctx.source).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| {
            java_object_type_from_text(node_text(node, ctx.source).unwrap_or_default())
        });
    if target_text.is_empty() {
        return;
    }
    let arity = node
        .child_by_field_name("arguments")
        .map(named_child_count)
        .unwrap_or_default();
    ctx.calls.push(ParsedCall {
        file_id: ctx.file.id.clone(),
        caller_id: owner_id.clone(),
        name: last_path_segment(&target_text),
        target_text: target_text.clone(),
        receiver: receiver_from_direct_call(&target_text),
        arity,
        kind: ParsedCallKind::Direct,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-java", "object_creation_expression"),
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

pub(crate) fn extract_java_reference(
    node: Node<'_>,
    kind: ReferenceKind,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let text = node_text(node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if text.is_empty() || is_java_keyword(&text) {
        return;
    }
    let body_kind = match kind {
        ReferenceKind::Identifier | ReferenceKind::Field => None,
        ReferenceKind::Attribute => Some(BodyHitKind::Attribute),
        ReferenceKind::Type => Some(BodyHitKind::Type),
        ReferenceKind::Path => Some(BodyHitKind::Path),
    };
    ctx.references.push(ParsedReference {
        file_id: ctx.file.id.clone(),
        owner_id: owner_id.clone(),
        text: text.clone(),
        kind,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-java", format!("{} reference", node.kind())),
    });
    if let Some(body_kind) = body_kind {
        ctx.body_hits.push(BodyHit {
            file_id: ctx.file.id.clone(),
            owner_id,
            text,
            kind: body_kind,
            span: span_from_node(node),
        });
    }
}

pub(crate) fn extract_java_annotation_reference(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let name_node = node
        .child_by_field_name("name")
        .or_else(|| java_first_name_descendant(node));
    let text = name_node
        .and_then(|child| node_text(child, ctx.source).ok())
        .map(|raw| raw.trim().to_string())
        .filter(|raw| !raw.is_empty())
        .unwrap_or_else(|| {
            let raw = node_text(node, ctx.source).unwrap_or_default();
            raw.trim()
                .trim_start_matches('@')
                .split('(')
                .next()
                .unwrap_or_default()
                .trim()
                .to_string()
        });
    if text.is_empty() || is_java_keyword(&text) {
        return;
    }
    let span = name_node
        .map(span_from_node)
        .unwrap_or_else(|| span_from_node(node));
    ctx.references.push(ParsedReference {
        file_id: ctx.file.id.clone(),
        owner_id: owner_id.clone(),
        text: text.clone(),
        kind: ReferenceKind::Attribute,
        span,
        provenance: Provenance::new("tree-sitter-java", format!("{} reference", node.kind())),
    });
    ctx.body_hits.push(BodyHit {
        file_id: ctx.file.id.clone(),
        owner_id,
        text,
        kind: BodyHitKind::Attribute,
        span,
    });
}

pub(crate) fn java_first_child_of_kind<'tree>(
    node: Node<'tree>,
    kind: &str,
) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.kind() == kind)
}

/// Synthesize a `Partial` Class symbol for an anonymous-class object creation
/// (`new Runnable(){ ... }`). Without it the anonymous type is invisible, its
/// supertype relationship is lost, and its overridden methods reparent to the
/// wrong owner. The supertype named after `new` is recorded as `base:<Name>`;
/// the shared inheritance-edge pass later classifies it as `Extends` (the base
/// is a class) or `Implements` (the base is an interface) from the resolved
/// target's kind, so we need not decide here.
pub(crate) fn java_anonymous_class_symbol(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> ParsedSymbol {
    let span = span_from_node(node);
    let name = format!(
        "__anonymous_class_{}_{}",
        span.start.line, span.start.column
    );
    let parent_id = parent_symbol.map(|(id, _)| id.clone());
    let body = java_first_child_of_kind(node, "class_body");
    let body_span = body.map(span_from_node);
    let signature_span = signature_span_from_nodes(node, body);
    let signature = signature_text(node, body, ctx.source);
    let id = symbol_id(
        &ctx.file,
        parent_id.as_ref(),
        SymbolKind::Class,
        &name,
        span,
    );
    let mut attributes = vec!["java:anonymous-class".to_string()];
    if let Some(type_node) = node.child_by_field_name("type")
        && let Ok(text) = node_text(type_node, ctx.source)
        && let Some(base) = java_type_name_from_text(text)
    {
        attributes.push(format!("base:{base}"));
    }
    attributes.sort();
    attributes.dedup();

    ParsedSymbol {
        id,
        file_id: ctx.file.id.clone(),
        parent_id,
        name,
        kind: SymbolKind::Class,
        language_identity: None,
        span,
        body_span,
        signature_span,
        signature,
        visibility: None,
        docs: Vec::new(),
        attributes,
        provenance: Provenance::new("tree-sitter-java", "anonymous class"),
        confidence: Confidence::Partial,
        freshness: Freshness::Fresh,
        arity: None,
    }
}

pub(crate) fn java_first_name_descendant(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if matches!(
            child.kind(),
            "identifier" | "scoped_identifier" | "type_identifier" | "scoped_type_identifier"
        ) {
            return Some(child);
        }
        if let Some(found) = java_first_name_descendant(child) {
            return Some(found);
        }
    }
    None
}

pub(crate) fn dedup_java_facts(ctx: &mut ExtractContext<'_>) {
    let mut references: HashSet<(u32, ReferenceKind)> = HashSet::new();
    ctx.references
        .retain(|reference| references.insert((reference.span.start_byte, reference.kind)));
    let mut body_hits: HashSet<(u32, BodyHitKind)> = HashSet::new();
    ctx.body_hits
        .retain(|hit| body_hits.insert((hit.span.start_byte, hit.kind)));
}

/// Annotation tags (delegated to the shared helper) plus the keyword modifiers
/// the grammar exposes as *anonymous* children of the `modifiers` node
/// (`abstract`, `final`, `static`, `default`, `sealed`, …). Tree-sitter only
/// lists `annotation`/`marker_annotation` as named children, so the keyword
/// tokens are reachable through the full `children` walk only — mirroring how
/// `java_visibility_text` reads `public`/`protected`/`private`. Each keyword is
/// pushed as `java:<kw>` so `decl_search` can filter abstract-vs-concrete,
/// static-vs-instance, and sealed declarations.
pub(crate) fn java_modifier_attributes(node: Node<'_>, source: &str) -> Vec<String> {
    let mut attributes = java_attributes_for_node(node, source);
    let Some(modifiers) = java_modifiers_node(node) else {
        return attributes;
    };
    let mut cursor = modifiers.walk();
    for child in modifiers.children(&mut cursor) {
        let keyword = match child.kind() {
            "abstract" | "final" | "static" | "default" | "sealed" | "non-sealed"
            | "synchronized" | "native" | "transient" | "volatile" | "strictfp" => child.kind(),
            _ => continue,
        };
        attributes.push(format!("java:{keyword}"));
    }
    attributes
}

/// Lower a type declaration's supertypes into `base:`/`iface:` attributes.
///
/// The shared `add_generic_inheritance_edges` resolution pass lowers `base:`
/// into `Extends`/`Implements` (keyed off the resolved target's kind) and
/// `iface:` always into `Implements`. We therefore route the `implements`
/// clause (the `interfaces` / `super_interfaces` field) to `iface:`, a class's
/// `extends` clause (`superclass`) to `base:`, and an interface's own `extends`
/// clause (`extends_interfaces`) to `base:` so interface-to-interface
/// inheritance surfaces too. Previously both clauses were collapsed into
/// `base:` and `extends_interfaces` was ignored entirely.
pub(crate) fn java_type_inheritance_attributes(node: Node<'_>, source: &str) -> Vec<String> {
    let mut base = Vec::new();
    for field in ["superclass", "extends_interfaces"] {
        if let Some(child) = node.child_by_field_name(field) {
            collect_java_type_names(child, source, &mut base);
        }
    }
    let mut iface = Vec::new();
    if let Some(child) = node.child_by_field_name("interfaces") {
        collect_java_type_names(child, source, &mut iface);
    }
    let mut attributes = Vec::new();
    attributes.extend(base.into_iter().map(|name| format!("base:{name}")));
    attributes.extend(iface.into_iter().map(|name| format!("iface:{name}")));
    attributes
}

/// Generic type-parameter bounds (`<T extends Comparable<T>>`) lowered to
/// `base:<Leaf>` constraint attributes for both types and methods. The grammar
/// nests each bound under `type_parameters → type_parameter → type_bound`,
/// whose children are the bound types. Only the bound types are emitted; the
/// parameter names (`T`, `U`) are intentionally dropped so they neither pollute
/// reference search nor leak as undifferentiated `Type` references.
pub(crate) fn java_generic_constraint_attributes(node: Node<'_>, source: &str) -> Vec<String> {
    let Some(type_parameters) = node.child_by_field_name("type_parameters") else {
        return Vec::new();
    };
    let mut names = Vec::new();
    let mut params = type_parameters.walk();
    for parameter in type_parameters.named_children(&mut params) {
        if parameter.kind() != "type_parameter" {
            continue;
        }
        let mut bounds = parameter.walk();
        for bound in parameter.named_children(&mut bounds) {
            if bound.kind() == "type_bound" {
                collect_java_type_names(bound, source, &mut names);
            }
        }
    }
    names
        .into_iter()
        .map(|name| format!("base:{name}"))
        .collect()
}
