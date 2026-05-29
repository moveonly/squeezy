use std::collections::HashMap;

use crate::languages::python::{normalize_python_import_module, split_python_alias};
use crate::languages::rust::*;
use crate::*;

pub(crate) fn extract_js_ts(file: FileRecord, source: &str, tree: &Tree) -> ParsedFile {
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

    visit_js_ts_node(root, &mut ctx, None, None);
    extract_js_ts_commonjs_facts(&mut ctx);
    dedup_js_ts_facts(&mut ctx);

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

pub(crate) fn visit_js_ts_node(
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

    let kind = node.kind();
    if matches!(kind, "import_statement" | "export_statement") {
        extract_js_ts_import_export(node, ctx, owner_symbol.clone());
    }

    if let Some(symbol) = js_ts_synthetic_binding_symbol(node, ctx, parent_symbol.as_ref()) {
        ctx.symbols.push(symbol);
    }

    if let Some(symbol) = js_ts_symbol_from_node(node, ctx, parent_symbol.as_ref()) {
        extract_js_ts_symbol_facts(node, &symbol, ctx);
        let next_parent = Some((symbol.id.clone(), symbol.kind));
        let next_owner = if symbol.body_span.is_some() {
            Some(symbol.id.clone())
        } else {
            owner_symbol.clone()
        };
        ctx.symbols.push(symbol);
        visit_js_ts_children(node, ctx, next_parent, next_owner);
        return;
    }

    if parent_symbol
        .as_ref()
        .map(|(_, parent_kind)| *parent_kind == SymbolKind::Class)
        .unwrap_or(false)
        && matches!(
            kind,
            "method_definition" | "public_field_definition" | "field_definition"
        )
    {
        visit_js_ts_children(node, ctx, None, owner_symbol);
        return;
    }

    if kind == "call_expression" || kind == "new_expression" {
        extract_js_ts_call(node, ctx, owner_symbol.clone());
    } else if let Some(reference_kind) = js_ts_reference_kind(kind) {
        extract_js_ts_reference(node, reference_kind, ctx, owner_symbol.clone());
    } else if is_js_ts_literal(kind) {
        extract_body_hit(node, BodyHitKind::Literal, ctx, owner_symbol.clone());
    }

    visit_js_ts_children(node, ctx, parent_symbol, owner_symbol);
}

pub(crate) fn visit_js_ts_children(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<(SymbolId, SymbolKind)>,
    owner_symbol: Option<SymbolId>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit_js_ts_node(child, ctx, parent_symbol.clone(), owner_symbol.clone());
    }
}

pub(crate) fn extract_js_ts_import_export(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let raw = node_text(node, ctx.source).unwrap_or_default().trim();
    let is_reexport = raw.starts_with("export");
    let imports = js_ts_imports_from_statement(raw);
    for (path, alias, is_glob) in imports {
        let leaf = last_path_segment(&path);
        let kind = if is_glob {
            ImportKind::Wildcard
        } else if leaf == "default" {
            ImportKind::Default
        } else if alias.is_some() && leaf == "*" {
            ImportKind::Namespace
        } else {
            ImportKind::Named
        };
        let imported_name = if is_glob || kind == ImportKind::Namespace {
            None
        } else if kind == ImportKind::Default {
            Some("default".to_string())
        } else {
            Some(leaf)
        };
        ctx.imports.push(ParsedImport {
            file_id: ctx.file.id.clone(),
            owner_id: owner_id.clone(),
            path,
            alias,
            is_glob,
            is_reexport,
            is_static: false,
            span: span_from_node(node),
            provenance: Provenance::new("tree-sitter-js-ts", "import/export declaration"),
            kind,
            imported_name,
            is_global: false,
        });
    }
}

pub(crate) fn js_ts_imports_from_statement(raw: &str) -> Vec<(String, Option<String>, bool)> {
    let mut imports = Vec::new();
    if raw.starts_with("import") {
        let Some(module) = js_ts_module_specifier(raw) else {
            return imports;
        };
        let before_from = raw
            .split_once(" from ")
            .map(|(before, _)| before)
            .unwrap_or(raw)
            .trim()
            .trim_start_matches("import")
            .trim()
            .trim_end_matches(';')
            .trim();
        if before_from.is_empty() || before_from.starts_with(['"', '\'']) {
            imports.push((module, None, false));
            return imports;
        }
        if let Some(namespace) = before_from.strip_prefix("* as ") {
            imports.push((
                format!("{module}.*"),
                Some(namespace.trim().to_string()),
                true,
            ));
            return imports;
        }
        let (default_part, named_part) = split_js_ts_default_and_named_import(before_from);
        if let Some(default_name) = default_part.filter(|name| is_js_ts_identifier(name)) {
            imports.push((
                format!("{module}.default"),
                Some(default_name.to_string()),
                false,
            ));
        }
        if let Some(named) = named_part {
            for (imported, alias) in js_ts_named_imports(named) {
                imports.push((format!("{module}.{imported}"), alias, false));
            }
        }
    } else if raw.starts_with("export") {
        let Some(module) = js_ts_module_specifier(raw) else {
            for (exported, alias) in js_ts_named_imports(raw) {
                imports.push((exported, alias, false));
            }
            return imports;
        };
        if raw.contains("* from ") {
            imports.push((format!("{module}.*"), None, true));
        }
        for (exported, alias) in js_ts_named_imports(raw) {
            imports.push((format!("{module}.{exported}"), alias, false));
        }
    }
    imports
}

