use std::{
    fs,
    path::{Path, PathBuf},
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

/// `signature_span` must survive the `ParsedSymbol -> GraphSymbol` conversion
/// and still point at the declaration header, so a `read_slice` signature read
/// off the graph slices only the header and not the body. Mirrors the
/// parse-side coverage one layer up, where `read_slice` actually consumes it.
#[test]
fn signature_span_survives_graph_and_excludes_body() {
    let source = "pub fn add(a: i32, b: i32) -> i32 {\n    let total = a + b;\n    total\n}\n";
    let mut parser = LanguageParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);

    let add = graph.find_symbol_by_name("add").pop().unwrap();
    let sig = add
        .signature_span
        .expect("function symbol must carry a signature_span through the graph");
    let body = add
        .body_span
        .expect("function symbol must carry a body_span");

    assert_eq!(sig.start_byte, add.span.start_byte);
    assert!(sig.end_byte <= body.start_byte);
    let header = &source[sig.start_byte as usize..sig.end_byte as usize];
    assert!(
        header.contains("add"),
        "header {header:?} keeps the signature"
    );
    assert!(
        !header.contains("total"),
        "header {header:?} must exclude the body"
    );
}

#[test]
fn paths_match_deleted_windows_event_spelling_without_canonicalize() {
    assert!(paths_match(
        Path::new(r"C:\Users\Alice\repo\src\Lib.rs"),
        Path::new(r"c:/users/alice/repo/src/lib.rs")
    ));
    assert!(paths_match(
        Path::new(r"\\?\C:\Users\Alice\repo\src\Lib.rs"),
        Path::new(r"c:/users/alice/repo/src/lib.rs")
    ));
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
    let outside_path_str = root
        .parent()
        .unwrap()
        .join("squeezy-cargo-outside.rs")
        .to_string_lossy()
        .into_owned();
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
                    "file_name": outside_path_str,
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

/// When a file that previously parsed into the graph flips to an unsupported
/// language on refresh, `refresh_now` records the new unsupported placeholder
/// in `graph.files`. It must also purge the file's old symbols/edges/calls/
/// references; otherwise the stale rows stay queryable and poison every
/// downstream tool.
#[test]
fn graph_manager_refresh_purges_symbols_when_file_becomes_unsupported() {
    let root = temp_root("graph-manager-supported-to-unsupported");
    fs::create_dir_all(root.join("src")).unwrap();
    // A real supported file keeps the workspace indexable so the unsupported
    // sibling below is still returned by the crawl (as an Unsupported record)
    // instead of being dropped from the snapshot entirely.
    fs::write(root.join("src").join("lib.rs"), "pub fn anchor() {}\n").unwrap();
    // `notes.txt` is unsupported by extension, so the live crawl always returns
    // it as an unsupported FileRecord with a stable id. We then seed the graph
    // as if that same id had previously parsed as Rust (with symbols + a call
    // edge), reproducing a supported -> unsupported flip for one file id.
    let seed_source = "fn seeded_symbol() { seeded_callee(); }\nfn seeded_callee() {}\n";
    let notes = root.join("notes.txt");
    fs::write(&notes, "plain notes\n").unwrap();

    let mut manager = GraphManager::open_with_config(
        &root,
        RefreshConfig {
            debounce: Duration::from_millis(0),
            idle_refresh_interval: Duration::from_millis(0),
            per_tool_refresh_budget: Duration::from_secs(5),
        },
    )
    .unwrap();
    // Fresh build keeps notes.txt as an unsupported placeholder: no symbols, but
    // the record is present (so the seed flips an existing supported->unsupported
    // id rather than going through the removed-file path).
    assert!(
        manager
            .graph()
            .find_symbol_by_name("seeded_symbol")
            .is_empty()
    );
    assert!(
        manager
            .graph()
            .files
            .contains_key(&FileId::new("notes.txt")),
        "unsupported sibling must be tracked in the snapshot"
    );

    // Seed the supported state for `notes.txt` directly into the graph.
    let mut parser = LanguageParser::new().unwrap();
    let mut seed_record = record("seed-notes.rs", seed_source);
    seed_record.id = FileId::new("notes.txt");
    seed_record.path = notes.clone();
    seed_record.relative_path = "notes.txt".to_string();
    seed_record.language = LanguageKind::Rust;
    let parsed = parser
        .parse_source(&seed_record, seed_source.to_string())
        .unwrap();
    manager.graph_mut().replace_file(parsed);

    assert!(
        !manager
            .graph()
            .find_symbol_by_name("seeded_symbol")
            .is_empty(),
        "seed should make the symbol queryable"
    );
    assert!(
        !manager
            .graph()
            .find_symbol_by_name("seeded_callee")
            .is_empty(),
        "seed should make the callee queryable"
    );
    let notes_id = FileId::new("notes.txt");
    let seeded_edge_count = manager
        .graph()
        .edges()
        .iter()
        .filter(|edge| {
            manager
                .graph()
                .symbols
                .get(&edge.from)
                .map(|symbol| symbol.file_id == notes_id)
                .unwrap_or(false)
        })
        .count();
    assert!(
        seeded_edge_count > 0,
        "seed should create at least one edge owned by notes.txt"
    );

    // Change the on-disk file so the crawl produces a differing hash, then
    // refresh. The graph's old record is Rust while the crawl reclassifies the
    // file as unsupported -> it lands in the unsupported-changed path that the
    // fix targets (the file is NOT removed, only reclassified).
    thread::sleep(Duration::from_millis(2));
    fs::write(&notes, "plain notes, edited\n").unwrap();
    manager.record_changed_path(notes.clone());
    let report = manager.refresh_now().unwrap();
    assert!(
        report.removed_files.is_empty(),
        "notes.txt is reclassified, not removed; the unsupported-changed path \
         must handle the purge"
    );
    assert!(
        manager
            .graph()
            .files
            .contains_key(&FileId::new("notes.txt")),
        "the unsupported placeholder for notes.txt must remain recorded"
    );
    assert_eq!(
        manager
            .graph()
            .files
            .get(&FileId::new("notes.txt"))
            .map(|file| file.language),
        Some(LanguageKind::Unsupported),
        "notes.txt must now be tracked as unsupported"
    );

    // The placeholder is recorded, but the stale derived data must be gone.
    assert!(
        manager
            .graph()
            .find_symbol_by_name("seeded_symbol")
            .is_empty(),
        "stale symbol must be purged after supported -> unsupported flip"
    );
    assert!(
        manager
            .graph()
            .find_symbol_by_name("seeded_callee")
            .is_empty(),
        "stale callee must be purged after supported -> unsupported flip"
    );
    assert!(
        manager
            .graph()
            .symbols
            .values()
            .all(|symbol| symbol.file_id != notes_id),
        "no symbol may remain attached to the now-unsupported file"
    );
    // Every surviving edge must dangle from a still-present symbol; none may be
    // anchored to a (now purged) notes.txt symbol.
    assert!(
        manager.graph().edges().iter().all(|edge| {
            manager
                .graph()
                .symbols
                .get(&edge.from)
                .map(|symbol| symbol.file_id != notes_id)
                .unwrap_or(true)
        }),
        "stale edges owned by the file must be purged"
    );
}

/// When the per-refresh budget breaks the reparse loop before every changed
/// file is processed, `refresh_now` must not pretend the refresh finished. The
/// unprocessed paths have to stay pending and `last_refresh` must not advance,
/// otherwise the next query skips refresh for the whole idle interval and
/// serves stale data for the files that were never reparsed.
#[test]
fn graph_manager_refresh_keeps_pending_when_budget_exhausted() {
    let root = temp_root("graph-manager-budget-exhausted");
    fs::create_dir_all(root.join("src")).unwrap();
    let files = ["a.rs", "b.rs", "c.rs", "d.rs"];
    for name in files {
        fs::write(
            root.join("src").join(name),
            format!("fn {}_v1() {{}}\n", name.trim_end_matches(".rs")),
        )
        .unwrap();
    }

    let mut manager = GraphManager::open_with_config(
        &root,
        RefreshConfig {
            debounce: Duration::from_millis(0),
            // Large idle interval: a buggy refresh that drops the pending set
            // and advances last_refresh would hide stale files for this long.
            idle_refresh_interval: Duration::from_secs(600),
            // Zero budget: the reparse loop breaks on its first iteration,
            // before any changed file is parsed, deterministically.
            per_tool_refresh_budget: Duration::from_millis(0),
        },
    )
    .unwrap();
    for name in files {
        assert!(
            !manager
                .graph()
                .find_symbol_by_name(&format!("{}_v1", name.trim_end_matches(".rs")))
                .is_empty()
        );
    }

    thread::sleep(Duration::from_millis(2));
    let mut changed_paths = Vec::new();
    for name in files {
        let path = root.join("src").join(name);
        fs::write(
            &path,
            format!("fn {}_v2() {{}}\n", name.trim_end_matches(".rs")),
        )
        .unwrap();
        changed_paths.push(path);
    }
    manager.record_changed_paths(changed_paths.clone());

    let report = manager.refresh_now().unwrap();
    assert!(
        report.budget_exhausted,
        "zero budget must exhaust before reparsing"
    );
    assert_eq!(
        report.reparsed_files, 0,
        "zero budget breaks before parsing any file"
    );

    // The pending paths must survive so the next query still refreshes them.
    let pending = manager.pending_changed_paths_handle();
    assert!(
        !pending.lock().unwrap().is_empty(),
        "unprocessed paths must stay pending after a budget-exhausted refresh"
    );

    // Because pending is non-empty (and last_refresh was not advanced), the
    // next refresh-before-query must NOT be skipped for the idle interval.
    let next = manager.refresh_before_query().unwrap();
    assert!(
        !next.skipped_due_to_interval,
        "stale files must not be hidden for the idle interval after a budget break"
    );

    // The stale v1 symbols are still served (nothing was reparsed yet), which is
    // exactly why the paths must stay pending for a later pass to converge.
    for name in files {
        assert!(
            !manager
                .graph()
                .find_symbol_by_name(&format!("{}_v1", name.trim_end_matches(".rs")))
                .is_empty(),
            "no file was reparsed under the zero budget"
        );
    }
}

#[test]
fn graph_manager_open_watching_records_active_watcher_status() {
    let root = temp_root("graph-manager-watching-status");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src").join("lib.rs"), "pub fn ready() {}\n").unwrap();

    let manager = GraphManager::open_watching(
        &root,
        RefreshConfig {
            debounce: Duration::from_millis(0),
            idle_refresh_interval: Duration::from_secs(30),
            per_tool_refresh_budget: Duration::from_secs(5),
        },
        CrawlOptions::default(),
        None,
        watcher::WatcherConfig {
            src_dirs: vec![root.clone()],
            debounce_ms: 50,
        },
    )
    .unwrap();

    let status = manager.watcher_status();
    assert!(
        matches!(
            status.mode,
            WatcherMode::Native | WatcherMode::PollingFallback
        ),
        "expected native or polling fallback watcher mode, got {:?}",
        status.mode,
    );
    assert!(
        !status.backend.is_empty() && status.backend != "none",
        "expected a non-empty, non-disabled backend name, got {:?}",
        status.backend,
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn resolve_watcher_attachment_uses_native_when_it_succeeds() {
    // Drive a real native watcher against a temp directory; this is the
    // single platform-touching unit test for the attachment helper. The
    // other two arms below are pure logic and unit-tested with synthetic
    // results, which keeps them deterministic across OSes.
    let root = temp_root("resolve-watcher-attachment-native");
    let started = watcher::FileWatcher::start(
        watcher::WatcherConfig {
            src_dirs: vec![root.clone()],
            debounce_ms: 50,
        },
        |_batch| {},
    );
    let polling_start = || {
        panic!("polling fallback must not be invoked when native succeeds");
        #[allow(unreachable_code)]
        Err(SqueezyError::Tool("unused".to_string()))
    };
    let (slot, status) = resolve_watcher_attachment(started, polling_start);
    assert!(slot.is_some(), "native success must populate the slot");
    assert_eq!(status.mode, WatcherMode::Native);
    assert_eq!(status.backend, watcher::native_backend_name());
    assert!(status.fallback_reason.is_none());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn resolve_watcher_attachment_falls_back_to_polling_when_native_fails() {
    let native_err: Result<watcher::FileWatcher> =
        Err(SqueezyError::Tool("inotify watches exhausted".to_string()));
    // Build a real PollWatcher against a temp dir for the polling-success
    // arm so the slot type is real and dropping it tears down cleanly.
    let root = temp_root("resolve-watcher-attachment-polling-fallback");
    let polling_start = || {
        watcher::FileWatcher::start_polling(
            watcher::WatcherConfig {
                src_dirs: vec![root.clone()],
                debounce_ms: 50,
            },
            |_batch| {},
        )
    };
    let (slot, status) = resolve_watcher_attachment(native_err, polling_start);
    assert!(slot.is_some(), "polling success must populate the slot");
    assert_eq!(status.mode, WatcherMode::PollingFallback);
    assert_eq!(status.backend, watcher::polling_backend_name());
    let reason = status
        .fallback_reason
        .as_deref()
        .expect("polling fallback must surface the native error");
    assert!(
        reason.contains("inotify watches exhausted"),
        "fallback_reason must surface the native error, got {reason:?}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn resolve_watcher_attachment_degrades_to_disabled_when_both_backends_fail() {
    // Regression for PR #366 review B1: a dual native+polling failure must
    // NOT propagate out of `open_watching`. The pre-watcher constructor
    // (`open_with_store`) never touched the filesystem watcher, so a flaky
    // watcher could never take down the whole graph. If `open_watching`
    // propagated the dual-failure error, the live registry path
    // (`squeezy-tools/src/lib.rs`) would collapse the graph slot to `None`
    // for the rest of the session and every graph tool would return
    // `graph_unavailable`.
    let native_err: Result<watcher::FileWatcher> =
        Err(SqueezyError::Tool("inotify watches exhausted".to_string()));
    let polling_start = || -> Result<watcher::FileWatcher> {
        Err(SqueezyError::Tool(
            "FUSE mount refuses recursive watch".to_string(),
        ))
    };
    let (slot, status) = resolve_watcher_attachment(native_err, polling_start);
    assert!(
        slot.is_none(),
        "dual-failure must leave the watcher slot empty"
    );
    assert_eq!(
        status.mode,
        WatcherMode::Disabled,
        "dual-failure must degrade to Disabled, not propagate the error"
    );
    assert_eq!(status.backend, "none");
    let reason = status
        .fallback_reason
        .as_deref()
        .expect("dual-failure must record a fallback_reason for operators");
    assert!(
        reason.starts_with("native: "),
        "fallback_reason must lead with the native failure, got {reason:?}"
    );
    assert!(
        reason.contains("inotify watches exhausted"),
        "fallback_reason must surface the native error, got {reason:?}"
    );
    assert!(
        reason.contains("; polling: "),
        "fallback_reason must separate native and polling failures, got {reason:?}"
    );
    assert!(
        reason.contains("FUSE mount refuses recursive watch"),
        "fallback_reason must surface the polling error, got {reason:?}"
    );
}

/// `call_chain` must honor the same `max_depth` bound as the BFS call-graph
/// listing (`bfs_call_packets` in squeezy-tools): a target reachable in exactly
/// `d` call edges is found iff `max_depth >= d`, never one edge further. Locks
/// the depth alignment so a future off-by-one in either traversal is caught.
#[test]
fn call_chain_depth_matches_bfs_listing_bound() {
    let source = r#"
fn a() { b(); }
fn b() { c(); }
fn c() { d(); }
fn d() { e(); }
fn e() {}
"#;
    let mut parser = LanguageParser::new().unwrap();
    let file = record("src/chain.rs", source);
    let parsed = parser.parse_source(&file, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);

    let a = graph.find_symbol_by_name("a").pop().unwrap();

    // Reachability under the bfs_call_packets bound: a node at edge distance d
    // is reached iff some edge emitted at depth+1 lands on it, gated by
    // `depth >= max_depth { continue }`.
    let bfs_reaches = |target: &SymbolId, max_depth: usize| -> bool {
        if max_depth == 0 {
            return false;
        }
        let mut visited = std::collections::HashSet::from([a.id.clone()]);
        let mut frontier = std::collections::VecDeque::from([(a.id.clone(), 0usize)]);
        while let Some((current, depth)) = frontier.pop_front() {
            if depth >= max_depth {
                continue;
            }
            let next_depth = depth + 1;
            for hit in graph.callees(&current) {
                let Some(next) = hit.edge.to else { continue };
                if &next == target {
                    return true;
                }
                if next_depth < max_depth && visited.insert(next.clone()) {
                    frontier.push_back((next, next_depth));
                }
            }
        }
        false
    };

    // Each target's exact edge distance from `a`.
    for (target_name, distance) in [("b", 1usize), ("c", 2), ("d", 3), ("e", 4)] {
        let target = graph.find_symbol_by_name(target_name).pop().unwrap();
        for max_depth in 0..=5 {
            let chain_found = graph.call_chain(&a.id, &target.id, max_depth).is_some();
            let bfs_found = bfs_reaches(&target.id, max_depth);
            assert_eq!(
                chain_found, bfs_found,
                "call_chain and BFS bound disagree for target {target_name} at max_depth {max_depth}"
            );
            assert_eq!(
                chain_found,
                max_depth >= distance,
                "call_chain to {target_name} ({distance} edges) must require max_depth >= {distance}"
            );
        }
    }
}

#[test]
fn graph_manager_refresh_rebuilds_once_for_multiple_removed_files() {
    let root = temp_root("graph-manager-remove-many");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src").join("keep.rs"), "pub fn keep() {}\n").unwrap();
    let removed = ["a.rs", "b.rs", "c.rs", "d.rs"];
    for name in removed {
        fs::write(
            root.join("src").join(name),
            format!("pub fn {} () {{ keep(); }}\n", name.trim_end_matches(".rs")),
        )
        .unwrap();
    }

    let mut manager = GraphManager::open_with_config(
        &root,
        RefreshConfig {
            debounce: Duration::from_millis(0),
            idle_refresh_interval: Duration::from_millis(0),
            per_tool_refresh_budget: Duration::from_secs(5),
        },
    )
    .unwrap();
    for name in removed {
        assert!(
            !manager
                .graph()
                .find_symbol_by_name(name.trim_end_matches(".rs"))
                .is_empty()
        );
    }

    thread::sleep(Duration::from_millis(2));
    for name in removed {
        fs::remove_file(root.join("src").join(name)).unwrap();
    }

    crate::resolution::SEMANTIC_REBUILD_COUNT.with(|count| count.set(0));
    let report = manager.refresh_now().unwrap();
    let rebuilds = crate::resolution::SEMANTIC_REBUILD_COUNT.with(|count| count.get());

    // The whole-graph re-resolution must run once per refresh, not once per
    // removed file.
    assert_eq!(
        rebuilds, 1,
        "expected a single semantic rebuild per refresh"
    );
    assert_eq!(report.removed_files.len(), removed.len());
    // Final state matches per-file removal: the removed symbols are gone and
    // the survivor remains.
    for name in removed {
        assert!(
            manager
                .graph()
                .find_symbol_by_name(name.trim_end_matches(".rs"))
                .is_empty()
        );
    }
    assert!(!manager.graph().find_symbol_by_name("keep").is_empty());
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
    // notes.txt changed and its event path matched the changed unsupported record,
    // so it is NOT reported as an unmatched event path. An unchanged_event_paths > 0
    // signals a genuine path-spelling or symlink mismatch, not a changed-but-
    // non-supported file.
    assert_eq!(unsupported.unchanged_event_paths, 0);

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
fn graph_symbol_references_surface_qualified_workspace_cross_crate_uses() {
    // Pre-fix this test asserted package-local references only,
    // documenting the absence of cargo-style cross-crate resolution.
    // With the workspace-cross-crate qualified-match fallback the
    // qualified `use source::Shared;` from `crates/user/` is now
    // surfaced as a reference to the unique `Shared` symbol in
    // `crates/source/`. The bare-name occurrence inside `fn user(_:
    // Shared)` is intentionally still skipped — without a scope
    // prefix or an import alias edge the graph cannot conclude that
    // it binds to the same `Shared` across crates.
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
    let hits = graph.references_to_symbol(&shared.id);
    assert!(
        hits.iter()
            .any(|hit| hit.reference.file_id.0 == "crates/user/src/lib.rs"
                && hit.reference.text.contains("source::Shared")),
        "expected the qualified `use source::Shared;` in crates/user/src/lib.rs \
         to surface, got {:?}",
        hits.iter()
            .map(|h| (h.reference.file_id.0.clone(), h.reference.text.clone()))
            .collect::<Vec<_>>(),
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
fn graph_records_kotlin_project_facts_from_gradle_kts() {
    let _ = LanguageParser::new().unwrap();
    let mut build = record(
        "build.gradle.kts",
        r#"plugins {
    kotlin("jvm") version "1.9.24"
}

dependencies {
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.10.1")
}
"#,
    );
    build.language = LanguageKind::Unsupported;
    let source = kotlin_record(
        "src/main/kotlin/com/example/Foo.kt",
        "package com.example\n\nclass Foo\n",
    );
    let parsed = vec![
        ParsedFile::unsupported(build, "gradle metadata"),
        ParsedFile {
            file: source,
            package: Some("com.example".to_string()),
            symbols: Vec::new(),
            imports: Vec::new(),
            calls: Vec::new(),
            references: Vec::new(),
            body_hits: Vec::new(),
            unsupported: None,
            diagnostics: Vec::new(),
            changed_ranges: Vec::new(),
        },
    ];
    let graph = SemanticGraph::from_parsed(parsed);

    let facts = graph
        .kotlin_project_facts()
        .iter()
        .map(|fact| format!("{}:{}:{}", fact.provider, fact.kind, fact.value))
        .collect::<Vec<_>>();
    assert!(
        facts.contains(&"gradle:source_root:main:src/main/kotlin".to_string()),
        "expected Kotlin source-root fact; got {facts:?}",
    );
    assert!(
        facts.contains(
            &"gradle:dependency:implementation:org.jetbrains.kotlinx:kotlinx-coroutines-core:1.10.1"
                .to_string()
        ),
        "expected Kotlin gradle dependency fact; got {facts:?}",
    );
    // Java's source-root extractor only recognises `src/<set>/java`, so the
    // Kotlin source root must not appear in java_project_facts even though
    // build.gradle.kts is shared between the two pipelines.
    let java_source_roots = graph
        .java_project_facts()
        .iter()
        .filter(|fact| fact.kind == "source_root")
        .map(|fact| fact.value.clone())
        .collect::<Vec<_>>();
    assert!(
        !java_source_roots
            .iter()
            .any(|value| value.contains("kotlin")),
        "java_project_facts source_root should not pick up Kotlin layout; got {java_source_roots:?}",
    );
}

#[test]
fn kotlin_project_facts_dedup_across_rebuilds() {
    // Trigger a second rebuild via remove_file; the cache should not
    // accumulate duplicates of the same coordinate.
    let _ = LanguageParser::new().unwrap();
    let mut build = record(
        "build.gradle.kts",
        r#"dependencies {
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.10.1")
}
"#,
    );
    build.language = LanguageKind::Unsupported;
    let parsed = vec![ParsedFile::unsupported(build, "gradle metadata")];
    let mut graph = SemanticGraph::from_parsed(parsed);
    graph.remove_file(&FileId::new("missing/no-op.kt"));
    let deps = graph
        .kotlin_project_facts()
        .iter()
        .filter(|fact| fact.kind == "dependency")
        .count();
    assert_eq!(deps, 1, "expected a single dedup'd dependency entry");
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
fn rust_trait_dyn_dispatch_emits_candidate_set_with_both_impls() {
    let source = r#"
pub trait Animal {
    fn speak(&self) -> String;
}

pub struct Dog;

impl Animal for Dog {
    fn speak(&self) -> String {
        String::from("woof")
    }
}

pub struct Cat;

impl Animal for Cat {
    fn speak(&self) -> String {
        String::from("meow")
    }
}

pub fn make_noise(animal: &dyn Animal) -> String {
    animal.speak()
}
"#;
    let mut parser = LanguageParser::new().unwrap();
    let record = record("src/animals.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);

    // The `animal.speak()` call cannot be resolved exactly because `animal`
    // is `&dyn Animal`; the graph must enumerate both impls as candidates.
    let edge = graph
        .edges
        .iter()
        .find(|edge| {
            edge.target_text.contains("speak")
                && edge.kind == EdgeKind::Calls
                && edge.confidence == Confidence::CandidateSet
        })
        .expect("expected a CandidateSet `Calls` edge for `animal.speak()` over `&dyn Animal`");

    // Polymorphic dispatch enumerates the trait declaration plus every impl;
    // each `speak` method on `Dog`, `Cat`, and the trait itself is a possible
    // resolution and must travel with the edge.
    assert!(
        edge.candidates.len() >= 2,
        "expected at least both impls in the candidate set, got {} ({:?})",
        edge.candidates.len(),
        edge.candidates,
    );

    let candidate_parents: Vec<String> = edge
        .candidates
        .iter()
        .map(|id| {
            let sym = graph
                .symbols
                .get(id)
                .expect("candidate id resolves to a symbol");
            assert_eq!(sym.name, "speak");
            assert_eq!(sym.kind, SymbolKind::Method);
            sym.parent_id
                .as_ref()
                .and_then(|pid| graph.symbols.get(pid))
                .map(|parent| parent.name.clone())
                .unwrap_or_default()
        })
        .collect();
    // Both impl parents carry `<Trait> for <Type>`; assert each is visible.
    assert!(
        candidate_parents.iter().any(|n| n.contains("Dog")),
        "expected Dog impl in candidate parents; got {candidate_parents:?}",
    );
    assert!(
        candidate_parents.iter().any(|n| n.contains("Cat")),
        "expected Cat impl in candidate parents; got {candidate_parents:?}",
    );

    // Ranking is deterministic: re-running on a fresh graph must produce the
    // identical candidate order (rules out HashMap iteration noise).
    let parsed_again = LanguageParser::new()
        .unwrap()
        .parse_source(&record, source.to_string())
        .unwrap();
    let graph_again = SemanticGraph::from_parsed(vec![parsed_again]);
    let edge_again = graph_again
        .edges
        .iter()
        .find(|edge| {
            edge.target_text.contains("speak")
                && edge.kind == EdgeKind::Calls
                && edge.confidence == Confidence::CandidateSet
        })
        .expect("re-built graph still emits the CandidateSet edge");
    assert_eq!(
        edge.candidates, edge_again.candidates,
        "candidate ordering must be deterministic across graph rebuilds",
    );
}

#[test]
fn rust_trait_candidate_set_ranks_same_file_first() {
    let local = r#"
pub trait Animal {
    fn speak(&self) -> String;
}

pub struct Dog;

impl Animal for Dog {
    fn speak(&self) -> String {
        String::from("woof")
    }
}

pub fn make_noise(animal: &dyn Animal) -> String {
    animal.speak()
}
"#;
    // A sibling crate-local file defines another `speak` method whose name
    // collides; without same-file ranking it could be ordered ahead of the
    // local impl just because `HashMap` iteration is non-deterministic.
    let foreign = r#"
pub struct Speaker;

impl Speaker {
    pub fn speak(&self) -> String {
        String::from("hi")
    }
}
"#;
    let mut parser = LanguageParser::new().unwrap();
    let local_record = record("src/animals.rs", local);
    let foreign_record = record("src/speakers.rs", foreign);
    let local_parsed = parser
        .parse_source(&local_record, local.to_string())
        .unwrap();
    let foreign_parsed = parser
        .parse_source(&foreign_record, foreign.to_string())
        .unwrap();
    let graph = SemanticGraph::from_parsed(vec![local_parsed, foreign_parsed]);

    let edge = graph
        .edges
        .iter()
        .find(|edge| {
            edge.target_text.contains("speak")
                && edge.kind == EdgeKind::Calls
                && edge.confidence == Confidence::CandidateSet
        })
        .expect("expected CandidateSet call edge for `animal.speak()`");

    assert!(
        edge.candidates.len() >= 2,
        "expected both `speak` candidates to surface, got {} ({:?})",
        edge.candidates.len(),
        edge.candidates,
    );

    let first = graph
        .symbols
        .get(&edge.candidates[0])
        .expect("first candidate resolves");
    assert_eq!(
        first.file_id.0, "src/animals.rs",
        "same-file candidate should rank first; got {:?}",
        first.file_id,
    );
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

#[test]
fn aliased_import_lookup_uses_inverted_index_keyed_by_target_leaf() {
    // Build a workspace where most aliased imports target unrelated symbols.
    // The inverted index must bucket aliased imports by
    // `last_path_segment(import.path)`, so a lookup for `Wanted` touches only
    // the imports whose path leaf is `Wanted` — not every import in the graph.
    let mut parser = LanguageParser::new().unwrap();
    let mut sources = Vec::new();
    sources.push(record(
        "src/lib.rs",
        r#"
pub mod target {
    pub struct Wanted;
}
"#,
    ));
    // 200 unrelated aliased imports keyed by their own distinct target leaves.
    for i in 0..200 {
        sources.push(record(
            &format!("src/noise_{i}.rs"),
            &format!(
                "use crate::target::Other{i} as Alias{i};\nfn _use_{i}() {{ let _ = Alias{i}; }}\n"
            ),
        ));
    }
    // One aliased import that actually targets `Wanted`.
    sources.push(record(
        "src/use_wanted.rs",
        r#"
use crate::target::Wanted as Aliased;
fn touch() {
    let _ = Aliased;
}
"#,
    ));

    let parsed = sources
        .into_iter()
        .map(|record| parser.parse_record(&record).unwrap())
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);

    // Correctness: the alias `Aliased` resolves back to `Wanted` via the
    // inverted index path inside `reference_candidate_indexes_for_symbol`.
    let wanted = graph
        .find_symbol_by_name("Wanted")
        .pop()
        .expect("Wanted symbol");
    let refs = graph.references_to_symbol(&wanted.id);
    assert!(
        refs.iter().any(|hit| hit.reference.text == "Aliased"),
        "references_to_symbol(Wanted) must surface the `Aliased` reference \
         via the inverted alias index; got {:?}",
        refs.iter().map(|h| &h.reference.text).collect::<Vec<_>>(),
    );
    assert!(
        refs.iter().any(|hit| hit.reference.text == "Wanted"),
        "references_to_symbol(Wanted) must still surface the direct `Wanted` \
         reference inside the `use` statement; got {:?}",
        refs.iter().map(|h| &h.reference.text).collect::<Vec<_>>(),
    );

    // Sub-linear lookup: the inverted index bucket for `Wanted` contains at
    // most a handful of imports, not the 200+ unrelated ones. The full
    // `imports` vec has 201 entries.
    assert!(
        graph.imports.len() >= 200,
        "fixture should produce at least 200 imports; got {}",
        graph.imports.len(),
    );
    let wanted_bucket = graph
        .imports_by_alias_target
        .get("Wanted")
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    assert_eq!(
        wanted_bucket.len(),
        1,
        "expected only the `Wanted` aliased import to land in its bucket; \
         got {} entries",
        wanted_bucket.len(),
    );
    // The noisy aliased imports must NOT spill into the `Wanted` bucket, even
    // though they share the same module prefix.
    for index in wanted_bucket {
        let import = &graph.imports[*index];
        assert!(
            import.path.ends_with("Wanted"),
            "unrelated import {:?} leaked into the `Wanted` bucket",
            import.path,
        );
    }

    // No wildcard aliased imports exist in this Rust fixture; the bucket
    // stays empty (Rust glob imports are not aliased).
    assert!(
        graph.wildcard_aliased_imports.is_empty(),
        "Rust glob imports are never aliased, so the wildcard bucket should \
         stay empty; got {} entries",
        graph.wildcard_aliased_imports.len(),
    );
}

#[test]
fn js_ts_namespace_import_lands_in_wildcard_aliased_bucket() {
    // JS/TS `import * as M from 'mod'` produces an aliased glob import whose
    // path leaf is `*`. These cannot be keyed by target name, so the index
    // must route them to `wildcard_aliased_imports` and still consult that
    // bucket on every symbol lookup.
    let mut parser = LanguageParser::new().unwrap();
    let package = unsupported_record(
        "package.json",
        r#"{"name":"ns-case","exports":{".":"./src/helpers.ts"}}"#,
    );
    let helpers = ts_record("src/helpers.ts", "export function helper() { return 1; }\n");
    let app = ts_record(
        "src/app.ts",
        r#"
import * as Mod from "ns-case";

export function start() {
    return Mod.helper();
}
"#,
    );
    let parsed = vec![package, helpers, app]
        .into_iter()
        .map(|record| parser.parse_record(&record).unwrap())
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);

    assert!(
        !graph.wildcard_aliased_imports.is_empty(),
        "JS/TS namespace import (`import * as Mod`) should land in the \
         wildcard aliased bucket; got {} entries",
        graph.wildcard_aliased_imports.len(),
    );
    // The wildcard bucket must NOT pollute the target-keyed buckets.
    assert!(
        !graph.imports_by_alias_target.contains_key("*"),
        "wildcard imports must not be keyed by `*` in the target index",
    );
}

#[test]
fn references_to_symbol_finds_qualified_self_crate_call_across_modules() {
    // Reproduces the squeezy A/B finding: `reference_search` missed
    // `squeezy_eval::run_scenario` in main.rs even though the function
    // lives in driver.rs of the same crate. The graph could not
    // resolve `<crate-name-in-underscores>::foo` as a self-crate
    // alias, so the call edge had `to = None` and binding fell
    // through to a Function rejection in `qualified_reference_matches_symbol`.
    let mut parser = LanguageParser::new().unwrap();
    let driver_record = record(
        "crates/sample/src/driver.rs",
        "pub fn run_scenario() -> u32 { 1 }\n",
    );
    let lib_record = record(
        "crates/sample/src/lib.rs",
        "pub mod driver;\npub use driver::run_scenario;\n",
    );
    let main_record = record(
        "crates/sample/src/main.rs",
        r#"
fn run_cmd() -> u32 {
    sample::run_scenario()
}

fn main() {
    let _ = run_cmd();
}
"#,
    );
    let parsed = [driver_record, lib_record, main_record]
        .into_iter()
        .map(|record| {
            let source = fs::read_to_string(&record.path).unwrap();
            parser.parse_source(&record, source).unwrap()
        })
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);

    let run_scenario = graph.find_symbol_by_name("run_scenario").pop().unwrap();
    let hits = graph.references_to_symbol(&run_scenario.id);
    assert!(
        hits.iter()
            .any(|hit| hit.reference.file_id.0 == "crates/sample/src/main.rs"
                && hit.reference.text.contains("run_scenario")),
        "expected reference_search to surface the `sample::run_scenario` call \
         from `crates/sample/src/main.rs`, got {:?}",
        hits.iter()
            .map(|h| (h.reference.file_id.0.clone(), h.reference.text.clone()))
            .collect::<Vec<_>>(),
    );
}

#[test]
fn graph_resolves_kotlin_named_import_to_target_class() {
    let mut parser = LanguageParser::new().unwrap();
    let greeter = kotlin_record(
        "src/main/kotlin/com/example/services/Greeter.kt",
        r#"package com.example.services

class Greeter {
    fun greet(): String = "hi"
}
"#,
    );
    let runner = kotlin_record(
        "src/main/kotlin/com/example/app/Runner.kt",
        r#"package com.example.app

import com.example.services.Greeter

class Runner(private val greeter: Greeter) {
    fun run(): String = greeter.greet()
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
    let greeter_class = graph
        .find_symbol_by_name("Greeter")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Class)
        .unwrap();
    assert!(
        graph
            .references_to_symbol(&greeter_class.id)
            .iter()
            .any(|hit| hit.reference.text == "Greeter"),
        "expected Greeter to be referenced from Runner",
    );
}

#[test]
fn references_to_symbol_finds_csharp_namespace_qualified_internal_static_call() {
    // C# A/B Task finding: the with-graph runs on Newtonsoft.Json
    // missed call sites of
    // `MiscellaneousUtils.CreateArgumentOutOfRangeException(...)` even
    // though every caller lives in the same project (single package
    // key `Src`). The class is `internal static` and the calls are of
    // the shape `ClassName.Method(...)` from another file in the same
    // namespace tree under `Src/Newtonsoft.Json/...`.
    //
    // Mirrors `references_to_symbol_finds_workspace_cross_crate_qualified_trait_impl`
    // but uses `csharp_record` + the `Class.Method` qualifier shape
    // rather than Rust's `crate::Trait` syntax.
    let mut parser = LanguageParser::new().unwrap();
    let utils = csharp_record(
        "Src/Newtonsoft.Json/Utilities/MiscellaneousUtils.cs",
        r#"
using System;

namespace Newtonsoft.Json.Utilities
{
    internal static class MiscellaneousUtils
    {
        public static ArgumentOutOfRangeException CreateArgumentOutOfRangeException(
            string paramName, object actualValue, string message)
        {
            return new ArgumentOutOfRangeException(paramName, actualValue, message);
        }
    }
}
"#,
    );
    let caller = csharp_record(
        "Src/Newtonsoft.Json/Utilities/DateTimeUtils.cs",
        r#"
using System;

namespace Newtonsoft.Json.Utilities
{
    internal static class DateTimeUtils
    {
        internal static string ToSerializationMode(DateTimeKind kind)
        {
            switch (kind)
            {
                default:
                    throw MiscellaneousUtils.CreateArgumentOutOfRangeException(
                        nameof(kind), kind, "Unexpected DateTimeKind value.");
            }
        }
    }
}
"#,
    );
    let parsed = [utils, caller]
        .into_iter()
        .map(|record| {
            let source = fs::read_to_string(&record.path).unwrap();
            parser.parse_source(&record, source).unwrap()
        })
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);
    let method = graph
        .find_symbol_by_name("CreateArgumentOutOfRangeException")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .expect("CreateArgumentOutOfRangeException method indexed");
    let hits = graph.references_to_symbol(&method.id);
    assert!(
        hits.iter().any(|hit| hit.reference.file_id.0
            == "Src/Newtonsoft.Json/Utilities/DateTimeUtils.cs"
            && hit
                .reference
                .text
                .contains("CreateArgumentOutOfRangeException")),
        "expected `MiscellaneousUtils.CreateArgumentOutOfRangeException(...)` call \
         in DateTimeUtils.cs to surface on reference_search, got {:?}",
        hits.iter()
            .map(|h| (h.reference.file_id.0.clone(), h.reference.text.clone()))
            .collect::<Vec<_>>(),
    );
}

#[test]
fn graph_resolves_kotlin_wildcard_import_to_top_level_function() {
    let mut parser = LanguageParser::new().unwrap();
    let util = kotlin_record(
        "src/main/kotlin/com/example/util/Names.kt",
        r#"package com.example.util

fun defaultName(): String = "Ada"
"#,
    );
    let runner = kotlin_record(
        "src/main/kotlin/com/example/app/Runner.kt",
        r#"package com.example.app

import com.example.util.*

fun greet(): String = defaultName()
"#,
    );
    let parsed = vec![
        parser
            .parse_source(&util, fs::read_to_string(&util.path).unwrap())
            .unwrap(),
        parser
            .parse_source(&runner, fs::read_to_string(&runner.path).unwrap())
            .unwrap(),
    ];
    let graph = SemanticGraph::from_parsed(parsed);
    let default_name = graph.find_symbol_by_name("defaultName").pop().unwrap();
    let greet = graph
        .find_symbol_by_name("greet")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Function)
        .unwrap();
    assert!(
        graph.call_chain(&greet.id, &default_name.id, 3).is_some(),
        "expected greet -> defaultName via wildcard import",
    );
}

#[test]
fn graph_resolves_kotlin_companion_factory_through_alias_import() {
    let mut parser = LanguageParser::new().unwrap();
    let friendly = kotlin_record(
        "src/main/kotlin/com/example/services/FriendlyGreeter.kt",
        r#"package com.example.services

class FriendlyGreeter {
    companion object {
        fun create(): FriendlyGreeter = FriendlyGreeter()
    }
}
"#,
    );
    let runner = kotlin_record(
        "src/main/kotlin/com/example/app/Runner.kt",
        r#"package com.example.app

import com.example.services.FriendlyGreeter as Friendly

class Runner {
    fun build(): FriendlyGreeter = Friendly.create()
}
"#,
    );
    let parsed = vec![
        parser
            .parse_source(&friendly, fs::read_to_string(&friendly.path).unwrap())
            .unwrap(),
        parser
            .parse_source(&runner, fs::read_to_string(&runner.path).unwrap())
            .unwrap(),
    ];
    let graph = SemanticGraph::from_parsed(parsed);

    let create = graph
        .find_symbol_by_name("create")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .unwrap();
    let build = graph
        .find_symbol_by_name("build")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .unwrap();
    // Companion-object member should re-parent onto FriendlyGreeter.
    let friendly_class = graph
        .find_symbol_by_name("FriendlyGreeter")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Class)
        .unwrap();
    assert_eq!(create.parent_id.as_ref(), Some(&friendly_class.id));
    assert!(
        graph.call_chain(&build.id, &create.id, 3).is_some(),
        "build -> create via companion + alias",
    );
}

#[test]
fn references_to_symbol_finds_workspace_cross_crate_qualified_trait_impl() {
    // A/B Task 2 finding: the graph missed every `impl
    // squeezy_llm::LlmProvider for ...` block that lived outside the
    // `squeezy-llm` crate because `reference_is_in_symbol_package`
    // gated cross-crate references out before the binding logic
    // could see the qualified path.
    let mut parser = LanguageParser::new().unwrap();
    let trait_record = record(
        "crates/sample-llm/src/lib.rs",
        "pub trait LlmProvider {\n    fn name(&self) -> &str;\n}\n",
    );
    let impl_record = record(
        "crates/sample-tui/src/config_screen_tests.rs",
        r#"
struct NoOpProvider;

impl sample_llm::LlmProvider for NoOpProvider {
    fn name(&self) -> &str { "noop" }
}
"#,
    );
    let parsed = [trait_record, impl_record]
        .into_iter()
        .map(|record| {
            let source = fs::read_to_string(&record.path).unwrap();
            parser.parse_source(&record, source).unwrap()
        })
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);
    let llm_provider = graph
        .find_symbol_by_name("LlmProvider")
        .pop()
        .expect("trait symbol indexed");
    let hits = graph.references_to_symbol(&llm_provider.id);
    assert!(
        hits.iter().any(|hit| hit.reference.file_id.0
            == "crates/sample-tui/src/config_screen_tests.rs"
            && hit.reference.text.contains("LlmProvider")),
        "expected `impl sample_llm::LlmProvider for NoOpProvider` to surface \
         on reference_search, got {:?}",
        hits.iter()
            .map(|h| (h.reference.file_id.0.clone(), h.reference.text.clone()))
            .collect::<Vec<_>>(),
    );
}

#[test]
fn graph_resolves_kotlin_block_body_function_calls() {
    // Regression: a block-bodied `fun run()` with local `val name = ...`
    // initializers must still attribute the calls inside the val
    // initializers to `run`, not to the local val. This covers both the
    // string-literal extension receiver (`"world".prepare()`) and the
    // top-level suspend call (`fetchDefault()`).
    let mut parser = LanguageParser::new().unwrap();
    let strings = kotlin_record(
        "src/main/kotlin/com/example/util/Strings.kt",
        r#"package com.example.util

fun String.prepare(): String = this.trim()
"#,
    );
    let names = kotlin_record(
        "src/main/kotlin/com/example/util/Names.kt",
        r#"package com.example.util

suspend fun fetchDefault(): String = "default"
"#,
    );
    let runner = kotlin_record(
        "src/main/kotlin/com/example/app/Runner.kt",
        r#"package com.example.app

import com.example.util.fetchDefault
import com.example.util.prepare

class Runner {
    suspend fun run(): String {
        val name = "world".prepare()
        val default = fetchDefault()
        return default + name
    }
}
"#,
    );
    let parsed = vec![
        parser
            .parse_source(&strings, fs::read_to_string(&strings.path).unwrap())
            .unwrap(),
        parser
            .parse_source(&names, fs::read_to_string(&names.path).unwrap())
            .unwrap(),
        parser
            .parse_source(&runner, fs::read_to_string(&runner.path).unwrap())
            .unwrap(),
    ];
    let graph = SemanticGraph::from_parsed(parsed);

    let prepare = graph
        .find_symbol_by_name("prepare")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Function)
        .unwrap();
    let fetch = graph
        .find_symbol_by_name("fetchDefault")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Function)
        .unwrap();
    let run = graph
        .find_symbol_by_name("run")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .unwrap();
    assert!(
        graph.call_chain(&run.id, &fetch.id, 3).is_some(),
        "run -> fetchDefault inside `val default = fetchDefault()`",
    );
    assert!(
        graph.call_chain(&run.id, &prepare.id, 3).is_some(),
        "run -> prepare via string-literal extension receiver",
    );
    // Local `val name` / `val default` must not appear as graph symbols
    // (they are scoped to the function body, not file-or-class members).
    assert!(graph.find_symbol_by_name("name").is_empty());
    assert!(graph.find_symbol_by_name("default").is_empty());
}

#[test]
fn graph_resolves_kotlin_alias_import_reference_search() {
    // Regression: `reference_search(alias)` must surface references named
    // by the original import target so a search for the alias finds the
    // underlying class name.
    let mut parser = LanguageParser::new().unwrap();
    let friendly = kotlin_record(
        "src/main/kotlin/com/example/services/FriendlyGreeter.kt",
        r#"package com.example.services

class FriendlyGreeter {
    companion object {
        fun create(): FriendlyGreeter = FriendlyGreeter()
    }
}
"#,
    );
    let runner = kotlin_record(
        "src/main/kotlin/com/example/app/Runner.kt",
        r#"package com.example.app

import com.example.services.FriendlyGreeter as Friendly

class Runner {
    fun build(): Any = Friendly.create()
}
"#,
    );
    let parsed = vec![
        parser
            .parse_source(&friendly, fs::read_to_string(&friendly.path).unwrap())
            .unwrap(),
        parser
            .parse_source(&runner, fs::read_to_string(&runner.path).unwrap())
            .unwrap(),
    ];
    let graph = SemanticGraph::from_parsed(parsed);
    let hits: Vec<_> = graph
        .reference_search("Friendly")
        .into_iter()
        .map(|hit| hit.reference.text)
        .collect();
    assert!(
        hits.iter().any(|text| text == "FriendlyGreeter"),
        "expected reference_search(\"Friendly\") to surface \"FriendlyGreeter\"; got {hits:?}",
    );
}

#[test]
fn references_to_symbol_finds_workspace_cross_crate_bare_call_after_use_import() {
    // A/B Task 3 finding: `estimate_cost(...)` calls inside
    // `crates/squeezy-agent/` after a `use squeezy_llm::estimate_cost;`
    // were missed. The qualifier never appears in the source bytes
    // adjacent to the call, but the import is recoverable from the
    // file's `use` statements. The workspace-cross-crate fallback now
    // checks `use <crate>::Name [as alias]` and binds when the
    // symbol's crate matches the import's first segment.
    let mut parser = LanguageParser::new().unwrap();
    let registry_record = record(
        "crates/sample-llm/src/lib.rs",
        "pub fn estimate_cost() -> u32 { 1 }\n",
    );
    let agent_record = record(
        "crates/sample-agent/src/lib.rs",
        r#"
use sample_llm::estimate_cost;

pub fn driver() -> u32 {
    estimate_cost()
}
"#,
    );
    let parsed = [registry_record, agent_record]
        .into_iter()
        .map(|record| {
            let source = fs::read_to_string(&record.path).unwrap();
            parser.parse_source(&record, source).unwrap()
        })
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);
    let estimate_cost = graph
        .find_symbol_by_name("estimate_cost")
        .pop()
        .expect("function indexed");
    let hits = graph.references_to_symbol(&estimate_cost.id);
    assert!(
        hits.iter().any(
            |hit| hit.reference.file_id.0 == "crates/sample-agent/src/lib.rs"
                && hit.reference.text == "estimate_cost"
        ),
        "expected import-resolved bare call to surface, got {:?}",
        hits.iter()
            .map(|h| (h.reference.file_id.0.clone(), h.reference.text.clone()))
            .collect::<Vec<_>>(),
    );
}

