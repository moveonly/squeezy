use std::collections::HashSet;

use squeezy_core::LanguageFamily;

use super::inventory;

#[test]
fn every_language_family_has_one_oracle() {
    let mut families = HashSet::new();
    for oracle in inventory() {
        assert!(
            families.insert(oracle.family()),
            "duplicate oracle for {:?}",
            oracle.family()
        );
    }

    for family in LanguageFamily::all() {
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
