use std::collections::HashSet;

use crate::languages::rust::*;
use crate::*;

pub(crate) fn extract_csharp(file: FileRecord, source: &str, tree: &Tree) -> ParsedFile {
    let mut ctx = ExtractContext::new(file.clone(), source);
    let root = tree.root_node();
    record_parse_error_diagnostics(root, &mut ctx);

    let mut scope = CsharpScope::default();
    visit_csharp_node(root, &mut ctx, None, None, &mut scope);
    dedup_csharp_facts(&mut ctx);

    ParsedFile {
        file,
        // Surface the file's dominant namespace as the `package` field, the
        // same way the Go extractor surfaces the file's `package` declaration.
        // File-scoped `namespace Foo;` and the first encountered braced
        // namespace both work; if neither is present this stays `None`.
        package: scope.top_namespace.clone(),
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

#[derive(Debug, Default, Clone)]
struct CsharpScope {
    namespace_segments: Vec<String>,
    top_namespace: Option<String>,
    type_path: Vec<String>,
    callable_path: Vec<String>,
}

impl CsharpScope {
    pub(crate) fn current_namespace(&self) -> Option<String> {
        if self.namespace_segments.is_empty() {
            None
        } else {
            Some(self.namespace_segments.join("."))
        }
    }

    pub(crate) fn record_namespace(&mut self) {
        if self.top_namespace.is_some() {
            return;
        }
        if let Some(namespace) = self.current_namespace() {
            self.top_namespace = Some(namespace);
        }
    }
}

fn visit_csharp_node(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<(SymbolId, SymbolKind)>,
    owner_symbol: Option<SymbolId>,
    scope: &mut CsharpScope,
) {
    if node.is_missing() {
        record_missing_node_diagnostic(node, ctx);
        return;
    }

    let kind = node.kind();
    match kind {
        "namespace_declaration" | "file_scoped_namespace_declaration" => {
            let raw_name = csharp_field_text(node, "name", ctx.source).unwrap_or_default();
            let segments = csharp_qualified_segments(&raw_name);
            let pushed = segments.len();
            scope.namespace_segments.extend(segments.clone());
            scope.record_namespace();
            if let Some(symbol) =
                csharp_namespace_symbol(node, ctx, &raw_name, parent_symbol.as_ref())
            {
                let next_parent = Some((symbol.id.clone(), symbol.kind));
                let next_owner = owner_symbol.clone();
                ctx.symbols.push(symbol);
                visit_csharp_children(node, ctx, next_parent, next_owner, scope);
            } else {
                visit_csharp_children(
                    node,
                    ctx,
                    parent_symbol.clone(),
                    owner_symbol.clone(),
                    scope,
                );
            }
            if kind == "namespace_declaration" {
                for _ in 0..pushed {
                    scope.namespace_segments.pop();
                }
            }
            return;
        }
        "using_directive" => {
            extract_csharp_using_directive(node, ctx, owner_symbol.clone());
        }
        _ => {}
    }

    if let Some(symbol) = csharp_symbol_from_node(node, ctx, parent_symbol.as_ref(), scope) {
        extract_csharp_symbol_facts(node, &symbol, ctx);
        let next_parent = Some((symbol.id.clone(), symbol.kind));
        let next_owner = if symbol.body_span.is_some() {
            Some(symbol.id.clone())
        } else {
            owner_symbol.clone()
        };
        let pushed_type = csharp_symbol_can_own_type_members(symbol.kind);
        let pushed_callable = matches!(
            symbol.kind,
            SymbolKind::Function | SymbolKind::Method | SymbolKind::Test
        );
        if pushed_type {
            scope.type_path.push(symbol.name.clone());
        }
        if pushed_callable {
            scope.callable_path.push(symbol.name.clone());
        }
        ctx.symbols.push(symbol);
        visit_csharp_children(node, ctx, next_parent, next_owner, scope);
        if pushed_callable {
            scope.callable_path.pop();
        }
        if pushed_type {
            scope.type_path.pop();
        }
        return;
    }

    match kind {
        "field_declaration" | "event_field_declaration" => {
            extract_csharp_field_symbols(node, ctx, parent_symbol.as_ref(), scope);
        }
        "invocation_expression" => {
            extract_csharp_call(node, ctx, owner_symbol.clone());
        }
        "object_creation_expression" => {
            extract_csharp_object_creation(node, ctx, owner_symbol.clone());
        }
        "identifier" if !is_csharp_declaration_name(node) => {
            // The right-hand name of `Foo.Bar` (a `member_access_expression`)
            // is a method-or-field access, not a bare identifier; the
            // graph's binding chain treats `Method` symbols as
            // bindable from `Field`-kind references only (mirrors the
            // Java / JS / TS extractors). Without this distinction
            // every namespace-qualified call like
            // `MiscellaneousUtils.CreateArgumentOutOfRangeException(...)`
            // emitted only an `Identifier`-kind reference, which the
            // semantic-edge step rejected via
            // `reference_kind_can_bind_symbol`, so `reference_search`
            // silently missed the call site.
            let kind = if csharp_identifier_is_member_access_name(node) {
                ReferenceKind::Field
            } else {
                ReferenceKind::Identifier
            };
            extract_csharp_reference(node, kind, ctx, owner_symbol.clone());
        }
        "type_identifier" => {
            extract_csharp_reference(node, ReferenceKind::Type, ctx, owner_symbol.clone());
        }
        "generic_name" if !is_csharp_declaration_name(node) => {
            extract_csharp_reference(node, ReferenceKind::Type, ctx, owner_symbol.clone());
        }
        "qualified_name" => {
            extract_csharp_reference(node, ReferenceKind::Path, ctx, owner_symbol.clone());
        }
        kind if is_csharp_literal(kind) => {
            extract_body_hit(node, BodyHitKind::Literal, ctx, owner_symbol.clone());
        }
        _ => {}
    }

    visit_csharp_children(node, ctx, parent_symbol, owner_symbol, scope);
}

fn visit_csharp_children(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<(SymbolId, SymbolKind)>,
    owner_symbol: Option<SymbolId>,
    scope: &mut CsharpScope,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit_csharp_node(
            child,
            ctx,
            parent_symbol.clone(),
            owner_symbol.clone(),
            scope,
        );
    }
}

pub(crate) fn csharp_namespace_symbol(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    raw_name: &str,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> Option<ParsedSymbol> {
    let trimmed = raw_name.trim();
    if trimmed.is_empty() {
        return None;
    }
    let span = span_from_node(node);
    let body = node.child_by_field_name("body");
    let signature = signature_text(node, body, ctx.source);
    let parent_id = parent_symbol.map(|(id, _)| id.clone());
    let id = symbol_id(
        &ctx.file,
        parent_id.as_ref(),
        SymbolKind::Module,
        trimmed,
        span,
    );
    Some(ParsedSymbol {
        id,
        file_id: ctx.file.id.clone(),
        parent_id,
        name: trimmed.to_string(),
        kind: SymbolKind::Module,
        language_identity: Some(format!("N:{trimmed}")),
        span,
        body_span: body.map(span_from_node),
        signature_span: signature_span_from_nodes(node, body),
        signature,
        visibility: None,
        docs: Vec::new(),
        attributes: vec!["csharp:namespace".to_string()],
        provenance: Provenance::new(
            "tree-sitter-c-sharp",
            format!("{} declaration", node.kind()),
        ),
        confidence: Confidence::ExactSyntax,
        freshness: Freshness::Fresh,
        arity: None,
    })
}

fn csharp_symbol_from_node(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
    scope: &CsharpScope,
) -> Option<ParsedSymbol> {
    let kind = match node.kind() {
        "class_declaration" => SymbolKind::Class,
        // C# interfaces map to `SymbolKind::Interface` (added with the Go
        // semantic graph PR) so they sit beside Go interface declarations
        // rather than overloading Rust's `Trait` kind.
        "interface_declaration" => SymbolKind::Interface,
        "record_declaration" => SymbolKind::Struct,
        "struct_declaration" => SymbolKind::Struct,
        "enum_declaration" => SymbolKind::Enum,
        "delegate_declaration" => SymbolKind::TypeAlias,
        "method_declaration" | "local_function_statement" => SymbolKind::Method,
        "constructor_declaration" | "destructor_declaration" => SymbolKind::Method,
        "operator_declaration" | "conversion_operator_declaration" => SymbolKind::Method,
        "property_declaration" | "indexer_declaration" => SymbolKind::Field,
        "event_declaration" => SymbolKind::Field,
        "enum_member_declaration" => SymbolKind::Variant,
        _ => return None,
    };

    let mut kind = kind;
    if matches!(
        node.kind(),
        "method_declaration" | "local_function_statement"
    ) {
        let inside_type = parent_symbol
            .map(|(_, parent_kind)| {
                matches!(
                    parent_kind,
                    SymbolKind::Class
                        | SymbolKind::Struct
                        | SymbolKind::Trait
                        | SymbolKind::Interface
                        | SymbolKind::Enum
                )
            })
            .unwrap_or(false);
        if !inside_type && (node.kind() != "local_function_statement" || scope.type_path.is_empty())
        {
            kind = SymbolKind::Function;
        }
    }

    let name = csharp_symbol_name(node, ctx.source)?;
    if name.is_empty() {
        return None;
    }

    let attributes_raw = csharp_attribute_strings(node, ctx.source);
    let modifiers = csharp_modifiers(node, ctx.source);
    let mut attributes = csharp_semantic_attributes(node, &attributes_raw, &modifiers);
    if matches!(node.kind(), "method_declaration") && csharp_is_test(&attributes_raw) {
        kind = SymbolKind::Test;
        attributes.push("csharp:test".to_string());
    }
    if matches!(node.kind(), "method_declaration")
        && csharp_is_test_filename(&ctx.file.relative_path)
        && !attributes.iter().any(|attr| attr == "csharp:test")
    {
        attributes.push("csharp:test-host".to_string());
    }
    // C# extension method: a `static` method whose first parameter carries the
    // `this` modifier. Tag it `csharp:extension` and capture the receiver type
    // so a future `csharp_extension_method_call` resolver branch can route
    // `value.Method()` to this static definition (parallels Kotlin/Swift/
    // Scala/Dart). The receiver leaf is also recorded as a `type:` attribute,
    // mirroring how the Swift resolver discovers the extended type.
    if matches!(node.kind(), "method_declaration")
        && modifiers.iter().any(|modifier| modifier == "static")
        && let Some(receiver) = csharp_extension_receiver_type(node, ctx.source)
    {
        attributes.push("csharp:extension".to_string());
        attributes.push(format!("csharp:extension-receiver:{receiver}"));
    }
    if let Some(namespace) = scope.current_namespace() {
        attributes.push(format!("csharp:namespace:{namespace}"));
    }
    if matches!(
        node.kind(),
        "class_declaration" | "interface_declaration" | "record_declaration" | "struct_declaration"
    ) {
        for base in csharp_collect_base_types(node, ctx.source) {
            attributes.push(format!("base:{base}"));
        }
    }
    let docs = csharp_doc_comments(node, ctx.source);
    attributes.sort();
    attributes.dedup();

    let span = span_from_node(node);
    let body = node
        .child_by_field_name("body")
        .or_else(|| node.child_by_field_name("accessors"));
    let signature = signature_text(node, body, ctx.source);
    let parent_id = parent_symbol.map(|(id, _)| id.clone());
    let visibility = csharp_visibility(&modifiers);
    let id = symbol_id(&ctx.file, parent_id.as_ref(), kind, &name, span);
    let language_identity = csharp_language_identity(node, kind, &name, &modifiers, scope);
    let arity = if matches!(
        kind,
        SymbolKind::Method | SymbolKind::Function | SymbolKind::Test
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
        language_identity,
        span,
        body_span: body.map(span_from_node),
        signature_span: signature_span_from_nodes(node, body),
        signature,
        visibility,
        docs,
        attributes,
        provenance: Provenance::new(
            "tree-sitter-c-sharp",
            format!("{} declaration", node.kind()),
        ),
        confidence: csharp_conditional_confidence(node, Confidence::ExactSyntax),
        freshness: Freshness::Fresh,
        arity,
    })
}

pub(crate) fn csharp_symbol_name(node: Node<'_>, source: &str) -> Option<String> {
    if node.kind() == "indexer_declaration" {
        return Some("Item".to_string());
    }
    if let Some(name_node) = node.child_by_field_name("name") {
        return node_text(name_node, source)
            .ok()
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty());
    }
    // operator_declaration uses an "operator" field; treat the operator token as the name.
    if let Some(op_node) = node.child_by_field_name("operator") {
        return node_text(op_node, source)
            .ok()
            .map(|text| format!("operator{}", text.trim()));
    }
    None
}

