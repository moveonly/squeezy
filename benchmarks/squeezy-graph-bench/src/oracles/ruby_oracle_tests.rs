use std::{collections::BTreeSet, fs, path::PathBuf};

use squeezy_core::{ContentHash, FileId, Freshness, LanguageKind};
use squeezy_graph::SemanticGraph;
use squeezy_parse::LanguageParser;
use squeezy_workspace::{FileRecord, stable_content_hash};

use super::*;

fn temp_root(name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
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

fn write_file(root: &std::path::Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

fn build_graph(root: &std::path::Path, files: &[(&str, &str)]) -> SemanticGraph {
    for (rel, contents) in files {
        write_file(root, rel, contents);
    }
    let mut parser = LanguageParser::new().unwrap();
    let parsed = files
        .iter()
        .map(|(rel, contents)| {
            let record = FileRecord {
                id: FileId::new(*rel),
                path: root.join(rel),
                relative_path: (*rel).to_string(),
                hash: ContentHash::new(stable_content_hash(contents.as_bytes())),
                size_bytes: contents.len() as u64,
                modified_unix_millis: 0,
                language: LanguageKind::Ruby,
                freshness: Freshness::Fresh,
            };
            parser
                .parse_source(&record, (*contents).to_string())
                .unwrap()
        })
        .collect::<Vec<_>>();
    SemanticGraph::from_parsed(parsed)
}

#[test]
fn ruby_oracle_falls_back_to_scan_only_when_ruby_missing() {
    // If `ruby` is on PATH we still want to assert that the report's mode is
    // either "prism" or "scan-only". On most dev/CI machines without Prism
    // installed this exercises the fallback path; on machines with Ruby +
    // Prism it exercises the happy path.
    let root = temp_root("ruby-oracle-test");
    let graph = build_graph(
        &root,
        &[(
            "app/models/user.rb",
            "class User\n  def full_name; \"x\"; end\nend\n",
        )],
    );
    let report = collect_ruby_oracle_accuracy(&root, &graph).expect("report");
    assert!(
        report.mode == "prism" || report.mode == "scan-only",
        "unexpected mode {}",
        report.mode
    );
    // In either mode the report must at least have status text.
    assert!(!report.status.is_empty());
    // The scan-only mode is a self-compare: precision and recall should be
    // 1.0 because we compared the scan against itself.
    if report.mode == "scan-only" {
        assert!(
            (report.symbols.precision - 1.0).abs() < 1e-6,
            "scan-only precision should be 1.0 (got {})",
            report.symbols.precision
        );
        assert!(
            (report.symbols.recall - 1.0).abs() < 1e-6,
            "scan-only recall should be 1.0 (got {})",
            report.symbols.recall
        );
    }
}

#[test]
fn ruby_symbol_scan_filters_synthesized_attr_methods() {
    // The bench-side `collect_squeezy_ruby_symbol_scan_excluding_files`
    // must exclude `ruby:synthesized` attr_* methods so they don't count as
    // false positives against the Prism oracle (which doesn't emit them).
    let root = temp_root("ruby-attr-filter");
    let graph = build_graph(
        &root,
        &[(
            "app/models/user.rb",
            "class User\n  attr_accessor :name\nend\n",
        )],
    );
    let scan = collect_squeezy_ruby_symbol_scan_excluding_files(&graph, &BTreeSet::new());
    // The `attr_accessor :name` synthesis yields `name` and `name=` Method
    // symbols; both have `ruby:synthesized` and must be filtered out.
    for key in scan.counts.keys() {
        assert!(
            key.name != "name" && key.name != "name=",
            "synthesized attr method leaked into scan: {key:?}"
        );
    }
}
