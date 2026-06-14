use std::collections::HashMap;

use crate::languages::rust::{java_visibility_text, signature_text, symbol_id};
use crate::*;

const SCALA_PACKAGE_ALIAS: &str = "__scala_package__";

pub(crate) fn extract_scala(file: FileRecord, source: &str, tree: &Tree) -> ParsedFile {
    let mut ctx = ExtractContext::new(file.clone(), source);
    let root = tree.root_node();
    record_parse_error_diagnostics(root, &mut ctx);

    visit_scala_node(root, &mut ctx, None, None, false);
    dedup_scala_facts(&mut ctx);
    annotate_companion_objects(&mut ctx);

    let package = ctx
        .imports
        .iter()
        .find(|import| import.alias.as_deref() == Some(SCALA_PACKAGE_ALIAS))
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

fn visit_scala_node(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<(SymbolId, SymbolKind)>,
    owner_symbol: Option<SymbolId>,
    inside_inline: bool,
) {
    if node.is_missing() {
        record_missing_node_diagnostic(node, ctx);
        return;
    }

    match node.kind() {
        "package_clause" => {
            extract_scala_package(node, ctx);
            // package_clause may have a body containing further declarations;
            // continue traversal so nested members emit normally.
            visit_scala_children(node, ctx, parent_symbol, owner_symbol, inside_inline);
            return;
        }
        "import_declaration" | "export_declaration" => {
            extract_scala_import(node, ctx, owner_symbol.clone());
            return;
        }
        _ => {}
    }

    // Multi-binding val/var: `val a, b: Int = ...` declares N symbols on one node.
    // Skip when the enclosing scope is a function body — SemanticDB classifies
    // those bindings as LOCAL and the comparison drops them.
    if matches!(
        node.kind(),
        "val_declaration" | "var_declaration" | "val_definition" | "var_definition"
    ) && !parent_is_callable(parent_symbol.as_ref())
    {
        let symbols = scala_multi_binding_symbols(node, ctx, parent_symbol.as_ref(), inside_inline);
        if symbols.len() > 1 {
            for symbol in symbols {
                ctx.symbols.push(symbol);
            }
            visit_scala_children(node, ctx, parent_symbol, owner_symbol, inside_inline);
            return;
        }
    }

    // Case class / class with primary-constructor parameters: emit Field symbols
    // for each parameter under the parent class.
    if matches!(node.kind(), "class_definition" | "full_enum_case")
        && let Some(symbol) = scala_symbol_from_node(node, ctx, parent_symbol.as_ref())
    {
        let next_inline = inside_inline || symbol_is_inline(&symbol);
        let next_parent = Some((symbol.id.clone(), symbol.kind));
        let next_owner = if symbol.body_span.is_some() {
            Some(symbol.id.clone())
        } else {
            owner_symbol.clone()
        };
        // Emit class-parameter fields before pushing children visit.
        let field_symbols = scala_class_parameter_symbols(node, ctx, &symbol);
        ctx.symbols.push(symbol);
        for field in field_symbols {
            ctx.symbols.push(field);
        }
        visit_scala_children(node, ctx, next_parent, next_owner, next_inline);
        return;
    }

    // Extension definitions: emit each inner function as an extension method.
    if node.kind() == "extension_definition" {
        emit_scala_extension(
            node,
            ctx,
            parent_symbol.as_ref(),
            owner_symbol.clone(),
            inside_inline,
        );
        return;
    }

    if let Some(symbol) = scala_symbol_from_node(node, ctx, parent_symbol.as_ref()) {
        let next_inline = inside_inline || symbol_is_inline(&symbol);
        let next_parent = Some((symbol.id.clone(), symbol.kind));
        let next_owner = if symbol.body_span.is_some() {
            Some(symbol.id.clone())
        } else {
            owner_symbol.clone()
        };
        ctx.symbols.push(symbol);
        visit_scala_children(node, ctx, next_parent, next_owner, next_inline);
        return;
    }

    match node.kind() {
        "call_expression" => {
            extract_scala_call(node, ctx, owner_symbol.clone(), inside_inline);
            visit_scala_children(node, ctx, parent_symbol, owner_symbol, inside_inline);
        }
        "instance_expression" => {
            extract_scala_instance(node, ctx, owner_symbol.clone(), inside_inline);
            visit_scala_children(node, ctx, parent_symbol, owner_symbol, inside_inline);
        }
        "infix_expression" => {
            extract_scala_infix_call(node, ctx, owner_symbol.clone(), inside_inline);
            visit_scala_children(node, ctx, parent_symbol, owner_symbol, inside_inline);
        }
        "field_expression" => {
            extract_scala_field_reference(node, ctx, owner_symbol.clone());
            visit_scala_children(node, ctx, parent_symbol, owner_symbol, inside_inline);
        }
        "stable_identifier" => {
            extract_scala_reference(node, ReferenceKind::Path, ctx, owner_symbol.clone());
            visit_scala_children(node, ctx, parent_symbol, owner_symbol, inside_inline);
        }
        "type_identifier" | "stable_type_identifier" => {
            extract_scala_reference(node, ReferenceKind::Type, ctx, owner_symbol.clone());
        }
        "generic_type" => {
            // Recurse into the type identifier children but don't emit a
            // reference for the wrapper node text itself.
            visit_scala_children(node, ctx, parent_symbol, owner_symbol, inside_inline);
        }
        "annotation" => {
            extract_scala_annotation(node, ctx, owner_symbol.clone());
            visit_scala_children(node, ctx, parent_symbol, owner_symbol, inside_inline);
        }
        "identifier" => {
            // Bare identifiers in expression position are handled by their
            // parent constructs (call, field, stable). Skip to avoid noise.
        }
        kind if is_scala_literal(kind) => {
            extract_body_hit(node, BodyHitKind::Literal, ctx, owner_symbol);
        }
        _ => visit_scala_children(node, ctx, parent_symbol, owner_symbol, inside_inline),
    }
}

fn visit_scala_children(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<(SymbolId, SymbolKind)>,
    owner_symbol: Option<SymbolId>,
    inside_inline: bool,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit_scala_node(
            child,
            ctx,
            parent_symbol.clone(),
            owner_symbol.clone(),
            inside_inline,
        );
    }
}

