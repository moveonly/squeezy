use std::collections::HashSet;

use squeezy_core::{LanguageFamily, LanguageKind};
use squeezy_parse::backend;
use squeezy_workspace::classify_language;

#[test]
fn every_language_family_has_one_backend() {
    let mut families = HashSet::new();
    for backend in backend::inventory() {
        assert!(
            families.insert(backend.family()),
            "duplicate parse backend for {:?}",
            backend.family()
        );
    }

    for family in LanguageFamily::all() {
        assert!(
            families.contains(family),
            "missing parse backend for {family:?}"
        );
    }
}

#[test]
fn every_supported_language_kind_maps_to_a_backend() {
    // Iterate every family (and every kind it advertises) so a newly-added
    // family or LanguageKind is automatically covered without editing this test.
    for family in LanguageFamily::all() {
        for &kind in family.kinds() {
            let backend = backend::backend_for_kind(kind)
                .unwrap_or_else(|| panic!("missing parse backend for {kind:?}"));
            assert_eq!(
                backend.family(),
                *family,
                "kind {kind:?} routed to the wrong backend family"
            );
            assert!(
                backend.kinds().contains(&kind),
                "backend {:?} does not advertise {kind:?}",
                backend.family()
            );
            assert!(
                backend.tree_sitter_language(kind).is_some(),
                "backend {:?} does not expose a tree-sitter language for {kind:?}",
                backend.family()
            );
        }
    }

    assert!(backend::backend_for_kind(LanguageKind::Unsupported).is_none());
    assert!(backend::backend_for_kind(LanguageKind::Unknown).is_none());
}

#[test]
fn every_family_file_extension_classifies_to_that_family() {
    // Iterate `LanguageFamily::all()` so a newly-registered extension is
    // automatically exercised, guarding against extension-classification drift.
    for family in LanguageFamily::all() {
        for &extension in family.file_extensions() {
            let kind = LanguageKind::from_extension(extension);
            assert_eq!(
                kind.family(),
                Some(*family),
                "extension {extension:?} of {family:?} classified as {kind:?}"
            );
            assert!(
                backend::backend_for_kind(kind).is_some(),
                "no backend for {kind:?} (extension {extension:?})"
            );
        }
    }
}

#[test]
fn uppercase_extensions_classify_to_correct_family() {
    use std::path::PathBuf;
    // `classify_language` in squeezy-workspace lowers ASCII uppercase before
    // dispatching to `LanguageKind::from_extension`, so Windows-style
    // extensions such as `.RS`, `.CS`, `.TSX` must round-trip through the
    // same family as their lowercase counterparts.
    for family in LanguageFamily::all() {
        for &ext in family.file_extensions() {
            let uppercase_ext = ext.to_ascii_uppercase();
            let path = PathBuf::from(format!("TESTFILE.{uppercase_ext}"));
            let kind = classify_language(&path);
            assert_eq!(
                kind.family(),
                Some(*family),
                "uppercase extension .{uppercase_ext} (from .{ext}) should classify as \
                 {family:?}, got {kind:?}"
            );
        }
    }
}

#[test]
fn every_supported_kind_parses_a_minimal_fixture() {
    // Regression guard: instantiating a parser for each grammar exercises the
    // grammar crate's FFI boundary. A Windows/MSVC-only link or ABI mismatch
    // would surface here rather than being silently skipped.
    fn minimal_source(kind: LanguageKind) -> &'static str {
        match kind {
            LanguageKind::Rust => "fn main() {}\n",
            LanguageKind::Python => "x = 1\n",
            LanguageKind::Java => "class Main {}\n",
            LanguageKind::CSharp => "class Main {}\n",
            LanguageKind::Go => "package main\n",
            LanguageKind::C => "int main(void) { return 0; }\n",
            LanguageKind::Cpp => "int main() { return 0; }\n",
            LanguageKind::JavaScript | LanguageKind::Jsx => "const x = 1;\n",
            LanguageKind::TypeScript | LanguageKind::Tsx => "const x = 1;\n",
            LanguageKind::Ruby => "puts 'hello'\n",
            LanguageKind::Php => "<?php\n$x = 1;\n",
            LanguageKind::Kotlin => "fun main() {}\n",
            LanguageKind::Swift => "let x = 1\n",
            LanguageKind::Scala => "object Main {}\n",
            LanguageKind::Dart => "void main() {}\n",
            _ => panic!("no minimal fixture defined for {kind:?}"),
        }
    }

    for family in LanguageFamily::all() {
        for &kind in family.kinds() {
            let backend = backend::backend_for_kind(kind)
                .unwrap_or_else(|| panic!("missing backend for {kind:?}"));
            let mut parser = backend
                .parser(kind)
                .unwrap_or_else(|err| panic!("parser creation failed for {kind:?}: {err}"));
            let source = minimal_source(kind);
            let tree = parser
                .parse(source, None)
                .unwrap_or_else(|| panic!("parser returned None tree for {kind:?}"));
            // The root should not be a pure error node — if the grammar
            // rejected the entire fixture the linking succeeded but the
            // fixture is wrong and should be fixed.
            assert!(
                !tree.root_node().is_error(),
                "root node is ERROR for {kind:?}; fix the minimal fixture for that language"
            );
        }
    }
}
