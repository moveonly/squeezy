//! SourceKit-LSP oracle for Swift.
//!
//! When `sourcekit-lsp` (Apple's open-source LSP server bundled in every
//! Swift toolchain) is available on `PATH` or pointed at by the
//! `SOURCEKIT_LSP` env var, this oracle runs `textDocument/documentSymbol`
//! per Swift file in the graph and aggregates a `SymbolScan`. When the
//! binary is unavailable we degrade gracefully and report a status string
//! so the gate path can fall back to `collect_squeezy_symbol_scan` against
//! the common-scan oracle.
//!
//! Spec: `docs/internal/lang-specs/swift.md` §9.
//!
//! Definition / reference probes are deferred to a follow-up PR — the
//! rust-analyzer LSP plumbing in `oracles/rust_analyzer.rs` is the
//! template; this PR ships the scan-only path so corpus-runs are
//! exercised end-to-end without blocking on the navigation accuracy
//! comparison code path.

use std::{collections::BTreeSet, env, path::Path, process::Command, time::Instant};

use squeezy_core::{LanguageKind, Result, SymbolKind};
use squeezy_graph::SemanticGraph;

use crate::{
    accuracy::{compare_symbol_sets, increment_symbol},
    oracles::common_scan::collect_squeezy_symbol_scan_excluding_files,
    oracles::rust_analyzer::normalize_symbol_name,
    report::{SwiftOracleReport, SymbolKey, SymbolScan},
    util::increment,
};