fn scala_symbol_from_node(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> Option<ParsedSymbol> {
    let node_kind = node.kind();
    // Bindings inside a method/function body are locals; they do not appear in
    // SemanticDB's declaration set and squeezy must not emit them as Const /
    // Field. Suppress for val/var/given/nested-def — the surrounding block is
    // still traversed for call / reference extraction.
    if matches!(
        node_kind,
        "val_definition"
            | "var_definition"
            | "given_definition"
            | "function_definition"
            | "function_declaration"
    ) && parent_is_callable(parent_symbol)
    {
        return None;
    }
    let (kind, is_case_class) = match node_kind {
        "class_definition" => {
            if scala_modifier_present(node, ctx.source, "case") {
                (SymbolKind::Struct, true)
            } else {
                (SymbolKind::Class, false)
            }
        }
        "object_definition" => (SymbolKind::Class, false),
        "package_object" => (SymbolKind::Module, false),
        "trait_definition" => (SymbolKind::Trait, false),
        "enum_definition" => (SymbolKind::Enum, false),
        "simple_enum_case" | "full_enum_case" => (SymbolKind::Variant, false),
        "type_definition" => (SymbolKind::TypeAlias, false),
        "function_definition" | "function_declaration" => (scala_def_kind(parent_symbol), false),
        "given_definition" => (SymbolKind::Const, false),
        "val_definition" => (scala_val_kind(parent_symbol), false),
        "var_definition" => (scala_var_kind(parent_symbol), false),
        _ => return None,
    };

    let name = scala_symbol_name(node, ctx.source)?;
    if name == "_" {
        return None;
    }
    let body = node.child_by_field_name("body");
    let span = span_from_node(node);
    let body_span = body.map(span_from_node);
    let signature_span = signature_span_from_nodes(node, body);
    let signature = signature_text(node, body, ctx.source);
    let parent_id = parent_symbol.map(|(id, _)| id.clone());
    let id = symbol_id(&ctx.file, parent_id.as_ref(), kind, &name, span);
    let arity = if matches!(kind, SymbolKind::Method | SymbolKind::Function) {
        node.child_by_field_name("parameters")
            .map(|params| u8::try_from(named_child_count(params)).unwrap_or(u8::MAX))
    } else {
        None
    };

    let mut attributes = scala_attributes_for_node(node, ctx.source);
    if matches!(node_kind, "object_definition" | "package_object") {
        attributes.push("scala:object".to_string());
    }
    if matches!(
        kind,
        SymbolKind::Class | SymbolKind::Struct | SymbolKind::Trait | SymbolKind::Enum
    ) {
        attributes.extend(scala_inheritance_attributes(node, ctx.source));
        attributes.extend(scala_derives_attributes(node, ctx.source));
        attributes.extend(scala_self_type_attributes(node, ctx.source));
    }
    if matches!(
        kind,
        SymbolKind::Class
            | SymbolKind::Struct
            | SymbolKind::Trait
            | SymbolKind::Enum
            | SymbolKind::Function
            | SymbolKind::Method
            | SymbolKind::TypeAlias
    ) {
        attributes.extend(scala_type_parameter_bound_attributes(node, ctx.source));
    }
    if is_case_class {
        attributes.push("scala:case-class".to_string());
    }
    if node_kind == "given_definition" {
        attributes.push("scala:given".to_string());
        if let Some(target) = scala_given_for(node, ctx.source) {
            attributes.push(format!("scala:given-for:{target}"));
        }
    }
    if node_kind == "type_definition" && scala_modifier_present(node, ctx.source, "opaque") {
        attributes.push("scala:opaque".to_string());
    }
    if scala_modifier_present(node, ctx.source, "inline") {
        attributes.push("scala:inline".to_string());
    }
    if scala_modifier_present(node, ctx.source, "implicit")
        && matches!(kind, SymbolKind::Function | SymbolKind::Method)
    {
        // Scala 2 implicit def → implicit conversion candidate.
        attributes.push("scala:implicit-conversion".to_string());
    }
    if scala_modifier_present(node, ctx.source, "sealed") {
        attributes.push("scala:sealed".to_string());
    }
    if scala_modifier_present(node, ctx.source, "abstract") {
        attributes.push("scala:abstract".to_string());
    }
    if scala_modifier_present(node, ctx.source, "override") {
        attributes.push("scala:override".to_string());
    }
    attributes.sort();
    attributes.dedup();

    let confidence = if matches!(node_kind, "given_definition") {
        Confidence::Partial
    } else {
        Confidence::ExactSyntax
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
        visibility: java_visibility_text(node, ctx.source),
        docs: scala_docs_for_node(node, ctx.source),
        attributes,
        provenance: Provenance::new("tree-sitter-scala", format!("{node_kind} declaration")),
        confidence,
        freshness: Freshness::Fresh,
        arity,
    })
}

fn scala_def_kind(parent_symbol: Option<&(SymbolId, SymbolKind)>) -> SymbolKind {
    if parent_kind_is_container(parent_symbol) {
        SymbolKind::Method
    } else {
        SymbolKind::Function
    }
}

fn scala_val_kind(_parent_symbol: Option<&(SymbolId, SymbolKind)>) -> SymbolKind {
    // Treat all `val` bindings as immutable `Const`. The Java semantic-model
    // shape uses `Const` for `static final` and we mirror it for parity.
    SymbolKind::Const
}

fn scala_var_kind(parent_symbol: Option<&(SymbolId, SymbolKind)>) -> SymbolKind {
    if parent_kind_is_container(parent_symbol) {
        SymbolKind::Field
    } else {
        SymbolKind::Static
    }
}

fn parent_kind_is_container(parent_symbol: Option<&(SymbolId, SymbolKind)>) -> bool {
    matches!(
        parent_symbol.map(|(_, kind)| *kind),
        Some(
            SymbolKind::Class
                | SymbolKind::Struct
                | SymbolKind::Trait
                | SymbolKind::Enum
                | SymbolKind::Module
        )
    )
}

/// Returns true when the immediate enclosing symbol is a function or method
/// (i.e. we are inside a callable body). Used to suppress local val / var /
/// given / nested-def emissions that SemanticDB classifies as LOCAL and the
/// declaration-set comparison therefore must ignore.
fn parent_is_callable(parent_symbol: Option<&(SymbolId, SymbolKind)>) -> bool {
    matches!(
        parent_symbol.map(|(_, kind)| *kind),
        Some(SymbolKind::Function | SymbolKind::Method)
    )
}

fn symbol_is_inline(symbol: &ParsedSymbol) -> bool {
    symbol
        .attributes
        .iter()
        .any(|attribute| attribute == "scala:inline")
}