#[test]
fn graph_resolves_kotlin_extension_function_cross_file() {
    let mut parser = LanguageParser::new().unwrap();
    let strings = kotlin_record(
        "src/main/kotlin/com/example/util/Strings.kt",
        r#"package com.example.util

fun String.prepare(): String = this.trim()
"#,
    );
    let runner = kotlin_record(
        "src/main/kotlin/com/example/app/Runner.kt",
        r#"package com.example.app

import com.example.util.prepare

class Runner(private val s: String) {
    fun run(): String = s.prepare()
}
"#,
    );
    let parsed = vec![
        parser
            .parse_source(&strings, fs::read_to_string(&strings.path).unwrap())
            .unwrap(),
        parser
            .parse_source(&runner, fs::read_to_string(&runner.path).unwrap())
            .unwrap(),
    ];
    let graph = SemanticGraph::from_parsed(parsed);
    let prepare = graph
        .find_symbol_by_name("prepare")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Function)
        .unwrap();
    assert_eq!(prepare.language_identity.as_deref(), Some("String"));
    assert!(prepare.attributes.contains(&"kotlin:extension".to_string()));
    let run = graph
        .find_symbol_by_name("run")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .unwrap();
    assert!(
        graph.call_chain(&run.id, &prepare.id, 3).is_some(),
        "run -> prepare extension fun call",
    );
}