fn csharp_symbol_can_own_type_members(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class
            | SymbolKind::Struct
            | SymbolKind::Interface
            | SymbolKind::Enum
            | SymbolKind::Module
    )
}

fn csharp_language_identity(
    node: Node<'_>,
    kind: SymbolKind,
    name: &str,
    modifiers: &[String],
    scope: &CsharpScope,
) -> Option<String> {
    let type_name = csharp_type_identity_name(scope, name);
    match kind {
        SymbolKind::Class
        | SymbolKind::Struct
        | SymbolKind::Interface
        | SymbolKind::Enum
        | SymbolKind::TypeAlias => Some(format!("T:{type_name}")),
        SymbolKind::Function | SymbolKind::Method | SymbolKind::Test => {
            let member = match node.kind() {
                "constructor_declaration" => {
                    if modifiers.iter().any(|modifier| modifier == "static") {
                        "#cctor".to_string()
                    } else {
                        "#ctor".to_string()
                    }
                }
                "destructor_declaration" => "Finalize".to_string(),
                "operator_declaration" | "conversion_operator_declaration" => {
                    csharp_operator_identity_name(name)
                }
                _ => name.to_string(),
            };
            Some(format!(
                "M:{}.{}",
                csharp_member_owner_identity(scope)?,
                member
            ))
        }
        SymbolKind::Field => {
            let prefix = match node.kind() {
                "event_declaration" => "E",
                "property_declaration" | "indexer_declaration" => "P",
                _ => "F",
            };
            Some(format!(
                "{prefix}:{}.{}",
                csharp_member_owner_identity(scope)?,
                name
            ))
        }
        SymbolKind::Variant => Some(format!(
            "F:{}.{}",
            csharp_member_owner_identity(scope)?,
            name
        )),
        _ => None,
    }
}

