use std::{
    fs,
    path::PathBuf,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use squeezy_core::{ContentHash, FileId, LanguageKind};
use squeezy_parse::{LanguageParser, ParsedFile, ReferenceKind, RustParser};
use squeezy_workspace::{CrawlOptions, FileRecord, stable_content_hash};

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
    let mut parser = LanguageParser::new().unwrap();
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
fn cargo_compiler_facts_attach_diagnostics_and_track_staleness() {
    let source = "pub fn bad() -> i32 {\n    \"nope\"\n}\n";
    let mut parser = LanguageParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let root = record
        .path
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let mut graph = SemanticGraph::from_parsed(vec![parsed]);
    let provenance = CargoFactProvenance {
        command: "cargo metadata --format-version=1 --no-deps; cargo check --message-format=json"
            .to_string(),
        cargo_version: Some("cargo 1.93.1".to_string()),
        rustc_version: Some("rustc 1.93.1".to_string()),
        captured_unix_millis: 123,
    };
    let metadata = serde_json::json!({
        "packages": [{
            "id": "path+file:///case#case@0.1.0",
            "name": "case",
            "manifest_path": "Cargo.toml",
            "targets": [{
                "name": "case",
                "kind": ["lib"],
                "src_path": "src/lib.rs"
            }],
            "features": {
                "default": [],
                "serde": []
            }
        }],
        "workspace_members": ["path+file:///case#case@0.1.0"],
        "workspace_root": ".",
        "target_directory": "target"
    })
    .to_string();
    let diagnostic_start = source.find("\"nope\"").unwrap() as u32;
    let diagnostic_end = diagnostic_start + "\"nope\"".len() as u32;
    let diagnostics = serde_json::json!({
        "reason": "compiler-message",
        "package_id": "path+file:///case#case@0.1.0",
        "target": {"name": "case"},
        "message": {
            "message": "mismatched types",
            "level": "error",
            "code": {"code": "E0308"},
            "spans": [{
                "file_name": "src/lib.rs",
                "byte_start": diagnostic_start,
                "byte_end": diagnostic_end,
                "line_start": 2,
                "line_end": 2,
                "column_start": 5,
                "column_end": 11,
                "is_primary": true,
                "label": "expected i32"
            }]
        }
    })
    .to_string();

    let report = graph
        .refresh_cargo_facts_from_json(&metadata, Some(&diagnostics), provenance, &root)
        .unwrap();

    assert_eq!(report.summary.workspaces, 1);
    assert_eq!(report.summary.packages, 1);
    assert_eq!(report.summary.targets, 1);
    assert_eq!(report.summary.features, 2);
    assert_eq!(report.summary.diagnostics, 1);
    assert_eq!(
        report.summary.freshness.as_ref().unwrap().status,
        Freshness::Fresh
    );

    let bad = graph.find_symbol_by_name("bad").pop().unwrap();
    let hits = graph.cargo_diagnostics_for_symbol(&bad);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].diagnostic.code.as_deref(), Some("E0308"));
    assert_eq!(hits[0].freshness.status, Freshness::Fresh);
    // rustc reports 1-indexed line/column; the graph stores 0-indexed
    // coordinates everywhere else, so diagnostic spans must match that
    // convention. The owning `bad` symbol's start line is 0; the diagnostic's
    // start line should be 1 (rustc 2 - 1) and start column 4 (rustc 5 - 1).
    let diagnostic_span = hits[0].diagnostic.span.unwrap();
    assert_eq!(diagnostic_span.start.line, 1);
    assert_eq!(diagnostic_span.start.column, 4);
    assert_eq!(diagnostic_span.end.line, 1);
    assert_eq!(diagnostic_span.end.column, 10);
    assert!(diagnostic_span.start.line >= bad.span.start.line);

    graph
        .files
        .get_mut(&FileId::new("src/lib.rs"))
        .unwrap()
        .hash = ContentHash::new("changed");
    let stale_hits = graph.cargo_diagnostics_for_symbol(&bad);
    assert_eq!(stale_hits[0].freshness.status, Freshness::Stale);
    assert!(!stale_hits[0].freshness.stale_reasons.is_empty());
}

fn cargo_provenance(command: &str) -> CargoFactProvenance {
    CargoFactProvenance {
        command: command.to_string(),
        cargo_version: Some("cargo 1.93.1".to_string()),
        rustc_version: Some("rustc 1.93.1".to_string()),
        captured_unix_millis: 123,
    }
}

fn cargo_metadata_fixture() -> String {
    serde_json::json!({
        "packages": [{
            "id": "path+file:///case#case@0.1.0",
            "name": "case",
            "manifest_path": "Cargo.toml",
            "targets": [{
                "name": "case",
                "kind": ["lib"],
                "src_path": "src/lib.rs"
            }],
            "features": {}
        }],
        "workspace_members": ["path+file:///case#case@0.1.0"],
        "workspace_root": ".",
        "target_directory": "target"
    })
    .to_string()
}

#[test]
fn cargo_compiler_facts_filter_non_compiler_messages_and_sort_diagnostics() {
    let source_a = "pub fn a() {}\n";
    let source_b = "pub fn b() {}\n";
    let mut parser = LanguageParser::new().unwrap();
    let record_a = record("src/a.rs", source_a);
    let record_b = record("src/b.rs", source_b);
    let parsed_a = parser
        .parse_source(&record_a, source_a.to_string())
        .unwrap();
    let parsed_b = parser
        .parse_source(&record_b, source_b.to_string())
        .unwrap();
    let root = record_a
        .path
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let mut graph = SemanticGraph::from_parsed(vec![parsed_a, parsed_b]);

    let metadata = cargo_metadata_fixture();
    // Stream contains two compiler-message events plus several events that
    // should be silently filtered (compiler-artifact, build-script-executed,
    // build-finished, and a malformed line).
    let stream = [
        r#"{"reason":"compiler-artifact","package_id":"path+file:///case#case@0.1.0","target":{"name":"case"}}"#.to_string(),
        r#"{"reason":"build-script-executed","package_id":"path+file:///case#case@0.1.0"}"#.to_string(),
        serde_json::json!({
            "reason": "compiler-message",
            "package_id": "path+file:///case#case@0.1.0",
            "target": {"name": "case"},
            "message": {
                "message": "unused variable",
                "level": "warning",
                "code": {"code": "unused_variables"},
                "spans": [{
                    "file_name": "src/b.rs",
                    "byte_start": 4,
                    "byte_end": 5,
                    "line_start": 1,
                    "line_end": 1,
                    "column_start": 5,
                    "column_end": 6,
                    "is_primary": true,
                    "label": null
                }]
            }
        })
        .to_string(),
        "not a json line".to_string(),
        serde_json::json!({
            "reason": "compiler-message",
            "package_id": "path+file:///case#case@0.1.0",
            "target": {"name": "case"},
            "message": {
                "message": "lint",
                "level": "warning",
                "code": null,
                "spans": [{
                    "file_name": "src/a.rs",
                    "byte_start": 0,
                    "byte_end": 1,
                    "line_start": 1,
                    "line_end": 1,
                    "column_start": 1,
                    "column_end": 2,
                    "is_primary": true,
                    "label": null
                }]
            }
        })
        .to_string(),
        r#"{"reason":"build-finished","success":true}"#.to_string(),
    ]
    .join("\n");

    graph
        .refresh_cargo_facts_from_json(
            &metadata,
            Some(&stream),
            cargo_provenance("cargo metadata; cargo check"),
            &root,
        )
        .unwrap();

    let diagnostics = graph
        .cargo_facts()
        .map(|facts| facts.diagnostics.clone())
        .unwrap_or_default();
    assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");
    assert_eq!(
        diagnostics[0]
            .file_id
            .as_ref()
            .map(|id| id.0.as_str())
            .unwrap_or(""),
        "src/a.rs"
    );
    assert_eq!(
        diagnostics[1]
            .file_id
            .as_ref()
            .map(|id| id.0.as_str())
            .unwrap_or(""),
        "src/b.rs"
    );
}

#[test]
fn cargo_compiler_facts_emit_diagnostic_without_span() {
    let source = "pub fn a() {}\n";
    let mut parser = LanguageParser::new().unwrap();
    let record = record("src/a.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let root = record
        .path
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let mut graph = SemanticGraph::from_parsed(vec![parsed]);

    let metadata = cargo_metadata_fixture();
    let stream = serde_json::json!({
        "reason": "compiler-message",
        "package_id": "path+file:///case#case@0.1.0",
        "target": {"name": "case"},
        "message": {
            "message": "command-level note",
            "level": "note",
            "code": null,
            "spans": []
        }
    })
    .to_string();

    graph
        .refresh_cargo_facts_from_json(
            &metadata,
            Some(&stream),
            cargo_provenance("cargo check"),
            &root,
        )
        .unwrap();

    let diagnostics = graph
        .cargo_facts()
        .map(|facts| facts.diagnostics.clone())
        .unwrap_or_default();
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0].file_id.is_none());
    assert!(diagnostics[0].span.is_none());
}

#[test]
fn cargo_compiler_facts_emit_one_diagnostic_per_primary_span() {
    let source = "pub fn a() {}\npub fn b() {}\n";
    let mut parser = LanguageParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let root = record
        .path
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let mut graph = SemanticGraph::from_parsed(vec![parsed]);

    let metadata = cargo_metadata_fixture();
    let stream = serde_json::json!({
        "reason": "compiler-message",
        "package_id": "path+file:///case#case@0.1.0",
        "target": {"name": "case"},
        "message": {
            "message": "two-site warning",
            "level": "warning",
            "code": {"code": "W1234"},
            "spans": [
                {
                    "file_name": "src/lib.rs",
                    "byte_start": 0,
                    "byte_end": 5,
                    "line_start": 1,
                    "line_end": 1,
                    "column_start": 1,
                    "column_end": 6,
                    "is_primary": true,
                    "label": "first"
                },
                {
                    "file_name": "src/lib.rs",
                    "byte_start": 14,
                    "byte_end": 19,
                    "line_start": 2,
                    "line_end": 2,
                    "column_start": 1,
                    "column_end": 6,
                    "is_primary": true,
                    "label": "second"
                }
            ]
        }
    })
    .to_string();

    graph
        .refresh_cargo_facts_from_json(
            &metadata,
            Some(&stream),
            cargo_provenance("cargo check"),
            &root,
        )
        .unwrap();

    let diagnostics = graph
        .cargo_facts()
        .map(|facts| facts.diagnostics.clone())
        .unwrap_or_default();
    assert_eq!(diagnostics.len(), 2);
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| diagnostic.code.as_deref() == Some("W1234")
                && diagnostic.message == "two-site warning")
    );
    let labels = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.label.clone().unwrap_or_default())
        .collect::<Vec<_>>();
    assert!(labels.contains(&"first".to_string()));
    assert!(labels.contains(&"second".to_string()));
}

