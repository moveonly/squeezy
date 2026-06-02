use std::collections::HashMap;

use crate::languages::rust::*;
use crate::*;

pub(crate) fn extract_go(file: FileRecord, source: &str, tree: &Tree) -> ParsedFile {
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

    let package = go_package_name(root, source);
    // Pre-scan top-level type declarations so methods declared earlier in
    // source order than their receiver type still attach to the right parent.
    // The symbol ids computed here must match the ones produced later by
    // `go_type_symbol`, so we use the same `symbol_id` inputs (file, parent=None,
    // kind, name, span).
    ctx.go_type_index = collect_go_type_index(root, &file, source);
    visit_go_node(root, &mut ctx, None, None);
    dedup_go_facts(&mut ctx);

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

pub(crate) fn visit_go_node(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<SymbolId>,
    owner_symbol: Option<SymbolId>,
) {
    if node.is_missing() {
        ctx.diagnostics.push(ParseDiagnostic {
            message: format!("missing {}", node.kind()),
            span: Some(span_from_node(node)),
            confidence: Confidence::Partial,
        });
        return;
    }

    match node.kind() {
        "import_declaration" => extract_go_import(node, ctx, owner_symbol.clone()),
        "const_declaration" | "var_declaration"
            if owner_symbol.is_none() && !go_has_ancestor_kind(node, "func_literal") =>
        {
            extract_go_value_declarations(node, ctx, parent_symbol.clone());
        }
        "field_declaration" => {
            ctx.symbols
                .extend(go_field_symbols(node, ctx, parent_symbol.clone()));
        }
        _ => {}
    }

    if let Some(symbol) =
        go_symbol_from_node(node, ctx, parent_symbol.clone(), owner_symbol.as_ref())
    {
        let next_parent = Some(symbol.id.clone());
        let next_owner = if symbol.body_span.is_some()
            || matches!(
                symbol.kind,
                SymbolKind::Function | SymbolKind::Method | SymbolKind::Test
            ) {
            Some(symbol.id.clone())
        } else {
            owner_symbol.clone()
        };
        ctx.symbols.push(symbol);
        visit_go_children(node, ctx, next_parent, next_owner);
        return;
    }

    match node.kind() {
        "call_expression" => extract_go_call(node, ctx, owner_symbol.clone()),
        "selector_expression" => extract_go_selector_reference(node, ctx, owner_symbol.clone()),
        kind if go_reference_kind(kind).is_some() => {
            extract_go_reference(
                node,
                go_reference_kind(kind).unwrap(),
                ctx,
                owner_symbol.clone(),
            );
        }
        kind if is_go_literal(kind) => {
            extract_body_hit(node, BodyHitKind::Literal, ctx, owner_symbol.clone())
        }
        _ => {}
    }

    visit_go_children(node, ctx, parent_symbol, owner_symbol);
}

pub(crate) fn visit_go_children(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<SymbolId>,
    owner_symbol: Option<SymbolId>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit_go_node(child, ctx, parent_symbol.clone(), owner_symbol.clone());
    }
}

pub(crate) fn go_package_name(root: Node<'_>, source: &str) -> Option<String> {
    let mut cursor = root.walk();
    root.named_children(&mut cursor)
        .find(|child| child.kind() == "package_clause")
        .and_then(|package| first_named_child_text(package, source))
}

pub(crate) fn go_symbol_from_node(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<SymbolId>,
    owner_symbol: Option<&SymbolId>,
) -> Option<ParsedSymbol> {
    match node.kind() {
        "function_declaration" => {
            go_function_symbol(node, ctx, SymbolKind::Function, parent_symbol)
        }
        "method_declaration" => {
            let receiver = go_receiver_type(node, ctx.source);
            let parent_id = receiver
                .as_deref()
                .and_then(|name| find_go_type_parent_id(ctx, name))
                .or(parent_symbol);
            let mut symbol = go_function_symbol(node, ctx, SymbolKind::Method, parent_id)?;
            if let Some(receiver) = receiver {
                symbol.attributes.push(format!("go:receiver:{receiver}"));
            }
            Some(symbol)
        }
        "type_alias" | "type_spec"
            if owner_symbol.is_none() && !go_has_ancestor_kind(node, "func_literal") =>
        {
            go_type_symbol(node, ctx, parent_symbol)
        }
        _ => None,
    }
}

