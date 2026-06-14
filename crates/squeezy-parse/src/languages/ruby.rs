//! Ruby extractor.
//!
//! Modelled on the Python extractor (closest dynamic-dispatch analogue).
//! See `docs/internal/lang-specs/ruby.md` for the contract; key gotchas:
//!
//! - `class`/`module` bodies host child `method`/`singleton_method` symbols.
//! - `attr_accessor`/`attr_reader`/`attr_writer`/`attr` are synthesized into
//!   `Partial` Method symbols (one reader and/or writer per symbol argument).
//! - `require`/`require_relative`/`load`/`autoload` calls become imports.
//! - `include`/`extend`/`prepend` calls become Type references on the host
//!   class plus BOTH a bare `mixin:<Mod>` attribute (queryable by
//!   `decl_search attribute=mixin:T` and the grep→graph augment) and a
//!   kind-tagged `mixin:<include|extend|prepend>:<Mod>` attribute, so the graph
//!   resolver can walk the ancestor chain and enumerate mixers.
//! - `heredoc_body` subtrees are skipped wholesale.
//! - `define_method`, `eval`/`instance_eval`/`class_eval`/`module_eval` are
//!   not mined for symbols (documented recall gap in the spec).
use crate::languages::rust::*;
use crate::*;

pub(crate) fn extract_ruby(file: FileRecord, source: &str, tree: &Tree) -> ParsedFile {
    let mut ctx = ExtractContext::new(file.clone(), source);
    let root = tree.root_node();
    record_parse_error_diagnostics(root, &mut ctx);

    visit_ruby_node(root, &mut ctx, None, None, None);
    dedup_ruby_facts(&mut ctx);

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

fn visit_ruby_node(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<(SymbolId, SymbolKind)>,
    owner_symbol: Option<SymbolId>,
    // Nearest enclosing class-like (Class / Module / `class << self`) host.
    // Tracked separately from `parent_symbol` so ivar/cvar assignments inside
    // a method body can attach to the class, not the method.
    host_class: Option<(SymbolId, SymbolKind)>,
) {
    if node.is_missing() {
        record_missing_node_diagnostic(node, ctx);
        return;
    }

    let kind = node.kind();

    // Heredoc bodies look like source code to tree-sitter but contain no
    // identifiers we should treat as references. Skip the subtree entirely
    // (spec §4(i)).
    if kind == "heredoc_body" {
        return;
    }

    if let Some(symbol) = ruby_symbol_from_node(node, ctx, parent_symbol.as_ref()) {
        extract_ruby_symbol_facts(node, &symbol, ctx);
        let next_parent = Some((symbol.id.clone(), symbol.kind));
        let next_owner = if symbol.body_span.is_some() {
            Some(symbol.id.clone())
        } else {
            owner_symbol.clone()
        };
        let next_host = if matches!(symbol.kind, SymbolKind::Class | SymbolKind::Module) {
            Some((symbol.id.clone(), symbol.kind))
        } else {
            host_class.clone()
        };
        ctx.symbols.push(symbol);
        visit_ruby_children(node, ctx, next_parent, next_owner, next_host);
        return;
    }

    // `singleton_class` (`class << self`) is a scope but not a symbol.
    // Methods inside it should still be picked up; just descend.
    if kind == "singleton_class" {
        visit_ruby_children(node, ctx, parent_symbol, owner_symbol, host_class);
        return;
    }

    if kind == "call" {
        if extract_ruby_import(node, ctx, owner_symbol.clone()) {
            // require/require_relative/load/autoload were recorded; still
            // descend so e.g. interpolated args produce body hits, but skip
            // the `ParsedCall` for the require call itself.
        } else if extract_ruby_mixin_or_attr(node, ctx, host_class.as_ref(), owner_symbol.clone()) {
            // include/extend/prepend produced a Type reference + class
            // attribute, or attr_* synthesized methods. The call itself is
            // not emitted as a `ParsedCall`.
        } else {
            extract_ruby_call(node, ctx, owner_symbol.clone());
        }
    } else if kind == "assignment" {
        extract_ruby_assignment_symbol(
            node,
            ctx,
            parent_symbol.as_ref(),
            host_class.as_ref(),
            owner_symbol.clone(),
        );
    } else if kind == "identifier" {
        extract_ruby_reference(node, ReferenceKind::Identifier, ctx, owner_symbol.clone());
    } else if kind == "global_variable" {
        // `$var` read (the LHS of an assignment is handled by the assignment
        // arm and suppressed here via `ruby_node_is_declared_name`). Emit an
        // Identifier reference + body hit so `$config` reads are searchable.
        extract_ruby_reference(node, ReferenceKind::Identifier, ctx, owner_symbol.clone());
    } else if kind == "constant" {
        let ref_kind = if ruby_constant_in_type_position(node) {
            ReferenceKind::Type
        } else {
            ReferenceKind::Identifier
        };
        extract_ruby_reference(node, ref_kind, ctx, owner_symbol.clone());
    } else if kind == "scope_resolution" {
        extract_ruby_reference(node, ReferenceKind::Path, ctx, owner_symbol.clone());
    } else if is_ruby_literal(kind) {
        extract_body_hit(node, BodyHitKind::Literal, ctx, owner_symbol.clone());
    }

    visit_ruby_children(node, ctx, parent_symbol, owner_symbol, host_class);
}

fn visit_ruby_children(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<(SymbolId, SymbolKind)>,
    owner_symbol: Option<SymbolId>,
    host_class: Option<(SymbolId, SymbolKind)>,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit_ruby_node(
            child,
            ctx,
            parent_symbol.clone(),
            owner_symbol.clone(),
            host_class.clone(),
        );
    }
}

