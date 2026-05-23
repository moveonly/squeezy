use std::{
    fs,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use squeezy_core::SymbolKind;
use squeezy_graph::{BodySearchQuery, GraphManager, HierarchyNode, RefreshConfig, SignatureQuery};

#[test]
fn semantic_pipeline_indexes_queries_and_refreshes_incrementally() {
    let root = temp_root("semantic-pipeline");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"case\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(root.join("README.md"), "# docs\n").unwrap();
    fs::write(
        root.join("src").join("extra.rs"),
        "pub fn extra() -> usize { 1 }\n",
    )
    .unwrap();
    fs::write(root.join("src").join("lib.rs"), initial_source()).unwrap();

    let mut manager = GraphManager::open_with_config(
        &root,
        RefreshConfig {
            debounce: Duration::from_millis(0),
            idle_refresh_interval: Duration::from_millis(0),
            per_tool_refresh_budget: Duration::from_secs(5),
        },
    )
    .unwrap();
    let graph = manager.graph();

    let hierarchy = flatten_hierarchy(&graph.hierarchy(None, 8));
    assert!(hierarchy.contains(&"File:src/lib.rs".to_string()));
    assert!(!hierarchy.contains(&"File:Cargo.toml".to_string()));
    assert!(!hierarchy.contains(&"File:README.md".to_string()));
    assert_eq!(
        graph
            .signature_search(&SignatureQuery {
                text: "pub fn run".to_string(),
                kind: Some(SymbolKind::Method),
                visibility: None,
                attribute: None,
            })
            .len(),
        1
    );
    assert!(
        graph
            .body_search(&BodySearchQuery {
                text: "helper".to_string(),
                owner_kind: Some(SymbolKind::Method),
                hit_kind: None,
            })
            .iter()
            .any(|hit| hit.hit.text == "helper")
    );
    assert!(!graph.reference_search("Runner").is_empty());

    let run = graph.find_symbol_by_name("run").pop().unwrap();
    let helper = graph.find_symbol_by_name("helper").pop().unwrap();
    assert!(graph.call_chain(&run.id, &helper.id, 4).is_some());

    fs::write(root.join("src").join("lib.rs"), updated_source()).unwrap();
    manager.record_changed_path(root.join("src").join("lib.rs"));
    let report = manager.refresh_before_query().unwrap();

    assert!(report.refreshed);
    assert_eq!(report.reparsed_files, 1);
    assert!(manager.graph().find_symbol_by_name("helper").is_empty());
    assert!(!manager.graph().find_symbol_by_name("helper_two").is_empty());
    assert!(!manager.graph().find_symbol_by_name("extra").is_empty());
}

fn initial_source() -> &'static str {
    r#"
pub struct Runner;

impl Runner {
    pub fn run(&self) -> usize {
        helper()
    }
}

fn helper() -> usize { 1 }
"#
}

fn updated_source() -> &'static str {
    r#"
pub struct Runner;

impl Runner {
    pub fn run(&self) -> usize {
        helper_two()
    }
}

fn helper_two() -> usize { 2 }
"#
}

fn flatten_hierarchy(nodes: &[HierarchyNode]) -> Vec<String> {
    fn visit(node: &HierarchyNode, out: &mut Vec<String>) {
        out.push(format!("{:?}:{}", node.kind, node.name));
        for child in &node.children {
            visit(child, out);
        }
    }

    let mut out = Vec::new();
    for node in nodes {
        visit(node, &mut out);
    }
    out
}

fn temp_root(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("squeezy-{name}-{nonce}"));
    fs::create_dir_all(&root).unwrap();
    root
}
