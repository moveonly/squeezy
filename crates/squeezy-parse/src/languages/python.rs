use crate::languages::js_ts::{
    is_python_identifier, python_assignment_target, python_from_imports, python_plain_imports,
    python_simple_assignment_name, python_string_list_values,
};
use crate::languages::rust::*;
use crate::*;

pub(crate) fn extract_python(file: FileRecord, source: &str, tree: &Tree) -> ParsedFile {
    let mut ctx = ExtractContext::new(file.clone(), source);
    let root = tree.root_node();
    record_parse_error_diagnostics(root, &mut ctx);

    visit_python_node(root, &mut ctx, None, None);
    extract_python_module_exports(&mut ctx);
    dedup_python_facts(&mut ctx);

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

use std::collections::HashSet;

pub(crate) fn extract_python_module_exports(ctx: &mut ExtractContext<'_>) {
    for line in ctx.source.lines() {
        let line = line.trim();
        // Require a word boundary after `__all__` so identifiers like
        // `__all__module = "x"` and `__all_xs = "x"` (the latter does not even
        // share the full prefix) are not matched as `__all__` assignments,
        // and so docstring/string content of the form `__all__ = ["fake"]`
        // is only accepted when it is genuinely at line start.
        let Some(rest) = line.strip_prefix("__all__") else {
            continue;
        };
        if !rest.starts_with(|ch: char| ch.is_whitespace() || ch == '=' || ch == '+') {
            continue;
        }
        let Some((_, right)) = rest.split_once('=') else {
            continue;
        };
        for exported in python_string_list_values(right) {
            let imported_name = Some(exported.clone());
            ctx.imports.push(ParsedImport {
                file_id: ctx.file.id.clone(),
                owner_id: None,
                path: exported,
                alias: None,
                is_glob: false,
                is_reexport: true,
                is_static: false,
                span: SourceSpan::new(0, 0, SourcePoint::new(0, 0), SourcePoint::new(0, 0)),
                provenance: Provenance::new("tree-sitter-python", "__all__ export"),
                kind: ImportKind::Named,
                imported_name,
                is_global: false,
            });
        }
    }
}

pub(crate) fn dedup_python_facts(ctx: &mut ExtractContext<'_>) {
    let mut imports = HashSet::new();
    ctx.imports.retain(|import| {
        imports.insert(format!(
            "{}|{:?}|{}|{:?}|{}|{}",
            import.file_id.0,
            import.owner_id.as_ref().map(|id| id.0.as_str()),
            import.path,
            import.alias,
            import.is_glob,
            import.is_reexport
        ))
    });
}

pub(crate) fn visit_python_node(
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
    if matches!(kind, "import_statement" | "import_from_statement") {
        extract_python_import(node, ctx, owner_symbol.clone());
    }

    // PEP 695 `type Alias[T] = ...`. tree-sitter-python models this as a
    // dedicated `type_alias_statement` node (not an `assignment`), which
    // `python_symbol_from_node` does not recognise, so emit the TypeAlias
    // symbol here.
    if kind == "type_alias_statement" {
        extract_python_type_alias(node, ctx, parent_symbol.as_ref());
        visit_python_children(node, ctx, parent_symbol, owner_symbol);
        return;
    }

    if let Some(mut symbol) = python_symbol_from_node(node, ctx, parent_symbol.as_ref()) {
        python_refine_symbol(node, &mut symbol, &ctx.file);
        extract_python_symbol_facts(node, &symbol, ctx);
        python_refine_symbol_facts(&symbol, ctx);
        let next_parent = Some((symbol.id.clone(), symbol.kind));
        let next_owner = if symbol.body_span.is_some() {
            Some(symbol.id.clone())
        } else {
            owner_symbol.clone()
        };
        ctx.symbols.push(symbol);
        visit_python_children(node, ctx, next_parent, next_owner);
        return;
    }

    if kind == "call" && !python_node_is_inside_decorator(node) {
        extract_python_call(node, ctx, owner_symbol.clone());
    } else if matches!(kind, "assignment" | "assignment_statement") {
        extract_python_field_symbol(node, ctx, parent_symbol.as_ref());
        extract_python_assignment(node, ctx, owner_symbol.clone());
    } else if kind == "identifier" {
        extract_python_reference(node, ReferenceKind::Identifier, ctx, owner_symbol.clone());
    } else if kind == "attribute" {
        extract_python_reference(node, ReferenceKind::Field, ctx, owner_symbol.clone());
    } else if is_python_literal(kind) {
        extract_body_hit(node, BodyHitKind::Literal, ctx, owner_symbol.clone());
    }

    visit_python_children(node, ctx, parent_symbol, owner_symbol);
}

pub(crate) fn visit_python_children(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<(SymbolId, SymbolKind)>,
    owner_symbol: Option<SymbolId>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit_python_node(child, ctx, parent_symbol.clone(), owner_symbol.clone());
    }
}

