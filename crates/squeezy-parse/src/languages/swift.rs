//! Swift extractor.
//!
//! Models class/struct/actor/protocol/enum declarations, extensions
//! (`language_identity` propagation), generics with constraints, property
//! wrappers and other `@attributes`, computed properties, and module
//! imports. See `docs/internal/lang-specs/swift.md` for the contract.
//!
//! Out-of-scope compiler/runtime features (per spec §4):
//! - `@dynamicMemberLookup` runtime member resolution
//! - Full protocol-witness tracking
//! - Objective-C bridging (`.h`/`.m` headers, `@objc(name)` mapping)
//! - SwiftPM `Package.swift` parsing for module facts
//! - `#externalMacro`/`#freestanding` macro resolution

use crate::languages::rust::*;
use crate::*;

pub(crate) fn extract_swift(file: FileRecord, source: &str, tree: &Tree) -> ParsedFile {
    let mut ctx = ExtractContext::new(file.clone(), source);
    let root = tree.root_node();
    record_parse_error_diagnostics(root, &mut ctx);

    visit_swift_node(root, &mut ctx, None, None, None);
    dedup_swift_facts(&mut ctx);

    // Spec §4(i): derive the SwiftPM module name from the
    // `Sources/<Module>/...` layout convention. Stored on
    // `ParsedFile::package` so cross-file resolution can use the module
    // name as a hint without re-walking the file path.
    let package = swift_module_from_path(&file.relative_path);

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

/// Synthetic owner-type name propagated to members declared inside
/// `extension Foo { ... }`. Stored on each member's `language_identity`
/// so the cross-file resolver in `squeezy-graph` can match
/// `foo.bar()` to the extension's `bar` when `foo: Foo` lives in a
/// different file.
type ExtensionOwner<'a> = Option<&'a str>;

fn visit_swift_node(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<(SymbolId, SymbolKind)>,
    owner_symbol: Option<SymbolId>,
    extension_owner: ExtensionOwner<'_>,
) {
    if node.is_missing() {
        record_missing_node_diagnostic(node, ctx);
        return;
    }

    if node.kind() == "import_declaration" {
        extract_swift_import(node, ctx, owner_symbol.clone());
        return;
    }

    // `class_declaration` covers `class`, `struct`, `actor`, `enum`, and
    // `extension` in tree-sitter-swift. Distinguish via the
    // `declaration_kind` field (the leading keyword token).
    if node.kind() == "class_declaration"
        && let Some(extension_name) = swift_extension_extended_type(node, ctx.source)
    {
        // Members of the extension body inherit `language_identity = <Foo>`
        // but no `parent_id` — extensions are not synthesized as symbols.
        // Spec gotcha (a): conformance refs on `extension Foo: Bar { ... }`
        // float to the first member's owner if no member exists; we
        // emit them with `owner_id = None` and `text = Bar` so the
        // graph resolver can attach them after-the-fact.
        for base in swift_inheritance_names(node, ctx.source) {
            ctx.references.push(ParsedReference {
                file_id: ctx.file.id.clone(),
                owner_id: None,
                text: base,
                kind: ReferenceKind::Type,
                span: span_from_node(node),
                provenance: Provenance::new("tree-sitter-swift", "extension inheritance reference"),
            });
        }
        // Walk the body with `extension_owner` set, but no new parent
        // symbol; members emit at file scope with `language_identity`.
        if let Some(body) = node.child_by_field_name("body") {
            visit_swift_children(
                body,
                ctx,
                parent_symbol.clone(),
                owner_symbol.clone(),
                Some(extension_name.as_str()),
            );
        }
        // Walk attributes/modifiers and inheritance specifiers so
        // attributes referenced on the extension itself still surface.
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "modifiers" || child.kind() == "inheritance_specifier" {
                visit_swift_node(
                    child,
                    ctx,
                    parent_symbol.clone(),
                    owner_symbol.clone(),
                    Some(extension_name.as_str()),
                );
            }
        }
        return;
    }

    if node.kind() == "enum_entry" {
        let symbols = swift_enum_entry_symbols(node, ctx, parent_symbol.as_ref());
        for symbol in symbols {
            ctx.symbols.push(symbol);
        }
        visit_swift_children(node, ctx, parent_symbol, owner_symbol, extension_owner);
        return;
    }

    if node.kind() == "property_declaration" || node.kind() == "protocol_property_declaration" {
        // SourceKit-LSP's `documentSymbol` only surfaces stored / computed
        // properties whose owner is a type (class/struct/enum/protocol/
        // actor) or an extension thereof. File-scope `let foo = ...`
        // (kind 13 Variable, no parent) and function-body locals
        // (kind 13 Variable, parent = Method) are intentionally absent.
        // Mirror that filter so the Squeezy/SourceKit symbol scans agree
        // (covers `Package.swift`'s `let package = Package(...)` SwiftPM
        // manifest binding and function-body `let trimmed = ...`).
        if swift_property_owner_is_type(parent_symbol.as_ref(), extension_owner, node.kind()) {
            let symbols = swift_property_symbols_from_node(
                node,
                ctx,
                parent_symbol.as_ref(),
                extension_owner,
            );
            if !symbols.is_empty() {
                let new_owners: Vec<SymbolId> = symbols.iter().map(|s| s.id.clone()).collect();
                for symbol in symbols {
                    ctx.symbols.push(symbol);
                }
                // Treat the property's computed body as the owning context for
                // body-hit / reference extraction. If multiple symbols (pattern
                // `let a, b = ...`), attribute hits to the first.
                let next_owner = new_owners.into_iter().next().or(owner_symbol.clone());
                visit_swift_children(node, ctx, parent_symbol, next_owner, extension_owner);
                return;
            }
        }
    }

    if let Some(mut symbol) =
        swift_symbol_from_node(node, ctx, parent_symbol.as_ref(), extension_owner)
    {
        if let Some(name) = extension_owner {
            symbol.language_identity = Some(name.to_string());
        }
        let next_parent = Some((symbol.id.clone(), symbol.kind));
        let next_owner = if symbol.body_span.is_some() {
            Some(symbol.id.clone())
        } else {
            owner_symbol.clone()
        };
        ctx.symbols.push(symbol);
        // Members of a type owner inherit the parent's identity; extension
        // language_identity does not propagate into a nested type.
        let next_extension_owner = if extension_owner.is_some() && next_parent.is_some() {
            None
        } else {
            extension_owner
        };
        visit_swift_children(node, ctx, next_parent, next_owner, next_extension_owner);
        return;
    }

    match node.kind() {
        "call_expression" => {
            extract_swift_call(node, ctx, owner_symbol.clone());
            visit_swift_children(node, ctx, parent_symbol, owner_symbol, extension_owner);
        }
        "navigation_expression" => {
            // `foo.bar()` parses as a `call_expression` whose `function`
            // child is a `navigation_expression`. The enclosing
            // `call_expression` branch already records the invocation
            // as a `ParsedCall` (the canonical fact for an extension-
            // method dispatch like `"  Ada  ".sanitized()` resolving to
            // `extension String { func sanitized() }`); emitting an
            // extra `Field`-kind reference at the suffix would
            // double-count against SourceKit-LSP's
            // `textDocument/references`, which ties the call to the
            // method declaration via its semantic index and does not
            // surface the raw suffix as a separate hit. We still keep
            // the path-flavored body hit so body-search queries (like
            // "find `trimmingCharacters` invocations") continue to
            // attribute the hit to its owning method.
            let emit_reference = !swift_navigation_is_call_function(node);
            extract_swift_navigation_facts(node, ctx, owner_symbol.clone(), emit_reference);
            visit_swift_children(node, ctx, parent_symbol, owner_symbol, extension_owner);
        }
        "attribute" => {
            extract_swift_attribute_reference(node, ctx, owner_symbol.clone());
            visit_swift_children(node, ctx, parent_symbol, owner_symbol, extension_owner);
        }
        "type_identifier" | "user_type" | "simple_user_type" => {
            if !swift_node_is_declaration_name(node) {
                extract_swift_type_reference(node, ctx, owner_symbol.clone());
            }
            visit_swift_children(node, ctx, parent_symbol, owner_symbol, extension_owner);
        }
        "inheritance_specifier" => {
            // Conformance / supertype reference. Owner is the parent's owner
            // (the type being declared). Already covered for direct
            // declarations via `swift_inheritance_names`, but we still want
            // a `ParsedReference` for each listed type for query coverage.
            if let Some(inherits) = node.child_by_field_name("inherits_from") {
                extract_swift_type_reference(inherits, ctx, owner_symbol.clone());
            }
            visit_swift_children(node, ctx, parent_symbol, owner_symbol, extension_owner);
        }
        kind if is_swift_literal(kind) => {
            extract_body_hit(node, BodyHitKind::Literal, ctx, owner_symbol.clone());
            visit_swift_children(node, ctx, parent_symbol, owner_symbol, extension_owner);
        }
        _ => visit_swift_children(node, ctx, parent_symbol, owner_symbol, extension_owner),
    }
}