fn scala_class_parameter_symbols(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent: &ParsedSymbol,
) -> Vec<ParsedSymbol> {
    let mut symbols = Vec::new();
    let mut cursor = node.walk();
    // class_definition has `class_parameters` field (multiple).
    for child in node.children_by_field_name("class_parameters", &mut cursor) {
        let mut param_cursor = child.walk();
        for param in child.named_children(&mut param_cursor) {
            if param.kind() != "class_parameter" {
                continue;
            }
            let name_node = match param.child_by_field_name("name") {
                Some(name) => name,
                None => continue,
            };
            let name = match node_text(name_node, ctx.source) {
                Ok(text) if !text.trim().is_empty() => text.trim().to_string(),
                _ => continue,
            };
            let span = span_from_node(param);
            let id = symbol_id(&ctx.file, Some(&parent.id), SymbolKind::Field, &name, span);
            let mut attributes = Vec::new();
            if let Some(type_node) = param.child_by_field_name("type")
                && let Ok(text) = node_text(type_node, ctx.source)
            {
                attributes.push(format!("type:{}", text.trim()));
            }
            symbols.push(ParsedSymbol {
                id,
                file_id: ctx.file.id.clone(),
                parent_id: Some(parent.id.clone()),
                name,
                kind: SymbolKind::Field,
                language_identity: None,
                span,
                body_span: None,
                signature_span: None,
                signature: node_text(param, ctx.source)
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
                visibility: None,
                docs: Vec::new(),
                attributes,
                provenance: Provenance::new("tree-sitter-scala", "class_parameter declaration"),
                confidence: Confidence::ExactSyntax,
                freshness: Freshness::Fresh,
                arity: None,
            });
        }
    }
    symbols
}

fn scala_multi_binding_symbols(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
    inside_inline: bool,
) -> Vec<ParsedSymbol> {
    let kind = if matches!(node.kind(), "val_declaration" | "val_definition") {
        scala_val_kind(parent_symbol)
    } else {
        scala_var_kind(parent_symbol)
    };
    let mut cursor = node.walk();
    let mut names: Vec<(Node<'_>, String)> = node
        .children_by_field_name("name", &mut cursor)
        .filter_map(|name_node| {
            node_text(name_node, ctx.source)
                .ok()
                .map(|text| (name_node, text.trim().to_string()))
                .filter(|(_, text)| !text.is_empty())
        })
        .collect();
    if names.is_empty()
        && let Some(pattern) = node.child_by_field_name("pattern")
        && pattern.kind() == "identifiers"
    {
        let mut pat_cursor = pattern.walk();
        for child in pattern.named_children(&mut pat_cursor) {
            if child.kind() == "identifier"
                && let Ok(text) = node_text(child, ctx.source)
            {
                let text = text.trim().to_string();
                if !text.is_empty() {
                    names.push((child, text));
                }
            }
        }
    }
    if names.len() < 2 {
        return Vec::new();
    }
    let parent_id = parent_symbol.map(|(id, _)| id.clone());
    let visibility = java_visibility_text(node, ctx.source);
    let signature = signature_text(node, None, ctx.source);
    let attributes = scala_attributes_for_node(node, ctx.source);
    let confidence = if inside_inline {
        Confidence::Heuristic
    } else {
        Confidence::ExactSyntax
    };
    let mut symbols = Vec::with_capacity(names.len());
    for (name_node, name) in names {
        let span = span_from_node(name_node);
        let id = symbol_id(&ctx.file, parent_id.as_ref(), kind, &name, span);
        symbols.push(ParsedSymbol {
            id,
            file_id: ctx.file.id.clone(),
            parent_id: parent_id.clone(),
            name,
            kind,
            language_identity: None,
            span,
            body_span: None,
            signature_span: None,
            signature: signature.clone(),
            visibility: visibility.clone(),
            docs: Vec::new(),
            attributes: attributes.clone(),
            provenance: Provenance::new(
                "tree-sitter-scala",
                format!("{} declaration", node.kind()),
            ),
            confidence,
            freshness: Freshness::Fresh,
            arity: None,
        });
    }
    symbols
}

fn emit_scala_extension(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
    owner_symbol: Option<SymbolId>,
    inside_inline: bool,
) {
    let receiver_type = scala_extension_receiver_type(node, ctx.source);
    let is_generic = scala_extension_has_type_params(node, ctx.source);

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if !matches!(child.kind(), "function_definition" | "function_declaration") {
            // Recurse through unrelated children (annotations, etc.).
            visit_scala_node(
                child,
                ctx,
                parent_symbol.cloned(),
                owner_symbol.clone(),
                inside_inline,
            );
            continue;
        }
        let Some(mut symbol) = scala_symbol_from_node(child, ctx, parent_symbol) else {
            continue;
        };
        symbol.kind = SymbolKind::Function;
        symbol.attributes.push("scala:extension".to_string());
        if let Some(receiver) = receiver_type.clone() {
            symbol.language_identity = Some(receiver.clone());
            symbol
                .attributes
                .push(format!("scala:extension-receiver:{receiver}"));
        }
        symbol.attributes.sort();
        symbol.attributes.dedup();
        if is_generic {
            symbol.confidence = Confidence::Partial;
        }
        let next_parent = Some((symbol.id.clone(), symbol.kind));
        let next_owner = if symbol.body_span.is_some() {
            Some(symbol.id.clone())
        } else {
            owner_symbol.clone()
        };
        let next_inline = inside_inline || symbol_is_inline(&symbol);
        ctx.symbols.push(symbol);
        visit_scala_children(child, ctx, next_parent, next_owner, next_inline);
    }
}

fn scala_extension_receiver_type(node: Node<'_>, source: &str) -> Option<String> {
    let mut cursor = node.walk();
    let parameters = node
        .children_by_field_name("parameters", &mut cursor)
        .next()?;
    let mut param_cursor = parameters.walk();
    for param in parameters.named_children(&mut param_cursor) {
        if !matches!(param.kind(), "parameter" | "class_parameter") {
            continue;
        }
        if let Some(type_node) = param.child_by_field_name("type")
            && let Ok(text) = node_text(type_node, source)
        {
            return Some(text.trim().to_string());
        }
    }
    None
}

fn scala_extension_has_type_params(node: Node<'_>, source: &str) -> bool {
    if let Some(type_params) = node.child_by_field_name("type_parameters")
        && let Ok(text) = node_text(type_params, source)
    {
        return !text.trim().is_empty();
    }
    false
}

