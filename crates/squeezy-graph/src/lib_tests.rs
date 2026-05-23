use std::{
    fs,
    path::PathBuf,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use squeezy_core::{ContentHash, FileId, LanguageKind};
use squeezy_parse::{ParsedFile, ReferenceKind, RustParser};
use squeezy_workspace::{FileRecord, stable_content_hash};

use super::*;

#[test]
fn graph_answers_hierarchy_signature_body_reference_and_call_queries() {
    let source = r#"
pub struct Runner;

impl Runner {
    pub fn run(&self) {
        helper();
    }
}

fn helper() {}
"#;
    let mut parser = RustParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);

    assert!(graph.stats().symbols >= 4);
    assert!(
        graph
            .hierarchy(None, 4)
            .iter()
            .any(|node| node.name == "src/lib.rs")
    );
    assert_eq!(
        graph
            .signature_search(&SignatureQuery {
                text: "pub fn run".to_string(),
                kind: Some(SymbolKind::Method),
                visibility: None,
                attribute: None,
            })
            .len(),
        1
    );
    let body_hits = graph.body_search(&BodySearchQuery {
        text: "helper".to_string(),
        owner_kind: Some(SymbolKind::Method),
        hit_kind: None,
    });
    assert!(body_hits.iter().any(|hit| hit.hit.text == "helper"));
    assert!(!graph.reference_search("Runner").is_empty());

    let run = graph.find_symbol_by_name("run").pop().unwrap();
    let helper = graph.find_symbol_by_name("helper").pop().unwrap();
    assert!(graph.call_chain(&run.id, &helper.id, 3).is_some());
}

#[test]
fn graph_answers_python_navigation_queries() {
    let source = r#"
class Greeter:
    def greet(self, name):
        return name

def make():
    greeter = Greeter()
    return greeter.greet("Ada")
"#;
    let mut parser = RustParser::new().unwrap();
    let record = python_record("app.py", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);

    assert!(
        graph
            .signature_search(&SignatureQuery {
                text: "class Greeter".to_string(),
                kind: Some(SymbolKind::Class),
                visibility: None,
                attribute: None,
            })
            .iter()
            .any(|symbol| symbol.name == "Greeter")
    );
    let make = graph.find_symbol_by_name("make").pop().unwrap();
    let greeter = graph.find_symbol_by_name("Greeter").pop().unwrap();
    assert!(graph.call_chain(&make.id, &greeter.id, 2).is_some());
    assert!(!graph.reference_search("Greeter").is_empty());
}

#[test]
fn graph_uses_python_navigation_heuristics() {
    let mut parser = RustParser::new().unwrap();
    let greeter = python_record(
        "services/greeter.py",
        r#"
class Greeter:
    def greet(self, name):
        return name
"#,
    );
    let helpers = python_record(
        "helpers.py",
        r#"
def build():
    return "Ada"
"#,
    );
    let app = python_record(
        "app.py",
        r#"
from services.greeter import Greeter as GreeterAlias
import helpers

router = APIRouter()

class Runner(GreeterAlias):
    """Routes greeting requests."""

    @router.get("/hello")
    def run(self, name: GreeterAlias) -> GreeterAlias:
        return self.greet(name)

def make():
    greeter = GreeterAlias()
    helpers.build()
    return greeter.greet("Ada")
"#,
    );
    let parsed = vec![
        parser
            .parse_source(&greeter, fs::read_to_string(&greeter.path).unwrap())
            .unwrap(),
        parser
            .parse_source(&helpers, fs::read_to_string(&helpers.path).unwrap())
            .unwrap(),
        parser
            .parse_source(&app, fs::read_to_string(&app.path).unwrap())
            .unwrap(),
    ];
    let graph = SemanticGraph::from_parsed(parsed);

    let make = graph.find_symbol_by_name("make").pop().unwrap();
    let run = graph.find_symbol_by_name("run").pop().unwrap();
    let greet = graph.find_symbol_by_name("greet").pop().unwrap();
    let build = graph.find_symbol_by_name("build").pop().unwrap();
    let greeter_class = graph.find_symbol_by_name("Greeter").pop().unwrap();

    assert!(
        run.attributes.contains(&"route:GET".to_string())
            && run.attributes.contains(&"framework:web-route".to_string())
    );
    assert!(graph.call_chain(&run.id, &greet.id, 2).is_some());
    assert!(graph.call_chain(&make.id, &greet.id, 2).is_some());
    assert!(graph.call_chain(&make.id, &build.id, 2).is_some());
    assert!(graph.call_chain(&make.id, &greeter_class.id, 2).is_some());
    assert!(
        graph
            .references_to_symbol(&greeter_class.id)
            .iter()
            .any(|hit| hit.reference.kind == ReferenceKind::Type)
    );
}

