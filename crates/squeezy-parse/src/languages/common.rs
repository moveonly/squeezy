use tree_sitter::Node;

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