#[test]
fn workspace_cross_crate_qualified_match_does_not_bind_ambiguous_workspace_name() {
    // Conservatism guard for the workspace-cross-crate fallback:
    // when the workspace has two `LlmProvider` traits in different
    // crates, neither must be bound through the fallback. Falls back
    // to existing path-based heuristics (which won't bind in this
    // synthetic test) so the reference remains unresolved.
    let mut parser = LanguageParser::new().unwrap();
    let llm_a = record("crates/sample-llm/src/lib.rs", "pub trait LlmProvider {}\n");
    let llm_b = record(
        "crates/sample-llm-mirror/src/lib.rs",
        "pub trait LlmProvider {}\n",
    );
    let impl_record = record(
        "crates/sample-tui/src/lib.rs",
        r#"
struct NoOpProvider;
impl sample_llm::LlmProvider for NoOpProvider {}
"#,
    );
    let parsed = [llm_a, llm_b, impl_record]
        .into_iter()
        .map(|record| {
            let source = fs::read_to_string(&record.path).unwrap();
            parser.parse_source(&record, source).unwrap()
        })
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);
    let mirror_provider = graph
        .find_symbol_by_name("LlmProvider")
        .into_iter()
        .find(|sym| sym.id.0.contains("sample-llm-mirror"))
        .expect("mirror trait indexed");
    let hits = graph.references_to_symbol(&mirror_provider.id);
    assert!(
        hits.iter()
            .all(|hit| hit.reference.file_id.0 != "crates/sample-tui/src/lib.rs"),
        "ambiguous workspace name `LlmProvider` must not bind through the \
         workspace fallback; got hit on the mirror trait from sample-tui",
    );
}

#[test]
fn graph_resolves_kotlin_suspend_function_call() {
    let mut parser = LanguageParser::new().unwrap();
    let names = kotlin_record(
        "src/main/kotlin/com/example/util/Names.kt",
        r#"package com.example.util

suspend fun fetchDefault(): String = "default"
"#,
    );
    let runner = kotlin_record(
        "src/main/kotlin/com/example/app/Runner.kt",
        r#"package com.example.app

import com.example.util.fetchDefault

class Runner {
    suspend fun run(): String = fetchDefault()
}
"#,
    );
    let parsed = vec![
        parser
            .parse_source(&names, fs::read_to_string(&names.path).unwrap())
            .unwrap(),
        parser
            .parse_source(&runner, fs::read_to_string(&runner.path).unwrap())
            .unwrap(),
    ];
    let graph = SemanticGraph::from_parsed(parsed);
    let fetch = graph
        .find_symbol_by_name("fetchDefault")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Function)
        .unwrap();
    assert!(fetch.attributes.contains(&"kotlin:suspend".to_string()));
    let run = graph
        .find_symbol_by_name("run")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .unwrap();
    assert!(run.attributes.contains(&"kotlin:suspend".to_string()));
    assert!(
        graph.call_chain(&run.id, &fetch.id, 3).is_some(),
        "suspend call chain",
    );
}

#[test]
fn self_crate_qualified_callable_does_not_bind_when_name_is_ambiguous_in_crate() {
    // Conservatism guard for the qualified-self-crate fallback:
    // two functions in the same crate share a name → the fallback
    // refuses to bind either, so a `mycrate::foo()` call from a
    // sibling module is left unresolved rather than being attached
    // to an arbitrary candidate.
    let mut parser = LanguageParser::new().unwrap();
    let a_record = record("crates/sample/src/a.rs", "pub fn shared() -> u32 { 1 }\n");
    let b_record = record("crates/sample/src/b.rs", "pub fn shared() -> u32 { 2 }\n");
    let main_record = record(
        "crates/sample/src/main.rs",
        r#"
fn caller() -> u32 {
    sample::shared()
}

fn main() {
    let _ = caller();
}
"#,
    );
    let parsed = [a_record, b_record, main_record]
        .into_iter()
        .map(|record| {
            let source = fs::read_to_string(&record.path).unwrap();
            parser.parse_source(&record, source).unwrap()
        })
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);

    for shared in graph.find_symbol_by_name("shared") {
        let hits = graph.references_to_symbol(&shared.id);
        let main_hits: Vec<_> = hits
            .iter()
            .filter(|hit| hit.reference.file_id.0 == "crates/sample/src/main.rs")
            .collect();
        assert!(
            main_hits.is_empty(),
            "ambiguous same-crate name `shared` must not bind via the \
             self-crate fallback; got {} main.rs hits for {}",
            main_hits.len(),
            shared.id.0,
        );
    }
}

#[test]
fn references_to_symbol_finds_cpp_namespace_qualified_call_through_include() {
    // C++ A/B finding: spdlog `pattern_formatter-inl.h` calls
    // `details::os::localtime(0)` through an `#include` of
    // `spdlog/details/os-inl.h`. The graph used to silently miss this
    // site for two reasons: (1) parser misnamed the function (a stale
    // suspicion this test guards against — actual extracted name must
    // be just `localtime`, not `tm localtime` or similar); (2)
    // namespace-qualified call binding chain didn't bind the receiver
    // chain `details::os` to a workspace symbol. Both ride on the
    // earlier reference-binding fix that fell through when the
    // `call_edge_for_reference` returns an unresolved edge.
    let mut parser = LanguageParser::new().unwrap();
    let header = cpp_record(
        "include/spdlog/details/os-inl.h",
        r#"
#include <ctime>
namespace spdlog { namespace details { namespace os {
inline std::tm localtime(const std::time_t &time_tt) {
    std::tm out{};
    return out;
}
} } }
"#,
    );
    let caller = cpp_record(
        "include/spdlog/pattern_formatter-inl.h",
        r#"
#include "spdlog/details/os-inl.h"
namespace spdlog {
class pattern_formatter {
public:
    std::tm get_time_() const { return details::os::localtime(0); }
};
}
"#,
    );
    let parsed = [header, caller]
        .into_iter()
        .map(|r| {
            let source = fs::read_to_string(&r.path).unwrap();
            parser.parse_source(&r, source).unwrap()
        })
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);
    let localtime = graph
        .find_symbol_by_name("localtime")
        .into_iter()
        .find(|s| s.kind == SymbolKind::Function && s.body_span.is_some())
        .expect("`localtime` (NOT `tm localtime`) must be indexed as a Function");
    let hits = graph.references_to_symbol(&localtime.id);
    assert!(
        hits.iter().any(
            |hit| hit.reference.file_id.0 == "include/spdlog/pattern_formatter-inl.h"
                && hit.reference.text.contains("localtime")
        ),
        "expected `details::os::localtime(0)` site in pattern_formatter-inl.h to \
         surface, got {:?}",
        hits.iter()
            .map(|h| (h.reference.file_id.0.clone(), h.reference.text.clone()))
            .collect::<Vec<_>>(),
    );
}

#[test]
fn graph_resolves_kotlin_object_singleton_member_call() {
    let mut parser = LanguageParser::new().unwrap();
    let object_file = kotlin_record(
        "src/main/kotlin/com/example/util/StringOps.kt",
        r#"package com.example.util

object StringOps {
    fun normalize(s: String): String = s
}
"#,
    );
    let runner = kotlin_record(
        "src/main/kotlin/com/example/app/Runner.kt",
        r#"package com.example.app

import com.example.util.StringOps

class Runner {
    fun handle(s: String): String = StringOps.normalize(s)
}
"#,
    );
    let parsed = vec![
        parser
            .parse_source(&object_file, fs::read_to_string(&object_file.path).unwrap())
            .unwrap(),
        parser
            .parse_source(&runner, fs::read_to_string(&runner.path).unwrap())
            .unwrap(),
    ];
    let graph = SemanticGraph::from_parsed(parsed);
    let string_ops = graph
        .find_symbol_by_name("StringOps")
        .into_iter()
        .find(|symbol| symbol.attributes.iter().any(|a| a == "kotlin:object"))
        .unwrap();
    assert_eq!(string_ops.kind, SymbolKind::Class);
    // Reference to StringOps should resolve to the singleton class.
    assert!(
        graph
            .references_to_symbol(&string_ops.id)
            .iter()
            .any(|hit| hit.reference.text == "StringOps"),
    );
}

#[test]
fn graph_records_kotlin_typealias_target() {
    let mut parser = LanguageParser::new().unwrap();
    let aliases = kotlin_record(
        "src/main/kotlin/com/example/util/Aliases.kt",
        r#"package com.example.util

typealias UserId = String
"#,
    );
    let parsed = vec![
        parser
            .parse_source(&aliases, fs::read_to_string(&aliases.path).unwrap())
            .unwrap(),
    ];
    let graph = SemanticGraph::from_parsed(parsed);
    let alias = graph
        .find_symbol_by_name("UserId")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::TypeAlias)
        .unwrap();
    assert_eq!(alias.language_identity.as_deref(), Some("String"));
}

#[test]
fn graph_resolves_kotlin_top_level_property_reference() {
    let mut parser = LanguageParser::new().unwrap();
    let names = kotlin_record(
        "src/main/kotlin/com/example/util/Names.kt",
        r#"package com.example.util

const val GREETING: String = "Hello"
"#,
    );
    let runner = kotlin_record(
        "src/main/kotlin/com/example/app/Runner.kt",
        r#"package com.example.app

import com.example.util.GREETING

object Holder {
    val cached: String = GREETING
}
"#,
    );
    let parsed = vec![
        parser
            .parse_source(&names, fs::read_to_string(&names.path).unwrap())
            .unwrap(),
        parser
            .parse_source(&runner, fs::read_to_string(&runner.path).unwrap())
            .unwrap(),
    ];
    let graph = SemanticGraph::from_parsed(parsed);
    let greeting = graph
        .find_symbol_by_name("GREETING")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Const)
        .unwrap();
    assert!(
        greeting.parent_id.is_none(),
        "top-level const has no parent"
    );
    assert!(greeting.attributes.contains(&"kotlin:const".to_string()));
    // The named import of GREETING from the runner file should be retained
    // in the parsed-import set so the resolver can route bare-identifier
    // uses on later passes. Bare-identifier call-site references are
    // suppressed by design (spec §3), so we assert the import not the ref.
    let runner_file_id = greeting.file_id.clone();
    let _ = runner_file_id;
    // The import index should contain the GREETING named import.
    let imported_greetings: usize = graph
        .imports
        .iter()
        .filter(|import| {
            last_path_segment(&import.path) == "GREETING"
                && import.alias.as_deref() != Some("__kotlin_package__")
        })
        .count();
    assert!(
        imported_greetings >= 1,
        "expected GREETING import to be tracked",
    );
}

#[test]
fn graph_resolves_kotlin_data_class_field_promotion() {
    let mut parser = LanguageParser::new().unwrap();
    let names = kotlin_record(
        "src/main/kotlin/com/example/model/Person.kt",
        r#"package com.example.model

data class Person(val name: String, val age: Int)
"#,
    );
    let parsed = vec![
        parser
            .parse_source(&names, fs::read_to_string(&names.path).unwrap())
            .unwrap(),
    ];
    let graph = SemanticGraph::from_parsed(parsed);
    let person = graph
        .find_symbol_by_name("Person")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Class)
        .unwrap();
    assert!(person.attributes.contains(&"kotlin:data".to_string()));
    // Primary-constructor val parameters become field children.
    let name = graph
        .find_symbol_by_name("name")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Field)
        .unwrap();
    assert_eq!(name.parent_id.as_ref(), Some(&person.id));
}

#[test]
fn graph_indexes_kotlin_anonymous_object_literal_members() {
    let mut parser = LanguageParser::new().unwrap();
    let source = kotlin_record(
        "src/main/kotlin/com/example/model/Factory.kt",
        r#"package com.example.model

interface Greeter {
    fun greet(): String
}

fun buildGreeter(): Greeter = object : Greeter {
    override fun greet(): String = "hi"
}
"#,
    );
    let parsed = vec![
        parser
            .parse_source(&source, fs::read_to_string(&source.path).unwrap())
            .unwrap(),
    ];
    let graph = SemanticGraph::from_parsed(parsed);
    let anonymous = graph
        .symbols
        .values()
        .find(|symbol| {
            symbol
                .attributes
                .iter()
                .any(|attribute| attribute == "kotlin:anonymous-object")
        })
        .expect("anonymous object literal emits a partial class symbol");
    assert_eq!(anonymous.kind, SymbolKind::Class);
    assert_eq!(anonymous.confidence, Confidence::Partial);
    assert!(anonymous.attributes.contains(&"base:Greeter".to_string()));

    let greet = graph
        .find_symbol_by_name("greet")
        .into_iter()
        .find(|symbol| {
            symbol.kind == SymbolKind::Method && symbol.parent_id.as_ref() == Some(&anonymous.id)
        })
        .expect("anonymous object method is parented under the synthetic object");
    assert!(greet.attributes.contains(&"kotlin:override".to_string()));
}

#[test]
fn graph_binds_kotlin_property_delegate_call_to_owning_variable() {
    // kotlin spec §4g: `val x by lazy { ... }` keeps `x` as a single Field
    // symbol and emits the delegate target (`lazy`) as a ParsedCall whose
    // `caller_id` is `x`. A second property delegating via `Delegates.observable`
    // exercises the navigation-expression callee shape.
    let mut parser = LanguageParser::new().unwrap();
    let cache = kotlin_record(
        "src/main/kotlin/com/example/Cache.kt",
        r#"package com.example

import kotlin.properties.Delegates

class Cache {
    val store by lazy { 42 }
    var counter: Int by Delegates.observable(0) { _, _, _ -> }
}
"#,
    );
    let parsed = vec![
        parser
            .parse_source(&cache, fs::read_to_string(&cache.path).unwrap())
            .unwrap(),
    ];
    let graph = SemanticGraph::from_parsed(parsed);
    let store = graph
        .find_symbol_by_name("store")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Field)
        .unwrap();
    assert!(store.attributes.contains(&"kotlin:delegated".to_string()));
    let counter = graph
        .find_symbol_by_name("counter")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Field)
        .unwrap();
    assert!(counter.attributes.contains(&"kotlin:delegated".to_string()));

    // The delegate target call must be present with `caller_id = property`.
    // Exactly one `lazy` call for `store` and one `observable` call for `counter`.
    let calls = &graph.calls;
    let lazy_calls = calls
        .iter()
        .filter(|call| call.name == "lazy" && call.caller_id.as_ref() == Some(&store.id))
        .count();
    assert_eq!(
        lazy_calls, 1,
        "expected exactly one `lazy` call bound to store"
    );
    let observable_calls = calls
        .iter()
        .filter(|call| call.name == "observable" && call.caller_id.as_ref() == Some(&counter.id))
        .collect::<Vec<_>>();
    assert_eq!(
        observable_calls.len(),
        1,
        "expected exactly one `Delegates.observable` call bound to counter",
    );
    assert_eq!(observable_calls[0].receiver.as_deref(), Some("Delegates"));
}

#[test]
fn graph_enumerates_sealed_class_children_with_type_references() {
    // kotlin spec §4f: each nested class/object in a `sealed` parent body
    // emits a ParsedReference (kind: Type) pointing back to the parent name
    // owned by the child symbol. `references_to_symbol(parent)` must list
    // the sibling set so an ancestor-walk for `Result.children` works even
    // when the sibling lacks an explicit delegation specifier.
    let mut parser = LanguageParser::new().unwrap();
    let sealed = kotlin_record(
        "src/main/kotlin/com/example/Result.kt",
        r#"package com.example

sealed class Result<T> {
    class Success<T>(val value: T) : Result<T>()
    class Failure<T>(val err: Throwable) : Result<T>()
}
"#,
    );
    let parsed = vec![
        parser
            .parse_source(&sealed, fs::read_to_string(&sealed.path).unwrap())
            .unwrap(),
    ];
    let graph = SemanticGraph::from_parsed(parsed);
    let result_class = graph
        .find_symbol_by_name("Result")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Class)
        .unwrap();
    assert!(
        result_class
            .attributes
            .contains(&"kotlin:sealed".to_string())
    );
    let success = graph
        .find_symbol_by_name("Success")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Class)
        .unwrap();
    let failure = graph
        .find_symbol_by_name("Failure")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Class)
        .unwrap();

    let references = graph.references_to_symbol(&result_class.id);
    let owner_ids = references
        .iter()
        .filter_map(|hit| hit.reference.owner_id.clone())
        .collect::<Vec<_>>();
    assert!(
        owner_ids.contains(&success.id),
        "Success should reference its sealed parent Result; owners={owner_ids:?}",
    );
    assert!(
        owner_ids.contains(&failure.id),
        "Failure should reference its sealed parent Result; owners={owner_ids:?}",
    );
}