fn ruby_symbol_from_node(
    node: Node<'_>,
    ctx: &ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
) -> Option<ParsedSymbol> {
    let kind = node.kind();
    let (symbol_kind, is_singleton) = match kind {
        "class" => (SymbolKind::Class, false),
        "module" => (SymbolKind::Module, false),
        "method" => {
            let parent_kind = parent_symbol.map(|(_, k)| *k);
            let symbol_kind = if matches!(
                parent_kind,
                Some(SymbolKind::Class) | Some(SymbolKind::Module)
            ) {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            };
            (symbol_kind, false)
        }
        "singleton_method" => (SymbolKind::Method, true),
        _ => return None,
    };

    let name = ruby_symbol_name(node, ctx.source)?;
    let span = span_from_node(node);
    let body = node.child_by_field_name("body");
    let body_span = body.map(span_from_node);
    let signature_span = signature_span_from_nodes(node, body);
    let signature = signature_text(node, body, ctx.source);
    let parent_id = parent_symbol.map(|(id, _)| id.clone());
    let id = symbol_id(&ctx.file, parent_id.as_ref(), symbol_kind, &name, span);
    let mut attributes = Vec::new();
    if symbol_kind == SymbolKind::Class
        && let Some(base) = ruby_superclass_name(node, ctx.source)
    {
        attributes.push(format!("base:{base}"));
    }
    if is_singleton {
        attributes.push("ruby:singleton".to_string());
        if let Some(receiver) = node
            .child_by_field_name("object")
            .and_then(|child| node_text(child, ctx.source).ok())
        {
            attributes.push(format!("ruby:singleton-receiver:{}", receiver.trim()));
        }
    }
    let arity = if matches!(symbol_kind, SymbolKind::Function | SymbolKind::Method) {
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
        kind: symbol_kind,
        language_identity: None,
        span,
        body_span,
        signature_span,
        signature,
        visibility: None,
        docs: Vec::new(),
        attributes,
        provenance: Provenance::new("tree-sitter-ruby", format!("{} declaration", kind)),
        confidence: Confidence::ExactSyntax,
        freshness: Freshness::Fresh,
        arity,
    })
}

fn ruby_symbol_name(node: Node<'_>, source: &str) -> Option<String> {
    let name_node = node.child_by_field_name("name")?;
    let raw = node_text(name_node, source).ok()?.trim();
    if raw.is_empty() {
        return None;
    }
    // For `class Foo::Bar` the name node is a `scope_resolution`; keep the
    // leaf component so the symbol name stays short (the qualified form is
    // already preserved in `signature`). For method declarations we keep the
    // raw text verbatim because Ruby method names legitimately end in `!`,
    // `?`, or `=`, and the generic `last_path_segment` helper strips
    // trailing `!`.
    if matches!(node.kind(), "method" | "singleton_method") {
        Some(raw.to_string())
    } else {
        Some(last_path_segment(raw))
    }
}

