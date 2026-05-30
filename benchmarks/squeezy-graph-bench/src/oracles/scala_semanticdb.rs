use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

use squeezy_core::{Result, SqueezyError};
use squeezy_graph::SemanticGraph;

use crate::{
    accuracy::{compare_symbol_sets, increment_symbol, symbol_count},
    oracles::common_scan::{
        collect_scala_squeezy_symbol_scan_excluding_files, default_oracle_exclusions,
    },
    oracles::rust_analyzer::normalize_symbol_name,
    report::{ScalaOracleReport, SymbolKey, SymbolScan},
    util::{command_exists, increment, temp_dir},
};

/// Status prefix indicating the oracle ran end-to-end against SemanticDB
/// protobufs. Used by gate logic to decide whether to enforce P/R thresholds.
pub(crate) const SCALA_SEMANTICDB_STATUS_PREFIX: &str = "SemanticDB oracle succeeded";

/// Status prefix indicating scalac was unavailable or failed. Gates honor this
/// as a soft skip so CI runners without a Scala toolchain still pass.
pub(crate) const SCALA_SCAN_ONLY_PREFIX: &str = "scan-only-fallback";

/// Times the SemanticDB oracle for validation comparison. Returns
/// (elapsed_ms, status). When scalac is missing the elapsed time is 0 and
/// status carries the scan-only-fallback prefix.
pub(crate) fn time_scala_oracle_optional(root: &Path) -> (u128, String) {
    if !command_exists("scalac") {
        return (
            0,
            format!("{SCALA_SCAN_ONLY_PREFIX}: scalac not found on PATH"),
        );
    }
    let started = Instant::now();
    match run_semanticdb_pipeline(root) {
        Ok(_) => (
            started.elapsed().as_millis(),
            format!("{SCALA_SEMANTICDB_STATUS_PREFIX} (validation timing)"),
        ),
        Err(err) => (
            0,
            format!("{SCALA_SCAN_ONLY_PREFIX}: scalac semanticdb failed: {err}"),
        ),
    }
}

pub(crate) fn collect_scala_oracle_accuracy(
    root: &Path,
    graph: &SemanticGraph,
) -> Result<ScalaOracleReport> {
    let squeezy_symbols =
        collect_scala_squeezy_symbol_scan_excluding_files(graph, &std::collections::BTreeSet::new());
    if !command_exists("scalac") {
        return Ok(ScalaOracleReport {
            oracle_ms: None,
            status: format!("{SCALA_SCAN_ONLY_PREFIX}: scalac not found on PATH"),
            oracle_unparseable_files: 0,
            oracle_unparseable_examples: Vec::new(),
            symbols: compare_symbol_sets(&squeezy_symbols, &SymbolScan::default()),
            limitations: scala_oracle_limitations(),
        });
    }
    let started = Instant::now();
    match run_semanticdb_pipeline(root) {
        Ok(pipeline) => {
            let oracle_ms = started.elapsed().as_millis();
            let oracle_unparseable_files = pipeline.unparseable_files.len();
            let oracle_unparseable_examples = pipeline
                .unparseable_files
                .iter()
                .take(10)
                .cloned()
                .collect::<Vec<_>>();
            let status = if pipeline.unparseable_files.is_empty() {
                format!(
                    "{SCALA_SEMANTICDB_STATUS_PREFIX} ({} declaration symbols)",
                    symbol_count(&pipeline.scan.counts)
                )
            } else {
                format!(
                    "{SCALA_SEMANTICDB_STATUS_PREFIX} ({} declaration symbols, {oracle_unparseable_files} unparseable files excluded)",
                    symbol_count(&pipeline.scan.counts)
                )
            };
            Ok(ScalaOracleReport {
                oracle_ms: Some(oracle_ms),
                status,
                oracle_unparseable_files,
                oracle_unparseable_examples,
                symbols: compare_symbol_sets(&squeezy_symbols, &pipeline.scan),
                limitations: scala_oracle_limitations(),
            })
        }
        Err(err) => Ok(ScalaOracleReport {
            oracle_ms: None,
            status: format!("{SCALA_SCAN_ONLY_PREFIX}: scalac semanticdb failed: {err}"),
            oracle_unparseable_files: 0,
            oracle_unparseable_examples: Vec::new(),
            symbols: compare_symbol_sets(&squeezy_symbols, &SymbolScan::default()),
            limitations: scala_oracle_limitations(),
        }),
    }
}

