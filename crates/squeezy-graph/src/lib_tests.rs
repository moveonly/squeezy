use std::{
    fs,
    path::PathBuf,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use squeezy_core::{ContentHash, FileId, LanguageKind};
use squeezy_parse::{ParsedFile, ReferenceKind, RustParser};
use squeezy_workspace::{CrawlOptions, FileRecord, stable_content_hash};

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
    @property
    def label(self):
        return "greeter"

    def greet(self, name):
        return name

class Other:
    def greet(self, name):
        return "other"
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
from services.greeter import Other
import helpers

router = APIRouter()

class Runner(GreeterAlias):
    """Routes greeting requests."""

    @router.get("/hello/{name}")
    def run(self, name: GreeterAlias) -> GreeterAlias:
        return self.label

def make():
    greeter = GreeterAlias()
    helpers.build()
    return greeter.greet("Ada")

def reassign():
    greeter = GreeterAlias()
    greeter = Other()
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
    let reassign = graph.find_symbol_by_name("reassign").pop().unwrap();
    let run = graph.find_symbol_by_name("run").pop().unwrap();
    let greeter_class = graph.find_symbol_by_name("Greeter").pop().unwrap();
    let other_class = graph.find_symbol_by_name("Other").pop().unwrap();
    let greet = graph
        .find_symbol_by_name("greet")
        .into_iter()
        .find(|symbol| symbol.parent_id.as_ref() == Some(&greeter_class.id))
        .unwrap();
    let other_greet = graph
        .find_symbol_by_name("greet")
        .into_iter()
        .find(|symbol| symbol.parent_id.as_ref() == Some(&other_class.id))
        .unwrap();
    let label = graph.find_symbol_by_name("label").pop().unwrap();
    let build = graph.find_symbol_by_name("build").pop().unwrap();

    assert!(
        run.attributes
            .contains(&"route:GET /hello/{name}".to_string())
            && run.attributes.contains(&"framework:web-route".to_string())
    );
    assert!(
        graph
            .references_to_symbol(&label.id)
            .iter()
            .any(|hit| hit.reference.text == "self.label")
    );
    assert!(graph.call_chain(&make.id, &greet.id, 2).is_some());
    assert!(graph.call_chain(&reassign.id, &other_greet.id, 2).is_some());
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
    assert_eq!(manager.build_report().language.rust_files, 1);
    assert_eq!(manager.build_report().language.supported_files, 1);

    thread::sleep(Duration::from_millis(2));
    fs::write(root.join("src").join("lib.rs"), "fn two() { beta(); }\n").unwrap();

    let report = manager.refresh_before_query().unwrap();

    assert!(report.refreshed);
    assert_eq!(report.reparsed_files, 1);
    assert_eq!(report.language.rust_files, 1);
    assert!(manager.graph().find_symbol_by_name("one").is_empty());
    assert!(!manager.graph().find_symbol_by_name("two").is_empty());
}

#[test]
fn graph_reports_indexing_policy_coverage() {
    let root = temp_root("graph-policy-coverage");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("vendor/lib")).unwrap();
    fs::write(root.join("src").join("lib.rs"), "pub fn indexed() {}\n").unwrap();
    fs::write(root.join("vendor/lib/lib.rs"), "pub fn vendored() {}\n").unwrap();
    fs::write(root.join("Cargo.lock"), "# lock\n").unwrap();

    let manager = GraphManager::open_with_crawl_options(
        &root,
        RefreshConfig::default(),
        CrawlOptions::default(),
    )
    .unwrap();

    assert!(!manager.graph().find_symbol_by_name("indexed").is_empty());
    assert!(manager.graph().find_symbol_by_name("vendored").is_empty());
    assert!(manager.build_report().excluded_files >= 2);
    assert!(
        manager
            .build_report()
            .coverage
            .reasons
            .contains_key("vendor")
    );
    assert!(
        manager
            .build_report()
            .coverage
            .reasons
            .contains_key("lockfile")
    );
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

#[test]
fn graph_binds_imported_grouped_type_references() {
    let mut parser = RustParser::new().unwrap();
    let lowargs = record(
        "crates/core/flags/lowargs.rs",
        r#"
pub enum ContextMode {
    Passthru,
}
"#,
    );
    let defs = record(
        "crates/core/flags/defs.rs",
        r#"
use crate::flags::lowargs::{ContextMode};

pub fn use_context(mode: ContextMode) {
    let _ = ContextMode::Passthru;
    let _ = mode;
}
"#,
    );
    let lowargs_parsed = parser
        .parse_source(&lowargs, fs::read_to_string(&lowargs.path).unwrap())
        .unwrap();
    let defs_parsed = parser
        .parse_source(&defs, fs::read_to_string(&defs.path).unwrap())
        .unwrap();
    let graph = SemanticGraph::from_parsed(vec![lowargs_parsed, defs_parsed]);
    let context_mode = graph.find_symbol_by_name("ContextMode").pop().unwrap();
    assert!(
        graph
            .references_to_symbol(&context_mode.id)
            .iter()
            .any(|hit| hit.reference.text == "ContextMode")
    );
}