fn scala_node_name(node: Node<'_>, source: &str) -> Option<String> {
    let name_node = node.child_by_field_name("name")?;
    let text = node_text(name_node, source).ok()?.trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

fn scala_symbol_name(node: Node<'_>, source: &str) -> Option<String> {
    match node.kind() {
        "val_definition" | "var_definition" => scala_pattern_first_name(node, source),
        "given_definition" => match scala_node_name(node, source) {
            Some(name) => Some(name),
            None => scala_anonymous_given_name(node, source),
        },
        _ => scala_node_name(node, source),
    }
}

fn scala_pattern_first_name(node: Node<'_>, source: &str) -> Option<String> {
    // val/var have `pattern` field of `_pattern | identifiers`. For simple
    // binders the pattern is itself an identifier or a `capture_pattern`.
    let pattern = node.child_by_field_name("pattern")?;
    scala_first_identifier(pattern, source)
}

fn scala_first_identifier(node: Node<'_>, source: &str) -> Option<String> {
    if node.kind() == "identifier" {
        return node_text(node, source)
            .ok()
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty());
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(name) = scala_first_identifier(child, source) {
            return Some(name);
        }
    }
    None
}

fn scala_anonymous_given_name(node: Node<'_>, source: &str) -> Option<String> {
    // Anonymous given derives a synthetic name from the return type so the
    // symbol still has a stable identifier. e.g. `given Ordering[Int] = ...`
    // becomes `given_OrderingInt`.
    let return_type = node.child_by_field_name("return_type")?;
    let raw = node_text(return_type, source).ok()?.trim().to_string();
    if raw.is_empty() {
        return None;
    }
    let mut sanitized = String::from("given_");
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            sanitized.push(ch);
        }
    }
    if sanitized == "given_" {
        None
    } else {
        Some(sanitized)
    }
}

/// Emit ordered inheritance attributes for a class/trait/enum: the first
/// supertype in the `extends` clause is the superclass (`base:`), every
/// subsequent `with`-mixin or comma-separated parent is a trait mixin
/// (`mixin:`). Order is significant — Scala loses no information at parse time
/// about which parent is the primary super and which are mixed-in traits, so we
/// must not sort before splitting. The `compound_type` form (`extends A with B`
/// collapsed into one node) separates `base` from `extra` structurally.
fn scala_inheritance_attributes(node: Node<'_>, source: &str) -> Vec<String> {
    let Some(extend) = node.child_by_field_name("extend") else {
        return Vec::new();
    };
    let mut names = Vec::new();
    scala_collect_constructor_applications(extend, source, &mut names);
    let mut attributes = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (index, name) in names.into_iter().enumerate() {
        // First parent is the superclass; the rest are mixed-in traits. A
        // single type still emits a `base:` so the generic inheritance pass can
        // lower it to an Extends/Implements edge.
        let prefix = if index == 0 { "base" } else { "mixin" };
        let attribute = format!("{prefix}:{name}");
        if seen.insert(attribute.clone()) {
            attributes.push(attribute);
        }
    }
    attributes
}

/// Walk the `extends_clause`'s `type` field, which is a `_constructor_applications`
/// (either comma-separated or `with`-separated constructor applications), pushing
/// each parent type's leaf name in source order. A `compound_type` constructor
/// application contributes its `base` first, then each `extra` mixin.
fn scala_collect_constructor_applications(node: Node<'_>, source: &str, names: &mut Vec<String>) {
    let mut cursor = node.walk();
    for child in node.children_by_field_name("type", &mut cursor) {
        scala_collect_ordered_type_names(child, source, names);
    }
}

/// Push the ordered leaf type names contributed by a single constructor
/// application. `compound_type` is handled specially so the structural
/// base/extra split is preserved in emission order; everything else falls back
/// to the recursive leaf collector.
fn scala_collect_ordered_type_names(node: Node<'_>, source: &str, names: &mut Vec<String>) {
    if node.kind() == "compound_type" {
        if let Some(base) = node.child_by_field_name("base") {
            collect_scala_type_names(base, source, names);
        }
        let mut cursor = node.walk();
        for extra in node.children_by_field_name("extra", &mut cursor) {
            collect_scala_type_names(extra, source, names);
        }
        return;
    }
    collect_scala_type_names(node, source, names);
}

/// Read the Scala 3 `derives` clause, recording each derived typeclass as a
/// `derives:<Typeclass>` attribute so "which types derive X" is a structured
/// query rather than an undifferentiated `Type` mention.
fn scala_derives_attributes(node: Node<'_>, source: &str) -> Vec<String> {
    let Some(derive) = node.child_by_field_name("derive") else {
        return Vec::new();
    };
    let mut names = Vec::new();
    let mut cursor = derive.walk();
    for child in derive.children_by_field_name("type", &mut cursor) {
        collect_scala_type_names(child, source, &mut names);
    }
    let mut attributes = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for name in names {
        let attribute = format!("derives:{name}");
        if seen.insert(attribute.clone()) {
            attributes.push(attribute);
        }
    }
    attributes
}

/// Record cake-pattern self-type constraints (`self: T with U =>`) as
/// `scala:self-type:<T>` attributes on the enclosing trait/class so required
/// mixins are enumerable. The `self_type` node lives directly inside the
/// `template_body`; its leading self-identifier is skipped and only the ascribed
/// types are recorded.
fn scala_self_type_attributes(node: Node<'_>, source: &str) -> Vec<String> {
    let mut attributes = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut cursor = node.walk();
    for body in node.children_by_field_name("body", &mut cursor) {
        let mut body_cursor = body.walk();
        for child in body.named_children(&mut body_cursor) {
            if child.kind() != "self_type" {
                continue;
            }
            let mut names = Vec::new();
            let mut self_cursor = child.walk();
            for component in child.named_children(&mut self_cursor) {
                // Skip the leading self-identifier (`self`); only the ascribed
                // types after the `:` are required mixins.
                if matches!(component.kind(), "identifier" | "operator_identifier") {
                    continue;
                }
                collect_scala_type_names(component, source, &mut names);
            }
            for name in names {
                let attribute = format!("scala:self-type:{name}");
                if seen.insert(attribute.clone()) {
                    attributes.push(attribute);
                }
            }
        }
    }
    attributes
}

