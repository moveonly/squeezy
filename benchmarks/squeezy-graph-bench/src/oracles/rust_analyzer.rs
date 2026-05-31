use std::{
    collections::BTreeSet,
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    time::Instant,
};

use serde_json::{Value, json};
use squeezy_core::{LanguageKind, Result, SqueezyError, SymbolKind};
use squeezy_graph::SemanticGraph;

use crate::{
    accuracy::{LspNavigationClient, increment_symbol, merge_symbol_scan},
    report::{LocationKey, LspPosition, SymbolKey, SymbolScan},
    util::{command_exists, increment, truncate},
};

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

pub(crate) fn parse_rust_analyzer_symbol_line(
    line: &str,
    file: &str,
) -> Option<(String, Option<SymbolKey>)> {
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

pub(crate) fn time_rust_analyzer(repo: &Path) -> (Option<u128>, String) {
    let started = Instant::now();
    let Some(mut command) = rust_analyzer_command() else {
        return (None, "rust-analyzer not found".to_string());
    };
    let output = command
        .arg("analysis-stats")
        .arg("--run-all-ide-things")
        .arg(repo)
        .output();
    match output {
        Ok(output) if output.status.success() => (
            Some(started.elapsed().as_millis()),
            "rust-analyzer analysis-stats --run-all-ide-things succeeded".to_string(),
        ),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = stderr
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or_default();
            (
                None,
                format!(
                    "rust-analyzer analysis-stats failed with {}{}",
                    output.status,
                    if detail.is_empty() {
                        String::new()
                    } else {
                        format!(": {}", truncate(detail, 240))
                    }
                ),
            )
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            (None, "rust-analyzer not found".to_string())
        }
        Err(err) => (None, format!("rust-analyzer failed to start: {err}")),
    }
}

pub(crate) struct RustAnalyzerLsp {
    root: PathBuf,
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
    opened: BTreeSet<String>,
}

impl RustAnalyzerLsp {
    pub(crate) fn start(root: &Path) -> Result<Self> {
        let Some(program) = rust_analyzer_program() else {
            return Err(SqueezyError::Graph("rust-analyzer not found".to_string()));
        };
        let root = fs::canonicalize(root)?;
        let mut child = Command::new(program)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| SqueezyError::Graph("failed to open rust-analyzer stdin".to_string()))?;
        let stdout = child.stdout.take().ok_or_else(|| {
            SqueezyError::Graph("failed to open rust-analyzer stdout".to_string())
        })?;
        let mut client = Self {
            root: root.clone(),
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
            opened: BTreeSet::new(),
        };
        let root_uri = path_to_file_uri(&root)?;
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
                        "references": {}
                    }
                }
            }),
        )?;
        client.notify("initialized", json!({}))?;
        std::thread::sleep(std::time::Duration::from_millis(750));
        Ok(client)
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
                    "languageId": "rust",
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
                    "rust-analyzer LSP closed stdout".to_string(),
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

impl LspNavigationClient for RustAnalyzerLsp {
    fn did_open(&mut self, uri: &str, path: &Path) -> Result<()> {
        RustAnalyzerLsp::did_open(self, uri, path)
    }

    fn definition(&mut self, uri: &str, position: LspPosition) -> Result<Vec<LocationKey>> {
        RustAnalyzerLsp::definition(self, uri, position)
    }

    fn references(&mut self, uri: &str, position: LspPosition) -> Result<Vec<LocationKey>> {
        RustAnalyzerLsp::references(self, uri, position)
    }
}

