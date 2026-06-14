use std::collections::HashSet;

use crate::languages::rust::*;
use crate::*;

const PROVENANCE: &str = "tree-sitter-php";

pub(crate) fn extract_php(file: FileRecord, source: &str, tree: &Tree) -> ParsedFile {
    let mut ctx = ExtractContext::new(file.clone(), source);
    let root = tree.root_node();
    record_parse_error_diagnostics(root, &mut ctx);

    let mut scope = PhpScope::default();
    visit_php_node(root, &mut ctx, None, None, &mut scope);
    dedup_php_facts(&mut ctx);

    ParsedFile {
        file,
        // The file's dominant namespace surfaces as `package`, matching how the
        // Go extractor surfaces `package`; file-scoped `namespace Foo;` and the
        // first braced namespace declaration both qualify.
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
struct PhpScope {
    /// Namespace segments split by `\`. PHP namespaces use `\` as separators
    /// in source; the scope normalises them to dot form so the same dotted
    /// identity flows into `language_identity` and into the `php:namespace:*`
    /// attribute. `record_namespace` stores the first encountered namespace
    /// as the file-level `package`.
    namespace_segments: Vec<String>,
    top_namespace: Option<String>,
    /// Enclosing type-path for inner symbols. PHP classes/interfaces/traits/
    /// enums can nest only through `anonymous_class`; the path stays one
    /// segment deep in practice but is modelled as a stack for parity with
    /// the C# extractor.
    type_path: Vec<String>,
    callable_path: Vec<String>,
}

impl PhpScope {
    fn current_namespace(&self) -> Option<String> {
        if self.namespace_segments.is_empty() {
            None
        } else {
            Some(self.namespace_segments.join("."))
        }
    }

    fn record_namespace(&mut self) {
        if self.top_namespace.is_some() {
            return;
        }
        if let Some(namespace) = self.current_namespace() {
            self.top_namespace = Some(namespace);
        }
    }
}

fn visit_php_node(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<(SymbolId, SymbolKind)>,
    owner_symbol: Option<SymbolId>,
    scope: &mut PhpScope,
) {
    if node.is_missing() {
        record_missing_node_diagnostic(node, ctx);
        return;
    }

    let kind = node.kind();
    match kind {
        // Inline HTML and PHP open/close tags at program scope are recorded
        // as plain literal body-hits so substring search still matches inline
        // content, but they never feed identifier or reference extraction.
        "text" | "text_interpolation" | "php_tag" | "php_end_tag" => {
            extract_body_hit(node, BodyHitKind::Literal, ctx, owner_symbol.clone());
            return;
        }
        // Heredoc / nowdoc bodies are emitted as a single literal hit on the
        // outer span and the inner tree is not recursed into. This stops
        // `$variables` inside heredoc bodies from polluting identifier and
        // reference extraction.
        "heredoc" | "nowdoc" => {
            extract_body_hit(node, BodyHitKind::Literal, ctx, owner_symbol.clone());
            return;
        }
        _ => {}
    }

    match kind {
        "namespace_definition" => {
            let raw_name = php_field_text(node, "name", ctx.source).unwrap_or_default();
            let segments = php_qualified_segments(&raw_name);
            let pushed = segments.len();
            let braced = node.child_by_field_name("body").is_some();
            scope.namespace_segments.extend(segments);
            scope.record_namespace();
            if let Some(symbol) = php_namespace_symbol(node, ctx, &raw_name, parent_symbol.as_ref())
            {
                let next_parent = Some((symbol.id.clone(), symbol.kind));
                let next_owner = owner_symbol.clone();
                ctx.symbols.push(symbol);
                visit_php_children(node, ctx, next_parent, next_owner, scope);
            } else {
                visit_php_children(
                    node,
                    ctx,
                    parent_symbol.clone(),
                    owner_symbol.clone(),
                    scope,
                );
            }
            // File-scoped `namespace Foo;` applies for the rest of the file
            // and is never popped. Only braced declarations restore the
            // previous segments.
            if braced {
                for _ in 0..pushed {
                    scope.namespace_segments.pop();
                }
            }
            return;
        }
        "namespace_use_declaration" => {
            extract_php_use_declaration(node, ctx, owner_symbol.clone());
            // The grammar lets `namespace_use_declaration` carry inner
            // `namespace_use_clause` children whose `alias`/`name` fields
            // should not surface as references — drain them above and stop.
            return;
        }
        _ => {}
    }

    if let Some(symbol) = php_symbol_from_node(node, ctx, parent_symbol.as_ref(), scope) {
        extract_php_symbol_facts(node, &symbol, ctx, scope);
        let next_parent = Some((symbol.id.clone(), symbol.kind));
        let next_owner = if symbol.body_span.is_some() {
            Some(symbol.id.clone())
        } else {
            owner_symbol.clone()
        };
        let pushed_type = php_symbol_can_own_type_members(symbol.kind);
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
        visit_php_children(node, ctx, next_parent, next_owner, scope);
        if pushed_callable {
            scope.callable_path.pop();
        }
        if pushed_type {
            scope.type_path.pop();
        }
        return;
    }

    match kind {
        "property_declaration" => {
            extract_php_property_symbols(node, ctx, parent_symbol.as_ref(), scope);
        }
        "const_declaration" => {
            extract_php_const_symbols(node, ctx, parent_symbol.as_ref(), scope);
        }
        "use_declaration" => {
            // Trait inclusion inside class/trait body — emit one Type reference per
            // trait and stamp the enclosing type with `uses_trait:<name>`. The
            // optional `use_list` block carries `insteadof` / `as` clauses that
            // we record as a class-level attribute but do not model in detail.
            extract_php_trait_use(node, ctx, parent_symbol.as_ref());
        }
        "function_call_expression" => {
            extract_php_function_call(node, ctx, owner_symbol.clone());
            // Suppress descent into eval() arguments per the spec (the literal
            // body otherwise leaks as identifiers/refs the resolver cannot
            // bind).
            if php_call_callee_name(node, ctx.source).as_deref() == Some("eval") {
                return;
            }
        }
        "member_call_expression" | "nullsafe_member_call_expression" => {
            extract_php_member_call(node, ctx, owner_symbol.clone());
        }
        "scoped_call_expression" => {
            extract_php_scoped_call(node, ctx, owner_symbol.clone());
        }
        "object_creation_expression" => {
            extract_php_object_creation(node, ctx, owner_symbol.clone());
        }
        "qualified_name" if !is_php_declaration_name(node) => {
            extract_php_reference(node, ReferenceKind::Path, ctx, owner_symbol.clone());
        }
        "named_type" => {
            extract_php_reference(node, ReferenceKind::Type, ctx, owner_symbol.clone());
        }
        // `name` nodes that aren't declaration heads cover everything squeezy
        // treats as a plain identifier reference. Exclude declaration heads
        // and callee positions (already emitted by the call handlers above);
        // `member_access` field names are routed through the field branch
        // below.
        "name" if !is_php_declaration_name(node) && !is_php_call_name(node) => {
            extract_php_reference(node, ReferenceKind::Identifier, ctx, owner_symbol.clone());
        }
        "member_access_expression" | "nullsafe_member_access_expression" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                extract_php_reference(name_node, ReferenceKind::Field, ctx, owner_symbol.clone());
            }
        }
        "scoped_property_access_expression" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                extract_php_reference(name_node, ReferenceKind::Field, ctx, owner_symbol.clone());
            }
        }
        kind if is_php_literal(kind) => {
            extract_body_hit(node, BodyHitKind::Literal, ctx, owner_symbol.clone());
        }
        _ => {}
    }

    visit_php_children(node, ctx, parent_symbol, owner_symbol, scope);
}

