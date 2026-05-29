use squeezy_core::FileId;

use super::{ImportGraph, schedule};

fn fid(id: &str) -> FileId {
    FileId::new(id)
}

#[test]
fn empty_graph_schedules_to_empty_result() {
    let graph = ImportGraph::new();
    let plan = schedule(&graph);
    assert!(plan.sccs.is_empty());
    assert!(plan.levels.is_empty());
}

#[test]
fn isolated_nodes_form_their_own_single_file_sccs_at_level_zero() {
    let mut graph = ImportGraph::new();
    graph.add_node(fid("a"));
    graph.add_node(fid("b"));
    let plan = schedule(&graph);
    assert_eq!(plan.sccs.len(), 2);
    assert_eq!(plan.levels.len(), 1);
    assert_eq!(plan.levels[0].len(), 2);
    for scc in &plan.sccs {
        assert_eq!(scc.files.len(), 1);
        assert!(!scc.is_cyclic);
    }
}

#[test]
fn linear_chain_a_imports_b_imports_c_groups_each_file_into_its_own_scc() {
    let mut graph = ImportGraph::new();
    graph.add_edge(fid("a"), fid("b"));
    graph.add_edge(fid("b"), fid("c"));
    let plan = schedule(&graph);
    assert_eq!(plan.sccs.len(), 3);
    // Leaf c (depends on nothing) lands in level 0; a (depends on b) in
    // the last level.
    let c_id = plan
        .sccs
        .iter()
        .find(|scc| scc.files.contains(&fid("c")))
        .unwrap()
        .id;
    let a_id = plan
        .sccs
        .iter()
        .find(|scc| scc.files.contains(&fid("a")))
        .unwrap()
        .id;
    assert!(plan.levels[0].contains(&c_id));
    assert!(plan.levels[plan.levels.len() - 1].contains(&a_id));
}

#[test]
fn import_cycle_is_one_multi_file_scc_flagged_cyclic() {
    let mut graph = ImportGraph::new();
    graph.add_edge(fid("a"), fid("b"));
    graph.add_edge(fid("b"), fid("a"));
    let plan = schedule(&graph);
    assert_eq!(plan.sccs.len(), 1);
    assert_eq!(plan.sccs[0].files, vec![fid("a"), fid("b")]);
    assert!(plan.sccs[0].is_cyclic);
}