pub(crate) fn go_function_symbol(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    mut kind: SymbolKind,
    parent_symbol: Option<SymbolId>,
) -> Option<ParsedSymbol> {
    let name = node
        .child_by_field_name("name")
        .and_then(|child| node_text(child, ctx.source).ok())
        .map(str::to_string)
        .or_else(|| first_named_child_text(node, ctx.source))
        .map(|text| text.trim().to_string())
        .filter(|text| is_go_identifier(text))?;
    if matches!(kind, SymbolKind::Function | SymbolKind::Method)
        && go_is_test_function(&ctx.file.relative_path, &name)
    {
        kind = SymbolKind::Test;
    }
    let body = node.child_by_field_name("body");
    let span = span_from_node(node);
    let body_span = body.map(span_from_node);
    let signature = signature_text(node, body, ctx.source);
    let id = symbol_id(&ctx.file, parent_symbol.as_ref(), kind, &name, span);
    let mut attributes = go_doc_and_semantic_attributes(node, ctx.source);
    if kind == SymbolKind::Test {
        attributes.push("go:test".to_string());
    }
    attributes.sort();
    attributes.dedup();

    Some(ParsedSymbol {
        id,
        file_id: ctx.file.id.clone(),
        parent_id: parent_symbol,
        name,
        kind,
        language_identity: None,
        span,
        body_span,
        signature,
        visibility: go_visibility(node, ctx.source),
        docs: go_docs_for_node(node, ctx.source),
        attributes,
        provenance: Provenance::new("tree-sitter-go", format!("{} declaration", node.kind())),
        confidence: Confidence::ExactSyntax,
        freshness: Freshness::Fresh,
        arity: None,
    })
}

pub(crate) fn go_type_symbol(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<SymbolId>,
) -> Option<ParsedSymbol> {
    let name = node
        .child_by_field_name("name")
        .and_then(|child| node_text(child, ctx.source).ok())
        .map(str::to_string)
        .or_else(|| first_named_child_text(node, ctx.source))
        .map(|text| text.trim().to_string())
        .filter(|text| is_go_identifier(text))?;
    let type_node = node
        .child_by_field_name("type")
        .or_else(|| last_named_child(node));
    let kind = match type_node.map(|child| child.kind()) {
        Some("struct_type") => SymbolKind::Struct,
        Some("interface_type") => SymbolKind::Interface,
        _ => SymbolKind::TypeAlias,
    };
    let span = span_from_node(node);
    let body_span = type_node.map(span_from_node);
    let signature = node_text(node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    let attributes = go_doc_and_semantic_attributes(node, ctx.source);
    Some(ParsedSymbol {
        id: symbol_id(&ctx.file, parent_symbol.as_ref(), kind, &name, span),
        file_id: ctx.file.id.clone(),
        parent_id: parent_symbol,
        name,
        kind,
        language_identity: None,
        span,
        body_span,
        signature,
        visibility: go_visibility(node, ctx.source),
        docs: go_docs_for_node(node, ctx.source),
        attributes,
        provenance: Provenance::new("tree-sitter-go", format!("{} declaration", node.kind())),
        confidence: Confidence::ExactSyntax,
        freshness: Freshness::Fresh,
        arity: None,
    })
}

pub(crate) fn go_field_symbols(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<SymbolId>,
) -> Vec<ParsedSymbol> {
    let Some(parent_id) = parent_symbol else {
        return Vec::new();
    };
    let mut names = Vec::new();
    let mut is_embed = false;
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "field_identifier" | "identifier" => {
                if let Ok(text) = node_text(child, ctx.source) {
                    let text = text.trim();
                    if is_go_identifier(text) {
                        names.push((text.to_string(), span_from_node(child)));
                    }
                }
            }
            "type_identifier" if names.is_empty() => {
                if let Ok(text) = node_text(child, ctx.source) {
                    let text = text.trim();
                    if is_go_identifier(text) {
                        // A `type_identifier` with no preceding name token is
                        // a Go embedded field (e.g. `type Runner struct {
                        // Greeter }`), which promotes the embedded type's
                        // methods. Tag these so downstream consumers can
                        // distinguish them from named fields without parsing
                        // the receiver type themselves.
                        is_embed = true;
                        names.push((text.to_string(), span_from_node(child)));
                    }
                }
            }
            _ => {}
        }
    }
    names.sort_by_key(|left| left.1.start_byte);
    names.dedup_by(|left, right| left.0 == right.0 && left.1 == right.1);
    names
        .into_iter()
        .map(|(name, span)| {
            let mut attributes = vec!["go:field".to_string()];
            if is_embed {
                attributes.push("go:embed".to_string());
            }
            ParsedSymbol {
                id: symbol_id(&ctx.file, Some(&parent_id), SymbolKind::Field, &name, span),
                file_id: ctx.file.id.clone(),
                parent_id: Some(parent_id.clone()),
                name,
                kind: SymbolKind::Field,
                language_identity: None,
                span,
                body_span: None,
                signature: node_text(node, ctx.source)
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
                visibility: go_visibility(node, ctx.source),
                docs: Vec::new(),
                attributes,
                provenance: Provenance::new("tree-sitter-go", "field declaration"),
                confidence: Confidence::ExactSyntax,
                freshness: Freshness::Fresh,
                arity: None,
            }
        })
        .collect()
}