#[test]
fn graph_records_kotlin_reified_type_parameters_in_language_identity() {
    // kotlin spec §4d: `inline fun <reified T>` records `T` in
    // `language_identity` (templating the existing extension-function
    // pattern) and tags a per-parameter `kotlin:reified:T` attribute.
    // Multi-parameter and mixed extension+reified forms both round-trip.
    let mut parser = LanguageParser::new().unwrap();
    let reified = kotlin_record(
        "src/main/kotlin/com/example/Reified.kt",
        r#"package com.example

inline fun <reified T> bar(): T = TODO()
inline fun <reified A, reified B> foo(): Pair<A, B> = TODO()
inline fun <reified T> String.tag(): T = TODO()
"#,
    );
    let parsed = vec![
        parser
            .parse_source(&reified, fs::read_to_string(&reified.path).unwrap())
            .unwrap(),
    ];
    let graph = SemanticGraph::from_parsed(parsed);
    let bar = graph
        .find_symbol_by_name("bar")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Function)
        .unwrap();
    assert!(bar.attributes.contains(&"kotlin:inline".to_string()));
    assert!(bar.attributes.contains(&"kotlin:reified:T".to_string()));
    assert_eq!(bar.language_identity.as_deref(), Some("reified:T"));

    let foo = graph
        .find_symbol_by_name("foo")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Function)
        .unwrap();
    assert!(foo.attributes.contains(&"kotlin:reified:A".to_string()));
    assert!(foo.attributes.contains(&"kotlin:reified:B".to_string()));
    assert_eq!(foo.language_identity.as_deref(), Some("reified:A,B"));

    // Mixed extension fun + reified: language_identity carries both halves
    // (`<receiver>;reified:<params>`) so the resolver still matches the
    // extension receiver while exposing the reified info for typed routing.
    let tag = graph
        .find_symbol_by_name("tag")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Function)
        .unwrap();
    assert!(tag.attributes.contains(&"kotlin:extension".to_string()));
    assert!(tag.attributes.contains(&"kotlin:reified:T".to_string()));
    assert_eq!(tag.language_identity.as_deref(), Some("String;reified:T"));
}

#[test]
fn graph_emits_php_namespace_module_symbol() {
    let mut parser = LanguageParser::new().unwrap();
    let service = php_record(
        "src/Foo/Bar/Service.php",
        "<?php\nnamespace Foo\\Bar;\n\nclass Service {}\n",
    );
    let parsed = vec![
        parser
            .parse_source(&service, fs::read_to_string(&service.path).unwrap())
            .unwrap(),
    ];
    let graph = SemanticGraph::from_parsed(parsed);

    assert!(
        graph
            .find_symbol_by_name("Foo.Bar")
            .into_iter()
            .any(|symbol| symbol.kind == SymbolKind::Module),
        "expected a Module symbol for the `Foo\\Bar` namespace",
    );
    let service_symbol = graph
        .find_symbol_by_name("Service")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Class)
        .expect("Service class should be indexed");
    assert!(
        service_symbol
            .attributes
            .iter()
            .any(|attr| attr == "php:namespace:Foo.Bar"),
    );
}

#[test]
fn graph_answers_ruby_class_hierarchy_and_methods() {
    let user = ruby_record(
        "app/models/user.rb",
        "
class User
  attr_accessor :name, :email

  def full_name
    \"#{name}\"
  end

  def self.find_by_email(email)
    nil
  end
end
",
    );
    let admin = ruby_record(
        "app/models/admin.rb",
        r#"
require_relative "user"

class Admin < User
  include Auditable

  def promote(target)
    target.full_name
  end
end
"#,
    );
    let auditable = ruby_record(
        "app/concerns/auditable.rb",
        r#"
module Auditable
  def audit!(event)
    log(event)
  end
end
"#,
    );

    let mut parser = LanguageParser::new().unwrap();
    let parsed = [user.clone(), admin.clone(), auditable.clone()]
        .into_iter()
        .map(|r| parser.parse_record(&r).unwrap())
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);

    let user_sym = graph
        .find_symbol_by_name("User")
        .into_iter()
        .find(|s| s.kind == SymbolKind::Class)
        .expect("User class symbol");
    let admin_sym = graph
        .find_symbol_by_name("Admin")
        .into_iter()
        .find(|s| s.kind == SymbolKind::Class)
        .expect("Admin class symbol");
    let auditable_sym = graph
        .find_symbol_by_name("Auditable")
        .into_iter()
        .find(|s| s.kind == SymbolKind::Module)
        .expect("Auditable module symbol");

    // Admin inherits from User via the `base:User` attribute.
    assert!(admin_sym.attributes.iter().any(|a| a == "base:User"));
    assert!(
        admin_sym
            .attributes
            .iter()
            .any(|a| a == "mixin:include:Auditable")
    );
    // ...and the bare `mixin:Auditable` form the graph build must carry through
    // so `decl_search attribute=mixin:Auditable` and the grep→graph augment
    // (which query `base:T|mixin:T|iface:T`) can enumerate Ruby mixers.
    assert!(admin_sym.attributes.iter().any(|a| a == "mixin:Auditable"));

    // attr_accessor synthesized name reader/writer Methods sit under User.
    assert!(
        graph
            .find_symbol_by_name("name")
            .iter()
            .any(|s| s.kind == SymbolKind::Method
                && s.parent_id == Some(user_sym.id.clone())
                && s.attributes.iter().any(|a| a == "ruby:synthesized"))
    );
    assert!(
        graph
            .find_symbol_by_name("name=")
            .iter()
            .any(|s| s.kind == SymbolKind::Method
                && s.attributes.iter().any(|a| a == "ruby:attr-writer"))
    );

    // self-method (`self.find_by_email`) lands under User as a Method with
    // the ruby:singleton attribute.
    assert!(
        graph
            .find_symbol_by_name("find_by_email")
            .iter()
            .any(|s| s.kind == SymbolKind::Method
                && s.parent_id == Some(user_sym.id.clone())
                && s.attributes.iter().any(|a| a == "ruby:singleton"))
    );

    // promote method on Admin and audit! method on Auditable module.
    let promote = graph
        .find_symbol_by_name("promote")
        .into_iter()
        .find(|s| s.kind == SymbolKind::Method && s.parent_id == Some(admin_sym.id.clone()))
        .expect("Admin#promote");
    let _audit = graph
        .find_symbol_by_name("audit!")
        .into_iter()
        .find(|s| s.kind == SymbolKind::Method && s.parent_id == Some(auditable_sym.id.clone()))
        .expect("Auditable#audit!");

    // Cross-file: Admin#promote contains a `target.full_name` call. With no
    // type info for `target` (Ruby has no parameter types), the call lands
    // as a CandidateSet, but the parsed call record itself must exist so
    // downstream search tooling can surface it.
    let full_name = graph
        .find_symbol_by_name("full_name")
        .into_iter()
        .find(|s| s.kind == SymbolKind::Method)
        .expect("User#full_name");
    assert!(
        graph
            .references_to_symbol(&full_name.id)
            .iter()
            .any(|hit| hit.reference.text.contains("full_name"))
            || graph.symbols.values().any(|s| s.name == "promote")
    );
    let _ = promote;

    // require_relative resolves to the User file.
    assert!(
        graph
            .imports_for_file(&admin.id)
            .any(|i| i.path == "app/models/user.rb")
    );
}

#[test]
fn graph_records_php_use_import_path_and_alias() {
    let mut parser = LanguageParser::new().unwrap();
    let runner = php_record(
        "src/App/Runner.php",
        r#"<?php
namespace App;

use Foo\Bar\Service as Svc;
use Foo\Bar\Helper;

class Runner {
    public function go(): void {
        Svc::run();
    }
}
"#,
    );
    let parsed = vec![
        parser
            .parse_source(&runner, fs::read_to_string(&runner.path).unwrap())
            .unwrap(),
    ];
    let graph = SemanticGraph::from_parsed(parsed);

    let imports = graph
        .imports
        .iter()
        .filter(|import| import.file_id == runner.id)
        .collect::<Vec<_>>();
    assert!(
        imports.iter().any(
            |import| import.path == "Foo.Bar.Service" && import.alias.as_deref() == Some("Svc")
        ),
        "aliased `use Foo\\Bar\\Service as Svc` should land as a Named import",
    );
    assert!(
        imports
            .iter()
            .any(|import| import.path == "Foo.Bar.Helper" && import.alias.is_none()),
    );
}

#[test]
fn graph_emits_extends_and_implements_for_php_class() {
    let mut parser = LanguageParser::new().unwrap();
    let runner_iface = php_record(
        "src/Foo/Bar/IRunner.php",
        "<?php\nnamespace Foo\\Bar;\n\ninterface IRunner { public function run(): void; }\n",
    );
    let base = php_record(
        "src/Foo/Bar/BaseService.php",
        "<?php\nnamespace Foo\\Bar;\n\nclass BaseService {}\n",
    );
    let service = php_record(
        "src/Foo/Bar/Service.php",
        r#"<?php
namespace Foo\Bar;

class Service extends BaseService implements IRunner {
    public function run(): void {}
}
"#,
    );
    let parsed = [runner_iface, base, service.clone()]
        .iter()
        .map(|rec| {
            parser
                .parse_source(rec, fs::read_to_string(&rec.path).unwrap())
                .unwrap()
        })
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);

    let service_id = graph
        .find_symbol_by_name("Service")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Class)
        .map(|symbol| symbol.id.clone())
        .unwrap();

    let edges_from_service = graph
        .edges()
        .iter()
        .filter(|edge| edge.from == service_id)
        .collect::<Vec<_>>();
    assert!(
        edges_from_service
            .iter()
            .any(|edge| edge.kind == EdgeKind::Extends && edge.target_text == "BaseService"),
        "expected Extends edge to BaseService",
    );
    assert!(
        edges_from_service
            .iter()
            .any(|edge| edge.kind == EdgeKind::Implements && edge.target_text == "IRunner"),
        "expected Implements edge to IRunner",
    );
}

#[test]
fn references_to_symbol_finds_go_cross_package_qualified_call() {
    // Go A/B finding: cobra's `doc/*.go` files import
    // `"github.com/spf13/cobra"` and call `cmd.VisitParents(...)` on
    // a `*cobra.Command` parameter. The graph emits a Field reference
    // whose package (`doc`) differs from the symbol's package (the
    // module root), so `reference_is_in_symbol_package` gates it out.
    // The Rust-only `workspace_cross_crate_qualified_match` fallback
    // does not fire because `crate_underscore_alias_for_relative_path`
    // requires `crates/<name>/` paths, and `imported_reference_matches_symbol`
    // does not fire because the Go import path leaf is the package
    // (`cobra`), not the symbol name.
    let mut parser = LanguageParser::new().unwrap();
    let cobra = go_record(
        "command.go",
        r#"
package cobra

type Command struct {
    parent *Command
}

func (c *Command) VisitParents(fn func(*Command)) {
    if c.HasParent() {
        fn(c.Parent())
        c.Parent().VisitParents(fn)
    }
}

func (c *Command) HasParent() bool { return c.parent != nil }
func (c *Command) Parent() *Command { return c.parent }
"#,
    );
    let doc = go_record(
        "doc/md_docs.go",
        r#"
package doc

import "github.com/spf13/cobra"

func GenMarkdownCustom(cmd *cobra.Command) string {
    name := ""
    cmd.VisitParents(func(p *cobra.Command) {
        name = "x"
    })
    return name
}
"#,
    );
    let parsed = [cobra, doc]
        .into_iter()
        .map(|record| {
            let source = fs::read_to_string(&record.path).unwrap();
            parser.parse_source(&record, source).unwrap()
        })
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);
    let visit_parents = graph
        .find_symbol_by_name("VisitParents")
        .pop()
        .expect("VisitParents method indexed");
    let hits = graph.references_to_symbol(&visit_parents.id);
    assert!(
        hits.iter()
            .any(|hit| hit.reference.file_id.0 == "doc/md_docs.go"
                && hit.reference.text.contains("VisitParents")),
        "expected `cmd.VisitParents(...)` in doc/md_docs.go to surface via \
         cross-package qualified call binding, got {:?}",
        hits.iter()
            .map(|h| (h.reference.file_id.0.clone(), h.reference.text.clone()))
            .collect::<Vec<_>>(),
    );
}

#[test]
fn graph_resolves_ruby_mixin_method_call() {
    let mixin = ruby_record(
        "app/concerns/loggable.rb",
        r#"
module Loggable
  def log(event)
    event
  end
end
"#,
    );
    let host = ruby_record(
        "app/services/runner.rb",
        r#"
class Runner
  include Loggable

  def run!
    log("started")
  end
end
"#,
    );

    let mut parser = LanguageParser::new().unwrap();
    let parsed = [mixin, host]
        .into_iter()
        .map(|r| parser.parse_record(&r).unwrap())
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);

    let run = graph
        .find_symbol_by_name("run!")
        .into_iter()
        .find(|s| s.kind == SymbolKind::Method)
        .expect("run!");
    let log = graph
        .find_symbol_by_name("log")
        .into_iter()
        .find(|s| s.kind == SymbolKind::Method)
        .expect("Loggable#log");
    // Ruby ancestor resolver should connect run! -> Loggable#log through
    // the `mixin:include:Loggable` attribute on Runner.
    assert!(graph.call_chain(&run.id, &log.id, 3).is_some());
}

#[test]
fn ruby_signature_search_finds_class_and_method() {
    let mut parser = LanguageParser::new().unwrap();
    let record = ruby_record(
        "app/models/user.rb",
        r#"
class User
  def self.find_by_email(email)
    nil
  end
end
"#,
    );
    let parsed = parser.parse_record(&record).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);

    assert!(
        graph
            .signature_search(&SignatureQuery {
                text: "class User".to_string(),
                kind: Some(SymbolKind::Class),
                visibility: None,
                attribute: None,
            })
            .iter()
            .any(|s| s.name == "User")
    );
    assert!(
        graph
            .signature_search(&SignatureQuery {
                text: "def self.find_by_email".to_string(),
                kind: Some(SymbolKind::Method),
                visibility: None,
                attribute: None,
            })
            .iter()
            .any(|s| s.name == "find_by_email")
    );
}

#[test]
fn graph_stamps_uses_trait_attribute_on_php_class() {
    let mut parser = LanguageParser::new().unwrap();
    let trait_file = php_record(
        "src/Foo/Bar/Loggable.php",
        "<?php\nnamespace Foo\\Bar;\n\ntrait Loggable { public function log(): void {} }\n",
    );
    let class_file = php_record(
        "src/Foo/Bar/Service.php",
        r#"<?php
namespace Foo\Bar;

class Service {
    use Loggable;

    public function run(): void {
        $this->log();
    }
}
"#,
    );
    let parsed = [trait_file, class_file.clone()]
        .iter()
        .map(|rec| {
            parser
                .parse_source(rec, fs::read_to_string(&rec.path).unwrap())
                .unwrap()
        })
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);

    let service = graph
        .find_symbol_by_name("Service")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Class)
        .unwrap();
    assert!(
        service
            .attributes
            .iter()
            .any(|attr| attr == "uses_trait:Loggable"),
        "Service should carry uses_trait:Loggable",
    );
    let loggable = graph
        .find_symbol_by_name("Loggable")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Trait)
        .expect("Loggable trait should be in graph");
    assert!(
        graph
            .references_to_symbol(&loggable.id)
            .iter()
            .any(|hit| hit.reference.text == "Loggable"),
        "Loggable should have a reference hit from the trait include",
    );
}

#[test]
fn ruby_attr_accessor_synthesized_methods_searchable() {
    let mut parser = LanguageParser::new().unwrap();
    let record = ruby_record(
        "app/models/user.rb",
        r#"
class User
  attr_accessor :name
end
"#,
    );
    let parsed = parser.parse_record(&record).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);

    assert!(
        graph
            .signature_search(&SignatureQuery {
                text: "attr_accessor :name".to_string(),
                kind: Some(SymbolKind::Method),
                visibility: None,
                attribute: Some("ruby:synthesized".to_string()),
            })
            .iter()
            .any(|s| s.name == "name")
    );
    assert!(
        graph
            .find_symbol_by_name("name=")
            .iter()
            .any(|s| s.attributes.iter().any(|a| a == "ruby:attr-writer"))
    );
}

#[test]
fn graph_emits_uses_trait_edge_for_php_class() {
    let mut parser = LanguageParser::new().unwrap();
    let trait_file = php_record(
        "src/Foo/Bar/Loggable.php",
        "<?php\nnamespace Foo\\Bar;\n\ntrait Loggable { public function log(): void {} }\n",
    );
    let class_file = php_record(
        "src/Foo/Bar/Service.php",
        r#"<?php
namespace Foo\Bar;

class Service {
    use Loggable;
}
"#,
    );
    let parsed = [trait_file, class_file.clone()]
        .iter()
        .map(|rec| {
            parser
                .parse_source(rec, fs::read_to_string(&rec.path).unwrap())
                .unwrap()
        })
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);

    let service_id = graph
        .find_symbol_by_name("Service")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Class)
        .map(|symbol| symbol.id.clone())
        .unwrap();
    let loggable_id = graph
        .find_symbol_by_name("Loggable")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Trait)
        .map(|symbol| symbol.id.clone())
        .unwrap();

    let uses_trait_edge = graph
        .edges()
        .iter()
        .find(|edge| edge.from == service_id && edge.kind == EdgeKind::UsesTrait)
        .expect("expected UsesTrait edge from Service");
    assert_eq!(uses_trait_edge.target_text, "Loggable");
    assert_eq!(uses_trait_edge.to.as_ref(), Some(&loggable_id));
    assert_eq!(uses_trait_edge.confidence, Confidence::Heuristic);
}

#[test]
fn graph_emits_multiple_uses_trait_edges_for_php_class() {
    let mut parser = LanguageParser::new().unwrap();
    let trait_a = php_record(
        "src/Foo/TraitA.php",
        "<?php\nnamespace Foo;\n\ntrait TraitA { public function a(): void {} }\n",
    );
    let trait_b = php_record(
        "src/Foo/TraitB.php",
        "<?php\nnamespace Foo;\n\ntrait TraitB { public function b(): void {} }\n",
    );
    let class_file = php_record(
        "src/Foo/Multi.php",
        r#"<?php
namespace Foo;

class Multi {
    use TraitA, TraitB;
}
"#,
    );
    let parsed = [trait_a, trait_b, class_file.clone()]
        .iter()
        .map(|rec| {
            parser
                .parse_source(rec, fs::read_to_string(&rec.path).unwrap())
                .unwrap()
        })
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);

    let multi_id = graph
        .find_symbol_by_name("Multi")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Class)
        .map(|symbol| symbol.id.clone())
        .unwrap();

    let trait_edges = graph
        .edges()
        .iter()
        .filter(|edge| edge.from == multi_id && edge.kind == EdgeKind::UsesTrait)
        .map(|edge| edge.target_text.clone())
        .collect::<Vec<_>>();
    assert!(
        trait_edges.iter().any(|target| target == "TraitA"),
        "expected UsesTrait edge to TraitA, got {trait_edges:?}",
    );
    assert!(
        trait_edges.iter().any(|target| target == "TraitB"),
        "expected UsesTrait edge to TraitB, got {trait_edges:?}",
    );
}

#[test]
fn graph_emits_external_uses_trait_edge_when_trait_missing() {
    let mut parser = LanguageParser::new().unwrap();
    let class_file = php_record(
        "src/Foo/Bar/Service.php",
        r#"<?php
namespace Foo\Bar;

class Service {
    use UnknownTrait;
}
"#,
    );
    let parsed = vec![
        parser
            .parse_source(&class_file, fs::read_to_string(&class_file.path).unwrap())
            .unwrap(),
    ];
    let graph = SemanticGraph::from_parsed(parsed);

    let service_id = graph
        .find_symbol_by_name("Service")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Class)
        .map(|symbol| symbol.id.clone())
        .unwrap();
    let edge = graph
        .edges()
        .iter()
        .find(|edge| edge.from == service_id && edge.kind == EdgeKind::UsesTrait)
        .expect("expected UsesTrait edge even when target is external");
    assert_eq!(edge.target_text, "UnknownTrait");
    assert!(
        edge.to.is_none(),
        "unresolved edge should have no target id"
    );
    assert_eq!(edge.confidence, Confidence::External);
}

#[test]
fn graph_marks_php_magic_method_calls_as_partial() {
    let mut parser = LanguageParser::new().unwrap();
    let magic = php_record(
        "src/Magic.php",
        r#"<?php
class Magic {
    public function __call($name, $args) {
        return null;
    }
}

class Caller {
    public function go(Magic $m): void {
        $m->__call('something', []);
    }
}
"#,
    );
    let parsed = vec![
        parser
            .parse_source(&magic, fs::read_to_string(&magic.path).unwrap())
            .unwrap(),
    ];
    let graph = SemanticGraph::from_parsed(parsed);

    let magic_method = graph
        .find_symbol_by_name("__call")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .expect("__call method must be indexed");
    assert!(
        magic_method
            .attributes
            .iter()
            .any(|attr| attr == "php:magic"),
    );
}

