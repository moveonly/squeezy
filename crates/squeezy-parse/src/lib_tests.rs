use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use squeezy_core::{ContentHash, FileId};
use squeezy_workspace::{FileRecord, stable_content_hash};

use super::*;

#[test]
fn rust_parser_extracts_symbols_imports_calls_and_references() {
    let source = r#"
use crate::service::Service as Svc;

pub struct Runner;

const _: () = ();

pub trait Service {
    type Error: std::fmt::Display;
}

impl Runner {
    pub fn run(&self, svc: Svc) {
        svc.execute();
        helper();
        println!("done");
    }
}

fn helper() {}
"#;
    let mut parser = LanguageParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();

    assert!(parsed.unsupported.is_none());
    assert!(parsed.symbols.iter().any(|symbol| symbol.name == "Runner"));
    assert!(parsed.symbols.iter().any(|symbol| symbol.name == "run"));
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "Error" && symbol.kind == SymbolKind::TypeAlias)
    );
    assert!(
        !parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "_" && symbol.kind == SymbolKind::Const)
    );
    assert!(
        parsed
            .imports
            .iter()
            .any(|import| import.path.contains("Service"))
    );
    assert!(parsed.calls.iter().any(|call| call.name == "execute"));
    assert!(parsed.calls.iter().any(|call| call.name == "helper"));
    assert!(
        parsed
            .calls
            .iter()
            .any(|call| call.kind == ParsedCallKind::Macro)
    );
    assert!(
        parsed
            .references
            .iter()
            .any(|reference| reference.text == "Svc")
    );
}

#[test]
fn parser_extracts_python_symbols_imports_calls_and_references() {
    let source = r#"
from services.greeter import Greeter as GreeterAlias
from .models import User
import helpers

__all__ = ["Runner"]

class Runner(GreeterAlias):
    """Runs greetings."""

    id: int
    name: str = Field(default="")
    db_name = Column(String)
    django_name = models.CharField(max_length=255)

    @decorator
    @router.get("/hello/{name}")
    def run(self, name: GreeterAlias, user: User) -> GreeterAlias:
        helper = helpers.build(name)
        return self.greet(helper)

RunnerAlias = Runner

def make_runner():
    runner = RunnerAlias()
    return runner.run("Ada")

def test_runner():
    assert make_runner()
"#;
    let mut parser = LanguageParser::new().unwrap();
    let record = python_record("src/package/app.py", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();

    assert!(parsed.unsupported.is_none());
    assert!(parsed.symbols.iter().any(|symbol| symbol.name == "Runner"
        && symbol.kind == SymbolKind::Class
        && symbol.attributes.contains(&"base:GreeterAlias".to_string())
        && symbol.docs.iter().any(|doc| doc.contains("Runs greetings"))));
    assert!(parsed.symbols.iter().any(|symbol| {
        symbol.name == "run"
            && symbol.kind == SymbolKind::Method
            && symbol
                .attributes
                .contains(&"route:GET /hello/{name}".to_string())
            && symbol
                .attributes
                .contains(&"framework:web-route".to_string())
    }));
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "make_runner" && symbol.kind == SymbolKind::Function)
    );
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "test_runner"
                && symbol.attributes.contains(&"pytest:test".to_string()))
    );
    assert!(parsed.symbols.iter().any(|symbol| symbol.name == "name"
        && symbol.kind == SymbolKind::Field
        && symbol.attributes.contains(&"type:str".to_string())
        && symbol.attributes.contains(&"pydantic:field".to_string())));
    assert!(parsed.symbols.iter().any(|symbol| symbol.name == "db_name"
        && symbol.kind == SymbolKind::Field
        && symbol.attributes.contains(&"sqlalchemy:field".to_string())));
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "django_name"
                && symbol.kind == SymbolKind::Field
                && symbol.attributes.contains(&"django:field".to_string()))
    );
    assert!(
        parsed
            .imports
            .iter()
            .any(|import| import.path == "services.greeter.Greeter"
                && import.alias.as_deref() == Some("GreeterAlias"))
    );
    assert!(
        parsed
            .imports
            .iter()
            .any(|import| import.path == "package.models.User")
    );
    assert!(
        parsed
            .imports
            .iter()
            .any(|import| import.path == "Runner" && import.is_reexport)
    );
    assert!(
        parsed
            .imports
            .iter()
            .any(|import| import.path == "Runner"
                && import.alias.as_deref() == Some("RunnerAlias"))
    );
    assert!(
        parsed
            .imports
            .iter()
            .any(|import| import.path == "RunnerAlias"
                && import.alias.as_deref() == Some("runner"))
    );
    assert!(
        parsed
            .calls
            .iter()
            .any(|call| call.name == "build" && call.kind == ParsedCallKind::Method)
    );
    assert!(parsed.calls.iter().any(|call| call.name == "RunnerAlias"));
    assert!(
        parsed
            .references
            .iter()
            .any(|reference| reference.text == "GreeterAlias"
                && reference.kind == ReferenceKind::Type)
    );
}

