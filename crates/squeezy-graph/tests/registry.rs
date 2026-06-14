use std::collections::HashSet;
use std::path::PathBuf;

use squeezy_core::LanguageFamily;
use squeezy_graph::backend as graph_backend;
use squeezy_parse::backend as parse_backend;
use squeezy_workspace::classify_language;

#[test]
fn every_language_family_has_graph_parse_and_workspace_registration() {
    let mut graph_families = HashSet::new();
    for extension in graph_backend::inventory() {
        assert!(
            graph_families.insert(extension.family()),
            "duplicate graph extension for {:?}",
            extension.family()
        );
    }

    let mut parse_families = HashSet::new();
    for backend in parse_backend::inventory() {
        assert!(
            parse_families.insert(backend.family()),
            "duplicate parse backend for {:?}",
            backend.family()
        );
    }

    for family in LanguageFamily::all() {
        assert!(
            graph_families.contains(family),
            "missing graph extension for {family:?}"
        );
        assert!(
            parse_families.contains(family),
            "missing parse backend for {family:?}"
        );
        assert!(
            !family.file_extensions().is_empty(),
            "missing workspace extensions for {family:?}"
        );

        for &extension in family.file_extensions() {
            let path = PathBuf::from(format!("fixture.{extension}"));
            let kind = classify_language(&path);
            assert_eq!(
                kind.family(),
                Some(*family),
                "workspace selector for .{extension} classified as {kind:?}, expected {family:?}"
            );
            assert!(
                parse_backend::backend_for_kind(kind).is_some(),
                "workspace selector for .{extension} has no parse backend for {kind:?}"
            );
        }
    }
}

#[test]
fn java_extension_declares_project_facts() {
    for extension in graph_backend::inventory() {
        let expected = extension.family() == LanguageFamily::Java;
        assert_eq!(
            extension.supports_project_facts(),
            expected,
            "project fact support mismatch for {:?}",
            extension.family()
        );
    }
}
