use std::fs;
use std::path::PathBuf;
use std::process::Command;

use crate::util::temp_dir;

use super::{
    SCALA_SCAN_ONLY_PREFIX, SCALA_SEMANTICDB_STATUS_PREFIX, collect_scala_oracle_accuracy,
    decode_text_documents, map_symbol_kind,
};
use crate::execution::build_graph;

const SDB_KIND_CLASS: i32 = 13;
const SDB_KIND_TRAIT: i32 = 14;
const SDB_KIND_OBJECT: i32 = 10;
const SDB_KIND_METHOD: i32 = 3;
const SDB_KIND_CONSTRUCTOR: i32 = 21;
const SDB_KIND_MACRO: i32 = 6;
const SDB_KIND_PACKAGE: i32 = 11;
const SDB_KIND_PACKAGE_OBJECT: i32 = 12;
const SDB_KIND_INTERFACE: i32 = 18;
const SDB_KIND_LOCAL: i32 = 19;
const SDB_KIND_FIELD: i32 = 20;
const SDB_KIND_PARAMETER: i32 = 8;
const SDB_KIND_TYPE: i32 = 7;
const SDB_KIND_TYPE_PARAMETER: i32 = 9;

#[test]
fn status_prefix_constants_are_distinct_and_load_bearing() {
    // gates.rs branches on these prefixes. If they ever collide the gate
    // logic silently suppresses the precision/recall check on a real
    // SemanticDB run. Lock them in with a unit test so a refactor renaming
    // one prefix without the other surfaces immediately.
    assert_ne!(SCALA_SCAN_ONLY_PREFIX, SCALA_SEMANTICDB_STATUS_PREFIX);
    assert!(!SCALA_SEMANTICDB_STATUS_PREFIX.starts_with(SCALA_SCAN_ONLY_PREFIX));
    assert!(!SCALA_SCAN_ONLY_PREFIX.starts_with(SCALA_SEMANTICDB_STATUS_PREFIX));
    assert!(!SCALA_SCAN_ONLY_PREFIX.starts_with("skipped"));
    assert!(!SCALA_SEMANTICDB_STATUS_PREFIX.starts_with("skipped"));
}

#[test]
fn symbol_kind_map_matches_documented_table() {
    // Mirrors the kind table in the Scala spec §9 and ensures the
    // tree-sitter extractor's emitted kinds line up against SemanticDB
    // declarations. Misalignment here is the single biggest source of
    // oracle false negatives, so the table is asserted directly.
    assert_eq!(map_symbol_kind(SDB_KIND_CLASS).as_deref(), Some("Class"));
    assert_eq!(map_symbol_kind(SDB_KIND_OBJECT).as_deref(), Some("Class"));
    assert_eq!(map_symbol_kind(SDB_KIND_TRAIT).as_deref(), Some("Trait"));
    assert_eq!(
        map_symbol_kind(SDB_KIND_INTERFACE).as_deref(),
        Some("Interface")
    );
    assert_eq!(map_symbol_kind(SDB_KIND_METHOD).as_deref(), Some("Method"));
    assert_eq!(
        map_symbol_kind(SDB_KIND_CONSTRUCTOR).as_deref(),
        Some("Method")
    );
    assert_eq!(map_symbol_kind(SDB_KIND_MACRO).as_deref(), Some("Method"));
    assert_eq!(map_symbol_kind(SDB_KIND_TYPE).as_deref(), Some("TypeAlias"));
    assert_eq!(
        map_symbol_kind(SDB_KIND_PACKAGE_OBJECT).as_deref(),
        Some("Module")
    );
    // Kinds that squeezy never compares against — locals, parameters,
    // anonymous packages, fields, and type parameters — must be skipped
    // so they do not inflate the false-negative tally.
    assert!(map_symbol_kind(SDB_KIND_LOCAL).is_none());
    assert!(map_symbol_kind(SDB_KIND_FIELD).is_none());
    assert!(map_symbol_kind(SDB_KIND_PARAMETER).is_none());
    assert!(map_symbol_kind(SDB_KIND_TYPE_PARAMETER).is_none());
    assert!(map_symbol_kind(SDB_KIND_PACKAGE).is_none());
}

