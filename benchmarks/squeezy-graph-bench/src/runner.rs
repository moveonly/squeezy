use std::{fs, time::Instant};

use squeezy_core::{LanguageKind, Result, SqueezyError};

use crate::{
    accuracy::{collect_accuracy, empty_accuracy},
    cli::{Args, BenchmarkLanguage},
    execution::{build_graph, run_query},
    gates::enforce_gates,
    harness::toolchain::{time_cargo_check, time_clang_syntax, time_dotnet_build},
    mixed::{run_mixed_workload, run_refresh_probe},
    oracles::{
        collect_c_family_accuracy, collect_csharp_oracle_accuracy, collect_go_oracle_accuracy,
        collect_java_oracle_accuracy, collect_js_ts_accuracy, collect_js_ts_oracle_accuracy,
        collect_python_oracle_accuracy, heuristic_iteration_reports, time_go_ast_oracle,
        time_java_oracle_optional, time_js_ts_oracle, time_python_ast_oracle,
    },
    report::{BenchmarkReport, GraphReport, QuerySpecFile},
    summary::{print_summary, write_report},
};

pub fn main() -> Result<()> {
    let args = Args::parse()?;
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
        BenchmarkLanguage::Rust => (time_cargo_check(&args.fixture)?, "cargo check".to_string()),
        BenchmarkLanguage::Python => (
            time_python_ast_oracle(&args.fixture)?,
            "CPython ast oracle".to_string(),
        ),
        BenchmarkLanguage::Go => (
            time_go_ast_oracle(&args.fixture)?,
            "Go parser/type oracle".to_string(),
        ),
        BenchmarkLanguage::JavaScript | BenchmarkLanguage::TypeScript => {
            match time_js_ts_oracle(&args.fixture) {
                Ok(ms) => (ms, "TypeScript compiler API oracle".to_string()),
                Err(err) => (
                    0,
                    format!("TypeScript compiler API oracle unavailable: {err}"),
                ),
            }
        }
    };

    let build = build_graph(&args.fixture)?;
    let graph = build.graph;
    let squeezy_build_ms = build.phases.total_ms;

    let query_started = Instant::now();
    let query_reports = spec
        .queries
        .iter()
        .map(|query| run_query(&graph, query))
        .collect::<Result<Vec<_>>>()?;
    let squeezy_query_ms = query_started.elapsed().as_millis();
    let squeezy_total_ms = squeezy_build_ms + squeezy_query_ms;

    let accuracy = match args.language {
        BenchmarkLanguage::Java => empty_accuracy("rust-analyzer oracle not used for Java"),
        BenchmarkLanguage::Rust => collect_accuracy(&args.fixture, &graph, args.ra_lsp_probes),
        BenchmarkLanguage::CSharp => empty_accuracy("rust-analyzer oracle not used for C#"),
        BenchmarkLanguage::C | BenchmarkLanguage::Cpp => {
            collect_c_family_accuracy(&args.fixture, &graph, args.language, args.oracle_files)?
        }
        BenchmarkLanguage::Python => empty_accuracy("rust-analyzer oracle not used for Python"),
        BenchmarkLanguage::Go => empty_accuracy("rust-analyzer oracle not used for Go"),
        BenchmarkLanguage::JavaScript | BenchmarkLanguage::TypeScript => {
            collect_js_ts_accuracy(&args.fixture, &graph, args.ra_lsp_probes)
        }
    };
    let python_oracle = match args.language {
        BenchmarkLanguage::Python => Some(collect_python_oracle_accuracy(&args.fixture, &graph)?),
        _ => None,
    };
    let go_oracle = match args.language {
        BenchmarkLanguage::Go => Some(collect_go_oracle_accuracy(&args.fixture, &graph)?),
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
    let report = BenchmarkReport {
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
        csharp_oracle,
        go_oracle,
        refresh_probe,
        heuristic_iterations,
        queries: query_reports,
        mixed_workload,
    };

    write_report(&args.report, &report)?;
    print_summary(&report);
    enforce_gates(&report, args.no_speed_gate)
}
