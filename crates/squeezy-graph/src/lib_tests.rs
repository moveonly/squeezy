use std::{
    fs,
    path::PathBuf,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use squeezy_core::{ContentHash, FileId, LanguageKind};
use squeezy_parse::{ParsedFile, RustParser};
use squeezy_workspace::{FileRecord, stable_content_hash};

use super::*;

#[test]
fn graph_answers_hierarchy_signature_body_reference_and_call_queries() {
    let source = r#"
pub struct Runner;

impl Runner {
    pub fn run(&self) {
        helper();
    }
}

fn helper() {}
"#;
    let mut parser = RustParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);

    assert!(graph.stats().symbols >= 4);
    assert!(
        graph
            .hierarchy(None, 4)
            .iter()
            .any(|node| node.name == "src/lib.rs")
    );
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
    let body_hits = graph.body_search(&BodySearchQuery {
        text: "helper".to_string(),
        owner_kind: Some(SymbolKind::Method),
        hit_kind: None,
    });
    assert!(body_hits.iter().any(|hit| hit.hit.text == "helper"));
    assert!(!graph.reference_search("Runner").is_empty());

    let run = graph.find_symbol_by_name("run").pop().unwrap();
    let helper = graph.find_symbol_by_name("helper").pop().unwrap();
    assert!(graph.call_chain(&run.id, &helper.id, 3).is_some());
}

#[test]
fn graph_manager_refresh_replaces_changed_file_only() {
    let root = temp_root("graph-manager-refresh");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src").join("lib.rs"), "fn one() { alpha(); }\n").unwrap();

    let mut manager = GraphManager::open_with_config(
        &root,
        RefreshConfig {
            debounce: Duration::from_millis(0),
            idle_refresh_interval: Duration::from_millis(0),
            per_tool_refresh_budget: Duration::from_secs(5),
        },
    )
    .unwrap();
    assert!(!manager.graph().find_symbol_by_name("one").is_empty());

    thread::sleep(Duration::from_millis(2));
    fs::write(root.join("src").join("lib.rs"), "fn two() { beta(); }\n").unwrap();

    let report = manager.refresh_before_query().unwrap();

    assert!(report.refreshed);
    assert_eq!(report.reparsed_files, 1);
    assert!(manager.graph().find_symbol_by_name("one").is_empty());
    assert!(!manager.graph().find_symbol_by_name("two").is_empty());
}

#[test]
fn graph_filters_unsupported_files_from_hierarchy() {
    let mut readme = record("README.md", "# docs\n");
    readme.language = LanguageKind::Unsupported;
    let graph = SemanticGraph::from_parsed(vec![ParsedFile::unsupported(readme, "markdown")]);

    assert_eq!(graph.stats().files, 1);
    assert_eq!(graph.stats().symbols, 0);
    assert!(graph.hierarchy(None, 4).is_empty());
}

#[test]
fn graph_supports_callers_callees_and_removal() {
    let source = r#"
pub fn alpha() -> usize {
    beta()
}

fn beta() -> usize {
    1
}
"#;
    let mut parser = RustParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let file_id = record.id.clone();
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let mut graph = SemanticGraph::from_parsed(vec![parsed]);

    let alpha = graph.find_symbol_by_name("alpha").pop().unwrap();
    let beta = graph.find_symbol_by_name("beta").pop().unwrap();
    assert_eq!(graph.callees(&alpha.id).len(), 1);
    assert_eq!(graph.callers(&beta.id).len(), 1);
    assert!(
        graph
            .signature_search(&SignatureQuery {
                text: "pub fn alpha".to_string(),
                kind: Some(SymbolKind::Function),
                visibility: None,
                attribute: None,
            })
            .iter()
            .any(|symbol| symbol.name == "alpha")
    );

    graph.remove_file(&file_id);

    assert!(graph.find_symbol_by_name("alpha").is_empty());
    assert!(graph.edges().is_empty());
}

fn record(relative_path: &str, source: &str) -> FileRecord {
    let root = temp_root("graph-record");
    let path = root.join(relative_path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, source).unwrap();
    FileRecord {
        id: FileId::new(relative_path),
        path,
        relative_path: relative_path.to_string(),
        hash: ContentHash::new(stable_content_hash(source.as_bytes())),
        size_bytes: source.len() as u64,
        modified_unix_millis: 0,
        language: LanguageKind::Rust,
        freshness: Freshness::Fresh,
    }
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