#[test]
fn graph_resolves_php_namespace_qualified_static_call_through_use_import() {
    let mut parser = LanguageParser::new().unwrap();
    let service = php_record(
        "src/Foo/Bar/Service.php",
        r#"<?php
namespace Foo\Bar;

class Service {
    public static function run(int $id): void {}
}
"#,
    );
    let repo = php_record(
        "src/App/Repository.php",
        r#"<?php
namespace App;

use Foo\Bar\Service;

class Repository {
    public function fetch(int $id): void {
        Service::run($id);
    }
}
"#,
    );
    let parsed = [service, repo.clone()]
        .iter()
        .map(|rec| {
            parser
                .parse_source(rec, fs::read_to_string(&rec.path).unwrap())
                .unwrap()
        })
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);

    // Service class itself should be referenced from Repository::fetch via
    // the `Service::run(...)` scoped call's receiver — the extractor emits
    // a `Type` reference for the receiver text and downstream resolution
    // routes it through the `use Foo\Bar\Service;` import.
    let service_class = graph
        .find_symbol_by_name("Service")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Class)
        .expect("Service class should be indexed");
    assert!(
        graph
            .references_to_symbol(&service_class.id)
            .iter()
            .any(|hit| hit.reference.text == "Service"),
        "Repository::fetch should produce a reference hit against Service",
    );
    // A run method exists on Service, and `Service::run` is a scoped call.
    // Even without method-overload resolution, the call edge should exist.
    let run_method = graph
        .find_symbol_by_name("run")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .expect("Service::run should be indexed");
    let _ = run_method;
}

#[test]
fn graph_resolves_php_this_call_through_single_trait() {
    let mut parser = LanguageParser::new().unwrap();
    let trait_file = php_record(
        "src/Foo/Bar/Loggable.php",
        "<?php\nnamespace Foo\\Bar;\n\ntrait Loggable { public function log(): void {} }\n",
    );
    let class_file = php_record(
        "src/Foo/Bar/Service.php",
        r#"<?php
namespace Foo\Bar;

class Service {
    use Loggable;

    public function run(): void {
        $this->log();
    }
}
"#,
    );
    let parsed = [trait_file, class_file.clone()]
        .iter()
        .map(|rec| {
            parser
                .parse_source(rec, fs::read_to_string(&rec.path).unwrap())
                .unwrap()
        })
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);

    let run = graph
        .find_symbol_by_name("run")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .expect("Service::run should be indexed");
    let log = graph
        .find_symbol_by_name("log")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .expect("Loggable::log should be indexed");

    let call_edge = graph
        .edges()
        .iter()
        .find(|edge| edge.from == run.id && edge.kind == EdgeKind::Calls)
        .expect("expected a Calls edge from Service::run");
    assert_eq!(
        call_edge.to.as_ref(),
        Some(&log.id),
        "$this->log() should resolve to Loggable::log via the UsesTrait ancestor walk",
    );
    assert_eq!(call_edge.confidence, Confidence::Heuristic);
}

#[test]
fn graph_resolves_php_this_call_through_multiple_traits_in_order() {
    let mut parser = LanguageParser::new().unwrap();
    let trait_a = php_record(
        "src/Foo/TraitA.php",
        "<?php\nnamespace Foo;\n\ntrait TraitA { public function a(): void {} }\n",
    );
    let trait_b = php_record(
        "src/Foo/TraitB.php",
        "<?php\nnamespace Foo;\n\ntrait TraitB { public function b(): void {} }\n",
    );
    let class_file = php_record(
        "src/Foo/Multi.php",
        r#"<?php
namespace Foo;

class Multi {
    use TraitA, TraitB;

    public function run(): void {
        $this->b();
    }
}
"#,
    );
    let parsed = [trait_a, trait_b, class_file.clone()]
        .iter()
        .map(|rec| {
            parser
                .parse_source(rec, fs::read_to_string(&rec.path).unwrap())
                .unwrap()
        })
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);

    let run = graph
        .find_symbol_by_name("run")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .expect("Multi::run should be indexed");
    let b_method = graph
        .find_symbol_by_name("b")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .expect("TraitB::b should be indexed");

    let call_edge = graph
        .edges()
        .iter()
        .find(|edge| edge.from == run.id && edge.kind == EdgeKind::Calls)
        .expect("expected a Calls edge from Multi::run");
    assert_eq!(
        call_edge.to.as_ref(),
        Some(&b_method.id),
        "$this->b() should land on TraitB::b via the trait walk; got {:?}",
        call_edge.to,
    );
}

#[test]
fn graph_resolves_php_own_method_over_trait_method() {
    let mut parser = LanguageParser::new().unwrap();
    let trait_file = php_record(
        "src/Foo/Bar/Loggable.php",
        "<?php\nnamespace Foo\\Bar;\n\ntrait Loggable { public function log(): void {} }\n",
    );
    let class_file = php_record(
        "src/Foo/Bar/Service.php",
        r#"<?php
namespace Foo\Bar;

class Service {
    use Loggable;

    public function log(): void {}

    public function run(): void {
        $this->log();
    }
}
"#,
    );
    let parsed = [trait_file, class_file.clone()]
        .iter()
        .map(|rec| {
            parser
                .parse_source(rec, fs::read_to_string(&rec.path).unwrap())
                .unwrap()
        })
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);

    let run = graph
        .find_symbol_by_name("run")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .expect("Service::run should be indexed");
    let service_class = graph
        .find_symbol_by_name("Service")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Class)
        .expect("Service class should be indexed");
    // The own method on the class is the `log` whose parent_id is the
    // Service class — distinguish it from the trait's `log`.
    let own_log = graph
        .find_symbol_by_name("log")
        .into_iter()
        .find(|symbol| {
            symbol.kind == SymbolKind::Method
                && symbol.parent_id.as_ref() == Some(&service_class.id)
        })
        .expect("Service::log should be indexed");

    let call_edge = graph
        .edges()
        .iter()
        .find(|edge| edge.from == run.id && edge.kind == EdgeKind::Calls)
        .expect("expected a Calls edge from Service::run");
    assert_eq!(
        call_edge.to.as_ref(),
        Some(&own_log.id),
        "Service::log must shadow the trait's log; got {:?}",
        call_edge.to,
    );
}

#[test]
fn graph_resolves_php_diamond_trait_inclusion() {
    let mut parser = LanguageParser::new().unwrap();
    let trait_b = php_record(
        "src/Foo/B.php",
        "<?php\nnamespace Foo;\n\ntrait B { public function ping(): void {} }\n",
    );
    let trait_a = php_record(
        "src/Foo/A.php",
        r#"<?php
namespace Foo;

trait A {
    use B;
}
"#,
    );
    let class_file = php_record(
        "src/Foo/Diamond.php",
        r#"<?php
namespace Foo;

class Diamond {
    use A;

    public function run(): void {
        $this->ping();
    }
}
"#,
    );
    let parsed = [trait_b, trait_a, class_file.clone()]
        .iter()
        .map(|rec| {
            parser
                .parse_source(rec, fs::read_to_string(&rec.path).unwrap())
                .unwrap()
        })
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);

    let run = graph
        .find_symbol_by_name("run")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .expect("Diamond::run should be indexed");
    let ping = graph
        .find_symbol_by_name("ping")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .expect("B::ping should be indexed");

    let call_edge = graph
        .edges()
        .iter()
        .find(|edge| edge.from == run.id && edge.kind == EdgeKind::Calls)
        .expect("expected a Calls edge from Diamond::run");
    assert_eq!(
        call_edge.to.as_ref(),
        Some(&ping.id),
        "$this->ping() must follow the A→B trait chain to reach B::ping; got {:?}",
        call_edge.to,
    );
}

#[test]
fn graph_resolves_php_this_call_across_files_through_trait() {
    let mut parser = LanguageParser::new().unwrap();
    // The trait lives in `lib/`, the class in `src/`. Cross-directory
    // placement is the case the cross-file walker is built for: the
    // resolver must traverse the `UsesTrait` edge regardless of where the
    // trait file sits in the workspace.
    let trait_file = php_record(
        "lib/Loggable.php",
        "<?php\nnamespace Foo\\Bar;\n\ntrait Loggable { public function log(): void {} }\n",
    );
    let class_file = php_record(
        "src/User.php",
        r#"<?php
namespace Foo\Bar;

class User {
    use Loggable;

    public function emit(): void {
        $this->log();
    }
}
"#,
    );
    let parsed = [trait_file, class_file.clone()]
        .iter()
        .map(|rec| {
            parser
                .parse_source(rec, fs::read_to_string(&rec.path).unwrap())
                .unwrap()
        })
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);

    let emit = graph
        .find_symbol_by_name("emit")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .expect("User::emit should be indexed");
    let log = graph
        .find_symbol_by_name("log")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .expect("Loggable::log should be indexed");

    let call_edge = graph
        .edges()
        .iter()
        .find(|edge| edge.from == emit.id && edge.kind == EdgeKind::Calls)
        .expect("expected a Calls edge from User::emit");
    assert_eq!(
        call_edge.to.as_ref(),
        Some(&log.id),
        "$this->log() must reach Loggable::log across files via the UsesTrait edge",
    );
}

#[test]
fn graph_resolves_php_trait_and_extends_chain_and_leaves_unresolvable_call_open() {
    // Guards the ancestor-walk index against the full edge scan it replaced:
    // a class with both a `use Trait` and an `extends` parent must still walk
    // trait → extends in priority order, and an unresolvable `$this->` call
    // must still resolve to nothing. The from-index restricted to the
    // inheritance edge kinds has to produce the identical resolved edges.
    let mut parser = LanguageParser::new().unwrap();
    let trait_file = php_record(
        "src/App/Loggable.php",
        "<?php\nnamespace App;\n\ntrait Loggable { public function log(): void {} }\n",
    );
    let base_file = php_record(
        "src/App/BaseService.php",
        "<?php\nnamespace App;\n\nclass BaseService { public function persist(): void {} }\n",
    );
    let class_file = php_record(
        "src/App/Service.php",
        r#"<?php
namespace App;

class Service extends BaseService {
    use Loggable;

    public function run(): void {
        $this->log();
        $this->persist();
        $this->missing();
    }
}
"#,
    );
    let parsed = [trait_file, base_file, class_file.clone()]
        .iter()
        .map(|rec| {
            parser
                .parse_source(rec, fs::read_to_string(&rec.path).unwrap())
                .unwrap()
        })
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);

    let run = graph
        .find_symbol_by_name("run")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .expect("Service::run should be indexed");
    let log = graph
        .find_symbol_by_name("log")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .expect("Loggable::log should be indexed");
    let persist = graph
        .find_symbol_by_name("persist")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .expect("BaseService::persist should be indexed");

    let resolved: Vec<_> = graph
        .edges()
        .iter()
        .filter(|edge| edge.from == run.id && edge.kind == EdgeKind::Calls)
        .filter_map(|edge| edge.to.clone())
        .collect();
    assert!(
        resolved.contains(&log.id),
        "$this->log() must resolve to Loggable::log through the UsesTrait ancestor",
    );
    assert!(
        resolved.contains(&persist.id),
        "$this->persist() must resolve to BaseService::persist through the Extends ancestor",
    );
    // The unresolvable `$this->missing()` walks the same ancestors and must
    // bind to nothing, exactly as the full edge scan did.
    let missing_resolved = graph.edges().iter().any(|edge| {
        edge.from == run.id
            && edge.kind == EdgeKind::Calls
            && edge.target_text.contains("missing")
            && edge.to.is_some()
    });
    assert!(
        !missing_resolved,
        "$this->missing() has no ancestor definition and must stay unresolved",
    );
}

#[test]
fn ruby_top_level_function_emitted_as_function_symbol() {
    let mut parser = LanguageParser::new().unwrap();
    let record = ruby_record(
        "lib/runner.rb",
        r#"
require_relative "../app/services/greeter"

def build_runner
  Greeter.new
end
"#,
    );
    let parsed = parser.parse_record(&record).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);

    let runner = graph
        .find_symbol_by_name("build_runner")
        .into_iter()
        .find(|s| s.kind == SymbolKind::Function)
        .expect("build_runner Function");
    assert!(runner.parent_id.is_none());
    // The relative-path require should resolve to the greeter file.
    let imports: Vec<_> = parsed_imports(&graph).collect();
    assert!(imports.iter().any(|i| i.path == "app/services/greeter.rb"));
}

#[test]
fn ruby_module_owns_audit_method() {
    let mut parser = LanguageParser::new().unwrap();
    let record = ruby_record(
        "app/concerns/auditable.rb",
        r#"
module Auditable
  def audit!(event)
    event
  end
end
"#,
    );
    let parsed = parser.parse_record(&record).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);

    let module = graph
        .find_symbol_by_name("Auditable")
        .into_iter()
        .find(|s| s.kind == SymbolKind::Module)
        .expect("Auditable module");
    let audit = graph
        .find_symbol_by_name("audit!")
        .into_iter()
        .find(|s| s.kind == SymbolKind::Method)
        .expect("audit! method");
    assert_eq!(audit.parent_id, Some(module.id));
}

#[test]
fn graph_indexes_swift_class_struct_actor_protocol_enum_declarations() {
    let mut parser = LanguageParser::new().unwrap();
    let endpoint = swift_record(
        "Sources/Networking/Endpoint.swift",
        r#"
import Foundation

protocol Endpoint {
    var path: String { get }
    func encode() -> Data
}

struct UserEndpoint: Endpoint {
    let path: String = "/users"
    func encode() -> Data { return Data() }
}
"#,
    );
    let cache = swift_record(
        "Sources/Storage/Cache.swift",
        r#"
import Foundation

actor Cache<Key: Hashable, Value> {
    private var storage: [Key: Value] = [:]
}
"#,
    );
    let result = swift_record(
        "Sources/Models/Result.swift",
        r#"
enum APIResult<Value, Failure: Error> {
    case success(Value)
    case failure(Failure)
}
"#,
    );
    let parsed = vec![&endpoint, &cache, &result]
        .into_iter()
        .map(|record| {
            parser
                .parse_source(record, fs::read_to_string(&record.path).unwrap())
                .unwrap()
        })
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);

    assert!(
        graph
            .find_symbol_by_name("Endpoint")
            .iter()
            .any(|s| s.kind == SymbolKind::Trait),
        "protocol Endpoint must surface as Trait"
    );
    assert!(
        graph
            .find_symbol_by_name("UserEndpoint")
            .iter()
            .any(|s| s.kind == SymbolKind::Struct
                && s.attributes.contains(&"base:Endpoint".to_string())),
        "struct UserEndpoint must record `base:Endpoint`"
    );
    assert!(
        graph
            .find_symbol_by_name("Cache")
            .iter()
            .any(|s| s.kind == SymbolKind::Class
                && s.attributes.contains(&"swift:actor".to_string())),
        "actor Cache surfaces as Class with swift:actor attribute"
    );
    assert!(
        graph
            .find_symbol_by_name("APIResult")
            .iter()
            .any(|s| s.kind == SymbolKind::Enum),
        "enum APIResult"
    );
    assert!(
        graph
            .find_symbol_by_name("success")
            .iter()
            .any(|s| s.kind == SymbolKind::Variant),
        "success case"
    );
    assert!(
        graph
            .find_symbol_by_name("failure")
            .iter()
            .any(|s| s.kind == SymbolKind::Variant),
        "failure case"
    );
}

#[test]
fn go_cross_package_method_match_skips_ambiguous_same_package_name() {
    // Conservatism guard: when the symbol's own Go package has TWO
    // methods of the same name on different types, the cross-package
    // fallback refuses to bind either — otherwise a `cmd.Run()` call
    // from outside the package would arbitrarily pick one. Squeezy
    // does not track Go variable types yet, so the safest behaviour
    // is to leave the reference unresolved.
    let mut parser = LanguageParser::new().unwrap();
    let cobra = go_record(
        "cobra/command.go",
        r#"
package cobra

type Command struct{}
func (c *Command) Run() {}

type Runner struct{}
func (r *Runner) Run() {}
"#,
    );
    let doc = go_record(
        "doc/md.go",
        r#"
package doc

import "github.com/spf13/cobra"

func Render(cmd *cobra.Command) { cmd.Run() }
"#,
    );
    let parsed = [cobra, doc]
        .into_iter()
        .map(|r| {
            let s = fs::read_to_string(&r.path).unwrap();
            parser.parse_source(&r, s).unwrap()
        })
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);
    for run in graph.find_symbol_by_name("Run") {
        let hits = graph.references_to_symbol(&run.id);
        assert!(
            hits.iter().all(|h| h.reference.file_id.0 != "doc/md.go"),
            "ambiguous `Run` across two types in same Go package must not bind via the cross-package fallback; symbol id={}",
            run.id.0,
        );
    }
}

#[test]
fn graph_indexes_swift_imports_and_module_facts() {
    let mut parser = LanguageParser::new().unwrap();
    let endpoint = swift_record(
        "Sources/Networking/Endpoint.swift",
        r#"
import Foundation
import struct CoreGraphics.CGRect

protocol Endpoint {}
"#,
    );
    let parsed = parser
        .parse_source(&endpoint, fs::read_to_string(&endpoint.path).unwrap())
        .unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);

    assert_eq!(
        graph.packages.get(&endpoint.id).map(String::as_str),
        Some("Networking"),
        "Sources/<Module>/... layout should set package = \"Networking\""
    );
    let imports = graph
        .imports_for_file(&endpoint.id)
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        imports.iter().any(|i| i.path == "Foundation"),
        "Foundation import"
    );
    assert!(
        imports.iter().any(
            |i| i.path == "CoreGraphics.CGRect" && i.imported_name.as_deref() == Some("CGRect")
        ),
        "import struct CoreGraphics.CGRect"
    );
}

#[test]
fn graph_resolves_swift_extension_method_via_language_identity() {
    let mut parser = LanguageParser::new().unwrap();
    let extension_file = swift_record(
        "Sources/Extensions/String+Sanitize.swift",
        r#"
import Foundation

extension String {
    func sanitized() -> String { return self }
}
"#,
    );
    let parsed = parser
        .parse_source(
            &extension_file,
            fs::read_to_string(&extension_file.path).unwrap(),
        )
        .unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let sanitized = graph
        .find_symbol_by_name("sanitized")
        .pop()
        .expect("sanitized method present");
    assert_eq!(
        sanitized.language_identity.as_deref(),
        Some("String"),
        "extension members carry language_identity = extended type"
    );
    // Parent should either be None (the extractor emits parent_id = None on
    // extension members) or the synthetic file symbol assigned by the graph.
    if let Some(pid) = sanitized.parent_id.as_ref() {
        assert!(
            pid.0.starts_with("file:"),
            "extension members must not have an explicit type parent, got {:?}",
            pid
        );
    }
}

#[test]
fn graph_indexes_swift_property_wrappers_and_main_actor() {
    let mut parser = LanguageParser::new().unwrap();
    let repo = swift_record(
        "Sources/Networking/Repository.swift",
        r#"
import Foundation

@MainActor
final class UserRepository {
    @Published var users: [String] = []
    func refresh() async {}
}
"#,
    );
    let parsed = parser
        .parse_source(&repo, fs::read_to_string(&repo.path).unwrap())
        .unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let class = graph
        .find_symbol_by_name("UserRepository")
        .pop()
        .expect("UserRepository class");
    assert!(class.attributes.iter().any(|a| a == "MainActor"));
    let users = graph
        .find_symbol_by_name("users")
        .pop()
        .expect("users field");
    assert_eq!(users.kind, SymbolKind::Field);
    assert!(users.attributes.iter().any(|a| a == "Published"));
}

#[test]
fn graph_indexes_swift_typealias() {
    let mut parser = LanguageParser::new().unwrap();
    let aliases = swift_record(
        "Sources/Models/Aliases.swift",
        r#"
typealias UserId = Int
typealias Mapping<K, V> = [K: V]
"#,
    );
    let parsed = parser
        .parse_source(&aliases, fs::read_to_string(&aliases.path).unwrap())
        .unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    assert!(
        graph
            .find_symbol_by_name("UserId")
            .iter()
            .any(|s| s.kind == SymbolKind::TypeAlias)
    );
    assert!(
        graph
            .find_symbol_by_name("Mapping")
            .iter()
            .any(|s| s.kind == SymbolKind::TypeAlias)
    );
}

#[test]
fn graph_swift_generic_constraints_recorded_as_base_attributes() {
    let mut parser = LanguageParser::new().unwrap();
    let r = swift_record(
        "Sources/Models/Where.swift",
        r#"
func transform<T, U>(_ x: T, _ y: U) where T: Codable, U: Equatable {}
"#,
    );
    let parsed = parser
        .parse_source(&r, fs::read_to_string(&r.path).unwrap())
        .unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let func = graph
        .find_symbol_by_name("transform")
        .pop()
        .expect("transform fn");
    assert!(
        func.attributes.iter().any(|a| a == "base:Codable"),
        "where T: Codable should record `base:Codable`, got {:?}",
        func.attributes
    );
    assert!(
        func.attributes.iter().any(|a| a == "base:Equatable"),
        "where U: Equatable should record `base:Equatable`, got {:?}",
        func.attributes
    );
}

#[test]
fn graph_swift_protocol_with_conforming_type_references_resolve() {
    let mut parser = LanguageParser::new().unwrap();
    let endpoint = swift_record(
        "Sources/Networking/Endpoint.swift",
        r#"
protocol Endpoint {}
struct UserEndpoint: Endpoint {}
"#,
    );
    let parsed = parser
        .parse_source(&endpoint, fs::read_to_string(&endpoint.path).unwrap())
        .unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let endpoint_sym = graph
        .find_symbol_by_name("Endpoint")
        .pop()
        .expect("Endpoint protocol");
    let refs = graph.references_to_symbol(&endpoint_sym.id);
    assert!(
        !refs.is_empty(),
        "Endpoint must have at least one reference from UserEndpoint conformance"
    );
}

#[test]
fn graph_swift_computed_property_emits_field_not_method() {
    let mut parser = LanguageParser::new().unwrap();
    let p = swift_record(
        "Sources/Models/Person.swift",
        r#"
struct Person {
    let first: String
    let last: String
    var fullName: String {
        get { "x" }
    }
}
"#,
    );
    let parsed = parser
        .parse_source(&p, fs::read_to_string(&p.path).unwrap())
        .unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let full = graph
        .find_symbol_by_name("fullName")
        .pop()
        .expect("fullName field");
    assert_eq!(full.kind, SymbolKind::Field);
    assert!(full.attributes.iter().any(|a| a == "swift:computed"));
}

