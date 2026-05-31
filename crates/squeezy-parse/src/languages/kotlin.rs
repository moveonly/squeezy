// Kotlin language extractor.
//
// Modeled on `java.rs` per the langs/kotlin spec (target/lang-specs/kotlin.md).
// The Kotlin grammar shipped by `tree-sitter-kotlin-ng` exposes a different
// node-name set than `tree-sitter-java`, so the helpers below recreate the
// equivalent Java logic against Kotlin node kinds instead of sharing the
// `pub(crate)` Java surface. No code from `java.rs` is re-exported here —
// future Java refactors must not silently break Kotlin.
//
// What is *added* on top of the Java template:
//   - top-level function / property emission keyed on the file package
//   - companion-object child promotion (children get the host class as parent)
//   - extension-function receiver capture into `language_identity`
//   - `typealias` -> `SymbolKind::TypeAlias`
//   - `suspend`, `inline`, `data`, `sealed`, `open`, `override` attribute flags
//
// Deferred (TODO comments inline, see spec sections):
//   - data-class generated members (§4e): excluded for symmetry with oracle
//
// Filled in by langs/kotlin-deferred follow-up:
//   - delegated-property accessor binding (§4g): emit the delegate target
//     (`lazy`, `Delegates.observable`) as a `ParsedCall` whose `caller_id` is
//     the property symbol so cross-call resolvers can find it.
//   - sealed-class child enumeration (§4f): emit a `ParsedReference`
//     (`kind: Type`) from each nested class/object in a `sealed` parent
//     pointing back to the parent name, so `references_to_symbol(Parent)`
//     includes the siblings declared inside the parent body.
//   - `inline reified` modeling (§4d): record each `reified` type-parameter
//     name in `language_identity` (template the extension-function pattern)
//     so the resolver can use it for cross-call type matching.

use std::collections::HashSet;

use crate::languages::rust::*;
use crate::*;

pub(crate) fn extract_kotlin(file: FileRecord, source: &str, tree: &Tree) -> ParsedFile {
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

    visit_kotlin_node(root, &mut ctx, None, None);
    dedup_kotlin_facts(&mut ctx);

    let package = ctx
        .imports
        .iter()
        .find(|import| import.alias.as_deref() == Some("__kotlin_package__"))
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

pub(crate) fn visit_kotlin_node(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<(SymbolId, SymbolKind)>,
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
        "package_header" => {
            extract_kotlin_package(node, ctx);
            return;
        }
        "import" => {
            extract_kotlin_import(node, ctx, owner_symbol.clone());
            return;
        }
        _ => {}
    }

    // Property declarations may bind one or more names (via
    // `multi_variable_declaration` destructuring). Multi-name bindings get
    // expanded into one symbol per name; single-binding properties fall
    // through into the normal `kotlin_symbol_from_node` path so the
    // `Confidence::Partial`-for-delegated rule kicks in centrally.
    if node.kind() == "property_declaration"
        && let Some(symbols) = kotlin_property_symbols(node, ctx, parent_symbol.as_ref())
    {
        let next_parent = symbols
            .last()
            .map(|symbol| (symbol.id.clone(), symbol.kind));
        let next_owner = symbols.last().map(|symbol| symbol.id.clone());
        for symbol in symbols {
            ctx.symbols.push(symbol);
        }
        visit_kotlin_children(
            node,
            ctx,
            next_parent.or(parent_symbol),
            next_owner.or(owner_symbol),
        );
        return;
    }

    // Class-parameter (primary constructor `val name: T`) declarations emit
    // a `Field` symbol owned by the host class when they declare a property
    // (`val`/`var`). Whether or not the parameter declares a property, we
    // recurse into its children so type references (e.g. the `Greeter` in
    // `private val greeter: Greeter`) flow through to the references list.
    if node.kind() == "class_parameter" {
        if let Some(symbol) = kotlin_class_parameter_symbol(node, ctx, parent_symbol.as_ref()) {
            let owner = Some(symbol.id.clone());
            ctx.symbols.push(symbol);
            visit_kotlin_children(node, ctx, parent_symbol, owner);
        } else {
            visit_kotlin_children(node, ctx, parent_symbol, owner_symbol);
        }
        return;
    }

    if let Some(symbol) = kotlin_symbol_from_node(node, ctx, parent_symbol.as_ref()) {
        let next_kind = symbol.kind;
        let next_parent = Some((symbol.id.clone(), symbol.kind));
        let next_owner = if symbol.body_span.is_some() {
            Some(symbol.id.clone())
        } else {
            owner_symbol.clone()
        };
        // kotlin spec §4f: emit a Type reference from each class/object
        // declared inside a `sealed` parent body pointing back to the
        // parent's name. The `delegation_specifier` walk only fires when
        // the child explicitly says `: Parent()`; this catches sealed
        // siblings declared inside the same body and (more importantly)
        // attaches a reference whose `owner_id` is the *child* symbol so
        // an ancestor-walk for `Parent.children` finds the sibling set.
        let sealed_child_ref = if matches!(
            symbol.kind,
            SymbolKind::Class | SymbolKind::Trait | SymbolKind::Enum
        ) {
            kotlin_sealed_parent_name(node, ctx.source)
                .map(|parent_name| (parent_name, symbol.id.clone(), symbol.span))
        } else {
            None
        };

        // Companion-object child promotion: children of `companion_object`
        // re-parent onto the *grandparent* class so signatures like
        // `Host.factory()` resolve directly. The companion symbol itself
        // still exists (kept for navigation), but its body is walked with
        // the host class as the implicit parent for children.
        let child_parent = if node.kind() == "companion_object"
            && let Some(host) = parent_symbol.as_ref()
        {
            ctx.symbols.push(symbol);
            Some((host.0.clone(), host.1))
        } else {
            ctx.symbols.push(symbol);
            next_parent
        };

        if let Some((parent_name, child_id, span)) = sealed_child_ref {
            ctx.references.push(ParsedReference {
                file_id: ctx.file.id.clone(),
                owner_id: Some(child_id),
                text: parent_name,
                kind: ReferenceKind::Type,
                span,
                provenance: Provenance::new("tree-sitter-kotlin", "sealed child enumeration"),
            });
        }

        let _ = next_kind;
        visit_kotlin_children(node, ctx, child_parent, next_owner);
        return;
    }

    match node.kind() {
        "call_expression" => {
            extract_kotlin_call_expression(node, ctx, owner_symbol.clone());
            visit_kotlin_children(node, ctx, parent_symbol, owner_symbol);
        }
        "constructor_invocation" => {
            extract_kotlin_constructor_invocation(node, ctx, owner_symbol.clone());
            visit_kotlin_children(node, ctx, parent_symbol, owner_symbol);
        }
        "navigation_expression" => {
            extract_kotlin_navigation_expression(node, ctx, owner_symbol.clone());
            visit_kotlin_children(node, ctx, parent_symbol, owner_symbol);
        }
        "user_type" => {
            extract_kotlin_user_type_reference(node, ctx, owner_symbol.clone());
            visit_kotlin_children(node, ctx, parent_symbol, owner_symbol);
        }
        "annotation" => {
            extract_kotlin_annotation_reference(node, ctx, owner_symbol);
        }
        "property_delegate" => {
            // kotlin spec §4g: bind the delegate target to the enclosing
            // property symbol so cross-call resolution can find it. The
            // delegate's immediate child is the call expression
            // (`lazy { ... }` / `Delegates.observable(...)`); emit a single
            // ParsedCall whose `caller_id` is the property and then recurse
            // *under* the immediate call expression so calls inside any
            // trailing lambda body are still attached to the property scope
            // without double-emitting the delegate target itself.
            extract_kotlin_property_delegate_call(node, ctx, owner_symbol.clone());
            visit_kotlin_property_delegate_children(node, ctx, parent_symbol, owner_symbol);
        }
        "identifier" => {
            // Suppressed; bare identifiers are too noisy to emit as
            // references on every reachable name.
        }
        kind if is_kotlin_literal(kind) => {
            extract_body_hit(node, BodyHitKind::Literal, ctx, owner_symbol);
        }
        _ => visit_kotlin_children(node, ctx, parent_symbol, owner_symbol),
    }
}