pub(crate) fn split_js_ts_default_and_named_import(text: &str) -> (Option<&str>, Option<&str>) {
    if let Some(open) = text.find('{') {
        let default = text[..open].trim().trim_end_matches(',').trim();
        let named = Some(&text[open..]);
        (Some(default).filter(|value| !value.is_empty()), named)
    } else {
        (Some(text.trim()).filter(|value| !value.is_empty()), None)
    }
}

pub(crate) fn js_ts_named_imports(text: &str) -> Vec<(String, Option<String>)> {
    let inside = if let Some(open) = text.find('{') {
        text[open + 1..]
            .split_once('}')
            .map(|(inside, _)| inside)
            .unwrap_or_default()
    } else {
        text.trim()
    };
    split_top_level_commas(inside)
        .into_iter()
        .filter_map(|part| {
            let part = part.trim().trim_start_matches("type ").trim();
            if part.is_empty() {
                return None;
            }
            let (imported, alias) = part
                .split_once(" as ")
                .map(|(left, right)| (left.trim(), Some(right.trim().to_string())))
                .unwrap_or((part, None));
            if is_js_ts_identifier(imported) {
                Some((imported.to_string(), alias))
            } else {
                None
            }
        })
        .collect()
}

pub(crate) fn js_ts_module_specifier(raw: &str) -> Option<String> {
    let source = if let Some((_, after_from)) = raw.rsplit_once(" from ") {
        after_from
    } else if raw.trim_start().starts_with("import") {
        raw.trim_start().trim_start_matches("import").trim()
    } else {
        return None;
    };
    first_js_ts_string_literal(source)
}

