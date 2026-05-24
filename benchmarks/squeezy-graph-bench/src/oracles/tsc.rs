use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    path::Path,
    process::{Command, Stdio},
    time::Instant,
};

use serde::Deserialize;
use serde_json::{Value, json};
use squeezy_core::{EdgeKind, LanguageKind, Result, SqueezyError, SymbolId, SymbolKind};
use squeezy_graph::SemanticGraph;

use crate::{
    accuracy::{
        compare_symbol_sets, increment_symbol, location_key_for_reference_hit,
        location_matches_symbol, probe_byte_for_edge, probe_byte_for_symbol, push_example, ratio,
        render_locations,
    },
    mixed::select_scenarios,
    oracles::{
        common_scan::collect_squeezy_symbol_scan,
        rust_analyzer::{byte_to_lsp_position, normalize_symbol_name},
    },
    report::{
        AccuracyReport, DefinitionAccuracyReport, JsTsOracleReport, LocationKey,
        NavigationAccuracyReport, ReferenceAccuracyReport, SymbolKey, SymbolScan,
    },
};

#[derive(Debug, Deserialize)]
pub(crate) struct JsTsOracleSymbol {
    file: String,
    kind: String,
    name: String,
}

pub(crate) fn collect_js_ts_oracle_accuracy(
    root: &Path,
    graph: &SemanticGraph,
) -> JsTsOracleReport {
    let started = Instant::now();
    match collect_js_ts_symbol_scan(root) {
        Ok(oracle) => JsTsOracleReport {
            oracle_ms: started.elapsed().as_millis(),
            status: "TypeScript compiler API symbol oracle succeeded".to_string(),
            symbols: compare_symbol_sets(&collect_squeezy_symbol_scan(graph), &oracle),
            limitations: js_ts_oracle_limitations(),
        },
        Err(err) => JsTsOracleReport {
            oracle_ms: started.elapsed().as_millis(),
            status: format!("TypeScript compiler API oracle unavailable: {err}"),
            symbols: compare_symbol_sets(&SymbolScan::default(), &SymbolScan::default()),
            limitations: js_ts_oracle_limitations(),
        },
    }
}