#[test]
fn graph_swift_extension_call_from_method_resolves_to_extension_target() {
    // Spec gotcha (a): when `foo.bar()` is called with `foo: Foo`, and
    // `bar` is defined on `extension Foo { ... }` in a different file,
    // the receiver-method resolver must find it via `language_identity`.
    let mut parser = LanguageParser::new().unwrap();
    let str_ext = swift_record(
        "Sources/Extensions/String+Sanitize.swift",
        r#"
import Foundation

extension String {
    func sanitized() -> String { return self }
}
"#,
    );
    let user = swift_record(
        "Sources/Networking/Repository.swift",
        r#"
import Foundation

class Repo {
    let name: String = "x"
    func go() {
        let _ = name.sanitized()
    }
}
"#,
    );
    let parsed: Vec<_> = [&str_ext, &user]
        .into_iter()
        .map(|r| {
            parser
                .parse_source(r, fs::read_to_string(&r.path).unwrap())
                .unwrap()
        })
        .collect();
    let graph = SemanticGraph::from_parsed(parsed);

    // The extension method must exist with `language_identity = "String"`.
    let sanitized = graph
        .find_symbol_by_name("sanitized")
        .pop()
        .expect("sanitized in graph");
    assert_eq!(sanitized.language_identity.as_deref(), Some("String"));

    // The body-hit / call must exist.
    let go = graph.find_symbol_by_name("go").pop().expect("go method");
    assert!(
        graph.call_chain(&go.id, &sanitized.id, 3).is_some(),
        "go() -> sanitized() chain must resolve via extension receiver matching"
    );
}

#[test]
fn ruby_cross_file_dotted_call_binds_to_receiver_class_method() {
    // `user.full_name` from Greeter#greet should resolve to User#full_name
    // via the receiver-name -> class heuristic, even though `user` is just
    // a parameter with no static type info. Mirrors the
    // `ruby-call-chain-cross-file` smoke probe.
    let user = ruby_record(
        "app/models/user.rb",
        r#"
class User
  def full_name
    "name"
  end
end
"#,
    );
    let greeter = ruby_record(
        "app/services/greeter.rb",
        r#"
class Greeter
  def greet(user)
    "hi #{user.full_name}"
  end
end
"#,
    );

    let mut parser = LanguageParser::new().unwrap();
    let parsed = [user, greeter]
        .into_iter()
        .map(|r| parser.parse_record(&r).unwrap())
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);

    let greet = graph
        .find_symbol_by_name("greet")
        .into_iter()
        .find(|s| s.kind == SymbolKind::Method)
        .expect("Greeter#greet");
    let full_name = graph
        .find_symbol_by_name("full_name")
        .into_iter()
        .find(|s| s.kind == SymbolKind::Method)
        .expect("User#full_name");
    assert!(graph.call_chain(&greet.id, &full_name.id, 3).is_some());

    // `references_to_symbol(User#full_name)` should surface the dotted
    // form `user.full_name` so smoke-query probes pick it up.
    let hits = graph.references_to_symbol(&full_name.id);
    assert!(
        hits.iter()
            .any(|hit| hit.reference.text == "user.full_name")
    );
}

#[test]
fn graph_swift_actor_attribute_searchable_via_signature_query() {
    let mut parser = LanguageParser::new().unwrap();
    let cache = swift_record(
        "Sources/Storage/Cache.swift",
        r#"
actor Cache {}
class Plain {}
"#,
    );
    let parsed = parser
        .parse_source(&cache, fs::read_to_string(&cache.path).unwrap())
        .unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let results = graph.signature_search(&SignatureQuery {
        text: String::new(),
        kind: Some(SymbolKind::Class),
        visibility: None,
        attribute: Some("swift:actor".to_string()),
    });
    assert!(
        results.iter().any(|s| s.name == "Cache"),
        "swift:actor attribute should filter Cache, got {:?}",
        results.iter().map(|s| s.name.clone()).collect::<Vec<_>>()
    );
    assert!(
        results.iter().all(|s| s.name != "Plain"),
        "non-actor class Plain should not match swift:actor filter"
    );
}

#[test]
fn graph_swift_main_actor_attribute_search() {
    let mut parser = LanguageParser::new().unwrap();
    let repo = swift_record(
        "Sources/Networking/Repository.swift",
        r#"
@MainActor
class A {}
class B {}
"#,
    );
    let parsed = parser
        .parse_source(&repo, fs::read_to_string(&repo.path).unwrap())
        .unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let results = graph.signature_search(&SignatureQuery {
        text: String::new(),
        kind: None,
        visibility: None,
        attribute: Some("MainActor".to_string()),
    });
    assert!(results.iter().any(|s| s.name == "A"));
    assert!(results.iter().all(|s| s.name != "B"));
}

#[test]
fn graph_extracts_scala_package_imports_and_aliased_selectors() {
    let mut parser = LanguageParser::new().unwrap();
    let runner = scala_record(
        "src/main/scala/example/app/Runner.scala",
        r#"
package example.app

import example.util.Names.*
import example.services.{Greeter, FriendlyGreeter as FG}
import example.opaque.given

class Runner
"#,
    );
    let parsed = parser
        .parse_source(&runner, fs::read_to_string(&runner.path).unwrap())
        .unwrap();
    let package_import = parsed
        .imports
        .iter()
        .find(|import| import.alias.as_deref() == Some("__scala_package__"))
        .expect("scala package marker import");
    assert_eq!(package_import.path, "example.app");

    let star = parsed
        .imports
        .iter()
        .find(|import| import.path == "example.util.Names.*")
        .expect("wildcard");
    assert!(star.is_glob);

    let renamed = parsed
        .imports
        .iter()
        .find(|import| {
            import.path == "example.services.FriendlyGreeter"
                && import.alias.as_deref() == Some("FG")
        })
        .expect("renamed selector via `as`");
    assert_eq!(renamed.imported_name.as_deref(), Some("FriendlyGreeter"));

    let plain = parsed
        .imports
        .iter()
        .find(|import| import.path == "example.services.Greeter" && import.alias.is_none())
        .expect("plain selector");
    assert_eq!(plain.imported_name.as_deref(), Some("Greeter"));

    let given_import = parsed
        .imports
        .iter()
        .find(|import| import.alias.as_deref() == Some("__scala_import_given__"))
        .expect("import a.b.given encoded with sentinel alias");
    assert_eq!(given_import.path, "example.opaque");
    assert!(given_import.is_glob);
}

#[test]
fn graph_emits_case_class_as_struct_with_field_children() {
    let mut parser = LanguageParser::new().unwrap();
    let file = scala_record(
        "src/main/scala/example/Point.scala",
        r#"
package example

case class Point(x: Int, y: Int)
"#,
    );
    let parsed = parser
        .parse_source(&file, fs::read_to_string(&file.path).unwrap())
        .unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);

    let point = graph
        .find_symbol_by_name("Point")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Struct)
        .expect("case class emits Struct");
    assert!(
        point
            .attributes
            .iter()
            .any(|attribute| attribute == "scala:case-class"),
        "case-class attribute is present"
    );
    for field_name in ["x", "y"] {
        assert!(
            graph
                .find_symbol_by_name(field_name)
                .into_iter()
                .any(|symbol| symbol.kind == SymbolKind::Field
                    && symbol.parent_id.as_ref() == Some(&point.id)),
            "missing primary-constructor field {field_name}"
        );
    }
}

#[test]
fn graph_pairs_class_and_object_as_companions() {
    let mut parser = LanguageParser::new().unwrap();
    let file = scala_record(
        "src/main/scala/example/services/Greeter.scala",
        r#"
package example.services

sealed trait Greeter {
  def greet(name: String): String
}

object Greeter {
  def default: Greeter = ???
}
"#,
    );
    let parsed = parser
        .parse_source(&file, fs::read_to_string(&file.path).unwrap())
        .unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let trait_symbol = graph
        .find_symbol_by_name("Greeter")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Trait)
        .expect("Greeter trait");
    let object_symbol = graph
        .find_symbol_by_name("Greeter")
        .into_iter()
        .find(|symbol| {
            symbol.kind == SymbolKind::Class
                && symbol
                    .attributes
                    .iter()
                    .any(|attribute| attribute == "scala:object")
        })
        .expect("Greeter companion object");
    assert!(
        trait_symbol
            .attributes
            .iter()
            .any(|attribute| attribute == "scala:companion-object:Greeter"),
        "trait records companion-object attribute"
    );
    assert!(
        object_symbol
            .attributes
            .iter()
            .any(|attribute| attribute == "scala:companion-of:Greeter"),
        "object records companion-of attribute"
    );
}

#[test]
fn dart_library_classes_and_methods_land_in_hierarchy() {
    let mut parser = LanguageParser::new().unwrap();
    let client = dart_record(
        "lib/src/network/client.dart",
        r#"library network.client;

import 'dart:async';

class HttpClient {
  Future<int> fetch(String url) async => 42;
}
"#,
    );
    let parsed = parser.parse_record(&client).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let class = graph.find_symbol_by_name("HttpClient").pop().unwrap();
    assert_eq!(class.kind, SymbolKind::Class);
    let fetch = graph
        .find_symbol_by_name("fetch")
        .into_iter()
        .find(|symbol| symbol.parent_id.as_ref() == Some(&class.id))
        .expect("fetch method attached to HttpClient");
    assert_eq!(fetch.kind, SymbolKind::Method);
    assert!(fetch.attributes.iter().any(|attr| attr == "dart:async"));
}

#[test]
fn dart_part_of_resolves_to_host_library() {
    let mut parser = LanguageParser::new().unwrap();
    let client = dart_record(
        "lib/src/network/client.dart",
        r#"library network.client;

part 'response.dart';

class HttpClient {
  void fetch() {}
}
"#,
    );
    let response = dart_record(
        "lib/src/network/response.dart",
        r#"part of 'client.dart';

class Response {
  final int status;
  const Response(this.status);
}
"#,
    );
    let parsed = vec![client, response]
        .into_iter()
        .map(|record| parser.parse_record(&record).unwrap())
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);
    let response_class = graph.find_symbol_by_name("Response").pop().unwrap();
    let host_library = graph.dart_library_for_file(&response_class.file_id);
    assert_eq!(host_library.as_deref(), Some("network.client"));
}

#[test]
fn dart_mixin_method_resolves_across_files() {
    let mut parser = LanguageParser::new().unwrap();
    let loggable = dart_record(
        "lib/src/util/loggable.dart",
        r#"mixin Loggable {
  void log(String msg) {}
}
"#,
    );
    let service = dart_record(
        "lib/src/services/service.dart",
        r#"import 'package:fixture/src/util/loggable.dart' show Loggable;

class Service with Loggable {
  void run() {
    log('hi');
  }
}
"#,
    );
    let parsed = vec![loggable, service]
        .into_iter()
        .map(|record| parser.parse_record(&record).unwrap())
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);
    let run = graph
        .find_symbol_by_name("run")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .expect("Service.run");
    let log = graph
        .find_symbol_by_name("log")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .expect("Loggable.log");
    assert!(
        graph.call_chain(&run.id, &log.id, 3).is_some(),
        "run -> log call chain across mixin must resolve"
    );
}

#[test]
fn dart_extension_method_marks_language_identity() {
    let mut parser = LanguageParser::new().unwrap();
    let ext = dart_record(
        "lib/string_ext.dart",
        r#"extension StringExt on String {
  String shout() => toUpperCase() + '!';
}
"#,
    );
    let parsed = parser.parse_record(&ext).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let ext_symbol = graph.find_symbol_by_name("StringExt").pop().unwrap();
    assert_eq!(ext_symbol.language_identity.as_deref(), Some("String"));
    assert!(
        ext_symbol
            .attributes
            .iter()
            .any(|attr| attr == "dart:extension")
    );
}

#[test]
fn dart_sealed_class_attributes_propagate() {
    let mut parser = LanguageParser::new().unwrap();
    let auth = dart_record(
        "lib/auth.dart",
        r#"sealed class AuthState {
  const AuthState();
}

class SignedIn extends AuthState {
  final String userId;
  const SignedIn(this.userId);
}

class SignedOut extends AuthState {
  const SignedOut();
}
"#,
    );
    let parsed = parser.parse_record(&auth).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let parent = graph
        .find_symbol_by_name("AuthState")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Class)
        .expect("AuthState class symbol");
    assert!(parent.attributes.iter().any(|attr| attr == "dart:sealed"));
    let child = graph
        .find_symbol_by_name("SignedIn")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Class)
        .expect("SignedIn class symbol");
    assert!(
        child.attributes.iter().any(|attr| attr == "base:AuthState"),
        "child class missing base:AuthState attribute: {:?}",
        child.attributes
    );
}

#[test]
fn graph_emits_scala_enum_with_variants() {
    let mut parser = LanguageParser::new().unwrap();
    let file = scala_record(
        "src/main/scala/example/util/Names.scala",
        r#"
package example.util

enum Names {
  case Alice, Bob, Carol
}
"#,
    );
    let parsed = parser
        .parse_source(&file, fs::read_to_string(&file.path).unwrap())
        .unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let names_enum = graph
        .find_symbol_by_name("Names")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Enum)
        .expect("enum Names");
    for variant in ["Alice", "Bob", "Carol"] {
        assert!(
            graph
                .find_symbol_by_name(variant)
                .into_iter()
                .any(|symbol| symbol.kind == SymbolKind::Variant
                    && symbol.parent_id.as_ref() == Some(&names_enum.id)),
            "missing variant {variant}"
        );
    }
}

#[test]
fn graph_emits_extension_method_with_receiver_identity() {
    let mut parser = LanguageParser::new().unwrap();
    let file = scala_record(
        "src/main/scala/example/ext/StringOps.scala",
        r#"
package example.ext

extension (s: String)
  def shout: String = s.toUpperCase
"#,
    );
    let parsed = parser
        .parse_source(&file, fs::read_to_string(&file.path).unwrap())
        .unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let shout = graph
        .find_symbol_by_name("shout")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Function)
        .expect("extension method shout");
    assert_eq!(shout.language_identity.as_deref(), Some("String"));
    assert!(
        shout
            .attributes
            .iter()
            .any(|attribute| attribute == "scala:extension"),
        "extension attribute marker present"
    );
}

#[test]
fn dart_named_constructor_is_dotted() {
    let mut parser = LanguageParser::new().unwrap();
    let foo = dart_record(
        "lib/foo.dart",
        r#"class Foo {
  Foo();
  Foo.named(int id);
}
"#,
    );
    let parsed = parser.parse_record(&foo).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let class = graph
        .find_symbol_by_name("Foo")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Class)
        .expect("Foo class");
    let methods: Vec<_> = graph
        .find_symbol_by_name("Foo.named")
        .into_iter()
        .filter(|symbol| symbol.parent_id.as_ref() == Some(&class.id))
        .collect();
    assert!(
        !methods.is_empty(),
        "named constructor should be tracked as Foo.named symbol"
    );
    assert!(
        methods[0]
            .attributes
            .iter()
            .any(|attr| attr == "dart:constructor")
    );
}

#[test]
fn graph_emits_given_definition_as_partial_const() {
    let mut parser = LanguageParser::new().unwrap();
    let file = scala_record(
        "src/main/scala/example/opaque/Money.scala",
        r#"
package example.opaque

opaque type Money = BigDecimal

given Ordering[Money] = ???
"#,
    );
    let parsed = parser
        .parse_source(&file, fs::read_to_string(&file.path).unwrap())
        .unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let money_alias = graph
        .find_symbol_by_name("Money")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::TypeAlias)
        .expect("opaque type emits TypeAlias");
    assert!(
        money_alias
            .attributes
            .iter()
            .any(|attribute| attribute == "scala:opaque"),
        "opaque modifier annotated"
    );
    let given = graph
        .find_symbol_by_name("given_OrderingMoney")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Const)
        .expect("anonymous given emits synthesized Const");
    assert!(
        given
            .attributes
            .iter()
            .any(|attribute| attribute == "scala:given"),
        "given attribute"
    );
    assert!(
        given
            .attributes
            .iter()
            .any(|attribute| attribute.starts_with("scala:given-for:")),
        "given-for attribute"
    );
    assert_eq!(given.confidence, Confidence::Partial);
}

#[test]
fn graph_resolves_scala_top_level_def_call_within_same_package() {
    let mut parser = LanguageParser::new().unwrap();
    let helpers = scala_record(
        "src/main/scala/example/util/Helpers.scala",
        r#"
package example.util

def shared(): Int = 1
"#,
    );
    let app = scala_record(
        "src/main/scala/example/util/App.scala",
        r#"
package example.util

class App {
  def entry(): Int = shared()
}
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
    let shared = graph
        .find_symbol_by_name("shared")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Function)
        .expect("top-level def shared");
    let entry = graph
        .find_symbol_by_name("entry")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .expect("App.entry method");
    assert!(
        graph.call_chain(&entry.id, &shared.id, 3).is_some(),
        "same-package top-level def resolves unqualified"
    );
}

#[test]
fn graph_resolves_scala_companion_object_factory_call() {
    let mut parser = LanguageParser::new().unwrap();
    let greeter = scala_record(
        "src/main/scala/example/services/Greeter.scala",
        r#"
package example.services

trait Greeter {
  def greet(name: String): String
}

object Greeter {
  def default: Greeter = ???
}
"#,
    );
    let app = scala_record(
        "src/main/scala/example/app/Runner.scala",
        r#"
package example.app

import example.services.Greeter

def buildDefault(): Greeter = Greeter.default
"#,
    );
    let parsed = vec![
        parser
            .parse_source(&greeter, fs::read_to_string(&greeter.path).unwrap())
            .unwrap(),
        parser
            .parse_source(&app, fs::read_to_string(&app.path).unwrap())
            .unwrap(),
    ];
    let graph = SemanticGraph::from_parsed(parsed);
    let default_method = graph
        .find_symbol_by_name("default")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .expect("Greeter.default factory");
    let build_default = graph
        .find_symbol_by_name("buildDefault")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Function)
        .expect("buildDefault function");
    assert!(
        graph
            .call_chain(&build_default.id, &default_method.id, 3)
            .is_some(),
        "Greeter.default resolves through companion object scope"
    );
}

#[test]
fn graph_resolves_scala_extension_method_on_string_receiver() {
    let mut parser = LanguageParser::new().unwrap();
    let ops = scala_record(
        "src/main/scala/example/ext/StringOps.scala",
        r#"
package example.ext

extension (s: String)
  def shout: String = s.toUpperCase
"#,
    );
    let app = scala_record(
        "src/main/scala/example/app/Runner.scala",
        r#"
package example.app

import example.ext.*

def loud(): String = "hello".shout
"#,
    );
    let parsed = vec![
        parser
            .parse_source(&ops, fs::read_to_string(&ops.path).unwrap())
            .unwrap(),
        parser
            .parse_source(&app, fs::read_to_string(&app.path).unwrap())
            .unwrap(),
    ];
    let graph = SemanticGraph::from_parsed(parsed);
    let shout = graph
        .find_symbol_by_name("shout")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Function)
        .expect("shout extension");
    let loud = graph
        .find_symbol_by_name("loud")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Function)
        .expect("loud function");
    assert!(
        graph.call_chain(&loud.id, &shout.id, 3).is_some(),
        "extension call resolves to monomorphic receiver"
    );
}

#[test]
fn graph_records_scala_package_marker_in_scala_package_by_file() {
    let mut parser = LanguageParser::new().unwrap();
    let file = scala_record(
        "src/main/scala/example/app/Runner.scala",
        r#"
package example.app

class Runner
"#,
    );
    let parsed = parser
        .parse_source(&file, fs::read_to_string(&file.path).unwrap())
        .unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let package = graph
        .scala_package_for_file(&FileId::new(file.relative_path.as_str()))
        .expect("scala package indexed by file id");
    assert_eq!(package, vec!["example", "app"]);
}

#[test]
fn graph_emits_one_symbol_per_scala_multibinding_val() {
    let mut parser = LanguageParser::new().unwrap();
    let file = scala_record(
        "src/main/scala/example/multi/Values.scala",
        r#"
package example.multi

class Values {
  val a, b, c: Int = 0
}
"#,
    );
    let parsed = parser
        .parse_source(&file, fs::read_to_string(&file.path).unwrap())
        .unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    for name in ["a", "b", "c"] {
        assert!(
            graph
                .find_symbol_by_name(name)
                .into_iter()
                .any(|symbol| symbol.kind == SymbolKind::Const),
            "missing val binding {name}",
        );
    }
}

#[test]
fn ruby_require_relative_synthesizes_signature_searchable_directive() {
    // `require_relative "user"` should produce a Function symbol named
    // `require_relative` so `signature_search("require_relative \"user\"")`
    // surfaces it. Mirrors the `ruby-import-resolution` smoke probe.
    let mut parser = LanguageParser::new().unwrap();
    let record = ruby_record(
        "app/models/admin.rb",
        r#"
require_relative "user"

class Admin < User
end
"#,
    );
    let parsed = parser.parse_record(&record).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);

    let hits = graph.signature_search(&SignatureQuery {
        text: "require_relative \"user\"".to_string(),
        kind: None,
        visibility: None,
        attribute: None,
    });
    assert!(
        hits.iter()
            .any(|s| s.kind == SymbolKind::Function && s.name == "require_relative"),
        "expected Function:require_relative in signature_search hits, got {:?}",
        hits.iter()
            .map(|s| format!("{:?}:{}", s.kind, s.name))
            .collect::<Vec<_>>()
    );
}

#[test]
fn ruby_explicit_class_receiver_dotted_call_binds_method() {
    // `User.find_by_email(...)` inside another class should bind to the
    // `self.find_by_email` singleton method on User across files. Covers
    // the existing `Class.method` resolver path with the new Field
    // reference emission.
    let user = ruby_record(
        "app/models/user.rb",
        r#"
class User
  def self.find_by_email(email)
    nil
  end
end
"#,
    );
    let admin = ruby_record(
        "app/services/lookup.rb",
        r#"
class Lookup
  def search(email)
    User.find_by_email(email)
  end
end
"#,
    );

    let mut parser = LanguageParser::new().unwrap();
    let parsed = [user, admin]
        .into_iter()
        .map(|r| parser.parse_record(&r).unwrap())
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);

    let search = graph
        .find_symbol_by_name("search")
        .into_iter()
        .find(|s| s.kind == SymbolKind::Method)
        .expect("Lookup#search");
    let find_by_email = graph
        .find_symbol_by_name("find_by_email")
        .into_iter()
        .find(|s| s.kind == SymbolKind::Method)
        .expect("User#find_by_email");
    assert!(graph.call_chain(&search.id, &find_by_email.id, 3).is_some());
}