fn csharp_type_identity_name(scope: &CsharpScope, name: &str) -> String {
    let mut identity = String::with_capacity(csharp_identity_capacity(
        &scope.namespace_segments,
        &scope.type_path,
        &[],
        Some(name),
    ));
    for segment in &scope.namespace_segments {
        push_csharp_identity_segment(&mut identity, segment);
    }
    for segment in &scope.type_path {
        push_csharp_identity_segment(&mut identity, segment);
    }
    push_csharp_identity_segment(&mut identity, name);
    identity
}

fn csharp_member_owner_identity(scope: &CsharpScope) -> Option<String> {
    if scope.namespace_segments.is_empty() && scope.type_path.is_empty() {
        return None;
    }

    let mut identity = String::with_capacity(csharp_identity_capacity(
        &scope.namespace_segments,
        &scope.type_path,
        &scope.callable_path,
        None,
    ));
    for segment in &scope.namespace_segments {
        push_csharp_identity_segment(&mut identity, segment);
    }
    for segment in &scope.type_path {
        push_csharp_identity_segment(&mut identity, segment);
    }
    for segment in &scope.callable_path {
        push_csharp_identity_segment(&mut identity, segment);
    }
    Some(identity)
}

fn push_csharp_identity_segment(identity: &mut String, segment: &str) {
    if !identity.is_empty() {
        identity.push('.');
    }
    identity.push_str(segment);
}

fn csharp_identity_capacity(
    namespace_segments: &[String],
    type_path: &[String],
    callable_path: &[String],
    tail: Option<&str>,
) -> usize {
    let segment_count = namespace_segments.len()
        + type_path.len()
        + callable_path.len()
        + usize::from(tail.is_some());
    let separator_count = segment_count.saturating_sub(1);
    namespace_segments
        .iter()
        .chain(type_path)
        .chain(callable_path)
        .map(String::len)
        .sum::<usize>()
        + tail.map(str::len).unwrap_or(0)
        + separator_count
}

fn csharp_operator_identity_name(name: &str) -> String {
    match name.trim_start_matches("operator") {
        "+" => "op_Addition",
        "-" => "op_Subtraction",
        "*" => "op_Multiply",
        "/" => "op_Division",
        "%" => "op_Modulus",
        "==" => "op_Equality",
        "!=" => "op_Inequality",
        "<" => "op_LessThan",
        ">" => "op_GreaterThan",
        "<=" => "op_LessThanOrEqual",
        ">=" => "op_GreaterThanOrEqual",
        "true" => "op_True",
        "false" => "op_False",
        "!" => "op_LogicalNot",
        "~" => "op_OnesComplement",
        "&" => "op_BitwiseAnd",
        "|" => "op_BitwiseOr",
        "^" => "op_ExclusiveOr",
        "<<" => "op_LeftShift",
        ">>" => "op_RightShift",
        "++" => "op_Increment",
        "--" => "op_Decrement",
        other => return format!("op_{other}"),
    }
    .to_string()
}

fn csharp_field_language_identity(
    node: Node<'_>,
    name: &str,
    scope: &CsharpScope,
) -> Option<String> {
    let owner = csharp_member_owner_identity(scope)?;
    let prefix = if node.kind() == "event_field_declaration" {
        "E"
    } else {
        "F"
    };
    Some(format!("{prefix}:{owner}.{name}"))
}

pub(crate) fn csharp_field_text(node: Node<'_>, field: &str, source: &str) -> Option<String> {
    let child = node.child_by_field_name(field)?;
    node_text(child, source)
        .ok()
        .map(|text| text.trim().to_string())
}

pub(crate) fn csharp_qualified_segments(raw: &str) -> Vec<String> {
    raw.split('.')
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
        .collect()
}

pub(crate) fn csharp_attribute_strings(node: Node<'_>, source: &str) -> Vec<String> {
    let mut attributes = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "attribute_list" {
            let mut inner = child.walk();
            for attribute_node in child.named_children(&mut inner) {
                if attribute_node.kind() == "attribute"
                    && let Ok(text) = node_text(attribute_node, source)
                {
                    attributes.push(text.trim().to_string());
                }
            }
        }
    }
    attributes
}

