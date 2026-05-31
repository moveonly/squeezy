use std::collections::HashSet;

use squeezy_core::LanguageFamily;

use super::inventory;

#[test]
fn every_oracle_targets_a_distinct_family() {
    // Each registered oracle must own a distinct LanguageFamily. We do NOT
    // assert that every `LanguageFamily` has an oracle: scaffold families
    // intentionally land before their oracles do. Each oracle PR lifts its
    // language out of the placeholder set, the same pattern Ruby followed
    // before its Prism oracle landed.
    let mut families = HashSet::new();
    for oracle in inventory() {
        assert!(
            families.insert(oracle.family()),
            "duplicate oracle for {:?}",
            oracle.family()
        );
    }

    let required: &[LanguageFamily] = &[
        LanguageFamily::Rust,
        LanguageFamily::Python,
        LanguageFamily::Java,
        LanguageFamily::CSharp,
        LanguageFamily::Go,
        LanguageFamily::CFamily,
        LanguageFamily::JsTs,
        LanguageFamily::Ruby,
        LanguageFamily::Dart,
    ];
    for family in required {
        assert!(families.contains(family), "missing oracle for {family:?}");
    }
}

#[test]
fn oracle_mixed_workload_flags_match_benchmark_language() {
    for oracle in inventory() {
        assert_eq!(
            oracle.supports_mixed_workload(),
            oracle.benchmark_language().supports_mixed_workload(),
            "mixed workload mismatch for {}",
            oracle.id()
        );
    }
}
