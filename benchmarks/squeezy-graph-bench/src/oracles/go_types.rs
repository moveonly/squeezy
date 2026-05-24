use std::{collections::BTreeSet, fs, path::Path, process::Command, time::Instant};

use squeezy_core::{Result, SqueezyError};
use squeezy_graph::SemanticGraph;

use crate::{
    accuracy::{compare_symbol_sets, increment_symbol},
    cli::BenchmarkLanguage,
    oracles::common_scan::{
        GoAstOracleOutput, GoAstSymbolScan, collect_squeezy_symbol_scan_excluding_files,
    },
    oracles::rust_analyzer::normalize_symbol_name,
    report::{
        AccuracyReport, DefinitionAccuracyReport, GoOracleReport, HeuristicIterationReport,
        NavigationAccuracyReport, ReferenceAccuracyReport, SymbolKey, SymbolScan,
    },
    util::temp_dir,
};

pub(crate) fn collect_go_oracle_accuracy(
    root: &Path,
    graph: &SemanticGraph,
) -> Result<GoOracleReport> {
    let started = Instant::now();
    let oracle = collect_go_ast_symbol_scan(root)?;
    let oracle_ms = started.elapsed().as_millis();
    let unparseable_files = oracle
        .unparseable_files
        .into_iter()
        .collect::<BTreeSet<_>>();
    let squeezy_symbols = collect_squeezy_symbol_scan_excluding_files(graph, &unparseable_files);
    let symbols = compare_symbol_sets(&squeezy_symbols, &oracle.symbols);
    let oracle_unparseable_examples = unparseable_files
        .iter()
        .take(10)
        .cloned()
        .collect::<Vec<_>>();
    let oracle_unparseable_files = unparseable_files.len();

    Ok(GoOracleReport {
        oracle_ms,
        status: if oracle_unparseable_files == 0 {
            "Go AST oracle succeeded".to_string()
        } else {
            format!(
                "Go AST oracle succeeded with {oracle_unparseable_files} unparseable files excluded from symbol FP accounting"
            )
        },
        oracle_unparseable_files,
        oracle_unparseable_examples,
        symbols,
        limitations: vec![
            "The Go oracle uses the Go parser/AST for declaration discovery and does not execute package code.".to_string(),
            "Symbol comparison is file/name/kind based; receiver dispatch, interface satisfaction, build tags, generated files, and external modules remain heuristic or excluded.".to_string(),
            "Heuristic changes should be accepted by FP/FN deltas on smoke plus external corpora, with rejected broad matches documented in the report.".to_string(),
        ],
    })
}

pub(crate) fn heuristic_iteration_reports(
    language: BenchmarkLanguage,
    go_oracle: &Option<GoOracleReport>,
) -> Vec<HeuristicIterationReport> {
    if language != BenchmarkLanguage::Go {
        return Vec::new();
    }
    if go_oracle.is_none() {
        return Vec::new();
    }
    vec![
        HeuristicIterationReport {
            name: "baseline-tree-sitter".to_string(),
            status: "accepted".to_string(),
            notes: vec![
                "Package/import/declaration extraction is the baseline for Go heuristic comparisons.".to_string(),
            ],
        },
        HeuristicIterationReport {
            name: "top-level-declaration-scope".to_string(),
            status: "accepted".to_string(),
            notes: vec![
                "Function-local var/const/type declarations, blank identifiers, and declarations inside top-level function literals are excluded from top-level symbol accuracy.".to_string(),
            ],
        },
        HeuristicIterationReport {
            name: "go-alias-and-declaration-lists".to_string(),
            status: "accepted".to_string(),
            notes: vec![
                "Grouped var/const specs and tree-sitter-go type_alias nodes are expanded so multi-name declarations and aliases count as symbols.".to_string(),
            ],
        },
        HeuristicIterationReport {
            name: "go-test-method-normalization".to_string(),
            status: "accepted".to_string(),
            notes: vec![
                "Suite-style _test.go methods with Test/Benchmark/Fuzz names are normalized to test functions for oracle comparison.".to_string(),
            ],
        },
        HeuristicIterationReport {
            name: "go-external-package-examples".to_string(),
            status: "targeted-next".to_string(),
            notes: vec![
                "Remaining etcd FNs are concentrated in external-package example test files; keep them visible instead of broad lexical matching.".to_string(),
            ],
        },
        HeuristicIterationReport {
            name: "go-lazy-reference-materialization".to_string(),
            status: "targeted-next".to_string(),
            notes: vec![
                "Prometheus and etcd are slower than the declaration-only Go oracle because cold build materializes references, body hits, calls, and edges eagerly.".to_string(),
            ],
        },
        HeuristicIterationReport {
            name: "broad-lexical-reference-binding".to_string(),
            status: "rejected-default".to_string(),
            notes: vec![
                "Broad same-name binding is not enabled by default; Go navigation favors exact package/import/receiver evidence before recall-only expansion.".to_string(),
            ],
        },
    ]
}