pub(crate) fn csharp_modifiers(node: Node<'_>, source: &str) -> Vec<String> {
    let mut modifiers = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "modifier"
            && let Ok(text) = node_text(child, source)
        {
            modifiers.push(text.trim().to_string());
        }
    }
    modifiers
}

pub(crate) fn csharp_visibility(modifiers: &[String]) -> Option<String> {
    for visibility in ["public", "internal", "protected", "private", "file"] {
        if modifiers.iter().any(|modifier| modifier == visibility) {
            return Some(visibility.to_string());
        }
    }
    None
}

pub(crate) fn csharp_semantic_attributes(
    node: Node<'_>,
    attributes_raw: &[String],
    modifiers: &[String],
) -> Vec<String> {
    let mut attributes = Vec::new();
    for modifier in modifiers {
        attributes.push(format!("csharp:modifier:{modifier}"));
        if modifier == "partial" {
            attributes.push("csharp:partial".to_string());
        }
        if modifier == "static" {
            attributes.push("csharp:static".to_string());
        }
        if modifier == "abstract" {
            attributes.push("csharp:abstract".to_string());
        }
        if modifier == "async" {
            attributes.push("csharp:async".to_string());
        }
    }
    for attribute in attributes_raw {
        let cleaned = csharp_attribute_head(attribute);
        if cleaned.is_empty() {
            continue;
        }
        attributes.push(format!("csharp:attr:{cleaned}"));
        match cleaned.as_str() {
            "ApiController" | "Controller" => {
                attributes.push("framework:aspnet".to_string());
                attributes.push("framework:web-route".to_string());
            }
            "Route" => {
                attributes.push("framework:aspnet".to_string());
                attributes.push("framework:web-route".to_string());
                if let Some(path) = first_csharp_string_literal(attribute) {
                    attributes.push(format!("route:{path}"));
                }
            }
            "HttpGet" | "HttpPost" | "HttpPut" | "HttpPatch" | "HttpDelete" | "HttpOptions"
            | "HttpHead" => {
                let method = cleaned.trim_start_matches("Http").to_ascii_uppercase();
                attributes.push("framework:aspnet".to_string());
                attributes.push("framework:web-route".to_string());
                attributes.push(format!("route:{method}"));
                if let Some(path) = first_csharp_string_literal(attribute) {
                    attributes.push(format!("route:{method} {path}"));
                }
            }
            "Inject" => attributes.push("framework:di".to_string()),
            "Serializable" | "DataContract" => attributes.push("csharp:serializable".to_string()),
            _ => {}
        }
    }
    if matches!(
        node.kind(),
        "class_declaration" | "struct_declaration" | "record_declaration"
    ) {
        let _ = node;
    }
    attributes
}

pub(crate) fn csharp_attribute_head(attribute: &str) -> String {
    let body = attribute
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim();
    let body = body
        .split_once(':')
        .map(|(_, rest)| rest.trim())
        .unwrap_or(body);
    let head = body.split('(').next().unwrap_or(body).trim();
    let head = head.rsplit('.').next().unwrap_or(head).trim();
    head.to_string()
}

pub(crate) fn csharp_is_test(attributes_raw: &[String]) -> bool {
    attributes_raw.iter().any(|attribute| {
        let head = csharp_attribute_head(attribute);
        matches!(
            head.as_str(),
            "Fact"
                | "Test"
                | "Theory"
                | "TestMethod"
                | "TestCase"
                | "TestCaseSource"
                | "InlineData"
                | "DataTestMethod"
                | "Property"
                | "FsCheck"
        )
    })
}

pub(crate) fn csharp_is_test_filename(relative_path: &str) -> bool {
    let file_name = relative_path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(relative_path);
    let stem = file_name
        .strip_suffix(".cs")
        .or_else(|| file_name.strip_suffix(".csx"))
        .unwrap_or(file_name);
    let lower = stem.to_ascii_lowercase();
    lower.ends_with("tests") || lower.ends_with("test") || lower.contains(".tests.")
}

pub(crate) fn csharp_doc_comments(node: Node<'_>, source: &str) -> Vec<String> {
    let mut docs = Vec::new();
    let mut walker = node;
    while let Some(previous) = walker.prev_named_sibling() {
        walker = previous;
        match previous.kind() {
            "comment" => {
                if let Ok(text) = node_text(previous, source) {
                    let trimmed = text.trim();
                    if trimmed.starts_with("///") {
                        docs.push(trimmed.to_string());
                        continue;
                    }
                }
                break;
            }
            "attribute_list" => continue,
            _ => break,
        }
    }
    docs.reverse();
    docs
}

