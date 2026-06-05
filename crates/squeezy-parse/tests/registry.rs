use std::collections::HashSet;

use squeezy_core::{LanguageFamily, LanguageKind};
use squeezy_parse::backend;

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