fn visit_swift_children(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<(SymbolId, SymbolKind)>,
    owner_symbol: Option<SymbolId>,
    extension_owner: ExtensionOwner<'_>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit_swift_node(
            child,
            ctx,
            parent_symbol.clone(),
            owner_symbol.clone(),
            extension_owner,
        );
    }
}

fn swift_symbol_from_node(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
    extension_owner: ExtensionOwner<'_>,
) -> Option<ParsedSymbol> {
    // tree-sitter-swift surfaces class/struct/actor/enum/extension as
    // a single `class_declaration` node distinguished by `declaration_kind`.
    let (kind, decl_kind_text) = match node.kind() {
        "class_declaration" => {
            let dk = swift_declaration_kind_text(node, ctx.source)?;
            // `extension` is handled by the caller in `visit_swift_node` and
            // never produces its own symbol.
            if dk == "extension" {
                return None;
            }
            let kind = match dk.as_str() {
                "class" => SymbolKind::Class,
                "struct" => SymbolKind::Struct,
                "actor" => SymbolKind::Class,
                "enum" => SymbolKind::Enum,
                _ => return None,
            };
            (kind, dk)
        }
        "protocol_declaration" => (SymbolKind::Trait, "protocol".to_string()),
        "function_declaration" => {
            // File-scope free functions stay `Function`; functions inside a
            // type, extension, or protocol become `Method` (spec §1 c-family
            // selection rule + extension propagation).
            let is_method = parent_symbol
                .map(|(_, parent_kind)| swift_kind_owns_methods(*parent_kind))
                .unwrap_or(false)
                || extension_owner.is_some();
            let kind = if is_method {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            };
            (kind, "func".to_string())
        }
        // Protocol method requirements parse under a distinct grammar
        // node (`protocol_function_declaration`) — handle it alongside
        // `function_declaration` so SourceKit-LSP's `documentSymbol`
        // entry for the protocol's abstract method (kind 6 Method
        // under the protocol parent) has a matching Squeezy symbol.
        "protocol_function_declaration" => (SymbolKind::Method, "func".to_string()),
        "init_declaration" => (SymbolKind::Method, "init".to_string()),
        "deinit_declaration" => (SymbolKind::Method, "deinit".to_string()),
        "subscript_declaration" => (SymbolKind::Method, "subscript".to_string()),
        "typealias_declaration" => (SymbolKind::TypeAlias, "typealias".to_string()),
        // A protocol's `associatedtype Foo: Bar` is the one protocol-member
        // kind that was previously unmatched. Model it as a TypeAlias so
        // decl/definition search can find it; its `must_inherit` / `where`
        // constraints surface as `iface:` attributes below.
        "associatedtype_declaration" => (SymbolKind::TypeAlias, "associatedtype".to_string()),
        // Custom operator declarations (`infix operator <=>`) and
        // `precedencegroup` declarations are the canonical definition sites of
        // public-API operators. They have no `name` field — the name is a
        // `custom_operator` / `simple_identifier` child — so `swift_symbol_name`
        // resolves them via a dedicated path below. Modelled as `Const` since
        // they are nominal, value-like declarations with no body or callable
        // signature.
        "operator_declaration" => (SymbolKind::Const, "operator".to_string()),
        "precedence_group_declaration" => (SymbolKind::Const, "precedencegroup".to_string()),
        "enum_entry" => return None, // handled separately, multiple symbols per node
        _ => return None,
    };

    let name = swift_symbol_name(node, kind, ctx.source, &decl_kind_text)?;
    if name.is_empty() {
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
        Some(swift_arity_for_callable(node))
    } else {
        None
    };
    let mut attributes = swift_attributes_for_node(node, ctx.source);
    if decl_kind_text == "actor" {
        attributes.push("swift:actor".to_string());
    }
    match node.kind() {
        "init_declaration" => attributes.push("swift:init".to_string()),
        "deinit_declaration" => attributes.push("swift:deinit".to_string()),
        "subscript_declaration" => attributes.push("swift:subscript".to_string()),
        "operator_declaration" => attributes.push("swift:operator".to_string()),
        "precedence_group_declaration" => attributes.push("swift:precedencegroup".to_string()),
        _ => {}
    }
    if matches!(
        kind,
        SymbolKind::Class | SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Trait
    ) {
        // Swift's grammar lists every supertype and protocol conformance under
        // an identical `inheritance_specifier`, so we split them by language
        // rule: only a `class` may declare a superclass, and Swift requires it
        // to be the first listed type. Everything that follows (and every
        // supertype of a struct/enum/protocol/actor) is a protocol conformance.
        // The superclass becomes `base:` (lowered to `Extends`); conformances
        // become `iface:` (lowered to `Implements`) by the shared generic
        // inheritance-edge pass in `squeezy-graph`.
        let (bases, ifaces) = swift_categorized_inheritance(node, &decl_kind_text, ctx.source);
        attributes.extend(bases.into_iter().map(|base| format!("base:{base}")));
        attributes.extend(ifaces.into_iter().map(|iface| format!("iface:{iface}")));
    }
    if node.kind() == "associatedtype_declaration" {
        // `associatedtype Foo: Bar` and `associatedtype Foo where Bar: X`
        // constrain the associated type to conform to a protocol — record it
        // like a conformance so the constraint survives decl/definition search.
        attributes.extend(
            swift_associatedtype_constraints(node, ctx.source)
                .into_iter()
                .map(|c| format!("iface:{c}")),
        );
    }
    if matches!(
        kind,
        SymbolKind::Method
            | SymbolKind::Function
            | SymbolKind::Class
            | SymbolKind::Struct
            | SymbolKind::Enum
            | SymbolKind::Trait
    ) {
        // Generic constraint references attach as `base:<Constraint>`.
        // Captures both type-parameter clause types and `where`-clause types
        // (spec §4(g)). Symmetric across callable and type symbols.
        attributes.extend(
            swift_callable_generic_constraints(node, ctx.source)
                .into_iter()
                .map(|c| format!("base:{c}")),
        );
    }
    if swift_async_modifier(node, ctx.source) {
        attributes.push("swift:async".to_string());
    }
    if swift_throws_modifier(node, ctx.source) {
        attributes.push("swift:throws".to_string());
    }
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
        visibility: swift_visibility_text(node, ctx.source),
        docs: swift_docs_for_node(node, ctx.source),
        attributes,
        provenance: Provenance::new("tree-sitter-swift", format!("{} declaration", node.kind())),
        confidence: Confidence::ExactSyntax,
        freshness: Freshness::Fresh,
        arity,
    })
}

