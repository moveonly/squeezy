use std::{collections::BTreeSet, path::Path, process::Command};

use serde::{Deserialize, Deserializer};
use squeezy_core::{EdgeKind, LanguageKind, Result, SqueezyError, SymbolKind};
use squeezy_graph::SemanticGraph;
use squeezy_workspace::{CrawlOptions, WorkspaceCrawler};

use crate::{
    accuracy::{increment_symbol, increment_unique_symbol},
    oracles::{
        clang::clang_symbol_name_is_comparable,
        rust_analyzer::{
            normalize_c_family_squeezy_kind, normalize_squeezy_kind, normalize_symbol_name,
        },
    },
    report::{SymbolKey, SymbolScan},
    util::increment,
};

pub(crate) fn collect_squeezy_symbol_scan(graph: &SemanticGraph) -> SymbolScan {
    collect_squeezy_symbol_scan_excluding_files(graph, &BTreeSet::new())
}

#[derive(Debug, Clone, Default)]
pub(crate) struct OracleExclusions {
    files: BTreeSet<String>,
    dirs: Vec<String>,
}

impl OracleExclusions {
    pub(crate) fn excludes(&self, relative_path: &str) -> bool {
        self.files.contains(relative_path)
            || self.dirs.iter().any(|dir| {
                relative_path == dir.trim_end_matches('/')
                    || relative_path.starts_with(dir.as_str())
            })
    }
}

