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
    let supported = [
        LanguageKind::Rust,
        LanguageKind::Python,
        LanguageKind::Java,
        LanguageKind::CSharp,
        LanguageKind::Go,
        LanguageKind::C,
        LanguageKind::Cpp,
        LanguageKind::JavaScript,
        LanguageKind::Jsx,
        LanguageKind::TypeScript,
        LanguageKind::Tsx,
    ];

    for kind in supported {
        let backend = backend::backend_for_kind(kind)
            .unwrap_or_else(|| panic!("missing parse backend for {kind:?}"));
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

    assert!(backend::backend_for_kind(LanguageKind::Unsupported).is_none());
    assert!(backend::backend_for_kind(LanguageKind::Unknown).is_none());
}
