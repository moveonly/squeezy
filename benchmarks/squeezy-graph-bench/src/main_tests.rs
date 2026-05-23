use std::{fs, path::Path};

use serde_json::json;

use super::*;

#[test]
fn select_scenarios_spreads_capped_samples() {
    let sample = select_scenarios(100, 10);

    assert_eq!(sample.len(), 10);
    assert!(sample.windows(2).all(|pair| pair[0] < pair[1]));
    assert_ne!(sample, (0..10).collect::<Vec<_>>());
}

#[test]
fn select_scenarios_zero_means_exhaustive() {
    assert_eq!(select_scenarios(5, 0), vec![0, 1, 2, 3, 4]);
}

#[test]
fn byte_to_lsp_position_counts_utf16_characters() {
    let source = "a\néx\n";

    assert_eq!(
        byte_to_lsp_position(source, 4),
        LspPosition {
            line: 1,
            character: 1
        }
    );
}

#[test]
fn parse_lsp_locations_relativizes_locations_and_location_links() {
    let value = json!([
        {
            "uri": "file:///repo/src/lib.rs",
            "range": {"start": {"line": 2, "character": 4}}
        },
        {
            "targetUri": "file:///repo/src/main.rs",
            "targetSelectionRange": {"start": {"line": 5, "character": 8}}
        }
    ]);

    let locations = parse_lsp_locations(&value, Path::new("/repo")).unwrap();

    assert_eq!(
        locations,
        vec![
            LocationKey {
                file: "src/lib.rs".to_string(),
                line: 2,
                character: 4
            },
            LocationKey {
                file: "src/main.rs".to_string(),
                line: 5,
                character: 8
            }
        ]
    );
}

#[test]
fn python_ast_oracle_reports_unparseable_files_separately() {
    let root = temp_dir("python-ast-oracle-unparseable").unwrap();
    fs::write(root.join("valid.py"), "def ok():\n    pass\n").unwrap();
    fs::write(root.join("invalid.py"), "def broken(:\n    pass\n").unwrap();

    let scan = collect_python_ast_symbol_scan(&root).unwrap();

    assert_eq!(scan.unparseable_files, vec!["invalid.py".to_string()]);
    assert_eq!(scan.symbols.raw_total, 1);
    assert!(
        scan.symbols.counts.contains_key(&SymbolKey {
            file: "valid.py".to_string(),
            kind: "Function".to_string(),
            name: "ok".to_string()
        })
    );
}