#[test]
fn cargo_compiler_facts_normalize_absolute_paths() {
    let source = "pub fn a() {}\n";
    let mut parser = LanguageParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let root = record
        .path
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let inside_path = root.join("src/lib.rs");
    let inside_path_str = inside_path.to_string_lossy().into_owned();
    let mut graph = SemanticGraph::from_parsed(vec![parsed]);

    let metadata = cargo_metadata_fixture();
    let stream = [
        serde_json::json!({
            "reason": "compiler-message",
            "package_id": "path+file:///case#case@0.1.0",
            "target": {"name": "case"},
            "message": {
                "message": "inside warning",
                "level": "warning",
                "code": null,
                "spans": [{
                    "file_name": inside_path_str,
                    "byte_start": 0,
                    "byte_end": 1,
                    "line_start": 1,
                    "line_end": 1,
                    "column_start": 1,
                    "column_end": 2,
                    "is_primary": true,
                    "label": null
                }]
            }
        })
        .to_string(),
        serde_json::json!({
            "reason": "compiler-message",
            "package_id": "path+file:///case#case@0.1.0",
            "target": {"name": "case"},
            "message": {
                "message": "outside warning",
                "level": "warning",
                "code": null,
                "spans": [{
                    "file_name": "/tmp/squeezy-cargo-outside.rs",
                    "byte_start": 0,
                    "byte_end": 1,
                    "line_start": 1,
                    "line_end": 1,
                    "column_start": 1,
                    "column_end": 2,
                    "is_primary": true,
                    "label": null
                }]
            }
        })
        .to_string(),
    ]
    .join("\n");

    graph
        .refresh_cargo_facts_from_json(
            &metadata,
            Some(&stream),
            cargo_provenance("cargo check"),
            &root,
        )
        .unwrap();

    let diagnostics = graph
        .cargo_facts()
        .map(|facts| facts.diagnostics.clone())
        .unwrap_or_default();
    assert_eq!(diagnostics.len(), 2);
    let inside = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.message == "inside warning")
        .unwrap();
    assert_eq!(
        inside.file_id.as_ref().map(|id| id.0.as_str()),
        Some("src/lib.rs")
    );
    let outside = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.message == "outside warning")
        .unwrap();
    assert!(outside.file_id.is_none());
}

#[test]
fn cargo_compiler_facts_flip_stale_on_cargo_toml_change() {
    let source = "pub fn a() -> i32 {\n    \"nope\"\n}\n";
    let mut parser = LanguageParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let root = record
        .path
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let mut graph = SemanticGraph::from_parsed(vec![parsed]);

    let manifest = "[package]\nname='case'\nversion='0.1.0'\nedition='2024'\n";
    fs::write(root.join("Cargo.toml"), manifest).unwrap();
    let manifest_record = FileRecord {
        id: FileId::new("Cargo.toml"),
        path: root.join("Cargo.toml"),
        relative_path: "Cargo.toml".to_string(),
        hash: ContentHash::new(stable_content_hash(manifest.as_bytes())),
        size_bytes: manifest.len() as u64,
        modified_unix_millis: 0,
        language: LanguageKind::Unsupported,
        freshness: Freshness::Fresh,
    };
    graph
        .files
        .insert(manifest_record.id.clone(), manifest_record);

    let diagnostic_start = source.find("\"nope\"").unwrap() as u32;
    let diagnostic_end = diagnostic_start + "\"nope\"".len() as u32;
    let metadata = cargo_metadata_fixture();
    let stream = serde_json::json!({
        "reason": "compiler-message",
        "package_id": "path+file:///case#case@0.1.0",
        "target": {"name": "case"},
        "message": {
            "message": "manifest-driven warning",
            "level": "warning",
            "code": null,
            "spans": [{
                "file_name": "src/lib.rs",
                "byte_start": diagnostic_start,
                "byte_end": diagnostic_end,
                "line_start": 2,
                "line_end": 2,
                "column_start": 5,
                "column_end": 11,
                "is_primary": true,
                "label": null
            }]
        }
    })
    .to_string();

    graph
        .refresh_cargo_facts_from_json(
            &metadata,
            Some(&stream),
            cargo_provenance("cargo metadata; cargo check"),
            &root,
        )
        .unwrap();

    let symbol_a = graph.find_symbol_by_name("a").pop().unwrap();
    let fresh_hits = graph.cargo_diagnostics_for_symbol(&symbol_a);
    assert_eq!(fresh_hits.len(), 1, "{fresh_hits:?}");
    assert_eq!(fresh_hits[0].freshness.status, Freshness::Fresh);

    graph
        .files
        .get_mut(&FileId::new("Cargo.toml"))
        .unwrap()
        .hash = ContentHash::new("changed");
    let hits = graph.cargo_diagnostics_for_symbol(&symbol_a);
    assert_eq!(hits[0].freshness.status, Freshness::Stale);
    assert!(!hits[0].freshness.stale_reasons.is_empty());
}

#[test]
fn cargo_compiler_facts_file_symbol_returns_all_diagnostics_in_file() {
    let source = "pub fn a() {}\npub fn b() {}\n";
    let mut parser = LanguageParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let root = record
        .path
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let mut graph = SemanticGraph::from_parsed(vec![parsed]);

    let metadata = cargo_metadata_fixture();
    let stream = [
        serde_json::json!({
            "reason": "compiler-message",
            "package_id": "path+file:///case#case@0.1.0",
            "target": {"name": "case"},
            "message": {
                "message": "first",
                "level": "warning",
                "code": null,
                "spans": [{
                    "file_name": "src/lib.rs",
                    "byte_start": 0,
                    "byte_end": 1,
                    "line_start": 1,
                    "line_end": 1,
                    "column_start": 1,
                    "column_end": 2,
                    "is_primary": true,
                    "label": null
                }]
            }
        })
        .to_string(),
        serde_json::json!({
            "reason": "compiler-message",
            "package_id": "path+file:///case#case@0.1.0",
            "target": {"name": "case"},
            "message": {
                "message": "second",
                "level": "warning",
                "code": null,
                "spans": [{
                    "file_name": "src/lib.rs",
                    "byte_start": 100,
                    "byte_end": 101,
                    "line_start": 5,
                    "line_end": 5,
                    "column_start": 1,
                    "column_end": 2,
                    "is_primary": true,
                    "label": null
                }]
            }
        })
        .to_string(),
    ]
    .join("\n");

    graph
        .refresh_cargo_facts_from_json(
            &metadata,
            Some(&stream),
            cargo_provenance("cargo check"),
            &root,
        )
        .unwrap();

    let file_symbol = graph
        .symbols
        .values()
        .find(|symbol| {
            symbol.kind == SymbolKind::File && symbol.file_id == FileId::new("src/lib.rs")
        })
        .cloned()
        .expect("file symbol present");
    let hits = graph.cargo_diagnostics_for_symbol(&file_symbol);
    assert_eq!(hits.len(), 2);
    let messages = hits
        .iter()
        .map(|hit| hit.diagnostic.message.clone())
        .collect::<Vec<_>>();
    assert!(messages.contains(&"first".to_string()));
    assert!(messages.contains(&"second".to_string()));
}

#[test]
fn cargo_fact_input_path_matches_basename_and_nested_paths() {
    assert!(is_cargo_fact_input_path("Cargo.toml"));
    assert!(is_cargo_fact_input_path("crates/foo/Cargo.toml"));
    assert!(is_cargo_fact_input_path("Cargo.lock"));
    assert!(is_cargo_fact_input_path("rust-toolchain"));
    assert!(is_cargo_fact_input_path("rust-toolchain.toml"));
    assert!(is_cargo_fact_input_path("build.rs"));
    assert!(is_cargo_fact_input_path("crates/foo/build.rs"));
    assert!(is_cargo_fact_input_path(".cargo/config"));
    assert!(is_cargo_fact_input_path(".cargo/config.toml"));
    assert!(is_cargo_fact_input_path("crates/foo/.cargo/config.toml"));

    assert!(!is_cargo_fact_input_path("foo/NotCargo.toml"));
    assert!(!is_cargo_fact_input_path("docs/build.rst"));
    assert!(!is_cargo_fact_input_path("src/lib.rs"));
}

#[test]
fn persistent_graph_warm_start_skips_unchanged_parsing() {
    let root = temp_root("persistent-warm-start");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='demo'\nversion='0.1.0'\nedition='2024'\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn hello() {}\n").unwrap();

    let first = GraphManager::open_persistent_with_crawl_options(
        &root,
        RefreshConfig::default(),
        CrawlOptions::default(),
        None,
    )
    .unwrap();
    assert!(first.build_report().parsed_files > 0);
    assert_eq!(first.build_report().persisted_files_loaded, 0);
    drop(first);

    let second = GraphManager::open_persistent_with_crawl_options(
        &root,
        RefreshConfig::default(),
        CrawlOptions::default(),
        None,
    )
    .unwrap();
    assert_eq!(second.build_report().parsed_files, 0);
    assert!(second.build_report().persisted_files_loaded > 0);
    assert!(
        second
            .graph()
            .find_symbol_by_name("hello")
            .iter()
            .any(|symbol| symbol.name == "hello")
    );
}

#[test]
fn persistent_graph_refresh_updates_changed_partitions() {
    let root = temp_root("persistent-refresh");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='demo'\nversion='0.1.0'\nedition='2024'\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    let source = root.join("src/lib.rs");
    fs::write(&source, "pub fn old_name() {}\n").unwrap();

    let mut manager = GraphManager::open_persistent_with_crawl_options(
        &root,
        RefreshConfig {
            debounce: Duration::from_millis(0),
            idle_refresh_interval: Duration::from_secs(60),
            per_tool_refresh_budget: Duration::from_secs(5),
        },
        CrawlOptions::default(),
        None,
    )
    .unwrap();
    fs::write(&source, "pub fn new_name() {}\n").unwrap();
    manager.record_changed_path(&source);
    let refresh = manager.refresh_now().unwrap();
    assert_eq!(refresh.reparsed_files, 1);
    assert!(manager.graph().find_symbol_by_name("new_name").len() == 1);
    drop(manager);

    let reopened = GraphManager::open_persistent_with_crawl_options(
        &root,
        RefreshConfig::default(),
        CrawlOptions::default(),
        None,
    )
    .unwrap();
    assert_eq!(reopened.build_report().parsed_files, 0);
    assert!(reopened.graph().find_symbol_by_name("new_name").len() == 1);
    assert!(reopened.graph().find_symbol_by_name("old_name").is_empty());
}

#[test]
fn graph_answers_python_navigation_queries() {
    let source = r#"
class Greeter:
    def greet(self, name):
        return name

def make():
    greeter = Greeter()
    return greeter.greet("Ada")
"#;
    let mut parser = LanguageParser::new().unwrap();
    let record = python_record("app.py", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);

    assert!(
        graph
            .signature_search(&SignatureQuery {
                text: "class Greeter".to_string(),
                kind: Some(SymbolKind::Class),
                visibility: None,
                attribute: None,
            })
            .iter()
            .any(|symbol| symbol.name == "Greeter")
    );
    let make = graph.find_symbol_by_name("make").pop().unwrap();
    let greeter = graph.find_symbol_by_name("Greeter").pop().unwrap();
    assert!(graph.call_chain(&make.id, &greeter.id, 2).is_some());
    assert!(!graph.reference_search("Greeter").is_empty());
}