pub(crate) fn first_csharp_string_literal(text: &str) -> Option<String> {
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        let quote = match ch {
            '"' => '"',
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

pub(crate) fn extract_csharp_using_directive(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let raw = node_text(node, ctx.source).unwrap_or_default();
    let trimmed = raw.trim().trim_end_matches(';').trim();
    let body = trimmed.strip_prefix("using").unwrap_or(trimmed).trim();
    let is_global = body
        .strip_prefix("global")
        .map(|rest| {
            rest.trim_start()
                .starts_with(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
        })
        .unwrap_or(false);
    let body = body.strip_prefix("global").unwrap_or(body).trim();
    let (is_static, body) = if let Some(rest) = body.strip_prefix("static") {
        (true, rest.trim())
    } else {
        (false, body)
    };
    let (alias, path) = match body.split_once('=') {
        Some((alias, target)) => (Some(alias.trim().to_string()), target.trim().to_string()),
        None => (None, body.trim().to_string()),
    };
    let path = path.trim().trim_end_matches(';').trim().to_string();
    if path.is_empty() {
        return;
    }
    // Classify the binding shape so the parse-coverage histogram reflects it:
    //   `using static N.C;`     -> Static  (members of C in scope)
    //   `using Alias = N.Type;` -> Named   (a single aliased name in scope)
    //   `using N;`              -> Namespace (every name under N in scope)
    let kind = if is_static {
        ImportKind::Static
    } else if alias.is_some() {
        ImportKind::Named
    } else {
        ImportKind::Namespace
    };
    let imported_name = if is_static {
        None
    } else if let Some(alias) = alias.as_deref() {
        // For an alias, the bound leaf is the alias identifier itself.
        Some(alias.to_string())
    } else {
        Some(last_path_segment(&path))
    };
    let mut import = ParsedImport {
        file_id: ctx.file.id.clone(),
        owner_id: owner_id.clone(),
        path,
        alias,
        is_glob: is_static,
        is_reexport: is_global,
        is_static,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-c-sharp", "using directive"),
        kind,
        imported_name,
        is_global,
    };
    if is_static {
        import.path = format!("{}.*", import.path);
    }
    ctx.imports.push(import);
}

pub(crate) fn extract_csharp_call(
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
    let (name, receiver, kind) = csharp_call_target_parts(function_node, &target_text, ctx.source);
    let arity = node
        .child_by_field_name("arguments")
        .map(|arguments| named_child_count(arguments))
        .unwrap_or_default();

    ctx.calls.push(ParsedCall {
        file_id: ctx.file.id.clone(),
        caller_id: owner_id.clone(),
        name,
        target_text: target_text.clone(),
        receiver,
        arity,
        kind,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-c-sharp", "invocation_expression"),
        confidence: csharp_conditional_confidence(node, Confidence::Heuristic),
    });
    extract_body_hit(node, BodyHitKind::Call, ctx, owner_id);
}

pub(crate) fn csharp_call_target_parts(
    function_node: Node<'_>,
    target_text: &str,
    source: &str,
) -> (String, Option<String>, ParsedCallKind) {
    match function_node.kind() {
        "member_access_expression" => {
            let name = function_node
                .child_by_field_name("name")
                .and_then(|name| node_text(name, source).ok())
                .map(|text| text.trim().to_string())
                .unwrap_or_else(|| last_path_segment(target_text));
            let receiver = function_node
                .child_by_field_name("expression")
                .and_then(|receiver| node_text(receiver, source).ok())
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty());
            (name, receiver, ParsedCallKind::Method)
        }
        "qualified_name" => (
            last_path_segment(target_text),
            receiver_from_direct_call(target_text),
            ParsedCallKind::Direct,
        ),
        "generic_name" => {
            let base = function_node
                .child_by_field_name("name")
                .and_then(|name| node_text(name, source).ok())
                .map(|text| text.trim().to_string())
                .unwrap_or_else(|| last_path_segment(target_text));
            (base, None, ParsedCallKind::Direct)
        }
        "alias_qualified_name" => (
            last_path_segment(target_text),
            receiver_from_direct_call(target_text),
            ParsedCallKind::Direct,
        ),
        "conditional_access_expression" | "element_access_expression" => {
            (last_path_segment(target_text), None, ParsedCallKind::Method)
        }
        _ => (
            last_path_segment(target_text),
            receiver_from_direct_call(target_text),
            ParsedCallKind::Direct,
        ),
    }
}

pub(crate) fn extract_csharp_object_creation(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let Some(type_node) = node.child_by_field_name("type") else {
        return;
    };
    let target_text = node_text(type_node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if target_text.is_empty() {
        return;
    }
    let name = last_path_segment(&target_text);
    if name.is_empty() {
        return;
    }
    let arity = node
        .child_by_field_name("arguments")
        .map(|arguments| named_child_count(arguments))
        .unwrap_or_default();

    ctx.calls.push(ParsedCall {
        file_id: ctx.file.id.clone(),
        caller_id: owner_id.clone(),
        name: name.clone(),
        target_text: target_text.clone(),
        receiver: receiver_from_direct_call(&target_text),
        arity,
        kind: ParsedCallKind::Direct,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-c-sharp", "object_creation_expression"),
        confidence: csharp_conditional_confidence(node, Confidence::Heuristic),
    });
    ctx.references.push(ParsedReference {
        file_id: ctx.file.id.clone(),
        owner_id: owner_id.clone(),
        text: target_text,
        kind: ReferenceKind::Type,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-c-sharp", "object_creation_expression"),
    });
    extract_body_hit(node, BodyHitKind::Call, ctx, owner_id);
}

pub(crate) fn extract_csharp_reference(
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
    if csharp_is_keyword_or_predefined(&text) {
        return;
    }
    let body_kind = match kind {
        ReferenceKind::Identifier => BodyHitKind::Identifier,
        ReferenceKind::Field => BodyHitKind::Identifier,
        ReferenceKind::Attribute => BodyHitKind::Attribute,
        ReferenceKind::Type => BodyHitKind::Type,
        ReferenceKind::Path => BodyHitKind::Path,
    };
    ctx.references.push(ParsedReference {
        file_id: ctx.file.id.clone(),
        owner_id: owner_id.clone(),
        text: text.clone(),
        kind,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-c-sharp", format!("{} reference", node.kind())),
    });
    ctx.body_hits.push(BodyHit {
        file_id: ctx.file.id.clone(),
        owner_id,
        text,
        kind: body_kind,
        span: span_from_node(node),
    });
}

fn extract_csharp_field_symbols(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
    scope: &CsharpScope,
) {
    let Some((parent_id, parent_kind)) = parent_symbol else {
        return;
    };
    if !matches!(
        parent_kind,
        SymbolKind::Class
            | SymbolKind::Struct
            | SymbolKind::Trait
            | SymbolKind::Interface
            | SymbolKind::Enum
    ) {
        return;
    }
    let attributes_raw = csharp_attribute_strings(node, ctx.source);
    let modifiers = csharp_modifiers(node, ctx.source);
    let mut base_attributes = csharp_semantic_attributes(node, &attributes_raw, &modifiers);
    base_attributes.push("csharp:field".to_string());
    if node.kind() == "event_field_declaration" {
        base_attributes.push("csharp:event".to_string());
    }
    // A `const` field is a compile-time constant, not mutable storage; mint it
    // as `SymbolKind::Const` so `decl_search kind=const` finds it and the
    // member can be distinguished from a mutable field. `readonly`/`volatile`
    // stay `Field` but carry their own flag so callers can filter on them.
    // (Events never carry these modifiers.)
    let is_const = node.kind() != "event_field_declaration"
        && modifiers.iter().any(|modifier| modifier == "const");
    let field_kind = if is_const {
        SymbolKind::Const
    } else {
        SymbolKind::Field
    };
    if is_const {
        base_attributes.push("csharp:const".to_string());
    }
    if modifiers.iter().any(|modifier| modifier == "readonly") {
        base_attributes.push("csharp:readonly".to_string());
    }
    if modifiers.iter().any(|modifier| modifier == "volatile") {
        base_attributes.push("csharp:volatile".to_string());
    }
    let mut cursor = node.walk();
    let mut declarations = Vec::new();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "variable_declaration" {
            declarations.push(child);
        }
    }
    for declaration in declarations {
        let mut declarator_cursor = declaration.walk();
        let type_node = declaration.child_by_field_name("type");
        let type_text = type_node
            .and_then(|node| node_text(node, ctx.source).ok())
            .map(|text| text.trim().to_string());
        for declarator in declaration.named_children(&mut declarator_cursor) {
            if declarator.kind() != "variable_declarator" {
                continue;
            }
            let Some(name_node) = declarator.child_by_field_name("name") else {
                continue;
            };
            let Some(name) = node_text(name_node, ctx.source)
                .ok()
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty())
            else {
                continue;
            };
            let span = span_from_node(declarator);
            let mut attributes = base_attributes.clone();
            if let Some(type_text) = type_text.clone() {
                attributes.push(format!("type:{}", last_path_segment(&type_text)));
            }
            attributes.sort();
            attributes.dedup();
            let signature = signature_text(
                declaration,
                declarator.child_by_field_name("value"),
                ctx.source,
            );
            ctx.symbols.push(ParsedSymbol {
                id: symbol_id(&ctx.file, Some(parent_id), field_kind, &name, span),
                file_id: ctx.file.id.clone(),
                parent_id: Some(parent_id.clone()),
                name: name.clone(),
                kind: field_kind,
                language_identity: csharp_field_language_identity(node, &name, scope),
                span,
                body_span: None,
                signature_span: None,
                signature,
                visibility: csharp_visibility(&modifiers),
                docs: Vec::new(),
                attributes,
                provenance: Provenance::new("tree-sitter-c-sharp", "field declaration"),
                confidence: csharp_conditional_confidence(node, Confidence::ExactSyntax),
                freshness: Freshness::Fresh,
                arity: None,
            });
            if let Some(type_text) = type_text.clone() {
                ctx.references.push(ParsedReference {
                    file_id: ctx.file.id.clone(),
                    owner_id: Some(parent_id.clone()),
                    text: type_text,
                    kind: ReferenceKind::Type,
                    span,
                    provenance: Provenance::new("tree-sitter-c-sharp", "field type reference"),
                });
            }
        }
    }
}

pub(crate) fn csharp_collect_base_types(node: Node<'_>, source: &str) -> Vec<String> {
    let mut bases = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "base_list" {
            continue;
        }
        let mut base_cursor = child.walk();
        for base in child.named_children(&mut base_cursor) {
            let raw = match base.kind() {
                "primary_constructor_base_type" => base
                    .child_by_field_name("type")
                    .and_then(|type_node| node_text(type_node, source).ok()),
                _ => node_text(base, source).ok(),
            };
            if let Some(text) = raw
                && let Some(name) = csharp_type_name_from_annotation(text)
            {
                bases.push(name);
            }
        }
    }
    bases.sort();
    bases.dedup();
    bases
}