#[test]
fn python_parser_skips_calls_inside_decorators() {
    let source = r#"
from fastapi import APIRouter

router = APIRouter()

class Runner:
    @router.get("/x")
    def handle(self):
        return router.get_settings()
"#;
    let mut parser = LanguageParser::new().unwrap();
    let mut record_py = record("src/handler.py", source);
    record_py.language = LanguageKind::Python;
    let parsed = parser.parse_source(&record_py, source.to_string()).unwrap();

    // The decorator `@router.get("/x")` must NOT generate a `get` call edge
    // belonging to the surrounding class. The body call to `router.get_settings()`
    // is still recorded.
    let decorator_get_call = parsed
        .calls
        .iter()
        .any(|call| call.name == "get" && call.receiver.as_deref() == Some("router"));
    assert!(
        !decorator_get_call,
        "calls inside `@router.get(...)` must not be recorded as method calls",
    );
    let body_get_settings_call = parsed
        .calls
        .iter()
        .any(|call| call.name == "get_settings" && call.receiver.as_deref() == Some("router"));
    assert!(
        body_get_settings_call,
        "body method call must still be recorded",
    );
}

#[test]
fn parser_extracts_go_symbols_imports_calls_and_references() {
    let source = r#"
package greeter

import (
    "fmt"
    util "example.com/acme/app/util"
    . "example.com/acme/app/dot"
    _ "example.com/acme/app/sideeffect"
)

const DefaultName = "Ada"
var shared = Runner{}
var First, Second = 1, 2
var formatter = func() string {
    var closureLocal string
    return closureLocal
}

type Alias = string
type (
    AliasFunc = func(string) bool
    LocalType = Runner
)

type Greeter interface {
    Greet(name string) string
}

type Runner struct {
    Name string
    Greeter
}

func NewRunner(name string) Runner {
    return Runner{Name: name}
}

func (r Runner) Greet(name string) string {
    var localOnly string
    fmt.Println(name)
    helper()
    _ = localOnly
    return util.Format(name)
}

func helper() {}

func (r Runner) TestSuiteStyle() {}
"#;
    let mut parser = LanguageParser::new().unwrap();
    let record = go_record("greeter/runner_test.go", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();

    assert!(parsed.unsupported.is_none());
    assert_eq!(parsed.package.as_deref(), Some("greeter"));
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "Greeter" && symbol.kind == SymbolKind::Interface)
    );
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "Runner" && symbol.kind == SymbolKind::Struct)
    );
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "Alias" && symbol.kind == SymbolKind::TypeAlias)
    );
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "AliasFunc" && symbol.kind == SymbolKind::TypeAlias)
    );
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "LocalType" && symbol.kind == SymbolKind::TypeAlias)
    );
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "Name" && symbol.kind == SymbolKind::Field)
    );
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "NewRunner" && symbol.kind == SymbolKind::Function)
    );
    assert!(parsed.symbols.iter().any(|symbol| {
        symbol.name == "Greet"
            && symbol.kind == SymbolKind::Method
            && symbol
                .attributes
                .contains(&"go:receiver:Runner".to_string())
    }));
    assert!(parsed.symbols.iter().any(|symbol| {
        symbol.name == "TestSuiteStyle"
            && symbol.kind == SymbolKind::Test
            && symbol
                .attributes
                .contains(&"go:receiver:Runner".to_string())
    }));
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "DefaultName" && symbol.kind == SymbolKind::Const)
    );
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "First" && symbol.kind == SymbolKind::Static)
    );
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "Second" && symbol.kind == SymbolKind::Static)
    );
    assert!(
        !parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "name" && symbol.kind == SymbolKind::Static),
        "local variables must not be exposed as top-level graph declarations"
    );
    assert!(
        !parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "localOnly" && symbol.kind == SymbolKind::Static),
        "function-local variables must stay out of declaration symbols"
    );
    assert!(
        !parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "closureLocal" && symbol.kind == SymbolKind::Static),
        "variables inside top-level function literals must stay out of declaration symbols"
    );
    assert!(
        parsed
            .imports
            .iter()
            .any(|import| import.path == "example.com/acme/app/util"
                && import.alias.as_deref() == Some("util"))
    );
    assert!(
        parsed
            .imports
            .iter()
            .any(|import| import.path == "example.com/acme/app/dot" && import.is_glob)
    );
    assert!(
        parsed
            .calls
            .iter()
            .any(|call| call.name == "Println" && call.receiver.as_deref() == Some("fmt"))
    );
    assert!(parsed.calls.iter().any(|call| call.name == "helper"));
    assert!(
        parsed
            .references
            .iter()
            .any(|reference| reference.text == "Runner" && reference.kind == ReferenceKind::Type)
    );
}