#[test]
fn graph_answers_java_navigation_queries() {
    let mut parser = LanguageParser::new().unwrap();
    let greeter = java_record(
        "src/main/java/com/example/services/Greeter.java",
        r#"
package com.example.services;

public class Greeter {
    public String greet(String name) {
        return name;
    }
}
"#,
    );
    let runner = java_record(
        "src/main/java/com/example/app/Runner.java",
        r#"
package com.example.app;

import com.example.services.Greeter;

public class Runner implements Runnable {
    private final Greeter greeter;

    public Runner(Greeter greeter) {
        this.greeter = greeter;
    }

    public void run() {
        greeter.greet("Ada");
    }
}
"#,
    );
    let parsed = vec![
        parser
            .parse_source(&greeter, fs::read_to_string(&greeter.path).unwrap())
            .unwrap(),
        parser
            .parse_source(&runner, fs::read_to_string(&runner.path).unwrap())
            .unwrap(),
    ];
    let graph = SemanticGraph::from_parsed(parsed);

    assert!(
        graph
            .signature_search(&SignatureQuery {
                text: "class Runner".to_string(),
                kind: Some(SymbolKind::Class),
                visibility: Some("public".to_string()),
                attribute: None,
            })
            .iter()
            .any(|symbol| symbol.name == "Runner")
    );
    let run = graph.find_symbol_by_name("run").pop().unwrap();
    let greet = graph.find_symbol_by_name("greet").pop().unwrap();
    assert!(graph.call_chain(&run.id, &greet.id, 3).is_some());
    assert!(
        graph
            .references_to_symbol(&graph.find_symbol_by_name("Greeter").pop().unwrap().id)
            .iter()
            .any(|hit| hit.reference.text == "Greeter")
    );
}

#[test]
fn graph_answers_go_navigation_queries() {
    let mut parser = LanguageParser::new().unwrap();
    let util = go_record(
        "util/format.go",
        r#"
package util

func Format(name string) string {
    return name
}
"#,
    );
    let app = go_record(
        "greeter/runner.go",
        r#"
package greeter

import util "example.com/acme/app/util"

type Runner struct {
    Name string
}

func NewRunner(name string) Runner {
    return Runner{Name: name}
}

func (r Runner) Greet(name string) string {
    helper()
    return util.Format(name)
}

func helper() {}
"#,
    );
    let parsed = vec![
        parser
            .parse_source(&util, fs::read_to_string(&util.path).unwrap())
            .unwrap(),
        parser
            .parse_source(&app, fs::read_to_string(&app.path).unwrap())
            .unwrap(),
    ];
    let graph = SemanticGraph::from_parsed(parsed);

    assert!(
        graph
            .find_symbol_by_name("Runner")
            .iter()
            .any(|symbol| symbol.kind == SymbolKind::Struct)
    );
    let greet = graph.find_symbol_by_name("Greet").pop().unwrap();
    let helper = graph.find_symbol_by_name("helper").pop().unwrap();
    let format = graph.find_symbol_by_name("Format").pop().unwrap();
    assert!(graph.call_chain(&greet.id, &helper.id, 2).is_some());
    assert!(graph.call_chain(&greet.id, &format.id, 2).is_some());
    assert!(!graph.reference_search("Format").is_empty());
}

#[test]
fn graph_answers_js_ts_navigation_queries() {
    let mut parser = LanguageParser::new().unwrap();
    let helpers = ts_record(
        "src/helpers.ts",
        r#"
export function buildRunner(name: string) {
    return name;
}
"#,
    );
    let app = tsx_record(
        "src/app.tsx",
        r#"
import { buildRunner } from "./helpers";

interface RunnerProps {
    name: string;
}

class Runner {
    start(props: RunnerProps) {
        return buildRunner(props.name);
    }
}

export const RunnerView = (props: RunnerProps) => <Runner />;
"#,
    );
    let parsed = vec![
        parser
            .parse_source(&helpers, fs::read_to_string(&helpers.path).unwrap())
            .unwrap(),
        parser
            .parse_source(&app, fs::read_to_string(&app.path).unwrap())
            .unwrap(),
    ];
    let graph = SemanticGraph::from_parsed(parsed);

    let start = graph.find_symbol_by_name("start").pop().unwrap();
    let build = graph.find_symbol_by_name("buildRunner").pop().unwrap();
    let runner_props = graph.find_symbol_by_name("RunnerProps").pop().unwrap();
    let runner_view = graph.find_symbol_by_name("RunnerView").pop().unwrap();

    assert!(graph.call_chain(&start.id, &build.id, 2).is_some());
    assert!(
        graph
            .references_to_symbol(&runner_props.id)
            .iter()
            .any(|hit| {
                hit.reference.text == "RunnerProps" && hit.reference.kind == ReferenceKind::Type
            })
    );
    assert!(
        runner_view
            .attributes
            .contains(&"jsx:component".to_string())
    );
}

#[test]
fn graph_resolves_js_ts_alias_package_and_index_imports() {
    let mut parser = LanguageParser::new().unwrap();
    let tsconfig = unsupported_record(
        "tsconfig.json",
        r#"{"compilerOptions":{"baseUrl":".","paths":{"@app/*":["src/*"]}}}"#,
    );
    let package = unsupported_record(
        "package.json",
        r#"{"name":"semantic-cases","exports":{".":"./src/index.ts","./helpers":"./src/helpers.ts"}}"#,
    );
    let helpers = ts_record(
        "src/helpers.ts",
        r#"
export function buildRunner(name: string) {
    return name;
}
"#,
    );
    let index = ts_record(
        "src/index.ts",
        r#"
export function packageEntry() {
    return "entry";
}
"#,
    );
    let app = ts_record(
        "src/app.ts",
        r#"
import { buildRunner } from "@app/helpers";
import { packageEntry } from "semantic-cases";

export function start() {
    return buildRunner(packageEntry());
}
"#,
    );
    let parsed = vec![tsconfig, package, helpers, index, app]
        .into_iter()
        .map(|record| parser.parse_record(&record).unwrap())
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);

    let start = graph.find_symbol_by_name("start").pop().unwrap();
    let build = graph.find_symbol_by_name("buildRunner").pop().unwrap();
    let package_entry = graph.find_symbol_by_name("packageEntry").pop().unwrap();

    assert!(
        graph
            .callees(&start.id)
            .iter()
            .any(|hit| hit.edge.to.as_ref() == Some(&build.id)
                && hit.edge.confidence == Confidence::ImportResolved)
    );
    assert!(
        graph
            .callees(&start.id)
            .iter()
            .any(|hit| hit.edge.to.as_ref() == Some(&package_entry.id)
                && hit.edge.confidence == Confidence::ImportResolved)
    );
}

#[test]
fn graph_keeps_unmapped_js_ts_bare_imports_external() {
    let mut parser = LanguageParser::new().unwrap();
    let local = ts_record(
        "src/react.ts",
        r#"
export function useMemo() {
    return "local";
}
"#,
    );
    let app = ts_record(
        "src/app.ts",
        r#"
import { useMemo } from "react";

export function start() {
    return useMemo();
}
"#,
    );
    let parsed = vec![local, app]
        .into_iter()
        .map(|record| parser.parse_record(&record).unwrap())
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);
    let start = graph.find_symbol_by_name("start").pop().unwrap();
    let use_memo = graph.find_symbol_by_name("useMemo").pop().unwrap();

    assert!(
        !graph
            .callees(&start.id)
            .iter()
            .any(|hit| hit.edge.to.as_ref() == Some(&use_memo.id))
    );
}

#[test]
fn graph_manager_refresh_rebuilds_js_ts_alias_resolution_for_dependents() {
    let root = temp_root("graph-manager-js-ts-alias-refresh");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("lib")).unwrap();
    fs::write(
        root.join("tsconfig.json"),
        r#"{"compilerOptions":{"baseUrl":".","paths":{"@app/*":["src/*"]}}}"#,
    )
    .unwrap();
    fs::write(
        root.join("src").join("helpers.ts"),
        "export function buildRunner() { return 'src'; }\n",
    )
    .unwrap();
    fs::write(
        root.join("lib").join("helpers.ts"),
        "export function buildRunner() { return 'lib'; }\n",
    )
    .unwrap();
    fs::write(
        root.join("src").join("app.ts"),
        "import { buildRunner } from '@app/helpers';\nexport function start() { return buildRunner(); }\n",
    )
    .unwrap();

    let mut manager = GraphManager::open_with_config(
        &root,
        RefreshConfig {
            debounce: Duration::from_millis(0),
            idle_refresh_interval: Duration::from_millis(0),
            per_tool_refresh_budget: Duration::from_secs(5),
        },
    )
    .unwrap();
    let start = manager.graph().find_symbol_by_name("start").pop().unwrap();
    let src_build = manager
        .graph()
        .find_symbol_by_name("buildRunner")
        .into_iter()
        .find(|symbol| symbol.file_id.0 == "src/helpers.ts")
        .unwrap();
    let lib_build = manager
        .graph()
        .find_symbol_by_name("buildRunner")
        .into_iter()
        .find(|symbol| symbol.file_id.0 == "lib/helpers.ts")
        .unwrap();
    assert!(
        manager
            .graph()
            .callees(&start.id)
            .iter()
            .any(|hit| hit.edge.to.as_ref() == Some(&src_build.id))
    );

    thread::sleep(Duration::from_millis(2));
    fs::write(
        root.join("tsconfig.json"),
        r#"{"compilerOptions":{"baseUrl":".","paths":{"@app/*":["lib/*"]}}}"#,
    )
    .unwrap();
    manager.record_changed_path(root.join("tsconfig.json"));
    let report = manager.refresh_before_query().unwrap();

    assert!(report.refreshed);
    assert_eq!(report.changed_paths_from_events, 1);
    assert!(
        manager
            .graph()
            .callees(&start.id)
            .iter()
            .any(|hit| hit.edge.to.as_ref() == Some(&lib_build.id)
                && hit.edge.confidence == Confidence::ImportResolved)
    );
}

