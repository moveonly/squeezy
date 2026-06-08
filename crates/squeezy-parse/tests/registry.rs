use std::collections::HashSet;
use std::path::Path;

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

/// Pin uppercase-extension classification through the workspace layer so a
/// regression in `classify_language`'s normalization path is caught before it
/// silently excludes Linux repos with mixed-case filenames.
#[test]
fn classify_language_handles_uppercase_extensions_for_all_families() {
    let cases: &[(&str, LanguageFamily)] = &[
        ("MAIN.RS", LanguageFamily::Rust),
        ("lib.PY", LanguageFamily::Python),
        ("App.JAVA", LanguageFamily::Java),
        ("Program.CS", LanguageFamily::CSharp),
        ("main.GO", LanguageFamily::Go),
        ("runner.C", LanguageFamily::CFamily),
        ("widget.CPP", LanguageFamily::CFamily),
        ("index.JS", LanguageFamily::JsTs),
        ("view.TS", LanguageFamily::JsTs),
        ("component.TSX", LanguageFamily::JsTs),
        ("item.JSX", LanguageFamily::JsTs),
        ("helper.RB", LanguageFamily::Ruby),
        ("page.PHP", LanguageFamily::Php),
        ("Greeter.KT", LanguageFamily::Kotlin),
        ("Model.SWIFT", LanguageFamily::Swift),
        ("Main.SCALA", LanguageFamily::Scala),
        ("widget.DART", LanguageFamily::Dart),
    ];
    for (filename, expected_family) in cases {
        let kind = classify_language(Path::new(filename));
        assert_eq!(
            kind.family(),
            Some(*expected_family),
            "uppercase filename {filename:?} classified as {kind:?}, expected {expected_family:?}"
        );
        assert!(
            backend::backend_for_kind(kind).is_some(),
            "no backend for {kind:?} (file {filename:?})"
        );
    }
}