#[test]
fn graph_binds_grouped_import_clause_to_imported_type() {
    let mut parser = RustParser::new().unwrap();
    let lowargs = record(
        "crates/core/flags/lowargs.rs",
        r#"
pub enum ContextMode {
    Passthru,
}
"#,
    );
    let defs = record(
        "crates/core/flags/defs.rs",
        r#"
use crate::flags::lowargs::{ContextMode};
"#,
    );
    let lowargs_parsed = parser
        .parse_source(&lowargs, fs::read_to_string(&lowargs.path).unwrap())
        .unwrap();
    let defs_parsed = parser
        .parse_source(&defs, fs::read_to_string(&defs.path).unwrap())
        .unwrap();
    let graph = SemanticGraph::from_parsed(vec![lowargs_parsed, defs_parsed]);
    let context_mode = graph.find_symbol_by_name("ContextMode").pop().unwrap();

    assert!(
        graph
            .references_to_symbol(&context_mode.id)
            .iter()
            .any(|hit| hit.reference.text == "ContextMode")
    );
}

#[test]
fn graph_resolves_inline_module_qualified_calls() {
    let source = r#"
fn caller() {
    convert::string();
}

mod convert {
    pub fn string() {}
}
"#;
    let mut parser = RustParser::new().unwrap();
    let record = record("src/flags/defs.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let caller = graph.find_symbol_by_name("caller").pop().unwrap();
    let string = graph.find_symbol_by_name("string").pop().unwrap();

    assert!(graph.call_chain(&caller.id, &string.id, 2).is_some());
    assert!(
        graph
            .references_to_symbol(&string.id)
            .iter()
            .any(|hit| hit.reference.text == "convert::string")
    );
}

#[test]
fn graph_binds_trait_method_impls_and_self_calls_to_trait_method() {
    let source = r#"
pub trait Decoder {
    fn decode();

    fn decode_again(&self) {
        self.decode();
    }
}

struct Concrete;

impl Decoder for Concrete {
    fn decode() {}
}

fn run() {
    Concrete::decode();
}
"#;
    let mut parser = RustParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let trait_decode = graph
        .find_symbol_by_name("decode")
        .into_iter()
        .find(|symbol| {
            symbol
                .parent_id
                .as_ref()
                .and_then(|id| graph.symbols.get(id))
                .map(|parent| parent.kind == SymbolKind::Trait)
                .unwrap_or(false)
        })
        .unwrap();
    let refs = graph.references_to_symbol(&trait_decode.id);

    assert!(
        refs.iter()
            .any(|hit| hit.reference.text == "decode" && hit.reference.span.start_byte > 100)
    );
    assert!(
        refs.iter()
            .any(|hit| hit.reference.text == "Concrete::decode")
    );
}

#[test]
fn graph_does_not_cross_bind_same_name_use_tree_siblings() {
    let source = r#"
mod a {
    pub struct Foo;
}

mod b {
    pub struct Foo;
}

use crate::{a::Foo as FA, b::Foo as FB};

fn build() -> (FA, FB) {
    let fa: FA = a::Foo;
    let fb: FB = b::Foo;
    (fa, fb)
}
"#;
    let mut parser = RustParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);

    let foos: Vec<_> = graph.find_symbol_by_name("Foo").into_iter().collect();
    assert_eq!(foos.len(), 2);

    let module_of = |sym: &GraphSymbol| {
        sym.parent_id
            .as_ref()
            .and_then(|id| graph.symbols.get(id))
            .map(|module| module.name.clone())
            .unwrap_or_default()
    };
    let foo_a = foos
        .iter()
        .find(|sym| module_of(sym) == "a")
        .expect("a::Foo");
    let foo_b = foos
        .iter()
        .find(|sym| module_of(sym) == "b")
        .expect("b::Foo");

    // Locate the byte offsets of the two `Foo` identifier tokens inside the
    // `use` clause; with the bug, both references bind to BOTH foo symbols.
    let use_start = source.find("use crate::{").expect("use clause");
    let use_end = source[use_start..].find("};").expect("end of use clause") + use_start;
    let foo_in_a_use = source[use_start..use_end]
        .find("a::Foo")
        .expect("a::Foo in use")
        + use_start
        + "a::".len();
    let foo_in_b_use = source[use_start..use_end]
        .find("b::Foo")
        .expect("b::Foo in use")
        + use_start
        + "b::".len();

    let in_use_clause_only = |hits: &[ReferenceHit]| -> Vec<u32> {
        hits.iter()
            .map(|h| h.reference.span.start_byte)
            .filter(|byte| (*byte as usize) >= use_start && (*byte as usize) < use_end)
            .collect()
    };
    let refs_a_use = in_use_clause_only(&graph.references_to_symbol(&foo_a.id));
    let refs_b_use = in_use_clause_only(&graph.references_to_symbol(&foo_b.id));

    // Critical no-cross-bind invariant: the inside-use `Foo` token from one
    // segment must NEVER bind to the other module's struct. (extract_import
    // currently records the whole `use_declaration` span on every flattened
    // import, so without the collision guard both inside-segment references
    // would bind to both Foo symbols.)
    assert!(
        !refs_a_use.contains(&(foo_in_b_use as u32)),
        "a::Foo must not be bound by the `Foo` token inside the b::Foo segment"
    );
    assert!(
        !refs_b_use.contains(&(foo_in_a_use as u32)),
        "b::Foo must not be bound by the `Foo` token inside the a::Foo segment"
    );
}