#[test]
fn graph_uses_python_navigation_heuristics() {
    let mut parser = LanguageParser::new().unwrap();
    let greeter = python_record(
        "services/greeter.py",
        r#"
class Greeter:
    @property
    def label(self):
        return "greeter"

    def greet(self, name):
        return name

class Other:
    def greet(self, name):
        return "other"
"#,
    );
    let helpers = python_record(
        "helpers.py",
        r#"
def build():
    return "Ada"
"#,
    );
    let app = python_record(
        "app.py",
        r#"
from services.greeter import Greeter as GreeterAlias
from services.greeter import Other
import helpers

router = APIRouter()

class Runner(GreeterAlias):
    """Routes greeting requests."""

    @router.get("/hello/{name}")
    def run(self, name: GreeterAlias) -> GreeterAlias:
        return self.label

def make():
    greeter = GreeterAlias()
    helpers.build()
    return greeter.greet("Ada")

def reassign():
    greeter = GreeterAlias()
    greeter = Other()
    return greeter.greet("Ada")
"#,
    );
    let parsed = vec![
        parser
            .parse_source(&greeter, fs::read_to_string(&greeter.path).unwrap())
            .unwrap(),
        parser
            .parse_source(&helpers, fs::read_to_string(&helpers.path).unwrap())
            .unwrap(),
        parser
            .parse_source(&app, fs::read_to_string(&app.path).unwrap())
            .unwrap(),
    ];
    let graph = SemanticGraph::from_parsed(parsed);

    let make = graph.find_symbol_by_name("make").pop().unwrap();
    let reassign = graph.find_symbol_by_name("reassign").pop().unwrap();
    let run = graph.find_symbol_by_name("run").pop().unwrap();
    let greeter_class = graph.find_symbol_by_name("Greeter").pop().unwrap();
    let other_class = graph.find_symbol_by_name("Other").pop().unwrap();
    let greet = graph
        .find_symbol_by_name("greet")
        .into_iter()
        .find(|symbol| symbol.parent_id.as_ref() == Some(&greeter_class.id))
        .unwrap();
    let other_greet = graph
        .find_symbol_by_name("greet")
        .into_iter()
        .find(|symbol| symbol.parent_id.as_ref() == Some(&other_class.id))
        .unwrap();
    let label = graph.find_symbol_by_name("label").pop().unwrap();
    let build = graph.find_symbol_by_name("build").pop().unwrap();

    assert!(
        run.attributes
            .contains(&"route:GET /hello/{name}".to_string())
            && run.attributes.contains(&"framework:web-route".to_string())
    );
    assert!(
        graph
            .references_to_symbol(&label.id)
            .iter()
            .any(|hit| hit.reference.text == "self.label")
    );
    assert!(graph.call_chain(&make.id, &greet.id, 2).is_some());
    assert!(graph.call_chain(&reassign.id, &other_greet.id, 2).is_some());
    assert!(graph.call_chain(&make.id, &build.id, 2).is_some());
    assert!(graph.call_chain(&make.id, &greeter_class.id, 2).is_some());
    assert!(
        graph
            .references_to_symbol(&greeter_class.id)
            .iter()
            .any(|hit| hit.reference.kind == ReferenceKind::Type)
    );
}

#[test]
fn graph_resolves_csharp_this_and_base_method_calls() {
    let mut parser = LanguageParser::new().unwrap();
    let animal = csharp_record(
        "src/Animal.cs",
        r#"
namespace App;

public class Animal
{
    public virtual string Speak() { return "generic"; }
}
"#,
    );
    let dog = csharp_record(
        "src/Dog.cs",
        r#"
namespace App;

public class Dog : Animal
{
    public string Bark() { return this.Speak(); }
    public override string Speak() { return base.Speak(); }
}
"#,
    );
    let parsed = [animal, dog]
        .into_iter()
        .map(|record| {
            let source = fs::read_to_string(&record.path).unwrap();
            parser.parse_source(&record, source).unwrap()
        })
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);

    let dog_id = graph
        .find_symbol_by_name("Dog")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Class)
        .expect("Dog class")
        .id;
    let animal_id = graph
        .find_symbol_by_name("Animal")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Class)
        .expect("Animal class")
        .id;

    let speaks = graph.find_symbol_by_name("Speak");
    let dog_speak_id = speaks
        .iter()
        .find(|symbol| symbol.parent_id.as_ref() == Some(&dog_id))
        .expect("Dog.Speak")
        .id
        .clone();
    let animal_speak_id = speaks
        .iter()
        .find(|symbol| symbol.parent_id.as_ref() == Some(&animal_id))
        .expect("Animal.Speak")
        .id
        .clone();
    let bark = graph
        .find_symbol_by_name("Bark")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .expect("Bark method");

    // `this.Speak()` from `Dog.Bark` must bind to `Dog.Speak` (the override).
    let this_edge = graph
        .edges()
        .iter()
        .find(|edge| edge.from == bark.id && edge.kind == EdgeKind::Calls)
        .expect("Bark -> Speak edge");
    assert_eq!(this_edge.to.as_ref(), Some(&dog_speak_id));

    // `base.Speak()` from `Dog.Speak` must bind to `Animal.Speak`.
    let base_edge = graph
        .edges()
        .iter()
        .find(|edge| edge.from == dog_speak_id && edge.kind == EdgeKind::Calls)
        .expect("Dog.Speak -> Animal.Speak edge");
    assert_eq!(base_edge.to.as_ref(), Some(&animal_speak_id));
}

#[test]
fn graph_resolves_csharp_partial_direct_calls_and_project_facts() {
    let mut parser = LanguageParser::new().unwrap();
    let records = [
        csharp_record(
            "src/Runner.cs",
            r#"
namespace App;
public partial class Runner {
    public string Run(string input) { return input; }
}
"#,
        ),
        csharp_record(
            "src/Runner.Partial.cs",
            r#"
namespace App;
public partial class Runner {
    public string RunAsync(string input) { return Run(input); }
}
"#,
        ),
        unsupported_record(
            "App.csproj",
            r#"<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup><TargetFrameworks>net8.0;net9.0</TargetFrameworks></PropertyGroup>
  <ItemGroup>
    <PackageReference Include="Example.Dependency" Version="1.2.3" />
    <ProjectReference Include="src/Lib/Lib.csproj" />
  </ItemGroup>
</Project>"#,
        ),
        unsupported_record("global.json", r#"{ "sdk": { "version": "8.0.100" } }"#),
        unsupported_record(
            "Directory.Build.props",
            r#"<Project><ItemGroup><Compile Include="generated/*.cs" /></ItemGroup></Project>"#,
        ),
        unsupported_record(
            "packages.lock.json",
            r#"{ "dependencies": { "net8.0": { "Locked.Package": { "resolved": "4.5.6" } } } }"#,
        ),
    ];
    let parsed = records
        .into_iter()
        .map(|record| {
            let source = fs::read_to_string(&record.path).unwrap();
            parser.parse_source(&record, source).unwrap()
        })
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);

    let run_async = graph
        .find_symbol_by_name("RunAsync")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .expect("RunAsync method");
    let run = graph
        .find_symbol_by_name("Run")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .expect("Run method");
    let edge = graph
        .edges()
        .iter()
        .find(|edge| edge.from == run_async.id && edge.kind == EdgeKind::Calls)
        .expect("RunAsync call edge");
    assert_eq!(edge.to.as_ref(), Some(&run.id));
    assert_eq!(edge.confidence, Confidence::ExactSyntax);

    let facts = graph
        .dotnet_project_facts()
        .iter()
        .map(|fact| format!("{}:{}:{}", fact.provider, fact.kind, fact.value))
        .collect::<Vec<_>>();
    for expected in [
        "csproj:target_framework:net8.0",
        "csproj:target_framework:net9.0",
        "csproj:dependency:Example.Dependency:1.2.3",
        "csproj:project_reference:src/Lib/Lib.csproj",
        "global-json:sdk:8.0.100",
        "directory-build-props:configured_source:generated/*.cs",
        "packages-lock:dependency:Locked.Package:4.5.6",
    ] {
        assert!(
            facts.iter().any(|fact| fact == expected),
            "missing {expected}; facts={facts:?}"
        );
    }
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
    assert_eq!(manager.build_report().language.rust_files, 1);
    assert_eq!(manager.build_report().language.csharp_files, 0);
    assert_eq!(manager.build_report().language.go_files, 0);
    assert_eq!(manager.build_report().language.python_files, 0);
    assert_eq!(manager.build_report().language.supported_files, 1);

    thread::sleep(Duration::from_millis(2));
    fs::write(root.join("src").join("lib.rs"), "fn two() { beta(); }\n").unwrap();

    let report = manager.refresh_before_query().unwrap();

    assert!(report.refreshed);
    assert_eq!(report.reparsed_files, 1);
    assert_eq!(report.changed_paths_from_events, 0);
    assert_eq!(report.changed_paths_from_polling, 1);
    assert_eq!(report.unchanged_event_paths, 0);
    assert_eq!(report.language.rust_files, 1);
    assert!(manager.graph().find_symbol_by_name("one").is_empty());
    assert!(!manager.graph().find_symbol_by_name("two").is_empty());
}

#[test]
fn graph_manager_refresh_converges_for_csharp_changes_and_ignores_unsupported_only() {
    let root = temp_root("graph-manager-csharp-refresh");
    fs::create_dir_all(root.join("src")).unwrap();
    let project = root.join("App.csproj");
    fs::write(
        &project,
        "<Project Sdk=\"Microsoft.NET.Sdk\"><PropertyGroup><TargetFramework>net8.0</TargetFramework></PropertyGroup></Project>",
    )
    .unwrap();
    let runner = root.join("src").join("Runner.cs");
    fs::write(
        &runner,
        "namespace App;\npublic partial class Runner { public string One() => \"one\"; }\n",
    )
    .unwrap();
    let notes = root.join("notes.txt");
    fs::write(&notes, "first\n").unwrap();

    let mut manager = GraphManager::open_with_config(
        &root,
        RefreshConfig {
            debounce: Duration::from_millis(0),
            idle_refresh_interval: Duration::from_millis(0),
            per_tool_refresh_budget: Duration::from_secs(5),
        },
    )
    .unwrap();
    assert_eq!(manager.build_report().language.csharp_files, 1);
    assert!(!manager.graph().find_symbol_by_name("One").is_empty());

    thread::sleep(Duration::from_millis(2));
    fs::write(&notes, "second\n").unwrap();
    manager.record_changed_path(notes.clone());
    let unsupported = manager.refresh_before_query().unwrap();
    assert!(!unsupported.refreshed);
    assert_eq!(unsupported.reparsed_files, 0);
    assert_eq!(unsupported.changed_paths_from_events, 0);
    assert_eq!(unsupported.unchanged_event_paths, 1);

    thread::sleep(Duration::from_millis(2));
    fs::write(
        &runner,
        "namespace App;\npublic partial class Runner { public string Two() => \"two\"; }\n",
    )
    .unwrap();
    manager.record_changed_paths([runner.clone(), notes]);
    let changed = manager.refresh_before_query().unwrap();
    assert!(changed.refreshed);
    assert_eq!(changed.reparsed_files, 1);
    assert_eq!(changed.changed_paths_from_events, 1);
    assert_eq!(changed.language.csharp_files, 1);
    assert!(manager.graph().find_symbol_by_name("One").is_empty());
    assert!(!manager.graph().find_symbol_by_name("Two").is_empty());

    let fresh = GraphManager::open_with_config(
        &root,
        RefreshConfig {
            debounce: Duration::from_millis(0),
            idle_refresh_interval: Duration::from_millis(0),
            per_tool_refresh_budget: Duration::from_secs(5),
        },
    )
    .unwrap();
    assert_eq!(manager.graph().stats(), fresh.graph().stats());
    assert_eq!(
        manager
            .graph()
            .find_symbol_by_name("Two")
            .into_iter()
            .map(|symbol| symbol.language_identity)
            .collect::<Vec<_>>(),
        fresh
            .graph()
            .find_symbol_by_name("Two")
            .into_iter()
            .map(|symbol| symbol.language_identity)
            .collect::<Vec<_>>()
    );
}