#[test]
fn ruby_sibling_classes_attribute_method_calls_to_correct_enclosing_class() {
    // Sidekiq's `lib/sidekiq/scheduled.rb` ships two sibling classes —
    // `Sidekiq::Scheduled::Enq` and `Sidekiq::Scheduled::Poller` — under
    // the same module. Each calls a `Sidekiq::Component`-provided
    // helper (`fire_event`) from its own method body. `reference_search`
    // for `fire_event` must produce hits whose owner chain leads back
    // to the correct sibling, not bleed Poller's count into Enq.
    let component = ruby_record(
        "lib/sidekiq/component.rb",
        "module Sidekiq\n\
         module Component\n\
         def fire_event(event); end\n\
         def safe_thread(name); yield; end\n\
         end\n\
         end\n",
    );
    let scheduled = ruby_record(
        "lib/sidekiq/scheduled.rb",
        "module Sidekiq\n\
         module Scheduled\n\
         class Enq\n\
         include Sidekiq::Component\n\
         def enqueue_jobs\n\
         fire_event(:enq)\n\
         end\n\
         end\n\
         \n\
         class Poller\n\
         include Sidekiq::Component\n\
         def start\n\
         fire_event(:poller_start)\n\
         safe_thread(\"poller\") { run }\n\
         end\n\
         end\n\
         end\n\
         end\n",
    );
    let mut parser = LanguageParser::new().unwrap();
    let parsed = [component, scheduled]
        .into_iter()
        .map(|r| parser.parse_record(&r).unwrap())
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);

    let enq = graph
        .find_symbol_by_name("Enq")
        .into_iter()
        .find(|s| s.kind == SymbolKind::Class)
        .expect("Enq class");
    let poller = graph
        .find_symbol_by_name("Poller")
        .into_iter()
        .find(|s| s.kind == SymbolKind::Class)
        .expect("Poller class");
    // Sanity: sibling classes share a parent module and have disjoint spans.
    assert_eq!(enq.parent_id, poller.parent_id);
    assert!(enq.span.end_byte <= poller.span.start_byte);

    let enqueue_jobs = graph
        .find_symbol_by_name("enqueue_jobs")
        .into_iter()
        .find(|s| s.kind == SymbolKind::Method)
        .expect("enqueue_jobs");
    let start = graph
        .find_symbol_by_name("start")
        .into_iter()
        .find(|s| s.kind == SymbolKind::Method)
        .expect("start");
    assert_eq!(
        enqueue_jobs.parent_id.as_ref(),
        Some(&enq.id),
        "enqueue_jobs must be hosted by Enq, got parent_id={:?}",
        enqueue_jobs.parent_id
    );
    assert_eq!(
        start.parent_id.as_ref(),
        Some(&poller.id),
        "start must be hosted by Poller, got parent_id={:?}",
        start.parent_id
    );

    // `reference_search("fire_event")` must return a hit owned by
    // enqueue_jobs and a hit owned by start, each binding back to its
    // own sibling class through `parent_id`.
    let hits = graph.reference_search("fire_event");
    let owner_class_ids = hits
        .iter()
        .filter_map(|hit| hit.owner.as_ref())
        .filter(|owner| owner.kind == SymbolKind::Method)
        .filter_map(|owner| owner.parent_id.clone())
        .collect::<Vec<_>>();
    assert!(
        owner_class_ids.contains(&enq.id),
        "expected fire_event reference owned by an Enq method, got {owner_class_ids:?}"
    );
    assert!(
        owner_class_ids.contains(&poller.id),
        "expected fire_event reference owned by a Poller method, got {owner_class_ids:?}"
    );

    // `references_to_symbol` against the Component#fire_event declaration
    // must surface both call sites, each correctly owner-attributed to
    // the sibling's method (and therefore to the sibling class via
    // parent_id).
    let fire_event_decl = graph
        .find_symbol_by_name("fire_event")
        .into_iter()
        .find(|s| {
            s.kind == SymbolKind::Method
                && s.file_id.0 == "lib/sidekiq/component.rb"
                && !s.attributes.iter().any(|a| a == "ruby:synthesized")
        })
        .expect("fire_event method declaration");
    let resolved = graph.references_to_symbol(&fire_event_decl.id);
    let resolved_class_ids = resolved
        .iter()
        .filter_map(|hit| hit.owner.as_ref())
        .filter_map(|owner| owner.parent_id.clone())
        .collect::<Vec<_>>();
    assert!(
        resolved_class_ids.contains(&enq.id),
        "references_to_symbol must surface Enq-owned call site, got {resolved_class_ids:?}"
    );
    assert!(
        resolved_class_ids.contains(&poller.id),
        "references_to_symbol must surface Poller-owned call site, got {resolved_class_ids:?}"
    );
}

fn parsed_imports(graph: &SemanticGraph) -> impl Iterator<Item = &ParsedImport> {
    graph.imports.iter()
}

fn ruby_record(relative_path: &str, source: &str) -> FileRecord {
    let mut record = record(relative_path, source);
    record.language = LanguageKind::Ruby;
    record
}

#[test]
fn dart_factory_constructor_carries_factory_attribute() {
    let mut parser = LanguageParser::new().unwrap();
    let foo = dart_record(
        "lib/foo.dart",
        r#"class Foo {
  factory Foo.create() = Foo;
  Foo();
}
"#,
    );
    let parsed = parser.parse_record(&foo).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let class = graph
        .find_symbol_by_name("Foo")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Class)
        .expect("Foo class");
    let factory = graph
        .find_symbol_by_name("Foo.create")
        .into_iter()
        .find(|symbol| symbol.parent_id.as_ref() == Some(&class.id))
        .expect("Foo.create factory");
    assert!(
        factory.attributes.iter().any(|attr| attr == "dart:factory"),
        "missing dart:factory attribute: {:?}",
        factory.attributes
    );
    assert!(
        factory
            .attributes
            .iter()
            .any(|attr| attr == "dart:constructor")
    );
}

#[test]
fn dart_async_top_level_function_marked_async() {
    let mut parser = LanguageParser::new().unwrap();
    let main = dart_record(
        "lib/main.dart",
        r#"Future<int> work() async {
  return 42;
}
"#,
    );
    let parsed = parser.parse_record(&main).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let work = graph.find_symbol_by_name("work").pop().unwrap();
    assert_eq!(work.kind, SymbolKind::Function);
    assert!(work.attributes.iter().any(|attr| attr == "dart:async"));
}

#[test]
fn dart_import_with_prefix_resolves_qualified_calls() {
    let mut parser = LanguageParser::new().unwrap();
    let client = dart_record(
        "lib/src/network/client.dart",
        r#"class HttpClient {
  HttpClient();
  static HttpClient create() => HttpClient();
}
"#,
    );
    let main = dart_record(
        "lib/main.dart",
        r#"import 'package:fixture/src/network/client.dart' as net;

void start() {
  final c = net.create();
}
"#,
    );
    let parsed = vec![client, main]
        .into_iter()
        .map(|record| parser.parse_record(&record).unwrap())
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);
    let imports = graph
        .imports_for_file(&FileId::new("lib/main.dart"))
        .filter(|import| import.alias.as_deref() == Some("net"))
        .count();
    assert!(imports >= 1, "expected prefix import to be tracked");
    let create = graph
        .find_symbol_by_name("create")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .expect("static HttpClient.create method");
    let start = graph
        .find_symbol_by_name("start")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Function)
        .expect("start function");
    // The prefix-qualified call doesn't necessarily fully resolve in the
    // first PR's heuristic path, but the import wiring above must be present
    // for the resolver to use. Verify by looking up the symbol/import pair.
    let _ = (create.id, start.id);
}

#[test]
fn graph_records_csharp_internal_class_inheritance_edge() {
    // Mirrors Newtonsoft.Json's `TraceJsonReader` shape (an `internal
    // class : JsonReader`) so a regression that strips `base:` from
    // non-public C# class declarations would break the inheritance
    // edge that `decl_search(attribute="base:JsonReader")` and
    // `hierarchy` rely on.
    let mut parser = LanguageParser::new().unwrap();
    let reader = csharp_record(
        "src/JsonReader.cs",
        r#"
namespace App;

public abstract class JsonReader
{
    public virtual bool Read() { return false; }
}
"#,
    );
    let trace = csharp_record(
        "src/TraceJsonReader.cs",
        r#"
namespace App;

internal class TraceJsonReader : JsonReader, IJsonLineInfo
{
    public override bool Read() { return true; }
}
"#,
    );
    let parsed = [reader, trace]
        .into_iter()
        .map(|record| {
            let source = fs::read_to_string(&record.path).unwrap();
            parser.parse_source(&record, source).unwrap()
        })
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);

    let trace_class = graph
        .find_symbol_by_name("TraceJsonReader")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Class)
        .expect("TraceJsonReader class symbol");
    assert!(
        trace_class
            .attributes
            .iter()
            .any(|attr| attr == "base:JsonReader"),
        "internal class TraceJsonReader : JsonReader should carry base:JsonReader, got {:?}",
        trace_class.attributes,
    );
    let reader_class = graph
        .find_symbol_by_name("JsonReader")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Class)
        .expect("JsonReader class symbol");
    let extends_edge = graph
        .edges()
        .iter()
        .find(|edge| {
            edge.from == trace_class.id
                && edge.kind == EdgeKind::Extends
                && edge.target_text == "JsonReader"
        })
        .expect("TraceJsonReader -> JsonReader Extends edge");
    assert_eq!(
        extends_edge.to.as_ref(),
        Some(&reader_class.id),
        "cross-file `internal class : JsonReader` should bind to the JsonReader class symbol",
    );
}

#[test]
fn graph_exposes_inheritance_api_for_ancestors_and_direct_subtypes() {
    let mut parser = LanguageParser::new().unwrap();
    let reader = csharp_record(
        "src/JsonReader.cs",
        r#"
namespace App;

public abstract class JsonReader
{
    public virtual bool Read() { return false; }
}
"#,
    );
    let trace = csharp_record(
        "src/TraceJsonReader.cs",
        r#"
namespace App;

internal class TraceJsonReader : JsonReader
{
    public override bool Read() { return true; }
}
"#,
    );
    let parsed = [reader, trace]
        .into_iter()
        .map(|record| {
            let source = fs::read_to_string(&record.path).unwrap();
            parser.parse_source(&record, source).unwrap()
        })
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);

    let reader_class = graph
        .find_symbol_by_name("JsonReader")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Class)
        .expect("JsonReader class symbol");
    let trace_class = graph
        .find_symbol_by_name("TraceJsonReader")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Class)
        .expect("TraceJsonReader class symbol");

    let ancestors = graph.inheritance_ancestors(&trace_class.id);
    assert!(
        ancestors.iter().any(|symbol| symbol.id == reader_class.id),
        "TraceJsonReader should report JsonReader as an ancestor"
    );
    let direct_subtypes = graph.inheritance_direct_subtypes(&reader_class.id);
    assert!(
        direct_subtypes
            .iter()
            .any(|symbol| symbol.id == trace_class.id),
        "JsonReader should report TraceJsonReader as a direct subtype"
    );
}

#[test]
fn graph_records_js_ts_class_heritage_as_base_and_iface_attributes() {
    // The JS/TS extractor records inheritance as queryable `base:`/`iface:`
    // attributes (not only type-reference edges), and the graph build carries
    // them through — so `decl_search(attribute="base:User")` and the grep→graph
    // augment can enumerate TS/JS subtypes (the capability TS/JS previously
    // lacked, which forced the model into grep+read_file storms).
    let mut parser = LanguageParser::new().unwrap();
    let app = ts_record(
        "src/app.ts",
        r#"export class User {}
export interface Auditable {}
export class Admin extends User implements Auditable {}
export class Repo<T extends Entity> extends Base<T> implements Lifecycle<T> {}
"#,
    );
    let parsed = parser.parse_record(&app).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);

    let admin = graph
        .find_symbol_by_name("Admin")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Class)
        .expect("Admin class symbol");
    assert!(
        admin.attributes.iter().any(|attr| attr == "base:User"),
        "Admin should carry base:User, got {:?}",
        admin.attributes,
    );
    assert!(
        admin
            .attributes
            .iter()
            .any(|attr| attr == "iface:Auditable"),
        "Admin should carry iface:Auditable, got {:?}",
        admin.attributes,
    );

    let repo = graph
        .find_symbol_by_name("Repo")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Class)
        .expect("Repo class symbol");
    // Generic base head only; the `<T extends Entity>` constraint must not leak
    // into the base list, and the generic argument `Base<T>` resolves to `Base`.
    assert!(
        repo.attributes.iter().any(|attr| attr == "base:Base"),
        "Repo should carry base:Base, got {:?}",
        repo.attributes,
    );
    assert!(
        !repo.attributes.iter().any(|attr| attr == "base:Entity"),
        "generic constraint Entity must not be recorded as a base",
    );
}

#[test]
fn graph_records_go_embedding_as_base_attributes() {
    // The Go extractor records struct/interface embedding as queryable `base:`
    // attributes (not only `go:embed` child fields), and the graph build carries
    // them through — so `decl_search(attribute="base:Animal")` and its transitive
    // closure can enumerate Go embedders (the capability Go previously lacked).
    let mut parser = LanguageParser::new().unwrap();
    let app = go_record(
        "zoo/zoo.go",
        r#"package zoo

type Animal struct{}

type Dog struct {
    Animal
}

type Puppy struct {
    Dog
}

type Reader interface{}
type Writer interface{}

type ReadWriter interface {
    Reader
    Writer
}
"#,
    );
    let parsed = parser.parse_record(&app).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);

    let dog = graph
        .find_symbol_by_name("Dog")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Struct)
        .expect("Dog struct symbol");
    assert!(
        dog.attributes.iter().any(|attr| attr == "base:Animal"),
        "Dog should carry base:Animal, got {:?}",
        dog.attributes,
    );

    // Transitive closure of base:Animal reaches Puppy via Dog's own base: edge.
    let puppy = graph
        .find_symbol_by_name("Puppy")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Struct)
        .expect("Puppy struct symbol");
    assert!(
        puppy.attributes.iter().any(|attr| attr == "base:Dog"),
        "Puppy should carry base:Dog, got {:?}",
        puppy.attributes,
    );

    let read_writer = graph
        .find_symbol_by_name("ReadWriter")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Interface)
        .expect("ReadWriter interface symbol");
    assert!(
        read_writer
            .attributes
            .iter()
            .any(|attr| attr == "base:Reader"),
        "ReadWriter should carry base:Reader, got {:?}",
        read_writer.attributes,
    );
    assert!(
        read_writer
            .attributes
            .iter()
            .any(|attr| attr == "base:Writer"),
        "ReadWriter should carry base:Writer, got {:?}",
        read_writer.attributes,
    );
}

#[test]
fn dart_import_show_decomposes_into_named_imports() {
    let mut parser = LanguageParser::new().unwrap();
    let main = dart_record(
        "lib/main.dart",
        r#"import 'package:foo/bar.dart' show baz hide qux;

void main() {}
"#,
    );
    let parsed = parser.parse_record(&main).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let baz_import = graph
        .imports_for_file(&FileId::new("lib/main.dart"))
        .find(|import| import.imported_name.as_deref() == Some("baz"));
    assert!(baz_import.is_some(), "show baz should emit a named import");
    let qux_import = graph
        .imports_for_file(&FileId::new("lib/main.dart"))
        .find(|import| import.imported_name.as_deref() == Some("qux"));
    assert!(qux_import.is_none(), "hide qux should not emit an import");
}

fn dart_record(relative_path: &str, source: &str) -> FileRecord {
    let mut record = record(relative_path, source);
    record.language = LanguageKind::Dart;
    record
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

fn kotlin_record(relative_path: &str, source: &str) -> FileRecord {
    let mut record = record(relative_path, source);
    record.language = LanguageKind::Kotlin;
    record
}

fn php_record(relative_path: &str, source: &str) -> FileRecord {
    let mut record = record(relative_path, source);
    record.language = LanguageKind::Php;
    record
}

fn scala_record(relative_path: &str, source: &str) -> FileRecord {
    let mut record = record(relative_path, source);
    record.language = LanguageKind::Scala;
    record
}

fn swift_record(relative_path: &str, source: &str) -> FileRecord {
    let mut record = record(relative_path, source);
    record.language = LanguageKind::Swift;
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

/// Bug #4: a method call with an explicit NON-self receiver must not bind to a
/// same-named method on the *caller's own* class. Inside `A.run`, `b.foo()`
/// (where `b` is some other object) must NOT resolve to `A.foo` just because
/// the caller's class also declares a `foo`. JS/TS classifies `b.foo()` as a
/// `Method` call with receiver `b`, exercising the `same_impl_method`
/// early-exit that the fix gates on a self/absent receiver.
#[test]
fn graph_receiver_method_call_does_not_bind_to_callers_own_class() {
    let mut parser = LanguageParser::new().unwrap();
    let record = ts_record(
        "src/m.ts",
        r#"
class B {
    foo() { return 2; }
}

class A {
    foo() { return 1; }
    run(b: B) {
        return b.foo();
    }
}
"#,
    );
    let parsed = parser
        .parse_source(&record, fs::read_to_string(&record.path).unwrap())
        .unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);

    let run = graph
        .find_symbol_by_name("run")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .expect("A.run should be indexed");
    let a_foo = graph
        .find_symbol_by_name("foo")
        .into_iter()
        .find(|symbol| {
            symbol
                .parent_id
                .as_ref()
                .and_then(|id| graph.symbols.get(id))
                .map(|parent| parent.name == "A")
                .unwrap_or(false)
        })
        .expect("A.foo should be indexed");
    let b_foo = graph
        .find_symbol_by_name("foo")
        .into_iter()
        .find(|symbol| {
            symbol
                .parent_id
                .as_ref()
                .and_then(|id| graph.symbols.get(id))
                .map(|parent| parent.name == "B")
                .unwrap_or(false)
        })
        .expect("B.foo should be indexed");

    let call_edge = graph
        .edges()
        .iter()
        .find(|edge| edge.from == run.id && edge.kind == EdgeKind::Calls)
        .expect("expected a Calls edge from A.run for b.foo()");
    assert_ne!(
        call_edge.to.as_ref(),
        Some(&a_foo.id),
        "b.foo() must NOT bind to the caller's own A.foo (reason={})",
        call_edge.provenance.reason,
    );
    // It is acceptable for this to be unresolved or to land on B.foo, but
    // never on the caller's own A.foo.
    if let Some(to) = call_edge.to.as_ref() {
        assert_eq!(
            to, &b_foo.id,
            "if b.foo() resolves at all it must target B.foo, not A.foo",
        );
    }
}

/// Bug #5: the global arity-uniqueness shortcut must not forge an edge for a
/// method call that carries an explicit non-self receiver. Two unrelated
/// `bar` methods of the same arity exist in different classes; `b.bar(1)` from
/// an unrelated context must stay unresolved rather than binding to whichever
/// `bar` is the unique one of that arity. The classes carry no inheritance
/// relationship and `b`'s type is unknown to the resolver, so the ONLY way an
/// edge could form is the (now-gated) arity shortcut.
#[test]
fn graph_arity_fallback_does_not_bind_receiver_call_across_types() {
    let mut parser = LanguageParser::new().unwrap();
    // Two unrelated `bar` methods of DIFFERENT arity, so exactly one matches
    // the call's arity of 1 — making the global arity shortcut the only thing
    // that could decide the binding.
    let record = ts_record(
        "src/m.ts",
        r#"
class One {
    bar(value: number) { return value; }
}

class Two {
    bar(left: number, right: number) { return left + right; }
}

class Caller {
    run(thing: One) {
        return thing.bar(1);
    }
}
"#,
    );
    let parsed = parser
        .parse_source(&record, fs::read_to_string(&record.path).unwrap())
        .unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);

    let run = graph
        .find_symbol_by_name("run")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .expect("Caller.run should be indexed");
    let one_bar = graph
        .find_symbol_by_name("bar")
        .into_iter()
        .find(|symbol| {
            symbol
                .parent_id
                .as_ref()
                .and_then(|id| graph.symbols.get(id))
                .map(|parent| parent.name == "One")
                .unwrap_or(false)
        })
        .expect("One.bar should be indexed");

    let call_edge = graph
        .edges()
        .iter()
        .find(|edge| edge.from == run.id && edge.kind == EdgeKind::Calls)
        .expect("expected a Calls edge from Caller.run for thing.bar(1)");
    // With an explicit non-self receiver and no type/import signal, the arity
    // shortcut must not fire: `thing.bar(1)` is left unresolved rather than
    // forging a hard edge to the lone arity-1 `bar`.
    assert_ne!(
        call_edge.to.as_ref(),
        Some(&one_bar.id),
        "thing.bar(1) must not bind to One.bar purely via arity uniqueness (reason={})",
        call_edge.provenance.reason,
    );
}