pub(crate) fn visit_kotlin_children(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<(SymbolId, SymbolKind)>,
    owner_symbol: Option<SymbolId>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit_kotlin_node(child, ctx, parent_symbol.clone(), owner_symbol.clone());
    }
}

/// Returns a `ParsedSymbol` for declaration nodes that map 1:1 to a single
/// symbol. Returns `None` for nodes handled out-of-band (`property_declaration`
/// multi-bindings, `class_parameter` field promotion).
pub(crate) fn kotlin_symbol_from_node(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> Option<ParsedSymbol> {
    let kind_str = node.kind();
    match kind_str {
        "class_declaration" => Some(kotlin_class_declaration_symbol(node, ctx, parent_symbol)),
        "object_declaration" => Some(kotlin_object_declaration_symbol(node, ctx, parent_symbol)),
        "companion_object" => Some(kotlin_companion_object_symbol(node, ctx, parent_symbol)),
        "function_declaration" => kotlin_function_declaration_symbol(node, ctx, parent_symbol),
        "secondary_constructor" => kotlin_secondary_constructor_symbol(node, ctx, parent_symbol),
        "type_alias" => kotlin_type_alias_symbol(node, ctx, parent_symbol),
        "enum_entry" => kotlin_enum_entry_symbol(node, ctx, parent_symbol),
        _ => None,
    }
}

fn kotlin_class_declaration_symbol(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> ParsedSymbol {
    let name = kotlin_node_name(node, ctx.source).unwrap_or_else(|| "<anonymous>".to_string());
    let span = span_from_node(node);
    let parent_id = parent_symbol.map(|(id, _)| id.clone());
    let kind = if kotlin_class_is_interface(node) {
        SymbolKind::Trait
    } else if kotlin_class_is_enum(node) {
        SymbolKind::Enum
    } else {
        SymbolKind::Class
    };
    let body = kotlin_class_body(node);
    let body_span = body.map(span_from_node);
    let signature = signature_text(node, body, ctx.source);
    let id = symbol_id(&ctx.file, parent_id.as_ref(), kind, &name, span);
    let mut attributes = kotlin_attributes_for_node(node, ctx.source);
    attributes.extend(
        kotlin_type_inheritance_names(node, ctx.source)
            .into_iter()
            .map(|base| format!("base:{base}")),
    );
    if kotlin_class_modifier_present(node, "data") {
        attributes.push("kotlin:data".to_string());
    }
    if kotlin_class_modifier_present(node, "sealed") {
        attributes.push("kotlin:sealed".to_string());
    }
    if kotlin_class_modifier_present(node, "annotation") {
        attributes.push("kotlin:annotation".to_string());
    }
    if kotlin_class_modifier_present(node, "inner") {
        attributes.push("kotlin:inner".to_string());
    }
    attributes.sort();
    attributes.dedup();

    ParsedSymbol {
        id,
        file_id: ctx.file.id.clone(),
        parent_id,
        name,
        kind,
        language_identity: None,
        span,
        body_span,
        signature,
        visibility: kotlin_visibility_text(node, ctx.source),
        docs: kotlin_docs_for_node(node, ctx.source),
        attributes,
        provenance: Provenance::new("tree-sitter-kotlin", "class declaration"),
        confidence: Confidence::ExactSyntax,
        freshness: Freshness::Fresh,
        arity: None,
    }
}

fn kotlin_object_declaration_symbol(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> ParsedSymbol {
    let name = kotlin_node_name(node, ctx.source).unwrap_or_else(|| "<anonymous>".to_string());
    let span = span_from_node(node);
    let parent_id = parent_symbol.map(|(id, _)| id.clone());
    let body = kotlin_class_body(node);
    let body_span = body.map(span_from_node);
    let signature = signature_text(node, body, ctx.source);
    let id = symbol_id(
        &ctx.file,
        parent_id.as_ref(),
        SymbolKind::Class,
        &name,
        span,
    );
    let mut attributes = kotlin_attributes_for_node(node, ctx.source);
    attributes.push("kotlin:object".to_string());
    attributes.extend(
        kotlin_type_inheritance_names(node, ctx.source)
            .into_iter()
            .map(|base| format!("base:{base}")),
    );
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
        signature,
        visibility: kotlin_visibility_text(node, ctx.source),
        docs: kotlin_docs_for_node(node, ctx.source),
        attributes,
        provenance: Provenance::new("tree-sitter-kotlin", "object declaration"),
        confidence: Confidence::ExactSyntax,
        freshness: Freshness::Fresh,
        arity: None,
    }
}

fn kotlin_companion_object_symbol(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> ParsedSymbol {
    // Companion objects may be unnamed (default "Companion") or named.
    let name = kotlin_node_name(node, ctx.source).unwrap_or_else(|| "Companion".to_string());
    let span = span_from_node(node);
    let parent_id = parent_symbol.map(|(id, _)| id.clone());
    let body = kotlin_class_body(node);
    let body_span = body.map(span_from_node);
    let signature = signature_text(node, body, ctx.source);
    let id = symbol_id(
        &ctx.file,
        parent_id.as_ref(),
        SymbolKind::Class,
        &name,
        span,
    );
    let mut attributes = kotlin_attributes_for_node(node, ctx.source);
    attributes.push("kotlin:companion".to_string());
    attributes.push("kotlin:object".to_string());
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
        signature,
        visibility: kotlin_visibility_text(node, ctx.source),
        docs: kotlin_docs_for_node(node, ctx.source),
        attributes,
        provenance: Provenance::new("tree-sitter-kotlin", "companion object"),
        confidence: Confidence::ExactSyntax,
        freshness: Freshness::Fresh,
        arity: None,
    }
}

fn kotlin_function_declaration_symbol(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> Option<ParsedSymbol> {
    let name = kotlin_node_name(node, ctx.source)?;
    let span = span_from_node(node);
    // Methods are functions whose enclosing parent symbol is a class-like
    // declaration; otherwise they're top-level functions. Companion-object
    // children are re-parented to the host class before this fires, so they
    // also get SymbolKind::Method as expected.
    let parent_kind = parent_symbol.map(|(_, kind)| *kind);
    let kind = match parent_kind {
        Some(SymbolKind::Class | SymbolKind::Trait | SymbolKind::Enum | SymbolKind::Struct) => {
            SymbolKind::Method
        }
        _ => SymbolKind::Function,
    };
    let parent_id = parent_symbol.map(|(id, _)| id.clone());
    let body = node
        .child_by_field_name("body")
        .or_else(|| kotlin_first_child_of_kind(node, "function_body"));
    let body_span = body.map(span_from_node);
    let signature = signature_text(node, body, ctx.source);
    let id = symbol_id(&ctx.file, parent_id.as_ref(), kind, &name, span);

    let mut attributes = kotlin_attributes_for_node(node, ctx.source);
    if kotlin_function_modifier_present(node, "suspend") {
        attributes.push("kotlin:suspend".to_string());
    }
    let inline_function = kotlin_function_modifier_present(node, "inline");
    if inline_function {
        attributes.push("kotlin:inline".to_string());
    }
    // kotlin spec §4d: capture each `reified` type parameter as a per-name
    // attribute (`kotlin:reified:T`) plus a sorted list folded into
    // `language_identity` (`reified:T,U`) so the resolver can match
    // call-site type arguments against the function. We only check the
    // modifier when the function is `inline`, because `reified` is only
    // valid on inline type parameters; honouring that constraint keeps
    // syntactically-invalid sources from leaking junk identities.
    let reified_type_params = if inline_function {
        kotlin_reified_type_parameters(node, ctx.source)
    } else {
        Vec::new()
    };
    for name in &reified_type_params {
        attributes.push(format!("kotlin:reified:{name}"));
    }
    if kotlin_function_modifier_present(node, "operator") {
        attributes.push("kotlin:operator".to_string());
    }
    if kotlin_function_modifier_present(node, "infix") {
        attributes.push("kotlin:infix".to_string());
    }
    if kotlin_function_modifier_present(node, "tailrec") {
        attributes.push("kotlin:tailrec".to_string());
    }
    if kotlin_member_modifier_present(node, "override") {
        attributes.push("kotlin:override".to_string());
    }
    if kotlin_member_modifier_present(node, "abstract") {
        attributes.push("kotlin:abstract".to_string());
    }
    if kotlin_member_modifier_present(node, "open") {
        attributes.push("kotlin:open".to_string());
    }
    if kotlin_class_modifier_present(node, "expect") {
        attributes.push("kotlin:expect".to_string());
    }
    if kotlin_class_modifier_present(node, "actual") {
        attributes.push("kotlin:actual".to_string());
    }
    // Companion-object membership: when the function's enclosing class body
    // is inside a `companion_object`, tag it so resolvers can route
    // `Host.factory()` calls against the host class.
    if kotlin_node_is_inside_companion(node) {
        attributes.push("kotlin:companion".to_string());
    }

    // Extension-function receiver capture. The grammar emits the receiver
    // type as a `user_type` (or `nullable_type` wrapping a `user_type`)
    // before the function's name `identifier`. If we find one, store its
    // text in `language_identity` and tag the function `kotlin:extension`.
    let (extension_receiver, receiver_simple) = kotlin_extension_receiver(node, ctx.source);
    let mut language_identity = None;
    let confidence = if let Some(receiver) = extension_receiver.clone() {
        attributes.push("kotlin:extension".to_string());
        language_identity = Some(receiver);
        if receiver_simple {
            Confidence::ExactSyntax
        } else {
            Confidence::Partial
        }
    } else {
        Confidence::ExactSyntax
    };
    // kotlin spec §4d: fold `reified` type-parameter names into
    // `language_identity`, templating the existing extension-receiver
    // pattern. Format is `<receiver>;reified:<T,U>` so both halves remain
    // round-trippable. A pure reified inline (no extension receiver) lands
    // as `reified:<T>`; an extension fun on a resolvable type with reified
    // params lands as `String;reified:T`.
    if !reified_type_params.is_empty() {
        let reified_tag = format!("reified:{}", reified_type_params.join(","));
        language_identity = Some(match language_identity {
            Some(existing) if !existing.is_empty() => format!("{existing};{reified_tag}"),
            _ => reified_tag,
        });
    }

    if is_kotlin_test_symbol(&ctx.file.relative_path, kind, &name, &attributes) {
        attributes.push("kotlin:test".to_string());
    }

    attributes.sort();
    attributes.dedup();

    let arity = node
        .child_by_field_name("parameters")
        .or_else(|| kotlin_first_child_of_kind(node, "function_value_parameters"))
        .map(|params| u8::try_from(kotlin_parameter_count(params)).unwrap_or(u8::MAX));

    Some(ParsedSymbol {
        id,
        file_id: ctx.file.id.clone(),
        parent_id,
        name,
        kind,
        language_identity,
        span,
        body_span,
        signature,
        visibility: kotlin_visibility_text(node, ctx.source),
        docs: kotlin_docs_for_node(node, ctx.source),
        attributes,
        provenance: Provenance::new("tree-sitter-kotlin", "function declaration"),
        confidence,
        freshness: Freshness::Fresh,
        arity,
    })
}

fn kotlin_secondary_constructor_symbol(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> Option<ParsedSymbol> {
    let parent = parent_symbol?;
    let name = if let Some(parent_symbol_ref) = parent_symbol {
        // Recover the enclosing class name from the parent's id (best-effort).
        kotlin_symbol_name_from_id(&parent_symbol_ref.0).unwrap_or_else(|| "<init>".to_string())
    } else {
        "<init>".to_string()
    };
    let span = span_from_node(node);
    let body = kotlin_first_child_of_kind(node, "block");
    let body_span = body.map(span_from_node);
    let signature = signature_text(node, body, ctx.source);
    let id = symbol_id(&ctx.file, Some(&parent.0), SymbolKind::Method, &name, span);
    let mut attributes = kotlin_attributes_for_node(node, ctx.source);
    attributes.push("kotlin:secondary_constructor".to_string());
    attributes.sort();
    attributes.dedup();
    let arity = kotlin_first_child_of_kind(node, "function_value_parameters")
        .map(|params| u8::try_from(kotlin_parameter_count(params)).unwrap_or(u8::MAX));

    Some(ParsedSymbol {
        id,
        file_id: ctx.file.id.clone(),
        parent_id: Some(parent.0.clone()),
        name,
        kind: SymbolKind::Method,
        language_identity: None,
        span,
        body_span,
        signature,
        visibility: kotlin_visibility_text(node, ctx.source),
        docs: kotlin_docs_for_node(node, ctx.source),
        attributes,
        provenance: Provenance::new("tree-sitter-kotlin", "secondary constructor"),
        confidence: Confidence::ExactSyntax,
        freshness: Freshness::Fresh,
        arity,
    })
}

fn kotlin_type_alias_symbol(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> Option<ParsedSymbol> {
    let name = kotlin_node_name(node, ctx.source)?;
    let span = span_from_node(node);
    let parent_id = parent_symbol.map(|(id, _)| id.clone());
    let id = symbol_id(
        &ctx.file,
        parent_id.as_ref(),
        SymbolKind::TypeAlias,
        &name,
        span,
    );
    let signature = signature_text(node, None, ctx.source);
    let target = kotlin_first_child_of_kind(node, "type")
        .and_then(|child| node_text(child, ctx.source).ok())
        .or_else(|| {
            kotlin_first_child_of_kind(node, "user_type")
                .and_then(|child| node_text(child, ctx.source).ok())
        })
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());
    let mut attributes = kotlin_attributes_for_node(node, ctx.source);
    attributes.push("kotlin:typealias".to_string());
    attributes.sort();
    attributes.dedup();

    Some(ParsedSymbol {
        id,
        file_id: ctx.file.id.clone(),
        parent_id,
        name,
        kind: SymbolKind::TypeAlias,
        language_identity: target,
        span,
        body_span: None,
        signature,
        visibility: kotlin_visibility_text(node, ctx.source),
        docs: kotlin_docs_for_node(node, ctx.source),
        attributes,
        provenance: Provenance::new("tree-sitter-kotlin", "type alias"),
        confidence: Confidence::ExactSyntax,
        freshness: Freshness::Fresh,
        arity: None,
    })
}

fn kotlin_enum_entry_symbol(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> Option<ParsedSymbol> {
    let name = kotlin_first_child_of_kind(node, "identifier")
        .and_then(|child| node_text(child, ctx.source).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())?;
    let span = span_from_node(node);
    let parent_id = parent_symbol.map(|(id, _)| id.clone());
    let id = symbol_id(
        &ctx.file,
        parent_id.as_ref(),
        SymbolKind::Variant,
        &name,
        span,
    );
    let signature = signature_text(node, kotlin_class_body(node), ctx.source);
    let mut attributes = kotlin_attributes_for_node(node, ctx.source);
    attributes.push("kotlin:enum_entry".to_string());
    attributes.sort();
    attributes.dedup();

    Some(ParsedSymbol {
        id,
        file_id: ctx.file.id.clone(),
        parent_id,
        name,
        kind: SymbolKind::Variant,
        language_identity: None,
        span,
        body_span: kotlin_class_body(node).map(span_from_node),
        signature,
        visibility: None,
        docs: kotlin_docs_for_node(node, ctx.source),
        attributes,
        provenance: Provenance::new("tree-sitter-kotlin", "enum entry"),
        confidence: Confidence::ExactSyntax,
        freshness: Freshness::Fresh,
        arity: None,
    })
}

/// Expands a `property_declaration` into one or more `ParsedSymbol`s, one per
/// declared binding. `var x = 1` -> one symbol; `val (a, b) = pair` ->
/// two symbols sharing the same span.
fn kotlin_property_symbols(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> Option<Vec<ParsedSymbol>> {
    let parent_kind = parent_symbol.map(|(_, kind)| *kind);
    // Local `val`/`var` declarations inside a function/method/test body are
    // not symbols themselves — they should not own calls and should not
    // appear in the symbol table. Emitting them as `Const` would steal call
    // attribution from the enclosing function (so a `val x = foo()` inside
    // `fun run()` would route `foo` to `x` instead of `run`).
    if matches!(
        parent_kind,
        Some(SymbolKind::Function | SymbolKind::Method | SymbolKind::Test),
    ) {
        return None;
    }
    let is_field = matches!(
        parent_kind,
        Some(SymbolKind::Class | SymbolKind::Trait | SymbolKind::Enum | SymbolKind::Struct,)
    );
    let parent_id = parent_symbol.map(|(id, _)| id.clone());

    let mut bindings: Vec<(String, SourceSpan)> = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "variable_declaration" => {
                if let Some(name) = kotlin_first_child_of_kind(child, "identifier")
                    .and_then(|n| node_text(n, ctx.source).ok())
                    .map(|text| text.trim().to_string())
                    .filter(|text| !text.is_empty())
                {
                    bindings.push((name, span_from_node(child)));
                }
            }
            "multi_variable_declaration" => {
                let mut inner = child.walk();
                for grand in child.named_children(&mut inner) {
                    if grand.kind() == "variable_declaration"
                        && let Some(name) = kotlin_first_child_of_kind(grand, "identifier")
                            .and_then(|n| node_text(n, ctx.source).ok())
                            .map(|text| text.trim().to_string())
                            .filter(|text| !text.is_empty())
                    {
                        bindings.push((name, span_from_node(grand)));
                    }
                }
            }
            _ => {}
        }
    }

    if bindings.is_empty() {
        return None;
    }

    let is_mutable = kotlin_property_is_var(node, ctx.source);
    let kind = if is_field {
        SymbolKind::Field
    } else if is_mutable {
        SymbolKind::Static
    } else {
        SymbolKind::Const
    };
    let signature = signature_text(node, None, ctx.source);
    let visibility = kotlin_visibility_text(node, ctx.source);
    let docs = kotlin_docs_for_node(node, ctx.source);
    let mut attributes = kotlin_attributes_for_node(node, ctx.source);
    if kotlin_class_modifier_present(node, "const") {
        attributes.push("kotlin:const".to_string());
    }
    if kotlin_member_modifier_present(node, "override") {
        attributes.push("kotlin:override".to_string());
    }
    if kotlin_class_modifier_present(node, "lateinit") {
        attributes.push("kotlin:lateinit".to_string());
    }
    let has_delegate = kotlin_first_child_of_kind(node, "property_delegate").is_some();
    if has_delegate {
        attributes.push("kotlin:delegated".to_string());
    }
    if kotlin_node_is_inside_companion(node) {
        attributes.push("kotlin:companion".to_string());
    }
    if let Some(field_type) = kotlin_property_type(node, ctx.source) {
        attributes.push(format!("type:{field_type}"));
    }
    attributes.sort();
    attributes.dedup();

    let confidence = if has_delegate {
        Confidence::Partial
    } else {
        Confidence::ExactSyntax
    };

    let symbols = bindings
        .into_iter()
        .map(|(name, span)| {
            let id = symbol_id(&ctx.file, parent_id.as_ref(), kind, &name, span);
            ParsedSymbol {
                id,
                file_id: ctx.file.id.clone(),
                parent_id: parent_id.clone(),
                name,
                kind,
                language_identity: None,
                span,
                body_span: None,
                signature: signature.clone(),
                visibility: visibility.clone(),
                docs: docs.clone(),
                attributes: attributes.clone(),
                provenance: Provenance::new("tree-sitter-kotlin", "property declaration"),
                confidence,
                freshness: Freshness::Fresh,
                arity: None,
            }
        })
        .collect();
    Some(symbols)
}

fn kotlin_class_parameter_symbol(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> Option<ParsedSymbol> {
    // Only `val`/`var` class parameters declare a property; bare positional
    // parameters do not contribute a field symbol.
    if !kotlin_class_parameter_declares_property(node, ctx.source) {
        return None;
    }
    let parent = parent_symbol?;
    // Only promote when the host is a class-like declaration.
    if !matches!(
        parent.1,
        SymbolKind::Class | SymbolKind::Trait | SymbolKind::Enum | SymbolKind::Struct,
    ) {
        return None;
    }
    let name = kotlin_first_child_of_kind(node, "identifier")
        .and_then(|child| node_text(child, ctx.source).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())?;
    let span = span_from_node(node);
    let id = symbol_id(&ctx.file, Some(&parent.0), SymbolKind::Field, &name, span);
    let signature = signature_text(node, None, ctx.source);
    let mut attributes = kotlin_attributes_for_node(node, ctx.source);
    attributes.push("kotlin:ctor_property".to_string());
    if let Some(field_type) = kotlin_class_parameter_type(node, ctx.source) {
        attributes.push(format!("type:{field_type}"));
    }
    attributes.sort();
    attributes.dedup();

    Some(ParsedSymbol {
        id,
        file_id: ctx.file.id.clone(),
        parent_id: Some(parent.0.clone()),
        name,
        kind: SymbolKind::Field,
        language_identity: None,
        span,
        body_span: None,
        signature,
        visibility: kotlin_visibility_text(node, ctx.source),
        docs: kotlin_docs_for_node(node, ctx.source),
        attributes,
        provenance: Provenance::new("tree-sitter-kotlin", "primary-constructor property"),
        confidence: Confidence::ExactSyntax,
        freshness: Freshness::Fresh,
        arity: None,
    })
}

pub(crate) fn extract_kotlin_package(node: Node<'_>, ctx: &mut ExtractContext<'_>) {
    let Some(qualified) = kotlin_first_child_of_kind(node, "qualified_identifier")
        .or_else(|| kotlin_first_child_of_kind(node, "identifier"))
    else {
        return;
    };
    let path = node_text(qualified, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if path.is_empty() {
        return;
    }
    ctx.imports.push(ParsedImport {
        file_id: ctx.file.id.clone(),
        owner_id: None,
        path,
        alias: Some("__kotlin_package__".to_string()),
        is_glob: false,
        is_reexport: true,
        is_static: false,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-kotlin", "package declaration"),
        kind: ImportKind::Unspecified,
        imported_name: None,
        is_global: false,
    });
}

pub(crate) fn extract_kotlin_import(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    // Layout: `import` qualified_identifier (`as` identifier)? (`.*`)?
    // The `*` and `as` keywords are anonymous tokens and don't surface as
    // named children, so we have to inspect the raw text for both.
    let Some(path_node) = kotlin_first_child_of_kind(node, "qualified_identifier")
        .or_else(|| kotlin_first_child_of_kind(node, "identifier"))
    else {
        return;
    };
    let path = node_text(path_node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if path.is_empty() {
        return;
    }

    let raw = node_text(node, ctx.source).unwrap_or_default();
    let is_glob = raw.trim_end().ends_with(".*");

    // Alias is the *last* named `identifier` child after the qualified
    // identifier, present only when the source contains ` as `.
    let alias = if raw.contains(" as ") {
        let mut cursor = node.walk();
        node.named_children(&mut cursor)
            .filter(|child| child.kind() == "identifier")
            .last()
            .and_then(|child| node_text(child, ctx.source).ok())
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty())
    } else {
        None
    };

    let kind = if is_glob {
        ImportKind::Wildcard
    } else {
        ImportKind::Named
    };
    let imported_name = if is_glob {
        None
    } else if let Some(alias_name) = alias.clone() {
        Some(alias_name)
    } else {
        Some(last_path_segment(&path))
    };

    ctx.imports.push(ParsedImport {
        file_id: ctx.file.id.clone(),
        owner_id,
        path,
        alias,
        is_glob,
        is_reexport: false,
        is_static: false,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-kotlin", "import declaration"),
        kind,
        imported_name,
        is_global: false,
    });
}

pub(crate) fn extract_kotlin_call_expression(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    // `call_expression` is `<expression> <type_arguments>? <value_arguments> <annotated_lambda>?`.
    // The callee expression is the first child of `expression` kind (or
    // `navigation_expression`/`identifier` if the grammar inlined the
    // expression supertype). We hand back the *raw* callee text as
    // `target_text` and decompose into name + receiver so the resolver can
    // route both pure `foo(x)` and `obj.foo(x)` calls.
    let raw = node_text(node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if raw.is_empty() {
        return;
    }
    let callee_node = node.named_child(0);
    let Some(callee) = callee_node else {
        return;
    };

    let (name, receiver, call_kind) = match callee.kind() {
        "navigation_expression" => {
            // Receiver is the first child; method name is the last `identifier`.
            let mut cursor = callee.walk();
            let children = callee.named_children(&mut cursor).collect::<Vec<_>>();
            let method = children
                .iter()
                .rev()
                .find(|child| child.kind() == "identifier")
                .and_then(|child| node_text(*child, ctx.source).ok())
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty())
                .unwrap_or_else(|| method_name_from_text(&raw));
            let receiver = children
                .first()
                .and_then(|child| node_text(*child, ctx.source).ok())
                .map(|text| text.trim().to_string())
                .filter(|text| !text.is_empty());
            (method, receiver, ParsedCallKind::Method)
        }
        "identifier" => {
            let name = node_text(callee, ctx.source)
                .unwrap_or_default()
                .trim()
                .to_string();
            (name, None, ParsedCallKind::Direct)
        }
        _ => {
            // Fallback: derive callee name from raw text. Captures shapes
            // like `Foo<T>()` (callee is `user_type`) or chained calls.
            let name = method_name_from_text(&raw);
            let receiver = receiver_from_method_text(&raw, &name);
            let call_kind = if receiver.is_some() {
                ParsedCallKind::Method
            } else {
                ParsedCallKind::Direct
            };
            (name, receiver, call_kind)
        }
    };
    if name.is_empty() {
        return;
    }
    let arity = kotlin_first_child_of_kind(node, "value_arguments")
        .map(kotlin_argument_count)
        .unwrap_or_default();

    ctx.calls.push(ParsedCall {
        file_id: ctx.file.id.clone(),
        caller_id: owner_id.clone(),
        name,
        target_text: raw,
        receiver,
        arity,
        kind: call_kind,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-kotlin", "call_expression"),
        confidence: Confidence::CandidateSet,
    });
    extract_body_hit(node, BodyHitKind::Call, ctx, owner_id);
}

pub(crate) fn extract_kotlin_constructor_invocation(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    // `constructor_invocation` is `<type> <value_arguments>`. Used in
    // `delegation_specifier` and (per the grammar) as a sub-form of call.
    // We treat it as a `Direct` construction so the resolver can match it
    // against the target class.
    let Some(type_node) = kotlin_first_child_of_kind(node, "type")
        .or_else(|| kotlin_first_child_of_kind(node, "user_type"))
    else {
        return;
    };
    let target_text = node_text(type_node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if target_text.is_empty() {
        return;
    }
    let arity = kotlin_first_child_of_kind(node, "value_arguments")
        .map(kotlin_argument_count)
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
        provenance: Provenance::new("tree-sitter-kotlin", "constructor_invocation"),
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

/// kotlin spec §4g: emit the delegate target call (e.g. `lazy` /
/// `Delegates.observable`) as a `ParsedCall` whose `caller_id` is the
/// enclosing property. The variable symbol itself is still emitted by
/// `kotlin_property_symbols`; this helper only adds the delegate-binding
/// call so a graph query like "what does `x` delegate to?" resolves.
pub(crate) fn extract_kotlin_property_delegate_call(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let Some(inner) = kotlin_property_delegate_inner(node) else {
        return;
    };
    let (name, receiver, kind) = match inner.kind() {
        "call_expression" => kotlin_call_expression_callee_summary(inner, ctx.source),
        "navigation_expression" => kotlin_navigation_call_summary(inner, ctx.source),
        "identifier" => {
            let raw = node_text(inner, ctx.source)
                .unwrap_or_default()
                .trim()
                .to_string();
            if raw.is_empty() {
                return;
            }
            (raw, None, ParsedCallKind::Direct)
        }
        _ => return,
    };
    if name.is_empty() {
        return;
    }
    let raw = node_text(inner, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    let arity = kotlin_first_child_of_kind(inner, "value_arguments")
        .map(kotlin_argument_count)
        .unwrap_or_default();
    ctx.calls.push(ParsedCall {
        file_id: ctx.file.id.clone(),
        caller_id: owner_id,
        name,
        target_text: raw,
        receiver,
        arity,
        kind,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-kotlin", "property_delegate"),
        confidence: Confidence::CandidateSet,
    });
}

/// Walk children of a `property_delegate` *below* its immediate call
/// expression so calls inside any trailing lambda body are still captured
/// without re-emitting the delegate-target call itself (which
/// `extract_kotlin_property_delegate_call` already handled). For non-call
/// inner expressions (`property by someValue`) we delegate to the normal
/// child walker.
pub(crate) fn visit_kotlin_property_delegate_children(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<(SymbolId, SymbolKind)>,
    owner_symbol: Option<SymbolId>,
) {
    let Some(inner) = kotlin_property_delegate_inner(node) else {
        visit_kotlin_children(node, ctx, parent_symbol, owner_symbol);
        return;
    };
    if !matches!(inner.kind(), "call_expression" | "navigation_expression") {
        visit_kotlin_children(node, ctx, parent_symbol, owner_symbol);
        return;
    }
    visit_kotlin_delegate_call_body(inner, ctx, parent_symbol, owner_symbol);
}

/// Walk the children of a delegate's call/navigation expression, suppressing
/// the immediate call/callee emission so the delegate target call is not
/// duplicated. Trailing-lambda forms (`foo() { ... }`) parse as nested
/// `call_expression`s, so we recurse one level deeper while still skipping
/// the inner call's callee.
fn visit_kotlin_delegate_call_body(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<(SymbolId, SymbolKind)>,
    owner_symbol: Option<SymbolId>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            // Skip the immediate callee — its name is already captured by
            // the delegate-call summary, so re-emitting via the normal
            // call_expression / navigation_expression path would duplicate.
            "expression" | "identifier" | "navigation_expression" => continue,
            // Trailing-lambda parse: the outer call_expression's first child
            // is the inner call_expression. Recurse one level deeper.
            "call_expression" => {
                visit_kotlin_delegate_call_body(
                    child,
                    ctx,
                    parent_symbol.clone(),
                    owner_symbol.clone(),
                );
            }
            _ => visit_kotlin_node(child, ctx, parent_symbol.clone(), owner_symbol.clone()),
        }
    }
}

fn kotlin_property_delegate_inner(node: Node<'_>) -> Option<Node<'_>> {
    // `property_delegate`'s only child is the delegate expression (concrete
    // type, e.g. `call_expression`, `navigation_expression`, `identifier`).
    let mut cursor = node.walk();
    node.named_children(&mut cursor).next()
}

fn kotlin_call_expression_callee_summary(
    node: Node<'_>,
    source: &str,
) -> (String, Option<String>, ParsedCallKind) {
    let raw = node_text(node, source)
        .unwrap_or_default()
        .trim()
        .to_string();
    let Some(callee) = node.named_child(0) else {
        return (method_name_from_text(&raw), None, ParsedCallKind::Direct);
    };
    match callee.kind() {
        "navigation_expression" => kotlin_navigation_call_summary(callee, source),
        "identifier" => {
            let name = node_text(callee, source)
                .unwrap_or_default()
                .trim()
                .to_string();
            (name, None, ParsedCallKind::Direct)
        }
        // Trailing-lambda form: `foo()(bar) { ... }` parses as an outer
        // `call_expression` whose callee is itself a `call_expression`. Use
        // the inner callee's name as the delegate target.
        "call_expression" => kotlin_call_expression_callee_summary(callee, source),
        _ => {
            let name = method_name_from_text(&raw);
            let receiver = receiver_from_method_text(&raw, &name);
            let kind = if receiver.is_some() {
                ParsedCallKind::Method
            } else {
                ParsedCallKind::Direct
            };
            (name, receiver, kind)
        }
    }
}

fn kotlin_navigation_call_summary(
    node: Node<'_>,
    source: &str,
) -> (String, Option<String>, ParsedCallKind) {
    let mut cursor = node.walk();
    let children = node.named_children(&mut cursor).collect::<Vec<_>>();
    let method = children
        .iter()
        .rev()
        .find(|child| child.kind() == "identifier")
        .and_then(|child| node_text(*child, source).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_default();
    let receiver = children
        .first()
        .and_then(|child| node_text(*child, source).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());
    (method, receiver, ParsedCallKind::Method)
}

pub(crate) fn extract_kotlin_navigation_expression(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    // We emit:
    //   - a Field reference for the trailing identifier (the "member" half),
    //   - a Type/Path reference for the receiver when it's a single
    //     identifier whose first character is uppercase (i.e. resolves
    //     to a class / object / companion). Lowercase receivers are skipped
    //     because they're typically values, not declarations.
    let raw = node_text(node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if raw.is_empty() {
        return;
    }

    let mut cursor = node.walk();
    let children = node.named_children(&mut cursor).collect::<Vec<_>>();
    let Some(last_id) = children
        .iter()
        .rev()
        .find(|child| child.kind() == "identifier")
    else {
        return;
    };
    let field_text = node_text(*last_id, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if !field_text.is_empty() && !is_kotlin_keyword(&field_text) {
        ctx.references.push(ParsedReference {
            file_id: ctx.file.id.clone(),
            owner_id: owner_id.clone(),
            text: field_text.clone(),
            kind: ReferenceKind::Field,
            span: span_from_node(*last_id),
            provenance: Provenance::new("tree-sitter-kotlin", "navigation_expression field"),
        });
    }

    // Receiver type reference. Only emit when the receiver is a single
    // capitalised identifier (object, companion, class name); a lowercase
    // receiver is almost always a property/local and would dominate the
    // reference list.
    if let Some(first_child) = children.first()
        && first_child.kind() == "identifier"
    {
        let receiver_text = node_text(*first_child, ctx.source)
            .unwrap_or_default()
            .trim()
            .to_string();
        if let Some(first) = receiver_text.chars().next()
            && first.is_ascii_uppercase()
            && !is_kotlin_keyword(&receiver_text)
        {
            ctx.references.push(ParsedReference {
                file_id: ctx.file.id.clone(),
                owner_id: owner_id.clone(),
                text: receiver_text,
                kind: ReferenceKind::Type,
                span: span_from_node(*first_child),
                provenance: Provenance::new("tree-sitter-kotlin", "navigation_expression receiver"),
            });
        }
    }

    // Multi-segment path? Emit a `Path` body hit on the whole expression
    // so `a.b.c` shows up the same way `a.b.c` would in Java's
    // `scoped_identifier`.
    if raw.matches('.').count() >= 2 {
        ctx.body_hits.push(BodyHit {
            file_id: ctx.file.id.clone(),
            owner_id,
            text: raw,
            kind: BodyHitKind::Path,
            span: span_from_node(node),
        });
    }
}

pub(crate) fn extract_kotlin_user_type_reference(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    // `user_type` is the named type form `Foo` / `Foo.Bar` / `Foo<T>`.
    // Emit a Type reference for the simple leaf and a Path body hit when
    // multi-segment so signatures like `kotlin.text.StringBuilder` get
    // tracked.
    let text = node_text(node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if text.is_empty() || is_kotlin_keyword(&text) {
        return;
    }
    // Skip if the parent is the extension-function receiver position;
    // we already capture that text as `language_identity` and emitting
    // an extra Type reference doubles the count.
    if let Some(parent) = node.parent()
        && parent.kind() == "function_declaration"
        && let Some(idx) = kotlin_child_index_of(parent, node)
        && kotlin_user_type_is_extension_receiver(parent, idx)
    {
        return;
    }
    let leaf = kotlin_first_child_of_kind(node, "identifier")
        .and_then(|child| node_text(child, ctx.source).ok())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| text.clone());

    ctx.references.push(ParsedReference {
        file_id: ctx.file.id.clone(),
        owner_id: owner_id.clone(),
        text: leaf.clone(),
        kind: ReferenceKind::Type,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-kotlin", "user_type reference"),
    });
    ctx.body_hits.push(BodyHit {
        file_id: ctx.file.id.clone(),
        owner_id,
        text,
        kind: BodyHitKind::Type,
        span: span_from_node(node),
    });
}

pub(crate) fn extract_kotlin_annotation_reference(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    // `annotation` wraps a `constructor_invocation` (or just a `type`) plus
    // an optional `use_site_target`. The name lives under `type -> user_type
    // -> identifier`.
    let name_node = kotlin_first_child_of_kind(node, "constructor_invocation")
        .and_then(|inv| kotlin_first_child_of_kind(inv, "type"))
        .or_else(|| kotlin_first_child_of_kind(node, "type"));
    let text = name_node
        .and_then(|n| node_text(n, ctx.source).ok())
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
    if text.is_empty() || is_kotlin_keyword(&text) {
        return;
    }
    let leaf = text.rsplit('.').next().unwrap_or(text.as_str()).to_string();
    let span = name_node.map(span_from_node).unwrap_or_else(|| {
        let raw_span = span_from_node(node);
        SourceSpan::new(
            raw_span.start_byte.saturating_add(1).min(raw_span.end_byte),
            raw_span.end_byte,
            raw_span.start,
            raw_span.end,
        )
    });
    ctx.references.push(ParsedReference {
        file_id: ctx.file.id.clone(),
        owner_id: owner_id.clone(),
        text: leaf.clone(),
        kind: ReferenceKind::Attribute,
        span,
        provenance: Provenance::new("tree-sitter-kotlin", "annotation reference"),
    });
    ctx.body_hits.push(BodyHit {
        file_id: ctx.file.id.clone(),
        owner_id,
        text: leaf,
        kind: BodyHitKind::Attribute,
        span,
    });
}

pub(crate) fn dedup_kotlin_facts(ctx: &mut ExtractContext<'_>) {
    let mut references: HashSet<(u32, ReferenceKind)> = HashSet::new();
    ctx.references
        .retain(|reference| references.insert((reference.span.start_byte, reference.kind)));
    let mut body_hits: HashSet<(u32, BodyHitKind)> = HashSet::new();
    ctx.body_hits
        .retain(|hit| body_hits.insert((hit.span.start_byte, hit.kind)));
}

// ---- helpers ---------------------------------------------------------------

fn kotlin_node_name(node: Node<'_>, source: &str) -> Option<String> {
    if let Some(name) = node
        .child_by_field_name("name")
        .and_then(|child| node_text(child, source).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
    {
        return Some(name);
    }
    // Type alias / extension function names come from an `identifier` child
    // that's not necessarily exposed as the `name` field in every grammar
    // version. We *also* want to skip the leading extension `user_type` here
    // — the first `identifier` after any `modifiers` / `user_type` is the
    // declared name.
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "modifiers" | "type_modifiers" | "type_parameters" | "user_type" | "type"
            | "nullable_type" => continue,
            "identifier" => {
                if let Ok(text) = node_text(child, source) {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        return Some(trimmed.to_string());
                    }
                }
            }
            _ => break,
        }
    }
    None
}

fn kotlin_class_body(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| matches!(child.kind(), "class_body" | "enum_class_body"))
}

fn kotlin_class_is_interface(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| child.kind() == "interface")
}

fn kotlin_class_is_enum(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    if node
        .named_children(&mut cursor)
        .any(|child| child.kind() == "enum_class_body")
    {
        return true;
    }
    kotlin_class_modifier_present(node, "enum")
}

fn kotlin_class_modifier_present(node: Node<'_>, modifier: &str) -> bool {
    kotlin_modifier_matches(
        node,
        &[
            "class_modifier",
            "inheritance_modifier",
            "property_modifier",
        ],
        modifier,
    )
}

fn kotlin_function_modifier_present(node: Node<'_>, modifier: &str) -> bool {
    kotlin_modifier_matches(
        node,
        &["function_modifier", "inheritance_modifier"],
        modifier,
    )
}

fn kotlin_member_modifier_present(node: Node<'_>, modifier: &str) -> bool {
    kotlin_modifier_matches(node, &["member_modifier", "inheritance_modifier"], modifier)
}

fn kotlin_modifier_matches(node: Node<'_>, kinds: &[&str], modifier: &str) -> bool {
    let Some(modifiers) = kotlin_modifiers_node(node) else {
        return false;
    };
    let mut cursor = modifiers.walk();
    for child in modifiers.named_children(&mut cursor) {
        if !kinds.contains(&child.kind()) {
            continue;
        }
        let mut inner = child.walk();
        let any_text_matches = child
            .children(&mut inner)
            .any(|leaf| leaf.kind() == modifier);
        if any_text_matches {
            return true;
        }
    }
    false
}

fn kotlin_modifiers_node(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.kind() == "modifiers")
}

fn kotlin_attributes_for_node(node: Node<'_>, source: &str) -> Vec<String> {
    let mut attributes = Vec::new();
    let Some(modifiers) = kotlin_modifiers_node(node) else {
        return attributes;
    };
    let mut cursor = modifiers.walk();
    for child in modifiers.named_children(&mut cursor) {
        if child.kind() == "annotation"
            && let Some(name) = kotlin_annotation_name(child, source)
        {
            attributes.push(format!("kotlin:annotation:{name}"));
            let leaf = name.rsplit('.').next().unwrap_or(name.as_str());
            match leaf {
                "Test" | "ParameterizedTest" => attributes.push("junit:test".to_string()),
                "Override" => attributes.push("kotlin:override".to_string()),
                _ => {}
            }
        }
    }
    attributes
}

fn kotlin_annotation_name(node: Node<'_>, source: &str) -> Option<String> {
    let target = kotlin_first_child_of_kind(node, "constructor_invocation")
        .and_then(|inv| kotlin_first_child_of_kind(inv, "type"))
        .or_else(|| kotlin_first_child_of_kind(node, "type"))?;
    node_text(target, source)
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

fn kotlin_visibility_text(node: Node<'_>, source: &str) -> Option<String> {
    let modifiers = kotlin_modifiers_node(node)?;
    let mut cursor = modifiers.walk();
    for child in modifiers.named_children(&mut cursor) {
        if child.kind() != "visibility_modifier" {
            continue;
        }
        let raw = node_text(child, source).unwrap_or_default().trim();
        if matches!(raw, "public" | "protected" | "private" | "internal") {
            return Some(raw.to_string());
        }
        // The grammar may emit visibility as a child anonymous token; walk
        // children of the visibility_modifier node and pick the first leaf.
        let mut inner = child.walk();
        for leaf in child.children(&mut inner) {
            if matches!(leaf.kind(), "public" | "protected" | "private" | "internal") {
                return Some(leaf.kind().to_string());
            }
        }
    }
    None
}

fn kotlin_docs_for_node(node: Node<'_>, source: &str) -> Vec<String> {
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

fn kotlin_type_inheritance_names(node: Node<'_>, source: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "delegation_specifiers" {
            let mut inner = child.walk();
            for spec in child.named_children(&mut inner) {
                if spec.kind() != "delegation_specifier" {
                    continue;
                }
                collect_kotlin_type_names(spec, source, &mut names);
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

fn collect_kotlin_type_names(node: Node<'_>, source: &str, names: &mut Vec<String>) {
    if matches!(node.kind(), "user_type" | "type")
        && let Ok(text) = node_text(node, source)
        && let Some(name) = kotlin_type_name_from_text(text)
    {
        names.push(name);
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_kotlin_type_names(child, source, names);
    }
}

fn kotlin_type_name_from_text(text: &str) -> Option<String> {
    let clean = text
        .split('<')
        .next()
        .unwrap_or(text)
        .trim()
        .trim_end_matches('?')
        .trim_end_matches("()")
        .trim()
        .to_string();
    // `delegation_specifier` may contain `Foo()` (constructor call) — strip
    // the trailing parens we just removed and any remaining whitespace.
    let clean = clean.split('(').next().unwrap_or(clean.as_str()).trim();
    if clean.is_empty() || is_kotlin_keyword(clean) {
        None
    } else {
        Some(clean.to_string())
    }
}

fn kotlin_property_type(node: Node<'_>, source: &str) -> Option<String> {
    // The property type lives under `variable_declaration -> type` or as a
    // direct `user_type` / `type` child of the property_declaration.
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "variable_declaration" => {
                if let Some(t) = kotlin_first_child_of_kind(child, "type")
                    .or_else(|| kotlin_first_child_of_kind(child, "user_type"))
                {
                    return node_text(t, source)
                        .ok()
                        .and_then(kotlin_type_name_from_text);
                }
            }
            "type" | "user_type" | "nullable_type" | "parenthesized_type" => {
                return node_text(child, source)
                    .ok()
                    .and_then(kotlin_type_name_from_text);
            }
            _ => {}
        }
    }
    None
}

fn kotlin_class_parameter_type(node: Node<'_>, source: &str) -> Option<String> {
    kotlin_first_child_of_kind(node, "type")
        .or_else(|| kotlin_first_child_of_kind(node, "user_type"))
        .and_then(|child| node_text(child, source).ok())
        .and_then(kotlin_type_name_from_text)
}

fn kotlin_class_parameter_declares_property(node: Node<'_>, _source: &str) -> bool {
    // A `class_parameter` declares a property when its modifiers contain
    // `val` or `var` (which the grammar emits as anonymous tokens inside
    // `property_modifier` / `member_modifier` / unscoped). The textual scan
    // is safe because Kotlin doesn't have a `val`/`var` identifier.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if matches!(child.kind(), "val" | "var") {
            return true;
        }
        if child.kind() == "modifiers" {
            let mut inner = child.walk();
            for grand in child.children(&mut inner) {
                if matches!(grand.kind(), "val" | "var") {
                    return true;
                }
                let mut leaf_cursor = grand.walk();
                for leaf in grand.children(&mut leaf_cursor) {
                    if matches!(leaf.kind(), "val" | "var") {
                        return true;
                    }
                }
            }
        }
    }
    false
}

fn kotlin_property_is_var(node: Node<'_>, _source: &str) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "var" {
            return true;
        }
        if child.kind() == "val" {
            return false;
        }
    }
    false
}

/// Returns `(receiver_type_text, is_simple_user_type)`. `None` if the
/// function has no receiver. `is_simple_user_type` is true when the
/// receiver is a single resolvable type identifier (no nullability, no
/// generics, no `<` / `?` / `.`), so the caller can pick `ExactSyntax` vs
/// `Partial` confidence per spec §4(c).
fn kotlin_extension_receiver(node: Node<'_>, source: &str) -> (Option<String>, bool) {
    let mut cursor = node.walk();
    let children = node.named_children(&mut cursor).collect::<Vec<_>>();
    // Find the function name (first `identifier` after optional modifiers
    // and type-parameter blocks). If a `user_type` / `nullable_type` appears
    // *before* the name and *after* the modifiers/type-parameters block,
    // it's the extension receiver.
    let mut receiver_type: Option<Node<'_>> = None;
    for child in &children {
        match child.kind() {
            "modifiers" | "type_modifiers" | "type_parameters" => continue,
            "user_type" | "nullable_type" | "parenthesized_type" => {
                receiver_type = Some(*child);
                continue;
            }
            "identifier" => break,
            _ => {}
        }
    }
    let Some(receiver_node) = receiver_type else {
        return (None, false);
    };
    let text = node_text(receiver_node, source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if text.is_empty() {
        return (None, false);
    }
    let is_simple = receiver_node.kind() == "user_type"
        && !text.contains('<')
        && !text.contains('?')
        && !text.contains('.');
    (Some(text), is_simple)
}

/// Collect identifier names of every `reified` type parameter on a
/// `function_declaration`. The grammar shape is
/// `type_parameters -> type_parameter -> [type_parameter_modifiers ->
/// reification_modifier 'reified'] identifier`. Caller is responsible for
/// gating on the `inline` modifier (only inline functions accept `reified`).
fn kotlin_reified_type_parameters(node: Node<'_>, source: &str) -> Vec<String> {
    let Some(params) = kotlin_first_child_of_kind(node, "type_parameters") else {
        return Vec::new();
    };
    let mut names = Vec::new();
    let mut cursor = params.walk();
    for param in params.named_children(&mut cursor) {
        if param.kind() != "type_parameter" {
            continue;
        }
        // Must carry a `reification_modifier` inside its
        // `type_parameter_modifiers` block.
        let has_reified = kotlin_first_child_of_kind(param, "type_parameter_modifiers")
            .map(|modifiers| {
                let mut mod_cursor = modifiers.walk();
                modifiers
                    .named_children(&mut mod_cursor)
                    .any(|child| child.kind() == "reification_modifier")
            })
            .unwrap_or(false);
        if !has_reified {
            continue;
        }
        if let Some(name) = kotlin_first_child_of_kind(param, "identifier")
            .and_then(|child| node_text(child, source).ok())
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty())
        {
            names.push(name);
        }
    }
    names
}

fn kotlin_user_type_is_extension_receiver(parent: Node<'_>, idx: usize) -> bool {
    // Used by `extract_kotlin_user_type_reference` to skip emitting a type
    // reference for the extension-function receiver type (we already track
    // it via `language_identity`).
    if parent.kind() != "function_declaration" {
        return false;
    }
    let mut cursor = parent.walk();
    let children = parent.named_children(&mut cursor).collect::<Vec<_>>();
    for (i, child) in children.iter().enumerate() {
        if i == idx {
            return child.kind() == "user_type";
        }
        if matches!(
            child.kind(),
            "modifiers" | "type_modifiers" | "type_parameters"
        ) {
            continue;
        }
        if child.kind() == "identifier" {
            return false;
        }
    }
    false
}

fn kotlin_parameter_count(node: Node<'_>) -> usize {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| child.kind() == "parameter")
        .count()
}

fn kotlin_argument_count(node: Node<'_>) -> usize {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| child.kind() == "value_argument")
        .count()
}

fn kotlin_first_child_of_kind<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.kind() == kind)
}

/// kotlin spec §4f: if `node` (a class/object declaration) is the immediate
/// child of a `sealed` parent class's body, return the parent's identifier.
/// Used to emit a Type reference from each sealed sibling back to the
/// parent so `references_to_symbol(Parent)` lists the enumerated cases.
fn kotlin_sealed_parent_name(node: Node<'_>, source: &str) -> Option<String> {
    let body = node.parent()?;
    // Sealed children live in `class_body -> class_member_declaration ->
    // declaration -> class_declaration|object_declaration`. The grammar may
    // inline `class_member_declaration` and `declaration` as subtypes, so
    // the parent could be `class_body` directly or one of those wrappers.
    let mut current = Some(body);
    while let Some(walker) = current {
        if walker.kind() == "class_body" {
            break;
        }
        if matches!(walker.kind(), "class_declaration" | "object_declaration") {
            // Already past the body; we're in our own declaration's chain.
            return None;
        }
        current = walker.parent();
    }
    let class_body = current?;
    let parent_decl = class_body.parent()?;
    if parent_decl.kind() != "class_declaration" {
        return None;
    }
    if !kotlin_class_modifier_present(parent_decl, "sealed") {
        return None;
    }
    kotlin_first_child_of_kind(parent_decl, "identifier")
        .or_else(|| parent_decl.child_by_field_name("name"))
        .and_then(|child| node_text(child, source).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

/// True when `node` is nested inside a `companion_object` ancestor. Used
/// to tag companion-object members so cross-file resolvers can route
/// `Host.factory()` against them.
fn kotlin_node_is_inside_companion(node: Node<'_>) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "companion_object" => return true,
            // A *named* class/object enclosing the function caps the walk —
            // we don't want to claim companion membership transitively past
            // an inner class.
            "class_declaration" | "object_declaration" | "function_declaration" => {
                return false;
            }
            _ => {}
        }
        current = parent.parent();
    }
    false
}

fn kotlin_child_index_of(parent: Node<'_>, target: Node<'_>) -> Option<usize> {
    let mut cursor = parent.walk();
    parent
        .named_children(&mut cursor)
        .position(|child| child.id() == target.id())
}

fn kotlin_symbol_name_from_id(id: &SymbolId) -> Option<String> {
    // `symbol_id` formats as `<scope>::<kind>:<safe_name>@<byte>`. Reverse
    // it to recover the class name for `secondary_constructor` symbols
    // whose AST node doesn't carry the enclosing class name.
    let raw = &id.0;
    let after_colon = raw.rsplit_once("::")?.1;
    let after_kind = after_colon.split_once(':')?.1;
    let before_at = after_kind.rsplit_once('@')?.0;
    if before_at.is_empty() {
        None
    } else {
        Some(before_at.to_string())
    }
}

fn is_kotlin_literal(kind: &str) -> bool {
    matches!(
        kind,
        "string_literal"
            | "multiline_string_literal"
            | "number_literal"
            | "float_literal"
            | "boolean_literal"
            | "character_literal"
            | "null_literal"
    )
}

fn is_kotlin_keyword(text: &str) -> bool {
    matches!(
        text,
        "abstract"
            | "actual"
            | "annotation"
            | "as"
            | "break"
            | "by"
            | "catch"
            | "class"
            | "companion"
            | "const"
            | "constructor"
            | "continue"
            | "crossinline"
            | "data"
            | "do"
            | "dynamic"
            | "else"
            | "enum"
            | "expect"
            | "external"
            | "false"
            | "field"
            | "file"
            | "final"
            | "finally"
            | "for"
            | "fun"
            | "get"
            | "if"
            | "import"
            | "in"
            | "infix"
            | "init"
            | "inline"
            | "inner"
            | "interface"
            | "internal"
            | "is"
            | "it"
            | "lateinit"
            | "noinline"
            | "null"
            | "object"
            | "open"
            | "operator"
            | "out"
            | "override"
            | "package"
            | "param"
            | "private"
            | "property"
            | "protected"
            | "public"
            | "receiver"
            | "reified"
            | "return"
            | "sealed"
            | "set"
            | "setparam"
            | "super"
            | "suspend"
            | "tailrec"
            | "this"
            | "throw"
            | "true"
            | "try"
            | "typealias"
            | "val"
            | "value"
            | "var"
            | "vararg"
            | "when"
            | "where"
            | "while"
    )
}

fn is_kotlin_test_symbol(
    relative_path: &str,
    kind: SymbolKind,
    name: &str,
    attributes: &[String],
) -> bool {
    matches!(
        kind,
        SymbolKind::Method | SymbolKind::Class | SymbolKind::Function
    ) && (relative_path.contains("/test/")
        || relative_path.contains("/src/test/")
        || relative_path.ends_with("Test.kt")
        || relative_path.ends_with("Tests.kt")
        || name.ends_with("Test")
        || name.ends_with("Tests")
        || attributes.iter().any(|attribute| attribute == "junit:test"))
}