#[test]
fn graph_manager_refresh_reports_event_and_unchanged_paths() {
    let root = temp_root("graph-manager-refresh-events");
    fs::create_dir_all(root.join("src")).unwrap();
    let path = root.join("src").join("app.ts");
    fs::write(&path, "export const one = () => one;\n").unwrap();

    let mut manager = GraphManager::open_with_config(
        &root,
        RefreshConfig {
            debounce: Duration::from_millis(0),
            idle_refresh_interval: Duration::from_millis(0),
            per_tool_refresh_budget: Duration::from_secs(5),
        },
    )
    .unwrap();
    assert_eq!(manager.build_report().language.typescript_files, 1);

    manager.record_changed_path(path.clone());
    let unchanged = manager.refresh_before_query().unwrap();
    assert!(!unchanged.refreshed);
    assert_eq!(unchanged.unchanged_event_paths, 1);

    thread::sleep(Duration::from_millis(2));
    fs::write(&path, "export const two = () => two;\n").unwrap();
    manager.record_changed_path(path);
    let changed = manager.refresh_before_query().unwrap();
    assert!(changed.refreshed);
    assert_eq!(changed.reparsed_files, 1);
    assert_eq!(changed.changed_paths_from_events, 1);
    assert_eq!(changed.changed_paths_from_polling, 0);
    assert_eq!(changed.language.typescript_files, 1);
    assert!(manager.graph().find_symbol_by_name("one").is_empty());
    assert!(!manager.graph().find_symbol_by_name("two").is_empty());
}

#[test]
fn graph_manager_refresh_indexes_c_family_and_reparses_changed_header() {
    let root = temp_root("graph-manager-c-family-refresh");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src").join("runner.cpp"),
        "#include \"runner.hpp\"\nint run() { return helper(); }\nint helper() { return 1; }\n",
    )
    .unwrap();
    fs::write(root.join("src").join("runner.hpp"), "int run();\n").unwrap();

    let mut manager = GraphManager::open_with_config(
        &root,
        RefreshConfig {
            debounce: Duration::from_millis(0),
            idle_refresh_interval: Duration::from_millis(0),
            per_tool_refresh_budget: Duration::from_secs(5),
        },
    )
    .unwrap();
    assert_eq!(manager.build_report().language.cpp_files, 2);
    assert_eq!(manager.build_report().language.supported_files, 2);
    assert!(!manager.graph().find_symbol_by_name("run").is_empty());

    thread::sleep(Duration::from_millis(2));
    fs::write(
        root.join("src").join("runner.hpp"),
        "int run();\nint added();\n",
    )
    .unwrap();
    manager.record_changed_path(root.join("src").join("runner.hpp"));
    let report = manager.refresh_before_query().unwrap();

    assert!(report.refreshed);
    assert_eq!(report.reparsed_files, 1);
    assert_eq!(report.language.cpp_files, 2);
    assert!(!manager.graph().find_symbol_by_name("added").is_empty());
}

#[test]
fn graph_reports_indexing_policy_coverage() {
    let root = temp_root("graph-policy-coverage");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("vendor/lib")).unwrap();
    fs::write(root.join("src").join("lib.rs"), "pub fn indexed() {}\n").unwrap();
    fs::write(root.join("vendor/lib/lib.rs"), "pub fn vendored() {}\n").unwrap();
    fs::write(root.join("Cargo.lock"), "# lock\n").unwrap();

    let manager = GraphManager::open_with_crawl_options(
        &root,
        RefreshConfig::default(),
        CrawlOptions::default(),
    )
    .unwrap();

    assert!(!manager.graph().find_symbol_by_name("indexed").is_empty());
    assert!(manager.graph().find_symbol_by_name("vendored").is_empty());
    // Cargo.lock is a file-level exclusion; vendor/ is a directory-level
    // pruning (one entry rather than one entry per file under it).
    assert!(manager.build_report().excluded_files >= 1);
    assert!(manager.build_report().excluded_dirs >= 1);
    assert!(
        manager
            .build_report()
            .coverage
            .reasons
            .contains_key("vendor")
    );
    assert!(
        manager
            .build_report()
            .coverage
            .reasons
            .contains_key("lockfile")
    );
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
    let mut parser = LanguageParser::new().unwrap();
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

#[test]
fn graph_binds_references_to_selected_same_name_symbol() {
    let mut parser = LanguageParser::new().unwrap();
    let first = record(
        "src/first.rs",
        r#"
pub fn target() {}

pub fn caller() {
    target();
}
"#,
    );
    let second = record(
        "src/second.rs",
        r#"
pub fn target() {}

pub fn caller() {
    target();
}
"#,
    );
    let first_parsed = parser
        .parse_source(&first, fs::read_to_string(&first.path).unwrap())
        .unwrap();
    let second_parsed = parser
        .parse_source(&second, fs::read_to_string(&second.path).unwrap())
        .unwrap();
    let graph = SemanticGraph::from_parsed(vec![first_parsed, second_parsed]);
    let mut targets = graph.find_symbol_by_name("target");
    targets.sort_by(|left, right| left.file_id.0.cmp(&right.file_id.0));

    let first_refs = graph.references_to_symbol(&targets[0].id);
    let second_refs = graph.references_to_symbol(&targets[1].id);

    assert!(graph.reference_search("target").len() > first_refs.len());
    assert!(
        first_refs
            .iter()
            .all(|hit| hit.reference.file_id.0 == "src/first.rs")
    );
    assert!(
        second_refs
            .iter()
            .all(|hit| hit.reference.file_id.0 == "src/second.rs")
    );
}

#[test]
fn graph_does_not_bind_external_receiver_method_to_unique_local_method() {
    let source = r#"
pub struct Local;

impl Local {
    pub fn get(&self) {}
}

pub fn caller(map: std::collections::HashMap<String, String>) {
    map.get("key");
}
"#;
    let mut parser = LanguageParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let caller = graph.find_symbol_by_name("caller").pop().unwrap();

    assert!(
        graph.callees(&caller.id).iter().all(|hit| hit
            .callee
            .as_ref()
            .map(|symbol| symbol.name.as_str())
            != Some("get"))
    );
}

#[test]
fn graph_does_not_bind_value_identifier_to_same_name_function() {
    let source = r#"
fn lookup() {}

fn caller() {
    let lookup = 1;
    let _ = lookup;
}
"#;
    let mut parser = LanguageParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let lookup = graph.find_symbol_by_name("lookup").pop().unwrap();

    assert!(
        graph
            .references_to_symbol(&lookup.id)
            .iter()
            .all(|hit| hit.reference.span.start_byte < lookup.body_span.unwrap().start_byte)
    );
}

#[test]
fn graph_does_not_bind_enum_variant_path_to_same_name_struct() {
    let source = r#"
struct Generate;

enum Mode {
    Generate,
}

fn caller() {
    let _ = Mode::Generate;
}
"#;
    let mut parser = LanguageParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let generate = graph.find_symbol_by_name("Generate").pop().unwrap();

    assert!(
        graph
            .references_to_symbol(&generate.id)
            .iter()
            .all(|hit| hit.reference.text != "Mode::Generate")
    );
}

#[test]
fn graph_declaration_match_ignores_same_name_signature_parameters() {
    let source = r#"
trait Sink {
    fn finish(&mut self, finish: &usize);
}
"#;
    let mut parser = LanguageParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let finish = graph.find_symbol_by_name("finish").pop().unwrap();

    assert!(graph.references_to_symbol(&finish.id).is_empty());
}

#[test]
fn graph_symbol_references_are_package_local_until_cargo_resolution_exists() {
    let mut parser = LanguageParser::new().unwrap();
    let source_package = record("crates/source/src/lib.rs", "pub struct Shared;\n");
    let user_package = record(
        "crates/user/src/lib.rs",
        r#"
use source::Shared;

pub fn user(_: Shared) {}
"#,
    );
    let source_parsed = parser
        .parse_source(
            &source_package,
            fs::read_to_string(&source_package.path).unwrap(),
        )
        .unwrap();
    let user_parsed = parser
        .parse_source(
            &user_package,
            fs::read_to_string(&user_package.path).unwrap(),
        )
        .unwrap();
    let graph = SemanticGraph::from_parsed(vec![source_parsed, user_parsed]);
    let shared = graph.find_symbol_by_name("Shared").pop().unwrap();

    assert!(
        graph.references_to_symbol(&shared.id).iter().all(|hit| hit
            .reference
            .file_id
            .0
            .starts_with("crates/source/"))
    );
}

#[test]
fn graph_does_not_bind_external_std_path_to_local_type() {
    let source = r#"
struct IntoIter;

fn caller() -> std::vec::IntoIter<u8> {
    todo!()
}
"#;
    let mut parser = LanguageParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let into_iter = graph.find_symbol_by_name("IntoIter").pop().unwrap();

    assert!(graph.references_to_symbol(&into_iter.id).is_empty());
}

#[test]
fn graph_resolves_module_qualified_direct_calls() {
    let mut parser = LanguageParser::new().unwrap();
    let output = record("src/output.rs", "pub fn print_entry() {}\n");
    let walk = record(
        "src/walk.rs",
        r#"
use crate::output;

pub fn scan() {
    output::print_entry();
}
"#,
    );
    let output_parsed = parser
        .parse_source(&output, fs::read_to_string(&output.path).unwrap())
        .unwrap();
    let walk_parsed = parser
        .parse_source(&walk, fs::read_to_string(&walk.path).unwrap())
        .unwrap();
    let graph = SemanticGraph::from_parsed(vec![output_parsed, walk_parsed]);
    let scan = graph.find_symbol_by_name("scan").pop().unwrap();
    let print_entry = graph.find_symbol_by_name("print_entry").pop().unwrap();

    assert!(graph.call_chain(&scan.id, &print_entry.id, 2).is_some());
    assert!(
        graph
            .references_to_symbol(&print_entry.id)
            .iter()
            .any(|hit| hit.reference.text == "output::print_entry")
    );
}

#[test]
fn path_starts_with_external_root_is_language_aware() {
    // Rust paths only match Rust stdlib roots.
    assert!(path_starts_with_external_root(
        "std::fmt::Debug",
        LanguageKind::Rust
    ));
    assert!(path_starts_with_external_root(
        "core::convert::From",
        LanguageKind::Rust
    ));
    // Rust paths that happen to start with Go stdlib package names (e.g.
    // `sync::Mutex` after `use tokio::sync;`) must NOT be treated as external.
    for path in [
        "sync::Mutex",
        "io::Read",
        "os::ProcessId",
        "time::Duration",
        "fmt::Formatter",
        "errors::Error",
    ] {
        assert!(
            !path_starts_with_external_root(path, LanguageKind::Rust),
            "{path} must not be flagged external for Rust",
        );
    }
    // Go paths match Go stdlib roots regardless of separator.
    for path in [
        "fmt.Println",
        "fmt.Errorf",
        "sync.Mutex",
        "io.Reader",
        "os.Getenv",
        "time.Now",
        "context.Background",
    ] {
        assert!(
            path_starts_with_external_root(path, LanguageKind::Go),
            "{path} must be flagged external for Go",
        );
    }
    // Go paths starting with Rust stdlib roots are not Go externals.
    assert!(!path_starts_with_external_root("std.Foo", LanguageKind::Go));
    // Python references do not currently classify any path as external.
    assert!(!path_starts_with_external_root(
        "os.path.join",
        LanguageKind::Python
    ));
    assert!(!path_starts_with_external_root(
        "sys.argv",
        LanguageKind::Python
    ));
}

