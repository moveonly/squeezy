pub(crate) fn collect_rust_analyzer_symbol_scan(graph: &SemanticGraph) -> (SymbolScan, String) {
    let Some(program) = rust_analyzer_program() else {
        return (SymbolScan::default(), "rust-analyzer not found".to_string());
    };

    let mut records = graph
        .files
        .values()
        .filter(|record| record.language == LanguageKind::Rust)
        .collect::<Vec<_>>();
    records.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));

    let mut scan = SymbolScan::default();
    let mut failures = Vec::new();
    for record in &records {
        match rust_analyzer_symbols_for_file(&program, record) {
            Ok(Some(file_scan)) => {
                merge_symbol_scan(&mut scan, file_scan);
            }
            Ok(None) => {
                scan.skipped_non_utf8_files += 1;
            }
            Err(err) => {
                failures.push(format!("{}: {err}", record.relative_path));
            }
        }
    }

    if failures.is_empty() {
        (
            scan.clone(),
            format!(
                "rust-analyzer symbols succeeded for {} Rust files; skipped {} non-UTF-8 Rust files",
                records.len() - scan.skipped_non_utf8_files,
                scan.skipped_non_utf8_files
            ),
        )
    } else {
        (
            scan,
            format!(
                "rust-analyzer symbols partially failed for {}/{} Rust files: {}",
                failures.len(),
                records.len(),
                failures
                    .iter()
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        )
    }
}