fn ruby_superclass_name(node: Node<'_>, source: &str) -> Option<String> {
    let superclass = node.child_by_field_name("superclass")?;
    // `superclass` wraps `<` followed by the constant/expression. The first
    // named child is the typed expression we want; fall back to text strip.
    let mut cursor = superclass.walk();
    let named = superclass
        .named_children(&mut cursor)
        .find_map(|child| node_text(child, source).ok())
        .map(str::trim);
    if let Some(text) = named.filter(|t| !t.is_empty()) {
        // Take the rightmost segment for the attribute name; `base:` is
        // matched by leaf name during graph resolution.
        return Some(last_path_segment(text));
    }
    let raw = node_text(superclass, source).ok()?.trim();
    let stripped = raw.trim_start_matches('<').trim();
    if stripped.is_empty() {
        None
    } else {
        Some(last_path_segment(stripped))
    }
}

fn extract_ruby_symbol_facts(node: Node<'_>, symbol: &ParsedSymbol, ctx: &mut ExtractContext<'_>) {
    let _ = node;
    if symbol.kind == SymbolKind::Class {
        for attr in &symbol.attributes {
            if let Some(base) = attr.strip_prefix("base:") {
                ctx.references.push(ParsedReference {
                    file_id: ctx.file.id.clone(),
                    owner_id: Some(symbol.id.clone()),
                    text: base.to_string(),
                    kind: ReferenceKind::Type,
                    span: symbol.span,
                    provenance: Provenance::new("tree-sitter-ruby", "class superclass"),
                });
            }
        }
    }
}

fn extract_ruby_assignment_symbol(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
    host_class: Option<&(SymbolId, SymbolKind)>,
    owner_id: Option<SymbolId>,
) {
    let Some(left) = node.child_by_field_name("left") else {
        return;
    };
    let Some(left_text) = node_text(left, ctx.source).ok().map(str::trim) else {
        return;
    };
    if left_text.is_empty() {
        return;
    }
    let span = span_from_node(node);
    let raw = node_text(node, ctx.source).unwrap_or_default().trim();

    match left.kind() {
        "constant" => {
            // Top-of-class/module/program constants become Const symbols.
            let host_is_class_like = matches!(
                parent_symbol.map(|(_, k)| *k),
                Some(SymbolKind::Class) | Some(SymbolKind::Module) | None
            );
            if !host_is_class_like {
                return;
            }
            let right_kind = node
                .child_by_field_name("right")
                .map(|n| n.kind().to_string())
                .unwrap_or_default();
            // Const + rhs literal/constant -> ExactSyntax. Const + rhs call
            // is `Partial` because we cannot tell whether the call returns a
            // constant value at runtime (spec §5).
            let symbol_confidence = match right_kind.as_str() {
                "call" => Confidence::Partial,
                _ => Confidence::ExactSyntax,
            };
            let const_parent = parent_symbol.map(|(id, _)| id.clone());
            let id = symbol_id(
                &ctx.file,
                const_parent.as_ref(),
                SymbolKind::Const,
                left_text,
                span,
            );
            ctx.symbols.push(ParsedSymbol {
                id,
                file_id: ctx.file.id.clone(),
                parent_id: const_parent,
                name: left_text.to_string(),
                kind: SymbolKind::Const,
                language_identity: None,
                span,
                body_span: None,
                signature_span: None,
                signature: raw.to_string(),
                visibility: None,
                docs: Vec::new(),
                attributes: Vec::new(),
                provenance: Provenance::new("tree-sitter-ruby", "constant assignment"),
                confidence: symbol_confidence,
                freshness: Freshness::Fresh,
                arity: None,
            });
        }
        "global_variable" => {
            // `$config = ...` is a process-global. Model it as a file-scoped
            // `Static` (the closest analogue to a module-level constant binding
            // that is mutable and not namespaced) so `$config` is queryable as a
            // declaration. Globals have no lexical owner, so the symbol is
            // parented at the file regardless of the enclosing class/method.
            let id = symbol_id(&ctx.file, None, SymbolKind::Static, left_text, span);
            ctx.symbols.push(ParsedSymbol {
                id,
                file_id: ctx.file.id.clone(),
                parent_id: None,
                name: left_text.to_string(),
                kind: SymbolKind::Static,
                language_identity: None,
                span,
                body_span: None,
                signature_span: None,
                signature: raw.to_string(),
                visibility: None,
                docs: Vec::new(),
                attributes: vec!["ruby:global".to_string()],
                provenance: Provenance::new("tree-sitter-ruby", "global variable assignment"),
                confidence: Confidence::Heuristic,
                freshness: Freshness::Fresh,
                arity: None,
            });
        }
        "instance_variable" | "class_variable" => {
            // Ivar/cvar Fields attach to the nearest enclosing class/module,
            // not the immediate parent symbol (which is usually the method).
            let Some((host_id, _)) = host_class else {
                return;
            };
            let attribute = if left.kind() == "instance_variable" {
                "ruby:ivar"
            } else {
                "ruby:cvar"
            };
            let id = symbol_id(&ctx.file, Some(host_id), SymbolKind::Field, left_text, span);
            ctx.symbols.push(ParsedSymbol {
                id,
                file_id: ctx.file.id.clone(),
                parent_id: Some(host_id.clone()),
                name: left_text.to_string(),
                kind: SymbolKind::Field,
                language_identity: None,
                span,
                body_span: None,
                signature_span: None,
                signature: raw.to_string(),
                visibility: None,
                docs: Vec::new(),
                attributes: vec![attribute.to_string()],
                provenance: Provenance::new("tree-sitter-ruby", "ivar/cvar assignment"),
                confidence: Confidence::Heuristic,
                freshness: Freshness::Fresh,
                arity: None,
            });
        }
        _ => {}
    }

    // Always emit the lhs as an Identifier reference for body searches.
    if matches!(left.kind(), "identifier") {
        ctx.references.push(ParsedReference {
            file_id: ctx.file.id.clone(),
            owner_id: owner_id.clone(),
            text: left_text.to_string(),
            kind: ReferenceKind::Identifier,
            span: span_from_node(left),
            provenance: Provenance::new("tree-sitter-ruby", "assignment lhs reference"),
        });
    }
}