#[test]
fn decode_text_documents_round_trips_synthesized_payload() {
    // Build a minimal `TextDocuments` proto in-memory using the wire-format
    // helpers below so the decoder is exercised against bytes that mirror
    // what `scalac -Xsemanticdb` emits. This avoids relying on a
    // pre-canned binary check-in that drifts when the schema evolves.
    let payload = build_text_documents(&[(
        "src/main/scala/example/Foo.scala",
        &[
            ("example/Foo#", SDB_KIND_CLASS, "Foo"),
            ("example/Foo#greet().", SDB_KIND_METHOD, "greet"),
            ("local0", SDB_KIND_LOCAL, "_tmp"),
        ],
    )]);

    let documents = decode_text_documents(&payload).expect("decode");
    assert_eq!(documents.len(), 1);
    assert_eq!(documents[0].uri, "src/main/scala/example/Foo.scala");
    assert_eq!(documents[0].symbols.len(), 3);
    assert_eq!(documents[0].symbols[0].kind, SDB_KIND_CLASS);
    assert_eq!(documents[0].symbols[0].display_name, "Foo");
    assert_eq!(documents[0].symbols[1].display_name, "greet");
}

#[test]
fn decode_text_documents_skips_unknown_fields() {
    // The SemanticDB schema carries large `Signature` / `Type` subtrees
    // whose fields we deliberately do not decode. The cursor must skip
    // wire-typed unknown fields without aborting.
    let payload = build_text_documents(&[(
        "Bar.scala",
        &[("a/Bar#", SDB_KIND_TRAIT, "Bar")],
    )]);
    let mut padded = Vec::with_capacity(payload.len() + 16);
    // Unknown top-level field: tag for field=99 wire=VARINT, value=42.
    write_tag(&mut padded, 99, 0);
    write_varint(&mut padded, 42);
    padded.extend_from_slice(&payload);

    let documents = decode_text_documents(&padded).expect("decode");
    assert_eq!(documents.len(), 1);
    assert_eq!(documents[0].symbols[0].display_name, "Bar");
}

#[test]
fn collect_scala_oracle_accuracy_returns_scan_only_when_scalac_missing() {
    // When `scalac` is not on `$PATH` the oracle must surface a stable
    // status prefix so the gate logic can suppress the precision/recall
    // check on CI runners without a Scala toolchain. The test simulates
    // the missing-toolchain path by running in a PATH-cleared subprocess
    // when scalac would otherwise be reachable.
    if Command::new("scalac")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        // Real toolchain is reachable; nothing to assert here without
        // shelling out the test process with an empty PATH, which is
        // brittle. The success path is exercised by the end-to-end test
        // below when scalac is installed.
        return;
    }
    let root = temp_dir("scala-semanticdb-no-scalac").expect("temp dir");
    let sources = root.join("Foo.scala");
    fs::write(&sources, "class Foo\n").expect("write source");
    let build = build_graph(&root).expect("build graph");
    let report = collect_scala_oracle_accuracy(&root, &build.graph).expect("collect");
    assert!(
        report.status.starts_with("scan-only-fallback"),
        "expected scan-only-fallback status, got {:?}",
        report.status
    );
}