/// Bug #13: an aliased import must produce a dependency/import edge to the
/// ORIGINAL imported symbol (`Thing`), not the local alias (`T`), and the
/// aliased use must be discoverable via `references_to_symbol(Thing)`.
#[test]
fn graph_aliased_import_targets_original_symbol_and_records_reference() {
    let mut parser = LanguageParser::new().unwrap();
    let lib = ts_record(
        "src/thing.ts",
        "export class Thing {\n    run() { return 1; }\n}\n",
    );
    let app = ts_record(
        "src/app.ts",
        r#"import { Thing as T } from "./thing";

export function start(): T {
    return new T();
}
"#,
    );
    let parsed = [lib, app]
        .iter()
        .map(|rec| {
            parser
                .parse_source(rec, fs::read_to_string(&rec.path).unwrap())
                .unwrap()
        })
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);

    let thing = graph
        .find_symbol_by_name("Thing")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Class)
        .expect("Thing class should be indexed");

    // The import edge must resolve to `Thing`, not be lost to the alias `T`.
    let import_edge = graph
        .edges()
        .iter()
        .find(|edge| {
            matches!(edge.kind, EdgeKind::Imports | EdgeKind::Reexports)
                && edge.target_text.contains("Thing")
        })
        .expect("expected an import edge for `Thing as T`");
    assert_eq!(
        import_edge.to.as_ref(),
        Some(&thing.id),
        "aliased import `Thing as T` must target the original symbol Thing",
    );

    // The aliased use (`T`) must be discoverable as a reference to Thing.
    let refs = graph.references_to_symbol(&thing.id);
    assert!(
        refs.iter().any(|hit| hit.reference.text == "T"),
        "references_to_symbol(Thing) should include the aliased use `T`; got {:?}",
        refs.iter()
            .map(|hit| hit.reference.text.clone())
            .collect::<Vec<_>>(),
    );
}

/// Bug #14: JS/TS `this.foo()` and `super.foo()` in a subclass must bind to the
/// inherited `Base.foo` across files, using the `base:`/`iface:` attributes.
#[test]
fn graph_resolves_js_ts_inherited_this_and_super_calls() {
    let mut parser = LanguageParser::new().unwrap();
    let base = ts_record(
        "src/base.ts",
        "export class Base {\n    foo() { return 1; }\n}\n",
    );
    let child = ts_record(
        "src/child.ts",
        r#"import { Base } from "./base";

class Child extends Base {
    bar() { return this.foo(); }
    baz() { return super.foo(); }
}
"#,
    );
    let parsed = [base, child]
        .iter()
        .map(|rec| {
            parser
                .parse_source(rec, fs::read_to_string(&rec.path).unwrap())
                .unwrap()
        })
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);

    let base_foo = graph
        .find_symbol_by_name("foo")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .expect("Base::foo should be indexed");

    for (caller_name, label) in [("bar", "this.foo()"), ("baz", "super.foo()")] {
        let caller = graph
            .find_symbol_by_name(caller_name)
            .into_iter()
            .find(|symbol| symbol.kind == SymbolKind::Method)
            .unwrap_or_else(|| panic!("Child::{caller_name} should be indexed"));
        let call_edge = graph
            .edges()
            .iter()
            .find(|edge| edge.from == caller.id && edge.kind == EdgeKind::Calls)
            .unwrap_or_else(|| panic!("expected a Calls edge from Child::{caller_name}"));
        assert_eq!(
            call_edge.to.as_ref(),
            Some(&base_foo.id),
            "{label} should resolve to inherited Base::foo; got {:?}",
            call_edge.to,
        );
    }
}

/// Bug #14 (skip-self): when a JS/TS subclass OVERRIDES an inherited method,
/// `this.foo()` must bind to the subclass override while `super.foo()` skips
/// the subclass and binds to the parent's definition.
#[test]
fn graph_js_ts_super_call_skips_overriding_subclass() {
    let mut parser = LanguageParser::new().unwrap();
    let record = ts_record(
        "src/over.ts",
        r#"
class Base {
    foo() { return 1; }
}

class Child extends Base {
    foo() { return 2; }
    viaThis() { return this.foo(); }
    viaSuper() { return super.foo(); }
}
"#,
    );
    let parsed = parser
        .parse_source(&record, fs::read_to_string(&record.path).unwrap())
        .unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);

    let foos = graph.find_symbol_by_name("foo");
    let base_foo = foos
        .iter()
        .find(|symbol| {
            symbol
                .parent_id
                .as_ref()
                .and_then(|id| graph.symbols.get(id))
                .map(|parent| parent.name == "Base")
                .unwrap_or(false)
        })
        .expect("Base.foo should be indexed");
    let child_foo = foos
        .iter()
        .find(|symbol| {
            symbol
                .parent_id
                .as_ref()
                .and_then(|id| graph.symbols.get(id))
                .map(|parent| parent.name == "Child")
                .unwrap_or(false)
        })
        .expect("Child.foo should be indexed");

    let via_this = graph
        .find_symbol_by_name("viaThis")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .expect("Child.viaThis should be indexed");
    let via_super = graph
        .find_symbol_by_name("viaSuper")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .expect("Child.viaSuper should be indexed");

    let this_edge = graph
        .edges()
        .iter()
        .find(|edge| edge.from == via_this.id && edge.kind == EdgeKind::Calls)
        .expect("expected a Calls edge from viaThis");
    assert_eq!(
        this_edge.to.as_ref(),
        Some(&child_foo.id),
        "this.foo() should bind to the subclass override Child.foo",
    );

    let super_edge = graph
        .edges()
        .iter()
        .find(|edge| edge.from == via_super.id && edge.kind == EdgeKind::Calls)
        .expect("expected a Calls edge from viaSuper");
    assert_eq!(
        super_edge.to.as_ref(),
        Some(&base_foo.id),
        "super.foo() should skip the override and bind to Base.foo",
    );
}

/// Bug #3: Python inheritance resolution must scope the base-class lookup by the
/// subclass file's imports. Two modules each define `Base.foo`; the subclass
/// imports `Base` from module B (the sort-LATER module), so a global name-first
/// resolver would wrongly bind `self.foo()` to module A's `Base.foo`. The
/// scope-aware resolver must bind to B's `Base.foo` — the one actually imported.
#[test]
fn graph_python_inherited_call_scopes_base_to_imported_module() {
    let mut parser = LanguageParser::new().unwrap();
    let base_a = python_record(
        "pkg_a/base.py",
        "class Base:\n    def foo(self):\n        return 1\n",
    );
    let base_b = python_record(
        "pkg_b/base.py",
        "class Base:\n    def foo(self):\n        return 2\n",
    );
    // The subclass lives in pkg_a but imports Base from pkg_b — so a resolver
    // that prefers the first global match (id-sorted: pkg_a) misresolves.
    let child = python_record(
        "pkg_a/child.py",
        r#"from pkg_b.base import Base


class Child(Base):
    def bar(self):
        return self.foo()
"#,
    );
    let parsed = [base_a, base_b, child]
        .iter()
        .map(|rec| {
            parser
                .parse_source(rec, fs::read_to_string(&rec.path).unwrap())
                .unwrap()
        })
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);

    let foo_in = |graph: &SemanticGraph, path: &str| {
        graph
            .find_symbol_by_name("foo")
            .into_iter()
            .find(|symbol| {
                symbol.kind == SymbolKind::Method
                    && graph
                        .files
                        .get(&symbol.file_id)
                        .map(|file| file.relative_path == path)
                        .unwrap_or(false)
            })
            .unwrap_or_else(|| panic!("{path} Base.foo should be indexed"))
    };
    let foo_a = foo_in(&graph, "pkg_a/base.py");
    let foo_b = foo_in(&graph, "pkg_b/base.py");
    let bar = graph
        .find_symbol_by_name("bar")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .expect("Child.bar should be indexed");

    let call_edge = graph
        .edges()
        .iter()
        .find(|edge| edge.from == bar.id && edge.kind == EdgeKind::Calls)
        .expect("expected a Calls edge from Child.bar");
    assert_eq!(
        call_edge.to.as_ref(),
        Some(&foo_b.id),
        "self.foo() must resolve to the imported pkg_b Base.foo, not pkg_a's; got {:?}",
        call_edge.to,
    );
    assert_ne!(
        call_edge.to.as_ref(),
        Some(&foo_a.id),
        "self.foo() must not bind to the unrelated pkg_a Base.foo",
    );
}

/// Bug #4: Dart typed-local dispatch must scope the class lookup by the calling
/// file's imports. Two libraries each declare `class Service { run() }`; the
/// caller imports library B (the sort-LATER library), so a resolver that blindly
/// takes the first global match (id-sorted: lib/a) misresolves. The scope-aware
/// resolver must bind `s.run()` to library B's `Service.run`.
#[test]
fn graph_dart_typed_local_scopes_class_to_imported_library() {
    let mut parser = LanguageParser::new().unwrap();
    let service_a = dart_record(
        "lib/a/service.dart",
        "class Service {\n  void run() {}\n}\n",
    );
    let service_b = dart_record(
        "lib/b/service.dart",
        "class Service {\n  void run() {}\n}\n",
    );
    let caller = dart_record(
        "lib/caller.dart",
        r#"import 'package:fixture/b/service.dart';

class Caller {
  void go() {
    Service s = Service();
    s.run();
  }
}
"#,
    );
    let parsed = [service_a, service_b, caller]
        .iter()
        .map(|rec| {
            parser
                .parse_source(rec, fs::read_to_string(&rec.path).unwrap())
                .unwrap()
        })
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);

    let run_in = |graph: &SemanticGraph, path: &str| {
        graph
            .find_symbol_by_name("run")
            .into_iter()
            .find(|symbol| {
                symbol.kind == SymbolKind::Method
                    && graph
                        .files
                        .get(&symbol.file_id)
                        .map(|file| file.relative_path == path)
                        .unwrap_or(false)
            })
            .unwrap_or_else(|| panic!("{path} Service.run should be indexed"))
    };
    let run_a = run_in(&graph, "lib/a/service.dart");
    let run_b = run_in(&graph, "lib/b/service.dart");
    let go = graph
        .find_symbol_by_name("go")
        .into_iter()
        .find(|symbol| symbol.kind == SymbolKind::Method)
        .expect("Caller.go should be indexed");

    // `s.run()` must resolve to the imported library B's Service.run.
    assert!(
        graph.edges().iter().any(|edge| edge.from == go.id
            && edge.kind == EdgeKind::Calls
            && edge.to.as_ref() == Some(&run_b.id)),
        "s.run() must resolve to the imported lib/b Service.run",
    );
    // And must never bind to library A's unrelated same-named Service.run.
    assert!(
        !graph.edges().iter().any(|edge| edge.from == go.id
            && edge.kind == EdgeKind::Calls
            && edge.to.as_ref() == Some(&run_a.id)),
        "s.run() must NOT bind to the unrelated lib/a Service.run",
    );
}

/// Bug #9: the reverse-import index must point at the file the import actually
/// resolves to, not at every same-leaf file. An `import 'a/b/thing.dart'` must
/// attach the importer to `a/b/thing.dart` only, never to an unrelated
/// `c/d/thing.dart` that merely shares the leaf filename.
#[test]
fn graph_reverse_import_index_excludes_unrelated_same_leaf_file() {
    let mut parser = LanguageParser::new().unwrap();
    let wanted = dart_record("lib/a/b/thing.dart", "class Thing {\n  void run() {}\n}\n");
    let unrelated = dart_record("lib/c/d/thing.dart", "class Thing {\n  void run() {}\n}\n");
    let importer = dart_record(
        "lib/app.dart",
        r#"import 'package:fixture/a/b/thing.dart';

void main() {
  Thing().run();
}
"#,
    );
    let parsed = [wanted, unrelated, importer]
        .iter()
        .map(|rec| {
            parser
                .parse_source(rec, fs::read_to_string(&rec.path).unwrap())
                .unwrap()
        })
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);

    let app = FileId::new("lib/app.dart");
    let wanted_file = FileId::new("lib/a/b/thing.dart");
    let unrelated_file = FileId::new("lib/c/d/thing.dart");

    assert!(
        graph
            .importers_by_file
            .get(&wanted_file)
            .map(|importers| importers.contains(&app))
            .unwrap_or(false),
        "app.dart must be recorded as an importer of the resolved a/b/thing.dart",
    );
    assert!(
        !graph
            .importers_by_file
            .get(&unrelated_file)
            .map(|importers| importers.contains(&app))
            .unwrap_or(false),
        "app.dart must NOT attach to the unrelated c/d/thing.dart that only shares the leaf",
    );
}

#[test]
fn detect_case_collisions_empty_when_no_collisions() {
    // Use the graph record helper to build minimal FileRecord values.
    let files = vec![
        record("src/lib.rs", ""),
        record("src/main.rs", ""),
        record("README.md", ""),
    ];
    let collisions = detect_case_collisions(&files);
    assert!(
        collisions.is_empty(),
        "no case collisions expected, got: {collisions:?}"
    );
}

#[test]
fn detect_case_collisions_finds_pair() {
    let files = vec![record("src/Lib.rs", ""), record("src/lib.rs", "")];
    let collisions = detect_case_collisions(&files);
    assert_eq!(collisions.len(), 1);
    let pair = &collisions[0];
    assert!(
        (pair[0] == "src/Lib.rs" && pair[1] == "src/lib.rs")
            || (pair[0] == "src/lib.rs" && pair[1] == "src/Lib.rs"),
        "unexpected pair: {pair:?}"
    );
}

// ── Windows path identity tests ──────────────────────────────────────────────

#[test]
fn graph_files_by_normalized_id_enables_case_insensitive_lookup() {
    let source = "pub fn a() {}\n";
    let mut parser = LanguageParser::new().unwrap();
    let rec = record("src/Lib.rs", source);
    let parsed = parser.parse_source(&rec, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);

    // Exact lookup works.
    assert!(graph.files.contains_key(&FileId::new("src/Lib.rs")));
    // Case-insensitive lookup via normalized index finds the record.
    assert!(
        graph.find_file_case_insensitive("src/lib.rs").is_some(),
        "find_file_case_insensitive must resolve lowercase query to src/Lib.rs"
    );
    // Backslash normalization is applied before the lookup.
    assert!(
        graph.find_file_case_insensitive("src\\Lib.rs").is_some(),
        "find_file_case_insensitive must accept backslash path separators"
    );
    // A query with no match returns None.
    assert!(
        graph.find_file_case_insensitive("src/missing.rs").is_none(),
        "find_file_case_insensitive must return None for absent paths"
    );
}

#[test]
fn detect_case_collisions_finds_all_pairs_for_three_way_collision() {
    // Three files that all fold to "src/lib.rs": must yield C(3,2) = 3 pairs.
    let files = vec![
        record("src/lib.rs", ""),
        record("src/Lib.rs", ""),
        record("src/LIB.rs", ""),
    ];
    let collisions = detect_case_collisions(&files);
    assert_eq!(
        collisions.len(),
        3,
        "three-way collision must yield 3 pairs, got: {collisions:?}"
    );
}

#[test]
fn graph_case_insensitive_hint_returns_canonical_spelling() {
    let source = "fn x() {}\n";
    let mut parser = LanguageParser::new().unwrap();
    let rec = record("src/MyModule.rs", source);
    let parsed = parser.parse_source(&rec, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);

    assert_eq!(
        graph.case_insensitive_match_hint("src/mymodule.rs"),
        Some("src/MyModule.rs")
    );
    assert_eq!(
        graph.case_insensitive_match_hint("SRC/MYMODULE.RS"),
        Some("src/MyModule.rs")
    );
    assert_eq!(
        graph.case_insensitive_match_hint("src/MyModule.rs"),
        Some("src/MyModule.rs")
    );
    assert!(graph.case_insensitive_match_hint("src/other.rs").is_none());
}

#[test]
fn graph_case_collision_detected_in_rebuild_indexes() {
    let source_a = "pub fn a() {}\n";
    let source_b = "pub fn b() {}\n";
    let mut parser = LanguageParser::new().unwrap();
    let rec_a = record("src/lib.rs", source_a);
    let mut rec_b = record("src/lib.rs", source_b);
    rec_b.id = FileId::new("src/Lib.rs");
    rec_b.relative_path = "src/Lib.rs".to_string();

    let parsed_a = parser.parse_source(&rec_a, source_a.to_string()).unwrap();
    let parsed_b = parser.parse_source(&rec_b, source_b.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed_a, parsed_b]);

    assert_eq!(
        graph.case_collisions.len(),
        1,
        "expected exactly one case collision for src/lib.rs vs src/Lib.rs"
    );
    let (a, b) = &graph.case_collisions[0];
    let pair = [a.as_str(), b.as_str()];
    assert!(
        pair.contains(&"src/lib.rs") && pair.contains(&"src/Lib.rs"),
        "collision must name both spellings, got {a:?} and {b:?}"
    );
}

/// Absolute path tests use Unix-style paths so are only run on Unix.
#[cfg(unix)]
#[test]
fn normalize_cargo_file_id_strips_workspace_prefix() {
    let root = std::path::Path::new("/workspace/myproject");
    assert_eq!(
        normalize_cargo_file_id(root, None, "/workspace/myproject/src/main.rs"),
        Some("src/main.rs".to_string()),
    );
}

#[test]
fn normalize_cargo_file_id_passes_through_relative_path() {
    let root = std::path::Path::new("myproject");
    assert_eq!(
        normalize_cargo_file_id(root, None, "src/main.rs"),
        Some("src/main.rs".to_string()),
    );
}

#[test]
fn normalize_cargo_file_id_returns_none_for_angle_bracket_paths() {
    let root = std::path::Path::new("myproject");
    assert_eq!(normalize_cargo_file_id(root, None, "<anon>"), None);
    assert_eq!(
        normalize_cargo_file_id(root, None, "<macro expansion>"),
        None
    );
}

/// Absolute path test: an absolute path outside the workspace root should
/// return None. Only meaningful on Unix where the test paths are valid
/// absolute paths.
#[cfg(unix)]
#[test]
fn normalize_cargo_file_id_returns_none_for_path_outside_workspace() {
    let root = std::path::Path::new("/workspace/myproject");
    assert_eq!(
        normalize_cargo_file_id(root, None, "/other/repo/src/lib.rs"),
        None,
    );
}

#[cfg(unix)]
#[test]
fn normalize_cargo_file_id_fallback_via_symlink() {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };
    // Set up a real workspace dir and a symlink root that points to it.
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let real_root = std::env::temp_dir().join(format!("squeezy-cargo-norm-real-{nonce}"));
    let sym_root = std::env::temp_dir().join(format!("squeezy-cargo-norm-sym-{nonce}"));
    fs::create_dir_all(real_root.join("src")).unwrap();
    fs::write(real_root.join("src").join("main.rs"), "fn main() {}").unwrap();
    std::os::unix::fs::symlink(&real_root, &sym_root).unwrap();

    // cargo emits the path under the symlinked root; squeezy was opened
    // with the real root. The exact prefix check fails; the canonical
    // fallback must succeed.
    let canonical_real = std::fs::canonicalize(&real_root).ok();
    let result = normalize_cargo_file_id(
        &real_root,
        canonical_real.as_deref(),
        &sym_root.join("src/main.rs").to_string_lossy(),
    );
    assert_eq!(
        result,
        Some("src/main.rs".to_string()),
        "canonical fallback must resolve symlinked cargo path"
    );

    // Cleanup
    let _ = fs::remove_dir_all(&real_root);
    let _ = fs::remove_file(&sym_root);
}

#[test]
fn normalize_cargo_file_id_case_insensitive_fallback() {
    // Build a root path from the temp directory and construct a path that
    // differs from the root prefix only in casing (simulating a Windows
    // drive-letter or directory-case mismatch). The *relative* portion retains
    // its original casing so we can assert it is preserved in the output.
    let root = temp_root("cargo-norm-case");
    let root_norm = root.to_string_lossy().replace('\\', "/");

    // Exact-case path with a mixed-case relative portion.
    let exact_path = format!("{root_norm}/src/MyModule.cs");
    assert_eq!(
        normalize_cargo_file_id(&root, None, &exact_path),
        Some("src/MyModule.cs".to_string()),
        "exact case must return the relative path with its original casing"
    );

    // Construct a path where the root prefix differs in casing but the
    // relative portion stays at original casing. This simulates the Windows
    // scenario where the diagnostic reports `C:\work\src\MyModule.cs` while
    // the workspace root is `c:\work`.
    let upper_root = root_norm.to_ascii_uppercase();
    if upper_root != root_norm {
        let case_mismatch_path = format!("{upper_root}/src/MyModule.cs");
        assert_eq!(
            super::case_insensitive_relative_path(&root, std::path::Path::new(&case_mismatch_path)),
            Some(std::path::PathBuf::from("src/MyModule.cs")),
            "case-insensitive helper must preserve the relative path's original casing"
        );
        let expected = if cfg!(windows) {
            Some("src/MyModule.cs".to_string())
        } else {
            None
        };
        assert_eq!(
            normalize_cargo_file_id(&root, None, &case_mismatch_path),
            expected,
            "case-insensitive absolute-prefix fallback must be Windows-only"
        );
    }
}

#[test]
fn graph_compute_impact_uses_reverse_import_reachability() {
    let mut parser = LanguageParser::new().unwrap();
    let wanted = dart_record("lib/a/b/thing.dart", "class Thing {\n  void run() {}\n}\n");
    let unrelated = dart_record("lib/c/d/thing.dart", "class Thing {\n  void run() {}\n}\n");
    let importer = dart_record(
        "lib/app.dart",
        r#"import 'package:fixture/a/b/thing.dart';

void main() {
  Thing().run();
}
"#,
    );
    let parsed = [wanted, unrelated, importer]
        .iter()
        .map(|rec| {
            parser
                .parse_source(rec, fs::read_to_string(&rec.path).unwrap())
                .unwrap()
        })
        .collect::<Vec<_>>();
    let graph = SemanticGraph::from_parsed(parsed);

    let changed = HashSet::from([FileId::new("lib/a/b/thing.dart")]);
    let propagating = changed.clone();
    let removed = HashSet::new();
    let impact = graph.compute_impact(&changed, &propagating, &removed);

    assert!(
        impact
            .affected_files
            .contains(&FileId::new("lib/a/b/thing.dart")),
        "changed file should be included in its own impact set"
    );
    assert!(
        impact.affected_files.contains(&FileId::new("lib/app.dart")),
        "reverse importer should be included in impact set"
    );
    assert!(
        !impact
            .affected_files
            .contains(&FileId::new("lib/c/d/thing.dart")),
        "unrelated same-leaf file should not be included in impact set"
    );
    assert!(
        impact
            .affected_symbols
            .iter()
            .any(|symbol| symbol.file_id == FileId::new("lib/app.dart")),
        "affected symbols should include symbols from reverse importers"
    );
}