/// Extract type-parameter bounds as structured attributes:
/// - context (`T: Ordering`) and view (`T <% X`) bounds → `bound:<Typeclass>`
/// - upper bounds (`T <: Foo`) → `upper-bound:<Foo>`
/// - lower bounds (`T >: Bar`) → `lower-bound:<Bar>`
///
/// Bounds appear on the `type_parameters` node directly (the `bound` field) and
/// on each variant param child (`covariant_type_parameter` /
/// `contravariant_type_parameter`). For functions the `type_parameters` node is
/// one of the (multiple) `parameters` field entries; for classes/traits/enums
/// it is the dedicated `type_parameters` field.
fn scala_type_parameter_bound_attributes(node: Node<'_>, source: &str) -> Vec<String> {
    let mut attributes = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for type_params in scala_type_parameter_nodes(node) {
        scala_collect_type_parameter_bounds(type_params, source, &mut attributes, &mut seen);
    }
    attributes
}

/// Collect every `type_parameters` node attached to a declaration: the
/// dedicated `type_parameters` field (classes/traits/enums/type aliases) and any
/// `type_parameters` appearing among a function's `parameters` field entries.
fn scala_type_parameter_nodes<'a>(node: Node<'a>) -> Vec<Node<'a>> {
    let mut nodes = Vec::new();
    if let Some(direct) = node.child_by_field_name("type_parameters") {
        nodes.push(direct);
    }
    let mut cursor = node.walk();
    for child in node.children_by_field_name("parameters", &mut cursor) {
        if child.kind() == "type_parameters" {
            nodes.push(child);
        }
    }
    nodes
}

/// Walk a `type_parameters` node, mapping its `bound` field entries and the
/// bounds nested in variant param children to structured attributes.
fn scala_collect_type_parameter_bounds(
    type_params: Node<'_>,
    source: &str,
    attributes: &mut Vec<String>,
    seen: &mut HashSet<String>,
) {
    let mut cursor = type_params.walk();
    for bound in type_params.children_by_field_name("bound", &mut cursor) {
        scala_push_bound_attribute(bound, source, attributes, seen);
    }
    let mut child_cursor = type_params.walk();
    for child in type_params.named_children(&mut child_cursor) {
        if matches!(
            child.kind(),
            "covariant_type_parameter" | "contravariant_type_parameter"
        ) {
            let mut inner = child.walk();
            for bound in child.children_by_field_name("bound", &mut inner) {
                scala_push_bound_attribute(bound, source, attributes, seen);
            }
        }
    }
}

/// Map a single bound node to its attribute prefix and push one attribute per
/// constraint type. Unknown bound kinds are ignored.
fn scala_push_bound_attribute(
    bound: Node<'_>,
    source: &str,
    attributes: &mut Vec<String>,
    seen: &mut HashSet<String>,
) {
    let prefix = match bound.kind() {
        // Context/view bounds name a typeclass the parameter must satisfy.
        "context_bound" | "view_bound" => "bound",
        "upper_bound" => "upper-bound",
        "lower_bound" => "lower-bound",
        _ => return,
    };
    let Some(type_node) = bound.child_by_field_name("type") else {
        return;
    };
    // Record the head type name (the typeclass / bound type itself), not the
    // nested type arguments — `T: Numeric` and `T <: Comparable[T]` should
    // record `Numeric` / `Comparable`, not the parameter `T`.
    let Some(name) = scala_head_type_name(type_node, source) else {
        return;
    };
    let attribute = format!("{prefix}:{name}");
    if seen.insert(attribute.clone()) {
        attributes.push(attribute);
    }
}

/// The head (constructor) type name of a type node: for a plain
/// `type_identifier` it is the identifier; for a `generic_type` /
/// `applied_constructor_type` it is the applied constructor, ignoring the type
/// arguments. Returns `None` for shapes with no leading nominal type.
fn scala_head_type_name(node: Node<'_>, source: &str) -> Option<String> {
    if matches!(node.kind(), "type_identifier" | "stable_type_identifier") {
        return node_text(node, source)
            .ok()
            .and_then(scala_type_name_from_text);
    }
    // `generic_type` / `applied_constructor_type` (and any other wrapper) defer
    // to their first child that yields a nominal head type.
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find_map(|child| scala_head_type_name(child, source))
}

fn collect_scala_type_names(node: Node<'_>, source: &str, names: &mut Vec<String>) {
    match node.kind() {
        "type_identifier" | "stable_type_identifier" => {
            if let Ok(text) = node_text(node, source)
                && let Some(name) = scala_type_name_from_text(text)
            {
                names.push(name);
            }
            return;
        }
        "generic_type" | "applied_constructor_type" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect_scala_type_names(child, source, names);
            }
            return;
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_scala_type_names(child, source, names);
    }
}

fn scala_type_name_from_text(text: &str) -> Option<String> {
    let clean = text.split('[').next().unwrap_or(text).trim();
    if clean.is_empty() {
        None
    } else {
        Some(clean.to_string())
    }
}

fn scala_given_for(node: Node<'_>, source: &str) -> Option<String> {
    let return_type = node.child_by_field_name("return_type")?;
    let text = node_text(return_type, source).ok()?.trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

fn extract_scala_package(node: Node<'_>, ctx: &mut ExtractContext<'_>) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let raw = node_text(name_node, ctx.source).unwrap_or_default();
    let path = raw.trim().trim_end_matches(';').trim().to_string();
    if path.is_empty() {
        return;
    }
    ctx.imports.push(ParsedImport {
        file_id: ctx.file.id.clone(),
        owner_id: None,
        path,
        alias: Some(SCALA_PACKAGE_ALIAS.to_string()),
        is_glob: false,
        is_reexport: true,
        is_static: false,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-scala", "package declaration"),
        kind: ImportKind::Unspecified,
        imported_name: None,
        is_global: false,
    });
}

