pub(crate) fn run_mixed_workload(
    repo: &Path,
    language: BenchmarkLanguage,
    requested_scenarios: usize,
    ra_lsp_probes: usize,
    oracle_files: usize,
) -> Result<MixedWorkloadReport> {
    let (compiler_check_ms, compiler_check_status) = match language {
        BenchmarkLanguage::C => time_clang_syntax_optional(repo, "clang", LanguageKind::C),
        BenchmarkLanguage::CSharp => time_dotnet_build_optional(repo),
        BenchmarkLanguage::Cpp => time_clang_syntax_optional(repo, "clang++", LanguageKind::Cpp),
        BenchmarkLanguage::Go => (
            None,
            "mixed workload compiler check not used for Go".to_string(),
        ),
        BenchmarkLanguage::Java => (
            None,
            "mixed workload compiler check not used for Java".to_string(),
        ),
        BenchmarkLanguage::Rust => time_cargo_check_optional(repo),
        BenchmarkLanguage::Python => (None, "mixed workload unsupported for Python".to_string()),
        BenchmarkLanguage::JavaScript | BenchmarkLanguage::TypeScript => {
            match time_js_ts_oracle(repo) {
                Ok(ms) => (Some(ms), "TypeScript compiler API oracle".to_string()),
                Err(err) => (
                    None,
                    format!("TypeScript compiler API oracle unavailable: {err}"),
                ),
            }
        }
    };
    let (rust_analyzer_ms, rust_analyzer_status) = if language == BenchmarkLanguage::Rust {
        time_rust_analyzer(repo)
    } else {
        (
            None,
            "rust-analyzer oracle not used for this language".to_string(),
        )
    };

    let build = build_graph(repo)?;
    let graph = build.graph;
    let squeezy_build_ms = build.phases.total_ms;
    let accuracy = match language {
        BenchmarkLanguage::Rust => collect_accuracy(repo, &graph, ra_lsp_probes),
        BenchmarkLanguage::CSharp => match collect_csharp_oracle_accuracy(repo, &graph) {
            Ok(report) => csharp_oracle_to_accuracy(&report),
            Err(err) => empty_accuracy(&format!("C# semantic oracle failed: {err}")),
        },
        BenchmarkLanguage::C | BenchmarkLanguage::Cpp => {
            collect_c_family_accuracy(repo, &graph, language, oracle_files)?
        }
        BenchmarkLanguage::Go => match collect_go_oracle_accuracy(repo, &graph) {
            Ok(report) => go_oracle_to_accuracy(&report),
            Err(err) => empty_accuracy(&format!("Go semantic oracle failed: {err}")),
        },
        BenchmarkLanguage::Java => {
            empty_accuracy("mixed workload accuracy oracle not used for Java")
        }
        BenchmarkLanguage::Python => empty_accuracy("mixed workload unsupported for Python"),
        BenchmarkLanguage::JavaScript | BenchmarkLanguage::TypeScript => {
            collect_js_ts_accuracy(repo, &graph, ra_lsp_probes)
        }
    };

    let scenarios = build_mixed_scenarios(&graph);
    let scenario_indexes = select_scenarios(scenarios.len(), requested_scenarios);
    let tools = scenarios
        .iter()
        .map(|scenario| scenario.tool().to_string())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();

    let query_started = Instant::now();
    let mut query_counts = BTreeMap::new();
    let mut query_time_micros: BTreeMap<String, u128> = BTreeMap::new();
    for index in scenario_indexes {
        let scenario = &scenarios[index];
        let scenario_started = Instant::now();
        run_mixed_scenario(&graph, scenario);
        let elapsed = scenario_started.elapsed().as_micros();
        increment(&mut query_counts, scenario.tool());
        *query_time_micros
            .entry(scenario.tool().to_string())
            .or_default() += elapsed;
    }
    let squeezy_query_ms = query_started.elapsed().as_millis();
    let query_time_ms = query_time_micros
        .into_iter()
        .map(|(tool, micros)| (tool, micros / 1_000))
        .collect::<BTreeMap<_, _>>();
    let squeezy_total_ms = squeezy_build_ms + squeezy_query_ms;
    let refresh_probe = run_refresh_probe(repo, language)?;

    Ok(MixedWorkloadReport {
        repo: repo.display().to_string(),
        requested_scenarios,
        available_scenarios: scenarios.len(),
        executed_scenarios: query_counts.values().sum(),
        tools,
        compiler_check_ms,
        compiler_check_status,
        rust_analyzer_ms,
        rust_analyzer_status,
        squeezy_build_ms,
        squeezy_query_ms,
        squeezy_total_ms,
        faster_than_compiler_check: compiler_check_ms.map(|ms| squeezy_total_ms < ms),
        faster_than_rust_analyzer: rust_analyzer_ms.map(|ms| squeezy_total_ms < ms),
        query_counts,
        query_time_ms,
        refresh_probe,
        accuracy,
    })
}

