//! Ruby Prism subprocess oracle (spec §9).
//!
//! Invokes `ruby` with an inline `-e` script that walks the fixture, parses
//! each `.rb` file with Prism, and emits `{ rows: [[file, kind, name], ...],
//! unparseable_files: [...] }` JSON on stdout — identical shape to the
//! Python AST oracle.
//!
//! When `ruby` is not on `PATH` (CI may fail to install it), the oracle
//! degrades to a `mode = "scan-only"` self-compare that re-emits Squeezy's
//! own scan as the oracle baseline. The gate logic in `gates.rs` interprets
//! the resulting precision/recall accordingly.
use std::{collections::BTreeSet, path::Path, process::Command, time::Instant};

use serde::Deserialize;
use squeezy_core::Result;
use squeezy_graph::SemanticGraph;

use crate::{
    accuracy::{compare_symbol_sets, increment_symbol},
    oracles::common_scan::{
        collect_squeezy_ruby_symbol_scan_excluding_files, default_oracle_exclusions,
    },
    oracles::rust_analyzer::normalize_symbol_name,
    report::{RubyOracleReport, SymbolKey, SymbolScan},
    util::increment,
};

#[derive(Debug, Deserialize)]
struct RubyOracleOutput {
    #[serde(default)]
    rows: Vec<[String; 3]>,
    #[serde(default)]
    unparseable_files: Vec<String>,
}

pub(crate) fn collect_ruby_oracle_accuracy(
    root: &Path,
    graph: &SemanticGraph,
) -> Result<RubyOracleReport> {
    let started = Instant::now();
    match collect_ruby_prism_symbol_scan(root) {
        Ok(oracle) => {
            let oracle_ms = started.elapsed().as_millis();
            let unparseable_files = oracle
                .unparseable_files
                .into_iter()
                .collect::<BTreeSet<_>>();
            let squeezy_symbols =
                collect_squeezy_ruby_symbol_scan_excluding_files(graph, &unparseable_files);
            let symbols = compare_symbol_sets(&squeezy_symbols, &oracle.symbols);
            let oracle_unparseable_examples = unparseable_files
                .iter()
                .take(10)
                .cloned()
                .collect::<Vec<_>>();
            let oracle_unparseable_files = unparseable_files.len();
            Ok(RubyOracleReport {
                oracle_ms,
                status: if oracle_unparseable_files == 0 {
                    "Ruby Prism oracle succeeded".to_string()
                } else {
                    format!(
                        "Ruby Prism oracle succeeded with {oracle_unparseable_files} unparseable files excluded from symbol FP accounting"
                    )
                },
                mode: "prism".to_string(),
                oracle_unparseable_files,
                oracle_unparseable_examples,
                symbols,
                limitations: ruby_oracle_limitations(),
            })
        }
        Err(err) => {
            // Spec §9 "Scan-only fallback": degrade to a self-compare so the
            // bench can still emit a report without the Ruby toolchain.
            let oracle_ms = started.elapsed().as_millis();
            let scan = collect_squeezy_ruby_symbol_scan_excluding_files(graph, &BTreeSet::new());
            let symbols = compare_symbol_sets(&scan, &scan);
            Ok(RubyOracleReport {
                oracle_ms,
                status: format!(
                    "Ruby Prism oracle unavailable; degraded to scan-only ({err})"
                ),
                mode: "scan-only".to_string(),
                oracle_unparseable_files: 0,
                oracle_unparseable_examples: Vec::new(),
                symbols,
                limitations: ruby_oracle_limitations(),
            })
        }
    }
}

pub(crate) fn time_ruby_prism_oracle(fixture: &Path) -> Result<u128> {
    let started = Instant::now();
    let _ = collect_ruby_prism_symbol_scan(fixture)?;
    Ok(started.elapsed().as_millis())
}

fn ruby_oracle_limitations() -> Vec<String> {
    vec![
        "The Ruby oracle uses Prism for declarations and does not resolve dispatch; `method_missing`, `define_method`, and `eval`-built methods are systematic recall gaps that the oracle also excludes.".to_string(),
        "Symbol comparison is file/name/kind based; runtime metaprogramming (`include` of dynamically-built modules, anonymous classes) is not modelled.".to_string(),
        "Files Prism cannot parse are reported as oracle_unparseable and excluded from Squeezy false-positive accounting; tree-sitter recovery remains useful for incremental editing workflows.".to_string(),
    ]
}

#[derive(Debug)]
pub(crate) struct RubyPrismSymbolScan {
    pub(crate) symbols: SymbolScan,
    pub(crate) unparseable_files: Vec<String>,
}

pub(crate) fn collect_ruby_prism_symbol_scan(root: &Path) -> Result<RubyPrismSymbolScan> {
    let exclusions = default_oracle_exclusions(root)?;
    let output = Command::new("ruby")
        .arg("-e")
        .arg(RUBY_PRISM_ORACLE)
        .arg(root)
        .output()
        .map_err(|err| {
            squeezy_core::SqueezyError::Graph(format!(
                "failed to spawn Ruby Prism oracle: {err}"
            ))
        })?;
    if !output.status.success() {
        return Err(squeezy_core::SqueezyError::Graph(format!(
            "Ruby Prism oracle failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let parsed: RubyOracleOutput = serde_json::from_slice(&output.stdout).map_err(|err| {
        squeezy_core::SqueezyError::Graph(format!("invalid Ruby Prism oracle JSON: {err}"))
    })?;
    let mut scan = SymbolScan::default();
    for [file, kind, name] in parsed.rows {
        scan.raw_total += 1;
        if exclusions.excludes(&file) {
            increment(&mut scan.excluded_by_kind, "ExcludedPath");
            continue;
        }
        increment_symbol(
            &mut scan.counts,
            SymbolKey {
                file,
                kind,
                name: normalize_symbol_name(&name),
            },
        );
    }
    let unparseable_files = parsed
        .unparseable_files
        .into_iter()
        .filter(|file| !exclusions.excludes(file))
        .collect();
    Ok(RubyPrismSymbolScan {
        symbols: scan,
        unparseable_files,
    })
}

// Inline Ruby program (spec §9 sketch).
const RUBY_PRISM_ORACLE: &str = r#"
require "prism"
require "json"
require "find"
root = ARGV[0]
rows = []
unparseable = []
Find.find(root) do |p|
  if File.directory?(p) && (File.basename(p).start_with?(".") || %w[vendor node_modules tmp generated].include?(File.basename(p)))
    Find.prune
  end
  next unless p.end_with?(".rb")
  rel = p.sub(/^#{Regexp.escape(root)}\/?/, "")
  begin
    res = Prism.parse_file(p)
  rescue StandardError
    unparseable << rel
    next
  end
  if res.failure?
    unparseable << rel
    next
  end
  walk = ->(node, in_class) {
    case node
    when Prism::ClassNode
      rows << [rel, "Class", node.constant_path.slice]
      node.compact_child_nodes.each { |c| walk.call(c, true) }
    when Prism::ModuleNode
      rows << [rel, "Module", node.constant_path.slice]
      node.compact_child_nodes.each { |c| walk.call(c, true) }
    when Prism::DefNode
      rows << [rel, in_class ? "Method" : "Function", node.name.to_s]
      node.compact_child_nodes.each { |c| walk.call(c, in_class) }
    else
      node.compact_child_nodes.each { |c| walk.call(c, in_class) }
    end
  }
  walk.call(res.value, false)
end
puts JSON.generate({rows: rows, unparseable_files: unparseable})
"#;

#[cfg(test)]
#[path = "ruby_oracle_tests.rs"]
mod tests;