pub(crate) fn extract_go_value_declarations(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<SymbolId>,
) {
    let kind = if node.kind() == "const_declaration" {
        SymbolKind::Const
    } else {
        SymbolKind::Static
    };
    let mut cursor = node.walk();
    for child in go_value_specs(node, &mut cursor) {
        for (name, span) in go_names_in_spec(child, ctx.source) {
            ctx.symbols.push(ParsedSymbol {
                id: symbol_id(&ctx.file, parent_symbol.as_ref(), kind, &name, span),
                file_id: ctx.file.id.clone(),
                parent_id: parent_symbol.clone(),
                name,
                kind,
                language_identity: None,
                span,
                body_span: None,
                signature: node_text(child, ctx.source)
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
                visibility: go_visibility(child, ctx.source),
                docs: Vec::new(),
                attributes: vec![if kind == SymbolKind::Const {
                    "go:const".to_string()
                } else {
                    "go:var".to_string()
                }],
                provenance: Provenance::new(
                    "tree-sitter-go",
                    format!("{} declaration", child.kind()),
                ),
                confidence: Confidence::ExactSyntax,
                freshness: Freshness::Fresh,
                arity: None,
            });
        }
    }
}

pub(crate) fn go_value_specs<'tree>(
    node: Node<'tree>,
    cursor: &mut tree_sitter::TreeCursor<'tree>,
) -> Vec<Node<'tree>> {
    let mut specs = Vec::new();
    for child in node.named_children(cursor) {
        match child.kind() {
            "const_spec" | "var_spec" => specs.push(child),
            "const_spec_list" | "var_spec_list" => {
                let mut child_cursor = child.walk();
                specs.extend(go_value_specs(child, &mut child_cursor));
            }
            _ => {}
        }
    }
    specs
}

pub(crate) fn go_names_in_spec(node: Node<'_>, source: &str) -> Vec<(String, SourceSpan)> {
    let mut names = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if matches!(child.kind(), "identifier")
            && let Ok(text) = node_text(child, source)
        {
            let text = text.trim();
            if text != "_" && is_go_identifier(text) {
                names.push((text.to_string(), span_from_node(child)));
            }
            continue;
        }
        if child.kind() != "identifier" && !names.is_empty() {
            break;
        }
    }
    names
}