fn swift_property_symbols_from_node(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
    extension_owner: ExtensionOwner<'_>,
) -> Vec<ParsedSymbol> {
    let mut attributes = swift_attributes_for_node(node, ctx.source);
    if let Some(field_type) = swift_property_type(node, ctx.source) {
        attributes.push(format!("type:{field_type}"));
    }
    if node.child_by_field_name("computed_value").is_some()
        || node.kind() == "protocol_property_declaration"
    {
        attributes.push("swift:computed".to_string());
    }
    attributes.sort();
    attributes.dedup();

    let visibility = swift_visibility_text(node, ctx.source);
    let docs = swift_docs_for_node(node, ctx.source);
    let signature = signature_text(node, None, ctx.source);
    let parent_id = parent_symbol.map(|(id, _)| id.clone());
    let language_identity = extension_owner.map(|s| s.to_string());

    let mut symbols = Vec::new();
    for name in swift_property_binding_names(node, ctx.source) {
        let span = span_from_node(node);
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
            language_identity: language_identity.clone(),
            span,
            body_span: None,
            signature_span: None,
            signature: signature.clone(),
            visibility: visibility.clone(),
            docs: docs.clone(),
            attributes: attributes.clone(),
            provenance: Provenance::new(
                "tree-sitter-swift",
                format!("{} declaration", node.kind()),
            ),
            confidence: Confidence::ExactSyntax,
            freshness: Freshness::Fresh,
            arity: None,
        });
    }
    symbols
}