#[test]
fn graph_manager_refresh_replaces_changed_file_only() {
    let root = temp_root("graph-manager-refresh");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src").join("lib.rs"), "fn one() { alpha(); }\n").unwrap();

    let mut manager = GraphManager::open_with_config(
        &root,
        RefreshConfig {
            debounce: Duration::from_millis(0),
            idle_refresh_interval: Duration::from_millis(0),
            per_tool_refresh_budget: Duration::from_secs(5),
        },
    )
    .unwrap();
    assert!(!manager.graph().find_symbol_by_name("one").is_empty());

    thread::sleep(Duration::from_millis(2));
    fs::write(root.join("src").join("lib.rs"), "fn two() { beta(); }\n").unwrap();

    let report = manager.refresh_before_query().unwrap();

    assert!(report.refreshed);
    assert_eq!(report.reparsed_files, 1);
    assert!(manager.graph().find_symbol_by_name("one").is_empty());
    assert!(!manager.graph().find_symbol_by_name("two").is_empty());
}

#[test]
fn graph_filters_unsupported_files_from_hierarchy() {
    let mut readme = record("README.md", "# docs\n");
    readme.language = LanguageKind::Unsupported;
    let graph = SemanticGraph::from_parsed(vec![ParsedFile::unsupported(readme, "markdown")]);

    assert_eq!(graph.stats().files, 1);
    assert_eq!(graph.stats().symbols, 0);
    assert!(graph.hierarchy(None, 4).is_empty());
}

#[test]
fn graph_supports_callers_callees_and_removal() {
    let source = r#"
pub fn alpha() -> usize {
    beta()
}

fn beta() -> usize {
    1
}
"#;
    let mut parser = RustParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let file_id = record.id.clone();
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let mut graph = SemanticGraph::from_parsed(vec![parsed]);

    let alpha = graph.find_symbol_by_name("alpha").pop().unwrap();
    let beta = graph.find_symbol_by_name("beta").pop().unwrap();
    assert_eq!(graph.callees(&alpha.id).len(), 1);
    assert_eq!(graph.callers(&beta.id).len(), 1);
    assert!(
        graph
            .signature_search(&SignatureQuery {
                text: "pub fn alpha".to_string(),
                kind: Some(SymbolKind::Function),
                visibility: None,
                attribute: None,
            })
            .iter()
            .any(|symbol| symbol.name == "alpha")
    );

    graph.remove_file(&file_id);

    assert!(graph.find_symbol_by_name("alpha").is_empty());
    assert!(graph.edges().is_empty());
}

#[test]
fn graph_binds_references_to_selected_same_name_symbol() {
    let mut parser = RustParser::new().unwrap();
    let first = record(
        "src/first.rs",
        r#"
pub fn target() {}

pub fn caller() {
    target();
}
"#,
    );
    let second = record(
        "src/second.rs",
        r#"
pub fn target() {}

pub fn caller() {
    target();
}
"#,
    );
    let first_parsed = parser
        .parse_source(&first, fs::read_to_string(&first.path).unwrap())
        .unwrap();
    let second_parsed = parser
        .parse_source(&second, fs::read_to_string(&second.path).unwrap())
        .unwrap();
    let graph = SemanticGraph::from_parsed(vec![first_parsed, second_parsed]);
    let mut targets = graph.find_symbol_by_name("target");
    targets.sort_by(|left, right| left.file_id.0.cmp(&right.file_id.0));

    let first_refs = graph.references_to_symbol(&targets[0].id);
    let second_refs = graph.references_to_symbol(&targets[1].id);

    assert!(graph.reference_search("target").len() > first_refs.len());
    assert!(
        first_refs
            .iter()
            .all(|hit| hit.reference.file_id.0 == "src/first.rs")
    );
    assert!(
        second_refs
            .iter()
            .all(|hit| hit.reference.file_id.0 == "src/second.rs")
    );
}

#[test]
fn graph_does_not_bind_external_receiver_method_to_unique_local_method() {
    let source = r#"
pub struct Local;

impl Local {
    pub fn get(&self) {}
}

pub fn caller(map: std::collections::HashMap<String, String>) {
    map.get("key");
}
"#;
    let mut parser = RustParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let caller = graph.find_symbol_by_name("caller").pop().unwrap();

    assert!(
        graph.callees(&caller.id).iter().all(|hit| hit
            .callee
            .as_ref()
            .map(|symbol| symbol.name.as_str())
            != Some("get"))
    );
}