#[test]
fn go_parser_tags_embedded_struct_fields_with_embed_attribute() {
    let source = r#"
package greeter

type Greeter interface {
    Greet(name string) string
}

type Runner struct {
    Name string
    Greeter
}
"#;
    let mut parser = LanguageParser::new().unwrap();
    let record = go_record("greeter/embed.go", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();

    let runner_id = parsed
        .symbols
        .iter()
        .find(|symbol| symbol.name == "Runner" && symbol.kind == SymbolKind::Struct)
        .map(|symbol| symbol.id.clone())
        .expect("Runner struct declaration");
    let name_field = parsed
        .symbols
        .iter()
        .find(|symbol| {
            symbol.name == "Name"
                && symbol.kind == SymbolKind::Field
                && symbol.parent_id.as_ref() == Some(&runner_id)
        })
        .expect("Name field");
    assert!(name_field.attributes.contains(&"go:field".to_string()));
    assert!(
        !name_field.attributes.contains(&"go:embed".to_string()),
        "named fields must not be tagged go:embed"
    );
    let embedded = parsed
        .symbols
        .iter()
        .find(|symbol| {
            symbol.name == "Greeter"
                && symbol.kind == SymbolKind::Field
                && symbol.parent_id.as_ref() == Some(&runner_id)
        })
        .expect("embedded Greeter field");
    assert!(embedded.attributes.contains(&"go:embed".to_string()));
    assert!(embedded.attributes.contains(&"go:field".to_string()));
}

#[test]
fn go_parser_attaches_methods_to_types_declared_after_them() {
    let source = r#"
package greeter

func (r Runner) Greet(name string) string {
    return name
}

type Runner struct {
    Name string
}
"#;
    let mut parser = LanguageParser::new().unwrap();
    let record = go_record("greeter/order.go", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();

    let runner = parsed
        .symbols
        .iter()
        .find(|symbol| symbol.name == "Runner" && symbol.kind == SymbolKind::Struct)
        .expect("Runner struct declaration");
    let greet = parsed
        .symbols
        .iter()
        .find(|symbol| symbol.name == "Greet" && symbol.kind == SymbolKind::Method)
        .expect("Greet method declaration");
    assert_eq!(
        greet.parent_id.as_ref(),
        Some(&runner.id),
        "method declared before its type must still attach to the type"
    );
    assert!(
        greet
            .attributes
            .iter()
            .any(|attribute| attribute == "go:receiver:Runner")
    );
}

#[test]
fn go_parser_does_not_emit_wrapper_body_hit_for_selectors() {
    let source = r#"
package greeter

import "fmt"

func Use() {
    fmt.Println("hello")
}
"#;
    let mut parser = LanguageParser::new().unwrap();
    let record = go_record("greeter/use.go", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();

    // The selector `fmt.Println` should not emit a wrapper body hit covering
    // the whole `fmt.Println` text. The operand and field still produce body
    // hits via child traversal, so each appears at most once.
    let selector_hits = parsed
        .body_hits
        .iter()
        .filter(|hit| hit.text == "fmt.Println")
        .count();
    assert_eq!(
        selector_hits, 0,
        "selector wrappers must not be duplicated as body hits"
    );
    assert!(
        parsed.body_hits.iter().any(|hit| hit.text == "fmt"),
        "operand body hit must still be present"
    );
    assert!(
        parsed.body_hits.iter().any(|hit| hit.text == "Println"),
        "field body hit must still be present"
    );
    // The full-selector text is still recorded as a reference so the
    // import-aware resolver can match `fmt.Println` against the `fmt` import.
    assert!(
        parsed
            .references
            .iter()
            .any(|reference| reference.text == "fmt.Println"),
        "selector reference text must still be recorded for import resolution"
    );
}

#[test]
fn python_class_bases_filter_out_keyword_arguments() {
    let bases = python_class_bases("class Foo(Bar, metaclass=Meta, total=False)");
    assert_eq!(bases, vec!["Bar".to_string()]);
}

#[test]
fn extract_python_module_exports_requires_word_boundary() {
    use crate::ExtractContext;

    fn run_exports(source: &str) -> Vec<String> {
        let mut record_py = record("src/mod.py", source);
        record_py.language = LanguageKind::Python;
        let mut ctx = ExtractContext {
            file: record_py,
            source,
            symbols: Vec::new(),
            imports: Vec::new(),
            calls: Vec::new(),
            references: Vec::new(),
            body_hits: Vec::new(),
            diagnostics: Vec::new(),
            go_type_index: std::collections::HashMap::new(),
        };
        extract_python_module_exports(&mut ctx);
        ctx.imports
            .into_iter()
            .filter(|imp| imp.is_reexport)
            .map(|imp| imp.path)
            .collect()
    }

    let real = run_exports("__all__ = [\"foo\", \"bar\"]\n");
    assert_eq!(real, vec!["foo".to_string(), "bar".to_string()]);

    let bogus_prefixed = run_exports("__all__module = [\"value\"]\n");
    assert!(
        bogus_prefixed.is_empty(),
        "identifiers prefixed with `__all__` must not be treated as reexports"
    );

    let bogus_partial = run_exports("__all_xs = [\"value\"]\n");
    assert!(bogus_partial.is_empty());

    let plus_eq = run_exports("__all__ += [\"extra\"]\n");
    assert_eq!(plus_eq, vec!["extra".to_string()]);
}

#[test]
fn parser_reports_changed_ranges_for_cached_file() {
    let first = "fn one() { alpha(); }\n";
    let second = "fn one() { beta(); }\n";
    let mut parser = LanguageParser::new().unwrap();
    let mut record = record("src/lib.rs", first);

    let initial = parser.parse_source(&record, first.to_string()).unwrap();
    assert!(initial.changed_ranges.is_empty());

    record.hash = ContentHash::new(stable_content_hash(second.as_bytes()));
    let updated = parser.parse_source(&record, second.to_string()).unwrap();
    assert!(!updated.changed_ranges.is_empty());
    assert!(updated.calls.iter().any(|call| call.name == "beta"));
}

#[test]
fn unsupported_language_returns_structured_result() {
    let mut parser = LanguageParser::new().unwrap();
    let mut record = record("README.md", "# docs\n");
    record.language = LanguageKind::Unsupported;

    let parsed = parser
        .parse_source(&record, "# docs\n".to_string())
        .unwrap();

    assert!(parsed.unsupported.is_some());
    assert_eq!(parsed.symbols.len(), 0);
}

#[test]
fn parser_treats_non_utf8_rust_files_as_unsupported() {
    let mut parser = LanguageParser::new().unwrap();
    let mut record = record("src/lib.rs", "");
    fs::write(&record.path, b"\xff\xfe").unwrap();
    record.hash = ContentHash::new(stable_content_hash(b"\xff\xfe"));
    record.size_bytes = 2;

    let parsed = parser.parse_record(&record).unwrap();

    assert!(parsed.unsupported.is_some());
    assert!(parsed.symbols.is_empty());
}

#[test]
fn parser_classifies_associated_functions_and_unsafe_impl_names() {
    let source = r#"
pub struct Runner;

unsafe impl Send for Runner {}

impl Runner {
    pub fn new() -> Self { Runner }
    pub fn run(&self) {}
}
"#;
    let mut parser = LanguageParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();

    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| { symbol.name == "Send for Runner" && symbol.kind == SymbolKind::Impl })
    );
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| { symbol.name == "new" && symbol.kind == SymbolKind::Function })
    );
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| { symbol.name == "run" && symbol.kind == SymbolKind::Method })
    );
}