fn extract_scala_import(node: Node<'_>, ctx: &mut ExtractContext<'_>, owner_id: Option<SymbolId>) {
    let prefix = scala_import_prefix(node, ctx.source);
    let selectors = scala_import_selectors(node, ctx.source);
    let span = span_from_node(node);
    let is_export = node.kind() == "export_declaration";

    if selectors.is_empty() {
        if prefix.is_empty() {
            return;
        }
        let is_glob = prefix == "_" || prefix.ends_with(".*");
        let (path, kind, imported_name) = if is_glob {
            (prefix.clone(), ImportKind::Wildcard, None)
        } else {
            let leaf = last_path_segment(&prefix);
            (prefix.clone(), ImportKind::Named, Some(leaf))
        };
        ctx.imports.push(ParsedImport {
            file_id: ctx.file.id.clone(),
            owner_id,
            path,
            alias: None,
            is_glob,
            is_reexport: is_export,
            is_static: false,
            span,
            provenance: Provenance::new("tree-sitter-scala", "import declaration"),
            kind,
            imported_name,
            is_global: false,
        });
        return;
    }

    for selector in selectors {
        let ScalaSelector {
            name,
            alias,
            is_wildcard,
            is_given,
        } = selector;
        let path = if prefix.is_empty() {
            name.clone()
        } else if is_wildcard && name == "*" {
            format!("{prefix}.*")
        } else if name.is_empty() {
            prefix.clone()
        } else {
            format!("{prefix}.{name}")
        };
        let kind = if is_wildcard {
            ImportKind::Wildcard
        } else {
            ImportKind::Named
        };
        let imported_name = if is_wildcard || name.is_empty() {
            None
        } else {
            Some(name.clone())
        };
        let mut alias_value = alias;
        if is_given && alias_value.is_none() {
            // Encode `import a.b.given` as an attribute sentinel in alias since
            // ParsedImport lacks a structured attributes field.
            alias_value = Some("__scala_import_given__".to_string());
        }
        ctx.imports.push(ParsedImport {
            file_id: ctx.file.id.clone(),
            owner_id: owner_id.clone(),
            path,
            alias: alias_value,
            is_glob: is_wildcard,
            is_reexport: is_export,
            is_static: false,
            span,
            provenance: Provenance::new("tree-sitter-scala", "import declaration"),
            kind,
            imported_name,
            is_global: false,
        });
    }
}

#[derive(Debug)]
struct ScalaSelector {
    name: String,
    alias: Option<String>,
    is_wildcard: bool,
    is_given: bool,
}

fn scala_import_prefix(node: Node<'_>, source: &str) -> String {
    let mut cursor = node.walk();
    let mut segments = Vec::new();
    for child in node.children_by_field_name("path", &mut cursor) {
        if matches!(child.kind(), "identifier" | "operator_identifier")
            && let Ok(text) = node_text(child, source)
        {
            segments.push(text.trim().to_string());
        }
    }
    segments.join(".")
}

fn scala_import_selectors(node: Node<'_>, source: &str) -> Vec<ScalaSelector> {
    let mut selectors = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "namespace_selectors" => {
                let mut inner = child.walk();
                for selector_node in child.named_children(&mut inner) {
                    if let Some(selector) = scala_selector_from_node(selector_node, source) {
                        selectors.push(selector);
                    }
                }
            }
            "namespace_wildcard" => {
                let text = node_text(child, source).unwrap_or_default().trim();
                let is_given_marker = text == "given";
                selectors.push(ScalaSelector {
                    name: if is_given_marker {
                        String::new()
                    } else {
                        "*".to_string()
                    },
                    alias: None,
                    is_wildcard: true,
                    is_given: is_given_marker,
                });
            }
            "as_renamed_identifier" | "arrow_renamed_identifier" => {
                if let Some(selector) = scala_selector_from_node(child, source) {
                    selectors.push(selector);
                }
            }
            _ => {}
        }
    }
    selectors
}

fn scala_selector_from_node(node: Node<'_>, source: &str) -> Option<ScalaSelector> {
    match node.kind() {
        "identifier" | "operator_identifier" => {
            let text = node_text(node, source).ok()?.trim().to_string();
            if text.is_empty() {
                return None;
            }
            let is_given = text == "given";
            Some(ScalaSelector {
                name: if is_given { String::new() } else { text },
                alias: None,
                is_wildcard: false,
                is_given,
            })
        }
        "namespace_wildcard" | "wildcard" => {
            let text = node_text(node, source).unwrap_or_default().trim();
            let is_given_marker = text == "given";
            Some(ScalaSelector {
                name: if is_given_marker {
                    String::new()
                } else {
                    "*".to_string()
                },
                alias: None,
                is_wildcard: true,
                is_given: is_given_marker,
            })
        }
        "as_renamed_identifier" | "arrow_renamed_identifier" => {
            let name_node = node.child_by_field_name("name")?;
            let alias_node = node.child_by_field_name("alias")?;
            let name = node_text(name_node, source).ok()?.trim().to_string();
            let alias = node_text(alias_node, source).ok()?.trim().to_string();
            if name.is_empty() {
                return None;
            }
            let alias_is_wild = alias == "_";
            Some(ScalaSelector {
                name,
                alias: if alias_is_wild { None } else { Some(alias) },
                is_wildcard: alias_is_wild,
                is_given: false,
            })
        }
        // `{given Ordering[Int]}` — the inner type identifier is the imported
        // given. Treat the leaf identifier as the named import target.
        "type_identifier" | "stable_type_identifier" => {
            let text = node_text(node, source).ok()?.trim().to_string();
            if text.is_empty() {
                return None;
            }
            Some(ScalaSelector {
                name: text,
                alias: Some("__scala_import_given__".to_string()),
                is_wildcard: false,
                is_given: true,
            })
        }
        "generic_type" | "applied_constructor_type" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if matches!(child.kind(), "type_identifier" | "stable_type_identifier") {
                    return scala_selector_from_node(child, source);
                }
            }
            None
        }
        _ => None,
    }
}

fn extract_scala_call(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
    inside_inline: bool,
) {
    let raw = node_text(node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if raw.is_empty() {
        return;
    }
    let function = node.child_by_field_name("function");
    let (name, receiver) = match function {
        Some(fun) => scala_call_function_parts(fun, ctx.source),
        None => (
            method_name_from_text(&raw),
            receiver_from_method_text(&raw, &raw),
        ),
    };
    if name.is_empty() {
        return;
    }
    let arity = node
        .child_by_field_name("arguments")
        .map(named_child_count)
        .unwrap_or_default();
    let kind = if receiver.is_some() {
        ParsedCallKind::Method
    } else {
        ParsedCallKind::Direct
    };
    let confidence = if inside_inline {
        Confidence::Heuristic
    } else if matches!(kind, ParsedCallKind::Method) {
        Confidence::CandidateSet
    } else {
        Confidence::Heuristic
    };
    ctx.calls.push(ParsedCall {
        file_id: ctx.file.id.clone(),
        caller_id: owner_id.clone(),
        name,
        target_text: raw,
        receiver,
        arity,
        kind,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-scala", "call_expression"),
        confidence,
    });
    extract_body_hit(node, BodyHitKind::Call, ctx, owner_id);
}

fn scala_call_function_parts(node: Node<'_>, source: &str) -> (String, Option<String>) {
    match node.kind() {
        "field_expression" => {
            let receiver = node
                .child_by_field_name("value")
                .and_then(|child| node_text(child, source).ok())
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty());
            let name = node
                .child_by_field_name("field")
                .and_then(|child| node_text(child, source).ok())
                .map(|text| text.trim().to_string())
                .unwrap_or_default();
            (name, receiver)
        }
        "identifier" | "operator_identifier" => {
            let text = node_text(node, source)
                .unwrap_or_default()
                .trim()
                .to_string();
            (text, None)
        }
        "generic_function" => {
            let mut cursor = node.walk();
            if let Some(first) = node.named_children(&mut cursor).next() {
                scala_call_function_parts(first, source)
            } else {
                let raw = node_text(node, source)
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                (method_name_from_text(&raw), None)
            }
        }
        _ => {
            let raw = node_text(node, source)
                .unwrap_or_default()
                .trim()
                .to_string();
            (
                method_name_from_text(&raw),
                receiver_from_method_text(&raw, &raw),
            )
        }
    }
}