fn visit_php_children(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<(SymbolId, SymbolKind)>,
    owner_symbol: Option<SymbolId>,
    scope: &mut PhpScope,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit_php_node(
            child,
            ctx,
            parent_symbol.clone(),
            owner_symbol.clone(),
            scope,
        );
    }
}

fn php_namespace_symbol(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    raw_name: &str,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> Option<ParsedSymbol> {
    let trimmed = raw_name.trim();
    if trimmed.is_empty() {
        return None;
    }
    let dotted = trimmed.replace('\\', ".");
    let span = span_from_node(node);
    let body = node.child_by_field_name("body");
    let signature = signature_text(node, body, ctx.source);
    let parent_id = parent_symbol.map(|(id, _)| id.clone());
    let id = symbol_id(
        &ctx.file,
        parent_id.as_ref(),
        SymbolKind::Module,
        &dotted,
        span,
    );
    Some(ParsedSymbol {
        id,
        file_id: ctx.file.id.clone(),
        parent_id,
        name: dotted.clone(),
        kind: SymbolKind::Module,
        language_identity: Some(format!("N:{dotted}")),
        span,
        body_span: body.map(span_from_node),
        signature_span: signature_span_from_nodes(node, body),
        signature,
        visibility: None,
        docs: Vec::new(),
        attributes: vec!["php:namespace".to_string()],
        provenance: Provenance::new(PROVENANCE, format!("{} declaration", node.kind())),
        confidence: Confidence::ExactSyntax,
        freshness: Freshness::Fresh,
        arity: None,
    })
}

fn php_symbol_from_node(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
    scope: &PhpScope,
) -> Option<ParsedSymbol> {
    let mut kind = match node.kind() {
        "class_declaration" => SymbolKind::Class,
        "interface_declaration" => SymbolKind::Interface,
        "trait_declaration" => SymbolKind::Trait,
        "enum_declaration" => SymbolKind::Enum,
        "enum_case" => SymbolKind::Variant,
        "function_definition" => SymbolKind::Function,
        "method_declaration" => SymbolKind::Method,
        _ => return None,
    };

    let name = php_symbol_name(node, ctx.source)?;
    if name.is_empty() {
        return None;
    }

    let attributes_raw = php_attribute_strings(node, ctx.source);
    let modifiers = php_modifier_strings(node, ctx.source);
    let mut attributes = php_semantic_attributes(node, &attributes_raw, &modifiers);
    let mut confidence = Confidence::ExactSyntax;
    if matches!(kind, SymbolKind::Method) && is_php_magic_method(&name) {
        attributes.push("php:magic".to_string());
        // The declaration itself stays Exact; only call sites that resolve
        // against magic-method names (handled in extract_php_member_call /
        // extract_php_scoped_call) get downgraded.
    }
    if matches!(kind, SymbolKind::Method) && name == "__construct" {
        attributes.push("php:ctor".to_string());
    }
    if matches!(kind, SymbolKind::Method) && name == "__destruct" {
        attributes.push("php:dtor".to_string());
    }
    // PHPUnit promotes a method to a `Test` symbol when it carries a `#[Test]`/
    // `#[DataProvider]` attribute, is named `test*`, or lives in a `*Test.php`
    // file / `TestCase`-derived class. Mirrors the C# `Fact`/`Test` promotion.
    if matches!(kind, SymbolKind::Method)
        && !is_php_magic_method(&name)
        && php_is_test_method(
            node,
            &name,
            &attributes_raw,
            ctx.source,
            &ctx.file.relative_path,
        )
    {
        kind = SymbolKind::Test;
        attributes.push("php:test".to_string());
    }
    if let Some(namespace) = scope.current_namespace() {
        attributes.push(format!("php:namespace:{namespace}"));
    }
    // class extends/implements bases produce both `base:<leaf>` attributes
    // (downstream resolution synthesizes `Extends`/`Implements` edges from
    // these) and matching `Type` references via `extract_php_symbol_facts`.
    if matches!(
        node.kind(),
        "class_declaration" | "interface_declaration" | "enum_declaration"
    ) {
        for base in php_collect_base_types(node, ctx.source) {
            attributes.push(format!("base:{base}"));
        }
    }
    if node.kind() == "enum_declaration"
        && let Some(primitive) = php_enum_backing_type(node, ctx.source)
    {
        attributes.push(format!("php:backed:{primitive}"));
    }
    attributes.sort();
    attributes.dedup();

    if attributes
        .iter()
        .any(|attribute| attribute == "php:partial-parse")
    {
        confidence = Confidence::Partial;
    }
    let docs = php_doc_comments(node, ctx.source);

    let span = span_from_node(node);
    let body = node.child_by_field_name("body");
    let signature = signature_text(node, body, ctx.source);
    let parent_id = parent_symbol.map(|(id, _)| id.clone());
    let visibility = php_visibility(&modifiers);
    let id = symbol_id(&ctx.file, parent_id.as_ref(), kind, &name, span);
    let language_identity = php_language_identity(kind, &name, scope);
    let arity = if matches!(kind, SymbolKind::Method | SymbolKind::Function) {
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
        provenance: Provenance::new(PROVENANCE, format!("{} declaration", node.kind())),
        confidence,
        freshness: Freshness::Fresh,
        arity,
    })
}