/// Try to interpret a `call` node as a `require`/`require_relative`/`load`/
/// `autoload` import. Returns `true` if an import was recorded.
fn extract_ruby_import(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) -> bool {
    // Require/load calls have no receiver.
    if node.child_by_field_name("receiver").is_some() {
        return false;
    }
    let Some(method_node) = node.child_by_field_name("method") else {
        return false;
    };
    let method_name = node_text(method_node, ctx.source)
        .unwrap_or_default()
        .trim();
    let kind_label = match method_name {
        "require" | "require_relative" | "load" | "autoload" => method_name,
        _ => return false,
    };
    let Some(args) = node.child_by_field_name("arguments") else {
        return false;
    };
    let mut cursor = args.walk();
    let mut args_iter = args.named_children(&mut cursor);

    // `autoload(:Name, "path")`: first arg is the alias symbol, second is the path.
    if kind_label == "autoload" {
        let Some(first) = args_iter.next() else {
            return false;
        };
        let Some(second) = args_iter.next() else {
            return false;
        };
        let Some(alias) = ruby_symbol_arg_value(first, ctx.source) else {
            return false;
        };
        let Some(raw_path) = ruby_string_literal_value(second, ctx.source) else {
            return false;
        };
        ctx.imports.push(ParsedImport {
            file_id: ctx.file.id.clone(),
            owner_id,
            path: raw_path.clone(),
            alias: Some(alias.clone()),
            is_glob: false,
            is_reexport: false,
            is_static: false,
            span: span_from_node(node),
            provenance: Provenance::new("tree-sitter-ruby", "autoload"),
            kind: ImportKind::Named,
            imported_name: Some(alias),
            is_global: false,
        });
        return true;
    }

    let Some(first) = args_iter.next() else {
        return false;
    };
    let Some(raw_path) = ruby_string_literal_value(first, ctx.source) else {
        return false;
    };
    let resolved_path = if kind_label == "require_relative" {
        ruby_resolve_relative_path(&ctx.file.relative_path, &raw_path)
    } else {
        raw_path.clone()
    };
    // Leaf of the required path (e.g. `user` for `app/models/user.rb`). Kept so
    // the resolver can fall back to leaf-name matching even though the binding
    // is a whole-file glob.
    let imported_name = ruby_imported_name_from_path(&resolved_path);
    let provenance_label = if kind_label == "load" {
        "ruby:load"
    } else {
        kind_label
    };
    let span = span_from_node(node);
    ctx.imports.push(ParsedImport {
        file_id: ctx.file.id.clone(),
        owner_id: owner_id.clone(),
        path: resolved_path,
        alias: None,
        // `require`/`require_relative`/`load` evaluate the whole target file,
        // exposing every top-level definition to the requiring file — the same
        // whole-file 'expose everything' semantics as C's `#include` (which is
        // `Wildcard` + `is_glob`). Model them as globs so the resolver routes
        // single-candidate require edges through the glob branch (it switches on
        // `is_glob`, not `ImportKind`) and `Wildcard` keeps the binding shape
        // honest. `autoload` stays `Named` — it binds exactly one constant.
        is_glob: true,
        is_reexport: false,
        is_static: false,
        span,
        provenance: Provenance::new("tree-sitter-ruby", provenance_label),
        kind: ImportKind::Wildcard,
        imported_name,
        is_global: false,
    });
    // Synthesize a Function symbol for the import directive so
    // `signature_search("require_relative \"user\"")` surfaces it. Without
    // this the import lives only in `imports` (not in `symbols`) and the
    // signature index never indexes its source text. The signature uses
    // the raw call source to keep the trigram lookup tight.
    let raw = node_text(node, ctx.source).unwrap_or_default().trim();
    if !raw.is_empty() {
        let directive_name = kind_label.to_string();
        let id = symbol_id(
            &ctx.file,
            owner_id.as_ref(),
            SymbolKind::Function,
            &directive_name,
            span,
        );
        ctx.symbols.push(ParsedSymbol {
            id,
            file_id: ctx.file.id.clone(),
            parent_id: owner_id,
            name: directive_name,
            kind: SymbolKind::Function,
            language_identity: None,
            span,
            body_span: None,
            signature_span: None,
            signature: raw.to_string(),
            visibility: None,
            docs: Vec::new(),
            attributes: vec!["ruby:import-directive".to_string()],
            provenance: Provenance::new("tree-sitter-ruby", "import directive synthesis"),
            confidence: Confidence::Heuristic,
            freshness: Freshness::Fresh,
            arity: None,
        });
    }
    true
}