fn extract_scala_instance(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
    inside_inline: bool,
) {
    // `instance_expression` is `new T(...)` - target type is the first named
    // child that is a type-like node.
    let mut cursor = node.walk();
    let type_node = node.named_children(&mut cursor).find(|child| {
        matches!(
            child.kind(),
            "type_identifier"
                | "stable_type_identifier"
                | "generic_type"
                | "applied_constructor_type"
                | "compound_type"
        )
    });
    let target_text = type_node
        .and_then(|child| node_text(child, ctx.source).ok())
        .map(|text| text.trim().to_string())
        .unwrap_or_default();
    if target_text.is_empty() {
        return;
    }
    let arity = node
        .child_by_field_name("arguments")
        .map(named_child_count)
        .unwrap_or_default();
    // `instance_expression` calls are inherently heuristic — we don't know
    // which constructor overload the new expression dispatches to. Inline
    // bodies can't lower this further.
    let _ = inside_inline;
    let confidence = Confidence::Heuristic;
    ctx.calls.push(ParsedCall {
        file_id: ctx.file.id.clone(),
        caller_id: owner_id.clone(),
        name: last_path_segment(&target_text),
        target_text: target_text.clone(),
        receiver: receiver_from_direct_call(&target_text),
        arity,
        kind: ParsedCallKind::Direct,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-scala", "instance_expression"),
        confidence,
    });
    ctx.body_hits.push(BodyHit {
        file_id: ctx.file.id.clone(),
        owner_id,
        text: target_text,
        kind: BodyHitKind::Call,
        span: span_from_node(node),
    });
}