/// Post-process a freshly-built Python symbol with signals the shared
/// `python_symbol_from_node` dispatch does not derive: classes are reclassified
/// to `Enum` (and `NamedTuple`/`TypedDict` tagged) from their bases, and
/// functions/methods gain coroutine (`python:async`) and generator
/// (`python:generator`) markers read off the live tree-sitter node.
pub(crate) fn python_refine_symbol(node: Node<'_>, symbol: &mut ParsedSymbol, file: &FileRecord) {
    if symbol.kind == SymbolKind::Class {
        python_refine_class(symbol, file);
        // An enum subclass is never also a test class; only promote plain
        // classes that carry the test-class attributes.
        if symbol.kind == SymbolKind::Class {
            python_promote_test_symbol(symbol, file);
        }
        return;
    }
    if !matches!(symbol.kind, SymbolKind::Function | SymbolKind::Method) {
        return;
    }
    // `async def` — the `async` keyword is an anonymous leading token, so the
    // signature (which starts at the function node) begins with `async`.
    if symbol
        .signature
        .trim_start()
        .strip_prefix("async")
        .map(|rest| rest.starts_with(|ch: char| ch.is_whitespace()))
        .unwrap_or(false)
    {
        symbol.attributes.push("python:async".to_string());
    }
    // A `yield`/`yield from` anywhere in the body (but not inside a nested
    // function/lambda) makes this a generator.
    if let Some(body) = node.child_by_field_name("body")
        && python_body_has_yield(body)
    {
        symbol.attributes.push("python:generator".to_string());
    }
    symbol.attributes.sort();
    symbol.attributes.dedup();
    python_promote_test_symbol(symbol, file);
}

/// Promote a test-tagged symbol to `SymbolKind::Test` so `decl_search(kind=test)`
/// finds it. The framework attributes (`python:test`/`pytest:test`/...) are
/// added by the shared `python_test_attributes` and are kept. Reclassifying
/// changes the kind embedded in the id, so the id is regenerated.
fn python_promote_test_symbol(symbol: &mut ParsedSymbol, file: &FileRecord) {
    if symbol.kind == SymbolKind::Test {
        return;
    }
    let is_test = symbol.attributes.iter().any(|attribute| {
        matches!(
            attribute.as_str(),
            "python:test" | "pytest:test" | "python:test-class" | "pytest:test-class"
        )
    });
    if !is_test {
        return;
    }
    symbol.kind = SymbolKind::Test;
    symbol.id = symbol_id(
        file,
        symbol.parent_id.as_ref(),
        SymbolKind::Test,
        &symbol.name,
        symbol.span,
    );
}

/// True when `node`'s subtree contains a `yield` expression that belongs to the
/// enclosing function, i.e. not nested inside another `function_definition` or
/// `lambda` (those own their own `yield`s). Generator expressions do not use
/// `yield`, so they need no special handling here.
fn python_body_has_yield(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "yield" => return true,
            // Nested callables own their own yields; do not descend.
            "function_definition" | "lambda" => continue,
            _ => {
                if python_body_has_yield(child) {
                    return true;
                }
            }
        }
    }
    false
}

