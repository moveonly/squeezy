use super::*;
use crate::report::{SymbolKey, SymbolScan};
use serde_json::json;

#[test]
fn swift_oracle_excludes_generated_and_vendor_files() {
    assert!(is_swift_oracle_excluded_file("generated/R.generated.swift"));
    assert!(is_swift_oracle_excluded_file("vendor/SwiftBundled/Bundled.swift"));
    assert!(is_swift_oracle_excluded_file("Sources/X/R.generated.swift"));
    assert!(!is_swift_oracle_excluded_file(
        "Sources/Networking/Endpoint.swift"
    ));
}

#[test]
fn swift_oracle_normalizes_kinds() {
    use squeezy_core::SymbolKind;
    assert_eq!(
        normalize_swift_squeezy_kind(SymbolKind::Class).as_deref(),
        Some("Class")
    );
    assert_eq!(
        normalize_swift_squeezy_kind(SymbolKind::Trait).as_deref(),
        Some("Trait")
    );
    assert_eq!(
        normalize_swift_squeezy_kind(SymbolKind::Variant).as_deref(),
        Some("Variant")
    );
    assert_eq!(
        normalize_swift_squeezy_kind(SymbolKind::Field).as_deref(),
        Some("Field")
    );
    assert_eq!(
        normalize_swift_squeezy_kind(SymbolKind::Method).as_deref(),
        Some("Method")
    );
    // Unknown kinds are dropped from the scan (not part of LSP shape).
    assert_eq!(normalize_swift_squeezy_kind(SymbolKind::Impl), None);
    assert_eq!(normalize_swift_squeezy_kind(SymbolKind::Macro), None);
}

#[test]
fn sourcekit_lsp_kind_maps_swift_constructs_to_canonical_kinds() {
    // LSP SymbolKind::Class = 5 covers `class`, `actor`, and reference-type
    // extensions. SymbolKind::Struct = 23 maps to Swift structs.
    assert_eq!(
        normalize_sourcekit_lsp_kind(5, None).as_deref(),
        Some("Class")
    );
    assert_eq!(
        normalize_sourcekit_lsp_kind(23, None).as_deref(),
        Some("Struct")
    );
    assert_eq!(
        normalize_sourcekit_lsp_kind(10, None).as_deref(),
        Some("Enum")
    );
    // Swift protocols arrive as LSP Interface (= 11); Squeezy labels them
    // Trait so the kind names align with rust-analyzer.
    assert_eq!(
        normalize_sourcekit_lsp_kind(11, None).as_deref(),
        Some("Trait")
    );
    assert_eq!(
        normalize_sourcekit_lsp_kind(12, None).as_deref(),
        Some("Function")
    );
    assert_eq!(
        normalize_sourcekit_lsp_kind(6, None).as_deref(),
        Some("Method")
    );
    assert_eq!(
        normalize_sourcekit_lsp_kind(22, None).as_deref(),
        Some("Variant")
    );
    // Property / Field (7 / 8) collapse onto Field uniformly.
    assert_eq!(
        normalize_sourcekit_lsp_kind(7, None).as_deref(),
        Some("Field")
    );
    // Variable (13) is only counted as a Field when nested inside a type
    // declaration — module-scope `let`/`var` is not a Squeezy symbol.
    assert_eq!(
        normalize_sourcekit_lsp_kind(13, Some("Class")).as_deref(),
        Some("Field")
    );
    assert_eq!(normalize_sourcekit_lsp_kind(13, None), None);
    // TypeParameter (26) has no Squeezy counterpart.
    assert_eq!(normalize_sourcekit_lsp_kind(26, None), None);
}

#[test]
fn collect_document_symbols_aggregates_kinds_and_drops_property_wrappers() {
    let symbols = vec![json!({
        "name": "UserRepository",
        "kind": 5,  // Class
        "children": [
            { "name": "refresh", "kind": 6 },             // Method
            { "name": "users", "kind": 7 },               // Property -> Field
            { "name": "$users", "kind": 7 },              // Skipped: $-prefixed
            { "name": "_users", "kind": 7 },              // Skipped: _-prefixed
        ]
    })];
    let mut scan = SymbolScan::default();
    collect_document_symbols(&symbols, "Repository.swift", &mut scan, None);
    assert_eq!(
        scan.counts
            .get(&SymbolKey {
                file: "Repository.swift".to_string(),
                kind: "Class".to_string(),
                name: "UserRepository".to_string(),
            })
            .copied(),
        Some(1)
    );
    assert_eq!(
        scan.counts
            .get(&SymbolKey {
                file: "Repository.swift".to_string(),
                kind: "Method".to_string(),
                name: "refresh".to_string(),
            })
            .copied(),
        Some(1)
    );
    assert_eq!(
        scan.counts
            .get(&SymbolKey {
                file: "Repository.swift".to_string(),
                kind: "Field".to_string(),
                name: "users".to_string(),
            })
            .copied(),
        Some(1)
    );
    // Property-wrapper synthesized accessors are not emitted to avoid
    // inflating FP vs SourceKit-LSP.
    assert!(
        !scan.counts.keys().any(|key| key.name.starts_with('$')),
        "property-wrapper $-prefixed names should be filtered"
    );
    assert!(
        !scan.counts.keys().any(|key| key.name.starts_with('_')),
        "property-wrapper _-prefixed names should be filtered"
    );
}

#[test]
fn count_scan_disagreement_counts_per_key_deltas_symmetrically() {
    let mut left = SymbolScan::default();
    left.counts.insert(
        SymbolKey {
            file: "a.swift".to_string(),
            kind: "Class".to_string(),
            name: "A".to_string(),
        },
        2,
    );
    left.counts.insert(
        SymbolKey {
            file: "a.swift".to_string(),
            kind: "Method".to_string(),
            name: "shared".to_string(),
        },
        1,
    );
    let mut right = SymbolScan::default();
    right.counts.insert(
        SymbolKey {
            file: "a.swift".to_string(),
            kind: "Class".to_string(),
            name: "A".to_string(),
        },
        3,
    );
    right.counts.insert(
        SymbolKey {
            file: "b.swift".to_string(),
            kind: "Struct".to_string(),
            name: "B".to_string(),
        },
        1,
    );
    // shared key: |2 - 3| = 1; right-only Struct B: 1; left-only Method
    // shared: 1. Total: 3.
    assert_eq!(count_scan_disagreement(&left, &right), 3);
}