#[test]
fn graph_resolves_type_qualified_associated_functions() {
    let source = r#"
pub struct Command;

impl Command {
    pub fn new() -> Self {
        Command
    }
}

pub fn caller() {
    Command::new();
}
"#;
    let mut parser = LanguageParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let caller = graph.find_symbol_by_name("caller").pop().unwrap();
    let new = graph.find_symbol_by_name("new").pop().unwrap();

    assert!(graph.call_chain(&caller.id, &new.id, 2).is_some());
    assert!(
        graph
            .references_to_symbol(&new.id)
            .iter()
            .any(|hit| hit.reference.text == "Command::new")
    );
}

#[test]
fn graph_binds_imported_grouped_type_references() {
    let mut parser = LanguageParser::new().unwrap();
    let lowargs = record(
        "crates/core/flags/lowargs.rs",
        r#"
pub enum ContextMode {
    Passthru,
}
"#,
    );
    let defs = record(
        "crates/core/flags/defs.rs",
        r#"
use crate::flags::lowargs::{ContextMode};

pub fn use_context(mode: ContextMode) {
    let _ = ContextMode::Passthru;
    let _ = mode;
}
"#,
    );
    let lowargs_parsed = parser
        .parse_source(&lowargs, fs::read_to_string(&lowargs.path).unwrap())
        .unwrap();
    let defs_parsed = parser
        .parse_source(&defs, fs::read_to_string(&defs.path).unwrap())
        .unwrap();
    let graph = SemanticGraph::from_parsed(vec![lowargs_parsed, defs_parsed]);
    let context_mode = graph.find_symbol_by_name("ContextMode").pop().unwrap();
    assert!(
        graph
            .references_to_symbol(&context_mode.id)
            .iter()
            .any(|hit| hit.reference.text == "ContextMode")
    );
}

#[test]
fn graph_binds_grouped_import_clause_to_imported_type() {
    let mut parser = LanguageParser::new().unwrap();
    let lowargs = record(
        "crates/core/flags/lowargs.rs",
        r#"
pub enum ContextMode {
    Passthru,
}
"#,
    );
    let defs = record(
        "crates/core/flags/defs.rs",
        r#"
use crate::flags::lowargs::{ContextMode};
"#,
    );
    let lowargs_parsed = parser
        .parse_source(&lowargs, fs::read_to_string(&lowargs.path).unwrap())
        .unwrap();
    let defs_parsed = parser
        .parse_source(&defs, fs::read_to_string(&defs.path).unwrap())
        .unwrap();
    let graph = SemanticGraph::from_parsed(vec![lowargs_parsed, defs_parsed]);
    let context_mode = graph.find_symbol_by_name("ContextMode").pop().unwrap();

    assert!(
        graph
            .references_to_symbol(&context_mode.id)
            .iter()
            .any(|hit| hit.reference.text == "ContextMode")
    );
}

#[test]
fn graph_resolves_inline_module_qualified_calls() {
    let source = r#"
fn caller() {
    convert::string();
}

mod convert {
    pub fn string() {}
}
"#;
    let mut parser = LanguageParser::new().unwrap();
    let record = record("src/flags/defs.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let caller = graph.find_symbol_by_name("caller").pop().unwrap();
    let string = graph.find_symbol_by_name("string").pop().unwrap();

    assert!(graph.call_chain(&caller.id, &string.id, 2).is_some());
    assert!(
        graph
            .references_to_symbol(&string.id)
            .iter()
            .any(|hit| hit.reference.text == "convert::string")
    );
}

#[test]
fn graph_binds_trait_method_impls_and_self_calls_to_trait_method() {
    let source = r#"
pub trait Decoder {
    fn decode();

    fn decode_again(&self) {
        self.decode();
    }
}

struct Concrete;

impl Decoder for Concrete {
    fn decode() {}
}

fn run() {
    Concrete::decode();
}
"#;
    let mut parser = LanguageParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let trait_decode = graph
        .find_symbol_by_name("decode")
        .into_iter()
        .find(|symbol| {
            symbol
                .parent_id
                .as_ref()
                .and_then(|id| graph.symbols.get(id))
                .map(|parent| parent.kind == SymbolKind::Trait)
                .unwrap_or(false)
        })
        .unwrap();
    let refs = graph.references_to_symbol(&trait_decode.id);

    assert!(
        refs.iter()
            .any(|hit| hit.reference.text == "decode" && hit.reference.span.start_byte > 100)
    );
    assert!(
        refs.iter()
            .any(|hit| hit.reference.text == "Concrete::decode")
    );
}

#[test]
fn graph_does_not_cross_bind_same_name_use_tree_siblings() {
    let source = r#"
mod a {
    pub struct Foo;
}

mod b {
    pub struct Foo;
}

use crate::{a::Foo as FA, b::Foo as FB};

fn build() -> (FA, FB) {
    let fa: FA = a::Foo;
    let fb: FB = b::Foo;
    (fa, fb)
}
"#;
    let mut parser = LanguageParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);

    let foos: Vec<_> = graph.find_symbol_by_name("Foo").into_iter().collect();
    assert_eq!(foos.len(), 2);

    let module_of = |sym: &GraphSymbol| {
        sym.parent_id
            .as_ref()
            .and_then(|id| graph.symbols.get(id))
            .map(|module| module.name.clone())
            .unwrap_or_default()
    };
    let foo_a = foos
        .iter()
        .find(|sym| module_of(sym) == "a")
        .expect("a::Foo");
    let foo_b = foos
        .iter()
        .find(|sym| module_of(sym) == "b")
        .expect("b::Foo");

    // Locate the byte offsets of the two `Foo` identifier tokens inside the
    // `use` clause; with the bug, both references bind to BOTH foo symbols.
    let use_start = source.find("use crate::{").expect("use clause");
    let use_end = source[use_start..].find("};").expect("end of use clause") + use_start;
    let foo_in_a_use = source[use_start..use_end]
        .find("a::Foo")
        .expect("a::Foo in use")
        + use_start
        + "a::".len();
    let foo_in_b_use = source[use_start..use_end]
        .find("b::Foo")
        .expect("b::Foo in use")
        + use_start
        + "b::".len();

    let in_use_clause_only = |hits: &[ReferenceHit]| -> Vec<u32> {
        hits.iter()
            .map(|h| h.reference.span.start_byte)
            .filter(|byte| (*byte as usize) >= use_start && (*byte as usize) < use_end)
            .collect()
    };
    let refs_a_use = in_use_clause_only(&graph.references_to_symbol(&foo_a.id));
    let refs_b_use = in_use_clause_only(&graph.references_to_symbol(&foo_b.id));

    // Critical no-cross-bind invariant: the inside-use `Foo` token from one
    // segment must NEVER bind to the other module's struct. (extract_import
    // currently records the whole `use_declaration` span on every flattened
    // import, so without the collision guard both inside-segment references
    // would bind to both Foo symbols.)
    assert!(
        !refs_a_use.contains(&(foo_in_b_use as u32)),
        "a::Foo must not be bound by the `Foo` token inside the b::Foo segment"
    );
    assert!(
        !refs_b_use.contains(&(foo_in_a_use as u32)),
        "b::Foo must not be bound by the `Foo` token inside the a::Foo segment"
    );
}

#[test]
fn graph_does_not_bind_impl_decl_across_same_name_traits_in_other_modules() {
    let source = r#"
mod a {
    pub trait Decoder {
        fn decode();
    }
}

mod b {
    pub trait Decoder {
        fn decode();
    }
}

struct Concrete;

impl crate::a::Decoder for Concrete {
    fn decode() {}
}
"#;
    let mut parser = LanguageParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);

    let trait_methods: Vec<_> = graph
        .find_symbol_by_name("decode")
        .into_iter()
        .filter(|symbol| {
            symbol
                .parent_id
                .as_ref()
                .and_then(|id| graph.symbols.get(id))
                .map(|parent| parent.kind == SymbolKind::Trait)
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(trait_methods.len(), 2);

    let module_of = |trait_method: &GraphSymbol| {
        trait_method
            .parent_id
            .as_ref()
            .and_then(|id| graph.symbols.get(id))
            .and_then(|trait_sym| trait_sym.parent_id.as_ref())
            .and_then(|id| graph.symbols.get(id))
            .map(|module| module.name.clone())
            .unwrap_or_default()
    };
    let trait_a = trait_methods
        .iter()
        .find(|sym| module_of(sym) == "a")
        .expect("trait a::Decoder::decode");
    let trait_b = trait_methods
        .iter()
        .find(|sym| module_of(sym) == "b")
        .expect("trait b::Decoder::decode");

    let refs_a = graph.references_to_symbol(&trait_a.id);
    let refs_b = graph.references_to_symbol(&trait_b.id);

    assert!(
        refs_a
            .iter()
            .any(|hit| hit.reference.text == "decode" && hit.reference.span.start_byte > 80),
        "impl decode declaration should bind to a::Decoder::decode"
    );
    assert!(
        !refs_b
            .iter()
            .any(|hit| hit.reference.text == "decode" && hit.reference.span.start_byte > 80),
        "impl decode declaration must NOT cross-bind to b::Decoder::decode"
    );
}

#[test]
fn graph_skips_impl_decl_with_multiline_cfg_attribute() {
    let source = r#"
pub trait Decoder {
    fn decode();
}

struct Concrete;

impl Decoder for Concrete {
    #[cfg(any(
        feature = "x",
        feature = "y",
    ))]
    fn decode() {}
}
"#;
    let mut parser = LanguageParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let trait_decode = graph
        .find_symbol_by_name("decode")
        .into_iter()
        .find(|symbol| {
            symbol
                .parent_id
                .as_ref()
                .and_then(|id| graph.symbols.get(id))
                .map(|parent| parent.kind == SymbolKind::Trait)
                .unwrap_or(false)
        })
        .unwrap();
    let refs = graph.references_to_symbol(&trait_decode.id);

    assert!(
        !refs
            .iter()
            .any(|hit| hit.reference.text == "decode" && hit.reference.span.start_byte > 90),
        "cfg-gated impl decode declaration must not bind to the trait method"
    );
}

#[test]
fn graph_binds_uppercase_struct_constructor_references() {
    let source = r#"
struct Generate;

fn flags() {
    let _ = &Generate;
}
"#;
    let mut parser = LanguageParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let generate = graph.find_symbol_by_name("Generate").pop().unwrap();

    assert!(
        graph
            .references_to_symbol(&generate.id)
            .iter()
            .any(|hit| hit.reference.text == "Generate")
    );
}