/// Reclassify a `class` symbol based on its declared bases (recorded as
/// `base:<Name>` attributes by the shared dispatch): an `enum.Enum` family base
/// promotes the class to `SymbolKind::Enum` (so its members can become
/// `Variant`), and `NamedTuple`/`TypedDict` bases are tagged for record-style
/// queries. Reclassifying changes the kind embedded in the symbol id, so the id
/// is regenerated to stay consistent (children are visited with the refined id).
fn python_refine_class(symbol: &mut ParsedSymbol, file: &FileRecord) {
    let mut is_enum = false;
    let mut extra = Vec::new();
    for attribute in &symbol.attributes {
        let Some(base) = attribute.strip_prefix("base:") else {
            continue;
        };
        match base {
            "Enum" | "IntEnum" | "StrEnum" | "Flag" | "IntFlag" | "ReprEnum" => is_enum = true,
            "NamedTuple" => extra.push("python:namedtuple".to_string()),
            "TypedDict" => extra.push("python:typeddict".to_string()),
            _ => {}
        }
    }
    if is_enum {
        symbol.kind = SymbolKind::Enum;
        extra.push("python:enum".to_string());
        // The kind is encoded in the symbol id; regenerate it so the id, the
        // kind, and the parent id propagated to members all agree.
        symbol.id = symbol_id(
            file,
            symbol.parent_id.as_ref(),
            SymbolKind::Enum,
            &symbol.name,
            symbol.span,
        );
    }
    if !extra.is_empty() {
        symbol.attributes.extend(extra);
        symbol.attributes.sort();
        symbol.attributes.dedup();
    }
}

/// Emit base-class Type references for a reclassified Python `Enum`. The shared
/// `extract_python_symbol_facts` only does this for `SymbolKind::Class`, so an
/// enum (reclassified before facts run) would otherwise lose the references to
/// its `Enum`/`IntEnum`/... bases that a plain class records.
pub(crate) fn python_refine_symbol_facts(symbol: &ParsedSymbol, ctx: &mut ExtractContext<'_>) {
    if symbol.kind != SymbolKind::Enum {
        return;
    }
    for base in python_class_bases(&symbol.signature) {
        ctx.references.push(ParsedReference {
            file_id: ctx.file.id.clone(),
            owner_id: Some(symbol.id.clone()),
            text: base,
            kind: ReferenceKind::Type,
            span: symbol.span,
            provenance: Provenance::new("tree-sitter-python", "enum base reference"),
        });
    }
}

pub(crate) fn extract_python_import(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let raw = node_text(node, ctx.source).unwrap_or_default().trim();
    // `import os` / `import os.path` binds the whole module namespace, whereas
    // `from m import foo` binds a named member. Track which form produced each
    // tuple so the binding shape can be classified as Namespace vs Named.
    let (imports, is_plain_module_import) = if let Some(rest) = raw.strip_prefix("from ") {
        (python_from_imports(rest, &ctx.file.relative_path), false)
    } else if let Some(rest) = raw.strip_prefix("import ") {
        (python_plain_imports(rest), true)
    } else {
        (Vec::new(), false)
    };

    for (path, alias, is_glob) in imports {
        let imported_name = if is_glob {
            None
        } else {
            Some(last_path_segment(&path))
        };
        let kind = if is_glob {
            ImportKind::Wildcard
        } else if is_plain_module_import {
            // `import os` / `import os.path [as p]` binds the module namespace.
            ImportKind::Namespace
        } else {
            ImportKind::Named
        };
        ctx.imports.push(ParsedImport {
            file_id: ctx.file.id.clone(),
            owner_id: owner_id.clone(),
            path,
            alias,
            is_glob,
            is_reexport: ctx.file.relative_path.ends_with("__init__.py"),
            is_static: false,
            span: span_from_node(node),
            provenance: Provenance::new("tree-sitter-python", "import declaration"),
            kind,
            imported_name,
            is_global: false,
        });
    }
}