#[test]
fn graph_does_not_bind_impl_decl_across_same_name_traits_in_other_modules() {
    let source = r#"
mod a {
    pub trait Decoder {
        fn decode();
    }
}

mod b {
    pub trait Decoder {
        fn decode();
    }
}

struct Concrete;

impl crate::a::Decoder for Concrete {
    fn decode() {}
}
"#;
    let mut parser = RustParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);

    let trait_methods: Vec<_> = graph
        .find_symbol_by_name("decode")
        .into_iter()
        .filter(|symbol| {
            symbol
                .parent_id
                .as_ref()
                .and_then(|id| graph.symbols.get(id))
                .map(|parent| parent.kind == SymbolKind::Trait)
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(trait_methods.len(), 2);

    let module_of = |trait_method: &GraphSymbol| {
        trait_method
            .parent_id
            .as_ref()
            .and_then(|id| graph.symbols.get(id))
            .and_then(|trait_sym| trait_sym.parent_id.as_ref())
            .and_then(|id| graph.symbols.get(id))
            .map(|module| module.name.clone())
            .unwrap_or_default()
    };
    let trait_a = trait_methods
        .iter()
        .find(|sym| module_of(sym) == "a")
        .expect("trait a::Decoder::decode");
    let trait_b = trait_methods
        .iter()
        .find(|sym| module_of(sym) == "b")
        .expect("trait b::Decoder::decode");

    let refs_a = graph.references_to_symbol(&trait_a.id);
    let refs_b = graph.references_to_symbol(&trait_b.id);

    assert!(
        refs_a
            .iter()
            .any(|hit| hit.reference.text == "decode" && hit.reference.span.start_byte > 80),
        "impl decode declaration should bind to a::Decoder::decode"
    );
    assert!(
        !refs_b
            .iter()
            .any(|hit| hit.reference.text == "decode" && hit.reference.span.start_byte > 80),
        "impl decode declaration must NOT cross-bind to b::Decoder::decode"
    );
}

#[test]
fn graph_skips_impl_decl_with_multiline_cfg_attribute() {
    let source = r#"
pub trait Decoder {
    fn decode();
}

struct Concrete;

impl Decoder for Concrete {
    #[cfg(any(
        feature = "x",
        feature = "y",
    ))]
    fn decode() {}
}
"#;
    let mut parser = RustParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let trait_decode = graph
        .find_symbol_by_name("decode")
        .into_iter()
        .find(|symbol| {
            symbol
                .parent_id
                .as_ref()
                .and_then(|id| graph.symbols.get(id))
                .map(|parent| parent.kind == SymbolKind::Trait)
                .unwrap_or(false)
        })
        .unwrap();
    let refs = graph.references_to_symbol(&trait_decode.id);

    assert!(
        !refs
            .iter()
            .any(|hit| hit.reference.text == "decode" && hit.reference.span.start_byte > 90),
        "cfg-gated impl decode declaration must not bind to the trait method"
    );
}

#[test]
fn graph_binds_uppercase_struct_constructor_references() {
    let source = r#"
struct Generate;

fn flags() {
    let _ = &Generate;
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
            .any(|hit| hit.reference.text == "Generate")
    );
}