pub(crate) fn scala_oracle_limitations() -> Vec<String> {
    vec![
        "Scala oracle reads SemanticDB protobufs emitted by `scalac -Xsemanticdb`; implicit-conversion injection at call sites, `given`/`using` resolution at call sites, and macro-expanded synthetic members are excluded from the symbol comparison.".to_string(),
        "Path-dependent type references (`a.B`) are emitted as references with no resolution edge; they are excluded from navigation accuracy.".to_string(),
        "Anonymous classes and lambda bodies are not compared; SemanticDB emits `<anon>` symbols that the tree-sitter extractor omits.".to_string(),
        "Local `val`/`var` (LOCAL kind in SemanticDB), parameters, type parameters, and bare fields are excluded — squeezy does not emit comparable kinds for them.".to_string(),
        "If `scalac` is unavailable, the oracle falls back to scan-only and the gate is suppressed so CI without a Scala toolchain still passes.".to_string(),
    ]
}

#[derive(Debug)]
struct SemanticDbPipeline {
    scan: SymbolScan,
    unparseable_files: Vec<String>,
}

fn run_semanticdb_pipeline(root: &Path) -> Result<SemanticDbPipeline> {
    let scratch = temp_dir("squeezy-scala-semanticdb")?;
    let sdb_target = scratch.join("semanticdb");
    let classes = scratch.join("classes");
    fs::create_dir_all(&sdb_target)?;
    fs::create_dir_all(&classes)?;
    let absolute_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let sources = collect_scala_sources(&absolute_root);
    if sources.is_empty() {
        // No Scala sources to analyze — return an empty scan so gate logic
        // still surfaces the SemanticDB-succeeded status path.
        return Ok(SemanticDbPipeline {
            scan: SymbolScan::default(),
            unparseable_files: Vec::new(),
        });
    }
    let helper_script = scalac_helper_script(&absolute_root);
    // Scalac emits `.semanticdb` files alongside the source path when given
    // absolute source paths; with relative source paths it correctly
    // honours `-semanticdb-target`. Always invoke scalac with cwd == root
    // and pass sources as paths relative to `root`.
    let relative_sources = sources
        .iter()
        .map(|path| {
            path.strip_prefix(&absolute_root)
                .map(Path::to_path_buf)
                .unwrap_or_else(|_| path.clone())
        })
        .collect::<Vec<_>>();
    let output = if let Some(helper) = helper_script {
        Command::new("bash")
            .arg(helper)
            .arg(&absolute_root)
            .arg(&sdb_target)
            .arg(&classes)
            .current_dir(&absolute_root)
            .output()
            .map_err(|err| SqueezyError::Graph(format!("failed to spawn scala helper: {err}")))?
    } else {
        let mut cmd = Command::new("scalac");
        cmd.current_dir(&absolute_root)
            .arg("-Xsemanticdb")
            .arg("-semanticdb-target")
            .arg(&sdb_target)
            .arg("-d")
            .arg(&classes)
            .args(&relative_sources);
        cmd.output()
            .map_err(|err| SqueezyError::Graph(format!("failed to spawn scalac: {err}")))?
    };
    if !output.status.success() {
        return Err(SqueezyError::Graph(format!(
            "scalac -Xsemanticdb exited {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    parse_semanticdb_tree(&absolute_root, &sdb_target)
}

fn scalac_helper_script(root: &Path) -> Option<PathBuf> {
    // Walk up from `root` looking for the bench-supplied helper. The helper
    // shields callers from per-project sbt/mill build configuration: it
    // tries `sbt`/`mill` first when their build files exist, then falls
    // back to a bare `scalac` invocation over every `.scala` source.
    let mut cursor = root;
    for _ in 0..8 {
        let candidate = cursor.join("benchmarks/oracle/scala/run_oracle.sh");
        if candidate.exists() {
            return Some(candidate);
        }
        match cursor.parent() {
            Some(parent) => cursor = parent,
            None => break,
        }
    }
    None
}

fn collect_scala_sources(root: &Path) -> Vec<PathBuf> {
    let mut sources = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                // Skip vendored / generated trees that scalac will refuse
                // (they may import deps we have not staged). Mirrors the
                // exclusion list maintained for the Java oracle.
                if matches!(
                    name.as_ref(),
                    "vendor" | "generated" | "target" | "out" | "build" | "node_modules"
                ) {
                    continue;
                }
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "scala") {
                sources.push(path);
            }
        }
    }
    sources.sort();
    sources
}

fn parse_semanticdb_tree(root: &Path, sdb_dir: &Path) -> Result<SemanticDbPipeline> {
    let exclusions = default_oracle_exclusions(root)?;
    let mut scan = SymbolScan::default();
    let mut unparseable_files = Vec::new();
    let mut stack = vec![sdb_dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "semanticdb") {
                continue;
            }
            let bytes = fs::read(&path).map_err(SqueezyError::from)?;
            match decode_text_documents(&bytes) {
                Ok(documents) => {
                    for document in documents {
                        absorb_document(root, &exclusions, document, &mut scan);
                    }
                }
                Err(err) => {
                    let rel = path
                        .strip_prefix(sdb_dir)
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|_| path.display().to_string());
                    unparseable_files.push(format!("{rel}: {err}"));
                }
            }
        }
    }
    unparseable_files.sort();
    Ok(SemanticDbPipeline {
        scan,
        unparseable_files,
    })
}