#[test]
fn parser_expands_grouped_use_trees() {
    let source = r#"
pub use crate::flags::lowargs::{ContextMode, LowArgs as Args};
use crate::{config::Config, flags::{defs::Generate, parse::*}};
"#;
    let mut parser = LanguageParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let imports = parsed
        .imports
        .iter()
        .map(|import| {
            (
                import.path.as_str(),
                import.alias.as_deref(),
                import.is_glob,
                import.is_reexport,
            )
        })
        .collect::<Vec<_>>();

    assert!(imports.contains(&("crate::flags::lowargs::ContextMode", None, false, true)));
    assert!(imports.contains(&("crate::flags::lowargs::LowArgs", Some("Args"), false, true)));
    assert!(imports.contains(&("crate::config::Config", None, false, false)));
    assert!(imports.contains(&("crate::flags::defs::Generate", None, false, false)));
    assert!(imports.contains(&("crate::flags::parse::*", None, true, false)));
}

#[test]
fn parser_parallel_records_preserve_order_and_cache_changes() {
    let mut parser = LanguageParser::new().unwrap();
    let mut records = (0..10)
        .map(|index| {
            let source = format!("pub fn f{index}() {{}}\n");
            record(&format!("src/file{index}.rs"), &source)
        })
        .collect::<Vec<_>>();

    let (parsed, summary) = parser.parse_records(&records).unwrap();

    assert_eq!(summary.parsed_files, records.len());
    assert_eq!(summary.changed_files, 0);
    for (index, parsed_file) in parsed.iter().enumerate() {
        assert!(parsed_file.changed_ranges.is_empty());
        assert!(
            parsed_file
                .symbols
                .iter()
                .any(|symbol| symbol.name == format!("f{index}"))
        );
    }

    let changed_source = "pub fn f3() { helper3(); }\nfn helper3() {}\n";
    fs::write(&records[3].path, changed_source).unwrap();
    records[3].hash = ContentHash::new(stable_content_hash(changed_source.as_bytes()));
    records[3].size_bytes = changed_source.len() as u64;

    let (updated, summary) = parser.parse_records(&records).unwrap();

    assert_eq!(summary.parsed_files, records.len());
    assert_eq!(summary.changed_files, 1);
    assert_eq!(summary.changed_ranges, updated[3].changed_ranges.len());
    assert!(updated[3].calls.iter().any(|call| call.name == "helper3"));
    for (index, parsed_file) in updated.iter().enumerate() {
        if index == 3 {
            assert!(!parsed_file.changed_ranges.is_empty());
        } else {
            assert!(parsed_file.changed_ranges.is_empty());
        }
    }
}