pub(crate) fn extract_go_import(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let raw = node_text(node, ctx.source).unwrap_or_default();
    for (path, alias, is_glob, span) in go_import_specs(node, raw, ctx.source) {
        let kind = if is_glob {
            ImportKind::Wildcard
        } else {
            ImportKind::Namespace
        };
        let imported_name = if is_glob {
            None
        } else {
            Some(last_path_segment(&path))
        };
        ctx.imports.push(ParsedImport {
            file_id: ctx.file.id.clone(),
            owner_id: owner_id.clone(),
            path,
            alias,
            is_glob,
            is_reexport: false,
            is_static: false,
            span,
            provenance: Provenance::new("tree-sitter-go", "import declaration"),
            kind,
            imported_name,
            is_global: false,
        });
    }
}

pub(crate) fn go_import_specs(
    node: Node<'_>,
    raw: &str,
    source: &str,
) -> Vec<(String, Option<String>, bool, SourceSpan)> {
    let mut specs = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "import_spec"
            && child.kind() != "interpreted_string_literal"
            && child.kind() != "raw_string_literal"
        {
            continue;
        }
        let spec_text = node_text(child, source).unwrap_or_default().trim();
        if let Some((path, alias, is_glob)) = parse_go_import_spec_text(spec_text) {
            specs.push((path, alias, is_glob, span_from_node(child)));
        }
    }
    if specs.is_empty() {
        for line in raw.lines() {
            if let Some((path, alias, is_glob)) = parse_go_import_spec_text(line.trim()) {
                specs.push((path, alias, is_glob, span_from_node(node)));
            }
        }
    }
    specs
}

pub(crate) fn parse_go_import_spec_text(text: &str) -> Option<(String, Option<String>, bool)> {
    let text = text.trim().trim_start_matches("import").trim();
    let quote_index = text.find('"').or_else(|| text.find('`'))?;
    let quote = text.as_bytes()[quote_index] as char;
    let rest = &text[quote_index + quote.len_utf8()..];
    let close = rest.find(quote)?;
    let path = rest[..close].to_string();
    let alias_text = text[..quote_index].trim().trim_matches(['(', ')']).trim();
    let alias = match alias_text {
        "" => None,
        "." => return Some((path, None, true)),
        "_" => Some("_".to_string()),
        other if is_go_identifier(other) => Some(other.to_string()),
        _ => None,
    };
    Some((path, alias, false))
}

pub(crate) fn extract_go_call(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    // tree-sitter-go's `call_expression` always exposes a `function` field on
    // healthy parses. The first-named-child fallback only fires if the grammar
    // we link against ever drops or renames that field, and lets us record a
    // partial call instead of silently dropping the node.
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
    let receiver = receiver_from_go_call(&target_text);
    let arity = node
        .child_by_field_name("arguments")
        .or_else(|| last_named_child(node))
        .map(named_child_count)
        .unwrap_or_default();
    ctx.calls.push(ParsedCall {
        file_id: ctx.file.id.clone(),
        caller_id: owner_id.clone(),
        name: last_path_segment(&target_text),
        target_text: target_text.clone(),
        receiver: receiver.clone(),
        arity,
        kind: if receiver.is_some() {
            ParsedCallKind::Method
        } else {
            ParsedCallKind::Direct
        },
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-go", "call_expression"),
        confidence: if receiver.is_some() {
            Confidence::CandidateSet
        } else {
            Confidence::Heuristic
        },
    });
    extract_body_hit(node, BodyHitKind::Call, ctx, owner_id);
}

pub(crate) fn extract_go_selector_reference(
    node: Node<'_>,
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
    // Record the full selector text as a reference so import-aware resolution
    // can match `pkg.Fn` against an imported package alias. Body hits for the
    // operand and the trailing field are produced by `visit_go_children` when
    // it descends into this selector's identifier/field_identifier children, so
    // we intentionally avoid emitting an additional wrapper body hit here to
    // keep selector-heavy files (e.g. etcd, prometheus) from inflating the
    // body-hit index.
    ctx.references.push(ParsedReference {
        file_id: ctx.file.id.clone(),
        owner_id,
        text,
        kind: ReferenceKind::Field,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-go", "selector reference"),
    });
}