#[test]
fn graph_does_not_bind_prelude_variant_names_to_shadow_structs() {
    let source = r#"
struct None;

fn option() -> Option<u8> {
    None
}
"#;
    let mut parser = LanguageParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let none = graph.find_symbol_by_name("None").pop().unwrap();

    assert!(graph.references_to_symbol(&none.id).is_empty());
}

#[test]
fn graph_binds_trait_owned_self_associated_type_references() {
    let source = r#"
pub trait IntoThing {
    type Output;

    fn convert(self) -> Self::Output;
}
"#;
    let mut parser = LanguageParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let output = graph.find_symbol_by_name("Output").pop().unwrap();

    assert!(
        graph
            .references_to_symbol(&output.id)
            .iter()
            .any(|hit| hit.reference.text == "Self::Output")
    );
}

#[test]
fn graph_binds_trait_qualified_associated_type_to_trait_item() {
    let source = r#"
pub trait IntoThing {
    type Output;
}

struct Local;

impl IntoThing for Local {
    type Output = Local;
}

pub fn consume(_: IntoThing::Output) {}
"#;
    let mut parser = LanguageParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let trait_output = graph
        .find_symbol_by_name("Output")
        .into_iter()
        .find(|symbol| {
            symbol
                .parent_id
                .as_ref()
                .and_then(|id| graph.symbols.get(id))
                .map(|parent| parent.kind == SymbolKind::Trait)
                .unwrap_or(false)
        })
        .unwrap();
    let impl_output = graph
        .find_symbol_by_name("Output")
        .into_iter()
        .find(|symbol| {
            symbol
                .parent_id
                .as_ref()
                .and_then(|id| graph.symbols.get(id))
                .map(|parent| parent.kind == SymbolKind::Impl)
                .unwrap_or(false)
        })
        .unwrap();

    assert!(
        graph
            .references_to_symbol(&trait_output.id)
            .iter()
            .any(|hit| hit.reference.text == "IntoThing::Output")
    );
    assert!(
        graph
            .references_to_symbol(&impl_output.id)
            .iter()
            .all(|hit| hit.reference.text != "IntoThing::Output")
    );
}

#[test]
fn graph_does_not_bind_impl_self_projection_to_impl_associated_type() {
    let source = r#"
pub trait IntoThing {
    type Output;
}

struct Local;

impl IntoThing for Local {
    type Output = Local;

    fn convert(self) -> Self::Output {
        Local
    }
}
"#;
    let mut parser = LanguageParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let impl_output = graph
        .find_symbol_by_name("Output")
        .into_iter()
        .find(|symbol| {
            symbol
                .parent_id
                .as_ref()
                .and_then(|id| graph.symbols.get(id))
                .map(|parent| parent.kind == SymbolKind::Impl)
                .unwrap_or(false)
        })
        .unwrap();

    assert!(
        graph
            .references_to_symbol(&impl_output.id)
            .iter()
            .all(|hit| hit.reference.text != "Self::Output")
    );
}

#[test]
fn graph_resolves_cpp_same_class_direct_method_call() {
    let mut parser = RustParser::new().unwrap();
    let record = cpp_record(
        "src/runner.cpp",
        r#"
class Runner {
public:
    int run() {
        return helper();
    }
    int helper() {
        return 0;
    }
};
"#,
    );
    let parsed = parser
        .parse_source(&record, fs::read_to_string(&record.path).unwrap())
        .unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);

    let helpers = graph.find_symbol_by_name("helper");
    let helper = helpers
        .iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .expect("Runner::helper method should exist");
    let run = graph
        .find_symbol_by_name("run")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .expect("Runner::run method should exist");

    // `helper()` inside `run()` parses as a Direct call with no receiver.
    // Without the same-class fallback the resolver returns CandidateSet.
    let chain = graph
        .call_chain(&run.id, &helper.id, 2)
        .expect("Runner::run -> Runner::helper should resolve");
    assert!(chain.contains(&helper.id));
}

#[test]
fn graph_resolves_c_include_cross_translation_unit_calls() {
    let mut parser = RustParser::new().unwrap();
    let header = c_record(
        "src/runner.h",
        r#"
int helper(int value);
int runner_run(int value);
"#,
    );
    let definition = c_record(
        "src/runner.c",
        r#"
#include "runner.h"

int helper(int value) {
    return value + 1;
}

int runner_run(int value) {
    return helper(value);
}
"#,
    );
    let consumer = c_record(
        "src/consumer.c",
        r#"
#include "runner.h"

int consume(int value) {
    return helper(value) + runner_run(value);
}
"#,
    );
    let parsed = vec![
        parser
            .parse_source(&header, fs::read_to_string(&header.path).unwrap())
            .unwrap(),
        parser
            .parse_source(&definition, fs::read_to_string(&definition.path).unwrap())
            .unwrap(),
        parser
            .parse_source(&consumer, fs::read_to_string(&consumer.path).unwrap())
            .unwrap(),
    ];
    let graph = SemanticGraph::from_parsed(parsed);

    let helper = graph
        .find_symbol_by_name("helper")
        .into_iter()
        .find(|symbol| {
            graph
                .files
                .get(&symbol.file_id)
                .map(|file| file.relative_path == "src/runner.c")
                .unwrap_or(false)
        })
        .expect("definition-side `helper` should exist");
    let runner_run = graph
        .find_symbol_by_name("runner_run")
        .into_iter()
        .find(|symbol| {
            graph
                .files
                .get(&symbol.file_id)
                .map(|file| file.relative_path == "src/runner.c")
                .unwrap_or(false)
        })
        .expect("definition-side `runner_run` should exist");
    let consume = graph
        .find_symbol_by_name("consume")
        .pop()
        .expect("consume function should exist");

    // Without include-aware resolution these calls would land in
    // CandidateSet because the consumer file declares neither symbol.
    assert!(
        graph.call_chain(&consume.id, &helper.id, 2).is_some(),
        "consume -> helper should resolve via the include directive"
    );
    assert!(
        graph.call_chain(&consume.id, &runner_run.id, 2).is_some(),
        "consume -> runner_run should resolve via the include directive"
    );
}

#[test]
fn graph_include_lookup_is_gated_to_c_family_callers() {
    let mut parser = RustParser::new().unwrap();
    let rust_caller = record(
        "src/rust_caller/main.rs",
        r#"
fn caller() {
    helper();
}
"#,
    );
    let c_definition = c_record(
        "src/runner/runner.c",
        r#"
int helper(int value) {
    return value;
}
"#,
    );
    let parsed = vec![
        parser
            .parse_source(&rust_caller, fs::read_to_string(&rust_caller.path).unwrap())
            .unwrap(),
        parser
            .parse_source(
                &c_definition,
                fs::read_to_string(&c_definition.path).unwrap(),
            )
            .unwrap(),
    ];
    let graph = SemanticGraph::from_parsed(parsed);
    let caller = graph
        .find_symbol_by_name("caller")
        .pop()
        .expect("caller function should exist");

    // The include-aware lookup must never appear in the provenance for a
    // Rust caller, even if other heuristics happen to bind the call.
    for edge in graph.callees(&caller.id) {
        assert!(
            !edge.edge.provenance.reason.contains("include directive"),
            "Rust caller's call edge should never use the C/C++ include lookup; provenance was {:?}",
            edge.edge.provenance.reason
        );
    }
}

#[test]
fn graph_field_reference_with_arrow_access_matches_struct_field() {
    let mut parser = RustParser::new().unwrap();
    let record = c_record(
        "src/runner.c",
        r#"
struct Runner {
    int id;
};

int peek(struct Runner *runner) {
    return runner->id;
}
"#,
    );
    let parsed = parser
        .parse_source(&record, fs::read_to_string(&record.path).unwrap())
        .unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);

    let id_field = graph
        .find_symbol_by_name("id")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Field)
        .expect("id field should exist");

    // Arrow-access references like `runner->id` should bind to the local
    // field via the new `last_path_segment` arrow stripping.
    assert!(
        graph
            .references_to_symbol(&id_field.id)
            .iter()
            .any(|hit| hit.reference.text.contains("id")),
        "expected `runner->id` reference to bind to struct field `id`"
    );
}

#[test]
fn include_path_helper_matches_basename_and_suffix() {
    assert!(include_path_matches_file("runner.h", "src/runner.h"));
    assert!(include_path_matches_file(
        "utils/runner.h",
        "src/utils/runner.h"
    ));
    assert!(include_path_matches_file("src/runner.h", "src/runner.h"));
    assert!(!include_path_matches_file("runner.h", "src/runner_alt.h"));
    assert!(!include_path_matches_file(
        "utils/runner.h",
        "src/utils_x/runner.h"
    ));
    assert!(!include_path_matches_file("", "src/runner.h"));
}

fn c_record(relative_path: &str, source: &str) -> FileRecord {
    let mut record = record(relative_path, source);
    record.language = LanguageKind::C;
    record
}

fn cpp_record(relative_path: &str, source: &str) -> FileRecord {
    let mut record = record(relative_path, source);
    record.language = LanguageKind::Cpp;
    record
}

#[test]
fn annotate_dirty_ranges_marks_only_intersecting_symbols_and_clears_on_reapply() {
    let source = "pub fn first() -> usize { 1 }\npub fn second() -> usize { 2 }\npub fn third() -> usize { 3 }\n";
    let mut parser = LanguageParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let mut graph = SemanticGraph::from_parsed(vec![parsed]);

    let mut dirty = HashMap::new();
    dirty.insert(
        FileId::new("src/lib.rs"),
        DirtyAnnotation {
            status: "modified".to_string(),
            ranges: vec![DirtyRange {
                start_line: 1,
                end_line: 1,
            }],
        },
    );
    graph.annotate_dirty_ranges(&dirty);

    let dirty_names = graph
        .dirty_symbols()
        .into_iter()
        .map(|symbol| symbol.name)
        .collect::<Vec<_>>();
    assert_eq!(dirty_names, vec!["second".to_string()]);
    assert!(
        graph
            .dirty_symbols()
            .iter()
            .all(|symbol| symbol.kind != SymbolKind::File)
    );

    dirty.clear();
    dirty.insert(
        FileId::new("src/lib.rs"),
        DirtyAnnotation {
            status: "modified".to_string(),
            ranges: vec![DirtyRange {
                start_line: 2,
                end_line: 2,
            }],
        },
    );
    graph.annotate_dirty_ranges(&dirty);
    let dirty_names = graph
        .dirty_symbols()
        .into_iter()
        .map(|symbol| symbol.name)
        .collect::<Vec<_>>();
    assert_eq!(dirty_names, vec!["third".to_string()]);

    graph.annotate_dirty_ranges(&HashMap::new());
    assert!(graph.dirty_symbols().is_empty());
}