#[test]
fn graph_does_not_bind_value_identifier_to_same_name_function() {
    let source = r#"
fn lookup() {}

fn caller() {
    let lookup = 1;
    let _ = lookup;
}
"#;
    let mut parser = RustParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let lookup = graph.find_symbol_by_name("lookup").pop().unwrap();

    assert!(
        graph
            .references_to_symbol(&lookup.id)
            .iter()
            .all(|hit| hit.reference.span.start_byte < lookup.body_span.unwrap().start_byte)
    );
}

#[test]
fn graph_does_not_bind_enum_variant_path_to_same_name_struct() {
    let source = r#"
struct Generate;

enum Mode {
    Generate,
}

fn caller() {
    let _ = Mode::Generate;
}
"#;
    let mut parser = RustParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let generate = graph.find_symbol_by_name("Generate").pop().unwrap();

    assert!(
        graph
            .references_to_symbol(&generate.id)
            .iter()
            .all(|hit| hit.reference.text != "Mode::Generate")
    );
}

#[test]
fn graph_declaration_match_ignores_same_name_signature_parameters() {
    let source = r#"
trait Sink {
    fn finish(&mut self, finish: &usize);
}
"#;
    let mut parser = RustParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let finish = graph.find_symbol_by_name("finish").pop().unwrap();

    assert!(graph.references_to_symbol(&finish.id).is_empty());
}

#[test]
fn graph_symbol_references_are_package_local_until_cargo_resolution_exists() {
    let mut parser = RustParser::new().unwrap();
    let source_package = record("crates/source/src/lib.rs", "pub struct Shared;\n");
    let user_package = record(
        "crates/user/src/lib.rs",
        r#"
use source::Shared;

pub fn user(_: Shared) {}
"#,
    );
    let source_parsed = parser
        .parse_source(
            &source_package,
            fs::read_to_string(&source_package.path).unwrap(),
        )
        .unwrap();
    let user_parsed = parser
        .parse_source(
            &user_package,
            fs::read_to_string(&user_package.path).unwrap(),
        )
        .unwrap();
    let graph = SemanticGraph::from_parsed(vec![source_parsed, user_parsed]);
    let shared = graph.find_symbol_by_name("Shared").pop().unwrap();

    assert!(
        graph.references_to_symbol(&shared.id).iter().all(|hit| hit
            .reference
            .file_id
            .0
            .starts_with("crates/source/"))
    );
}

#[test]
fn graph_does_not_bind_external_std_path_to_local_type() {
    let source = r#"
struct IntoIter;

fn caller() -> std::vec::IntoIter<u8> {
    todo!()
}
"#;
    let mut parser = RustParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let into_iter = graph.find_symbol_by_name("IntoIter").pop().unwrap();

    assert!(graph.references_to_symbol(&into_iter.id).is_empty());
}

#[test]
fn graph_resolves_module_qualified_direct_calls() {
    let mut parser = RustParser::new().unwrap();
    let output = record("src/output.rs", "pub fn print_entry() {}\n");
    let walk = record(
        "src/walk.rs",
        r#"
use crate::output;

pub fn scan() {
    output::print_entry();
}
"#,
    );
    let output_parsed = parser
        .parse_source(&output, fs::read_to_string(&output.path).unwrap())
        .unwrap();
    let walk_parsed = parser
        .parse_source(&walk, fs::read_to_string(&walk.path).unwrap())
        .unwrap();
    let graph = SemanticGraph::from_parsed(vec![output_parsed, walk_parsed]);
    let scan = graph.find_symbol_by_name("scan").pop().unwrap();
    let print_entry = graph.find_symbol_by_name("print_entry").pop().unwrap();

    assert!(graph.call_chain(&scan.id, &print_entry.id, 2).is_some());
    assert!(
        graph
            .references_to_symbol(&print_entry.id)
            .iter()
            .any(|hit| hit.reference.text == "output::print_entry")
    );
}

#[test]
fn graph_resolves_type_qualified_associated_functions() {
    let source = r#"
pub struct Command;

impl Command {
    pub fn new() -> Self {
        Command
    }
}

pub fn caller() {
    Command::new();
}
"#;
    let mut parser = RustParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let caller = graph.find_symbol_by_name("caller").pop().unwrap();
    let new = graph.find_symbol_by_name("new").pop().unwrap();

    assert!(graph.call_chain(&caller.id, &new.id, 2).is_some());
    assert!(
        graph
            .references_to_symbol(&new.id)
            .iter()
            .any(|hit| hit.reference.text == "Command::new")
    );
}

fn record(relative_path: &str, source: &str) -> FileRecord {
    let root = temp_root("graph-record");
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