pub(crate) fn collect_js_ts_symbol_scan(root: &Path) -> Result<SymbolScan> {
    let script = r#"
const fs = require("fs");
const path = require("path");
const ts = require(process.env.SQUEEZY_TYPESCRIPT_PATH || "typescript");
const root = process.argv[1];
const out = [];
const GENERATED_MARKERS = [
  "@generated",
  "auto-generated",
  "automatically generated",
  "code generated",
  "do not edit",
];
const GENERATED_PREFIX_BYTES = 4096;
const SKIP_DIR_NAMES = new Set([
  ".git",
  "node_modules",
  "dist",
  "build",
  "coverage",
  "out",
  "vendor",
  "third_party",
]);
function walk(dir) {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    if (SKIP_DIR_NAMES.has(entry.name)) continue;
    if (entry.name.startsWith(".") && entry.name !== "." && entry.name !== "..") continue;
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      walk(full);
    } else if (/\.[cm]?[jt]sx?$/.test(entry.name)) {
      scan(full);
    }
  }
}
function rel(file) { return path.relative(root, file).split(path.sep).join("/"); }
function emit(file, kind, name) {
  if (name && /^[A-Za-z_$][A-Za-z0-9_$]*$/.test(name)) out.push({ file: rel(file), kind, name });
}
// Tracks loop/catch variable declaration list nodes so locals introduced by
// `for (const x of ...)`, `for (let x = ...; ...; ...)` and `catch (e)` are not
// counted against Squeezy's declaration set. Squeezy's graph anchors these on
// dedicated AST nodes and only synthesizes a binding symbol when the binding
// is a simple identifier, so this matches the local heuristic on both sides.
function isLoopOrCatchLocal(node) {
  const parent = node.parent;
  if (!parent) return false;
  if (ts.isCatchClause(parent)) return true;
  if (parent.kind === ts.SyntaxKind.VariableDeclarationList) {
    const grand = parent.parent;
    if (grand && (
      ts.isForInStatement(grand) ||
      ts.isForOfStatement(grand) ||
      ts.isForStatement(grand)
    )) {
      // For-statement initializer can still be a top-level lexical declaration,
      // but `for (let i = 0; ...)` is a loop local that the oracle should skip
      // because Squeezy does not promote it to a graph symbol either.
      return true;
    }
  }
  return false;
}
function scan(file) {
  const source = fs.readFileSync(file, "utf8");
  const head = source.slice(0, GENERATED_PREFIX_BYTES).toLowerCase();
  if (GENERATED_MARKERS.some((marker) => head.includes(marker))) return;
  const sf = ts.createSourceFile(file, source, ts.ScriptTarget.Latest, true, file.endsWith("x") ? ts.ScriptKind.TSX : ts.ScriptKind.TS);
  function visit(node) {
    if ((ts.isFunctionDeclaration(node) || ts.isFunctionExpression(node)) && node.name) emit(file, "Function", node.name.text);
    else if (ts.isClassDeclaration(node) && node.name) emit(file, "Class", node.name.text);
    else if (ts.isInterfaceDeclaration(node)) emit(file, "Interface", node.name.text);
    else if (ts.isModuleDeclaration(node) && ts.isIdentifier(node.name)) emit(file, "Module", node.name.text);
    else if (ts.isTypeAliasDeclaration(node)) emit(file, "TypeAlias", node.name.text);
    else if (ts.isEnumDeclaration(node)) emit(file, "Enum", node.name.text);
    else if ((ts.isMethodDeclaration(node) || ts.isMethodSignature(node)) && node.name && ts.isIdentifier(node.name)) emit(file, "Method", node.name.text);
    else if (ts.isPropertyDeclaration(node) && node.name && ts.isIdentifier(node.name)) {
      const init = node.initializer;
      if (init && (ts.isArrowFunction(init) || ts.isFunctionExpression(init))) emit(file, "Method", node.name.text);
    }
    else if (ts.isVariableDeclaration(node) && ts.isIdentifier(node.name) && !isLoopOrCatchLocal(node)) {
      const init = node.initializer;
      emit(file, init && (ts.isArrowFunction(init) || ts.isFunctionExpression(init)) ? "Function" : "Const", node.name.text);
    }
    ts.forEachChild(node, visit);
  }
  visit(sf);
}
walk(root);
console.log(JSON.stringify(out));
"#;
    let output = Command::new("node")
        .arg("-e")
        .arg(script)
        .arg(root)
        .output()
        .map_err(|err| SqueezyError::Graph(format!("node unavailable: {err}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = if stderr.contains("Cannot find module 'typescript'") {
            "node package 'typescript' is not installed".to_string()
        } else {
            stderr
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("node TypeScript oracle failed")
                .trim()
                .to_string()
        };
        return Err(SqueezyError::Graph(message));
    }
    let symbols: Vec<JsTsOracleSymbol> = serde_json::from_slice(&output.stdout)
        .map_err(|err| SqueezyError::Graph(format!("invalid JS/TS oracle JSON: {err}")))?;
    let mut scan = SymbolScan::default();
    for symbol in symbols {
        scan.raw_total += 1;
        increment_symbol(
            &mut scan.counts,
            SymbolKey {
                file: symbol.file,
                kind: symbol.kind,
                name: normalize_symbol_name(&symbol.name),
            },
        );
    }
    Ok(scan)
}

pub(crate) fn time_js_ts_oracle(fixture: &Path) -> Result<u128> {
    let started = Instant::now();
    let _ = collect_js_ts_symbol_scan(fixture)?;
    Ok(started.elapsed().as_millis())
}

pub(crate) fn js_ts_oracle_limitations() -> Vec<String> {
    vec![
        "The JS/TS oracle uses the TypeScript compiler API only in benchmark tooling; production navigation remains tree-sitter-only.".to_string(),
        "Symbol comparison is file/name/kind based and does not prove dynamic JavaScript dispatch, bundler aliases, or runtime module loading.".to_string(),
        "When node or the typescript package is unavailable, benchmark reports keep the oracle status explicit instead of blocking production parser tests.".to_string(),
    ]
}

/// Combine symbol oracle + TypeScript Language Service navigation probes into a
/// single `AccuracyReport`. When `probe_limit == 0` the navigation half is
/// skipped (same semantics as `--ra-lsp-probes 0` for Rust).
pub(crate) fn collect_js_ts_accuracy(
    root: &Path,
    graph: &SemanticGraph,
    probe_limit: usize,
) -> AccuracyReport {
    let oracle = collect_js_ts_oracle_accuracy(root, graph);
    let navigation = collect_js_ts_navigation_accuracy(root, graph, probe_limit);
    AccuracyReport {
        rust_analyzer_symbols_ms: Some(oracle.oracle_ms),
        rust_analyzer_symbol_status: oracle.status.clone(),
        symbols: oracle.symbols.clone(),
        navigation,
        limitations: oracle.limitations.clone(),
    }
}