fn extract_scala_infix_call(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
    inside_inline: bool,
) {
    let Some(operator) = node.child_by_field_name("operator") else {
        return;
    };
    // Only treat identifier operators as method calls; punctuation operators
    // (`+`, `-`, `==`) are too noisy to track.
    if operator.kind() != "identifier" {
        return;
    }
    let name = match node_text(operator, ctx.source) {
        Ok(text) if !text.trim().is_empty() => text.trim().to_string(),
        _ => return,
    };
    let receiver = node
        .child_by_field_name("left")
        .and_then(|left| node_text(left, ctx.source).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());
    let raw = node_text(node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    // Infix-method calls (`a max b`) are heuristic regardless of caller
    // because Scala leaves operator overload resolution to the type checker.
    let _ = inside_inline;
    let confidence = Confidence::Heuristic;
    ctx.calls.push(ParsedCall {
        file_id: ctx.file.id.clone(),
        caller_id: owner_id,
        name,
        target_text: raw,
        receiver,
        arity: 1,
        kind: ParsedCallKind::Method,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-scala", "infix_expression"),
        confidence,
    });
}

fn extract_scala_field_reference(
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
    let span = span_from_node(node);
    ctx.references.push(ParsedReference {
        file_id: ctx.file.id.clone(),
        owner_id: owner_id.clone(),
        text: text.clone(),
        kind: ReferenceKind::Field,
        span,
        provenance: Provenance::new("tree-sitter-scala", "field_expression reference"),
    });
    ctx.body_hits.push(BodyHit {
        file_id: ctx.file.id.clone(),
        owner_id: owner_id.clone(),
        text: text.clone(),
        kind: BodyHitKind::Path,
        span,
    });
    // Scala uniform-access: `value.method` with no arg list is a method call
    // when `method` resolves to a parameterless `def`. Emit a synthetic
    // `ParsedCall` with `Heuristic` confidence so cross-file/companion-object
    // resolvers can land it. The resolver picks the right interpretation by
    // looking at the candidate symbol's kind; a field-only target keeps the
    // edge as a candidate set.
    let field_node = node.child_by_field_name("field");
    let receiver_node = node.child_by_field_name("value");
    let Some(field_node) = field_node else {
        return;
    };
    let Some(receiver_node) = receiver_node else {
        return;
    };
    let name = node_text(field_node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if name.is_empty() {
        return;
    }
    let receiver = node_text(receiver_node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if receiver.is_empty() {
        return;
    }
    // Emit an additional bare-name reference for the selector so
    // `reference_search("Alice")` against `Names.Alice` resolves the leaf
    // identifier. Without this the lookup only matches the qualified text
    // `Names.Alice` and bench queries against the enum case (or any short
    // member name) miss.
    let field_span = span_from_node(field_node);
    ctx.references.push(ParsedReference {
        file_id: ctx.file.id.clone(),
        owner_id: owner_id.clone(),
        text: name.clone(),
        kind: ReferenceKind::Identifier,
        span: field_span,
        provenance: Provenance::new("tree-sitter-scala", "field_expression selector"),
    });
    // Skip the `Package.Type` style references that aren't calls. Heuristic:
    // if the receiver is an UpperCamelCase identifier path AND the field is
    // also UpperCamelCase, it's a nested type/access. We still emit the call
    // for these because parameterless companion-object members like
    // `Greeter.default` need it; the resolver disambiguates by candidate kind.
    ctx.calls.push(ParsedCall {
        file_id: ctx.file.id.clone(),
        caller_id: owner_id,
        name,
        target_text: text,
        receiver: Some(receiver),
        arity: 0,
        kind: ParsedCallKind::Method,
        span,
        provenance: Provenance::new("tree-sitter-scala", "field_expression call"),
        confidence: Confidence::Heuristic,
    });
}

fn extract_scala_reference(
    node: Node<'_>,
    kind: ReferenceKind,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let text = node_text(node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if text.is_empty() || is_scala_reserved(&text) {
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
        provenance: Provenance::new("tree-sitter-scala", format!("{} reference", node.kind())),
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

fn extract_scala_annotation(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let raw = node_text(name_node, ctx.source).unwrap_or_default().trim();
    if raw.is_empty() {
        return;
    }
    let text = raw.trim_start_matches('@').to_string();
    let span = span_from_node(name_node);
    ctx.references.push(ParsedReference {
        file_id: ctx.file.id.clone(),
        owner_id: owner_id.clone(),
        text: text.clone(),
        kind: ReferenceKind::Attribute,
        span,
        provenance: Provenance::new("tree-sitter-scala", "annotation reference"),
    });
    ctx.body_hits.push(BodyHit {
        file_id: ctx.file.id.clone(),
        owner_id,
        text,
        kind: BodyHitKind::Attribute,
        span,
    });
}

fn scala_attributes_for_node(node: Node<'_>, source: &str) -> Vec<String> {
    let mut attributes = Vec::new();
    let Some(modifiers) = scala_modifiers_node(node) else {
        return attributes;
    };
    let mut cursor = modifiers.walk();
    for child in modifiers.named_children(&mut cursor) {
        if child.kind() == "annotation"
            && let Some(name_node) = child.child_by_field_name("name")
            && let Ok(text) = node_text(name_node, source)
        {
            attributes.push(format!("scala:annotation:{}", text.trim()));
        }
    }
    attributes
}

fn scala_modifiers_node(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(|child| child.kind() == "modifiers")
}

fn scala_modifier_present(node: Node<'_>, source: &str, keyword: &str) -> bool {
    // Walk only the prefix tokens before the introducing keyword
    // (`class`, `def`, `val`, `var`, `trait`, `object`, `enum`, `type`,
    // `given`, `extension`, `package`). Stop once that keyword is seen so we
    // never recurse into the body and pick up a stray identifier.
    let introducer = match node.kind() {
        "class_definition" => Some("class"),
        "object_definition" => Some("object"),
        "trait_definition" => Some("trait"),
        "enum_definition" => Some("enum"),
        "function_definition" | "function_declaration" => Some("def"),
        "given_definition" => Some("given"),
        "extension_definition" => Some("extension"),
        "type_definition" => Some("type"),
        "val_definition" | "val_declaration" => Some("val"),
        "var_definition" | "var_declaration" => Some("var"),
        "package_object" => Some("package"),
        "simple_enum_case" | "full_enum_case" => Some("case"),
        _ => None,
    };
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(token) = introducer
            && child.kind() == token
        {
            break;
        }
        if child.kind() == keyword {
            return true;
        }
        // Recognize composite modifier wrapper nodes (e.g. `opaque_modifier`,
        // `inline_modifier`, `transparent_modifier`).
        if let Some(stripped) = child.kind().strip_suffix("_modifier")
            && stripped == keyword
        {
            return true;
        }
        if child.kind() == "modifiers" {
            let mut inner = child.walk();
            for grandchild in child.children(&mut inner) {
                if grandchild.kind() == keyword {
                    return true;
                }
                if let Some(stripped) = grandchild.kind().strip_suffix("_modifier")
                    && stripped == keyword
                {
                    return true;
                }
                if grandchild.kind() == "access_modifier"
                    && let Ok(text) = node_text(grandchild, source)
                    && text.trim() == keyword
                {
                    return true;
                }
            }
        }
    }
    false
}

fn scala_docs_for_node(node: Node<'_>, source: &str) -> Vec<String> {
    let mut docs = Vec::new();
    let Some(mut previous) = node.prev_named_sibling() else {
        return docs;
    };
    while matches!(previous.kind(), "comment" | "block_comment") {
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

fn is_scala_literal(kind: &str) -> bool {
    matches!(
        kind,
        "string"
            | "interpolated_string"
            | "interpolated_string_expression"
            | "integer_literal"
            | "floating_point_literal"
            | "boolean_literal"
            | "character_literal"
            | "null_literal"
            | "unit"
    )
}

fn is_scala_reserved(text: &str) -> bool {
    matches!(
        text,
        "abstract"
            | "case"
            | "catch"
            | "class"
            | "def"
            | "do"
            | "else"
            | "enum"
            | "export"
            | "extends"
            | "extension"
            | "false"
            | "final"
            | "finally"
            | "for"
            | "given"
            | "if"
            | "implicit"
            | "import"
            | "inline"
            | "lazy"
            | "match"
            | "new"
            | "null"
            | "object"
            | "opaque"
            | "open"
            | "override"
            | "package"
            | "private"
            | "protected"
            | "return"
            | "sealed"
            | "super"
            | "then"
            | "this"
            | "throw"
            | "trait"
            | "transparent"
            | "true"
            | "try"
            | "type"
            | "using"
            | "val"
            | "var"
            | "while"
            | "with"
            | "yield"
    )
}

fn dedup_scala_facts(ctx: &mut ExtractContext<'_>) {
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
}

fn annotate_companion_objects(ctx: &mut ExtractContext<'_>) {
    // Group top-level (file-owned) class-like and object-like symbols by name,
    // then attach companion-of attributes.
    let mut classes: HashMap<String, usize> = HashMap::new();
    let mut objects: HashMap<String, usize> = HashMap::new();
    for (index, symbol) in ctx.symbols.iter().enumerate() {
        let is_top_level = symbol.parent_id.is_none();
        if !is_top_level {
            continue;
        }
        let is_object_like = symbol
            .attributes
            .iter()
            .any(|attribute| attribute == "scala:object");
        if is_object_like {
            objects.insert(symbol.name.clone(), index);
        } else if matches!(
            symbol.kind,
            SymbolKind::Class | SymbolKind::Struct | SymbolKind::Trait | SymbolKind::Enum
        ) {
            classes.insert(symbol.name.clone(), index);
        }
    }
    for (name, class_index) in classes {
        let Some(object_index) = objects.get(&name).copied() else {
            continue;
        };
        if let Some(symbol) = ctx.symbols.get_mut(class_index) {
            symbol
                .attributes
                .push(format!("scala:companion-object:{name}"));
            symbol.attributes.sort();
            symbol.attributes.dedup();
        }
        if let Some(symbol) = ctx.symbols.get_mut(object_index) {
            symbol.attributes.push(format!("scala:companion-of:{name}"));
            symbol.attributes.sort();
            symbol.attributes.dedup();
        }
    }
}

#[cfg(test)]
#[path = "scala_tests.rs"]
mod tests;