pub(crate) fn extract_csharp_symbol_facts(
    node: Node<'_>,
    symbol: &ParsedSymbol,
    ctx: &mut ExtractContext<'_>,
) {
    if matches!(
        node.kind(),
        "class_declaration" | "interface_declaration" | "record_declaration" | "struct_declaration"
    ) {
        for base in csharp_collect_base_types(node, ctx.source) {
            ctx.references.push(ParsedReference {
                file_id: ctx.file.id.clone(),
                owner_id: Some(symbol.id.clone()),
                text: base,
                kind: ReferenceKind::Type,
                span: symbol.span,
                provenance: Provenance::new("tree-sitter-c-sharp", "base type reference"),
            });
        }
        // `csharp_collect_base_types` only returns the leaf of each base
        // (`IReadOnlyList`), so a type used solely as a generic argument of a
        // base (`: IEnumerable<MyEnum>`) would never bind. Walk the raw
        // `base_list` subtree to emit a `Type` ref per nested argument.
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "base_list" {
                csharp_push_generic_argument_refs(
                    child,
                    symbol,
                    ctx,
                    "base type argument reference",
                );
            }
        }
        // Positional records (`record Customer(string Name, int Age)`) and C# 12
        // primary-constructor classes/structs declare their parameters directly
        // on the type. The grammar models them as a `parameter_list` child of
        // the type declaration. Each one becomes an init-only property of the
        // type, so mint a `Field` member per parameter (plus its `Type` ref) —
        // otherwise the type shows field-less and `Customer.Name` has no decl.
        csharp_emit_primary_constructor_fields(node, symbol, ctx);
    }
    if matches!(
        node.kind(),
        "method_declaration" | "local_function_statement" | "constructor_declaration"
    ) {
        if let Some(parameters) = node.child_by_field_name("parameters") {
            let mut cursor = parameters.walk();
            for parameter in parameters.named_children(&mut cursor) {
                if parameter.kind() != "parameter" {
                    continue;
                }
                if let Some(type_node) = parameter.child_by_field_name("type") {
                    push_csharp_type_reference(type_node, symbol, ctx, "parameter type reference");
                }
            }
        }
        if let Some(returns) = node.child_by_field_name("returns") {
            push_csharp_type_reference(returns, symbol, ctx, "return type reference");
        }
    }
    // Generic constraint clauses (`where T : IEntity, new()`). The grammar
    // never gets read by the generic identifier path, so an interface/enum used
    // *only* as a constraint was emitted as `Identifier`-kind and rejected by
    // the binder. Emit a `Type` ref per constraint type so `where T : IEntity`
    // binds and surfaces in `reference_search`/`impact`. (The `new()` /
    // `class` / `struct` / `notnull` keyword constraints carry no type.)
    csharp_emit_constraint_type_references(node, symbol, ctx);
}