// ─── JS/TS TypeScript Language Service navigation oracle ────────────────────

/// Probes built from resolved Squeezy call edges in JS/TS files.
pub(crate) struct TsDefProbe {
    label: String,
    relative_file: String,
    byte_offset: u32,
    squeezy_target: Option<SymbolId>,
}

/// Probes built from JS/TS declaration symbols for reference comparison.
pub(crate) struct TsRefProbe {
    label: String,
    relative_file: String,
    byte_offset: u32,
    symbol_id: SymbolId,
    name: String,
}

pub(crate) fn js_ts_language(language: LanguageKind) -> bool {
    matches!(
        language,
        LanguageKind::JavaScript | LanguageKind::Jsx | LanguageKind::TypeScript | LanguageKind::Tsx
    )
}

pub(crate) fn build_ts_definition_probes(
    graph: &SemanticGraph,
    limit: usize,
) -> Result<(usize, Vec<TsDefProbe>)> {
    let mut edges: Vec<_> = graph
        .edges()
        .iter()
        .filter(|edge| edge.kind == EdgeKind::Calls)
        .filter_map(|edge| {
            let span = edge.span?;
            let from = graph.symbols.get(&edge.from)?;
            let file = graph.files.get(&from.file_id)?;
            if !js_ts_language(file.language) {
                return None;
            }
            Some((file.relative_path.clone(), span.start_byte, edge, file))
        })
        .collect();
    edges.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.1.cmp(&b.1))
            .then(a.2.target_text.cmp(&b.2.target_text))
    });
    let available = edges.len();
    let selected = select_scenarios(available, limit);

    let mut probes = Vec::new();
    for index in selected {
        let (_, _, edge, file) = edges[index];
        let Some(span) = edge.span else {
            continue;
        };
        let source = match fs::read_to_string(&file.path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let byte = probe_byte_for_edge(
            &source,
            span.start_byte as usize,
            span.end_byte as usize,
            &edge.target_text,
        );
        let pos = byte_to_lsp_position(&source, byte);
        probes.push(TsDefProbe {
            label: format!(
                "{}:{}:{} {}",
                file.relative_path,
                pos.line + 1,
                pos.character + 1,
                edge.target_text
            ),
            relative_file: file.relative_path.clone(),
            byte_offset: byte as u32,
            squeezy_target: edge.to.clone(),
        });
    }
    Ok((available, probes))
}