/// `attr_accessor`/`attr_reader`/`attr_writer`/`attr` synthesis,
/// `include`/`extend`/`prepend` mixin handling. Returns `true` if the call
/// was consumed (so the caller knows not to emit a `ParsedCall`).
fn extract_ruby_mixin_or_attr(
    node: Node<'_>,
    ctx: &mut ExtractContext<'_>,
    parent_symbol: Option<&(SymbolId, SymbolKind)>,
    owner_id: Option<SymbolId>,
) -> bool {
    if node.child_by_field_name("receiver").is_some() {
        return false;
    }
    let Some(method_node) = node.child_by_field_name("method") else {
        return false;
    };
    let method_name = node_text(method_node, ctx.source)
        .unwrap_or_default()
        .trim();
    let Some((parent_id, parent_kind)) = parent_symbol else {
        return false;
    };
    let host_is_class_like = matches!(*parent_kind, SymbolKind::Class | SymbolKind::Module);
    if !host_is_class_like {
        return false;
    }
    let attr_mode = match method_name {
        "attr_accessor" => Some((true, true)),
        "attr_reader" | "attr" => Some((true, false)),
        "attr_writer" => Some((false, true)),
        _ => None,
    };
    if let Some((emit_reader, emit_writer)) = attr_mode {
        let Some(args) = node.child_by_field_name("arguments") else {
            return false;
        };
        let mut cursor = args.walk();
        let span = span_from_node(node);
        let raw = node_text(node, ctx.source).unwrap_or_default().trim();
        for child in args.named_children(&mut cursor) {
            let Some(name) = ruby_symbol_arg_value(child, ctx.source) else {
                continue;
            };
            if emit_reader {
                push_synthetic_attr(ctx, parent_id, &name, /*writer=*/ false, span, raw);
            }
            if emit_writer {
                push_synthetic_attr(
                    ctx,
                    parent_id,
                    &format!("{name}="),
                    /*writer=*/ true,
                    span,
                    raw,
                );
            }
        }
        return true;
    }

    let mixin_kind = match method_name {
        "include" => Some("include"),
        "extend" => Some("extend"),
        "prepend" => Some("prepend"),
        _ => None,
    };
    if let Some(mixin) = mixin_kind {
        let Some(args) = node.child_by_field_name("arguments") else {
            return false;
        };
        let mut cursor = args.walk();
        for child in args.named_children(&mut cursor) {
            let Some(text) = node_text(child, ctx.source).ok().map(str::trim) else {
                continue;
            };
            if text.is_empty() {
                continue;
            }
            let leaf = last_path_segment(text);
            // Attach the mixin attribute to the host class symbol so the graph
            // resolver can find it via ancestor lookup. Emit BOTH the bare
            // `mixin:<Type>` form — so `decl_search attribute=mixin:T` and the
            // grep→graph augment (which query `base:T|mixin:T|iface:T`) match it,
            // exactly as Dart's `with` mixers do — AND the kind-tagged
            // `mixin:<include|extend|prepend>:<Type>` form that preserves which
            // directive introduced the mixin.
            if let Some(host) = ctx.symbols.iter_mut().find(|s| s.id == *parent_id) {
                host.attributes.push(format!("mixin:{leaf}"));
                host.attributes.push(format!("mixin:{mixin}:{leaf}"));
                // For a namespace-qualified mixin (`Sidekiq::Component`), also
                // record the fully-qualified form so `Sidekiq::Component` and
                // `Other::Component` stay distinguishable — the bare
                // `mixin:<leaf>` (kept for the grep→graph augment) collides on
                // the shared leaf, and the kind-tagged form does too.
                if text.contains("::") {
                    host.attributes.push(format!("mixin:{text}"));
                }
            }
            ctx.references.push(ParsedReference {
                file_id: ctx.file.id.clone(),
                owner_id: Some(parent_id.clone()),
                text: leaf,
                kind: ReferenceKind::Type,
                span: span_from_node(child),
                provenance: Provenance::new("tree-sitter-ruby", format!("{mixin} mixin")),
            });
        }
        let _ = owner_id;
        return true;
    }

    false
}

