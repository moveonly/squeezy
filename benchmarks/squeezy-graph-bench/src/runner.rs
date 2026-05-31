use std::{fs, path::PathBuf, time::Instant};

use serde::Serialize;

use squeezy_core::{LanguageKind, Result, SqueezyError};

use crate::{
    accuracy::{collect_accuracy, empty_accuracy},
    cli::{Args, BenchmarkCommand, BenchmarkLanguage},
    corpus::{CorpusManifest, ensure_repo},
    execution::{build_graph, fallback_quality_report, run_query},
    gates::enforce_gates,
    harness::toolchain::{time_cargo_check, time_clang_syntax, time_dotnet_build},
    mixed::{run_mixed_workload, run_refresh_probe},
    oracles::{
        collect_c_family_accuracy, collect_csharp_oracle_accuracy, collect_dart_oracle_accuracy,
        collect_go_oracle_accuracy, collect_java_oracle_accuracy, collect_js_ts_accuracy,
        collect_js_ts_oracle_accuracy, collect_kotlin_oracle_accuracy,
        collect_php_oracle_accuracy, collect_python_oracle_accuracy,
        collect_ruby_oracle_accuracy, collect_scala_oracle_accuracy, heuristic_iteration_reports,
        time_dart_oracle_optional, time_go_ast_oracle, time_java_oracle_optional,
        time_js_ts_oracle, time_kotlin_oracle_optional, time_php_oracle_optional,
        time_python_ast_oracle, time_scala_oracle_optional,
    },
    report::*,
    summary::{print_summary, write_report},
};

pub fn main() -> Result<()> {
    match BenchmarkCommand::parse()? {
        BenchmarkCommand::Single(args) => run_single(args),
        BenchmarkCommand::Corpus(args) => run_corpus(args),
    }
}

fn run_single(args: Args) -> Result<()> {
    let report = run_benchmark(&args, None)?;
    write_report(&args.report, &report)?;
    print_summary(&report);
    enforce_gates(&report, args.no_speed_gate)
}