#[test]
fn graph_does_not_bind_prelude_variant_names_to_shadow_structs() {
    let source = r#"
struct None;

fn option() -> Option<u8> {
    None
}
"#;
    let mut parser = RustParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let none = graph.find_symbol_by_name("None").pop().unwrap();

    assert!(graph.references_to_symbol(&none.id).is_empty());
}

#[test]
fn graph_binds_trait_owned_self_associated_type_references() {
    let source = r#"
pub trait IntoThing {
    type Output;

    fn convert(self) -> Self::Output;
}
"#;
    let mut parser = RustParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let output = graph.find_symbol_by_name("Output").pop().unwrap();

    assert!(
        graph
            .references_to_symbol(&output.id)
            .iter()
            .any(|hit| hit.reference.text == "Self::Output")
    );
}

#[test]
fn graph_binds_trait_qualified_associated_type_to_trait_item() {
    let source = r#"
pub trait IntoThing {
    type Output;
}

struct Local;

impl IntoThing for Local {
    type Output = Local;
}

pub fn consume(_: IntoThing::Output) {}
"#;
    let mut parser = RustParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let trait_output = graph
        .find_symbol_by_name("Output")
        .into_iter()
        .find(|symbol| {
            symbol
                .parent_id
                .as_ref()
                .and_then(|id| graph.symbols.get(id))
                .map(|parent| parent.kind == SymbolKind::Trait)
                .unwrap_or(false)
        })
        .unwrap();
    let impl_output = graph
        .find_symbol_by_name("Output")
        .into_iter()
        .find(|symbol| {
            symbol
                .parent_id
                .as_ref()
                .and_then(|id| graph.symbols.get(id))
                .map(|parent| parent.kind == SymbolKind::Impl)
                .unwrap_or(false)
        })
        .unwrap();

    assert!(
        graph
            .references_to_symbol(&trait_output.id)
            .iter()
            .any(|hit| hit.reference.text == "IntoThing::Output")
    );
    assert!(
        graph
            .references_to_symbol(&impl_output.id)
            .iter()
            .all(|hit| hit.reference.text != "IntoThing::Output")
    );
}

#[test]
fn graph_does_not_bind_impl_self_projection_to_impl_associated_type() {
    let source = r#"
pub trait IntoThing {
    type Output;
}

struct Local;

impl IntoThing for Local {
    type Output = Local;

    fn convert(self) -> Self::Output {
        Local
    }
}
"#;
    let mut parser = RustParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let graph = SemanticGraph::from_parsed(vec![parsed]);
    let impl_output = graph
        .find_symbol_by_name("Output")
        .into_iter()
        .find(|symbol| {
            symbol
                .parent_id
                .as_ref()
                .and_then(|id| graph.symbols.get(id))
                .map(|parent| parent.kind == SymbolKind::Impl)
                .unwrap_or(false)
        })
        .unwrap();

    assert!(
        graph
            .references_to_symbol(&impl_output.id)
            .iter()
            .all(|hit| hit.reference.text != "Self::Output")
    );
}

#[test]
fn annotate_dirty_ranges_marks_only_intersecting_symbols_and_clears_on_reapply() {
    let source = "pub fn first() -> usize { 1 }\npub fn second() -> usize { 2 }\npub fn third() -> usize { 3 }\n";
    let mut parser = RustParser::new().unwrap();
    let record = record("src/lib.rs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let mut graph = SemanticGraph::from_parsed(vec![parsed]);

    let mut dirty = HashMap::new();
    dirty.insert(
        FileId::new("src/lib.rs"),
        DirtyAnnotation {
            status: "modified".to_string(),
            ranges: vec![DirtyRange {
                start_line: 1,
                end_line: 1,
            }],
        },
    );
    graph.annotate_dirty_ranges(&dirty);

    let dirty_names = graph
        .dirty_symbols()
        .into_iter()
        .map(|symbol| symbol.name)
        .collect::<Vec<_>>();
    assert_eq!(dirty_names, vec!["second".to_string()]);
    assert!(
        graph
            .dirty_symbols()
            .iter()
            .all(|symbol| symbol.kind != SymbolKind::File)
    );

    dirty.clear();
    dirty.insert(
        FileId::new("src/lib.rs"),
        DirtyAnnotation {
            status: "modified".to_string(),
            ranges: vec![DirtyRange {
                start_line: 2,
                end_line: 2,
            }],
        },
    );
    graph.annotate_dirty_ranges(&dirty);
    let dirty_names = graph
        .dirty_symbols()
        .into_iter()
        .map(|symbol| symbol.name)
        .collect::<Vec<_>>();
    assert_eq!(dirty_names, vec!["third".to_string()]);

    graph.annotate_dirty_ranges(&HashMap::new());
    assert!(graph.dirty_symbols().is_empty());
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