fn push_synthetic_attr(
    ctx: &mut ExtractContext<'_>,
    parent_id: &SymbolId,
    name: &str,
    writer: bool,
    span: SourceSpan,
    raw: &str,
) {
    let mut attributes = vec![
        "ruby:attr".to_string(),
        "ruby:synthesized".to_string(),
        if writer {
            "ruby:attr-writer".to_string()
        } else {
            "ruby:attr-reader".to_string()
        },
    ];
    attributes.sort();
    attributes.dedup();
    let id = symbol_id(&ctx.file, Some(parent_id), SymbolKind::Method, name, span);
    ctx.symbols.push(ParsedSymbol {
        id,
        file_id: ctx.file.id.clone(),
        parent_id: Some(parent_id.clone()),
        name: name.to_string(),
        kind: SymbolKind::Method,
        language_identity: None,
        span,
        body_span: None,
        signature_span: None,
        signature: raw.to_string(),
        visibility: None,
        docs: Vec::new(),
        attributes,
        provenance: Provenance::new("tree-sitter-ruby", "attr_* synthesis"),
        confidence: Confidence::Partial,
        freshness: Freshness::Fresh,
        arity: Some(if writer { 1 } else { 0 }),
    });
}

/// True for Ruby metaprogramming sinks whose callee is resolved at runtime
/// (`obj.send(:foo)`, `define_method(:bar) { ... }`, `instance_eval(src)`).
/// Tagging these `ParsedCallKind::Macro` lets the graph resolver emit an
/// `InvokesMacro` classification for dynamic dispatch instead of pretending it
/// is an ordinary call.
fn ruby_is_macro_dispatch(name: &str) -> bool {
    matches!(
        name,
        "send"
            | "public_send"
            | "__send__"
            | "define_method"
            | "method"
            | "eval"
            | "instance_eval"
            | "class_eval"
            | "module_eval"
            | "instance_exec"
            | "class_exec"
            | "module_exec"
    )
}