pub(crate) fn rust_analyzer_symbols_for_file(
    program: &str,
    record: &squeezy_workspace::FileRecord,
) -> Result<Option<SymbolScan>> {
    let source = match fs::read_to_string(&record.path) {
        Ok(source) => source,
        Err(err) if err.kind() == std::io::ErrorKind::InvalidData => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let mut child = Command::new(program)
        .arg("symbols")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| SqueezyError::Graph("failed to open rust-analyzer stdin".to_string()))?;
    stdin.write_all(source.as_bytes())?;
    drop(stdin);

    let output = child.wait_with_output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SqueezyError::Graph(format!(
            "rust-analyzer symbols failed with {}: {}",
            output.status,
            stderr.trim()
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut scan = SymbolScan::default();
    for line in stdout.lines() {
        let Some((raw_kind, key)) = parse_rust_analyzer_symbol_line(line, &record.relative_path)
        else {
            continue;
        };
        scan.raw_total += 1;
        if let Some(key) = key {
            increment_symbol(&mut scan.counts, key);
        } else {
            increment(&mut scan.excluded_by_kind, &raw_kind);
        }
    }
    Ok(Some(scan))
}

pub(crate) fn parse_rust_analyzer_symbol_line(line: &str, file: &str) -> Option<(String, Option<SymbolKey>)> {
    let label = extract_quoted_field(line, "label")?;
    let raw_kind = extract_symbol_kind(line)?;
    let key = normalize_rust_analyzer_kind(&raw_kind).map(|kind| SymbolKey {
        file: file.to_string(),
        kind,
        name: normalize_symbol_name(&label),
    });
    Some((raw_kind, key))
}

pub(crate) fn extract_quoted_field(line: &str, field: &str) -> Option<String> {
    let prefix = format!("{field}: \"");
    let start = line.find(&prefix)? + prefix.len();
    let mut escaped = false;
    let mut value = String::new();
    for ch in line[start..].chars() {
        if escaped {
            value.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            return Some(value);
        } else {
            value.push(ch);
        }
    }
    None
}

pub(crate) fn extract_symbol_kind(line: &str) -> Option<String> {
    let prefix = "kind: SymbolKind(";
    let start = line.find(prefix)? + prefix.len();
    let rest = &line[start..];
    let end = rest.find(')')?;
    Some(rest[..end].to_string())
}

pub(crate) fn normalize_rust_analyzer_kind(kind: &str) -> Option<String> {
    match kind {
        "Module" => Some("Module".to_string()),
        "Struct" => Some("Struct".to_string()),
        "Enum" => Some("Enum".to_string()),
        "Union" => Some("Union".to_string()),
        "Trait" => Some("Trait".to_string()),
        "Impl" => Some("Impl".to_string()),
        "Function" => Some("Function".to_string()),
        "Method" => Some("Method".to_string()),
        "Const" => Some("Const".to_string()),
        "Static" => Some("Static".to_string()),
        "TypeAlias" => Some("TypeAlias".to_string()),
        "Macro" => Some("Macro".to_string()),
        _ => None,
    }
}

pub(crate) fn normalize_squeezy_kind(kind: SymbolKind) -> Option<String> {
    match kind {
        SymbolKind::Class => Some("Class".to_string()),
        SymbolKind::Interface => Some("Interface".to_string()),
        SymbolKind::Module => Some("Module".to_string()),
        SymbolKind::Struct => Some("Struct".to_string()),
        SymbolKind::Enum => Some("Enum".to_string()),
        SymbolKind::Union => Some("Union".to_string()),
        SymbolKind::Trait => Some("Trait".to_string()),
        SymbolKind::Impl => Some("Impl".to_string()),
        SymbolKind::Function | SymbolKind::Test => Some("Function".to_string()),
        SymbolKind::Method => Some("Method".to_string()),
        SymbolKind::Const => Some("Const".to_string()),
        SymbolKind::Static => Some("Static".to_string()),
        SymbolKind::TypeAlias => Some("TypeAlias".to_string()),
        SymbolKind::Macro => Some("Macro".to_string()),
        SymbolKind::Crate
        | SymbolKind::File
        | SymbolKind::Field
        | SymbolKind::Variant
        | SymbolKind::Unknown => None,
    }
}

pub(crate) fn normalize_c_family_squeezy_kind(kind: SymbolKind) -> Option<String> {
    match kind {
        SymbolKind::Class => Some("Class".to_string()),
        SymbolKind::Module => Some("Module".to_string()),
        SymbolKind::Struct => Some("Struct".to_string()),
        SymbolKind::Enum => Some("Enum".to_string()),
        SymbolKind::Union => Some("Union".to_string()),
        SymbolKind::Function | SymbolKind::Test => Some("Function".to_string()),
        SymbolKind::Method => Some("Method".to_string()),
        SymbolKind::TypeAlias => Some("TypeAlias".to_string()),
        // `Interface` is a Go concept and never produced by the C/C++
        // parser path, but the type system still needs an arm for it.
        SymbolKind::Crate
        | SymbolKind::Trait
        | SymbolKind::Impl
        | SymbolKind::Interface
        | SymbolKind::Const
        | SymbolKind::Static
        | SymbolKind::Macro
        | SymbolKind::File
        | SymbolKind::Field
        | SymbolKind::Variant
        | SymbolKind::Unknown => None,
    }
}

pub(crate) fn normalize_symbol_name(name: &str) -> String {
    trim_impl_header(&name.split_whitespace().collect::<Vec<_>>().join(" "))
}

pub(crate) fn trim_impl_header(raw: &str) -> String {
    let trimmed = raw.trim();
    let trimmed = trimmed.strip_prefix("unsafe ").unwrap_or(trimmed);
    let Some(rest) = trimmed.strip_prefix("impl") else {
        return trimmed.to_string();
    };
    let Some(next) = rest.chars().next() else {
        return trimmed.to_string();
    };
    if !next.is_whitespace() && next != '<' {
        return trimmed.to_string();
    }

    let mut rest = rest.trim_start();
    if rest.starts_with('<') {
        let mut depth = 0usize;
        let mut close_index = None;
        let mut previous = None;
        for (index, ch) in rest.char_indices() {
            match ch {
                '<' => depth += 1,
                '>' if previous != Some('-') => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        close_index = Some(index + ch.len_utf8());
                        break;
                    }
                }
                _ => {}
            }
            previous = Some(ch);
        }
        if let Some(index) = close_index {
            rest = rest[index..].trim_start();
        }
    }
    rest.split_once(" where ")
        .map(|(before, _)| before)
        .unwrap_or(rest)
        .trim_end_matches(',')
        .to_string()
}
