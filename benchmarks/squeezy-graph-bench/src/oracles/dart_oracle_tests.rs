use std::fs;

use crate::util::temp_dir;

use super::{DartOracleMode, normalize_oracle_kind, parse_oracle_stdout};

#[test]
fn normalize_oracle_kind_collapses_dart_aliases() {
    // Both halves of the comparison have to project Dart-specific kinds
    // (`Mixin`, `Extension`, `ExtensionType`) into the squeezy vocabulary
    // (`Trait`, `Class`, `Class`) or the SymbolKey hash will never match.
    assert_eq!(normalize_oracle_kind("Mixin"), Some("Trait"));
    assert_eq!(normalize_oracle_kind("Extension"), Some("Class"));
    assert_eq!(normalize_oracle_kind("ExtensionType"), Some("Class"));
    assert_eq!(normalize_oracle_kind("Enum"), Some("Enum"));
    assert_eq!(normalize_oracle_kind("Field"), Some("Field"));
    // Library rows describe a directive, not a navigable symbol: excluded.
    assert_eq!(normalize_oracle_kind("Library"), None);
    assert_eq!(normalize_oracle_kind("garbage"), None);
}

#[test]
fn parse_oracle_stdout_extracts_symbols_and_unparseable_files() {
    let dir = temp_dir("dart-oracle-parse").unwrap();
    fs::create_dir_all(&dir).unwrap();
    let payload = concat!(
        "{\"file\":\"a.dart\",\"symbols\":[",
        "{\"file\":\"a.dart\",\"kind\":\"Class\",\"name\":\"Foo\"},",
        "{\"file\":\"a.dart\",\"kind\":\"Mixin\",\"name\":\"Bar\"},",
        "{\"file\":\"a.dart\",\"kind\":\"Library\",\"name\":\"a\"}",
        "],\"imports\":[],\"exports\":[],\"parts\":[]}\n",
        "{\"file\":\"b.dart\",\"unparseable\":true}\n",
        "{\"summary\":{\"resolved_libraries\":1,\"unparseable_files\":1}}\n",
    );
    let scan = parse_oracle_stdout(payload, &dir, 11).unwrap();
    assert_eq!(scan.ms, 11);
    assert_eq!(scan.mode, DartOracleMode::Analyzer);
    assert_eq!(scan.unparseable_files, vec!["b.dart".to_string()]);
    // 3 rows -> 1 Class + 1 Trait (Mixin) accepted, 1 Library excluded.
    assert_eq!(scan.symbols.raw_total, 3);
    assert_eq!(scan.symbols.counts.len(), 2);
}

#[test]
fn parse_oracle_stdout_handles_missing_summary() {
    let dir = temp_dir("dart-oracle-no-summary").unwrap();
    fs::create_dir_all(&dir).unwrap();
    let payload = "{\"file\":\"x.dart\",\"symbols\":[],\"imports\":[],\"exports\":[],\"parts\":[]}\n";
    let scan = parse_oracle_stdout(payload, &dir, 0).unwrap();
    assert!(scan.status.contains("no summary"));
}