fn absorb_document(
    root: &Path,
    exclusions: &crate::oracles::common_scan::OracleExclusions,
    document: TextDocument,
    scan: &mut SymbolScan,
) {
    let rel = normalize_document_uri(root, &document.uri);
    for symbol in document.symbols {
        scan.raw_total += 1;
        // SemanticDB symbol strings end with a kind-specific descriptor
        // sigil that distinguishes overloaded members. Synthetic compiler
        // helpers carry the `synthetic` access marker — their symbol id
        // contains `(synthetic)` somewhere in the encoding.
        if is_synthetic_symbol(&symbol.symbol) {
            increment(&mut scan.excluded_by_kind, "SemanticDbSynthetic");
            continue;
        }
        let Some(kind) = map_symbol_kind(symbol.kind) else {
            increment(
                &mut scan.excluded_by_kind,
                &semanticdb_kind_label(symbol.kind),
            );
            continue;
        };
        if symbol.display_name.is_empty() {
            increment(&mut scan.excluded_by_kind, "EmptyDisplayName");
            continue;
        }
        // Compiler-generated primary / secondary constructors are emitted
        // with a `<init>` display name; squeezy does not emit a sibling
        // constructor symbol for `class Foo { ... }`. Skip them so they do
        // not inflate the false-positive tally.
        if symbol.display_name == "<init>" {
            increment(&mut scan.excluded_by_kind, "ConstructorInit");
            continue;
        }
        // Synthetic case-class members emitted by the compiler — `apply`,
        // `unapply`, `copy`, `copy$default$N`, `_<N>`, `productElement`,
        // `productElementName`, `productPrefix`, `productArity`,
        // `productIterator`, `canEqual` — and Scala 3 enum desugaring
        // helpers (`$new`, `$values`, `values`, `valueOf`, `fromOrdinal`)
        // do not appear in the source tree squeezy parses. Drop them so
        // they do not push the comparison into FN territory.
        if is_case_class_synthetic_name(&symbol.display_name)
            || is_enum_synthetic_name(&symbol.display_name)
        {
            increment(&mut scan.excluded_by_kind, "SemanticDbCaseClassSynthetic");
            continue;
        }
        // Scalac synthesises a `<file>$package` package object for every
        // file with top-level definitions. Its display_name is the
        // package leaf (e.g. `app`, `ext`) and its symbol id ends in
        // `$package.`. squeezy does not model these autogenerated
        // wrappers; treating them as Module FNs would mask the actual
        // declaration accuracy.
        if symbol.symbol.ends_with("$package.") {
            increment(&mut scan.excluded_by_kind, "SemanticDbSyntheticPackage");
            continue;
        }
        // Constructor-parameter getters (`Class#param.` — kind 3 Method
        // with a `.` terminator and no `()` in the symbol id) are emitted
        // by the compiler for every primary-constructor `val` parameter.
        // Squeezy already surfaces these as Field symbols on the class;
        // the bench-side scan emits a Method peer to match this getter,
        // so retaining the SemanticDB entry would cause double counting.
        if is_class_parameter_getter(&symbol.symbol) {
            increment(&mut scan.excluded_by_kind, "SemanticDbParamGetter");
            continue;
        }
        if exclusions.excludes(&rel) {
            increment(&mut scan.excluded_by_kind, "ExcludedPath");
            continue;
        }
        increment_symbol(
            &mut scan.counts,
            SymbolKey {
                file: rel.clone(),
                kind,
                name: normalize_symbol_name(&symbol.display_name),
            },
        );
    }
}