/// Emit a `TypeAlias` symbol for a PEP 695 `type Alias[T] = <type>` statement,
/// recording the RHS as a Type reference. The `left` field holds the alias
/// name (with optional `[T, ...]` type parameters); only the leading
/// identifier is the symbol name.
pub(crate) fn extract_python_type_alias(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) {
    let left = node
        .child_by_field_name("left")
        .and_then(|child| node_text(child, ctx.source).ok())
        .unwrap_or_default();
    // Strip the optional `[T, ...]` type-parameter list, then take the leaf
    // identifier of the alias name.
    let name_text = left.split('[').next().unwrap_or(left).trim();
    let Some(name) = python_field_type_name(name_text) else {
        return;
    };
    let span = span_from_node(node);
    let parent_id = parent_symbol.map(|(id, _)| id.clone());
    let signature = node_text(node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();

    let mut attributes = vec!["python:type-alias".to_string()];
    if let Some(target) = node
        .child_by_field_name("right")
        .and_then(|child| node_text(child, ctx.source).ok())
        .and_then(python_field_type_name)
    {
        attributes.push(format!("type:{target}"));
        ctx.references.push(ParsedReference {
            file_id: ctx.file.id.clone(),
            owner_id: parent_id.clone(),
            text: target,
            kind: ReferenceKind::Type,
            span,
            provenance: Provenance::new("tree-sitter-python", "type alias reference"),
        });
    }

    ctx.symbols.push(ParsedSymbol {
        id: symbol_id(&ctx.file, parent_id.as_ref(), SymbolKind::TypeAlias, &name, span),
        file_id: ctx.file.id.clone(),
        parent_id,
        name,
        kind: SymbolKind::TypeAlias,
        language_identity: None,
        span,
        body_span: None,
        signature_span: None,
        signature,
        visibility: None,
        docs: Vec::new(),
        attributes,
        provenance: Provenance::new("tree-sitter-python", "type_alias_statement declaration"),
        confidence: Confidence::ExactSyntax,
        freshness: Freshness::Fresh,
        arity: None,
    });
}

pub(crate) fn split_python_alias(text: &str) -> (&str, Option<&str>) {
    text.split_once(" as ")
        .map(|(path, alias)| (path.trim(), Some(alias.trim())))
        .unwrap_or_else(|| (text.trim(), None))
}

pub(crate) fn normalize_python_import_module(module: &str, relative_path: &str) -> String {
    let leading_dots = module.chars().take_while(|ch| *ch == '.').count();
    if leading_dots == 0 {
        return module.to_string();
    }

    let suffix = module.trim_start_matches('.');
    let mut package = python_module_path_for_relative_file(relative_path);
    if !relative_path.ends_with("__init__.py") {
        package.pop();
    }
    for _ in 1..leading_dots {
        package.pop();
    }
    if !suffix.is_empty() {
        package.extend(suffix.split('.').filter(|segment| !segment.is_empty()));
    }
    package.join(".")
}

pub(crate) fn python_module_path_for_relative_file(relative_path: &str) -> Vec<&str> {
    relative_path
        .trim_end_matches(".py")
        .trim_end_matches("/__init__")
        .trim_start_matches("src/")
        .split('/')
        .filter(|segment| !segment.is_empty() && *segment != "__init__")
        .collect()
}

pub(crate) fn extract_python_field_symbol(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) {
    // Class/Enum bodies emit member symbols (Field, or Variant for enum
    // members); module scope (no parent) emits binding symbols (Static/Const,
    // or TypeAlias for `X: TypeAlias = ...`). Assignments nested inside a
    // function/method (parent kind Function/Method) are locals and are skipped.
    let parent_kind = parent_symbol.map(|(_, kind)| *kind);
    let is_member = matches!(parent_kind, Some(SymbolKind::Class | SymbolKind::Enum));
    let is_module_level = parent_symbol.is_none();
    if !is_member && !is_module_level {
        return;
    }

    let raw = node_text(node, ctx.source).unwrap_or_default();
    let Some((left, right)) = split_python_assignment_like(raw) else {
        return;
    };
    let Some(name) = python_field_name_from_left(left) else {
        return;
    };
    let span = span_from_node(node);
    let parent_id = parent_symbol.map(|(id, _)| id.clone());

    // Member kind: enum members with a plain `NAME = value` body become
    // Variant; everything else in a class body stays Field. Module-level
    // bindings become TypeAlias (`X: TypeAlias = ...`), Static (ALL_CAPS or a
    // `Final` annotation, i.e. a constant), or Const otherwise.
    let annotation_text = left.split_once(':').map(|(_, ann)| ann.trim());
    let is_type_alias = annotation_text
        .map(|ann| python_field_type_name(ann).as_deref() == Some("TypeAlias"))
        .unwrap_or(false);
    let kind = if is_member {
        if matches!(parent_kind, Some(SymbolKind::Enum)) && python_is_simple_enum_member(left, right)
        {
            SymbolKind::Variant
        } else {
            SymbolKind::Field
        }
    } else if is_type_alias {
        SymbolKind::TypeAlias
    } else if python_binding_is_constant(&name, annotation_text) {
        SymbolKind::Static
    } else {
        SymbolKind::Const
    };

    let mut attributes = match kind {
        SymbolKind::Field => vec!["python:field".to_string()],
        SymbolKind::Variant => vec!["python:enum-member".to_string()],
        SymbolKind::TypeAlias => vec!["python:type-alias".to_string()],
        _ => vec!["python:binding".to_string()],
    };
    // For a TypeAlias the RHS is the aliased type; record it as a Type
    // reference. For other bindings the LHS annotation (if any) is the type.
    if kind == SymbolKind::TypeAlias {
        if let Some(target) = python_field_type_name(right) {
            attributes.push(format!("type:{target}"));
            ctx.references.push(ParsedReference {
                file_id: ctx.file.id.clone(),
                owner_id: parent_id.clone(),
                text: target,
                kind: ReferenceKind::Type,
                span,
                provenance: Provenance::new("tree-sitter-python", "type alias reference"),
            });
        }
    } else if let Some(annotation) = annotation_text.and_then(python_field_type_name) {
        attributes.push(format!("type:{annotation}"));
        ctx.references.push(ParsedReference {
            file_id: ctx.file.id.clone(),
            owner_id: parent_id.clone(),
            text: annotation,
            kind: ReferenceKind::Type,
            span,
            provenance: Provenance::new("tree-sitter-python", "field annotation reference"),
        });
    }
    if matches!(kind, SymbolKind::Field | SymbolKind::Variant) {
        attributes.extend(python_field_attributes(right));
    }
    attributes.sort();
    attributes.dedup();

    let provenance_label = match kind {
        SymbolKind::Variant => "enum variant",
        SymbolKind::TypeAlias => "type alias assignment",
        SymbolKind::Static | SymbolKind::Const => "module binding assignment",
        _ => "class field assignment",
    };

    ctx.symbols.push(ParsedSymbol {
        id: symbol_id(&ctx.file, parent_id.as_ref(), kind, &name, span),
        file_id: ctx.file.id.clone(),
        parent_id,
        name,
        kind,
        language_identity: None,
        span,
        body_span: None,
        signature_span: None,
        signature: raw.trim().to_string(),
        visibility: None,
        docs: Vec::new(),
        attributes,
        provenance: Provenance::new("tree-sitter-python", provenance_label),
        confidence: Confidence::Heuristic,
        freshness: Freshness::Fresh,
        arity: None,
    });
}

/// True when a module-level binding is constant-shaped: an ALL_CAPS name (the
/// PEP 8 constant convention) or a `Final` / `Final[...]` annotation.
pub(crate) fn python_binding_is_constant(name: &str, annotation: Option<&str>) -> bool {
    let is_all_caps = name.chars().any(|ch| ch.is_ascii_alphabetic())
        && name
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_');
    let is_final = annotation
        .map(|ann| {
            let leaf = python_field_type_name(ann);
            leaf.as_deref() == Some("Final") || ann.trim_start().starts_with("Final")
        })
        .unwrap_or(false);
    is_all_caps || is_final
}

/// True for a plain `NAME = value` enum member (e.g. `RED = 1`, `RED = auto()`)
/// as opposed to a method or a non-member assignment such as `_ignore_ = ...`.
pub(crate) fn python_is_simple_enum_member(left: &str, right: &str) -> bool {
    // Members are simple unannotated names; dunder/sunder housekeeping
    // attributes (`__slots__`, `_ignore_`) are not value members.
    if left.contains(':') {
        return false;
    }
    let name = left.trim();
    if name.starts_with('_') || !is_python_identifier(name) {
        return false;
    }
    !right.trim().is_empty()
}

pub(crate) fn python_field_type_name(annotation: &str) -> Option<String> {
    let text = annotation
        .split('=')
        .next()
        .unwrap_or(annotation)
        .trim()
        .trim_matches(|ch: char| {
            matches!(
                ch,
                '\'' | '"' | '[' | ']' | '(' | ')' | '{' | '}' | ':' | ',' | ' '
            )
        });
    if text.is_empty() {
        None
    } else {
        Some(last_path_segment(text))
    }
}

pub(crate) fn split_python_assignment_like(text: &str) -> Option<(&str, &str)> {
    if let Some((left, right)) = text.split_once('=') {
        return Some((left.trim(), right.trim()));
    }
    if let Some((left, annotation)) = text.split_once(':') {
        return Some((left.trim(), annotation.trim()));
    }
    None
}

pub(crate) fn python_field_name_from_left(left: &str) -> Option<String> {
    let name = left
        .split_once(':')
        .map(|(name, _)| name)
        .unwrap_or(left)
        .trim();
    python_simple_assignment_name(name)
}

pub(crate) fn python_field_attributes(right: &str) -> Vec<String> {
    let mut attributes = Vec::new();
    let callee = python_assignment_target(right).unwrap_or_else(|| right.trim().to_string());
    let lowered = callee.to_ascii_lowercase();
    if lowered.contains("column") || lowered.contains("mapped_column") {
        attributes.push("sqlalchemy:field".to_string());
    }
    if callee.contains("models.") && callee.contains("Field") {
        attributes.push("django:field".to_string());
    }
    if lowered.contains("field") {
        attributes.push("python:field-factory".to_string());
        attributes.push("dataclass:field".to_string());
        attributes.push("pydantic:field".to_string());
    }
    attributes
}

pub(crate) fn extract_python_assignment(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let raw = node_text(node, ctx.source).unwrap_or_default();
    let Some((left, right)) = raw.split_once('=') else {
        return;
    };
    let left = left.trim();
    let right = right.trim();
    if left == "__all__" {
        for exported in python_string_list_values(right) {
            let imported_name = Some(exported.clone());
            ctx.imports.push(ParsedImport {
                file_id: ctx.file.id.clone(),
                owner_id: owner_id.clone(),
                path: exported,
                alias: None,
                is_glob: false,
                is_reexport: true,
                is_static: false,
                span: span_from_node(node),
                provenance: Provenance::new("tree-sitter-python", "__all__ export"),
                kind: ImportKind::Named,
                imported_name,
                is_global: false,
            });
        }
        return;
    }

    let Some(alias) = python_simple_assignment_name(left) else {
        return;
    };
    let Some(target) = python_assignment_target(right) else {
        return;
    };
    if alias == last_path_segment(&target) {
        return;
    }

    let imported_name = Some(last_path_segment(&target));
    ctx.imports.push(ParsedImport {
        file_id: ctx.file.id.clone(),
        owner_id,
        path: target,
        alias: Some(alias),
        is_glob: false,
        is_reexport: false,
        is_static: false,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-python", "assignment alias"),
        kind: ImportKind::Named,
        imported_name,
        is_global: false,
    });
}

pub(crate) fn extract_python_call(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
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
    let name = last_path_segment(&target_text);
    let receiver = receiver_from_direct_call(&target_text);
    let arity = node
        .child_by_field_name("arguments")
        .or_else(|| {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .find(|child| child.kind() == "argument_list")
        })
        .map(|arguments| named_child_count(arguments))
        .unwrap_or_default();

    ctx.calls.push(ParsedCall {
        file_id: ctx.file.id.clone(),
        caller_id: owner_id.clone(),
        name,
        target_text: target_text.clone(),
        receiver,
        arity,
        kind: if target_text.contains('.') {
            ParsedCallKind::Method
        } else {
            ParsedCallKind::Direct
        },
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-python", "call"),
        confidence: Confidence::Heuristic,
    });
    extract_body_hit(node, BodyHitKind::Call, ctx, owner_id);
}

pub(crate) fn extract_python_reference(
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
        provenance: Provenance::new("tree-sitter-python", format!("{} reference", node.kind())),
    });
    ctx.body_hits.push(BodyHit {
        file_id: ctx.file.id.clone(),
        owner_id,
        text,
        kind: body_kind,
        span: span_from_node(node),
    });
}
