use std::fs;

use crate::{report::SymbolKey, util::temp_dir};

use super::collect_python_ast_symbol_scan;

#[test]
fn python_ast_oracle_reports_unparseable_files_separately() {
    let root = temp_dir("python-ast-oracle-unparseable").unwrap();
    fs::write(root.join("valid.py"), "def ok():\n    pass\n").unwrap();
    fs::write(root.join("invalid.py"), "def broken(:\n    pass\n").unwrap();

    let scan = collect_python_ast_symbol_scan(&root).unwrap();

    assert_eq!(scan.unparseable_files, vec!["invalid.py".to_string()]);
    assert_eq!(scan.symbols.raw_total, 1);
    assert!(scan.symbols.counts.contains_key(&SymbolKey {
        file: "valid.py".to_string(),
        kind: "Function".to_string(),
        name: "ok".to_string()
    }));
}