/// True when the display name matches a name that is *only* ever emitted by
/// the compiler — `copy`, `copy$default$N`, `_<digit>`, and the `Product*` /
/// `canEqual` machinery. `apply`/`unapply`/`toString` are deliberately *not*
/// on this list: users routinely override them on hand-written objects, and
/// the squeezy-side scan compensates for the case-class auto-generated
/// versions by emitting matching Method peers (`scala_case_class_synthetic_peers`).
fn is_case_class_synthetic_name(name: &str) -> bool {
    matches!(
        name,
        "copy"
            | "productArity"
            | "productIterator"
            | "productPrefix"
            | "productElement"
            | "productElementName"
            | "productElementNames"
            | "canEqual"
    ) || name.starts_with("copy$default$")
        || is_product_accessor(name)
}

/// Matches `_1`..`_22` — the positional product accessors generated for
/// case classes / tuples.
fn is_product_accessor(name: &str) -> bool {
    let Some(rest) = name.strip_prefix('_') else {
        return false;
    };
    !rest.is_empty() && rest.chars().all(|ch| ch.is_ascii_digit())
}

/// Scala 3 enum desugaring helpers emitted at the companion-object level
/// for every `enum` declaration.
fn is_enum_synthetic_name(name: &str) -> bool {
    matches!(name, "$new" | "$values" | "values" | "valueOf" | "fromOrdinal")
}

/// Detects a primary-constructor parameter getter. Its symbol id ends in
/// `<owner>#<param>.` (the `.` descriptor introducer, no `()` arglist).
fn is_class_parameter_getter(symbol_id: &str) -> bool {
    let Some(stripped) = symbol_id.strip_suffix('.') else {
        return false;
    };
    if stripped.contains("()") {
        return false;
    }
    // Owner must reference an instance (`#`) or companion (`.`) container.
    let owner_split = stripped.rfind(['#', '.']);
    let Some(idx) = owner_split else { return false };
    let owner = &stripped[..=idx];
    let member = &stripped[idx + 1..];
    if member.is_empty() {
        return false;
    }
    // Exclude top-level `<file>$package` getters; those are handled by the
    // dedicated `$package` filter above.
    !owner.ends_with("$package.") && owner.contains('#')
}

fn normalize_document_uri(root: &Path, uri: &str) -> String {
    let trimmed = uri
        .strip_prefix("file://")
        .map(|s| s.to_string())
        .unwrap_or_else(|| uri.to_string());
    let trimmed = trimmed.trim_start_matches('/');
    let path = Path::new(&trimmed);
    if path.is_absolute() {
        path.strip_prefix(root)
            .map(|stripped| stripped.display().to_string().replace('\\', "/"))
            .unwrap_or_else(|_| trimmed.replace('\\', "/"))
    } else {
        trimmed.replace('\\', "/")
    }
}