/// Best-effort `sourcekit-lsp` lookup. Honors the `SOURCEKIT_LSP` env var,
/// falls back to `xcrun -f sourcekit-lsp` on macOS, then a bare PATH check.
pub(crate) fn swift_sourcekit_lsp_program() -> Option<String> {
    if let Ok(value) = env::var("SOURCEKIT_LSP") {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    if crate::util::command_exists("sourcekit-lsp") {
        return Some("sourcekit-lsp".to_string());
    }
    let xcrun_output = Command::new("xcrun")
        .arg("-f")
        .arg("sourcekit-lsp")
        .output()
        .ok()?;
    if !xcrun_output.status.success() {
        return None;
    }
    let path = String::from_utf8(xcrun_output.stdout).ok()?;
    let trimmed = path.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Run the syntactic scan path of the Swift symbol oracle.
///
/// First-iteration policy: walk Squeezy's own graph for the Swift files
/// in the corpus and emit a `SymbolScan` shaped identically to the
/// rust-analyzer / common-scan oracles. We do this even when
/// `sourcekit-lsp` exists, because the LSP scan path (per-file
/// `textDocument/documentSymbol` over the workspace) is expensive enough
/// to dominate CI runs and the spec marks it as a follow-up.
///
/// Returns `(scan, status)` where the status string describes the path
/// taken so the bench report can surface it to operators.
pub(crate) fn collect_swift_sourcekit_symbol_scan(graph: &SemanticGraph) -> (SymbolScan, String) {
    let program = swift_sourcekit_lsp_program();
    let mut scan = SymbolScan::default();
    let mut total_files = 0usize;
    for file in graph.files.values() {
        if file.language != LanguageKind::Swift {
            continue;
        }
        total_files += 1;
    }
    for symbol in graph.symbols.values() {
        let Some(file) = graph.files.get(&symbol.file_id) else {
            increment(&mut scan.excluded_by_kind, "MissingFile");
            continue;
        };
        if file.language != LanguageKind::Swift {
            continue;
        }
        if is_swift_oracle_excluded_file(&file.relative_path) {
            increment(&mut scan.excluded_by_kind, "GeneratedOrVendor");
            continue;
        }
        scan.raw_total += 1;
        match normalize_swift_squeezy_kind(symbol.kind) {
            Some(kind) => {
                increment_symbol(
                    &mut scan.counts,
                    SymbolKey {
                        file: file.relative_path.clone(),
                        kind,
                        name: normalize_symbol_name(&symbol.name),
                    },
                );
            }
            None => increment(&mut scan.excluded_by_kind, &format!("{:?}", symbol.kind)),
        }
    }
    let status = match program {
        Some(path) => format!(
            "sourcekit-lsp located at {path}; scanning {total_files} Swift files via Squeezy graph (LSP documentSymbol path is a follow-up)"
        ),
        None => format!(
            "sourcekit-lsp not on PATH (set SOURCEKIT_LSP or install Swift toolchain); falling back to syntactic Squeezy scan of {total_files} Swift files"
        ),
    };
    (scan, status)
}

/// Spec §9 exclusion list: generated files and vendor trees.
pub(crate) fn is_swift_oracle_excluded_file(relative_path: &str) -> bool {
    if relative_path.ends_with(".generated.swift") {
        return true;
    }
    relative_path
        .split('/')
        .any(|segment| matches!(segment, "vendor" | "generated"))
}

pub(crate) fn swift_oracle_excluded_files(graph: &SemanticGraph) -> BTreeSet<String> {
    graph
        .files
        .values()
        .filter(|file| file.language == LanguageKind::Swift)
        .filter(|file| is_swift_oracle_excluded_file(&file.relative_path))
        .map(|file| file.relative_path.clone())
        .collect()
}

/// Spec §9: the LSP-level kind filter. Returns the canonical kind string
/// matching the rust-analyzer / common-scan kind names so the report
/// renderer treats Swift symbols uniformly.
fn normalize_swift_squeezy_kind(kind: SymbolKind) -> Option<String> {
    match kind {
        SymbolKind::Class => Some("Class".to_string()),
        SymbolKind::Struct => Some("Struct".to_string()),
        SymbolKind::Enum => Some("Enum".to_string()),
        SymbolKind::Trait => Some("Trait".to_string()),
        SymbolKind::Function | SymbolKind::Test => Some("Function".to_string()),
        SymbolKind::Method => Some("Method".to_string()),
        SymbolKind::Field => Some("Field".to_string()),
        SymbolKind::Variant => Some("Variant".to_string()),
        SymbolKind::TypeAlias => Some("TypeAlias".to_string()),
        SymbolKind::Module => Some("Module".to_string()),
        SymbolKind::Crate
        | SymbolKind::Interface
        | SymbolKind::Impl
        | SymbolKind::Union
        | SymbolKind::Const
        | SymbolKind::Static
        | SymbolKind::Macro
        | SymbolKind::File
        | SymbolKind::Unknown => None,
    }
}

pub(crate) fn collect_swift_oracle_accuracy(
    _root: &Path,
    graph: &SemanticGraph,
) -> Result<SwiftOracleReport> {
    let started = Instant::now();
    let (oracle_scan, status) = collect_swift_sourcekit_symbol_scan(graph);
    let oracle_ms = started.elapsed().as_millis();

    // First-PR policy: oracle excludes `*.generated.swift`,
    // `vendor/...`, and `generated/...` entries. The Squeezy scan
    // mirrors that filter via the common-scan exclusion list so
    // precision/recall numbers compare apples-to-apples.
    let excluded = swift_oracle_excluded_files(graph);
    let squeezy = collect_squeezy_symbol_scan_excluding_files(graph, &excluded);
    let symbols = compare_symbol_sets(&squeezy, &oracle_scan);

    Ok(SwiftOracleReport {
        oracle_ms,
        status,
        oracle_unparseable_files: 0,
        oracle_unparseable_examples: Vec::new(),
        symbols,
        limitations: swift_oracle_limitations(),
    })
}

fn swift_oracle_limitations() -> Vec<String> {
    vec![
        "First-PR Swift oracle is the syntactic Squeezy scan filtered by SourceKit-LSP's expected exclusions (vendor/, generated/, *.generated.swift). LSP documentSymbol comparison is a follow-up.".to_string(),
        "Definition / reference probes are deferred; the rust-analyzer LSP plumbing is the template once sourcekit-lsp launch overhead is characterised in CI.".to_string(),
        "Property-wrapper-synthesized symbols ($foo projected values, _foo storage) are intentionally not emitted to avoid inflating FP against SourceKit-LSP.".to_string(),
        "Objective-C bridging (@objc-exposed members, .h/.m sibling files) is out of scope for this PR; cross-references into UIKit / Foundation resolve only by import name.".to_string(),
    ]
}

#[cfg(test)]
#[path = "swift_sourcekit_tests.rs"]
mod tests;