#[test]
fn graph_resolves_java_static_member_imports_to_enclosing_class_method() {
    let mut parser = LanguageParser::new().unwrap();
    let names = java_record(
        "src/main/java/com/example/util/Names.java",
        r#"
package com.example.util;

public enum Names {
    DEFAULT;

    public static String defaultName() {
        return "Ada";
    }
}
"#,
    );
    let app = java_record(
        "src/main/java/com/example/app/Runner.java",
        r#"
package com.example.app;

import static com.example.util.Names.defaultName;

public class Runner {
    public String greet() {
        return defaultName();
    }
}
"#,
    );
    let parsed = vec![
        parser
            .parse_source(&names, fs::read_to_string(&names.path).unwrap())
            .unwrap(),
        parser
            .parse_source(&app, fs::read_to_string(&app.path).unwrap())
            .unwrap(),
    ];
    let graph = SemanticGraph::from_parsed(parsed);

    let names_class = graph.find_symbol_by_name("Names").pop().unwrap();
    let default_name = graph
        .find_symbol_by_name("defaultName")
        .into_iter()
        .find(|symbol| symbol.parent_id.as_ref() == Some(&names_class.id))
        .unwrap();
    let greet = graph
        .find_symbol_by_name("greet")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .unwrap();

    assert!(graph.call_chain(&greet.id, &default_name.id, 3).is_some());
}

#[test]
fn graph_resolves_java_glob_imports_to_classes_in_package() {
    let mut parser = LanguageParser::new().unwrap();
    let greeter = java_record(
        "src/main/java/com/example/services/Greeter.java",
        r#"
package com.example.services;

public class Greeter {
    public String greet(String name) {
        return name;
    }
}
"#,
    );
    let runner = java_record(
        "src/main/java/com/example/app/Runner.java",
        r#"
package com.example.app;

import com.example.services.*;

public class Runner {
    public Greeter create() {
        return new Greeter();
    }
}
"#,
    );
    let parsed = vec![
        parser
            .parse_source(&greeter, fs::read_to_string(&greeter.path).unwrap())
            .unwrap(),
        parser
            .parse_source(&runner, fs::read_to_string(&runner.path).unwrap())
            .unwrap(),
    ];
    let graph = SemanticGraph::from_parsed(parsed);

    let greeter_class = graph.find_symbol_by_name("Greeter").pop().unwrap();
    assert!(
        graph
            .references_to_symbol(&greeter_class.id)
            .iter()
            .any(|hit| hit.reference.text == "Greeter")
    );
}

#[test]
fn graph_resolves_java_nested_class_imports_to_inner_type() {
    let mut parser = LanguageParser::new().unwrap();
    let outer = java_record(
        "src/main/java/com/example/util/Outer.java",
        r#"
package com.example.util;

public class Outer {
    public static class Inner {
        public String describe() {
            return "inner";
        }
    }
}
"#,
    );
    let runner = java_record(
        "src/main/java/com/example/app/Runner.java",
        r#"
package com.example.app;

import com.example.util.Outer.Inner;

public class Runner {
    public Inner make() {
        return new Inner();
    }
}
"#,
    );
    let parsed = vec![
        parser
            .parse_source(&outer, fs::read_to_string(&outer.path).unwrap())
            .unwrap(),
        parser
            .parse_source(&runner, fs::read_to_string(&runner.path).unwrap())
            .unwrap(),
    ];
    let graph = SemanticGraph::from_parsed(parsed);

    let outer_class = graph
        .find_symbol_by_name("Outer")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Class)
        .unwrap();
    let inner_class = graph
        .find_symbol_by_name("Inner")
        .into_iter()
        .find(|symbol| symbol.parent_id.as_ref() == Some(&outer_class.id))
        .unwrap();
    assert!(
        graph
            .references_to_symbol(&inner_class.id)
            .iter()
            .any(|hit| hit.reference.text == "Inner")
    );
}

#[test]
fn graph_emits_one_symbol_per_java_field_declarator() {
    let mut parser = LanguageParser::new().unwrap();
    let widget = java_record(
        "src/main/java/com/example/util/Widget.java",
        r#"
package com.example.util;

public class Widget {
    private int alpha, beta, gamma;
    public static final String FIRST = "1", SECOND = "2";
}
"#,
    );
    let parsed = vec![
        parser
            .parse_source(&widget, fs::read_to_string(&widget.path).unwrap())
            .unwrap(),
    ];
    let graph = SemanticGraph::from_parsed(parsed);

    for name in ["alpha", "beta", "gamma", "FIRST", "SECOND"] {
        assert!(
            graph
                .find_symbol_by_name(name)
                .into_iter()
                .any(|symbol| symbol.kind == SymbolKind::Field),
            "missing field declarator {name}",
        );
    }
}

#[test]
fn graph_records_only_real_maven_dependencies() {
    let _ = LanguageParser::new().unwrap();
    let mut pom = record(
        "pom.xml",
        r#"<project>
  <modelVersion>4.0.0</modelVersion>
  <dependencyManagement>
    <dependencies>
      <dependency>
        <groupId>org.managed</groupId>
        <artifactId>only-managed</artifactId>
        <version>1.0.0</version>
      </dependency>
    </dependencies>
  </dependencyManagement>
  <dependencies>
    <dependency>
      <groupId>org.junit.jupiter</groupId>
      <artifactId>junit-jupiter</artifactId>
      <version>5.10.0</version>
      <scope>test</scope>
    </dependency>
  </dependencies>
  <build>
    <plugins>
      <plugin>
        <groupId>org.apache.maven.plugins</groupId>
        <artifactId>maven-compiler-plugin</artifactId>
        <version>3.11.0</version>
        <dependencies>
          <dependency>
            <groupId>org.plugin</groupId>
            <artifactId>plugin-dep</artifactId>
            <version>9.9.9</version>
          </dependency>
        </dependencies>
      </plugin>
    </plugins>
  </build>
</project>
"#,
    );
    pom.language = LanguageKind::Unsupported;
    let parsed = vec![ParsedFile::unsupported(pom, "maven metadata")];
    let graph = SemanticGraph::from_parsed(parsed);

    let dependencies = graph
        .java_project_facts()
        .iter()
        .filter(|fact| fact.kind == "dependency")
        .map(|fact| fact.value.clone())
        .collect::<Vec<_>>();
    assert!(
        dependencies.contains(&"test:org.junit.jupiter:junit-jupiter:5.10.0".to_string()),
        "expected real dependency to be recorded; got {dependencies:?}",
    );
    assert!(
        !dependencies
            .iter()
            .any(|value| value.contains("only-managed")),
        "managed dependencies should be excluded; got {dependencies:?}",
    );
    assert!(
        !dependencies
            .iter()
            .any(|value| value.contains("plugin-dep")),
        "plugin dependencies should be excluded; got {dependencies:?}",
    );
}

#[test]
fn candidate_set_call_edge_emits_ids() {
    let mut parser = LanguageParser::new().unwrap();
    let source = python_record(
        "src/dispatch.py",
        r#"
class Alpha:
    def do_thing(self):
        return 1

class Beta:
    def do_thing(self):
        return 2

def caller(obj):
    return obj.do_thing()
"#,
    );
    let parsed = parser
        .parse_source(&source, fs::read_to_string(&source.path).unwrap())
        .unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);

    let candidate_edges: Vec<&GraphEdge> = graph
        .edges
        .iter()
        .filter(|edge| edge.target_text.contains("do_thing"))
        .filter(|edge| edge.confidence == Confidence::CandidateSet)
        .collect();
    assert!(
        !candidate_edges.is_empty(),
        "expected at least one CandidateSet edge for `do_thing` (edges: {:?})",
        graph
            .edges
            .iter()
            .filter(|e| e.target_text.contains("do_thing"))
            .map(|e| (&e.target_text, e.confidence, e.candidates.len()))
            .collect::<Vec<_>>(),
    );
    let edge = candidate_edges[0];
    assert_eq!(
        edge.candidates.len(),
        2,
        "expected both `do_thing` methods to appear as candidates, got {} ({:?})",
        edge.candidates.len(),
        edge.candidates,
    );
    for id in &edge.candidates {
        let sym = graph.symbols.get(id).expect("candidate id resolves");
        assert_eq!(sym.name, "do_thing");
        assert_eq!(sym.kind, SymbolKind::Method);
    }
}

#[test]
fn candidate_set_truncated_to_max_edge_candidates() {
    let mut parser = LanguageParser::new().unwrap();
    let mut classes = String::new();
    for i in 0..12 {
        classes.push_str(&format!(
            "class Cls{i}:\n    def shared(self):\n        return {i}\n\n"
        ));
    }
    classes.push_str("def caller(obj):\n    return obj.shared()\n");
    let source = python_record("src/dispatch_big.py", &classes);
    let parsed = parser
        .parse_source(&source, fs::read_to_string(&source.path).unwrap())
        .unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);

    let edge = graph
        .edges
        .iter()
        .find(|edge| {
            edge.target_text.contains("shared") && edge.confidence == Confidence::CandidateSet
        })
        .expect("expected CandidateSet edge for `shared`");
    assert!(
        edge.candidates.len() <= MAX_EDGE_CANDIDATES,
        "candidates cap exceeded: {} > {}",
        edge.candidates.len(),
        MAX_EDGE_CANDIDATES,
    );
    assert_eq!(edge.candidates.len(), MAX_EDGE_CANDIDATES);
}

#[test]
fn graph_edge_deserializes_without_candidates_field() {
    let stored = serde_json::json!({
        "from": "file:src/x.rs",
        "to": null,
        "target_text": "thing",
        "kind": "Calls",
        "span": null,
        "confidence": "CandidateSet",
        "freshness": "Fresh",
        "provenance": {"source": "test", "reason": "fixture"}
    });
    let edge: GraphEdge = serde_json::from_value(stored).expect("legacy edge JSON deserializes");
    assert!(edge.candidates.is_empty());
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

fn python_record(relative_path: &str, source: &str) -> FileRecord {
    let mut record = record(relative_path, source);
    record.language = LanguageKind::Python;
    record
}

fn ts_record(relative_path: &str, source: &str) -> FileRecord {
    let mut record = record(relative_path, source);
    record.language = LanguageKind::TypeScript;
    record
}

fn tsx_record(relative_path: &str, source: &str) -> FileRecord {
    let mut record = record(relative_path, source);
    record.language = LanguageKind::Tsx;
    record
}

fn unsupported_record(relative_path: &str, source: &str) -> FileRecord {
    let mut record = record(relative_path, source);
    record.language = LanguageKind::Unsupported;
    record
}

fn java_record(relative_path: &str, source: &str) -> FileRecord {
    let mut record = record(relative_path, source);
    record.language = LanguageKind::Java;
    record
}

fn csharp_record(relative_path: &str, source: &str) -> FileRecord {
    let mut record = record(relative_path, source);
    record.language = LanguageKind::CSharp;
    record
}

fn go_record(relative_path: &str, source: &str) -> FileRecord {
    let mut record = record(relative_path, source);
    record.language = LanguageKind::Go;
    record
}

fn temp_root(name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("squeezy-{name}-{pid}-{counter}-{nonce}"));
    fs::create_dir_all(&root).unwrap();
    root
}