fn is_synthetic_symbol(symbol: &str) -> bool {
    // Scalameta encodes synthetic symbols with a `local` prefix or with the
    // `<` introducer (for compiler-generated anonymous classes / locals)
    // — both are excluded from comparable kinds upstream.
    symbol.starts_with("local") || symbol.contains("$anon") || symbol.contains("$$anonfun")
}

fn map_symbol_kind(kind: i32) -> Option<String> {
    match kind {
        SDB_KIND_CLASS | SDB_KIND_OBJECT => Some("Class".to_string()),
        SDB_KIND_INTERFACE => Some("Interface".to_string()),
        SDB_KIND_TRAIT => Some("Trait".to_string()),
        SDB_KIND_METHOD | SDB_KIND_CONSTRUCTOR | SDB_KIND_MACRO => Some("Method".to_string()),
        SDB_KIND_PACKAGE_OBJECT => Some("Module".to_string()),
        SDB_KIND_TYPE => Some("TypeAlias".to_string()),
        _ => None,
    }
}

fn semanticdb_kind_label(kind: i32) -> String {
    match kind {
        SDB_KIND_UNKNOWN => "Unknown".to_string(),
        SDB_KIND_LOCAL => "Local".to_string(),
        SDB_KIND_FIELD => "Field".to_string(),
        SDB_KIND_PARAMETER => "Parameter".to_string(),
        SDB_KIND_SELF_PARAMETER => "SelfParameter".to_string(),
        SDB_KIND_TYPE_PARAMETER => "TypeParameter".to_string(),
        SDB_KIND_PACKAGE => "Package".to_string(),
        _ => format!("SemanticDb{kind}"),
    }
}

// SemanticDB SymbolInformation.Kind enum values, copied from
// scalameta/scalameta/.../semanticdb.proto §SymbolInformation.Kind.
const SDB_KIND_UNKNOWN: i32 = 0;
const SDB_KIND_METHOD: i32 = 3;
const SDB_KIND_MACRO: i32 = 6;
const SDB_KIND_TYPE: i32 = 7;
const SDB_KIND_PARAMETER: i32 = 8;
const SDB_KIND_TYPE_PARAMETER: i32 = 9;
const SDB_KIND_OBJECT: i32 = 10;
const SDB_KIND_PACKAGE: i32 = 11;
const SDB_KIND_PACKAGE_OBJECT: i32 = 12;
const SDB_KIND_CLASS: i32 = 13;
const SDB_KIND_TRAIT: i32 = 14;
const SDB_KIND_SELF_PARAMETER: i32 = 17;
const SDB_KIND_INTERFACE: i32 = 18;
const SDB_KIND_LOCAL: i32 = 19;
const SDB_KIND_FIELD: i32 = 20;
const SDB_KIND_CONSTRUCTOR: i32 = 21;

/// Minimal `TextDocument` reflecting only the SemanticDB fields the oracle
/// compares against.
#[derive(Debug, Default, Clone)]
pub(crate) struct TextDocument {
    pub(crate) uri: String,
    pub(crate) symbols: Vec<SymbolInformation>,
}

/// Minimal `SymbolInformation` carrying the kind + display name needed for
/// declaration-set comparison.
#[derive(Debug, Default, Clone)]
pub(crate) struct SymbolInformation {
    pub(crate) symbol: String,
    pub(crate) kind: i32,
    pub(crate) display_name: String,
}

/// Decodes the `TextDocuments` envelope into the minimal field set the
/// oracle compares against. Implements a forgiving protobuf wire-format
/// decoder — unknown fields, including the much larger Type/Signature
/// trees, are skipped without allocating. This avoids dragging
/// `prost-build` (and therefore `protoc`) into the bench crate.
pub(crate) fn decode_text_documents(bytes: &[u8]) -> std::result::Result<Vec<TextDocument>, String> {
    let mut cursor = Cursor::new(bytes);
    let mut documents = Vec::new();
    while !cursor.eof() {
        let (field, wire) = cursor.read_tag()?;
        if field == 1 && wire == WIRE_LEN {
            let body = cursor.read_length_delimited()?;
            documents.push(decode_text_document(body)?);
        } else {
            cursor.skip(wire)?;
        }
    }
    Ok(documents)
}

