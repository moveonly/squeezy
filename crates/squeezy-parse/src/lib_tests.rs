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
    let mut parser = RustParser::new().unwrap();
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
    let mut parser = RustParser::new().unwrap();
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
fn parser_reports_changed_ranges_for_cached_file() {
    let first = "fn one() { alpha(); }\n";
    let second = "fn one() { beta(); }\n";
    let mut parser = RustParser::new().unwrap();
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
    let mut parser = RustParser::new().unwrap();
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
    let mut parser = RustParser::new().unwrap();
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
    let mut parser = RustParser::new().unwrap();
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
fn parser_parallel_records_preserve_order_and_cache_changes() {
    let mut parser = RustParser::new().unwrap();
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

fn temp_root(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("squeezy-{name}-{nonce}"));
    fs::create_dir_all(&root).unwrap();
    root
}