fn php_symbol_name(node: Node<'_>, source: &str) -> Option<String> {
    if let Some(name_node) = node.child_by_field_name("name") {
        return node_text(name_node, source)
            .ok()
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty());
    }
    None
}

fn php_symbol_can_own_type_members(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class
            | SymbolKind::Interface
            | SymbolKind::Trait
            | SymbolKind::Enum
            | SymbolKind::Module
    )
}

fn php_language_identity(kind: SymbolKind, name: &str, scope: &PhpScope) -> Option<String> {
    let type_name = php_type_identity_name(scope, name);
    match kind {
        SymbolKind::Class
        | SymbolKind::Interface
        | SymbolKind::Trait
        | SymbolKind::Enum
        | SymbolKind::TypeAlias => Some(format!("T:{type_name}")),
        SymbolKind::Function | SymbolKind::Method | SymbolKind::Test => {
            let owner = php_member_owner_identity(scope)?;
            Some(format!("M:{owner}.{name}"))
        }
        SymbolKind::Field => {
            let owner = php_member_owner_identity(scope)?;
            Some(format!("F:{owner}.{name}"))
        }
        SymbolKind::Variant => {
            let owner = php_member_owner_identity(scope)?;
            Some(format!("F:{owner}.{name}"))
        }
        _ => None,
    }
}

fn php_type_identity_name(scope: &PhpScope, name: &str) -> String {
    let mut parts = Vec::new();
    if let Some(namespace) = scope.current_namespace() {
        parts.push(namespace);
    }
    parts.extend(scope.type_path.iter().cloned());
    parts.push(name.to_string());
    parts.join(".")
}

fn php_member_owner_identity(scope: &PhpScope) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(namespace) = scope.current_namespace() {
        parts.push(namespace);
    }
    parts.extend(scope.type_path.iter().cloned());
    if parts.is_empty() {
        return None;
    }
    if !scope.callable_path.is_empty() {
        parts.extend(scope.callable_path.iter().cloned());
    }
    Some(parts.join("."))
}

fn php_field_text(node: Node<'_>, field: &str, source: &str) -> Option<String> {
    let child = node.child_by_field_name(field)?;
    node_text(child, source)
        .ok()
        .map(|text| text.trim().to_string())
}

fn php_qualified_segments(raw: &str) -> Vec<String> {
    raw.split('\\')
        .map(|segment| segment.trim().to_string())
        .filter(|segment| !segment.is_empty())
        .collect()
}

fn php_attribute_strings(node: Node<'_>, source: &str) -> Vec<String> {
    // PHP 8 `#[Foo(...)]` blocks attach as a single `attribute_list` field
    // whose grandchildren are `attribute` nodes. Mirror the C# extractor by
    // collecting each attribute's raw text.
    let mut attributes = Vec::new();
    let Some(list) = node.child_by_field_name("attributes") else {
        return attributes;
    };
    let mut cursor = list.walk();
    for group in list.named_children(&mut cursor) {
        if group.kind() != "attribute_group" {
            continue;
        }
        let mut inner = group.walk();
        for attribute in group.named_children(&mut inner) {
            if attribute.kind() == "attribute"
                && let Ok(text) = node_text(attribute, source)
            {
                attributes.push(text.trim().to_string());
            }
        }
    }
    attributes
}

fn php_modifier_strings(node: Node<'_>, source: &str) -> Vec<String> {
    // Each modifier sits as a separate child node — visibility_modifier,
    // static_modifier, abstract_modifier, final_modifier, readonly_modifier,
    // var_modifier. We treat every such child's text as a modifier string.
    let mut modifiers = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "visibility_modifier"
            | "static_modifier"
            | "abstract_modifier"
            | "final_modifier"
            | "readonly_modifier"
            | "var_modifier"
            | "reference_modifier" => {
                if let Ok(text) = node_text(child, source) {
                    modifiers.push(text.trim().to_string());
                }
            }
            _ => {}
        }
    }
    modifiers
}

fn php_visibility(modifiers: &[String]) -> Option<String> {
    for visibility in ["public", "protected", "private"] {
        if modifiers.iter().any(|modifier| modifier == visibility) {
            return Some(visibility.to_string());
        }
    }
    None
}

fn php_semantic_attributes(
    _node: Node<'_>,
    attributes_raw: &[String],
    modifiers: &[String],
) -> Vec<String> {
    let mut attributes = Vec::new();
    for modifier in modifiers {
        attributes.push(format!("php:modifier:{modifier}"));
        match modifier.as_str() {
            "static" => attributes.push("php:static".to_string()),
            "abstract" => attributes.push("php:abstract".to_string()),
            "final" => attributes.push("php:final".to_string()),
            "readonly" => attributes.push("php:readonly".to_string()),
            _ => {}
        }
    }
    for attribute in attributes_raw {
        let cleaned = php_attribute_head(attribute);
        if cleaned.is_empty() {
            continue;
        }
        attributes.push(format!("php:attr:{cleaned}"));
        match cleaned.as_str() {
            // Symfony framework attribute heuristics — mirrors the
            // C# ApiController/Route handling in csharp.rs.
            "Route" => {
                attributes.push("framework:symfony".to_string());
                attributes.push("framework:web-route".to_string());
                if let Some(path) = first_php_string_literal(attribute) {
                    attributes.push(format!("route:{path}"));
                }
            }
            "AsCommand" => {
                attributes.push("framework:symfony".to_string());
                attributes.push("framework:console".to_string());
            }
            "AsController" => {
                attributes.push("framework:symfony".to_string());
                attributes.push("framework:web-route".to_string());
            }
            _ => {}
        }
    }
    attributes
}