pub(crate) fn build_ts_reference_probes(
    graph: &SemanticGraph,
    limit: usize,
) -> Result<(usize, Vec<TsRefProbe>)> {
    let mut symbols: Vec<_> = graph
        .symbols
        .values()
        .filter(|sym| {
            matches!(
                sym.kind,
                SymbolKind::Function
                    | SymbolKind::Class
                    | SymbolKind::Interface
                    | SymbolKind::TypeAlias
                    | SymbolKind::Method
                    | SymbolKind::Enum
            ) && sym.name.len() >= 3
        })
        .filter(|sym| {
            graph
                .files
                .get(&sym.file_id)
                .map(|f| js_ts_language(f.language))
                .unwrap_or(false)
        })
        .collect();
    symbols.sort_by(|a, b| {
        a.file_id
            .0
            .cmp(&b.file_id.0)
            .then(a.span.start_byte.cmp(&b.span.start_byte))
            .then(a.name.cmp(&b.name))
    });
    let available = symbols.len();
    let selected = select_scenarios(available, limit);

    let mut probes = Vec::new();
    for index in selected {
        let sym = symbols[index];
        let Some(file) = graph.files.get(&sym.file_id) else {
            continue;
        };
        let source = match fs::read_to_string(&file.path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let byte = probe_byte_for_symbol(
            &source,
            sym.span.start_byte as usize,
            sym.span.end_byte as usize,
            &sym.name,
        );
        let pos = byte_to_lsp_position(&source, byte);
        probes.push(TsRefProbe {
            label: format!(
                "{}:{}:{} {}",
                file.relative_path,
                pos.line + 1,
                pos.character + 1,
                sym.name
            ),
            relative_file: file.relative_path.clone(),
            byte_offset: byte as u32,
            symbol_id: sym.id.clone(),
            name: sym.name.clone(),
        });
    }
    Ok((available, probes))
}

/// Embedded Node.js script that drives the TypeScript Language Service.
/// Reads a JSON document from stdin:
///   `{ root, def_probes: [{file, byte_offset, label}], ref_probes: [{file, byte_offset, label}] }`
/// Writes a JSON document to stdout:
///   `{ def_results: [{..., ts_locs: [{file, line, character}]}],
///      ref_results: [{..., ts_refs: [{file, line, character}]}] }`
const TS_NAV_ORACLE_SCRIPT: &str = r#"
(function() {
  'use strict';
  const fs = require('fs');
  const path = require('path');
  const ts = require(process.env.SQUEEZY_TYPESCRIPT_PATH || 'typescript');

  let buf = '';
  process.stdin.on('data', function(d) { buf += d; });
  process.stdin.on('end', function() {
    let parsed;
    try { parsed = JSON.parse(buf); } catch (e) {
      process.stdout.write(JSON.stringify({ error: 'parse input: ' + e }));
      return;
    }
    const root = parsed.root;
    const defProbes = parsed.def_probes || [];
    const refProbes = parsed.ref_probes || [];

    // Discover all JS/TS source files (skip generated/hidden/vendor)
    const SKIP = new Set(['.git','node_modules','dist','build','out','coverage',
      '__pycache__','vendor','.next','.nuxt','.svelte-kit']);
    const GEN_MARKERS = ['@generated','auto-generated','automatically generated',
      'code generated','do not edit'];
    const GEN_BYTES = 4096;
    const fileSet = new Set();
    function walk(dir) {
      let entries;
      try { entries = fs.readdirSync(dir, { withFileTypes: true }); } catch { return; }
      for (const e of entries) {
        if (SKIP.has(e.name) || e.name.startsWith('.')) continue;
        const full = path.join(dir, e.name);
        if (e.isDirectory()) { walk(full); continue; }
        if (!/\.[cm]?[jt]sx?$/.test(e.name)) continue;
        if (e.name.endsWith('.d.ts') || e.name.endsWith('.d.cts') || e.name.endsWith('.d.mts')) {
          // skip declaration files -- TypeScript treats them differently for findReferences
          continue;
        }
        try {
          const head = Buffer.allocUnsafe(GEN_BYTES);
          const fd = fs.openSync(full, 'r');
          const bytesRead = fs.readSync(fd, head, 0, GEN_BYTES, 0);
          fs.closeSync(fd);
          const preview = head.slice(0, bytesRead).toString('utf8').toLowerCase();
          if (GEN_MARKERS.some(function(m) { return preview.includes(m); })) continue;
        } catch {}
        fileSet.add(full);
      }
    }
    walk(root);
    const files = Array.from(fileSet);

    // TypeScript Language Service host
    const host = {
      getScriptFileNames: function() { return files; },
      getScriptVersion: function() { return '1'; },
      getScriptSnapshot: function(f) {
        try { return ts.ScriptSnapshot.fromString(fs.readFileSync(f, 'utf8')); } catch { return undefined; }
      },
      getCurrentDirectory: function() { return root; },
      getCompilationSettings: function() {
        return {
          target: ts.ScriptTarget.Latest,
          allowJs: true, checkJs: false,
          jsx: ts.JsxEmit.Preserve,
          moduleResolution: ts.ModuleResolutionKind.Node10,
          noEmit: true, skipLibCheck: true,
        };
      },
      getDefaultLibFileName: function(opts) { return ts.getDefaultLibFilePath(opts); },
      fileExists: ts.sys.fileExists,
      readFile: ts.sys.readFile,
      readDirectory: ts.sys.readDirectory,
      directoryExists: ts.sys.directoryExists,
      getDirectories: ts.sys.getDirectories,
      useCaseSensitiveFileNames: function() { return true; },
    };

    let ls;
    try {
      ls = ts.createLanguageService(host, ts.createDocumentRegistry());
    } catch (e) {
      process.stdout.write(JSON.stringify({ error: 'LanguageService: ' + e }));
      return;
    }

    function absPath(file) {
      return path.isAbsolute(file) ? file : path.join(root, file);
    }
    function relPath(file) {
      return path.relative(root, file).replace(/\\/g, '/');
    }
    function getLineChar(sf, offset) {
      try { return ts.getLineAndCharacterOfPosition(sf, offset); } catch { return { line: 0, character: 0 }; }
    }

    // Definition probes
    const defResults = defProbes.map(function(probe) {
      const absFile = absPath(probe.file);
      try {
        const prog = ls.getProgram();
        const defs = ls.getDefinitionAtPosition(absFile, probe.byte_offset) || [];
        const tsLocs = defs.reduce(function(acc, d) {
          const sf = prog ? prog.getSourceFile(d.fileName) : null;
          if (!sf) return acc;
          const lc = getLineChar(sf, d.textSpan.start);
          acc.push({ file: relPath(d.fileName), line: lc.line, character: lc.character });
          return acc;
        }, []);
        return { file: probe.file, byte_offset: probe.byte_offset, label: probe.label, ts_locs: tsLocs };
      } catch (e) {
        return { file: probe.file, byte_offset: probe.byte_offset, label: probe.label, ts_locs: [], error: String(e) };
      }
    });

    // Reference probes
    const refResults = refProbes.map(function(probe) {
      const absFile = absPath(probe.file);
      try {
        const prog = ls.getProgram();
        const groups = ls.findReferences(absFile, probe.byte_offset) || [];
        const tsRefs = [];
        for (const group of groups) {
          for (const ref of group.references) {
            if (ref.isDefinition) continue;  // exclude the declaration itself
            const sf = prog ? prog.getSourceFile(ref.fileName) : null;
            if (!sf) continue;
            const lc = getLineChar(sf, ref.textSpan.start);
            tsRefs.push({ file: relPath(ref.fileName), line: lc.line, character: lc.character });
          }
        }
        return { file: probe.file, byte_offset: probe.byte_offset, label: probe.label, ts_refs: tsRefs };
      } catch (e) {
        return { file: probe.file, byte_offset: probe.byte_offset, label: probe.label, ts_refs: [], error: String(e) };
      }
    });

    process.stdout.write(JSON.stringify({ def_results: defResults, ref_results: refResults }));
  });
})();
"#;

pub(crate) fn run_ts_navigation_node(
    root: &Path,
    def_probes: &[TsDefProbe],
    ref_probes: &[TsRefProbe],
) -> Result<Value> {
    let input = json!({
        "root": root.to_string_lossy(),
        "def_probes": def_probes.iter().map(|p| json!({
            "file": p.relative_file,
            "byte_offset": p.byte_offset,
            "label": p.label,
        })).collect::<Vec<_>>(),
        "ref_probes": ref_probes.iter().map(|p| json!({
            "file": p.relative_file,
            "byte_offset": p.byte_offset,
            "label": p.label,
        })).collect::<Vec<_>>(),
    });
    let input_bytes = input.to_string().into_bytes();

    let mut child = Command::new("node")
        .arg("-e")
        .arg(TS_NAV_ORACLE_SCRIPT)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| SqueezyError::Graph(format!("node unavailable: {e}")))?;

    // Write all probe data to stdin then close it so node sees EOF.
    {
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| SqueezyError::Graph("node stdin unavailable".to_string()))?;
        let mut stdin = stdin;
        stdin
            .write_all(&input_bytes)
            .map_err(|e| SqueezyError::Graph(format!("node stdin write: {e}")))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| SqueezyError::Graph(format!("node wait: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first_line = stderr.lines().next().unwrap_or("(no stderr)");
        return Err(SqueezyError::Graph(format!(
            "node TS navigation oracle failed: {first_line}"
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let result: Value = serde_json::from_str(&stdout)
        .map_err(|e| SqueezyError::Graph(format!("TS navigation oracle JSON: {e}")))?;

    if let Some(err) = result.get("error").and_then(|v| v.as_str()) {
        return Err(SqueezyError::Graph(format!(
            "TS navigation oracle error: {err}"
        )));
    }
    Ok(result)
}

pub(crate) fn score_ts_definition_results(
    root: &Path,
    graph: &SemanticGraph,
    probes: &[TsDefProbe],
    available: usize,
    raw_results: &[Value],
) -> DefinitionAccuracyReport {
    let mut report = DefinitionAccuracyReport {
        available_probes: available,
        probes: probes.len(),
        ..DefinitionAccuracyReport::default()
    };
    for (probe, result) in probes.iter().zip(raw_results.iter()) {
        let ts_locs: Vec<LocationKey> = result["ts_locs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| {
                let file = v["file"].as_str()?.to_string();
                let line = v["line"].as_u64()? as u32;
                let character = v["character"].as_u64()? as u32;
                Some(LocationKey {
                    file,
                    line,
                    character,
                })
            })
            .collect();

        let squeezy_has_target = probe.squeezy_target.is_some();
        let squeezy_matches = probe
            .squeezy_target
            .as_ref()
            .and_then(|id| graph.symbols.get(id))
            .map(|symbol| {
                ts_locs
                    .iter()
                    .any(|loc| location_matches_symbol(root, graph, loc, symbol))
            })
            .unwrap_or(false);

        match (ts_locs.is_empty(), squeezy_has_target, squeezy_matches) {
            (false, true, true) => report.true_positive += 1,
            (false, false, _) => {
                report.false_negative += 1;
                push_example(
                    &mut report.examples,
                    format!(
                        "FN definition {}: TS -> {}, Squeezy unresolved",
                        probe.label,
                        render_locations(&ts_locs)
                    ),
                );
            }
            (false, true, false) => {
                report.false_positive += 1;
                report.false_negative += 1;
                report.wrong_target += 1;
                push_example(
                    &mut report.examples,
                    format!(
                        "Wrong definition {}: TS -> {}, Squeezy -> {}",
                        probe.label,
                        render_locations(&ts_locs),
                        probe
                            .squeezy_target
                            .as_ref()
                            .map(|id| id.0.as_str())
                            .unwrap_or("<none>")
                    ),
                );
            }
            (true, true, false) => {
                report.false_positive += 1;
                report.squeezy_only += 1;
                push_example(
                    &mut report.examples,
                    format!(
                        "Squeezy-only definition {}: TS unresolved, Squeezy -> {}",
                        probe.label,
                        probe
                            .squeezy_target
                            .as_ref()
                            .map(|id| id.0.as_str())
                            .unwrap_or("<none>")
                    ),
                );
            }
            (true, false, _) => report.unresolved_agreement += 1,
            (true, true, true) => unreachable!("matched target requires a TS location"),
        }
    }
    report.precision = ratio(
        report.true_positive,
        report.true_positive + report.false_positive,
    );
    report.recall = ratio(
        report.true_positive,
        report.true_positive + report.false_negative,
    );
    report
}

pub(crate) fn score_ts_reference_results(
    graph: &SemanticGraph,
    probes: &[TsRefProbe],
    available: usize,
    raw_results: &[Value],
) -> ReferenceAccuracyReport {
    let mut report = ReferenceAccuracyReport {
        available_symbols: available,
        symbols_sampled: probes.len(),
        ..ReferenceAccuracyReport::default()
    };
    for (probe, result) in probes.iter().zip(raw_results.iter()) {
        let ts_refs: BTreeSet<LocationKey> = result["ts_refs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| {
                let file = v["file"].as_str()?.to_string();
                let line = v["line"].as_u64()? as u32;
                let character = v["character"].as_u64()? as u32;
                Some(LocationKey {
                    file,
                    line,
                    character,
                })
            })
            .collect();

        let squeezy: BTreeSet<LocationKey> = graph
            .references_to_symbol(&probe.symbol_id)
            .into_iter()
            .filter_map(|hit| location_key_for_reference_hit(graph, &hit, &probe.name))
            .collect();

        let tp = squeezy.intersection(&ts_refs).count();
        let fp: Vec<_> = squeezy.difference(&ts_refs).cloned().collect();
        let fn_: Vec<_> = ts_refs.difference(&squeezy).cloned().collect();
        report.true_positive += tp;
        report.false_positive += fp.len();
        report.false_negative += fn_.len();

        if !fp.is_empty() {
            push_example(
                &mut report.false_positive_examples,
                format!(
                    "{} FP refs: {}",
                    probe.label,
                    fp.iter()
                        .take(5)
                        .map(LocationKey::render)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
        }
        if !fn_.is_empty() {
            push_example(
                &mut report.false_negative_examples,
                format!(
                    "{} FN refs: {}",
                    probe.label,
                    fn_.iter()
                        .take(5)
                        .map(LocationKey::render)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
        }
    }
    report.precision = ratio(
        report.true_positive,
        report.true_positive + report.false_positive,
    );
    report.recall = ratio(
        report.true_positive,
        report.true_positive + report.false_negative,
    );
    report
}

pub(crate) fn collect_js_ts_navigation_accuracy(
    root: &Path,
    graph: &SemanticGraph,
    probe_limit: usize,
) -> NavigationAccuracyReport {
    if probe_limit == 0 {
        return NavigationAccuracyReport {
            rust_analyzer_lsp_ms: None,
            rust_analyzer_lsp_status: "disabled by --ra-lsp-probes 0".to_string(),
            requested_probe_limit: 0,
            definitions: DefinitionAccuracyReport::default(),
            references: ReferenceAccuracyReport::default(),
            limitations: js_ts_nav_limitations(),
        };
    }

    let (def_available, def_probes) = match build_ts_definition_probes(graph, probe_limit) {
        Ok(p) => p,
        Err(e) => {
            return nav_report_error(
                &format!("definition probe build: {e}"),
                probe_limit,
                js_ts_nav_limitations(),
            );
        }
    };
    let (ref_available, ref_probes) = match build_ts_reference_probes(graph, probe_limit) {
        Ok(p) => p,
        Err(e) => {
            return nav_report_error(
                &format!("reference probe build: {e}"),
                probe_limit,
                js_ts_nav_limitations(),
            );
        }
    };

    if def_probes.is_empty() && ref_probes.is_empty() {
        return NavigationAccuracyReport {
            rust_analyzer_lsp_ms: None,
            rust_analyzer_lsp_status: "no JS/TS call edges or symbols found for navigation probes"
                .to_string(),
            requested_probe_limit: probe_limit,
            definitions: DefinitionAccuracyReport {
                available_probes: def_available,
                ..DefinitionAccuracyReport::default()
            },
            references: ReferenceAccuracyReport {
                available_symbols: ref_available,
                ..ReferenceAccuracyReport::default()
            },
            limitations: js_ts_nav_limitations(),
        };
    }

    let started = Instant::now();
    let oracle_result = match run_ts_navigation_node(root, &def_probes, &ref_probes) {
        Ok(r) => r,
        Err(e) => {
            return nav_report_error(
                &format!("TS Language Service oracle: {e}"),
                probe_limit,
                js_ts_nav_limitations(),
            );
        }
    };
    let elapsed = started.elapsed().as_millis();

    let raw_defs = oracle_result["def_results"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let raw_refs = oracle_result["ref_results"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    let definitions =
        score_ts_definition_results(root, graph, &def_probes, def_available, &raw_defs);
    let references = score_ts_reference_results(graph, &ref_probes, ref_available, &raw_refs);

    NavigationAccuracyReport {
        rust_analyzer_lsp_ms: Some(elapsed),
        rust_analyzer_lsp_status:
            "TypeScript Language Service definition/reference probes succeeded".to_string(),
        requested_probe_limit: probe_limit,
        definitions,
        references,
        limitations: js_ts_nav_limitations(),
    }
}

pub(crate) fn nav_report_error(
    msg: &str,
    limit: usize,
    limitations: Vec<String>,
) -> NavigationAccuracyReport {
    NavigationAccuracyReport {
        rust_analyzer_lsp_ms: None,
        rust_analyzer_lsp_status: msg.to_string(),
        requested_probe_limit: limit,
        definitions: DefinitionAccuracyReport::default(),
        references: ReferenceAccuracyReport::default(),
        limitations,
    }
}

pub(crate) fn js_ts_nav_limitations() -> Vec<String> {
    vec![
        "Definition probes compare Squeezy resolved JS/TS call edge targets with TypeScript Language Service getDefinitionAtPosition for sampled call sites.".to_string(),
        "Reference probes compare Squeezy references_to_symbol results with TypeScript Language Service findReferences for sampled declarations; the declaration position itself is excluded from the reference set.".to_string(),
        "The TypeScript Language Service is not type-checking at full depth (skipLibCheck, noEmit); probes are accurate for within-workspace calls. External library definitions show as FN for Squeezy.".to_string(),
        "Byte offsets are used as character offsets; for ASCII-dominant TypeScript source this is exact. Multi-byte unicode in the same line as a call site can shift the probe by a few characters.".to_string(),
        "Samples are deterministic and capped by --ra-lsp-probes; increase for deeper local audits.".to_string(),
    ]
}