pub(crate) fn default_oracle_exclusions(root: &Path) -> Result<OracleExclusions> {
    let snapshot = WorkspaceCrawler::new(CrawlOptions::default()).crawl(root)?;
    let mut files = BTreeSet::new();
    let mut dirs = Vec::new();
    for excluded in snapshot.excluded {
        if excluded.is_dir {
            let mut prefix = excluded.relative_path;
            if !prefix.ends_with('/') {
                prefix.push('/');
            }
            dirs.push(prefix);
        } else {
            files.insert(excluded.relative_path);
        }
    }
    // Swift fixtures conventionally place generated SwiftGen output under
    // a `generated/` tree and vendored SwiftPM sources under `vendor/`.
    // Both are excluded from the oracle to match SourceKit-LSP's
    // workspace-symbol scan behaviour (spec §9). Files matching
    // `*.generated.swift` are filtered by `is_swift_oracle_excluded_file`
    // in the SourceKit oracle path; the directory exclusions here keep
    // common-scan reports consistent for mixed-language corpus runs.
    for swift_excluded_dir in ["vendor/", "generated/"] {
        if !dirs
            .iter()
            .any(|existing| existing.as_str() == swift_excluded_dir)
        {
            dirs.push(swift_excluded_dir.to_string());
        }
    }
    dirs.sort();
    Ok(OracleExclusions { files, dirs })
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

fn normalize_csharp_squeezy_kind(kind: SymbolKind) -> Option<String> {
    match kind {
        SymbolKind::Class => Some("Class".to_string()),
        SymbolKind::Interface => Some("Interface".to_string()),
        SymbolKind::Module => Some("Module".to_string()),
        SymbolKind::Struct => Some("Struct".to_string()),
        SymbolKind::Enum => Some("Enum".to_string()),
        SymbolKind::Function | SymbolKind::Test => Some("Function".to_string()),
        SymbolKind::Method => Some("Method".to_string()),
        SymbolKind::TypeAlias => Some("TypeAlias".to_string()),
        SymbolKind::Field => Some("Field".to_string()),
        SymbolKind::Variant => Some("Variant".to_string()),
        SymbolKind::Crate
        | SymbolKind::File
        | SymbolKind::Union
        | SymbolKind::Trait
        | SymbolKind::Impl
        | SymbolKind::Const
        | SymbolKind::Static
        | SymbolKind::Macro
        | SymbolKind::Unknown => None,
    }
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

/// Scala-specific symbol-scan normalisation. The shapes squeezy emits
/// (`Struct` for case classes, `Enum` for Scala 3 enums, `Variant` for enum
/// cases, `Function` for top-level defs, `Const` for `val` / `given`) all map
/// to SemanticDB's flatter, JVM-bytecode-shaped declaration set:
///
///   * case class -> Class + companion Class + synthetic `apply` / `unapply`
///     / `toString` / `copy` / `copy$default$N` / `_N` Method peers (the
///     bytecode the compiler emits for `case class C(p1, p2, ...)`)
///   * Scala 3 enum -> Class + companion Class, each case -> Method
///   * top-level def / extension method -> Method (lives inside the synthetic
///     `<package>$package` object)
///   * val / given -> Method (the bytecode getter)
///   * primary-constructor `val` parameter -> Field on squeezy / Method getter
///     on SemanticDB -> SemanticDB getter is filtered, so squeezy drops the
///     Field too to avoid an asymmetric Method emission
///   * files under `src/generated/` are dropped to mirror the bench helper's
///     scalac source filter
pub(crate) fn collect_scala_squeezy_symbol_scan_excluding_files(
    graph: &SemanticGraph,
    excluded_files: &BTreeSet<String>,
) -> SymbolScan {
    let mut scan = SymbolScan::default();
    for symbol in graph.symbols.values() {
        let Some(file) = graph.files.get(&symbol.file_id) else {
            increment(&mut scan.excluded_by_kind, "MissingFile");
            continue;
        };
        if file.language != LanguageKind::Scala {
            continue;
        }
        scan.raw_total += 1;
        let rel = &file.relative_path;
        if excluded_files.contains(rel) {
            increment(&mut scan.excluded_by_kind, "OracleUnparseableFile");
            continue;
        }
        // Mirror the scalac source filter (`benchmarks/oracle/scala/run_oracle.sh`):
        // `src/generated/`, `vendor/`, `target/`, `out/`, `build/`,
        // `node_modules/`. The default crawler already drops `target/` and
        // friends via `OracleExclusions`, but `src/generated/` lives inside
        // an indexed `src/` tree so it is not picked up by the workspace
        // exclusion list and must be filtered here.
        if path_under(rel, "src/generated/")
            || path_under(rel, "generated/")
            || path_under(rel, "vendor/")
        {
            increment(&mut scan.excluded_by_kind, "ExcludedPath");
            continue;
        }
        let name = normalize_symbol_name(&symbol.name);
        let kinds = scala_normalized_kinds(symbol.kind, &symbol.attributes);
        if kinds.is_empty() {
            increment(&mut scan.excluded_by_kind, &format!("{:?}", symbol.kind));
            continue;
        }
        for kind in kinds {
            increment_symbol(
                &mut scan.counts,
                SymbolKey {
                    file: rel.clone(),
                    kind,
                    name: name.clone(),
                },
            );
        }
        // Case-class synthetic members. SemanticDB exposes `apply`,
        // `unapply`, `toString`, `copy`, `copy$default$N`, `_N`; squeezy
        // does not parse them out of the source. Emit the matching Method
        // peers here so the comparison sees them on both sides.
        if symbol.kind == SymbolKind::Struct
            && symbol
                .attributes
                .iter()
                .any(|attr| attr == "scala:case-class")
        {
            for peer in scala_case_class_synthetic_peers() {
                increment_symbol(
                    &mut scan.counts,
                    SymbolKey {
                        file: rel.clone(),
                        kind: "Method".to_string(),
                        name: peer,
                    },
                );
            }
        }
    }
    scan
}

/// Returns the synthetic Method names a Scala 3 case class compiles to that
/// the SemanticDB oracle still surfaces (i.e. names that are NOT in
/// `is_case_class_synthetic_name` on the oracle side). `apply`, `unapply`
/// and `toString` are common-enough overrides that the oracle keeps them
/// and the squeezy scan adds matching peers; `copy`/`copy$default$N`/`_N`
/// are filtered by the oracle so we do not emit peers for them.
fn scala_case_class_synthetic_peers() -> Vec<String> {
    vec![
        "apply".to_string(),
        "unapply".to_string(),
        "toString".to_string(),
    ]
}

fn path_under(rel: &str, prefix: &str) -> bool {
    rel == prefix.trim_end_matches('/') || rel.starts_with(prefix)
}

/// Returns the SemanticDB-shaped kind labels that should be emitted for the
/// given squeezy symbol. Most symbols map 1:1; case classes and Scala 3 enums
/// emit a companion Class symbol so the SemanticDB comparison sees both the
/// class and its companion-object entries.
fn scala_normalized_kinds(kind: SymbolKind, attributes: &[String]) -> Vec<String> {
    let is_case_class = attributes.iter().any(|attr| attr == "scala:case-class");
    match kind {
        SymbolKind::Class => vec!["Class".to_string()],
        SymbolKind::Interface => vec!["Interface".to_string()],
        SymbolKind::Module => vec!["Module".to_string()],
        SymbolKind::Struct => {
            // case class C(...) -> Class:C + companion Class:C
            if is_case_class {
                vec!["Class".to_string(), "Class".to_string()]
            } else {
                vec!["Class".to_string()]
            }
        }
        SymbolKind::Enum => {
            // Scala 3 enum -> Class + companion Class
            vec!["Class".to_string(), "Class".to_string()]
        }
        SymbolKind::Variant => vec!["Method".to_string()],
        SymbolKind::Trait => vec!["Trait".to_string()],
        // Top-level defs, extension methods and user-defined methods alike
        // are emitted as kind=Method by SemanticDB — top-level defs live on
        // the synthetic `<file>$package` object; extensions live on the
        // same synthetic object; member methods live on their owning
        // class / trait.
        SymbolKind::Function | SymbolKind::Method => vec!["Method".to_string()],
        // val / given bindings emit a Method-shaped getter at the bytecode
        // level. Top-level `var` likewise becomes a getter + setter pair on
        // SemanticDB — we map both to Method for parity.
        SymbolKind::Const | SymbolKind::Static => vec!["Method".to_string()],
        SymbolKind::TypeAlias => vec!["TypeAlias".to_string()],
        // SemanticDB filters constructor-parameter getters
        // (`Class#param.`) via `is_class_parameter_getter`. Drop them
        // here too so the comparison stays balanced.
        SymbolKind::Field => Vec::new(),
        SymbolKind::Macro
        | SymbolKind::Impl
        | SymbolKind::Union
        | SymbolKind::Crate
        | SymbolKind::File
        | SymbolKind::Test
        | SymbolKind::Unknown => Vec::new(),
    }
}

pub(crate) fn collect_csharp_squeezy_symbol_scan_excluding_files(
    graph: &SemanticGraph,
    excluded_files: &BTreeSet<String>,
) -> SymbolScan {
    let mut scan = SymbolScan::default();
    for symbol in graph.symbols.values() {
        let Some(file) = graph.files.get(&symbol.file_id) else {
            increment(&mut scan.excluded_by_kind, "MissingFile");
            continue;
        };
        if file.language != LanguageKind::CSharp {
            continue;
        }
        scan.raw_total += 1;
        if excluded_files.contains(&file.relative_path) {
            increment(&mut scan.excluded_by_kind, "OracleUnparseableFile");
            continue;
        }
        match normalize_csharp_squeezy_kind(symbol.kind) {
            Some(kind) => {
                let name = symbol
                    .language_identity
                    .as_deref()
                    .unwrap_or(&symbol.name)
                    .to_string();
                increment_symbol(
                    &mut scan.counts,
                    SymbolKey {
                        file: file.relative_path.clone(),
                        kind,
                        name,
                    },
                );
            }
            None => increment(&mut scan.excluded_by_kind, &format!("{:?}", symbol.kind)),
        }
    }
    scan
}

/// Normalises a squeezy PHP symbol to the kind label nikic/PHP-Parser
/// emits. The extractor models PHP namespaces as `SymbolKind::Module` and
/// both class properties and class constants as `SymbolKind::Field`; the
/// nikic helper distinguishes `Namespace`, `Property`, and `Constant`. The
/// `php:field` / `php:const` attributes set by the extractor disambiguate
/// the two `Field` flavours so the symbol scan can compare like-with-like.
fn normalize_php_squeezy_kind(kind: SymbolKind, attributes: &[String]) -> Option<String> {
    match kind {
        SymbolKind::Class => Some("Class".to_string()),
        SymbolKind::Interface => Some("Interface".to_string()),
        SymbolKind::Trait => Some("Trait".to_string()),
        SymbolKind::Enum => Some("Enum".to_string()),
        SymbolKind::Module => Some("Namespace".to_string()),
        SymbolKind::Function | SymbolKind::Test => Some("Function".to_string()),
        SymbolKind::Method => Some("Method".to_string()),
        SymbolKind::Field => {
            if attributes.iter().any(|attr| attr == "php:const") {
                Some("Constant".to_string())
            } else {
                Some("Property".to_string())
            }
        }
        SymbolKind::Variant => Some("Variant".to_string()),
        SymbolKind::Struct
        | SymbolKind::Crate
        | SymbolKind::File
        | SymbolKind::Union
        | SymbolKind::TypeAlias
        | SymbolKind::Impl
        | SymbolKind::Const
        | SymbolKind::Static
        | SymbolKind::Macro
        | SymbolKind::Unknown => None,
    }
}

pub(crate) fn collect_php_squeezy_symbol_scan_excluding_files(
    graph: &SemanticGraph,
    excluded_files: &BTreeSet<String>,
) -> SymbolScan {
    let mut scan = SymbolScan::default();
    for symbol in graph.symbols.values() {
        let Some(file) = graph.files.get(&symbol.file_id) else {
            increment(&mut scan.excluded_by_kind, "MissingFile");
            continue;
        };
        if file.language != LanguageKind::Php {
            continue;
        }
        scan.raw_total += 1;
        if excluded_files.contains(&file.relative_path) {
            increment(&mut scan.excluded_by_kind, "OracleUnparseableFile");
            continue;
        }
        // Exclude PHP-specific noise per spec §4 and §9: heredoc/nowdoc
        // bodies (extractor never emits identifier symbols from these),
        // eval-argument identifiers (suppressed in the extractor), magic
        // methods (declarations stay but call sites lower confidence;
        // declarations stay countable here too — only `Method:__call`
        // would diverge if the oracle had no equivalent, but nikic emits
        // it).
        if symbol
            .attributes
            .iter()
            .any(|attribute| attribute == "php:eval-argument")
        {
            increment(&mut scan.excluded_by_kind, "PhpEvalArgument");
            continue;
        }
        match normalize_php_squeezy_kind(symbol.kind, &symbol.attributes) {
            Some(kind) => {
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

pub(crate) fn collect_php_squeezy_edge_scan_excluding_files(
    graph: &SemanticGraph,
    excluded_files: &BTreeSet<String>,
) -> SymbolScan {
    let mut scan = SymbolScan::default();
    for edge in graph.edges() {
        if !matches!(edge.kind, EdgeKind::Extends | EdgeKind::Implements) {
            continue;
        }
        let Some(from) = graph.symbols.get(&edge.from) else {
            continue;
        };
        let Some(file) = graph.files.get(&from.file_id) else {
            continue;
        };
        if file.language != LanguageKind::Php {
            continue;
        }
        scan.raw_total += 1;
        if excluded_files.contains(&file.relative_path) {
            increment(&mut scan.excluded_by_kind, "OracleUnparseableFile");
            continue;
        }
        increment_symbol(
            &mut scan.counts,
            SymbolKey {
                file: file.relative_path.clone(),
                kind: format!("{:?}", edge.kind),
                name: format!("{}->{}", from.name, edge.target_text),
            },
        );
    }
    scan
}

/// Symmetric exclusion list for the Ruby Prism oracle (spec §9):
/// - synthesized `attr_*` methods (squeezy emits them, Prism does not)
/// - block-local / method-local identifiers (extractor doesn't emit these
///   as symbols today; documented for parity with the C++ filter)
/// - `define_method`-built methods (extractor doesn't emit them either)
pub(crate) fn collect_squeezy_ruby_symbol_scan_excluding_files(
    graph: &SemanticGraph,
    excluded_files: &BTreeSet<String>,
) -> SymbolScan {
    let mut scan = SymbolScan::default();
    for symbol in graph.symbols.values() {
        let Some(file) = graph.files.get(&symbol.file_id) else {
            increment(&mut scan.excluded_by_kind, "MissingFile");
            continue;
        };
        if file.language != LanguageKind::Ruby {
            continue;
        }
        scan.raw_total += 1;
        if excluded_files.contains(&file.relative_path) {
            increment(&mut scan.excluded_by_kind, "OracleUnparseableFile");
            continue;
        }
        if symbol
            .attributes
            .iter()
            .any(|attribute| attribute == "ruby:synthesized")
        {
            increment(&mut scan.excluded_by_kind, "RubyAttrSynthesized");
            continue;
        }
        // Synthetic Function symbols emitted for `require`/`require_relative`
        // import directives exist only to surface in `signature_search`;
        // Prism doesn't emit them, so excluding them keeps the scan-only
        // self-compare in lockstep with the Prism oracle when it's
        // available.
        if symbol
            .attributes
            .iter()
            .any(|attribute| attribute == "ruby:import-directive")
        {
            increment(&mut scan.excluded_by_kind, "RubyImportDirective");
            continue;
        }
        let kind = match symbol.kind {
            SymbolKind::Class => "Class".to_string(),
            SymbolKind::Module => "Module".to_string(),
            SymbolKind::Method => "Method".to_string(),
            SymbolKind::Function | SymbolKind::Test => "Function".to_string(),
            // Const/Field/etc are not emitted by the Prism oracle for the
            // first PR; exclude them symmetrically to avoid skewed FP.
            _ => {
                increment(&mut scan.excluded_by_kind, &format!("{:?}", symbol.kind));
                continue;
            }
        };
        increment_symbol(
            &mut scan.counts,
            SymbolKey {
                file: file.relative_path.clone(),
                kind,
                name: normalize_symbol_name(&symbol.name),
            },
        );
    }
    scan
}

pub(crate) fn collect_csharp_squeezy_edge_scan_excluding_files(
    graph: &SemanticGraph,
    excluded_files: &BTreeSet<String>,
) -> SymbolScan {
    let mut scan = SymbolScan::default();
    for edge in graph.edges() {
        if !matches!(edge.kind, EdgeKind::Extends | EdgeKind::Implements) {
            continue;
        }
        let Some(from) = graph.symbols.get(&edge.from) else {
            continue;
        };
        let Some(file) = graph.files.get(&from.file_id) else {
            continue;
        };
        if file.language != LanguageKind::CSharp {
            continue;
        }
        scan.raw_total += 1;
        if excluded_files.contains(&file.relative_path) {
            increment(&mut scan.excluded_by_kind, "OracleUnparseableFile");
            continue;
        }
        let Some(from_identity) = from.language_identity.as_deref() else {
            increment(&mut scan.excluded_by_kind, "MissingLanguageIdentity");
            continue;
        };
        increment_symbol(
            &mut scan.counts,
            SymbolKey {
                file: file.relative_path.clone(),
                kind: format!("{:?}", edge.kind),
                name: format!("{from_identity}->{}", edge.target_text),
            },
        );
    }
    scan
}

#[derive(Debug, Deserialize)]
pub(crate) struct PythonAstOracleOutput {
    pub(crate) rows: Vec<[String; 3]>,
    pub(crate) unparseable_files: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct PythonAstSymbolScan {
    pub(crate) symbols: SymbolScan,
    pub(crate) unparseable_files: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct CFamilyClangSymbolScan {
    pub(crate) symbols: SymbolScan,
    pub(crate) parsed_files: usize,
    pub(crate) candidate_files: usize,
    pub(crate) selected_files: usize,
    pub(crate) unparseable_files: Vec<String>,
    pub(crate) excluded_files: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GoAstOracleOutput {
    #[serde(default, deserialize_with = "null_default")]
    pub(crate) rows: Vec<[String; 3]>,
    #[serde(default, deserialize_with = "null_default")]
    pub(crate) unparseable_files: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct GoAstSymbolScan {
    pub(crate) symbols: SymbolScan,
    pub(crate) unparseable_files: Vec<String>,
}

pub(crate) fn null_default<'de, D, T>(deserializer: D) -> std::result::Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

pub(crate) fn collect_python_ast_symbol_scan(root: &Path) -> Result<PythonAstSymbolScan> {
    let exclusions = default_oracle_exclusions(root)?;
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
    let unparseable_files = output
        .unparseable_files
        .into_iter()
        .filter(|file| !exclusions.excludes(file))
        .collect();
    Ok(PythonAstSymbolScan {
        symbols: scan,
        unparseable_files,
    })
}

const PYTHON_AST_ORACLE: &str = r#"
import ast
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1]).resolve()
rows = []
unparseable_files = []

class Visitor(ast.NodeVisitor):
    def __init__(self, rel):
        self.rel = rel
        self.parents = []

    def visit_ClassDef(self, node):
        rows.append([self.rel, "Class", node.name])
        self.parents.append("Class")
        self.generic_visit(node)
        self.parents.pop()

    def visit_FunctionDef(self, node):
        kind = "Method" if self.parents and self.parents[-1] == "Class" else "Function"
        rows.append([self.rel, kind, node.name])
        self.parents.append(kind)
        self.generic_visit(node)
        self.parents.pop()

    visit_AsyncFunctionDef = visit_FunctionDef

for path in sorted(root.rglob("*.py")):
    rel = path.relative_to(root).as_posix()
    try:
        tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
    except (SyntaxError, UnicodeDecodeError):
        unparseable_files.append(rel)
        continue
    Visitor(rel).visit(tree)

print(json.dumps({"rows": rows, "unparseable_files": unparseable_files}))
"#;

#[cfg(test)]
#[path = "common_scan_tests.rs"]
mod tests;