fn php_attribute_head(attribute: &str) -> String {
    let body = attribute
        .trim()
        .trim_start_matches("#[")
        .trim_end_matches(']')
        .trim();
    let head = body.split('(').next().unwrap_or(body).trim();
    // PHP attribute names can be `Foo\Bar`; take the leaf for parity with
    // the C# heuristic.
    let head = head.rsplit('\\').next().unwrap_or(head).trim();
    head.to_string()
}

fn first_php_string_literal(text: &str) -> Option<String> {
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        let quote = match ch {
            '"' | '\'' => ch,
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

fn php_doc_comments(node: Node<'_>, source: &str) -> Vec<String> {
    let mut docs = Vec::new();
    let mut walker = node;
    while let Some(previous) = walker.prev_named_sibling() {
        walker = previous;
        match previous.kind() {
            "comment" => {
                if let Ok(text) = node_text(previous, source) {
                    let trimmed = text.trim();
                    if trimmed.starts_with("/**") {
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

fn extract_php_use_declaration(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    // Map every `namespace_use_clause` under this declaration to a single
    // `ParsedImport`. Group `use Foo\{Bar, Baz as Q};` syntax surfaces the
    // prefix in the first child `namespace_name`, and each clause carries
    // either a name or qualified_name; we splice the prefix onto the leaf
    // when building each import path.
    let leading_prefix = php_namespace_use_prefix(node, ctx.source);
    let use_type = php_namespace_use_type(node, ctx.source);
    let attributes = php_namespace_use_attributes(use_type.as_deref());
    let span = span_from_node(node);
    let provenance = Provenance::new(PROVENANCE, "namespace_use_declaration");

    let mut emitted = false;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "namespace_use_clause" => {
                if let Some(import) = php_use_clause_to_import(
                    child,
                    ctx,
                    owner_id.clone(),
                    leading_prefix.as_deref(),
                    use_type.as_deref(),
                    span,
                    provenance.clone(),
                    &attributes,
                ) {
                    ctx.imports.push(import);
                    emitted = true;
                }
            }
            "namespace_use_group" => {
                let mut group_cursor = child.walk();
                for clause in child.named_children(&mut group_cursor) {
                    if clause.kind() != "namespace_use_clause" {
                        continue;
                    }
                    if let Some(import) = php_use_clause_to_import(
                        clause,
                        ctx,
                        owner_id.clone(),
                        leading_prefix.as_deref(),
                        use_type.as_deref(),
                        span,
                        provenance.clone(),
                        &attributes,
                    ) {
                        ctx.imports.push(import);
                        emitted = true;
                    }
                }
            }
            _ => {}
        }
    }

    // A `use Foo\Bar;` with no clauses is parsed as a single namespace_name
    // child of the declaration; treat the declaration text as a Named import
    // in that case.
    if !emitted {
        let raw = node_text(node, ctx.source).unwrap_or_default();
        let trimmed = raw.trim().trim_end_matches(';').trim();
        let body = trimmed.strip_prefix("use").unwrap_or(trimmed).trim();
        let body = body.strip_prefix("function ").unwrap_or(body);
        let body = body.strip_prefix("const ").unwrap_or(body);
        let parts = body.split('\\').collect::<Vec<_>>();
        if parts.is_empty() {
            return;
        }
        let path_raw = parts
            .iter()
            .map(|segment| segment.trim())
            .collect::<Vec<_>>();
        let path_dotted = path_raw.join(".");
        if path_dotted.trim().trim_matches('.').is_empty() {
            return;
        }
        let imported_name = path_raw
            .last()
            .map(|leaf| leaf.to_string())
            .filter(|leaf| !leaf.is_empty());
        ctx.imports.push(ParsedImport {
            file_id: ctx.file.id.clone(),
            owner_id,
            path: path_dotted,
            alias: None,
            is_glob: false,
            is_reexport: false,
            is_static: false,
            span,
            provenance,
            kind: ImportKind::Named,
            imported_name,
            is_global: false,
        });
        // Stamp `php:use-function` / `php:use-const` via the file-level
        // attributes path; we cannot mutate the import in this branch, so the
        // attribute is implicit in the use_type prefix we already retained.
        let _ = attributes;
    }
}

fn php_namespace_use_prefix(node: Node<'_>, source: &str) -> Option<String> {
    // For `use Foo\{Bar, Baz}` the leading namespace_name sits as a named
    // child before the namespace_use_group block.
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "namespace_name" => {
                return node_text(child, source)
                    .ok()
                    .map(|text| text.trim().to_string());
            }
            "namespace_use_clause" | "namespace_use_group" => break,
            _ => {}
        }
    }
    None
}

fn php_namespace_use_type(node: Node<'_>, source: &str) -> Option<String> {
    node.child_by_field_name("type")
        .and_then(|node| node_text(node, source).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

fn php_namespace_use_attributes(use_type: Option<&str>) -> Vec<String> {
    let mut attributes = Vec::new();
    match use_type {
        Some("function") => attributes.push("php:use-function".to_string()),
        Some("const") => attributes.push("php:use-const".to_string()),
        _ => {}
    }
    attributes
}

#[allow(clippy::too_many_arguments)]
fn php_use_clause_to_import(
    clause: Node<'_>,
    ctx: &ExtractContext<'_>,
    owner_id: Option<SymbolId>,
    leading_prefix: Option<&str>,
    use_type: Option<&str>,
    span: SourceSpan,
    provenance: Provenance,
    attributes: &[String],
) -> Option<ParsedImport> {
    let alias = clause
        .child_by_field_name("alias")
        .and_then(|node| node_text(node, ctx.source).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());
    let mut cursor = clause.walk();
    let raw_path = clause
        .named_children(&mut cursor)
        .find(|child| matches!(child.kind(), "qualified_name" | "name"))
        .and_then(|node| node_text(node, ctx.source).ok())
        .map(|text| text.trim().to_string())?;
    let combined = match (leading_prefix, raw_path.as_str()) {
        (Some(prefix), leaf) if !prefix.is_empty() => format!("{prefix}\\{leaf}"),
        _ => raw_path.clone(),
    };
    let path_dotted = combined
        .split('\\')
        .map(|segment| segment.trim())
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join(".");
    if path_dotted.is_empty() {
        return None;
    }
    let imported_name = path_dotted.rsplit('.').next().map(|leaf| leaf.to_string());

    let _ = use_type;
    let _ = attributes; // captured into provenance only — ParsedImport has no
    // attribute slot, so framework-aware downstream code uses the import path
    // shape to dispatch and the provenance reason to disambiguate.
    Some(ParsedImport {
        file_id: ctx.file.id.clone(),
        owner_id,
        path: path_dotted,
        alias,
        is_glob: false,
        is_reexport: false,
        is_static: false,
        span,
        provenance,
        kind: ImportKind::Named,
        imported_name,
        is_global: false,
    })
}

fn extract_php_trait_use(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) {
    let Some((parent_id, parent_kind)) = parent_symbol else {
        return;
    };
    if !matches!(
        parent_kind,
        SymbolKind::Class | SymbolKind::Trait | SymbolKind::Enum
    ) {
        return;
    }
    let mut cursor = node.walk();
    let mut had_resolution = false;
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "name" | "qualified_name" | "relative_name" => {
                let Ok(text) = node_text(child, ctx.source) else {
                    continue;
                };
                let trimmed = text.trim().to_string();
                if trimmed.is_empty() {
                    continue;
                }
                let leaf = trimmed.rsplit('\\').next().unwrap_or(&trimmed).to_string();
                ctx.references.push(ParsedReference {
                    file_id: ctx.file.id.clone(),
                    owner_id: Some(parent_id.clone()),
                    text: trimmed.clone(),
                    kind: ReferenceKind::Type,
                    span: span_from_node(child),
                    provenance: Provenance::new(PROVENANCE, "trait use reference"),
                });
                // Stamp the parent class with `uses_trait:<leaf>` so the spec
                // smoke gate (and downstream graph edges, once UsesTrait
                // exists) can find it.
                if let Some(parent) = ctx
                    .symbols
                    .iter_mut()
                    .find(|symbol| symbol.id == *parent_id)
                {
                    parent.attributes.push(format!("uses_trait:{leaf}"));
                    parent.attributes.sort();
                    parent.attributes.dedup();
                }
            }
            "use_list" => had_resolution = true,
            _ => {}
        }
    }
    if had_resolution
        && let Some(parent) = ctx
            .symbols
            .iter_mut()
            .find(|symbol| symbol.id == *parent_id)
    {
        parent.attributes.push("php:trait-resolution".to_string());
        parent.attributes.sort();
        parent.attributes.dedup();
    }
}

fn extract_php_function_call(
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
    if name.is_empty() {
        return;
    }
    let arity = node
        .child_by_field_name("arguments")
        .map(|arguments| named_child_count(arguments))
        .unwrap_or_default();
    let is_eval = name == "eval";
    let is_guard = matches!(
        name.as_str(),
        "class_exists" | "function_exists" | "interface_exists" | "trait_exists"
    );
    let reason = if is_eval {
        "function_call_expression eval"
    } else if is_guard {
        "function_call_expression guard"
    } else {
        "function_call_expression"
    };
    // Direct calls land at Heuristic by default — guard calls keep the same
    // baseline confidence; resolution downstream is what actually distinguishes
    // them via the provenance `reason` tag.
    let confidence = Confidence::Heuristic;
    let _ = is_guard;
    ctx.calls.push(ParsedCall {
        file_id: ctx.file.id.clone(),
        caller_id: owner_id.clone(),
        name,
        target_text: target_text.clone(),
        receiver: receiver_from_direct_call(&target_text),
        arity,
        kind: ParsedCallKind::Direct,
        span: span_from_node(node),
        provenance: Provenance::new(PROVENANCE, reason),
        confidence,
    });
    extract_body_hit(node, BodyHitKind::Call, ctx, owner_id);
}

fn php_call_callee_name(node: Node<'_>, source: &str) -> Option<String> {
    let function_node = node.child_by_field_name("function")?;
    let raw = node_text(function_node, source).ok()?;
    Some(last_path_segment(raw))
}

fn extract_php_member_call(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = node_text(name_node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if name.is_empty() {
        return;
    }
    let receiver = node
        .child_by_field_name("object")
        .and_then(|receiver| node_text(receiver, ctx.source).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());
    let arity = node
        .child_by_field_name("arguments")
        .map(|arguments| named_child_count(arguments))
        .unwrap_or_default();
    let target_text = node_text(node, ctx.source)
        .unwrap_or_default()
        .split('(')
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    let mut confidence = Confidence::Heuristic;
    if is_php_implicit_dispatch_target(&name) {
        confidence = Confidence::Partial;
    }
    ctx.calls.push(ParsedCall {
        file_id: ctx.file.id.clone(),
        caller_id: owner_id.clone(),
        name,
        target_text,
        receiver,
        arity,
        kind: ParsedCallKind::Method,
        span: span_from_node(node),
        provenance: Provenance::new(PROVENANCE, "member_call_expression"),
        confidence,
    });
    extract_body_hit(node, BodyHitKind::Call, ctx, owner_id);
}

fn extract_php_scoped_call(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = node_text(name_node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if name.is_empty() {
        return;
    }
    let receiver = node
        .child_by_field_name("scope")
        .and_then(|scope| node_text(scope, ctx.source).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());
    let arity = node
        .child_by_field_name("arguments")
        .map(|arguments| named_child_count(arguments))
        .unwrap_or_default();
    let target_text = node_text(node, ctx.source)
        .unwrap_or_default()
        .split('(')
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    let mut confidence = Confidence::Heuristic;
    if is_php_implicit_dispatch_target(&name) {
        confidence = Confidence::Partial;
    }
    ctx.calls.push(ParsedCall {
        file_id: ctx.file.id.clone(),
        caller_id: owner_id.clone(),
        name,
        target_text,
        receiver: receiver.clone(),
        arity,
        kind: ParsedCallKind::Method,
        span: span_from_node(node),
        provenance: Provenance::new(PROVENANCE, "scoped_call_expression"),
        confidence,
    });
    // Static call sites also stamp the receiver as a type reference so
    // resolver edges can land on the referenced class.
    if let Some(receiver_text) = receiver
        && !receiver_text.starts_with('$')
        && !receiver_text.is_empty()
    {
        ctx.references.push(ParsedReference {
            file_id: ctx.file.id.clone(),
            owner_id: owner_id.clone(),
            text: receiver_text,
            kind: ReferenceKind::Type,
            span: span_from_node(node),
            provenance: Provenance::new(PROVENANCE, "scoped call receiver"),
        });
    }
    extract_body_hit(node, BodyHitKind::Call, ctx, owner_id);
}

fn extract_php_object_creation(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let mut cursor = node.walk();
    let mut type_text: Option<(String, bool)> = None;
    let mut arity = 0usize;
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "name" | "qualified_name" | "relative_name" if type_text.is_none() => {
                let raw = node_text(child, ctx.source)
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if !raw.is_empty() {
                    type_text = Some((raw, false));
                }
            }
            "variable_name" if type_text.is_none() => {
                let raw = node_text(child, ctx.source)
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if !raw.is_empty() {
                    type_text = Some((raw, true));
                }
            }
            "arguments" => arity = named_child_count(child),
            _ => {}
        }
    }
    let Some((target_text, dynamic)) = type_text else {
        return;
    };
    let (name, confidence, reason) = if dynamic {
        (
            "<dynamic>".to_string(),
            Confidence::Partial,
            "object_creation_expression dynamic",
        )
    } else {
        (
            last_path_segment(&target_text),
            Confidence::Heuristic,
            "object_creation_expression",
        )
    };
    if name.is_empty() {
        return;
    }
    ctx.calls.push(ParsedCall {
        file_id: ctx.file.id.clone(),
        caller_id: owner_id.clone(),
        name,
        target_text: target_text.clone(),
        receiver: receiver_from_direct_call(&target_text),
        arity,
        kind: ParsedCallKind::Direct,
        span: span_from_node(node),
        provenance: Provenance::new(PROVENANCE, reason),
        confidence,
    });
    if !dynamic {
        ctx.references.push(ParsedReference {
            file_id: ctx.file.id.clone(),
            owner_id: owner_id.clone(),
            text: target_text,
            kind: ReferenceKind::Type,
            span: span_from_node(node),
            provenance: Provenance::new(PROVENANCE, "object_creation_expression"),
        });
    }
    extract_body_hit(node, BodyHitKind::Call, ctx, owner_id);
}

fn extract_php_reference(
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
    if php_is_keyword_or_predefined(&text) {
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
        provenance: Provenance::new(PROVENANCE, format!("{} reference", node.kind())),
    });
    ctx.body_hits.push(BodyHit {
        file_id: ctx.file.id.clone(),
        owner_id,
        text,
        kind: body_kind,
        span: span_from_node(node),
    });
}

fn extract_php_property_symbols(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
    scope: &PhpScope,
) {
    let Some((parent_id, parent_kind)) = parent_symbol else {
        return;
    };
    if !matches!(
        parent_kind,
        SymbolKind::Class | SymbolKind::Trait | SymbolKind::Interface | SymbolKind::Enum
    ) {
        return;
    }
    let attributes_raw = php_attribute_strings(node, ctx.source);
    let modifiers = php_modifier_strings(node, ctx.source);
    let mut base_attributes = php_semantic_attributes(node, &attributes_raw, &modifiers);
    base_attributes.push("php:field".to_string());
    let type_text = node
        .child_by_field_name("type")
        .and_then(|node| node_text(node, ctx.source).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "property_element" {
            continue;
        }
        let Some(name_node) = child.child_by_field_name("name") else {
            continue;
        };
        let Ok(raw_name) = node_text(name_node, ctx.source) else {
            continue;
        };
        let name = raw_name.trim().trim_start_matches('$').to_string();
        if name.is_empty() {
            continue;
        }
        let span = span_from_node(child);
        let mut attributes = base_attributes.clone();
        if let Some(type_text) = type_text.clone() {
            attributes.push(format!("type:{}", last_path_segment(&type_text)));
        }
        attributes.sort();
        attributes.dedup();
        let signature =
            signature_text(node, child.child_by_field_name("default_value"), ctx.source);
        let id = symbol_id(&ctx.file, Some(parent_id), SymbolKind::Field, &name, span);
        ctx.symbols.push(ParsedSymbol {
            id,
            file_id: ctx.file.id.clone(),
            parent_id: Some(parent_id.clone()),
            name: name.clone(),
            kind: SymbolKind::Field,
            language_identity: php_language_identity(SymbolKind::Field, &name, scope),
            span,
            body_span: None,
            signature_span: None,
            signature,
            visibility: php_visibility(&modifiers),
            docs: Vec::new(),
            attributes,
            provenance: Provenance::new(PROVENANCE, "property declaration"),
            confidence: Confidence::ExactSyntax,
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
                provenance: Provenance::new(PROVENANCE, "property type reference"),
            });
        }
    }
}

fn extract_php_const_symbols(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
    scope: &PhpScope,
) {
    let Some((parent_id, parent_kind)) = parent_symbol else {
        // Per the spec, top-level `const FOO = 1;` is not emitted as a symbol
        // in v1; only class/enum/trait/interface body consts surface as Field.
        return;
    };
    if !matches!(
        parent_kind,
        SymbolKind::Class | SymbolKind::Interface | SymbolKind::Trait | SymbolKind::Enum
    ) {
        return;
    }
    let attributes_raw = php_attribute_strings(node, ctx.source);
    let modifiers = php_modifier_strings(node, ctx.source);
    let mut base_attributes = php_semantic_attributes(node, &attributes_raw, &modifiers);
    base_attributes.push("php:const".to_string());
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "const_element" {
            continue;
        }
        let mut name_cursor = child.walk();
        let Some(name_node) = child.named_children(&mut name_cursor).next() else {
            continue;
        };
        let Ok(raw_name) = node_text(name_node, ctx.source) else {
            continue;
        };
        let name = raw_name.trim().to_string();
        if name.is_empty() {
            continue;
        }
        let span = span_from_node(child);
        let mut attributes = base_attributes.clone();
        attributes.sort();
        attributes.dedup();
        let signature = signature_text(node, None, ctx.source);
        let id = symbol_id(&ctx.file, Some(parent_id), SymbolKind::Field, &name, span);
        ctx.symbols.push(ParsedSymbol {
            id,
            file_id: ctx.file.id.clone(),
            parent_id: Some(parent_id.clone()),
            name: name.clone(),
            kind: SymbolKind::Field,
            language_identity: php_language_identity(SymbolKind::Field, &name, scope),
            span,
            body_span: None,
            signature_span: None,
            signature,
            visibility: php_visibility(&modifiers),
            docs: Vec::new(),
            attributes,
            provenance: Provenance::new(PROVENANCE, "const declaration"),
            confidence: Confidence::ExactSyntax,
            freshness: Freshness::Fresh,
            arity: None,
        });
    }
}

fn php_collect_base_types(node: Node<'_>, source: &str) -> Vec<String> {
    let mut bases = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !matches!(child.kind(), "base_clause" | "class_interface_clause") {
            continue;
        }
        let mut inner = child.walk();
        for base in child.named_children(&mut inner) {
            if let Ok(text) = node_text(base, source)
                && let Some(name) = php_type_name_from_annotation(text)
            {
                bases.push(name);
            }
        }
    }
    bases.sort();
    bases.dedup();
    bases
}