pub(crate) fn first_js_ts_string_literal(text: &str) -> Option<String> {
    let mut chars = text.char_indices();
    while let Some((_, ch)) = chars.next() {
        let quote = match ch {
            '\'' | '"' => ch,
            _ => continue,
        };
        let mut escaped = false;
        let mut value = String::new();
        for (_, ch) in chars.by_ref() {
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

pub(crate) fn python_plain_imports(rest: &str) -> Vec<(String, Option<String>, bool)> {
    rest.split(',')
        .filter_map(|part| {
            let part = part.trim();
            if part.is_empty() {
                return None;
            }
            let (path, alias) = split_python_alias(part);
            Some((path.to_string(), alias.map(str::to_string), false))
        })
        .collect()
}

pub(crate) fn python_from_imports(
    rest: &str,
    relative_path: &str,
) -> Vec<(String, Option<String>, bool)> {
    let Some((module, names)) = rest.split_once(" import ") else {
        return Vec::new();
    };
    let module = normalize_python_import_module(module.trim(), relative_path);
    names
        .split(',')
        .filter_map(|part| {
            let part = part.trim().trim_matches(['(', ')']);
            if part.is_empty() {
                return None;
            }
            let (name, alias) = split_python_alias(part);
            let is_glob = name == "*";
            let path = if is_glob {
                format!("{module}.*")
            } else {
                format!("{module}.{name}")
            };
            Some((path, alias.map(str::to_string), is_glob))
        })
        .collect()
}

pub(crate) fn extract_js_ts_commonjs_facts(ctx: &mut ExtractContext<'_>) {
    for line in ctx.source.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        if let Some((left, right)) = line.split_once("require(") {
            let alias = js_ts_commonjs_alias(left);
            if let Some(module) = first_js_ts_string_literal(right)
                && let Some(alias) = alias
            {
                ctx.imports.push(ParsedImport {
                    file_id: ctx.file.id.clone(),
                    owner_id: None,
                    path: module,
                    alias: Some(alias),
                    is_glob: false,
                    is_reexport: false,
                    is_static: false,
                    span: SourceSpan::new(0, 0, SourcePoint::new(0, 0), SourcePoint::new(0, 0)),
                    provenance: Provenance::new("tree-sitter-js-ts", "commonjs require"),
                    kind: ImportKind::Namespace,
                    imported_name: None,
                    is_global: false,
                });
            }
        }
        if let Some((_, right)) = line.split_once("module.exports")
            && let Some((_, exported)) = right.split_once('=')
        {
            let exported = exported.trim().trim_end_matches(';').trim();
            if is_js_ts_identifier(exported) {
                let imported_name = Some(exported.to_string());
                ctx.imports.push(ParsedImport {
                    file_id: ctx.file.id.clone(),
                    owner_id: None,
                    path: exported.to_string(),
                    alias: None,
                    is_glob: false,
                    is_reexport: true,
                    is_static: false,
                    span: SourceSpan::new(0, 0, SourcePoint::new(0, 0), SourcePoint::new(0, 0)),
                    provenance: Provenance::new("tree-sitter-js-ts", "commonjs export"),
                    kind: ImportKind::Named,
                    imported_name,
                    is_global: false,
                });
            }
        }
    }
}

pub(crate) fn js_ts_commonjs_alias(left: &str) -> Option<String> {
    let left = left
        .trim()
        .trim_start_matches("const ")
        .trim_start_matches("let ")
        .trim_start_matches("var ")
        .trim();
    let alias = left.split('=').next()?.trim();
    if is_js_ts_identifier(alias) {
        Some(alias.to_string())
    } else {
        None
    }
}

pub(crate) fn dedup_js_ts_facts(ctx: &mut ExtractContext<'_>) {
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

pub(crate) fn python_simple_assignment_name(left: &str) -> Option<String> {
    let left = left.trim();
    if left.contains('.') || left.contains('[') || left.contains(',') {
        return None;
    }
    if is_python_identifier(left) {
        Some(left.to_string())
    } else {
        None
    }
}

pub(crate) fn python_assignment_target(right: &str) -> Option<String> {
    let expression = right
        .split_once('#')
        .map(|(before, _)| before)
        .unwrap_or(right)
        .trim();
    if expression.is_empty() {
        return None;
    }
    let callee = expression
        .find('(')
        .map(|index| expression[..index].trim())
        .unwrap_or(expression)
        .trim();
    let starts_with_literal = callee
        .chars()
        .next()
        .map(|ch| matches!(ch, '\'' | '"' | '[' | '{' | '(') || ch.is_ascii_digit())
        .unwrap_or(false);
    if callee.is_empty() || starts_with_literal {
        return None;
    }
    if callee
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '.')
        && callee.split('.').all(is_python_identifier)
    {
        Some(callee.to_string())
    } else {
        None
    }
}

pub(crate) fn python_string_list_values(text: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some((_, ch)) = chars.next() {
        let quote = match ch {
            '\'' | '"' => ch,
            _ => continue,
        };
        let mut escaped = false;
        let mut value = String::new();
        for (_, ch) in chars.by_ref() {
            if escaped {
                value.push(ch);
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == quote {
                if !value.is_empty() {
                    values.push(value);
                }
                break;
            } else {
                value.push(ch);
            }
        }
    }
    values
}

pub(crate) fn is_python_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

pub(crate) fn extract_js_ts_call(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let function_node = node.child_by_field_name("function").or_else(|| {
        if node.kind() == "new_expression" {
            node.child_by_field_name("constructor")
        } else {
            None
        }
    });
    let Some(function_node) = function_node.or_else(|| {
        let mut cursor = node.walk();
        node.named_children(&mut cursor).next()
    }) else {
        return;
    };
    let target_text = node_text(function_node, ctx.source)
        .unwrap_or_default()
        .trim()
        .trim_start_matches("new ")
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
                .find(|child| child.kind() == "arguments")
        })
        .map(named_child_count)
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
        provenance: Provenance::new("tree-sitter-js-ts", node.kind()),
        confidence: Confidence::Heuristic,
    });
    extract_body_hit(node, BodyHitKind::Call, ctx, owner_id);
}

pub(crate) fn extract_js_ts_reference(
    node: Node<'_>,
    kind: ReferenceKind,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    let text = node_text(node, ctx.source)
        .unwrap_or_default()
        .trim()
        .trim_matches(['"', '\''])
        .to_string();
    if text.is_empty() || js_ts_reference_is_declaration_name(node) {
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
        provenance: Provenance::new("tree-sitter-js-ts", format!("{} reference", node.kind())),
    });
    ctx.body_hits.push(BodyHit {
        file_id: ctx.file.id.clone(),
        owner_id,
        text,
        kind: body_kind,
        span: span_from_node(node),
    });
}

pub(crate) fn js_ts_reference_is_declaration_name(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    matches!(
        parent.kind(),
        "class_declaration"
            | "enum_declaration"
            | "function_declaration"
            | "function"
            | "function_expression"
            | "function_signature"
            | "generator_function_declaration"
            | "generator_function"
            | "interface_declaration"
            | "method_definition"
            | "method_signature"
            | "public_field_definition"
            | "field_definition"
            | "property_signature"
            | "type_alias_declaration"
            | "variable_declarator"
    ) && parent
        .child_by_field_name("name")
        .map(|name| name == node)
        .unwrap_or(false)
}