fn swift_enum_entry_symbols(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> Vec<ParsedSymbol> {
    if node.kind() != "enum_entry" {
        return Vec::new();
    }
    let mut symbols = Vec::new();
    let parent_id = parent_symbol.map(|(id, _)| id.clone());
    let visibility = swift_visibility_text(node, ctx.source);
    let docs = swift_docs_for_node(node, ctx.source);
    let signature = signature_text(node, None, ctx.source);
    let attributes = swift_attributes_for_node(node, ctx.source);

    // `enum_entry` may contain multiple name fields when the source
    // declares `case foo, bar` in a single clause.
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "simple_identifier" {
            continue;
        }
        // Skip identifiers that are part of the data_contents (associated
        // values), not case names. The grammar uses the `name` field for
        // case names.
        let is_case_name =
            node.field_name_for_child(child_index_of(node, child) as u32) == Some("name");
        if !is_case_name {
            continue;
        }
        let Some(name) = node_text(child, ctx.source).ok().map(str::trim) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let span = span_from_node(child);
        let id = symbol_id(
            &ctx.file,
            parent_id.as_ref(),
            SymbolKind::Variant,
            name,
            span,
        );
        symbols.push(ParsedSymbol {
            id,
            file_id: ctx.file.id.clone(),
            parent_id: parent_id.clone(),
            name: name.to_string(),
            kind: SymbolKind::Variant,
            language_identity: None,
            span,
            body_span: None,
            signature_span: None,
            signature: signature.clone(),
            visibility: visibility.clone(),
            docs: docs.clone(),
            attributes: attributes.clone(),
            provenance: Provenance::new("tree-sitter-swift", "enum_entry case"),
            confidence: Confidence::ExactSyntax,
            freshness: Freshness::Fresh,
            arity: None,
        });
    }
    symbols
}

fn child_index_of(parent: Node<'_>, target: Node<'_>) -> usize {
    let mut cursor = parent.walk();
    for (idx, child) in parent.children(&mut cursor).enumerate() {
        if child.id() == target.id() {
            return idx;
        }
    }
    0
}

fn swift_symbol_name(
    node: Node<'_>,
    kind: SymbolKind,
    source: &str,
    decl_kind: &str,
) -> Option<String> {
    if matches!(kind, SymbolKind::Method) {
        match node.kind() {
            "init_declaration" => return Some("init".to_string()),
            "deinit_declaration" => return Some("deinit".to_string()),
            "subscript_declaration" => return Some("subscript".to_string()),
            _ => {}
        }
    }
    // Operator / precedencegroup declarations carry no `name` field; their
    // identifier is the `custom_operator` (e.g. `<=>`) or `simple_identifier`
    // (e.g. a precedencegroup name) child.
    if matches!(
        node.kind(),
        "operator_declaration" | "precedence_group_declaration"
    ) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if matches!(child.kind(), "custom_operator" | "simple_identifier")
                && let Ok(text) = node_text(child, source)
            {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
        return None;
    }
    let name_node = node.child_by_field_name("name")?;
    // For `class_declaration`, the `name` field on a regular type
    // declaration is a `type_identifier`. We've already filtered out
    // `extension` via the caller.
    let _ = decl_kind;
    let text = node_text(name_node, source).ok()?.trim();
    if text.is_empty() {
        return None;
    }
    Some(text.to_string())
}

fn swift_declaration_kind_text(node: Node<'_>, source: &str) -> Option<String> {
    // `declaration_kind` is the only field on a `class_declaration` whose
    // value distinguishes class/struct/actor/enum/extension.
    node.child_by_field_name("declaration_kind")
        .and_then(|child| node_text(child, source).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

fn swift_extension_extended_type(node: Node<'_>, source: &str) -> Option<String> {
    if swift_declaration_kind_text(node, source).as_deref() != Some("extension") {
        return None;
    }
    // The `name` field for `extension Foo { ... }` is a `user_type` whose
    // first `type_identifier` is the extended type. For nested type names
    // (`extension Outer.Inner`) we keep the last segment.
    let name_node = node.child_by_field_name("name")?;
    let raw = node_text(name_node, source).ok()?.trim().to_string();
    if raw.is_empty() {
        return None;
    }
    Some(last_path_segment(&raw))
}

fn swift_inheritance_names(node: Node<'_>, source: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "inheritance_specifier" {
            continue;
        }
        let Some(inherits) = child.child_by_field_name("inherits_from") else {
            continue;
        };
        if let Ok(text) = node_text(inherits, source) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                names.push(swift_type_leaf_name(trimmed));
            }
        }
    }
    names
}

/// Split a type declaration's `inheritance_specifier` list into superclasses
/// (`base:`) and protocol conformances (`iface:`).
///
/// Swift permits a superclass only on a `class`, and the language requires it
/// to be the first entry in the inheritance clause. Every other entry — and
/// every entry on a `struct`/`enum`/`protocol`/`actor` — is a protocol
/// conformance. We can't tell a class-named first entry from a protocol-named
/// one without type-checking, so we follow the syntactic position rule: for a
/// `class`, the first name is the superclass and the rest are conformances.
fn swift_categorized_inheritance(
    node: Node<'_>,
    decl_kind: &str,
    source: &str,
) -> (Vec<String>, Vec<String>) {
    let names = swift_inheritance_names(node, source);
    // Only a `class` can carry a superclass; `actor` (which also maps to
    // SymbolKind::Class) and value/protocol types conform to protocols only.
    if decl_kind == "class" {
        let mut iter = names.into_iter();
        let bases: Vec<String> = iter.by_ref().take(1).collect();
        let ifaces: Vec<String> = iter.collect();
        (bases, ifaces)
    } else {
        (Vec::new(), names)
    }
}