fn php_enum_backing_type(node: Node<'_>, source: &str) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "primitive_type"
            && let Ok(text) = node_text(child, source)
        {
            return Some(text.trim().to_string());
        }
    }
    None
}

fn extract_php_symbol_facts(
    node: Node<'_>,
    symbol: &ParsedSymbol,
    ctx: &mut ExtractContext<'_>,
    scope: &PhpScope,
) {
    if matches!(
        node.kind(),
        "class_declaration" | "interface_declaration" | "enum_declaration"
    ) {
        for base in php_collect_base_types(node, ctx.source) {
            ctx.references.push(ParsedReference {
                file_id: ctx.file.id.clone(),
                owner_id: Some(symbol.id.clone()),
                text: base,
                kind: ReferenceKind::Type,
                span: symbol.span,
                provenance: Provenance::new(PROVENANCE, "base type reference"),
            });
        }
    }
    if matches!(node.kind(), "method_declaration" | "function_definition") {
        if let Some(parameters) = node.child_by_field_name("parameters") {
            let mut cursor = parameters.walk();
            for parameter in parameters.named_children(&mut cursor) {
                if let Some(type_node) = parameter.child_by_field_name("type") {
                    push_php_type_reference(type_node, symbol, ctx, "parameter type reference");
                }
            }
        }
        if let Some(return_type) = node.child_by_field_name("return_type") {
            push_php_type_reference(return_type, symbol, ctx, "return type reference");
        }
    }
    // Constructor property promotion (`public readonly Foo $bar` in
    // `__construct`) mints class-parented Field symbols. Promoted properties
    // are the dominant modern PHP/Symfony/Laravel state declaration, so they
    // need to surface as real `Field` symbols rather than vanishing into a
    // single constructor-owned parameter type reference.
    if node.kind() == "method_declaration" && symbol.name == "__construct" {
        extract_php_promoted_fields(node, symbol, ctx, scope);
    }
}

