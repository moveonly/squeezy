pub(crate) fn collect_python_oracle_accuracy(
    root: &Path,
    graph: &SemanticGraph,
) -> Result<PythonOracleReport> {
    let started = Instant::now();
    let oracle = collect_python_ast_symbol_scan(root)?;
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

    Ok(PythonOracleReport {
        oracle_ms,
        status: if oracle_unparseable_files == 0 {
            "CPython ast oracle succeeded".to_string()
        } else {
            format!(
                "CPython ast oracle succeeded with {oracle_unparseable_files} unparseable files excluded from symbol FP accounting"
            )
        },
        oracle_unparseable_files,
        oracle_unparseable_examples,
        symbols,
        limitations: vec![
            "The Python oracle uses CPython ast for declarations and does not execute imports, infer dynamic attributes, or model metaclass-generated members.".to_string(),
            "Symbol comparison is file/name/kind based so it tracks declaration loss without pretending to prove runtime dispatch.".to_string(),
            "Python files that CPython ast cannot parse are reported as oracle_unparseable and excluded from Squeezy false-positive accounting; tree-sitter recovery remains useful for production editing workflows.".to_string(),
        ],
    })
}
