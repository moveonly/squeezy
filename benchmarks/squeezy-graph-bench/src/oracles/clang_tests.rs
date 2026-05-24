use std::fs;

use squeezy_core::LanguageKind;

use crate::util::{command_exists, temp_dir};

use super::collect_clang_ast_symbol_scan;

#[test]
fn c_family_clang_oracle_excludes_unselected_files_from_fp_accounting() {
    if !command_exists("clang") {
        return;
    }

    let root = temp_dir("c-family-clang-oracle-cap").unwrap();
    fs::write(root.join("one.c"), "int one(void) { return 1; }\n").unwrap();
    fs::write(root.join("two.c"), "int two(void) { return 2; }\n").unwrap();

    let scan = collect_clang_ast_symbol_scan(&root, LanguageKind::C, 1).unwrap();

    assert_eq!(scan.selected_files, 1);
    assert_eq!(scan.candidate_files, 2);
    assert_eq!(scan.excluded_files.len(), 1);
    assert_eq!(scan.symbols.counts.len(), 1);
}