fn extract_php_promoted_fields(
    method_node: Node<'_>,
    method_symbol: &ParsedSymbol,
    ctx: &mut ExtractContext<'_>,
    scope: &PhpScope,
) {
    // Promoted fields are parented to the enclosing class, which is the
    // constructor's own parent. Without a class parent (e.g. a stray
    // `__construct` at file scope) there is nothing to promote onto.
    let Some(class_id) = method_symbol.parent_id.clone() else {
        return;
    };
    let Some(parameters) = method_node.child_by_field_name("parameters") else {
        return;
    };
    let mut cursor = parameters.walk();
    for parameter in parameters.named_children(&mut cursor) {
        if parameter.kind() != "property_promotion_parameter" {
            continue;
        }
        let Some(name_node) = parameter.child_by_field_name("name") else {
            continue;
        };
        let Ok(raw_name) = node_text(name_node, ctx.source) else {
            continue;
        };
        let name = raw_name.trim().trim_start_matches('$').to_string();
        if name.is_empty() {
            continue;
        }
        let attributes_raw = php_attribute_strings(parameter, ctx.source);
        let modifiers = php_modifier_strings(parameter, ctx.source);
        let mut attributes = php_semantic_attributes(parameter, &attributes_raw, &modifiers);
        attributes.push("php:field".to_string());
        attributes.push("php:promoted".to_string());
        let type_text = parameter
            .child_by_field_name("type")
            .and_then(|node| node_text(node, ctx.source).ok())
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty());
        if let Some(type_text) = type_text.clone() {
            attributes.push(format!("type:{}", last_path_segment(&type_text)));
        }
        attributes.sort();
        attributes.dedup();
        let span = span_from_node(parameter);
        let id = symbol_id(&ctx.file, Some(&class_id), SymbolKind::Field, &name, span);
        ctx.symbols.push(ParsedSymbol {
            id,
            file_id: ctx.file.id.clone(),
            parent_id: Some(class_id.clone()),
            name: name.clone(),
            kind: SymbolKind::Field,
            language_identity: php_language_identity(SymbolKind::Field, &name, scope),
            span,
            body_span: None,
            signature_span: None,
            signature: node_text(parameter, ctx.source)
                .unwrap_or_default()
                .trim()
                .to_string(),
            visibility: php_visibility(&modifiers),
            docs: Vec::new(),
            attributes,
            provenance: Provenance::new(PROVENANCE, "constructor property promotion"),
            confidence: Confidence::ExactSyntax,
            freshness: Freshness::Fresh,
            arity: None,
        });
        if let Some(type_text) = type_text {
            for type_name in php_type_names_from_annotation(&type_text) {
                ctx.references.push(ParsedReference {
                    file_id: ctx.file.id.clone(),
                    owner_id: Some(class_id.clone()),
                    text: type_name,
                    kind: ReferenceKind::Type,
                    span,
                    provenance: Provenance::new(PROVENANCE, "promoted property type reference"),
                });
            }
        }
    }
}