fn swift_callable_generic_constraints(node: Node<'_>, source: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            // Spec §4(g): generic params can declare constraints inline:
            //   func foo<T: Codable>() {}
            "type_parameters" => {
                let mut inner_cursor = child.walk();
                for param in child.named_children(&mut inner_cursor) {
                    if param.kind() != "type_parameter" {
                        continue;
                    }
                    let Some(constraint) = param.child_by_field_name("name") else {
                        continue;
                    };
                    if let Ok(text) = node_text(constraint, source) {
                        let trimmed = text.trim();
                        if !trimmed.is_empty() {
                            out.push(swift_type_leaf_name(trimmed));
                        }
                    }
                }
            }
            "type_constraints" => {
                let mut inner_cursor = child.walk();
                for constraint in child.named_children(&mut inner_cursor) {
                    if constraint.kind() != "type_constraint" {
                        continue;
                    }
                    let mut deep_cursor = constraint.walk();
                    for inheritance in constraint.named_children(&mut deep_cursor) {
                        if inheritance.kind() != "inheritance_constraint" {
                            continue;
                        }
                        if let Some(name) = inheritance.child_by_field_name("name")
                            && let Ok(text) = node_text(name, source)
                        {
                            let trimmed = text.trim();
                            if !trimmed.is_empty() {
                                out.push(swift_type_leaf_name(trimmed));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Collect the protocol constraints on an `associatedtype` declaration: the
/// `must_inherit` field (`associatedtype Foo: Bar`) plus any `where`-clause
/// constraints carried in a `type_constraints` child.
fn swift_associatedtype_constraints(node: Node<'_>, source: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(must_inherit) = node.child_by_field_name("must_inherit")
        && let Ok(text) = node_text(must_inherit, source)
    {
        // A `must_inherit` can be a `protocol_composition_type` (`A & B`); keep
        // each leaf as its own conformance attribute.
        for part in text.split('&') {
            let leaf = swift_type_leaf_name(part);
            if !leaf.is_empty() {
                out.push(leaf);
            }
        }
    }
    out.extend(swift_callable_generic_constraints(node, source));
    out
}

fn swift_type_leaf_name(raw: &str) -> String {
    // Trim generic specialisation, optional markers, and namespace prefixes.
    // e.g. `Foundation.Decoder` → `Decoder`, `Array<Element>` → `Array`.
    let mut s = raw.trim().to_string();
    if let Some(idx) = s.find('<') {
        s.truncate(idx);
    }
    if let Some(idx) = s.find('?') {
        s.truncate(idx);
    }
    if let Some(idx) = s.find('!') {
        s.truncate(idx);
    }
    s.rsplit('.').next().unwrap_or(&s).trim().to_string()
}

fn swift_property_binding_names(node: Node<'_>, source: &str) -> Vec<String> {
    // `property_declaration` and `protocol_property_declaration` both expose
    // their bound identifier(s) under the `name` field. For the common case
    // there is a single `pattern` whose `bound_identifier` is the name.
    let mut names = Vec::new();
    let mut cursor = node.walk();
    for (idx, child) in node.children(&mut cursor).enumerate() {
        let field = node.field_name_for_child(idx as u32);
        if field != Some("name") {
            continue;
        }
        collect_swift_bound_identifiers(child, source, &mut names);
    }
    names
}

fn collect_swift_bound_identifiers(node: Node<'_>, source: &str, out: &mut Vec<String>) {
    match node.kind() {
        "simple_identifier" => {
            if let Ok(text) = node_text(node, source) {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    out.push(trimmed.to_string());
                }
            }
        }
        "pattern" => {
            // Most patterns nest a `bound_identifier`.
            if let Some(id) = node.child_by_field_name("bound_identifier") {
                collect_swift_bound_identifiers(id, source, out);
                return;
            }
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect_swift_bound_identifiers(child, source, out);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect_swift_bound_identifiers(child, source, out);
            }
        }
    }
}

fn swift_property_type(node: Node<'_>, source: &str) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "type_annotation" {
            continue;
        }
        let name = child.child_by_field_name("name")?;
        let text = node_text(name, source).ok()?;
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return None;
        }
        return Some(swift_type_leaf_name(trimmed));
    }
    None
}

fn swift_visibility_text(node: Node<'_>, source: &str) -> Option<String> {
    let modifiers = swift_modifiers_node(node)?;
    let mut cursor = modifiers.walk();
    for child in modifiers.named_children(&mut cursor) {
        if child.kind() == "visibility_modifier"
            && let Ok(text) = node_text(child, source)
        {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn swift_modifiers_node(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.kind() == "modifiers")
}

fn swift_async_modifier(node: Node<'_>, source: &str) -> bool {
    swift_has_keyword_child(node, source, "async")
}

fn swift_throws_modifier(node: Node<'_>, source: &str) -> bool {
    swift_has_keyword_child(node, source, "throws")
}

fn swift_has_keyword_child(node: Node<'_>, source: &str, keyword: &str) -> bool {
    // `async` and `throws` are not part of the `modifiers` node — they
    // appear after the parameter list on `function_declaration` /
    // `init_declaration`. Scan named and unnamed children for the
    // keyword token.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == keyword {
            return true;
        }
        // Some grammars wrap the keyword in a small node; check text too.
        if let Ok(text) = node_text(child, source)
            && text.trim() == keyword
        {
            return true;
        }
    }
    false
}

fn swift_arity_for_callable(node: Node<'_>) -> u8 {
    let mut count = 0usize;
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "parameter" {
            count += 1;
        }
    }
    u8::try_from(count).unwrap_or(u8::MAX)
}