impl Drop for RustAnalyzerLsp {
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

pub(crate) fn parse_lsp_locations(value: &Value, root: &Path) -> Result<Vec<LocationKey>> {
    if value.is_null() {
        return Ok(Vec::new());
    }
    if let Some(items) = value.as_array() {
        return items
            .iter()
            .map(|item| parse_lsp_location(item, root))
            .collect();
    }
    parse_lsp_location(value, root).map(|location| vec![location])
}

pub(crate) fn parse_lsp_location(value: &Value, root: &Path) -> Result<LocationKey> {
    let uri = value
        .get("uri")
        .or_else(|| value.get("targetUri"))
        .and_then(Value::as_str)
        .ok_or_else(|| SqueezyError::Graph(format!("LSP location missing uri: {value}")))?;
    let range = value
        .get("range")
        .or_else(|| value.get("targetSelectionRange"))
        .or_else(|| value.get("targetRange"))
        .ok_or_else(|| SqueezyError::Graph(format!("LSP location missing range: {value}")))?;
    let start = range
        .get("start")
        .ok_or_else(|| SqueezyError::Graph(format!("LSP range missing start: {range}")))?;
    let line = start
        .get("line")
        .and_then(Value::as_u64)
        .ok_or_else(|| SqueezyError::Graph(format!("LSP range start missing line: {start}")))?
        as u32;
    let character = start
        .get("character")
        .and_then(Value::as_u64)
        .ok_or_else(|| SqueezyError::Graph(format!("LSP range start missing character: {start}")))?
        as u32;
    let path = file_uri_to_path(uri)?;
    Ok(LocationKey {
        file: location_file_key(root, &path),
        line,
        character,
    })
}

pub(crate) fn location_file_key(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .ok()
        .map(|relative| relative.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

pub(crate) fn path_to_file_uri(path: &Path) -> Result<String> {
    let path = fs::canonicalize(path)?;
    let raw = path.to_string_lossy();
    Ok(format!("file://{}", percent_encode_path(&raw)))
}

pub(crate) fn file_uri_to_path(uri: &str) -> Result<PathBuf> {
    let raw = uri
        .strip_prefix("file://")
        .ok_or_else(|| SqueezyError::Graph(format!("unsupported non-file URI {uri}")))?;
    Ok(PathBuf::from(percent_decode(raw)?))
}

pub(crate) fn percent_encode_path(path: &str) -> String {
    let mut out = String::new();
    for byte in path.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

pub(crate) fn percent_decode(value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3])
                .map_err(|err| SqueezyError::Graph(format!("invalid URI escape: {err}")))?;
            out.push(
                u8::from_str_radix(hex, 16).map_err(|err| {
                    SqueezyError::Graph(format!("invalid URI escape %{hex}: {err}"))
                })?,
            );
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out)
        .map_err(|err| SqueezyError::Graph(format!("invalid UTF-8 file URI: {err}")))
}

pub(crate) fn byte_to_lsp_position(source: &str, byte: usize) -> LspPosition {
    let byte = byte.min(source.len());
    let mut line = 0u32;
    let mut line_start = 0usize;
    for (index, ch) in source.char_indices() {
        if index >= byte {
            break;
        }
        if ch == '\n' {
            line += 1;
            line_start = index + ch.len_utf8();
        }
    }
    let character = source
        .get(line_start..byte)
        .unwrap_or_default()
        .encode_utf16()
        .count() as u32;
    LspPosition { line, character }
}

pub(crate) fn line_char_to_byte(source: &str, line: u32, character: u32) -> Option<usize> {
    let mut current_line = 0u32;
    let mut line_start = 0usize;
    for (index, ch) in source.char_indices() {
        if current_line == line {
            break;
        }
        if ch == '\n' {
            current_line += 1;
            line_start = index + ch.len_utf8();
        }
    }
    if current_line != line {
        return None;
    }

    let mut utf16 = 0u32;
    for (offset, ch) in source[line_start..].char_indices() {
        if ch == '\n' {
            break;
        }
        if utf16 == character {
            return Some(line_start + offset);
        }
        utf16 += ch.len_utf16() as u32;
        if utf16 > character {
            return Some(line_start + offset);
        }
    }
    Some(
        line_start
            + source[line_start..]
                .lines()
                .next()
                .unwrap_or_default()
                .len(),
    )
}

pub(crate) fn rust_analyzer_command() -> Option<Command> {
    rust_analyzer_program().map(Command::new)
}

pub(crate) fn rust_analyzer_program() -> Option<String> {
    if command_exists("rust-analyzer") {
        return Some("rust-analyzer".to_string());
    }
    let output = Command::new("rustup")
        .arg("which")
        .arg("rust-analyzer")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let path = String::from_utf8(output.stdout).ok()?;
    let path = path.trim();
    if path.is_empty() {
        None
    } else {
        Some(path.to_string())
    }
}

#[cfg(test)]
#[path = "rust_analyzer_tests.rs"]
mod tests;