#[test]
fn attribute_path_extracts_leading_path_only() {
    assert_eq!(attribute_path("#[test]").as_deref(), Some("test"));
    assert_eq!(
        attribute_path("#[my_crate::test]").as_deref(),
        Some("my_crate::test")
    );
    assert_eq!(
        attribute_path("#[serde(rename = \"::test\")]").as_deref(),
        Some("serde")
    );
    assert_eq!(attribute_path("#[doc = \"x\"]").as_deref(), Some("doc"));
    assert_eq!(attribute_path("#[doc(hidden)]").as_deref(), Some("doc"));
    assert_eq!(
        attribute_path("#[derive(Document)]").as_deref(),
        Some("derive")
    );
    assert_eq!(
        attribute_path("#![allow(unused)]").as_deref(),
        Some("allow")
    );
    assert_eq!(attribute_path("not-an-attribute"), None);
    assert_eq!(attribute_path("#[]"), None);
}

#[test]
fn is_test_function_matches_only_test_attribute_paths() {
    assert!(is_test_function(&["#[test]".to_string()]));
    assert!(is_test_function(&["#[tokio::test]".to_string()]));
    assert!(is_test_function(&[
        "#[some_crate::nested::test]".to_string()
    ]));
    assert!(!is_test_function(&[
        "#[some_crate::test_helper]".to_string()
    ]));
    assert!(!is_test_function(&[
        "#[my::test_utils::install]".to_string()
    ]));
    assert!(!is_test_function(&[
        "#[serde(rename = \"::test\")]".to_string()
    ]));
    assert!(!is_test_function(&["#[cfg(test)]".to_string()]));
}