pub(crate) fn extract_go_reference(
    node: Node<'_>,
    kind: ReferenceKind,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let text = node_text(node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if text.is_empty() || go_keyword_like(&text) {
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
        provenance: Provenance::new("tree-sitter-go", format!("{} reference", node.kind())),
    });
    ctx.body_hits.push(BodyHit {
        file_id: ctx.file.id.clone(),
        owner_id,
        text,
        kind: body_kind,
        span: span_from_node(node),
    });
}

pub(crate) fn dedup_go_facts(ctx: &mut ExtractContext<'_>) {
    let mut symbols = HashSet::new();
    ctx.symbols.retain(|symbol| {
        symbols.insert(format!(
            "{}|{}|{:?}|{}",
            symbol.file_id.0, symbol.id.0, symbol.kind, symbol.name
        ))
    });
    let mut references = HashSet::new();
    ctx.references.retain(|reference| {
        references.insert(format!(
            "{}|{}|{}|{:?}",
            reference.file_id.0, reference.span.start_byte, reference.text, reference.kind
        ))
    });
    let mut body_hits = HashSet::new();
    ctx.body_hits.retain(|hit| {
        body_hits.insert(format!(
            "{}|{}|{}|{}|{:?}",
            hit.file_id.0, hit.span.start_byte, hit.span.end_byte, hit.text, hit.kind
        ))
    });
}

pub(crate) fn go_receiver_type(node: Node<'_>, source: &str) -> Option<String> {
    let receiver = node.child_by_field_name("receiver").or_else(|| {
        let mut cursor = node.walk();
        node.named_children(&mut cursor)
            .find(|child| child.kind() == "parameter_list")
    })?;
    let raw = node_text(receiver, source).ok()?;
    let inner = raw
        .trim()
        .trim_start_matches('(')
        .trim_end_matches(')')
        .trim();
    let last = inner
        .split_whitespace()
        .last()
        .unwrap_or(inner)
        .trim_start_matches('*')
        .trim();
    let name = last_path_segment(last);
    is_go_identifier(&name).then_some(name)
}

pub(crate) fn find_go_type_parent_id(ctx: &ExtractContext<'_>, name: &str) -> Option<SymbolId> {
    // The prepass populates `ctx.go_type_index` with every top-level type
    // declaration in the file so methods declared earlier in source order
    // than their receiver type still attach to the right parent symbol.
    if let Some(id) = ctx.go_type_index.get(name) {
        return Some(id.clone());
    }
    ctx.symbols
        .iter()
        .rev()
        .find(|symbol| {
            symbol.name == name
                && matches!(
                    symbol.kind,
                    SymbolKind::Struct | SymbolKind::Interface | SymbolKind::TypeAlias
                )
        })
        .map(|symbol| symbol.id.clone())
}

pub(crate) fn collect_go_type_index(
    root: Node<'_>,
    file: &FileRecord,
    source: &str,
) -> HashMap<String, SymbolId> {
    let mut index = HashMap::new();
    collect_go_type_index_in(root, file, source, &mut index);
    index
}

pub(crate) fn collect_go_type_index_in(
    node: Node<'_>,
    file: &FileRecord,
    source: &str,
    index: &mut HashMap<String, SymbolId>,
) {
    // We only index top-level types here. Top-level for Go means siblings of
    // the `package_clause` under `source_file`, plus their nested
    // `type_declaration` -> `type_spec`/`type_alias` children. Skip anything
    // inside `func_literal` to mirror the visitor's scope filter.
    if go_has_ancestor_kind(node, "func_literal") {
        return;
    }
    if matches!(node.kind(), "type_spec" | "type_alias")
        && let Some((name, span, kind)) = go_type_index_entry(node, source)
    {
        let id = symbol_id(file, None, kind, &name, span);
        index.entry(name).or_insert(id);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_go_type_index_in(child, file, source, index);
    }
}