#[derive(Debug, Serialize)]
struct CorpusCaseOutcome {
    name: String,
    family: String,
    tier: String,
    report: String,
    status: String,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct CorpusSummary {
    corpus: String,
    family: String,
    tier: String,
    cases: Vec<CorpusCaseOutcome>,
    failures: usize,
}

fn run_corpus(args: crate::cli::CorpusArgs) -> Result<()> {
    let manifest = CorpusManifest::load(&args.corpus)?;
    let cases = manifest
        .cases
        .iter()
        .filter(|case| case.matches(&args.family, &args.tier))
        .cloned()
        .collect::<Vec<_>>();
    if cases.is_empty() {
        return Err(SqueezyError::Graph(format!(
            "corpus {} had no cases for family={} tier={}",
            args.corpus.display(),
            args.family,
            args.tier
        )));
    }

    let mut outcomes = Vec::with_capacity(cases.len());
    let mut failure_messages = Vec::new();
    for case in cases {
        let outcome_name = case.name.clone();
        let outcome_family = case.family.clone();
        let outcome_tier = case.tier.clone();
        let report_path = args.report_dir.join(&case.report);
        let result = run_corpus_case(&args, &case);
        match result {
            Ok(()) => outcomes.push(CorpusCaseOutcome {
                name: outcome_name,
                family: outcome_family,
                tier: outcome_tier,
                report: report_path.display().to_string(),
                status: "passed".to_string(),
                error: None,
            }),
            Err(err) => {
                let message = err.to_string();
                eprintln!("corpus case {outcome_name} failed: {message}");
                failure_messages.push(format!("{outcome_name}: {message}"));
                outcomes.push(CorpusCaseOutcome {
                    name: outcome_name,
                    family: outcome_family,
                    tier: outcome_tier,
                    report: report_path.display().to_string(),
                    status: "failed".to_string(),
                    error: Some(message),
                });
            }
        }
    }

    let summary = CorpusSummary {
        corpus: args.corpus.display().to_string(),
        family: args.family.clone(),
        tier: args.tier.clone(),
        failures: failure_messages.len(),
        cases: outcomes,
    };
    fs::create_dir_all(&args.report_dir)?;
    // Write the corpus summary one directory above the per-report tree so it
    // is not picked up by the summarize.py glob (target/.../**/*.json).
    let summary_dir = args
        .report_dir
        .parent()
        .unwrap_or(&args.report_dir)
        .to_path_buf();
    fs::create_dir_all(&summary_dir)?;
    let summary_path = summary_dir.join("corpus-summary.json");
    let summary_text = serde_json::to_string_pretty(&summary)
        .map_err(|err| SqueezyError::Graph(format!("failed to serialize corpus summary: {err}")))?;
    fs::write(&summary_path, summary_text)?;
    println!(
        "corpus summary: {} cases, {} failures -> {}",
        summary.cases.len(),
        summary.failures,
        summary_path.display()
    );

    if !failure_messages.is_empty() {
        return Err(SqueezyError::Graph(format!(
            "{} corpus case(s) failed: {}",
            failure_messages.len(),
            failure_messages.join("; ")
        )));
    }
    Ok(())
}

fn run_corpus_case(args: &crate::cli::CorpusArgs, case: &crate::corpus::CorpusCase) -> Result<()> {
    ensure_repo(case)?;
    let bench_args = Args {
        language: BenchmarkLanguage::parse(&case.language)?,
        fixture: PathBuf::from(&case.fixture),
        spec: PathBuf::from(&case.spec),
        report: args.report_dir.join(&case.report),
        mixed_repo: case.mixed_repo.as_ref().map(PathBuf::from),
        mixed_iterations: case.mixed_iterations.unwrap_or(0),
        ra_lsp_probes: case.ra_lsp_probes.unwrap_or(25),
        oracle_files: case.oracle_files.unwrap_or(250),
        no_speed_gate: case.no_speed_gate,
    };
    let corpus_case = case.report_case();
    let report = run_benchmark(&bench_args, Some(corpus_case))?;
    write_report(&bench_args.report, &report)?;
    print_summary(&report);
    enforce_gates(&report, bench_args.no_speed_gate)
}

fn run_benchmark(args: &Args, corpus_case: Option<CorpusCaseReport>) -> Result<BenchmarkReport> {
    let spec_text = fs::read_to_string(&args.spec)?;
    let spec: QuerySpecFile = serde_json::from_str(&spec_text)
        .map_err(|err| SqueezyError::Graph(format!("invalid benchmark spec: {err}")))?;

    let (validation_ms, validation_status) = match args.language {
        BenchmarkLanguage::C => (
            time_clang_syntax(&args.fixture, "clang", LanguageKind::C)?,
            "clang -fsyntax-only".to_string(),
        ),
        BenchmarkLanguage::Cpp => (
            time_clang_syntax(&args.fixture, "clang++", LanguageKind::Cpp)?,
            "clang++ -fsyntax-only".to_string(),
        ),
        BenchmarkLanguage::CSharp => (
            time_dotnet_build(&args.fixture)?,
            "dotnet build".to_string(),
        ),
        BenchmarkLanguage::Java => time_java_oracle_optional(&args.fixture),
        BenchmarkLanguage::Kotlin => time_kotlin_oracle_optional(&args.fixture),
        BenchmarkLanguage::Scala => time_scala_oracle_optional(&args.fixture),
        BenchmarkLanguage::Rust => (time_cargo_check(&args.fixture)?, "cargo check".to_string()),
        BenchmarkLanguage::Python => (
            time_python_ast_oracle(&args.fixture)?,
            "CPython ast oracle".to_string(),
        ),
        BenchmarkLanguage::Go => (
            time_go_ast_oracle(&args.fixture)?,
            "Go parser/type oracle".to_string(),
        ),
        BenchmarkLanguage::Php => time_php_oracle_optional(&args.fixture),
        BenchmarkLanguage::Ruby => match crate::oracles::time_ruby_prism_oracle(&args.fixture) {
            Ok(ms) => (ms, "Ruby Prism oracle".to_string()),
            Err(err) => (0, format!("Ruby Prism oracle unavailable: {err}")),
        },
        BenchmarkLanguage::JavaScript | BenchmarkLanguage::TypeScript => {
            match time_js_ts_oracle(&args.fixture) {
                Ok(ms) => (ms, "TypeScript compiler API oracle".to_string()),
                Err(err) => (
                    0,
                    format!("TypeScript compiler API oracle unavailable: {err}"),
                ),
            }
        }
        BenchmarkLanguage::Swift => (
            0,
            "Swift validation oracle not run in first-iteration CI (SwiftPM build is expensive)".to_string(),
        ),
        BenchmarkLanguage::Dart => time_dart_oracle_optional(&args.fixture),
    };

    let build = build_graph(&args.fixture)?;
    let graph = build.graph;
    let squeezy_build_ms = build.phases.total_ms;
    let fallback_quality = fallback_quality_report(
        &build.coverage,
        build.unsupported_files,
        build.unsupported_file_samples,
        &graph,
    );
    let snapshot = build.snapshot;

    let query_started = Instant::now();
    let query_reports = spec
        .queries
        .iter()
        .map(|query| run_query(&snapshot, &graph, query, &fallback_quality))
        .collect::<Result<Vec<_>>>()?;
    let squeezy_query_ms = query_started.elapsed().as_millis();
    let squeezy_total_ms = squeezy_build_ms + squeezy_query_ms;

    let accuracy = match args.language {
        BenchmarkLanguage::Java => empty_accuracy("rust-analyzer oracle not used for Java"),
        BenchmarkLanguage::Kotlin => empty_accuracy("rust-analyzer oracle not used for Kotlin"),
        BenchmarkLanguage::Scala => empty_accuracy("rust-analyzer oracle not used for Scala"),
        BenchmarkLanguage::Rust => collect_accuracy(&args.fixture, &graph, args.ra_lsp_probes),
        BenchmarkLanguage::CSharp => empty_accuracy("rust-analyzer oracle not used for C#"),
        BenchmarkLanguage::C | BenchmarkLanguage::Cpp => {
            collect_c_family_accuracy(&args.fixture, &graph, args.language, args.oracle_files)?
        }
        BenchmarkLanguage::Python => empty_accuracy("rust-analyzer oracle not used for Python"),
        BenchmarkLanguage::Go => empty_accuracy("rust-analyzer oracle not used for Go"),
        BenchmarkLanguage::Php => empty_accuracy("rust-analyzer oracle not used for PHP"),
        BenchmarkLanguage::Ruby => empty_accuracy("Ruby LSP navigation oracle not used"),
        BenchmarkLanguage::JavaScript | BenchmarkLanguage::TypeScript => {
            collect_js_ts_accuracy(&args.fixture, &graph, args.ra_lsp_probes)
        }
        BenchmarkLanguage::Swift => empty_accuracy("rust-analyzer oracle not used for Swift"),
        BenchmarkLanguage::Dart => empty_accuracy("rust-analyzer oracle not used for Dart"),
    };
    let python_oracle = match args.language {
        BenchmarkLanguage::Python => Some(collect_python_oracle_accuracy(&args.fixture, &graph)?),
        _ => None,
    };
    let go_oracle = match args.language {
        BenchmarkLanguage::Go => Some(collect_go_oracle_accuracy(&args.fixture, &graph)?),
        _ => None,
    };
    let ruby_oracle = match args.language {
        BenchmarkLanguage::Ruby => Some(collect_ruby_oracle_accuracy(&args.fixture, &graph)?),
        _ => None,
    };
    let dart_oracle = match args.language {
        BenchmarkLanguage::Dart => Some(collect_dart_oracle_accuracy(&args.fixture, &graph)?),
        _ => None,
    };
    let js_ts_oracle = match args.language {
        BenchmarkLanguage::JavaScript | BenchmarkLanguage::TypeScript => {
            Some(collect_js_ts_oracle_accuracy(&args.fixture, &graph))
        }
        _ => None,
    };
    let csharp_oracle = match args.language {
        BenchmarkLanguage::CSharp => Some(collect_csharp_oracle_accuracy(&args.fixture, &graph)?),
        _ => None,
    };
    let java_oracle = match args.language {
        BenchmarkLanguage::Java => Some(collect_java_oracle_accuracy(
            &args.fixture,
            &graph,
            &query_reports,
        )?),
        _ => None,
    };
    let kotlin_oracle = match args.language {
        BenchmarkLanguage::Kotlin => Some(collect_kotlin_oracle_accuracy(
            &args.fixture,
            &graph,
            &query_reports,
        )?),
        _ => None,
    };
    let php_oracle = match args.language {
        BenchmarkLanguage::Php => Some(collect_php_oracle_accuracy(
            &args.fixture,
            &graph,
            &query_reports,
        )?),
        _ => None,
    };
    let scala_oracle = match args.language {
        BenchmarkLanguage::Scala => Some(collect_scala_oracle_accuracy(&args.fixture, &graph)?),
        _ => None,
    };
    let swift_oracle = match args.language {
        BenchmarkLanguage::Swift => Some(
            crate::oracles::swift_sourcekit::collect_swift_oracle_accuracy(
                &args.fixture,
                &graph,
                args.ra_lsp_probes,
            )?,
        ),
        _ => None,
    };
    let faster_than_validation =
        validation_status.starts_with("skipped") || squeezy_total_ms < validation_ms;

    let mixed_workload = if args.language.supports_mixed_workload() {
        args.mixed_repo
            .as_ref()
            .map(|repo| {
                run_mixed_workload(
                    repo,
                    args.language,
                    args.mixed_iterations,
                    args.ra_lsp_probes,
                    args.oracle_files,
                )
            })
            .transpose()?
    } else {
        None
    };

    let stats = graph.stats();
    let refresh_probe = Some(run_refresh_probe(&args.fixture, args.language)?);
    let heuristic_iterations = heuristic_iteration_reports(args.language, &go_oracle);
    let tool_metrics =
        tool_metrics_report(&query_reports, mixed_workload.as_ref(), squeezy_total_ms);
    let answer_quality = answer_quality_report(
        &query_reports,
        args.language,
        &accuracy,
        &python_oracle,
        &js_ts_oracle,
        &java_oracle,
        &kotlin_oracle,
        &scala_oracle,
        &csharp_oracle,
        &go_oracle,
        &php_oracle,
        &ruby_oracle,
        &swift_oracle,
        &dart_oracle,
    );

    Ok(BenchmarkReport {
        corpus_case,
        language: args.language.as_str().to_string(),
        fixture: args.fixture.display().to_string(),
        spec: args.spec.display().to_string(),
        validation_ms,
        validation_status,
        squeezy_build_ms,
        squeezy_query_ms,
        squeezy_total_ms,
        build_phases: build.phases,
        faster_than_validation: validation_ms == 0 || faster_than_validation,
        tool_metrics,
        answer_quality,
        fallback_quality,
        graph: GraphReport {
            files: stats.files,
            symbols: stats.symbols,
            edges: stats.edges,
            body_hits: stats.body_hits,
            references: stats.references,
            calls: stats.calls,
            body_hit_trigram_indexed: stats.body_hit_trigram_indexed,
            body_hit_trigram_terms: stats.body_hit_trigram_terms,
            reference_index_terms: stats.reference_index_terms,
        },
        accuracy,
        python_oracle,
        js_ts_oracle,
        java_oracle,
        kotlin_oracle,
        scala_oracle,
        csharp_oracle,
        go_oracle,
        php_oracle,
        ruby_oracle,
        swift_oracle,
        dart_oracle,
        refresh_probe,
        heuristic_iterations,
        queries: query_reports,
        mixed_workload,
    })
}

fn tool_metrics_report(
    query_reports: &[QueryReport],
    mixed_workload: Option<&MixedWorkloadReport>,
    wall_ms: u128,
) -> ToolMetricsReport {
    let grep_baseline_queries = query_reports
        .iter()
        .filter(|query| query.baseline.status == QueryBaselineStatus::Ran)
        .count();
    let mixed_scenarios = mixed_workload
        .map(|mixed| mixed.executed_scenarios)
        .unwrap_or_default();
    ToolMetricsReport {
        graph_queries: query_reports.len(),
        grep_baseline_queries,
        mixed_scenarios,
        deterministic_tool_calls: query_reports.len() + grep_baseline_queries + mixed_scenarios,
        wall_ms,
        estimated_usd_micros: 0,
        cost_basis: "deterministic local benchmark; no provider calls".to_string(),
    }
}

#[allow(clippy::too_many_arguments)] // mirrors the per-language oracle fan-out
fn answer_quality_report(
    query_reports: &[QueryReport],
    language: BenchmarkLanguage,
    accuracy: &AccuracyReport,
    python_oracle: &Option<PythonOracleReport>,
    js_ts_oracle: &Option<JsTsOracleReport>,
    java_oracle: &Option<JavaOracleReport>,
    kotlin_oracle: &Option<KotlinOracleReport>,
    scala_oracle: &Option<ScalaOracleReport>,
    csharp_oracle: &Option<CsharpOracleReport>,
    go_oracle: &Option<GoOracleReport>,
    php_oracle: &Option<PhpOracleReport>,
    ruby_oracle: &Option<RubyOracleReport>,
    swift_oracle: &Option<SwiftOracleReport>,
    dart_oracle: &Option<DartOracleReport>,
) -> AnswerQualityReport {
    let expected_checks = query_reports
        .iter()
        .map(|query| query.expected_contains.len())
        .sum::<usize>();
    let missing_checks = query_reports
        .iter()
        .map(|query| query.missing.len())
        .sum::<usize>();
    let extra_results = query_reports
        .iter()
        .map(|query| query.extras.len())
        .sum::<usize>();
    let documented_misses = query_reports
        .iter()
        .map(|query| query.documented_misses.len())
        .sum::<usize>();
    let (oracle_status, oracle_precision, oracle_recall) = oracle_summary(
        language,
        accuracy,
        python_oracle,
        js_ts_oracle,
        java_oracle,
        kotlin_oracle,
        scala_oracle,
        csharp_oracle,
        go_oracle,
        php_oracle,
        ruby_oracle,
        swift_oracle,
        dart_oracle,
    );

    AnswerQualityReport {
        query_count: query_reports.len(),
        expected_checks,
        satisfied_checks: expected_checks.saturating_sub(missing_checks),
        missing_checks,
        extra_results,
        documented_misses,
        passed: missing_checks == 0,
        oracle_status,
        oracle_precision,
        oracle_recall,
    }
}

#[allow(clippy::too_many_arguments)] // mirrors the per-language oracle fan-out
fn oracle_summary(
    language: BenchmarkLanguage,
    accuracy: &AccuracyReport,
    python_oracle: &Option<PythonOracleReport>,
    js_ts_oracle: &Option<JsTsOracleReport>,
    java_oracle: &Option<JavaOracleReport>,
    kotlin_oracle: &Option<KotlinOracleReport>,
    scala_oracle: &Option<ScalaOracleReport>,
    csharp_oracle: &Option<CsharpOracleReport>,
    go_oracle: &Option<GoOracleReport>,
    php_oracle: &Option<PhpOracleReport>,
    ruby_oracle: &Option<RubyOracleReport>,
    swift_oracle: &Option<SwiftOracleReport>,
    dart_oracle: &Option<DartOracleReport>,
) -> (String, Option<f64>, Option<f64>) {
    match language {
        BenchmarkLanguage::Python => python_oracle
            .as_ref()
            .map(|oracle| symbol_oracle_tuple(&oracle.status, &oracle.symbols))
            .unwrap_or_else(|| {
                symbol_oracle_tuple(&accuracy.rust_analyzer_symbol_status, &accuracy.symbols)
            }),
        BenchmarkLanguage::Go => go_oracle
            .as_ref()
            .map(|oracle| symbol_oracle_tuple(&oracle.status, &oracle.symbols))
            .unwrap_or_else(|| {
                symbol_oracle_tuple(&accuracy.rust_analyzer_symbol_status, &accuracy.symbols)
            }),
        BenchmarkLanguage::Java => java_oracle
            .as_ref()
            .map(|oracle| symbol_oracle_tuple(&oracle.status, &oracle.symbols))
            .unwrap_or_else(|| {
                symbol_oracle_tuple(&accuracy.rust_analyzer_symbol_status, &accuracy.symbols)
            }),
        BenchmarkLanguage::Kotlin => kotlin_oracle
            .as_ref()
            .map(|oracle| symbol_oracle_tuple(&oracle.status, &oracle.symbols))
            .unwrap_or_else(|| {
                symbol_oracle_tuple(&accuracy.rust_analyzer_symbol_status, &accuracy.symbols)
            }),
        BenchmarkLanguage::Scala => scala_oracle
            .as_ref()
            .map(|oracle| symbol_oracle_tuple(&oracle.status, &oracle.symbols))
            .unwrap_or_else(|| {
                symbol_oracle_tuple(&accuracy.rust_analyzer_symbol_status, &accuracy.symbols)
            }),
        BenchmarkLanguage::CSharp => csharp_oracle
            .as_ref()
            .map(|oracle| symbol_oracle_tuple(&oracle.status, &oracle.symbols))
            .unwrap_or_else(|| {
                symbol_oracle_tuple(&accuracy.rust_analyzer_symbol_status, &accuracy.symbols)
            }),
        BenchmarkLanguage::Php => php_oracle
            .as_ref()
            .map(|oracle| symbol_oracle_tuple(&oracle.status, &oracle.symbols))
            .unwrap_or_else(|| {
                symbol_oracle_tuple(&accuracy.rust_analyzer_symbol_status, &accuracy.symbols)
            }),
        BenchmarkLanguage::JavaScript | BenchmarkLanguage::TypeScript => js_ts_oracle
            .as_ref()
            .map(|oracle| symbol_oracle_tuple(&oracle.status, &oracle.symbols))
            .unwrap_or_else(|| {
                symbol_oracle_tuple(&accuracy.rust_analyzer_symbol_status, &accuracy.symbols)
            }),
        BenchmarkLanguage::Ruby => ruby_oracle
            .as_ref()
            .map(|oracle| symbol_oracle_tuple(&oracle.status, &oracle.symbols))
            .unwrap_or_else(|| {
                symbol_oracle_tuple(&accuracy.rust_analyzer_symbol_status, &accuracy.symbols)
            }),
        BenchmarkLanguage::Swift => swift_oracle
            .as_ref()
            .map(|oracle| symbol_oracle_tuple(&oracle.status, &oracle.symbols))
            .unwrap_or_else(|| {
                symbol_oracle_tuple(&accuracy.rust_analyzer_symbol_status, &accuracy.symbols)
            }),
        BenchmarkLanguage::Rust | BenchmarkLanguage::C | BenchmarkLanguage::Cpp => {
            symbol_oracle_tuple(&accuracy.rust_analyzer_symbol_status, &accuracy.symbols)
        }
        BenchmarkLanguage::Dart => dart_oracle
            .as_ref()
            .map(symbol_oracle_tuple_for_dart)
            .unwrap_or_else(|| {
                (
                    "Dart analyzer oracle unavailable".to_string(),
                    None,
                    None,
                )
            }),
    }
}

fn symbol_oracle_tuple_for_dart(
    oracle: &DartOracleReport,
) -> (String, Option<f64>, Option<f64>) {
    if oracle.mode == "scan-only" {
        return (oracle.status.clone(), None, None);
    }
    symbol_oracle_tuple(&oracle.status, &oracle.symbols)
}

fn symbol_oracle_tuple(
    status: &str,
    symbols: &AccuracySetReport,
) -> (String, Option<f64>, Option<f64>) {
    if status.contains("unavailable") || status.starts_with("skipped") {
        return (status.to_string(), None, None);
    }
    (
        status.to_string(),
        Some(symbols.precision),
        Some(symbols.recall),
    )
}