#[test]
fn docs_from_attributes_only_keeps_doc_attribute() {
    let attrs = vec![
        "#[doc = \"keep\"]".to_string(),
        "#[doc(hidden)]".to_string(),
        "#[derive(Document)]".to_string(),
        "#[serde(rename = \"doc_id\")]".to_string(),
        "#[derive(Debug, AsDocument)]".to_string(),
    ];
    let docs = docs_from_attributes(&attrs);
    assert_eq!(
        docs,
        vec![
            "#[doc = \"keep\"]".to_string(),
            "#[doc(hidden)]".to_string(),
        ]
    );
}

fn record(relative_path: &str, source: &str) -> FileRecord {
    let root = temp_root("parse-record");
    let path = root.join(relative_path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, source).unwrap();
    FileRecord {
        id: FileId::new(relative_path),
        path,
        relative_path: relative_path.to_string(),
        hash: ContentHash::new(stable_content_hash(source.as_bytes())),
        size_bytes: source.len() as u64,
        modified_unix_millis: 0,
        language: LanguageKind::Rust,
        freshness: Freshness::Fresh,
    }
}

fn python_record(relative_path: &str, source: &str) -> FileRecord {
    let mut record = record(relative_path, source);
    record.language = LanguageKind::Python;
    record
}

fn go_record(relative_path: &str, source: &str) -> FileRecord {
    let mut record = record(relative_path, source);
    record.language = LanguageKind::Go;
    record
}

fn temp_root(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("squeezy-{name}-{nonce}"));
    fs::create_dir_all(&root).unwrap();
    root
}