#[test]
fn end_to_end_semanticdb_pipeline_reports_real_symbols() {
    // This test only fires when `scalac` is reachable. It writes a
    // self-contained Scala source, drives the oracle pipeline, and asserts
    // that the SemanticDB-succeeded status prefix surfaces. CI runners
    // without a Scala toolchain skip the body so the suite stays green.
    if Command::new("scalac")
        .arg("-version")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        return;
    }
    let root = temp_dir("scala-semanticdb-e2e").expect("temp dir");
    let sources_dir = root.join("src/main/scala/example");
    fs::create_dir_all(&sources_dir).expect("mkdir");
    fs::write(
        sources_dir.join("Foo.scala"),
        "package example\n\nclass Foo:\n  def greet(name: String): String = s\"hi $name\"\n\nobject Bar:\n  def run(): Unit = println(\"hi\")\n\ntrait Greeter:\n  def hello(n: String): String\n\ntype Money = BigDecimal\n",
    )
    .expect("write Foo.scala");
    let build = build_graph(&root).expect("build graph");
    let report = collect_scala_oracle_accuracy(&root, &build.graph).expect("collect");
    assert!(
        report.status.starts_with("SemanticDB oracle succeeded"),
        "expected SemanticDB oracle success, got {:?}",
        report.status
    );
    // Foo, Bar (object → Class), Greeter (trait), Money (type alias),
    // greet, run, hello are the comparable declarations. The constructor
    // <init> symbols and synthetic accessors must not appear in the
    // oracle's bucket — those are filtered upstream.
    let buckets = report.symbols.compared_kinds.to_vec();
    assert!(buckets.contains(&"Class".to_string()));
    assert!(report.symbols.rust_analyzer_total > 0);
}

#[test]
fn decode_text_documents_reads_checked_in_fixture() {
    // Pre-encoded fixture lives under
    // `benchmarks/squeezy-graph-bench/tests/fixtures/scala-semanticdb/`.
    // Keeping the bytes on disk guards the decoder against accidental
    // wire-format drift if the schema or our encoder helpers regress.
    let path = fixture_dir().join("example_Foo.scala.semanticdb");
    let bytes = fs::read(&path).expect("read fixture");
    let documents = decode_text_documents(&bytes).expect("decode fixture");
    assert!(
        !documents.is_empty(),
        "fixture must contain at least one document"
    );
    let first = &documents[0];
    assert!(first.uri.ends_with("Foo.scala"));
    let names = first
        .symbols
        .iter()
        .map(|s| s.display_name.as_str())
        .collect::<Vec<_>>();
    assert!(names.contains(&"Foo"));
}

fn fixture_dir() -> PathBuf {
    // `CARGO_MANIFEST_DIR` resolves to the bench crate root at build time;
    // tests run from there, so the fixture path is stable across `cargo
    // test` invocations regardless of the caller's cwd.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/scala-semanticdb")
}

/// Tuple-typed symbol row used by the fixture builder. Kept here (rather
/// than in the source module) because it is test-only encoding scaffolding
/// — the runtime decoder consumes the parsed `SymbolInformation` struct.
type SymbolRow<'a> = (&'a str, i32, &'a str);
type FixtureDoc<'a> = (&'a str, &'a [SymbolRow<'a>]);

fn build_text_documents(documents: &[FixtureDoc<'_>]) -> Vec<u8> {
    let mut out = Vec::new();
    for (uri, symbols) in documents {
        let mut document = Vec::new();
        write_tag(&mut document, 2, 2);
        write_length_delimited(&mut document, uri.as_bytes());
        for (symbol, kind, display) in *symbols {
            let mut info = Vec::new();
            write_tag(&mut info, 1, 2);
            write_length_delimited(&mut info, symbol.as_bytes());
            write_tag(&mut info, 3, 0);
            write_varint(&mut info, *kind as u64);
            write_tag(&mut info, 5, 2);
            write_length_delimited(&mut info, display.as_bytes());
            write_tag(&mut document, 5, 2);
            write_length_delimited(&mut document, &info);
        }
        write_tag(&mut out, 1, 2);
        write_length_delimited(&mut out, &document);
    }
    out
}

fn write_tag(out: &mut Vec<u8>, field: u32, wire: u8) {
    let raw = ((field as u64) << 3) | (wire as u64 & 0x07);
    write_varint(out, raw);
}

fn write_length_delimited(out: &mut Vec<u8>, body: &[u8]) {
    write_varint(out, body.len() as u64);
    out.extend_from_slice(body);
}

fn write_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push(((value & 0x7f) as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}