fn push_php_type_reference(
    type_node: Node<'_>,
    symbol: &ParsedSymbol,
    ctx: &mut ExtractContext<'_>,
    reason: &'static str,
) {
    let Ok(text) = node_text(type_node, ctx.source) else {
        return;
    };
    for name in php_type_names_from_annotation(text) {
        ctx.references.push(ParsedReference {
            file_id: ctx.file.id.clone(),
            owner_id: Some(symbol.id.clone()),
            text: name,
            kind: ReferenceKind::Type,
            span: symbol.span,
            provenance: Provenance::new(PROVENANCE, reason),
        });
    }
}

fn php_type_names_from_annotation(annotation: &str) -> Vec<String> {
    // Union/intersection types resolve to their components. Split on `|` and
    // `&` (PHP 8.1+), strip nullable `?` prefixes, and collapse to leaf names.
    annotation
        .split(['|', '&'])
        .filter_map(|segment| php_type_name_from_annotation(segment.trim()))
        .collect()
}

fn php_type_name_from_annotation(annotation: &str) -> Option<String> {
    let trimmed = annotation.trim().trim_start_matches('?').trim().to_string();
    if trimmed.is_empty() {
        return None;
    }
    let leaf = trimmed
        .rsplit('\\')
        .next()
        .unwrap_or(&trimmed)
        .trim()
        .to_string();
    if php_is_keyword_or_predefined(&leaf) {
        return None;
    }
    Some(leaf)
}