/// Emit a `Type` reference for every type named in a declaration's
/// `type_parameter_constraints_clause`s (`where T : Base, IFace`). Each clause
/// holds `type_parameter_constraint` children whose `type` field is the
/// constraint type; keyword-only constraints (`new()`, `class`, …) are skipped.
fn csharp_emit_constraint_type_references(
    node: Node<'_>,
    symbol: &ParsedSymbol,
    ctx: &mut ExtractContext<'_>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "type_parameter_constraints_clause" {
            continue;
        }
        let mut constraint_cursor = child.walk();
        for constraint in child.named_children(&mut constraint_cursor) {
            if constraint.kind() != "type_parameter_constraint" {
                continue;
            }
            if let Some(type_node) = constraint.child_by_field_name("type") {
                push_csharp_type_reference(type_node, symbol, ctx, "type constraint reference");
            }
        }
    }
}

/// Returns the leaf receiver type of a C# extension method, i.e. the `type`
/// of the first parameter when that parameter carries the `this` modifier.
/// tree-sitter-c-sharp aliases the `this`/`ref`/`in`/`out` parameter prefixes
/// to named `modifier` nodes, so we scan the first parameter's children.
pub(crate) fn csharp_extension_receiver_type(node: Node<'_>, source: &str) -> Option<String> {
    let parameters = node.child_by_field_name("parameters")?;
    let mut cursor = parameters.walk();
    let first = parameters
        .named_children(&mut cursor)
        .find(|child| child.kind() == "parameter")?;
    let has_this = csharp_modifiers(first, source)
        .iter()
        .any(|modifier| modifier == "this");
    if !has_this {
        return None;
    }
    let type_node = first.child_by_field_name("type")?;
    let text = node_text(type_node, source).ok()?;
    csharp_type_name_from_annotation(text)
}

/// Mint a `Field` member for each positional-record / primary-constructor
/// parameter of a type declaration. These parameters become public init-only
/// properties of the type, so they are surfaced as members (DocComment-ID
/// prefix `P:`) parented to the type, each with a `Type` reference.
fn csharp_emit_primary_constructor_fields(
    node: Node<'_>,
    type_symbol: &ParsedSymbol,
    ctx: &mut ExtractContext<'_>,
) {
    let mut cursor = node.walk();
    let Some(parameter_list) = node
        .children(&mut cursor)
        .find(|child| child.kind() == "parameter_list")
    else {
        return;
    };
    // Property DocComment-ID owner derived from the type's own `T:` identity.
    let owner_identity = type_symbol
        .language_identity
        .as_deref()
        .and_then(|identity| identity.strip_prefix("T:"))
        .map(str::to_string);
    let mut param_cursor = parameter_list.walk();
    for parameter in parameter_list.named_children(&mut param_cursor) {
        if parameter.kind() != "parameter" {
            continue;
        }
        let Some(name_node) = parameter.child_by_field_name("name") else {
            continue;
        };
        let Some(name) = node_text(name_node, ctx.source)
            .ok()
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty())
        else {
            continue;
        };
        let type_node = parameter.child_by_field_name("type");
        let type_text = type_node
            .and_then(|n| node_text(n, ctx.source).ok())
            .map(|text| text.trim().to_string());
        let span = span_from_node(parameter);
        let mut attributes = vec![
            "csharp:field".to_string(),
            "csharp:primary-constructor-param".to_string(),
        ];
        if let Some(text) = type_text.as_ref() {
            attributes.push(format!("type:{}", last_path_segment(text)));
        }
        attributes.sort();
        attributes.dedup();
        let language_identity = owner_identity
            .as_ref()
            .map(|owner| format!("P:{owner}.{name}"));
        ctx.symbols.push(ParsedSymbol {
            id: symbol_id(
                &ctx.file,
                Some(&type_symbol.id),
                SymbolKind::Field,
                &name,
                span,
            ),
            file_id: ctx.file.id.clone(),
            parent_id: Some(type_symbol.id.clone()),
            name: name.clone(),
            kind: SymbolKind::Field,
            language_identity,
            span,
            body_span: None,
            signature_span: None,
            signature: node_text(parameter, ctx.source)
                .ok()
                .map(|text| text.trim().to_string())
                .unwrap_or_default(),
            visibility: Some("public".to_string()),
            docs: Vec::new(),
            attributes,
            provenance: Provenance::new(
                "tree-sitter-c-sharp",
                "primary constructor parameter",
            ),
            confidence: csharp_conditional_confidence(parameter, Confidence::ExactSyntax),
            freshness: Freshness::Fresh,
            arity: None,
        });
        if let Some(type_node) = type_node {
            push_csharp_type_reference(
                type_node,
                type_symbol,
                ctx,
                "primary constructor parameter type reference",
            );
        }
    }
}

pub(crate) fn push_csharp_type_reference(
    type_node: Node<'_>,
    symbol: &ParsedSymbol,
    ctx: &mut ExtractContext<'_>,
    reason: &'static str,
) {
    if let Ok(text) = node_text(type_node, ctx.source)
        && let Some(cleaned) = csharp_type_name_from_annotation(text)
    {
        ctx.references.push(ParsedReference {
            file_id: ctx.file.id.clone(),
            owner_id: Some(symbol.id.clone()),
            text: cleaned,
            kind: ReferenceKind::Type,
            span: symbol.span,
            provenance: Provenance::new("tree-sitter-c-sharp", reason),
        });
    }
    // `csharp_type_name_from_annotation` truncates at the first `<`, so the
    // outer ref only names the open generic (e.g. `List`). Recurse into every
    // nested `type_argument_list` and emit a `Type` ref per argument so a type
    // used *only* as a generic argument (`List<MyEnum>`) still binds — the
    // binder rejects `Identifier`-kind refs for interfaces/enums/aliases.
    csharp_push_generic_argument_refs(type_node, symbol, ctx, reason);
}

