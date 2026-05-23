pub(crate) fn collect_squeezy_symbol_scan(graph: &SemanticGraph) -> SymbolScan {
    collect_squeezy_symbol_scan_excluding_files(graph, &BTreeSet::new())
}

pub(crate) fn collect_squeezy_symbol_scan_excluding_files(
    graph: &SemanticGraph,
    excluded_files: &BTreeSet<String>,
) -> SymbolScan {
    let mut scan = SymbolScan::default();
    for symbol in graph.symbols.values() {
        scan.raw_total += 1;
        match normalize_squeezy_kind(symbol.kind) {
            Some(kind) => {
                let Some(file) = graph.files.get(&symbol.file_id) else {
                    increment(&mut scan.excluded_by_kind, "MissingFile");
                    continue;
                };
                if excluded_files.contains(&file.relative_path) {
                    increment(&mut scan.excluded_by_kind, "OracleUnparseableFile");
                    continue;
                }
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

pub(crate) fn collect_c_family_squeezy_symbol_scan(
    graph: &SemanticGraph,
    language: LanguageKind,
    excluded_files: &BTreeSet<String>,
) -> SymbolScan {
    let mut scan = SymbolScan::default();
    for symbol in graph.symbols.values() {
        let Some(file) = graph.files.get(&symbol.file_id) else {
            increment(&mut scan.excluded_by_kind, "MissingFile");
            continue;
        };
        if file.language != language {
            continue;
        }
        scan.raw_total += 1;
        if excluded_files.contains(&file.relative_path) {
            increment(&mut scan.excluded_by_kind, "OracleUnparseableFile");
            continue;
        }
        if !clang_symbol_name_is_comparable(&symbol.name) {
            increment(&mut scan.excluded_by_kind, "UnnamedOrOperator");
            continue;
        }
        // Clang's AST oracle emits `ClassTemplateSpecializationDecl` (not
        // `CXXRecordDecl`) for `template<> class Foo<int> {}` style
        // declarations, and our `clang_symbol_kind` mapping intentionally
        // skips that kind. Squeezy treats the same node as a Class symbol
        // tagged with `c++:template-specialization`; counting it here would
        // appear as a false positive against the oracle. Exclude these
        // symbols symmetrically.
        if symbol
            .attributes
            .iter()
            .any(|attribute| attribute == "c++:template-specialization")
        {
            increment(&mut scan.excluded_by_kind, "TemplateSpecialization");
            continue;
        }
        match normalize_c_family_squeezy_kind(symbol.kind) {
            Some(kind) => {
                increment_unique_symbol(
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

#[derive(Debug, Deserialize)]
pub(crate) struct PythonAstOracleOutput {
    rows: Vec<[String; 3]>,
    unparseable_files: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct PythonAstSymbolScan {
    symbols: SymbolScan,
    unparseable_files: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct CFamilyClangSymbolScan {
    symbols: SymbolScan,
    parsed_files: usize,
    candidate_files: usize,
    selected_files: usize,
    unparseable_files: Vec<String>,
    excluded_files: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GoAstOracleOutput {
    #[serde(default, deserialize_with = "null_default")]
    rows: Vec<[String; 3]>,
    #[serde(default, deserialize_with = "null_default")]
    unparseable_files: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct GoAstSymbolScan {
    symbols: SymbolScan,
    unparseable_files: Vec<String>,
}

pub(crate) fn null_default<'de, D, T>(deserializer: D) -> std::result::Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

pub(crate) fn collect_python_ast_symbol_scan(root: &Path) -> Result<PythonAstSymbolScan> {
    let output = Command::new("python3")
        .arg("-c")
        .arg(PYTHON_AST_ORACLE)
        .arg(root)
        .output()
        .map_err(|err| SqueezyError::Graph(format!("failed to run Python AST oracle: {err}")))?;
    if !output.status.success() {
        return Err(SqueezyError::Graph(format!(
            "Python AST oracle failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let output: PythonAstOracleOutput = serde_json::from_slice(&output.stdout)
        .map_err(|err| SqueezyError::Graph(format!("invalid Python AST oracle JSON: {err}")))?;
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
    Ok(PythonAstSymbolScan {
        symbols: scan,
        unparseable_files: output.unparseable_files,
    })
}
