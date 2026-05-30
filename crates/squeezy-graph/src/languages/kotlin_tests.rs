use super::*;

#[test]
fn kotlin_generated_source_root_recognises_kotlin_paths() {
    assert_eq!(
        kotlin_generated_source_root("src/generated/kotlin/com/example/Foo.kt").as_deref(),
        Some("src/generated/kotlin"),
    );
    assert_eq!(
        kotlin_generated_source_root("build/generated/source/kapt/main/com/example/Foo.kt")
            .as_deref(),
        Some("build/generated/source"),
    );
    assert!(kotlin_generated_source_root("src/main/kotlin/com/example/Foo.kt").is_none());
}

#[test]
fn kotlin_source_root_facts_picks_main_and_test() {
    let facts = kotlin_source_root_facts(
        "gradle",
        &[
            "src/main/kotlin/com/example/Foo.kt",
            "src/test/kotlin/com/example/FooTest.kt",
        ],
    );
    let kinds = facts
        .iter()
        .map(|(k, v, _)| (*k, v.clone()))
        .collect::<Vec<_>>();
    assert!(
        kinds
            .iter()
            .any(|(k, v)| *k == "source_root" && v == "main:src/main/kotlin"),
    );
    assert!(
        kinds
            .iter()
            .any(|(k, v)| *k == "test_root" && v == "test:src/test/kotlin"),
    );
}
