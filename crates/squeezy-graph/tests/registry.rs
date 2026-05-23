use std::collections::HashSet;

use squeezy_core::LanguageFamily;
use squeezy_graph::backend;

#[test]
fn every_language_family_has_one_graph_extension() {
    let mut families = HashSet::new();
    for extension in backend::inventory() {
        assert!(
            families.insert(extension.family()),
            "duplicate graph extension for {:?}",
            extension.family()
        );
    }

    for family in LanguageFamily::all() {
        assert!(
            families.contains(family),
            "missing graph extension for {family:?}"
        );
    }
}

#[test]
fn java_extension_declares_project_facts() {
    for extension in backend::inventory() {
        let expected = extension.family() == LanguageFamily::Java;
        assert_eq!(
            extension.supports_project_facts(),
            expected,
            "project fact support mismatch for {:?}",
            extension.family()
        );
    }
}