fn extract_ruby_call(node: Node<'_>, ctx: &mut ExtractContext<'_>, owner_id: Option<SymbolId>) {
    let Some(method_node) = node.child_by_field_name("method") else {
        return;
    };
    let name = node_text(method_node, ctx.source)
        .unwrap_or_default()
        .trim()
        .to_string();
    if name.is_empty() {
        return;
    }
    let receiver_text = node
        .child_by_field_name("receiver")
        .and_then(|child| node_text(child, ctx.source).ok())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    let target_text = match &receiver_text {
        Some(r) => format!("{r}.{name}"),
        None => name.clone(),
    };
    let arity = node
        .child_by_field_name("arguments")
        .map(named_child_count)
        .unwrap_or_default();
    // Known dynamic-dispatch sinks (metaprogramming) are tagged `Macro` so the
    // resolver's `InvokesMacro` classification fires for them rather than
    // treating them as ordinary method/direct calls. The actual callee is not
    // statically known (it's a runtime symbol/string), so resolution stays
    // `MacroOpaque` — but the edge kind alone lets callers find dynamic dispatch
    // sites. Receiver shape is irrelevant: both `obj.send(:foo)` and a bare
    // `define_method(:foo)` are dynamic dispatch.
    let call_kind = if ruby_is_macro_dispatch(&name) {
        ParsedCallKind::Macro
    } else if receiver_text.is_some() {
        ParsedCallKind::Method
    } else {
        ParsedCallKind::Direct
    };

    // Emit a reference for the method-name leaf so `reference_search`
    // and `references_to_symbol` both surface every call site. Two
    // shapes:
    //
    // - Explicit-receiver dispatch (`obj.method`) becomes a Field
    //   reference whose `text` is the dotted target so the Ruby graph
    //   resolver can bind it to the receiver class via
    //   `ruby_property_reference_matches`. The reference index also
    //   keys the leaf so a bare `reference_search("method")` still
    //   matches.
    // - Bare-name dispatch (`fire_event(:x)`) becomes an Identifier
    //   reference whose `text` is the method name. The identifier
    //   visitor itself skips this token because
    //   `ruby_node_is_declared_name` suppresses the `method` field of
    //   a `call`; without this emission `reference_search` and
    //   `references_to_symbol` would miss every mixin-resolved call
    //   site (a Ruby idiom: `include Sidekiq::Component; fire_event`).
    //   The owner_id pins the reference to the enclosing method, so
    //   per-class attribution still flows through `method.parent_id`
    //   when sibling classes both `include` the same module.
    if receiver_text.is_some() {
        ctx.references.push(ParsedReference {
            file_id: ctx.file.id.clone(),
            owner_id: owner_id.clone(),
            text: target_text.clone(),
            kind: ReferenceKind::Field,
            span: span_from_node(node),
            provenance: Provenance::new("tree-sitter-ruby", "dotted call reference"),
        });
    } else {
        ctx.references.push(ParsedReference {
            file_id: ctx.file.id.clone(),
            owner_id: owner_id.clone(),
            text: name.clone(),
            kind: ReferenceKind::Identifier,
            span: span_from_node(method_node),
            provenance: Provenance::new("tree-sitter-ruby", "bare call reference"),
        });
    }

    ctx.calls.push(ParsedCall {
        file_id: ctx.file.id.clone(),
        caller_id: owner_id.clone(),
        name,
        target_text: target_text.clone(),
        receiver: receiver_text,
        arity,
        kind: call_kind,
        span: span_from_node(node),
        provenance: Provenance::new("tree-sitter-ruby", "call"),
        confidence: Confidence::Heuristic,
    });
    extract_body_hit(node, BodyHitKind::Call, ctx, owner_id);
}

fn extract_ruby_reference(
    node: Node<'_>,
    kind: ReferenceKind,
    ctx: &mut ExtractContext<'_>,
    owner_id: Option<SymbolId>,
) {
    // Skip identifiers that are themselves part of the symbol they declare
    // (the `name` field of class/module/method/singleton_method, the
    // `superclass` field of class). Without this guard every method
    // declaration would emit a self-reference.
    if ruby_node_is_declared_name(node) {
        return;
    }
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
        provenance: Provenance::new("tree-sitter-ruby", format!("{} reference", node.kind())),
    });
    ctx.body_hits.push(BodyHit {
        file_id: ctx.file.id.clone(),
        owner_id,
        text,
        kind: body_kind,
        span: span_from_node(node),
    });
}

/// True if `node` is the `name` field of a class/module/method/singleton_method
/// declaration, the `superclass` field expression, or the `method` field of
/// the enclosing `call` (the dispatch target name). Those nodes are handled
/// by the symbol/import/call paths and would double-count as references.
fn ruby_node_is_declared_name(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    if let Some(name) = parent.child_by_field_name("name")
        && name.id() == node.id()
    {
        return matches!(
            parent.kind(),
            "class" | "module" | "method" | "singleton_method" | "scope_resolution"
        );
    }
    if parent.kind() == "call"
        && let Some(method) = parent.child_by_field_name("method")
        && method.id() == node.id()
    {
        return true;
    }
    if parent.kind() == "superclass" {
        return true;
    }
    if parent.kind() == "singleton_method"
        && let Some(obj) = parent.child_by_field_name("object")
        && obj.id() == node.id()
    {
        return true;
    }
    false
}