fn decode_text_document(bytes: &[u8]) -> std::result::Result<TextDocument, String> {
    let mut cursor = Cursor::new(bytes);
    let mut document = TextDocument::default();
    while !cursor.eof() {
        let (field, wire) = cursor.read_tag()?;
        match (field, wire) {
            (2, WIRE_LEN) => {
                let body = cursor.read_length_delimited()?;
                document.uri =
                    std::str::from_utf8(body).map_err(|err| err.to_string())?.to_string();
            }
            (5, WIRE_LEN) => {
                let body = cursor.read_length_delimited()?;
                document.symbols.push(decode_symbol_information(body)?);
            }
            _ => cursor.skip(wire)?,
        }
    }
    Ok(document)
}

fn decode_symbol_information(bytes: &[u8]) -> std::result::Result<SymbolInformation, String> {
    let mut cursor = Cursor::new(bytes);
    let mut symbol = SymbolInformation::default();
    while !cursor.eof() {
        let (field, wire) = cursor.read_tag()?;
        match (field, wire) {
            (1, WIRE_LEN) => {
                let body = cursor.read_length_delimited()?;
                symbol.symbol =
                    std::str::from_utf8(body).map_err(|err| err.to_string())?.to_string();
            }
            (3, WIRE_VARINT) => {
                symbol.kind = cursor.read_varint()? as i32;
            }
            (5, WIRE_LEN) => {
                let body = cursor.read_length_delimited()?;
                symbol.display_name =
                    std::str::from_utf8(body).map_err(|err| err.to_string())?.to_string();
            }
            _ => cursor.skip(wire)?,
        }
    }
    Ok(symbol)
}

// Protobuf wire types we care about. Wire types 3 and 4 (start/end group)
// are deprecated since proto3 and never appear in SemanticDB payloads.
const WIRE_VARINT: u8 = 0;
const WIRE_FIXED64: u8 = 1;
const WIRE_LEN: u8 = 2;
const WIRE_FIXED32: u8 = 5;

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn eof(&self) -> bool {
        self.offset >= self.bytes.len()
    }

    fn read_varint(&mut self) -> std::result::Result<u64, String> {
        let mut result = 0u64;
        let mut shift = 0u32;
        loop {
            if self.offset >= self.bytes.len() {
                return Err("varint overran buffer".to_string());
            }
            let byte = self.bytes[self.offset];
            self.offset += 1;
            result |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
            if shift >= 64 {
                return Err("varint exceeded 64 bits".to_string());
            }
        }
    }

    fn read_tag(&mut self) -> std::result::Result<(u32, u8), String> {
        let raw = self.read_varint()?;
        let field = (raw >> 3) as u32;
        let wire = (raw & 0x07) as u8;
        if field == 0 {
            return Err("tag has zero field number".to_string());
        }
        Ok((field, wire))
    }

    fn read_length_delimited(&mut self) -> std::result::Result<&'a [u8], String> {
        let length = self.read_varint()? as usize;
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| "length-delimited overflow".to_string())?;
        if end > self.bytes.len() {
            return Err(format!(
                "length-delimited overran buffer: need {end} of {}",
                self.bytes.len()
            ));
        }
        let body = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(body)
    }

    fn skip(&mut self, wire: u8) -> std::result::Result<(), String> {
        match wire {
            WIRE_VARINT => {
                self.read_varint()?;
            }
            WIRE_FIXED64 => {
                if self.offset + 8 > self.bytes.len() {
                    return Err("fixed64 overran buffer".to_string());
                }
                self.offset += 8;
            }
            WIRE_LEN => {
                self.read_length_delimited()?;
            }
            WIRE_FIXED32 => {
                if self.offset + 4 > self.bytes.len() {
                    return Err("fixed32 overran buffer".to_string());
                }
                self.offset += 4;
            }
            other => return Err(format!("unsupported wire type {other}")),
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "scala_semanticdb_tests.rs"]
mod tests;