fn swift_attributes_for_node(node: Node<'_>, source: &str) -> Vec<String> {
    let mut attrs = Vec::new();
    let Some(modifiers) = swift_modifiers_node(node) else {
        return attrs;
    };
    let mut cursor = modifiers.walk();
    for child in modifiers.named_children(&mut cursor) {
        match child.kind() {
            "attribute" => {
                if let Some(name) = swift_attribute_name(child, source) {
                    attrs.push(name);
                }
            }
            "inheritance_modifier"
            | "mutation_modifier"
            | "ownership_modifier"
            | "property_modifier"
            | "member_modifier"
            | "function_modifier" => {
                if let Ok(text) = node_text(child, source) {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        attrs.push(format!("swift:{trimmed}"));
                    }
                }
            }
            _ => {}
        }
    }
    attrs
}

fn swift_attribute_name(node: Node<'_>, source: &str) -> Option<String> {
    // `attribute` shape in tree-sitter-swift: `@ user_type ( simple_identifier ... )`
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "user_type"
            && let Ok(text) = node_text(child, source)
        {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(swift_type_leaf_name(trimmed));
            }
        }
    }
    // Fallback: full attribute text, stripping the leading `@` and arguments.
    let raw = node_text(node, source).ok()?;
    let stripped = raw.trim().trim_start_matches('@');
    let head = stripped.split('(').next()?.trim();
    if head.is_empty() {
        None
    } else {
        Some(head.to_string())
    }
}

fn swift_docs_for_node(node: Node<'_>, source: &str) -> Vec<String> {
    // Walk backwards from the node's start byte to collect contiguous
    // `///` doc comments that precede the declaration.
    let start = node.start_byte();
    let mut docs = Vec::new();
    let mut cursor = start;
    let bytes = source.as_bytes();
    while cursor > 0 {
        // Skip leading whitespace before scanning the previous line.
        let line_end = source[..cursor].rfind('\n').unwrap_or(0);
        let line = source[line_end..cursor].trim();
        if line.is_empty() {
            cursor = line_end;
            if cursor == 0 {
                break;
            }
            continue;
        }
        if let Some(stripped) = line.strip_prefix("///") {
            docs.push(stripped.trim().to_string());
        } else {
            break;
        }
        cursor = line_end;
        if cursor == 0 || bytes[cursor.saturating_sub(1)] != b'\n' {
            // No further newlines; stop scanning.
            break;
        }
    }
    docs.reverse();
    docs
}

fn swift_kind_owns_methods(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Class
            | SymbolKind::Struct
            | SymbolKind::Enum
            | SymbolKind::Trait
            | SymbolKind::Union
    )
}

/// Mirrors SourceKit-LSP's `documentSymbol` filter for stored / computed
/// properties: only emit a `Field` symbol when the declaration is owned
/// by a type (class/struct/enum/protocol/actor/union) or by an
/// `extension Foo { ... }` block. File-scope `let`/`var` (the SwiftPM
/// `let package = Package(...)` manifest binding among them) and
/// function-body `let`/`var` are locals SourceKit-LSP classifies as
/// kind 13 (Variable) and excludes from per-file symbol scans.
fn swift_property_owner_is_type(
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
    extension_owner: ExtensionOwner<'_>,
    node_kind: &str,
) -> bool {
    if extension_owner.is_some() {
        return true;
    }
    // `protocol_property_declaration` only parses inside a `protocol` body,
    // so by the time we see it the parent is guaranteed to be a `Trait`
    // symbol. Keep the explicit allow so the rule survives any future
    // grammar shuffle that re-routes the node kind.
    if node_kind == "protocol_property_declaration" {
        return true;
    }
    parent_symbol
        .map(|(_, kind)| swift_kind_owns_methods(*kind))
        .unwrap_or(false)
}

fn swift_node_is_declaration_name(node: Node<'_>) -> bool {
    // Suppress the type reference we would otherwise emit on a declaration's
    // own `name` field — e.g. the `Foo` in `class Foo {}` is the symbol's
    // name, not a reference.
    let Some(parent) = node.parent() else {
        return false;
    };
    let mut cursor = parent.walk();
    for (idx, child) in parent.children(&mut cursor).enumerate() {
        if child.id() == node.id() && parent.field_name_for_child(idx as u32) == Some("name") {
            return matches!(
                parent.kind(),
                "class_declaration"
                    | "protocol_declaration"
                    | "function_declaration"
                    | "protocol_function_declaration"
                    | "init_declaration"
                    | "subscript_declaration"
                    | "typealias_declaration"
                    | "associatedtype_declaration"
                    | "property_declaration"
                    | "protocol_property_declaration"
                    | "enum_entry"
                    | "parameter"
            );
        }
    }
    false
}

fn extract_swift_import(node: Node<'_>, ctx: &mut ExtractContext<'_>, owner_id: Option<SymbolId>) {
    // The module-qualified path lives in the `identifier` child, regardless of
    // any leading modifier (`@testable`, `private`) or attribute (`@_exported`,
    // `@_implementationOnly`) and regardless of a kind keyword
    // (`import struct M.T`). Reading the child instead of string-stripping the
    // raw text is what lets modifier/attribute-prefixed imports survive — the
    // old `strip_prefix("import")` silently dropped `@testable import Foo`
    // because the text begins with the modifier, not `import`.
    let path = swift_import_path(node, ctx.source).unwrap_or_default();
    if path.is_empty() {
        return;
    }
    // `@_exported import Foo` re-exports `Foo` to every importer of this module.
    let is_reexport = swift_import_has_attribute(node, ctx.source, "_exported");
    let imported_name = Some(last_path_segment(&path));
    ctx.imports.push(ParsedImport {
        file_id: ctx.file.id.clone(),
        owner_id,
        path,
        alias: None,
        is_glob: false,
        is_reexport,
        is_static: false,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-swift", "import declaration"),
        kind: ImportKind::Named,
        imported_name,
        is_global: false,
    });
}