/// True when this constant is a "Type position": superclass, mixin argument,
/// or appearing in `scope_resolution`. Defaults to Identifier otherwise.
fn ruby_constant_in_type_position(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    matches!(parent.kind(), "superclass" | "scope_resolution")
    // Note: include/extend/prepend argument constants are also Type
    // references, but the mixin extractor emits those references directly
    // with the correct kind, so the default-`Identifier` path here is
    // intentional and avoids double-counting.
}

fn ruby_symbol_arg_value(node: Node<'_>, source: &str) -> Option<String> {
    match node.kind() {
        "simple_symbol" => {
            let raw = node_text(node, source).ok()?.trim();
            Some(raw.trim_start_matches(':').to_string())
        }
        "hash_key_symbol" => {
            let raw = node_text(node, source).ok()?.trim();
            Some(raw.trim_end_matches(':').to_string())
        }
        "string" => ruby_string_literal_value(node, source),
        _ => None,
    }
}

fn ruby_string_literal_value(node: Node<'_>, source: &str) -> Option<String> {
    if node.kind() != "string" {
        return None;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "string_content" {
            return node_text(child, source).ok().map(|s| s.to_string());
        }
    }
    // Empty string -> "".
    Some(String::new())
}

/// Resolve `require_relative "../lib/foo"` against the file's directory.
fn ruby_resolve_relative_path(relative_path: &str, target: &str) -> String {
    let dir_parts: Vec<&str> = relative_path
        .rsplit_once('/')
        .map(|(dir, _)| dir.split('/').filter(|p| !p.is_empty()).collect())
        .unwrap_or_default();
    let mut segments: Vec<String> = dir_parts.into_iter().map(String::from).collect();
    for raw in target.split('/') {
        match raw {
            "" | "." => continue,
            ".." => {
                segments.pop();
            }
            other => segments.push(other.to_string()),
        }
    }
    let mut joined = segments.join("/");
    if !joined.ends_with(".rb") {
        joined.push_str(".rb");
    }
    joined
}

fn ruby_imported_name_from_path(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches(".rb");
    let leaf = trimmed.rsplit('/').next().unwrap_or(trimmed);
    if leaf.is_empty() {
        None
    } else {
        Some(leaf.to_string())
    }
}

/// One Field per `(host class, ivar/cvar name)` (spec §3); same for
/// duplicate include/extend/prepend mixin attributes and one Static per
/// global-variable name (`$config = ...` may be re-assigned many times).
/// Calls and references are not deduped — every call site is its own data
/// point.
fn dedup_ruby_facts(ctx: &mut ExtractContext<'_>) {
    let mut seen_fields = std::collections::HashSet::new();
    let mut seen_globals = std::collections::HashSet::new();
    ctx.symbols.retain(|symbol| {
        if symbol.kind == SymbolKind::Static
            && symbol.attributes.iter().any(|attr| attr == "ruby:global")
        {
            return seen_globals.insert(symbol.name.clone());
        }
        if symbol.kind != SymbolKind::Field {
            return true;
        }
        let key = format!(
            "{}|{}",
            symbol
                .parent_id
                .as_ref()
                .map(|id| id.0.as_str())
                .unwrap_or(""),
            symbol.name
        );
        seen_fields.insert(key)
    });
    for symbol in &mut ctx.symbols {
        if matches!(symbol.kind, SymbolKind::Class | SymbolKind::Module) {
            symbol.attributes.sort();
            symbol.attributes.dedup();
        }
    }
}

fn is_ruby_literal(kind: &str) -> bool {
    matches!(
        kind,
        "string"
            | "integer"
            | "float"
            | "rational"
            | "complex"
            | "simple_symbol"
            | "hash_key_symbol"
            | "bare_symbol"
            | "delimited_symbol"
            | "regex"
            | "character"
            | "chained_string"
            | "true"
            | "false"
            | "nil"
    )
}

#[cfg(test)]
#[path = "ruby_tests.rs"]
mod tests;