/// Walk the subtree of a type node, pushing a `Type` reference for each named
/// argument found inside any `type_argument_list` (handles nesting such as
/// `Dictionary<string, List<Foo>>`).
fn csharp_push_generic_argument_refs(
    node: Node<'_>,
    symbol: &ParsedSymbol,
    ctx: &mut ExtractContext<'_>,
    reason: &'static str,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "type_argument_list" {
            let mut arg_cursor = child.walk();
            for argument in child.named_children(&mut arg_cursor) {
                if let Ok(text) = node_text(argument, ctx.source)
                    && let Some(cleaned) = csharp_type_name_from_annotation(text)
                {
                    ctx.references.push(ParsedReference {
                        file_id: ctx.file.id.clone(),
                        owner_id: Some(symbol.id.clone()),
                        text: cleaned,
                        kind: ReferenceKind::Type,
                        span: symbol.span,
                        provenance: Provenance::new("tree-sitter-c-sharp", reason),
                    });
                }
                // Descend so deeper argument lists are captured too.
                csharp_push_generic_argument_refs(argument, symbol, ctx, reason);
            }
        } else {
            csharp_push_generic_argument_refs(child, symbol, ctx, reason);
        }
    }
}

pub(crate) fn csharp_type_name_from_annotation(annotation: &str) -> Option<String> {
    let mut text = annotation
        .trim()
        .trim_matches(|ch: char| matches!(ch, '?' | '*' | '&' | ' '))
        .to_string();
    if let Some(open) = text.find('<') {
        text.truncate(open);
    }
    let stripped = text.trim().to_string();
    if stripped.is_empty() {
        return None;
    }
    let leaf = last_path_segment(&stripped);
    if csharp_is_keyword_or_predefined(&leaf) {
        return None;
    }
    Some(leaf)
}

pub(crate) fn is_csharp_declaration_name(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    // The early-return only fires when the parent is genuinely a
    // declaration (class/method/struct/etc) and the current node is
    // that declaration's `name` field. Without the kind gate, every
    // inner identifier whose parent has a `name` field — most
    // notably the rhs of a `member_access_expression`
    // (`Foo.Bar(...)` → `Bar`) — was being incorrectly treated as a
    // declaration name, so qualified call sites never got a
    // `ParsedReference` and `reference_search` silently missed them
    // (verified by the
    // `references_to_symbol_finds_csharp_namespace_qualified_internal_static_call`
    // test from the Newtonsoft.Json A/B).
    matches!(
        parent.kind(),
        "variable_declarator"
            | "type_parameter"
            | "parameter"
            | "method_declaration"
            | "class_declaration"
            | "interface_declaration"
            | "record_declaration"
            | "struct_declaration"
            | "enum_declaration"
            | "enum_member_declaration"
            | "namespace_declaration"
            | "file_scoped_namespace_declaration"
            | "property_declaration"
            | "field_declaration"
            | "event_declaration"
            | "event_field_declaration"
            | "delegate_declaration"
            | "constructor_declaration"
            | "destructor_declaration"
            | "local_function_statement"
    ) && parent
        .child_by_field_name("name")
        .map(|name_node| name_node.id() == node.id())
        .unwrap_or(false)
}

/// Returns true when `node` is an `identifier` sitting on the
/// right-hand side of a `member_access_expression` (`Foo.Bar` → `Bar`).
/// Mirrors the Java / JS / TS heuristic that treats these as
/// field/method accesses rather than bare identifiers.
pub(crate) fn csharp_identifier_is_member_access_name(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if parent.kind() != "member_access_expression" {
        return false;
    }
    parent
        .child_by_field_name("name")
        .map(|name_node| name_node.id() == node.id())
        .unwrap_or(false)
}

/// True when `node` sits inside a `#if`/`#elif`/`#else` preprocessor branch.
/// tree-sitter-c-sharp nests the conditional body as children of the
/// `preproc_if`/`preproc_elif`/`preproc_else` node, so a single ancestor walk
/// detects it. `#region`/`#pragma`/`#nullable` are *not* conditional — they
/// never gate code out — so they are deliberately ignored, mirroring the
/// C/C++ extractor's `ConditionalUnknown` downgrade.
pub(crate) fn csharp_node_in_conditional(node: Node<'_>) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        if matches!(parent.kind(), "preproc_if" | "preproc_elif" | "preproc_else") {
            return true;
        }
        current = parent.parent();
    }
    false
}

/// Downgrade a base confidence to `ConditionalUnknown` when the node lives in a
/// preprocessor-gated branch whose activation we cannot evaluate.
pub(crate) fn csharp_conditional_confidence(node: Node<'_>, base: Confidence) -> Confidence {
    if csharp_node_in_conditional(node) {
        Confidence::ConditionalUnknown
    } else {
        base
    }
}

pub(crate) fn is_csharp_literal(kind: &str) -> bool {
    matches!(
        kind,
        "string_literal"
            | "verbatim_string_literal"
            | "raw_string_literal"
            | "integer_literal"
            | "real_literal"
            | "boolean_literal"
            | "character_literal"
            | "null_literal"
    )
}

pub(crate) fn csharp_is_keyword_or_predefined(text: &str) -> bool {
    matches!(
        text,
        "var"
            | "void"
            | "string"
            | "bool"
            | "byte"
            | "sbyte"
            | "char"
            | "decimal"
            | "double"
            | "float"
            | "int"
            | "uint"
            | "long"
            | "ulong"
            | "short"
            | "ushort"
            | "object"
            | "dynamic"
            | "nint"
            | "nuint"
            | "true"
            | "false"
            | "null"
            | "this"
            | "base"
            | "value"
    )
}

pub(crate) fn dedup_csharp_facts(ctx: &mut ExtractContext<'_>) {
    let mut import_seen = HashSet::with_capacity(ctx.imports.len());
    ctx.imports.retain(|import| {
        import_seen.insert(format!(
            "{}|{:?}|{}|{:?}|{}|{}",
            import.file_id.0,
            import.owner_id.as_ref().map(|id| id.0.as_str()),
            import.path,
            import.alias,
            import.is_glob,
            import.is_reexport
        ))
    });

    let mut reference_seen = HashSet::with_capacity(ctx.references.len());
    ctx.references.retain(|reference| {
        reference_seen.insert(format!(
            "{}|{:?}|{}|{:?}|{}|{}",
            reference.file_id.0,
            reference.owner_id.as_ref().map(|id| id.0.as_str()),
            reference.text,
            reference.kind,
            reference.span.start_byte,
            reference.span.end_byte
        ))
    });
}