/// Read the module-qualified path from an `import_declaration`'s `identifier`
/// child (e.g. `CoreGraphics.CGRect`), falling back to a text scan that strips
/// the `import`/modifier/kind-keyword prefix for grammar shapes that don't
/// surface a clean `identifier` child.
fn swift_import_path(node: Node<'_>, source: &str) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "identifier"
            && let Ok(text) = node_text(child, source)
        {
            let path = text.trim().to_string();
            if !path.is_empty() {
                return Some(path);
            }
        }
    }
    // Fallback for any grammar variant that does not expose `identifier`:
    // drop everything up to and including `import`, then the kind keyword.
    let raw = node_text(node, source).ok()?;
    let after_import = raw.split("import").nth(1)?.trim();
    let rest = strip_swift_import_kind(after_import);
    let path = rest.trim().trim_end_matches(';').trim().to_string();
    if path.is_empty() { None } else { Some(path) }
}

/// True when the `import_declaration`'s `modifiers` child carries an attribute
/// whose name (after the leading `@`) matches `name` — e.g. `_exported`.
fn swift_import_has_attribute(node: Node<'_>, source: &str, name: &str) -> bool {
    let Some(modifiers) = swift_modifiers_node(node) else {
        return false;
    };
    let mut cursor = modifiers.walk();
    modifiers.named_children(&mut cursor).any(|child| {
        child.kind() == "attribute"
            && swift_attribute_name(child, source).as_deref() == Some(name)
    })
}

fn strip_swift_import_kind(text: &str) -> &str {
    // Spec §4(i): `import struct M.T`, `import class M.T`, etc. The
    // leading keyword does not change resolution — strip it so the path
    // is the bare module-qualified name.
    for kw in [
        "struct ",
        "class ",
        "enum ",
        "protocol ",
        "typealias ",
        "func ",
        "let ",
        "var ",
    ] {
        if let Some(rest) = text.strip_prefix(kw) {
            return rest;
        }
    }
    text
}

fn extract_swift_call(node: Node<'_>, ctx: &mut ExtractContext<'_>, owner_id: Option<SymbolId>) {
    let raw = node_text(node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if raw.is_empty() {
        return;
    }
    // `call_expression` shape: function(arguments).
    let function_node = node.named_child(0);
    let (name, receiver, kind) = match function_node {
        Some(f) if f.kind() == "navigation_expression" => {
            let name = f
                .child_by_field_name("suffix")
                .and_then(|s| s.child_by_field_name("suffix"))
                .or_else(|| f.child_by_field_name("suffix"))
                .and_then(|s| node_text(s, ctx.source).ok())
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| method_name_from_text(&raw));
            let receiver = f
                .child_by_field_name("target")
                .and_then(|t| node_text(t, ctx.source).ok())
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty());
            (name, receiver, ParsedCallKind::Method)
        }
        Some(f) if f.kind() == "simple_identifier" => {
            let name = node_text(f, ctx.source)
                .ok()
                .map(|t| t.trim().to_string())
                .unwrap_or_else(|| method_name_from_text(&raw));
            (name, None, ParsedCallKind::Direct)
        }
        _ => {
            let name = method_name_from_text(&raw);
            (name, None, ParsedCallKind::Direct)
        }
    };
    if name.is_empty() {
        return;
    }
    let arity = node
        .named_children(&mut node.walk())
        .find(|c| c.kind() == "call_suffix")
        .and_then(|s| {
            s.named_children(&mut s.walk())
                .find(|c| c.kind() == "value_arguments")
        })
        .map(named_child_count)
        .unwrap_or(0);
    let confidence = if c_family_call_is_macro_like_swift(&name) {
        Confidence::MacroOpaque
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
        provenance: Provenance::new("tree-sitter-swift", "call_expression"),
        confidence,
    });
    extract_body_hit(node, BodyHitKind::Call, ctx, owner_id);
}

fn c_family_call_is_macro_like_swift(name: &str) -> bool {
    // Heuristic mirror of the c-family rule: ALL_CAPS callable names are
    // typically macros (`#externalMacro`, `#freestanding`).
    if name.is_empty() {
        return false;
    }
    let stripped = name.trim_start_matches('#');
    !stripped.is_empty() && stripped.chars().all(|c| c.is_ascii_uppercase() || c == '_')
}

/// Returns true when `node` is the function position of an enclosing
/// `call_expression`. In tree-sitter-swift the call shape is
/// `call_expression{ function: navigation_expression, call_suffix }`,
/// so a navigation_expression is the function position iff its parent is
/// a call_expression and it is the first named child of that parent
/// (the suffix that follows is a `call_suffix` node, not a
/// `navigation_expression`).
fn swift_navigation_is_call_function(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if parent.kind() != "call_expression" {
        return false;
    }
    parent
        .named_child(0)
        .is_some_and(|first| first.id() == node.id())
}