pub(crate) fn go_oracle_to_accuracy(report: &GoOracleReport) -> AccuracyReport {
    AccuracyReport {
        rust_analyzer_symbols_ms: Some(report.oracle_ms),
        rust_analyzer_symbol_status: report.status.clone(),
        symbols: report.symbols.clone(),
        navigation: NavigationAccuracyReport {
            rust_analyzer_lsp_ms: None,
            rust_analyzer_lsp_status: "Go LSP navigation oracle not used".to_string(),
            requested_probe_limit: 0,
            definitions: DefinitionAccuracyReport::default(),
            references: ReferenceAccuracyReport::default(),
            limitations: vec![
                "Go accuracy currently compares symbol declarations against the Go parser/type oracle; LSP-style go-to-definition probes are not exercised yet.".to_string(),
            ],
        },
        limitations: report.limitations.clone(),
    }
}

pub(crate) fn collect_go_ast_symbol_scan(root: &Path) -> Result<GoAstSymbolScan> {
    // The oracle Go program is written to a dedicated sub-directory of the
    // system temp directory and the whole sub-directory is removed when this
    // function returns. Tracking the sub-directory explicitly (instead of
    // relying on `script_path.parent()`) keeps the cleanup scoped even if a
    // future change ever co-locates additional files with the script.
    let oracle_dir = temp_dir("squeezy-go-oracle")?;
    let script_path = oracle_dir.join("oracle.go");
    let result = run_go_ast_oracle(&script_path, root);
    let _ = fs::remove_dir_all(&oracle_dir);
    result
}

pub(crate) fn time_go_ast_oracle(fixture: &Path) -> Result<u128> {
    let started = Instant::now();
    let _ = collect_go_ast_symbol_scan(fixture)?;
    Ok(started.elapsed().as_millis())
}

pub(crate) fn run_go_ast_oracle(script_path: &Path, root: &Path) -> Result<GoAstSymbolScan> {
    fs::write(script_path, GO_AST_ORACLE)?;
    let output = Command::new("go")
        .arg("run")
        .arg(script_path)
        .arg(root)
        .output()
        .map_err(|err| SqueezyError::Graph(format!("failed to run Go AST oracle: {err}")))?;
    if !output.status.success() {
        return Err(SqueezyError::Graph(format!(
            "Go AST oracle failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let output: GoAstOracleOutput = serde_json::from_slice(&output.stdout)
        .map_err(|err| SqueezyError::Graph(format!("invalid Go AST oracle JSON: {err}")))?;
    let mut scan = SymbolScan::default();
    for [file, kind, name] in output.rows {
        scan.raw_total += 1;
        increment_symbol(
            &mut scan.counts,
            SymbolKey {
                file,
                kind,
                name: normalize_symbol_name(&name),
            },
        );
    }
    Ok(GoAstSymbolScan {
        symbols: scan,
        unparseable_files: output.unparseable_files,
    })
}

const GO_AST_ORACLE: &str = r#"
package main

import (
	"encoding/json"
	"go/ast"
	"go/parser"
	"go/token"
	"os"
	"path/filepath"
	"sort"
	"strings"
)

type Output struct {
	Rows             [][3]string `json:"rows"`
	UnparseableFiles []string    `json:"unparseable_files"`
}

func main() {
	root, _ := filepath.Abs(os.Args[1])
	out := Output{}
	filepath.WalkDir(root, func(path string, entry os.DirEntry, err error) error {
		if err != nil || entry.IsDir() {
			if entry != nil && entry.IsDir() && (entry.Name() == "vendor" || strings.HasPrefix(entry.Name(), ".")) {
				return filepath.SkipDir
			}
			return nil
		}
		if !strings.HasSuffix(path, ".go") {
			return nil
		}
		rel, _ := filepath.Rel(root, path)
		rel = filepath.ToSlash(rel)
		fset := token.NewFileSet()
		file, err := parser.ParseFile(fset, path, nil, parser.ParseComments)
		if err != nil {
			out.UnparseableFiles = append(out.UnparseableFiles, rel)
			return nil
		}
		for _, decl := range file.Decls {
			switch decl := decl.(type) {
			case *ast.FuncDecl:
				kind := "Function"
				if decl.Recv != nil {
					kind = "Method"
				}
				if strings.HasSuffix(rel, "_test.go") && (strings.HasPrefix(decl.Name.Name, "Test") || strings.HasPrefix(decl.Name.Name, "Benchmark") || strings.HasPrefix(decl.Name.Name, "Fuzz")) {
					kind = "Function"
				}
				out.Rows = append(out.Rows, [3]string{rel, kind, decl.Name.Name})
			case *ast.GenDecl:
				for _, spec := range decl.Specs {
					switch spec := spec.(type) {
					case *ast.TypeSpec:
						kind := "TypeAlias"
						switch spec.Type.(type) {
						case *ast.StructType:
							kind = "Struct"
						case *ast.InterfaceType:
							kind = "Interface"
						}
						out.Rows = append(out.Rows, [3]string{rel, kind, spec.Name.Name})
					case *ast.ValueSpec:
						kind := "Static"
						if decl.Tok == token.CONST {
							kind = "Const"
						}
						for _, name := range spec.Names {
							if name.Name != "_" {
								out.Rows = append(out.Rows, [3]string{rel, kind, name.Name})
							}
						}
					}
				}
			}
		}
		return nil
	})
	sort.Slice(out.Rows, func(i, j int) bool {
		return strings.Join(out.Rows[i][:], "\x00") < strings.Join(out.Rows[j][:], "\x00")
	})
	_ = json.NewEncoder(os.Stdout).Encode(out)
}
"#;