pub(crate) fn go_type_index_entry(
    node: Node<'_>,
    source: &str,
) -> Option<(String, SourceSpan, SymbolKind)> {
    let name = node
        .child_by_field_name("name")
        .and_then(|child| node_text(child, source).ok())
        .map(str::to_string)
        .or_else(|| first_named_child_text(node, source))
        .map(|text| text.trim().to_string())
        .filter(|text| is_go_identifier(text))?;
    let type_node = node
        .child_by_field_name("type")
        .or_else(|| last_named_child(node));
    let kind = match type_node.map(|child| child.kind()) {
        Some("struct_type") => SymbolKind::Struct,
        Some("interface_type") => SymbolKind::Interface,
        _ => SymbolKind::TypeAlias,
    };
    Some((name, span_from_node(node), kind))
}

pub(crate) fn go_doc_and_semantic_attributes(node: Node<'_>, source: &str) -> Vec<String> {
    let mut attributes = Vec::new();
    if !go_docs_for_node(node, source).is_empty() {
        attributes.push("go:doc".to_string());
    }
    attributes
}

pub(crate) fn go_docs_for_node(node: Node<'_>, source: &str) -> Vec<String> {
    let Some(parent) = node.parent() else {
        return Vec::new();
    };
    let mut docs = Vec::new();
    let mut cursor = parent.walk();
    for child in parent.children(&mut cursor) {
        if child.end_byte() > node.start_byte() {
            break;
        }
        if matches!(child.kind(), "comment")
            && let Ok(text) = node_text(child, source)
        {
            docs.push(text.trim().to_string());
        } else if child.is_named() && !matches!(child.kind(), "comment") {
            docs.clear();
        }
    }
    docs
}

pub(crate) fn go_visibility(node: Node<'_>, source: &str) -> Option<String> {
    let name = node
        .child_by_field_name("name")
        .and_then(|child| node_text(child, source).ok())
        .map(str::to_string)
        .or_else(|| first_named_child_text(node, source))
        .unwrap_or_default();
    name.chars().next().map(|ch| {
        if ch.is_ascii_uppercase() {
            "exported"
        } else {
            "package"
        }
        .to_string()
    })
}

pub(crate) fn go_is_test_function(relative_path: &str, name: &str) -> bool {
    relative_path.ends_with("_test.go")
        && (name.starts_with("Test") || name.starts_with("Benchmark") || name.starts_with("Fuzz"))
}

pub(crate) fn go_has_ancestor_kind(node: Node<'_>, kind: &str) -> bool {
    let mut parent = node.parent();
    while let Some(current) = parent {
        if current.kind() == kind {
            return true;
        }
        parent = current.parent();
    }
    false
}

pub(crate) fn go_reference_kind(kind: &str) -> Option<ReferenceKind> {
    match kind {
        "identifier" => Some(ReferenceKind::Identifier),
        "type_identifier" | "qualified_type" | "pointer_type" => Some(ReferenceKind::Type),
        "field_identifier" => Some(ReferenceKind::Field),
        _ => None,
    }
}

pub(crate) fn is_go_literal(kind: &str) -> bool {
    matches!(
        kind,
        "raw_string_literal"
            | "interpreted_string_literal"
            | "int_literal"
            | "float_literal"
            | "imaginary_literal"
            | "rune_literal"
            | "true"
            | "false"
            | "nil"
    )
}

pub(crate) fn is_go_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_alphanumeric())
        && !go_keyword_like(text)
}

pub(crate) fn go_keyword_like(text: &str) -> bool {
    matches!(
        text,
        "break"
            | "default"
            | "func"
            | "interface"
            | "select"
            | "case"
            | "defer"
            | "go"
            | "map"
            | "struct"
            | "chan"
            | "else"
            | "goto"
            | "package"
            | "switch"
            | "const"
            | "fallthrough"
            | "if"
            | "range"
            | "type"
            | "continue"
            | "for"
            | "import"
            | "return"
            | "var"
    )
}

pub(crate) fn receiver_from_go_call(target_text: &str) -> Option<String> {
    target_text
        .rsplit_once('.')
        .map(|(receiver, _)| receiver.trim().to_string())
        .filter(|receiver| !receiver.is_empty())
}

pub(crate) fn first_named_child_text(node: Node<'_>, source: &str) -> Option<String> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .next()
        .and_then(|child| node_text(child, source).ok())
        .map(|text| text.trim().to_string())
}

pub(crate) fn last_named_child(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).last()
}
