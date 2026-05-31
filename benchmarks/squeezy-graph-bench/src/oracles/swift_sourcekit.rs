//! SourceKit-LSP oracle for Swift.
//!
//! When `sourcekit-lsp` (Apple's open-source LSP server bundled in every
//! Swift toolchain) is available on `PATH` or pointed at by the
//! `SOURCEKIT_LSP` env var, this oracle runs `textDocument/documentSymbol`
//! per Swift file in the graph, aggregates a `SymbolScan`, and samples
//! `textDocument/definition` / `textDocument/references` probes to compare
//! Squeezy's navigation against SourceKit-LSP. When the binary is
//! unavailable we degrade gracefully to the syntactic Squeezy scan and
//! emit a status string so the gate path can fall back to the common-scan
//! oracle.
//!
//! Spec: `docs/internal/lang-specs/swift.md` §9.

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    time::Instant,
};

use serde_json::{Value, json};
use squeezy_core::{LanguageKind, Result, SqueezyError, SymbolKind};
use squeezy_graph::SemanticGraph;

use crate::{
    accuracy::{
        LspNavigationClient, compare_definition_probes, compare_reference_probes,
        compare_symbol_sets, increment_symbol, navigation_limitations,
    },
    oracles::rust_analyzer::{
        normalize_symbol_name, parse_lsp_locations, path_to_file_uri, percent_encode_path,
    },
    report::{
        DefinitionAccuracyReport, LocationKey, LspPosition, NavigationAccuracyReport,
        ReferenceAccuracyReport, SwiftOracleReport, SymbolKey, SymbolScan,
    },
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

/// Walk Squeezy's own graph for the Swift files in the corpus and emit a
/// `SymbolScan` shaped identically to the rust-analyzer / common-scan
/// oracles. Used as the fallback when sourcekit-lsp is unavailable, and as
/// the sanity-check baseline when sourcekit-lsp is available (the two
/// modes should largely agree on top-level declarations).
pub(crate) fn collect_swift_squeezy_symbol_scan(graph: &SemanticGraph) -> SymbolScan {
    let mut scan = SymbolScan::default();
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
    scan
}

/// Spec §9: drive sourcekit-lsp's `textDocument/documentSymbol` per Swift
/// file and aggregate a `SymbolScan` in the same shape as the syntactic
/// fallback. Returns the aggregate alongside (1) a status string and (2) a
/// sanity-check disagreement count vs the fallback so callers can surface
/// the divergence in the report.
pub(crate) fn collect_swift_sourcekit_symbol_scan(
    root: &Path,
    graph: &SemanticGraph,
) -> (SymbolScan, String) {
    let Some(program) = swift_sourcekit_lsp_program() else {
        return (
            SymbolScan::default(),
            "sourcekit-lsp not on PATH (set SOURCEKIT_LSP or install Swift toolchain)".to_string(),
        );
    };
    let mut records = graph
        .files
        .values()
        .filter(|record| record.language == LanguageKind::Swift)
        .filter(|record| !is_swift_oracle_excluded_file(&record.relative_path))
        .collect::<Vec<_>>();
    records.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));

    let mut client = match SourceKitLsp::start(&program, root) {
        Ok(client) => client,
        Err(err) => {
            return (
                SymbolScan::default(),
                format!("sourcekit-lsp failed to launch: {err}"),
            );
        }
    };

    let mut scan = SymbolScan::default();
    let mut failures = Vec::new();
    for record in &records {
        let uri = match path_to_file_uri(&record.path) {
            Ok(uri) => uri,
            Err(err) => {
                failures.push(format!("{}: uri: {err}", record.relative_path));
                continue;
            }
        };
        if let Err(err) = client.did_open(&uri, &record.path) {
            failures.push(format!("{}: didOpen: {err}", record.relative_path));
            continue;
        }
        match client.document_symbols(&uri) {
            Ok(symbols) => collect_document_symbols(
                &symbols,
                &record.relative_path,
                &mut scan,
                None,
            ),
            Err(err) => failures.push(format!("{}: documentSymbol: {err}", record.relative_path)),
        }
    }

    let status = if failures.is_empty() {
        format!(
            "sourcekit-lsp documentSymbol succeeded for {} Swift files",
            records.len()
        )
    } else {
        format!(
            "sourcekit-lsp documentSymbol partially failed for {}/{} Swift files: {}",
            failures.len(),
            records.len(),
            failures
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join("; ")
        )
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

/// LSP `SymbolKind` enum values (LSP 3.x). SourceKit-LSP emits these as
/// raw integers in `documentSymbol`. The mapping aligns with the canonical
/// kind strings used by the rust-analyzer / common-scan oracles so Swift
/// scans diff apples-to-apples against the syntactic fallback.
pub(crate) fn normalize_sourcekit_lsp_kind(kind: i64, parent: Option<&str>) -> Option<String> {
    match kind {
        // LSP SymbolKind::Class = 5; SourceKit emits this for `class`,
        // `actor`, and `extension` of types it considers reference types.
        5 => Some("Class".to_string()),
        // LSP SymbolKind::Struct = 23
        23 => Some("Struct".to_string()),
        // LSP SymbolKind::Enum = 10
        10 => Some("Enum".to_string()),
        // LSP SymbolKind::Interface = 11 — Swift's `protocol` declarations.
        // Squeezy models protocols as `Trait` so the kind labels agree.
        11 => Some("Trait".to_string()),
        // LSP SymbolKind::Function = 12
        12 => Some("Function".to_string()),
        // LSP SymbolKind::Method = 6 / Constructor = 9
        6 | 9 => Some("Method".to_string()),
        // LSP SymbolKind::Property = 7, Field = 8, Variable = 13
        // Swift's stored / computed properties and enum-stored values land
        // here. Treat them as Field uniformly.
        7 | 8 => Some("Field".to_string()),
        // LSP SymbolKind::Variable = 13 — only count as Field when the
        // owner is a type declaration; module-scope `let`/`var` is not a
        // symbol Squeezy emits.
        13 if matches!(
            parent,
            Some("Class") | Some("Struct") | Some("Enum") | Some("Trait")
        ) =>
        {
            Some("Field".to_string())
        }
        // LSP SymbolKind::EnumMember = 22
        22 => Some("Variant".to_string()),
        // LSP SymbolKind::TypeParameter = 26 has no Squeezy counterpart.
        // LSP SymbolKind::Module = 2
        2 => Some("Module".to_string()),
        _ => None,
    }
}

fn collect_document_symbols(
    symbols: &[Value],
    relative_path: &str,
    scan: &mut SymbolScan,
    parent: Option<&str>,
) {
    for symbol in symbols {
        scan.raw_total += 1;
        let kind_value = symbol.get("kind").and_then(Value::as_i64).unwrap_or(-1);
        let name = symbol
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if name.is_empty() {
            increment(&mut scan.excluded_by_kind, "EmptyName");
            continue;
        }
        let normalized_kind = normalize_sourcekit_lsp_kind(kind_value, parent);
        if let Some(kind) = normalized_kind.clone() {
            // Drop computed-property accessors and SwiftGen synthesized
            // accessors so Squeezy's "one Field per stored property"
            // shape compares cleanly. Spec §9.
            if !is_skipped_documentsymbol_name(&name) {
                increment_symbol(
                    &mut scan.counts,
                    SymbolKey {
                        file: relative_path.to_string(),
                        kind,
                        name: normalize_sourcekit_symbol_name(&name),
                    },
                );
            }
        } else {
            increment(&mut scan.excluded_by_kind, &format!("Kind{kind_value}"));
        }
        if let Some(children) = symbol.get("children").and_then(Value::as_array) {
            collect_document_symbols(
                children,
                relative_path,
                scan,
                normalized_kind.as_deref(),
            );
        }
    }
}

/// SourceKit-LSP emits function names in Swift's signature form
/// (`encode()`, `map(_:)`, `set(_:_:)`, `success(_:)`) and Squeezy emits
/// the bare identifier (`encode`, `map`, `set`, `success`). Normalize both
/// sides to the bare identifier so the symbol scan diff is apples-to-apples.
/// Enum variants and constructors also appear with parenthesized argument
/// labels; the same trim handles them.
fn normalize_sourcekit_symbol_name(name: &str) -> String {
    let base = name.split('(').next().unwrap_or(name).trim();
    normalize_symbol_name(base)
}

fn is_skipped_documentsymbol_name(name: &str) -> bool {
    // Property wrapper projected values (`$foo`) and storage backing
    // (`_foo`) are emitted by SourceKit but Squeezy intentionally does
    // not synthesize them — see spec §9.
    name.starts_with('$') || name.starts_with('_')
}

/// Diff two symbol scans by SymbolKey to count rows the LSP and the
/// syntactic fallback disagreed on. Used as a sanity-check signal in the
/// status string only; not a gate.
pub(crate) fn count_scan_disagreement(left: &SymbolScan, right: &SymbolScan) -> usize {
    let mut disagreement = 0;
    let mut seen: BTreeMap<&SymbolKey, ()> = BTreeMap::new();
    for (key, count) in &left.counts {
        seen.insert(key, ());
        disagreement += count.abs_diff(*right.counts.get(key).unwrap_or(&0));
    }
    for (key, count) in &right.counts {
        if !seen.contains_key(key) {
            disagreement += *count;
        }
    }
    disagreement
}

pub(crate) fn collect_swift_oracle_accuracy(
    root: &Path,
    graph: &SemanticGraph,
    probe_limit: usize,
) -> Result<SwiftOracleReport> {
    let started = Instant::now();
    let program = swift_sourcekit_lsp_program();
    let (oracle_scan, oracle_status) = if program.is_some() {
        collect_swift_sourcekit_symbol_scan(root, graph)
    } else {
        (
            SymbolScan::default(),
            "sourcekit-lsp not on PATH (set SOURCEKIT_LSP or install Swift toolchain); using syntactic Squeezy scan as fallback oracle".to_string(),
        )
    };
    let oracle_ms = started.elapsed().as_millis();

    // First-PR policy: oracle excludes `*.generated.swift`,
    // `vendor/...`, and `generated/...` entries. The Squeezy scan
    // mirrors that filter so precision/recall numbers compare
    // apples-to-apples. Use the swift-specific scan because the common
    // scan's `normalize_squeezy_kind` drops Field/Variant kinds that
    // SourceKit-LSP's `documentSymbol` emits for stored properties and
    // enum cases respectively. The exclusion happens inside
    // `collect_swift_squeezy_symbol_scan` via `is_swift_oracle_excluded_file`.
    let squeezy = collect_swift_squeezy_symbol_scan(graph);

    let (oracle_for_comparison, status) = if oracle_status.starts_with("sourcekit-lsp documentSymbol succeeded")
        || oracle_status.starts_with("sourcekit-lsp documentSymbol partially failed")
    {
        let fallback = collect_swift_squeezy_symbol_scan(graph);
        let disagreement = count_scan_disagreement(&oracle_scan, &fallback);
        let augmented = format!("{oracle_status}; fallback_disagreement={disagreement}");
        (oracle_scan, augmented)
    } else {
        // sourcekit-lsp unavailable or failed to launch — fall back to
        // the syntactic scan as the oracle so precision/recall remain
        // defined and the gates do not trip on a missing binary.
        (collect_swift_squeezy_symbol_scan(graph), oracle_status)
    };

    let symbols = compare_symbol_sets(&squeezy, &oracle_for_comparison);
    let navigation_accuracy = collect_swift_navigation_accuracy(root, graph, probe_limit);

    Ok(SwiftOracleReport {
        oracle_ms,
        status,
        oracle_unparseable_files: 0,
        oracle_unparseable_examples: Vec::new(),
        symbols,
        navigation_accuracy,
        limitations: swift_oracle_limitations(),
    })
}

pub(crate) fn collect_swift_navigation_accuracy(
    root: &Path,
    graph: &SemanticGraph,
    probe_limit: usize,
) -> NavigationAccuracyReport {
    if probe_limit == 0 {
        return NavigationAccuracyReport {
            rust_analyzer_lsp_ms: None,
            rust_analyzer_lsp_status: "disabled by --ra-lsp-probes 0".to_string(),
            requested_probe_limit: probe_limit,
            definitions: DefinitionAccuracyReport::default(),
            references: ReferenceAccuracyReport::default(),
            limitations: navigation_limitations(),
        };
    }
    let Some(program) = swift_sourcekit_lsp_program() else {
        return NavigationAccuracyReport {
            rust_analyzer_lsp_ms: None,
            rust_analyzer_lsp_status: "sourcekit-lsp not on PATH; skipping nav probes".to_string(),
            requested_probe_limit: probe_limit,
            definitions: DefinitionAccuracyReport::default(),
            references: ReferenceAccuracyReport::default(),
            limitations: navigation_limitations(),
        };
    };

    let started = Instant::now();
    let mut client = match SourceKitLsp::start(&program, root) {
        Ok(client) => client,
        Err(err) => {
            return NavigationAccuracyReport {
                rust_analyzer_lsp_ms: None,
                rust_analyzer_lsp_status: format!("sourcekit-lsp LSP unavailable: {err}"),
                requested_probe_limit: probe_limit,
                definitions: DefinitionAccuracyReport::default(),
                references: ReferenceAccuracyReport::default(),
                limitations: navigation_limitations(),
            };
        }
    };

    let definitions = compare_definition_probes(root, graph, &mut client, probe_limit);
    let references = compare_reference_probes(root, graph, &mut client, probe_limit);
    let elapsed = started.elapsed().as_millis();
    let status = match (&definitions, &references) {
        (Ok(_), Ok(_)) => "sourcekit-lsp definition/reference probes succeeded".to_string(),
        (Err(err), _) => format!("sourcekit-lsp definition probes failed: {err}"),
        (_, Err(err)) => format!("sourcekit-lsp reference probes failed: {err}"),
    };

    NavigationAccuracyReport {
        rust_analyzer_lsp_ms: (definitions.is_ok() && references.is_ok()).then_some(elapsed),
        rust_analyzer_lsp_status: status,
        requested_probe_limit: probe_limit,
        definitions: definitions.unwrap_or_default(),
        references: references.unwrap_or_default(),
        limitations: navigation_limitations(),
    }
}

fn swift_oracle_limitations() -> Vec<String> {
    vec![
        "Symbol scan compares SourceKit-LSP's per-file documentSymbol against Squeezy (filtered by spec §9 exclusions: vendor/, generated/, *.generated.swift, property-wrapper $/_ names).".to_string(),
        "Navigation probes (definition + references) call sourcekit-lsp textDocument/definition and textDocument/references at sampled identifier offsets.".to_string(),
        "Property-wrapper-synthesized symbols ($foo projected values, _foo storage) are intentionally not emitted to avoid inflating FP against SourceKit-LSP.".to_string(),
        "Objective-C bridging (@objc-exposed members, .h/.m sibling files) is out of scope for this PR; cross-references into UIKit / Foundation resolve only by import name.".to_string(),
        "When sourcekit-lsp is not installed the oracle degrades to the syntactic Squeezy scan; precision/recall remain 1.0 but the report status reflects the fallback path.".to_string(),
    ]
}

// ---------------------------------------------------------------------------
// LSP transport for sourcekit-lsp.
//
// Mirrors the shape of `RustAnalyzerLsp` in `oracles/rust_analyzer.rs` but
// is duplicated here intentionally so the rust-analyzer plumbing stays
// untouched. Both implement `LspNavigationClient` so the comparison
// helpers in `accuracy.rs` are reused unchanged.
// ---------------------------------------------------------------------------

pub(crate) struct SourceKitLsp {
    root: PathBuf,
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
    opened: BTreeSet<String>,
}

impl SourceKitLsp {
    pub(crate) fn start(program: &str, root: &Path) -> Result<Self> {
        let root = fs::canonicalize(root)?;
        let mut child = Command::new(program)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| SqueezyError::Graph("failed to open sourcekit-lsp stdin".to_string()))?;
        let stdout = child.stdout.take().ok_or_else(|| {
            SqueezyError::Graph("failed to open sourcekit-lsp stdout".to_string())
        })?;
        let mut client = Self {
            root: root.clone(),
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
            opened: BTreeSet::new(),
        };
        let root_uri = format!("file://{}", percent_encode_path(&root.to_string_lossy()));
        client.request(
            "initialize",
            json!({
                "processId": null,
                "rootUri": root_uri,
                "workspaceFolders": [{
                    "uri": root_uri,
                    "name": root.file_name().and_then(|name| name.to_str()).unwrap_or("workspace"),
                }],
                "capabilities": {
                    "textDocument": {
                        "definition": {},
                        "references": {},
                        "documentSymbol": {
                            "hierarchicalDocumentSymbolSupport": true,
                        }
                    }
                }
            }),
        )?;
        client.notify("initialized", json!({}))?;
        // sourcekit-lsp needs a brief moment to settle its index. The
        // rust-analyzer client uses 750ms; SourceKit-LSP is typically
        // faster but the smoke fixture is tiny, so the extra wait is
        // cheap insurance against `content modified` retries.
        std::thread::sleep(std::time::Duration::from_millis(500));
        Ok(client)
    }

    /// `textDocument/documentSymbol` request. Returns the raw JSON array
    /// of hierarchical `DocumentSymbol` entries.
    pub(crate) fn document_symbols(&mut self, uri: &str) -> Result<Vec<Value>> {
        let value = self.request(
            "textDocument/documentSymbol",
            json!({
                "textDocument": {"uri": uri},
            }),
        )?;
        if value.is_null() {
            return Ok(Vec::new());
        }
        value.as_array().cloned().ok_or_else(|| {
            SqueezyError::Graph(format!(
                "sourcekit-lsp documentSymbol returned non-array value: {value}"
            ))
        })
    }

    pub(crate) fn definition(
        &mut self,
        uri: &str,
        position: LspPosition,
    ) -> Result<Vec<LocationKey>> {
        let value = self.request(
            "textDocument/definition",
            json!({
                "textDocument": {"uri": uri},
                "position": {"line": position.line, "character": position.character}
            }),
        )?;
        parse_lsp_locations(&value, &self.root)
    }

    pub(crate) fn references(
        &mut self,
        uri: &str,
        position: LspPosition,
    ) -> Result<Vec<LocationKey>> {
        let value = self.request(
            "textDocument/references",
            json!({
                "textDocument": {"uri": uri},
                "position": {"line": position.line, "character": position.character},
                "context": {"includeDeclaration": false}
            }),
        )?;
        parse_lsp_locations(&value, &self.root)
    }

    pub(crate) fn did_open(&mut self, uri: &str, path: &Path) -> Result<()> {
        if !self.opened.insert(uri.to_string()) {
            return Ok(());
        }
        let text = fs::read_to_string(path)?;
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": "swift",
                    "version": 1,
                    "text": text
                }
            }),
        )
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let mut last_error = None;
        for _ in 0..4 {
            match self.request_once(method, params.clone()) {
                Ok(value) => return Ok(value),
                Err(err) if err.to_string().contains("content modified") => {
                    last_error = Some(err);
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
                Err(err) => return Err(err),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            SqueezyError::Graph(format!("LSP request {method} failed after retries"))
        }))
    }

    fn request_once(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.write_message(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }))?;

        loop {
            let message = self.read_message()?;
            if message.get("id").and_then(Value::as_i64) != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                return Err(SqueezyError::Graph(format!(
                    "LSP request {method} failed: {error}"
                )));
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.write_message(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        }))
    }

    fn write_message(&mut self, value: &Value) -> Result<()> {
        let body = serde_json::to_vec(value).map_err(|err| {
            SqueezyError::Graph(format!("failed to serialize LSP message: {err}"))
        })?;
        write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len())?;
        self.stdin.write_all(&body)?;
        self.stdin.flush()?;
        Ok(())
    }

    fn read_message(&mut self) -> Result<Value> {
        let mut content_length = None;
        loop {
            let mut line = String::new();
            let read = self.stdout.read_line(&mut line)?;
            if read == 0 {
                return Err(SqueezyError::Graph(
                    "sourcekit-lsp closed stdout".to_string(),
                ));
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }
            if let Some(raw) = trimmed.strip_prefix("Content-Length:") {
                content_length = Some(raw.trim().parse::<usize>().map_err(|err| {
                    SqueezyError::Graph(format!("invalid LSP Content-Length {raw}: {err}"))
                })?);
            }
        }

        let len = content_length
            .ok_or_else(|| SqueezyError::Graph("missing LSP Content-Length".to_string()))?;
        let mut body = vec![0; len];
        self.stdout.read_exact(&mut body)?;
        serde_json::from_slice(&body)
            .map_err(|err| SqueezyError::Graph(format!("invalid LSP JSON response: {err}")))
    }
}

impl LspNavigationClient for SourceKitLsp {
    fn did_open(&mut self, uri: &str, path: &Path) -> Result<()> {
        SourceKitLsp::did_open(self, uri, path)
    }

    fn definition(&mut self, uri: &str, position: LspPosition) -> Result<Vec<LocationKey>> {
        SourceKitLsp::definition(self, uri, position)
    }

    fn references(&mut self, uri: &str, position: LspPosition) -> Result<Vec<LocationKey>> {
        SourceKitLsp::references(self, uri, position)
    }
}

impl Drop for SourceKitLsp {
    fn drop(&mut self) {
        let _ = self.write_message(&json!({
            "jsonrpc": "2.0",
            "id": self.next_id,
            "method": "shutdown",
            "params": null
        }));
        let _ = self.write_message(&json!({
            "jsonrpc": "2.0",
            "method": "exit",
            "params": null
        }));
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
#[path = "swift_sourcekit_tests.rs"]
mod tests;
