use crate::*;
use tree_sitter::Node;

pub(crate) fn record_missing_and_skip(node: Node<'_>, ctx: &mut ExtractContext<'_>) -> bool {
    if !node.is_missing() {
        return false;
    }
    record_missing_node_diagnostic(node, ctx);
    true
}

pub(crate) fn parsed_file_from_context(
    ctx: ExtractContext<'_>,
    package: Option<String>,
) -> ParsedFile {
    ParsedFile {
        file: ctx.file,
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

pub(crate) fn visit_named_children(node: Node<'_>, mut visit: impl FnMut(Node<'_>)) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit(child);
    }
}

pub(crate) fn visit_named_children_with_state<State: Clone>(
    node: Node<'_>,
    state: State,
    mut visit: impl FnMut(Node<'_>, State),
) {
    visit_named_children(node, |child| {
        visit(child, state.clone());
    });
}
