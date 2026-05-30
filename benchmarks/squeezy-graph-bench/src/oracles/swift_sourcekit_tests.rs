use super::*;

#[test]
fn swift_oracle_excludes_generated_and_vendor_files() {
    assert!(is_swift_oracle_excluded_file("generated/R.generated.swift"));
    assert!(is_swift_oracle_excluded_file("vendor/SwiftBundled/Bundled.swift"));
    assert!(is_swift_oracle_excluded_file("Sources/X/R.generated.swift"));
    assert!(!is_swift_oracle_excluded_file("Sources/Networking/Endpoint.swift"));
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