fn is_php_declaration_name(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if let Some(name_node) = parent.child_by_field_name("name")
        && name_node.id() == node.id()
    {
        return true;
    }
    if let Some(alias_node) = parent.child_by_field_name("alias")
        && alias_node.id() == node.id()
    {
        return true;
    }
    matches!(
        parent.kind(),
        "namespace_use_clause"
            | "namespace_use_declaration"
            | "namespace_definition"
            | "namespace_name"
            | "qualified_name"
    )
}

fn is_php_call_name(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if matches!(
        parent.kind(),
        "function_call_expression"
            | "member_call_expression"
            | "scoped_call_expression"
            | "nullsafe_member_call_expression"
    ) {
        if let Some(function_node) = parent.child_by_field_name("function")
            && function_node.id() == node.id()
        {
            return true;
        }
        if let Some(name_node) = parent.child_by_field_name("name")
            && name_node.id() == node.id()
        {
            return true;
        }
    }
    false
}

fn is_php_literal(kind: &str) -> bool {
    matches!(
        kind,
        "string" | "encapsed_string" | "integer" | "float" | "boolean" | "null" | "true" | "false"
    )
}

fn is_php_magic_method(name: &str) -> bool {
    matches!(
        name,
        "__construct"
            | "__destruct"
            | "__call"
            | "__callStatic"
            | "__get"
            | "__set"
            | "__isset"
            | "__unset"
            | "__invoke"
            | "__toString"
            | "__clone"
            | "__sleep"
            | "__wakeup"
            | "__serialize"
            | "__unserialize"
            | "__set_state"
            | "__debugInfo"
    )
}

fn php_is_test_method(
    node: Node<'_>,
    name: &str,
    attributes_raw: &[String],
    source: &str,
    relative_path: &str,
) -> bool {
    // PHPUnit attribute markers (`#[Test]`, `#[DataProvider]`, `#[TestWith]`,
    // `#[TestDox]`) are an unambiguous signal.
    if attributes_raw.iter().any(|attribute| {
        matches!(
            php_attribute_head(attribute).as_str(),
            "Test" | "DataProvider" | "TestWith" | "TestDox" | "Group" | "CoversClass"
        )
    }) {
        return true;
    }
    // The classic `test*`-prefix convention, but only inside a test context
    // (a `*Test.php` file or a `TestCase`-derived class) so a stray
    // `testimony()` helper in production code is not misclassified.
    php_method_name_is_test_prefixed(name)
        && (php_is_test_filename(relative_path) || php_method_in_testcase_class(node, source))
}

fn php_method_name_is_test_prefixed(name: &str) -> bool {
    // `test` followed by an uppercase letter or underscore — matches
    // `testFoo`/`test_foo` but not `testimony`.
    let Some(rest) = name.strip_prefix("test") else {
        return false;
    };
    rest.chars()
        .next()
        .is_some_and(|ch| ch == '_' || ch.is_ascii_uppercase())
}

fn php_is_test_filename(relative_path: &str) -> bool {
    let file_name = relative_path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(relative_path);
    let stem = file_name.strip_suffix(".php").unwrap_or(file_name);
    stem.ends_with("Test") || stem.ends_with("TestCase")
}

fn php_method_in_testcase_class(node: Node<'_>, source: &str) -> bool {
    // Walk up to the enclosing class and check its base list for a
    // `TestCase` ancestor (PHPUnit's base class, optionally namespaced).
    let mut walker = node;
    while let Some(parent) = walker.parent() {
        if matches!(parent.kind(), "class_declaration" | "anonymous_class") {
            return php_collect_base_types(parent, source)
                .iter()
                .any(|base| base == "TestCase");
        }
        walker = parent;
    }
    false
}

fn is_php_implicit_dispatch_target(name: &str) -> bool {
    // These magic-method names imply implicit dispatch from a real call site
    // and so should mark the call edge as Partial confidence per spec §4(f).
    matches!(
        name,
        "__call" | "__callStatic" | "__get" | "__set" | "__invoke"
    )
}

fn php_is_keyword_or_predefined(text: &str) -> bool {
    matches!(
        text,
        "self"
            | "static"
            | "parent"
            | "this"
            | "true"
            | "false"
            | "null"
            | "void"
            | "mixed"
            | "never"
            | "int"
            | "integer"
            | "float"
            | "double"
            | "string"
            | "bool"
            | "boolean"
            | "array"
            | "object"
            | "iterable"
            | "callable"
            | "resource"
    )
}

fn dedup_php_facts(ctx: &mut ExtractContext<'_>) {
    let mut import_seen = HashSet::new();
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
    let mut reference_seen = HashSet::new();
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