fn extract_swift_navigation_facts(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
    emit_reference: bool,
) {
    // `navigation_expression target: ... suffix: navigation_suffix suffix: simple_identifier`
    let Some(suffix) = node.child_by_field_name("suffix") else {
        return;
    };
    let Some(name) = suffix
        .child_by_field_name("suffix")
        .and_then(|n| node_text(n, ctx.source).ok())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
    else {
        return;
    };
    if emit_reference {
        ctx.references.push(ParsedReference {
            file_id: ctx.file.id.clone(),
            owner_id: owner_id.clone(),
            text: name.clone(),
            kind: ReferenceKind::Field,
            span: span_from_node(suffix),
            provenance: Provenance::new("tree-sitter-swift", "navigation_expression reference"),
        });
    }
    ctx.body_hits.push(BodyHit {
        file_id: ctx.file.id.clone(),
        owner_id,
        text: name,
        kind: BodyHitKind::Path,
        span: span_from_node(node),
    });
}

fn extract_swift_type_reference(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let Ok(text) = node_text(node, ctx.source) else {
        return;
    };
    let trimmed = text.trim();
    if trimmed.is_empty() || is_swift_keyword(trimmed) {
        return;
    }
    let leaf = swift_type_leaf_name(trimmed);
    if leaf.is_empty() {
        return;
    }
    ctx.references.push(ParsedReference {
        file_id: ctx.file.id.clone(),
        owner_id: owner_id.clone(),
        text: leaf.clone(),
        kind: ReferenceKind::Type,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-swift", format!("{} reference", node.kind())),
    });
    ctx.body_hits.push(BodyHit {
        file_id: ctx.file.id.clone(),
        owner_id,
        text: leaf,
        kind: BodyHitKind::Type,
        span: span_from_node(node),
    });
}

fn extract_swift_attribute_reference(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let Some(name) = swift_attribute_name(node, ctx.source) else {
        return;
    };
    let span = span_from_node(node);
    ctx.references.push(ParsedReference {
        file_id: ctx.file.id.clone(),
        owner_id: owner_id.clone(),
        text: name.clone(),
        kind: ReferenceKind::Attribute,
        span,
        provenance: Provenance::new("tree-sitter-swift", "attribute reference"),
    });
    ctx.body_hits.push(BodyHit {
        file_id: ctx.file.id.clone(),
        owner_id: owner_id.clone(),
        text: name,
        kind: BodyHitKind::Attribute,
        span,
    });
    // `@objc(customName)` records `customName` as a second attribute
    // reference for searchability (spec §4(f)).
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "simple_identifier"
            && let Ok(text) = node_text(child, ctx.source)
        {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                ctx.references.push(ParsedReference {
                    file_id: ctx.file.id.clone(),
                    owner_id: owner_id.clone(),
                    text: trimmed.to_string(),
                    kind: ReferenceKind::Attribute,
                    span: span_from_node(child),
                    provenance: Provenance::new(
                        "tree-sitter-swift",
                        "attribute argument reference",
                    ),
                });
            }
        }
    }
}

fn is_swift_literal(kind: &str) -> bool {
    matches!(
        kind,
        "string_literal"
            | "line_string_literal"
            | "multi_line_string_literal"
            | "raw_string_literal"
            | "integer_literal"
            | "hex_literal"
            | "oct_literal"
            | "bin_literal"
            | "real_literal"
            | "boolean_literal"
            | "true"
            | "false"
            | "nil"
            | "nil_literal"
    )
}

fn is_swift_keyword(text: &str) -> bool {
    matches!(
        text,
        "as" | "associatedtype"
            | "break"
            | "case"
            | "catch"
            | "class"
            | "continue"
            | "default"
            | "defer"
            | "deinit"
            | "do"
            | "else"
            | "enum"
            | "extension"
            | "fallthrough"
            | "false"
            | "fileprivate"
            | "for"
            | "func"
            | "guard"
            | "if"
            | "import"
            | "in"
            | "init"
            | "inout"
            | "internal"
            | "is"
            | "let"
            | "nil"
            | "open"
            | "operator"
            | "private"
            | "protocol"
            | "public"
            | "repeat"
            | "rethrows"
            | "return"
            | "self"
            | "Self"
            | "static"
            | "struct"
            | "subscript"
            | "super"
            | "switch"
            | "throw"
            | "throws"
            | "true"
            | "try"
            | "typealias"
            | "var"
            | "where"
            | "while"
    )
}

fn dedup_swift_facts(ctx: &mut ExtractContext<'_>) {
    let mut seen_refs: HashSet<(u32, ReferenceKind, String)> = HashSet::new();
    ctx.references
        .retain(|r| seen_refs.insert((r.span.start_byte, r.kind, r.text.clone())));
    let mut seen_hits: HashSet<(u32, BodyHitKind, String)> = HashSet::new();
    ctx.body_hits
        .retain(|h| seen_hits.insert((h.span.start_byte, h.kind, h.text.clone())));
    // Deduplicate symbols by (kind, name, span). Necessary because the
    // visitor revisits property/computed children and may emit duplicate
    // enum entries when both inherent and synthetic walks happen.
    let mut seen_syms: HashSet<(u32, SymbolKind, String)> = HashSet::new();
    ctx.symbols
        .retain(|s| seen_syms.insert((s.span.start_byte, s.kind, s.name.clone())));
}

/// Spec §4(i): infer the SwiftPM module from a path like
/// `Sources/<Module>/file.swift` or `Tests/<Module>/file.swift`.
fn swift_module_from_path(relative_path: &str) -> Option<String> {
    let parts: Vec<&str> = relative_path.split('/').collect();
    for marker in ["Sources", "Tests"] {
        if let Some(idx) = parts.iter().position(|s| *s == marker)
            && let Some(module) = parts.get(idx + 1)
            && !module.is_empty()
        {
            return Some((*module).to_string());
        }
    }
    None
}
