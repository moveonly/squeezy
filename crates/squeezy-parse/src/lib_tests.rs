use crate::languages::{python::extract_python_module_exports, rust::*};
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
fn parser_accepts_csharp_and_tracks_cached_changes() {
    let first = "namespace Demo;\nclass Runner { void Run() { } }\n";
    let second = "namespace Demo;\nclass Runner { void Run() { System.Console.WriteLine(1); } }\n";
    let mut parser = RustParser::new().unwrap();
    let mut record = csharp_record("src/Runner.cs", first);

    let initial = parser.parse_source(&record, first.to_string()).unwrap();
    assert!(initial.unsupported.is_none());
    assert!(initial.diagnostics.is_empty());
    assert!(initial.changed_ranges.is_empty());
    assert!(
        initial
            .symbols
            .iter()
            .any(|symbol| symbol.name == "Demo" && symbol.kind == SymbolKind::Module)
    );
    assert!(
        initial
            .symbols
            .iter()
            .any(|symbol| symbol.name == "Runner" && symbol.kind == SymbolKind::Class)
    );
    assert!(
        initial
            .symbols
            .iter()
            .any(|symbol| symbol.name == "Run" && symbol.kind == SymbolKind::Method)
    );

    record.hash = ContentHash::new(stable_content_hash(second.as_bytes()));
    record.size_bytes = second.len() as u64;
    let updated = parser.parse_source(&record, second.to_string()).unwrap();

    assert!(updated.unsupported.is_none());
    assert!(updated.diagnostics.is_empty());
    assert!(!updated.changed_ranges.is_empty());
    assert!(updated.calls.iter().any(|call| call.name == "WriteLine"));
}

#[test]
fn csharp_parser_extracts_symbols_imports_calls_and_references() {
    let source = r#"
using System;
using System.Collections.Generic;
using static System.Math;
using Json = System.Text.Json.JsonSerializer;

namespace Squeezy.CSharp.SemanticCases;

public interface IRunner
{
    string Run(string input);
}

public partial record Runner(string Prefix) : IRunner
{
    public List<string> History { get; init; } = new();

    public string Run(string input)
    {
        var formatted = Format(input);
        History.Add(formatted);
        return Json.Serialize(formatted);
    }
}

public partial record Runner
{
    public string Format(string input) => $"{Prefix}:{Abs(input.Length)}";
}
"#;
    let mut parser = RustParser::new().unwrap();
    let record = csharp_record("src/Runner.cs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();

    assert!(parsed.unsupported.is_none());
    assert!(parsed.diagnostics.is_empty());

    let namespace_symbol = parsed
        .symbols
        .iter()
        .find(|symbol| {
            symbol.kind == SymbolKind::Module && symbol.name == "Squeezy.CSharp.SemanticCases"
        })
        .expect("namespace symbol");
    assert!(
        namespace_symbol
            .attributes
            .iter()
            .any(|attribute| attribute == "csharp:namespace")
    );

    assert!(parsed.symbols.iter().any(|symbol| {
        symbol.name == "IRunner"
            && symbol.kind == SymbolKind::Interface
            && symbol.visibility.as_deref() == Some("public")
    }));
    let runners = parsed
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Struct && symbol.name == "Runner")
        .collect::<Vec<_>>();
    assert_eq!(
        runners.len(),
        2,
        "both partial record declarations recorded"
    );
    assert!(runners.iter().all(|symbol| {
        symbol
            .attributes
            .iter()
            .any(|attr| attr == "csharp:partial")
    }));
    assert!(parsed.symbols.iter().any(|symbol| symbol.name == "Run"
        && symbol.kind == SymbolKind::Method
        && symbol.visibility.as_deref() == Some("public")));
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "Format" && symbol.kind == SymbolKind::Method)
    );
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "History" && symbol.kind == SymbolKind::Field)
    );

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
    assert!(imports.contains(&("System", None, false, false)));
    assert!(imports.contains(&("System.Collections.Generic", None, false, false)));
    assert!(imports.contains(&("System.Math.*", None, true, false)));
    assert!(imports.contains(&(
        "System.Text.Json.JsonSerializer",
        Some("Json"),
        false,
        false
    )));

    assert!(parsed.calls.iter().any(|call| call.name == "Format"));
    assert!(parsed.calls.iter().any(|call| call.name == "Add"
        && call.kind == ParsedCallKind::Method
        && call.receiver.as_deref() == Some("History")));
    assert!(parsed.calls.iter().any(|call| call.name == "Serialize"
        && call.kind == ParsedCallKind::Method
        && call.receiver.as_deref() == Some("Json")));
    assert!(parsed.calls.iter().any(|call| call.name == "Abs"));

    assert!(
        parsed
            .references
            .iter()
            .any(|reference| reference.text == "IRunner" && reference.kind == ReferenceKind::Type)
    );
    assert!(
        !parsed
            .references
            .iter()
            .any(|reference| reference.text == "string"),
        "predefined types must not pollute references",
    );
}

#[test]
fn csharp_parser_marks_test_methods_via_attributes_and_filenames() {
    let source = r#"
using Xunit;
using NUnit.Framework;

namespace Demo.Tests;

public class RunnerTests
{
    [Fact]
    public void Runs_inputs_through_the_runner() { }

    [Test]
    public void Nunit_test_marker() { }

    public void NotATest() { }
}
"#;
    let mut parser = RustParser::new().unwrap();
    let record = csharp_record("tests/RunnerTests.cs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();

    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "Runs_inputs_through_the_runner"
                && symbol.kind == SymbolKind::Test
                && symbol.attributes.iter().any(|attr| attr == "csharp:test"))
    );
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "Nunit_test_marker"
                && symbol.kind == SymbolKind::Test
                && symbol.attributes.iter().any(|attr| attr == "csharp:test"))
    );
    let not_test = parsed
        .symbols
        .iter()
        .find(|symbol| symbol.name == "NotATest")
        .expect("non-test method symbol");
    assert_ne!(not_test.kind, SymbolKind::Test);
    assert!(
        not_test
            .attributes
            .iter()
            .any(|attribute| attribute == "csharp:test-host"),
        "methods in *Tests.cs files should be marked as test hosts even without attributes",
    );
}

#[test]
fn csharp_parser_emits_base_attributes_for_class_hierarchies() {
    let source = r#"
namespace App;

public class Animal { public virtual void Speak() { } }

public class Dog : Animal, IComparable<Dog>
{
    public override void Speak() { base.Speak(); }
}
"#;
    let mut parser = RustParser::new().unwrap();
    let record = csharp_record("src/Animals.cs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();

    let dog = parsed
        .symbols
        .iter()
        .find(|symbol| symbol.name == "Dog")
        .expect("Dog symbol");
    assert!(
        dog.attributes.iter().any(|attr| attr == "base:Animal"),
        "C# class inheritance should produce `base:` attributes for graph resolution",
    );
    assert!(
        dog.attributes.iter().any(|attr| attr == "base:IComparable"),
        "Generic base names should be stripped to their leaf identifier",
    );
}

#[test]
fn csharp_parser_records_route_attributes_for_aspnet_controllers() {
    let source = r#"
using Microsoft.AspNetCore.Mvc;

namespace App;

[ApiController]
[Route("api/[controller]")]
public class UsersController : ControllerBase
{
    [HttpGet("{id}")]
    public IActionResult Get(int id) => Ok(id);
}
"#;
    let mut parser = RustParser::new().unwrap();
    let record = csharp_record("src/Controllers/UsersController.cs", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();

    let controller = parsed
        .symbols
        .iter()
        .find(|symbol| symbol.name == "UsersController")
        .expect("controller symbol");
    assert!(
        controller
            .attributes
            .iter()
            .any(|attribute| attribute == "framework:aspnet"),
    );
    assert!(
        controller
            .attributes
            .iter()
            .any(|attribute| attribute == "framework:web-route"),
    );
    let get = parsed
        .symbols
        .iter()
        .find(|symbol| symbol.name == "Get")
        .expect("Get method");
    assert!(
        get.attributes
            .iter()
            .any(|attribute| attribute == "route:GET"),
    );
    assert!(
        get.attributes
            .iter()
            .any(|attribute| attribute == "route:GET {id}"),
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
fn parser_extracts_java_symbols_imports_calls_and_references() {
    let source = r#"
package com.example.app;

import com.example.services.Greeter;
import static com.example.util.Names.defaultName;

public class Runner extends BaseRunner implements Runnable {
    private final Greeter greeter;

    public Runner(Greeter greeter) {
        this.greeter = greeter;
    }

    @Override
    public void run() {
        String name = defaultName();
        greeter.greet(name);
        new Helper().assist();
    }
}

record Helper(String name) {
    void assist() {}
}

@interface Route {
    String value();
}
"#;
    let mut parser = RustParser::new().unwrap();
    let record = java_record("src/main/java/com/example/app/Runner.java", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();

    assert!(parsed.unsupported.is_none());
    assert!(parsed.imports.iter().any(|import| {
        import.path == "com.example.app" && import.alias.as_deref() == Some("__java_package__")
    }));
    assert!(
        parsed
            .imports
            .iter()
            .any(|import| import.path == "com.example.services.Greeter")
    );
    assert!(parsed.symbols.iter().any(|symbol| {
        symbol.name == "Runner"
            && symbol.kind == SymbolKind::Class
            && symbol.visibility.as_deref() == Some("public")
            && symbol.attributes.contains(&"base:BaseRunner".to_string())
            && symbol.attributes.contains(&"base:Runnable".to_string())
    }));
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "Helper" && symbol.kind == SymbolKind::Struct)
    );
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "greeter" && symbol.kind == SymbolKind::Field)
    );
    assert!(
        parsed
            .calls
            .iter()
            .any(|call| call.name == "defaultName" && call.kind == ParsedCallKind::Method)
    );
    assert!(
        parsed
            .calls
            .iter()
            .any(|call| call.name == "greet" && call.receiver.as_deref() == Some("greeter"))
    );
    assert!(
        parsed
            .calls
            .iter()
            .any(|call| call.name == "Helper" && call.kind == ParsedCallKind::Direct)
    );
    assert!(
        parsed
            .references
            .iter()
            .any(|reference| reference.text == "Greeter" && reference.kind == ReferenceKind::Type)
    );
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "value" && symbol.kind == SymbolKind::Method)
    );
}

#[test]
fn parser_extracts_kotlin_package_imports_classes_and_calls() {
    let source = r#"package com.example.app

import com.example.services.Greeter
import com.example.services.FriendlyGreeter as Friendly
import kotlin.text.*

class Runner(private val greeter: Greeter) {
    suspend fun run() {
        val name = greeter.greet()
        Friendly.create()
    }
}

fun String.prepare(): String = this.trim()

object StringOps {
    fun normalize(s: String): String = s.lowercase()
}
"#;
    let mut parser = LanguageParser::new().unwrap();
    let record = kotlin_record("src/main/kotlin/com/example/app/Runner.kt", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();

    assert!(parsed.unsupported.is_none(), "expected supported parse");
    assert_eq!(parsed.package.as_deref(), Some("com.example.app"));
    // Package marker import.
    assert!(parsed.imports.iter().any(|import| {
        import.path == "com.example.app" && import.alias.as_deref() == Some("__kotlin_package__")
    }));
    // Aliased import.
    assert!(parsed.imports.iter().any(|import| {
        import.path == "com.example.services.FriendlyGreeter"
            && import.alias.as_deref() == Some("Friendly")
    }));
    // Wildcard import.
    assert!(
        parsed
            .imports
            .iter()
            .any(|import| import.is_glob && import.path == "kotlin.text"),
    );

    // Class with primary-constructor field promotion.
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| { symbol.name == "Runner" && symbol.kind == SymbolKind::Class })
    );
    assert!(parsed.symbols.iter().any(|symbol| {
        symbol.name == "greeter"
            && symbol.kind == SymbolKind::Field
            && symbol
                .attributes
                .contains(&"kotlin:ctor_property".to_string())
    }));
    // Suspend function.
    assert!(parsed.symbols.iter().any(|symbol| {
        symbol.name == "run"
            && symbol.kind == SymbolKind::Method
            && symbol.attributes.contains(&"kotlin:suspend".to_string())
    }));
    // Extension function with receiver captured into language_identity.
    let prepare = parsed
        .symbols
        .iter()
        .find(|symbol| symbol.name == "prepare")
        .expect("prepare extension function");
    assert_eq!(prepare.kind, SymbolKind::Function);
    assert_eq!(prepare.language_identity.as_deref(), Some("String"));
    assert!(prepare.attributes.contains(&"kotlin:extension".to_string()));

    // Object declaration tagged.
    let string_ops = parsed
        .symbols
        .iter()
        .find(|symbol| symbol.name == "StringOps")
        .expect("StringOps object");
    assert_eq!(string_ops.kind, SymbolKind::Class);
    assert!(string_ops.attributes.contains(&"kotlin:object".to_string()));

    // Calls: navigation-form should be Method; bare-name should be Direct.
    assert!(parsed.calls.iter().any(|call| call.name == "greet"
        && call.kind == ParsedCallKind::Method
        && call.receiver.as_deref() == Some("greeter")));
}

#[test]
fn parser_handles_kotlin_data_class_and_sealed_interface() {
    let source = r#"package com.example.services

sealed interface Greeter {
    fun greet(): String
}

class FriendlyGreeter : Greeter {
    companion object {
        fun create(): FriendlyGreeter = FriendlyGreeter()
    }
    override fun greet(): String = "hi"
}

data class Person(val name: String, val age: Int)
"#;
    let mut parser = LanguageParser::new().unwrap();
    let record = kotlin_record("src/main/kotlin/com/example/services/Greeter.kt", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    assert!(parsed.unsupported.is_none());

    // sealed interface -> Trait + kotlin:sealed attribute
    let greeter = parsed
        .symbols
        .iter()
        .find(|symbol| symbol.name == "Greeter")
        .expect("Greeter trait");
    assert_eq!(greeter.kind, SymbolKind::Trait);
    assert!(greeter.attributes.contains(&"kotlin:sealed".to_string()));

    // delegation base recorded on FriendlyGreeter
    let friendly = parsed
        .symbols
        .iter()
        .find(|symbol| symbol.name == "FriendlyGreeter")
        .expect("FriendlyGreeter class");
    assert_eq!(friendly.kind, SymbolKind::Class);
    assert!(friendly.attributes.contains(&"base:Greeter".to_string()));

    // companion-object child re-parented to host class
    let create = parsed
        .symbols
        .iter()
        .find(|symbol| symbol.name == "create")
        .expect("create method");
    assert_eq!(create.kind, SymbolKind::Method);
    assert_eq!(create.parent_id.as_ref(), Some(&friendly.id));

    // data class flag
    let person = parsed
        .symbols
        .iter()
        .find(|symbol| symbol.name == "Person")
        .expect("Person class");
    assert!(person.attributes.contains(&"kotlin:data".to_string()));
    // primary-constructor fields promoted
    assert!(parsed.symbols.iter().any(|symbol| {
        symbol.name == "name"
            && symbol.kind == SymbolKind::Field
            && symbol.parent_id.as_ref() == Some(&person.id)
    }));
}

#[test]
fn parser_handles_kotlin_top_level_decls_typealias_and_enums() {
    let source = r#"package com.example.util

typealias UserId = String

val GREETING: String = "Hello"

var counter: Int = 0

suspend fun fetchDefault(): String = "default"

enum class Color {
    RED, GREEN, BLUE
}

val lazyVal: Int by lazy { 42 }
"#;
    let mut parser = LanguageParser::new().unwrap();
    let record = kotlin_record("src/main/kotlin/com/example/util/Util.kt", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    assert!(parsed.unsupported.is_none());

    let alias = parsed
        .symbols
        .iter()
        .find(|symbol| symbol.name == "UserId")
        .expect("UserId typealias");
    assert_eq!(alias.kind, SymbolKind::TypeAlias);
    assert_eq!(alias.language_identity.as_deref(), Some("String"));

    let greeting = parsed
        .symbols
        .iter()
        .find(|symbol| symbol.name == "GREETING")
        .expect("GREETING top-level val");
    assert_eq!(greeting.kind, SymbolKind::Const);
    assert!(greeting.parent_id.is_none(), "top-level val has no parent");

    let counter = parsed
        .symbols
        .iter()
        .find(|symbol| symbol.name == "counter")
        .expect("counter top-level var");
    assert_eq!(counter.kind, SymbolKind::Static);

    let fetch = parsed
        .symbols
        .iter()
        .find(|symbol| symbol.name == "fetchDefault")
        .expect("fetchDefault function");
    assert_eq!(fetch.kind, SymbolKind::Function);
    assert!(fetch.attributes.contains(&"kotlin:suspend".to_string()));

    let color = parsed
        .symbols
        .iter()
        .find(|symbol| symbol.name == "Color")
        .expect("Color enum");
    assert_eq!(color.kind, SymbolKind::Enum);
    let red = parsed
        .symbols
        .iter()
        .find(|symbol| symbol.name == "RED")
        .expect("RED variant");
    assert_eq!(red.kind, SymbolKind::Variant);
    assert_eq!(red.parent_id.as_ref(), Some(&color.id));

    let delegated = parsed
        .symbols
        .iter()
        .find(|symbol| symbol.name == "lazyVal")
        .expect("delegated lazyVal");
    assert!(
        delegated
            .attributes
            .contains(&"kotlin:delegated".to_string())
    );
    assert_eq!(delegated.confidence, Confidence::Partial);
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
fn parser_extracts_c_symbols_includes_calls_macros_and_references() {
    let source = r#"
#include "runner.h"
#define RUNNER_MAX 8

typedef struct Runner Runner;

enum RunnerState {
    RUNNER_READY,
};

struct Runner {
    int id;
};

int helper(int value);

int runner_run(Runner *runner, int value) {
    if (value > RUNNER_MAX) {
        return helper(value);
    }
    return runner->id;
}
"#;
    let mut parser = RustParser::new().unwrap();
    let record = c_record("src/runner.c", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();

    assert!(parsed.unsupported.is_none());
    assert!(
        parsed
            .imports
            .iter()
            .any(|import| import.path == "runner.h")
    );
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "RUNNER_MAX" && symbol.kind == SymbolKind::Macro)
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
            .any(|symbol| symbol.name == "RUNNER_READY" && symbol.kind == SymbolKind::Variant)
    );
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "id" && symbol.kind == SymbolKind::Field)
    );
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "runner_run" && symbol.kind == SymbolKind::Function)
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
fn parser_extracts_cpp_classes_methods_templates_and_candidate_calls() {
    let source = r#"
#include "runner.hpp"

namespace app {
template <typename T>
class Runner : public Base {
public:
    Runner();
    int fallback(int value);
    T run(T value) {
        return helper(value);
    }
};

int helper(int value);

int call_runner(Runner<int>& runner) {
    return runner.run(1);
}
}
"#;
    let mut parser = RustParser::new().unwrap();
    let record = cpp_record("src/runner.cpp", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();

    assert!(parsed.unsupported.is_none());
    assert!(
        parsed
            .imports
            .iter()
            .any(|import| import.path == "runner.hpp")
    );
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "app" && symbol.kind == SymbolKind::Module)
    );
    assert!(parsed.symbols.iter().any(|symbol| {
        symbol.name == "Runner"
            && symbol.kind == SymbolKind::Class
            && symbol.attributes.contains(&"c++:template".to_string())
            && symbol.confidence == Confidence::Partial
    }));
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "run" && symbol.kind == SymbolKind::Method)
    );
    assert!(parsed.symbols.iter().any(|symbol| {
        symbol.name == "fallback"
            && symbol.kind == SymbolKind::Method
            && symbol
                .attributes
                .contains(&"c-family:declaration".to_string())
    }));
    assert!(parsed.calls.iter().any(|call| {
        call.name == "run"
            && call.kind == ParsedCallKind::Method
            && call.confidence == Confidence::CandidateSet
    }));
    assert!(
        parsed
            .references
            .iter()
            .any(|reference| reference.text == "Base" && reference.kind == ReferenceKind::Type)
    );
}

#[test]
fn parser_demotes_function_pointer_fields_to_field_symbols() {
    let source = r#"
struct Runner {
    int (*callback)(int);
    int id;
};
"#;
    let mut parser = RustParser::new().unwrap();
    let record = c_record("src/runner.c", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();

    // Function-pointer fields must not be lifted to Function symbols; the
    // clang AST oracle reports them as FieldDecl and treating them as
    // Function would inflate Squeezy's FP count.
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "callback" && symbol.kind == SymbolKind::Field),
        "expected function-pointer field `callback` to be classified as Field"
    );
    assert!(
        !parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "callback" && symbol.kind == SymbolKind::Function),
        "function-pointer field must not be classified as Function"
    );
}

#[test]
fn parser_distinguishes_namespace_qualified_free_function_from_method() {
    let source = r#"
namespace ns {
int free_function(int value);
}
int ns::free_function(int value) {
    return value;
}

class Foo {
public:
    int method();
};
int Foo::method() { return 1; }
"#;
    let mut parser = RustParser::new().unwrap();
    let record = cpp_record("src/qualified.cpp", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();

    // `ns::free_function` has a lowercase namespace qualifier, so the
    // qualifier-is-type-like heuristic keeps it as Function rather than
    // mis-promoting to Method.
    let free_functions = parsed
        .symbols
        .iter()
        .filter(|symbol| symbol.name == "free_function")
        .collect::<Vec<_>>();
    assert!(
        free_functions
            .iter()
            .any(|symbol| symbol.kind == SymbolKind::Function),
        "expected `ns::free_function` to remain a Function symbol"
    );

    // `Foo::method` has an uppercase qualifier so it stays Method.
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "method" && symbol.kind == SymbolKind::Method),
        "expected `Foo::method` to be classified as Method"
    );
}

#[test]
fn parser_marks_template_specializations_with_attribute() {
    let source = r#"
template <typename T>
class Box {};

template <>
class Box<int> {
public:
    int value;
};
"#;
    let mut parser = RustParser::new().unwrap();
    let record = cpp_record("src/box.cpp", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();

    let specializations = parsed
        .symbols
        .iter()
        .filter(|symbol| {
            symbol.kind == SymbolKind::Class
                && symbol
                    .attributes
                    .iter()
                    .any(|attribute| attribute == "c++:template-specialization")
        })
        .collect::<Vec<_>>();
    assert!(
        !specializations.is_empty(),
        "expected `Box<int>` to be tagged as a template specialization"
    );
}

#[test]
fn parser_detects_all_caps_macro_like_calls() {
    let source = r#"
void use_macros(void) {
    ASSERT(value > 0);
    LOG("hello");
    EXPECT_EQ(left, right);
    helper(value);
}
"#;
    let mut parser = RustParser::new().unwrap();
    let record = c_record("src/use_macros.c", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();

    // `ASSERT` is all-caps but has no underscore — the previous heuristic
    // missed it; the new one flags any all-caps name >= 2 chars as
    // macro-like.
    let assert_call = parsed
        .calls
        .iter()
        .find(|call| call.name == "ASSERT")
        .expect("ASSERT call should be recorded");
    assert_eq!(assert_call.confidence, Confidence::MacroOpaque);
    let log_call = parsed
        .calls
        .iter()
        .find(|call| call.name == "LOG")
        .expect("LOG call should be recorded");
    assert_eq!(log_call.confidence, Confidence::MacroOpaque);
    let expect_call = parsed
        .calls
        .iter()
        .find(|call| call.name == "EXPECT_EQ")
        .expect("EXPECT_EQ call should be recorded");
    assert_eq!(expect_call.confidence, Confidence::MacroOpaque);

    // A regular lowercase call remains a non-macro Heuristic.
    let helper_call = parsed
        .calls
        .iter()
        .find(|call| call.name == "helper")
        .expect("helper call should be recorded");
    assert_eq!(helper_call.confidence, Confidence::Heuristic);
}

#[test]
fn parser_emits_imports_for_cpp_using_declarations_and_directives() {
    let source = r#"
#include <vector>

using std::vector;
using namespace app;

void run() {
    vector<int> v;
}
"#;
    let mut parser = RustParser::new().unwrap();
    let record = cpp_record("src/uses.cpp", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();

    assert!(
        parsed
            .imports
            .iter()
            .any(|import| import.path == "std::vector" && !import.is_glob),
        "expected `using std::vector;` to emit a non-glob import"
    );
    assert!(
        parsed
            .imports
            .iter()
            .any(|import| import.path == "app" && import.is_glob),
        "expected `using namespace app;` to emit a glob import"
    );
}

#[test]
fn parser_resolves_cpp_access_modifiers_from_access_specifier_blocks() {
    let source = r#"
class Foo {
public:
    void public_method();
private:
    void private_method();
    int hidden_field;
protected:
    void protected_method();
};

struct Bar {
    void default_public();
private:
    void explicit_private();
};
"#;
    let mut parser = RustParser::new().unwrap();
    let record = cpp_record("src/access.cpp", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();

    let visibility = |name: &str| {
        parsed
            .symbols
            .iter()
            .find(|symbol| symbol.name == name)
            .and_then(|symbol| symbol.visibility.clone())
    };

    assert_eq!(visibility("public_method").as_deref(), Some("public"));
    assert_eq!(visibility("private_method").as_deref(), Some("private"));
    assert_eq!(visibility("hidden_field").as_deref(), Some("private"));
    assert_eq!(visibility("protected_method").as_deref(), Some("protected"));
    // `struct` defaults to public for members declared before the first
    // access_specifier; explicit `private:` blocks apply to later members.
    assert_eq!(visibility("default_public").as_deref(), Some("public"));
    assert_eq!(visibility("explicit_private").as_deref(), Some("private"));
}

#[test]
fn parser_collapses_forward_declaration_and_definition() {
    let source = r#"
int helper(int value);
int helper(int value) {
    return value + 1;
}
"#;
    let mut parser = RustParser::new().unwrap();
    let record = c_record("src/collapse.c", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();

    let helpers = parsed
        .symbols
        .iter()
        .filter(|symbol| symbol.name == "helper")
        .collect::<Vec<_>>();
    assert_eq!(
        helpers.len(),
        1,
        "expected exactly one canonical `helper` symbol after collapse"
    );
    assert!(
        helpers[0].body_span.is_some(),
        "definition should win over forward declaration"
    );
}

#[test]
fn parser_includes_record_as_glob_imports() {
    let source = r#"
#include "runner.h"
#include <stdio.h>
"#;
    let mut parser = RustParser::new().unwrap();
    let record = c_record("src/inc.c", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();

    assert!(
        parsed
            .imports
            .iter()
            .any(|import| import.path == "runner.h" && import.is_glob),
        "expected `#include \"runner.h\"` to be recorded as a glob import"
    );
    assert!(
        parsed
            .imports
            .iter()
            .any(|import| import.path == "stdio.h" && import.is_glob)
    );
}

#[test]
fn parser_detects_virtual_signature_keyword_only_when_relevant() {
    let source = r#"
class Base {
public:
    virtual void run();
};
"#;
    let mut parser = RustParser::new().unwrap();
    let record = cpp_record("src/virtual.cpp", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();

    assert!(parsed.symbols.iter().any(|symbol| {
        symbol.name == "run"
            && symbol.kind == SymbolKind::Method
            && symbol.attributes.iter().any(|attr| attr == "c++:virtual")
    }));
    // `Base` itself is a Class, not a Function/Method/Field, so we must
    // not propagate `c++:virtual` to it just because the body mentions
    // the keyword.
    assert!(parsed.symbols.iter().any(|symbol| {
        symbol.name == "Base"
            && symbol.kind == SymbolKind::Class
            && !symbol.attributes.iter().any(|attr| attr == "c++:virtual")
    }));
}

#[test]
fn parser_extracts_php_symbols_imports_calls_and_references() {
    let source = r#"<?php
namespace Foo\Bar;

use Foo\Traits\Loggable;
use Foo\Bar\Service as Svc;

interface IRunner {
    public function run(int $id): void;
}

trait Loggable {
    protected function log(string $msg): void {
    }
}

class Service implements IRunner {
    use Loggable;

    public string $prefix = 'svc-';

    public function run(int $id): void {
        $this->log("running $id");
    }
}

enum Status: string {
    case Ok = 'ok';
    case Failed = 'fail';
}

class Magic {
    public function __call($name, $args) {
        return null;
    }
}
"#;
    let mut parser = LanguageParser::new().unwrap();
    let record = php_record("src/all.php", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();

    assert!(parsed.unsupported.is_none());
    assert_eq!(parsed.package.as_deref(), Some("Foo.Bar"));
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| { symbol.kind == SymbolKind::Interface && symbol.name == "IRunner" })
    );
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| { symbol.kind == SymbolKind::Trait && symbol.name == "Loggable" })
    );
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| { symbol.kind == SymbolKind::Class && symbol.name == "Service" })
    );
    assert!(parsed.symbols.iter().any(|symbol| {
        symbol.kind == SymbolKind::Enum
            && symbol.name == "Status"
            && symbol
                .attributes
                .iter()
                .any(|attr| attr == "php:backed:string")
    }));
    assert!(parsed.symbols.iter().any(|symbol| {
        symbol.kind == SymbolKind::Method
            && symbol.name == "__call"
            && symbol.attributes.iter().any(|attr| attr == "php:magic")
    }));
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| { symbol.kind == SymbolKind::Field && symbol.name == "prefix" })
    );
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| { symbol.kind == SymbolKind::Variant && symbol.name == "Ok" })
    );
    assert!(
        parsed
            .imports
            .iter()
            .any(|import| import.path == "Foo.Traits.Loggable" && import.alias.is_none())
    );
    assert!(
        parsed.imports.iter().any(
            |import| import.path == "Foo.Bar.Service" && import.alias.as_deref() == Some("Svc")
        )
    );
    let service = parsed
        .symbols
        .iter()
        .find(|symbol| symbol.name == "Service")
        .unwrap();
    assert!(
        service
            .attributes
            .iter()
            .any(|attr| attr == "uses_trait:Loggable"),
        "Service should carry uses_trait:Loggable attribute"
    );
    assert!(service.attributes.iter().any(|attr| attr == "base:IRunner"));
    assert!(parsed.calls.iter().any(|call| call.name == "log"));
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

#[test]
fn parser_extracts_js_ts_symbols_imports_calls_and_references() {
    let source = r#"
import React, { useMemo as memo } from "react";
import { buildRunner } from "./helpers";

export interface RunnerProps {
    name: string;
}

export class Runner {
    start(props: RunnerProps) {
        return buildRunner(props.name);
    }
}

export const RunnerView = (props: RunnerProps) => <Runner />;
"#;
    let mut parser = RustParser::new().unwrap();
    let record = tsx_record("src/app.tsx", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();

    assert!(parsed.unsupported.is_none());
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| { symbol.name == "RunnerProps" && symbol.kind == SymbolKind::Interface })
    );
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "Runner" && symbol.kind == SymbolKind::Class)
    );
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "start" && symbol.kind == SymbolKind::Method)
    );
    assert!(parsed.symbols.iter().any(|symbol| {
        symbol.name == "RunnerView"
            && symbol.kind == SymbolKind::Function
            && symbol.attributes.contains(&"jsx:component".to_string())
    }));
    assert!(parsed.imports.iter().any(|import| {
        import.path == "react.useMemo" && import.alias.as_deref() == Some("memo")
    }));
    assert!(
        parsed
            .imports
            .iter()
            .any(|import| import.path == "./helpers.buildRunner")
    );
    assert!(parsed.calls.iter().any(|call| call.name == "buildRunner"));
    assert!(parsed.references.iter().any(|reference| {
        reference.text == "RunnerProps" && reference.kind == ReferenceKind::Type
    }));
}

#[test]
fn parser_keeps_js_ts_const_and_function_symbol_scope_precise() {
    let source = r#"
const options = values.map((value) => value.name);
service.start = function start() {
    return options;
};
service.stop = function() {};

class Runner {
    constructor() {}
    #privateMethod() {}
    [Symbol.iterator]() {}
    run() {}
    handle = () => options;
}

class ConstructorLocals {
    constructor() {
        const localFactory = () => options;
    }
}

namespace RunnerNamespace {
    export function create() {
        return options;
    }
}

@sealed
class DecoratedRunner {}
"#;
    let mut parser = RustParser::new().unwrap();
    let record = ts_record("src/scope.ts", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();

    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| { symbol.name == "options" && symbol.kind == SymbolKind::Const })
    );
    assert!(
        !parsed
            .symbols
            .iter()
            .any(|symbol| { symbol.name == "options" && symbol.kind == SymbolKind::Function })
    );
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| { symbol.name == "start" && symbol.kind == SymbolKind::Function })
    );
    assert!(
        !parsed
            .symbols
            .iter()
            .any(|symbol| { symbol.name == "stop" && symbol.kind == SymbolKind::Function })
    );
    assert!(
        !parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "constructor")
    );
    assert!(
        !parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "#privateMethod")
    );
    assert!(
        !parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name.contains("Symbol.iterator"))
    );
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| { symbol.name == "handle" && symbol.kind == SymbolKind::Method })
    );
    assert!(
        !parsed
            .symbols
            .iter()
            .any(|symbol| { symbol.name == "localFactory" && symbol.kind == SymbolKind::Method })
    );
    assert!(
        parsed.symbols.iter().any(|symbol| {
            symbol.name == "RunnerNamespace" && symbol.kind == SymbolKind::Module
        })
    );
    assert!(parsed.symbols.iter().any(|symbol| {
        symbol.name == "DecoratedRunner"
            && symbol.attributes.contains(&"decorator:sealed".to_string())
    }));
}

#[test]
fn js_ts_static_method_not_dropped_by_body_comment_with_get_or_set_words() {
    // Regression: the prior accessor check string-scanned the whole method
    // text for " get "/" set ", which mis-fired when a benign comment in the
    // method body contained those words (e.g. axios's `static from(...)`).
    let mut parser = LanguageParser::new().unwrap();
    let source = r#"
class AxiosError {
  static from(error, code) {
    // Preserve status from the original error if not already set from response
    return null;
  }
  static set name(v) { return v; }
  static get foo() { return 1; }
}
"#;
    let record = js_record("src/axios-error.js", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let methods: Vec<_> = parsed
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Method)
        .map(|symbol| symbol.name.clone())
        .collect();
    assert!(
        methods.iter().any(|name| name == "from"),
        "expected `from` method, got {methods:?}"
    );
    assert!(
        methods.iter().all(|name| name != "name"),
        "static set accessor `name` should not be exposed as a method"
    );
    assert!(
        methods.iter().all(|name| name != "foo"),
        "static get accessor `foo` should not be exposed as a method"
    );
}

#[test]
fn js_ts_class_expression_method_arrow_field_is_recognized() {
    // `C = class { static f = () => 0 }` carries the field inside a
    // class_expression rather than a class_declaration, so the previous
    // Field-to-Method conversion (which required parent_kind == Class) never
    // fired. Anchoring the conversion on the class_body parent instead keeps
    // both named and anonymous class members visible.
    let mut parser = LanguageParser::new().unwrap();
    let source = r#"
class Named {
  static f = () => 0;
}
C = class {
  static f = () => 1;
};
"#;
    let record = js_record("src/class-expr.js", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let methods: Vec<_> = parsed
        .symbols
        .iter()
        .filter(|symbol| symbol.kind == SymbolKind::Method && symbol.name == "f")
        .collect();
    assert!(
        methods.len() >= 2,
        "expected `f` from both the named and anonymous classes, got {methods:#?}"
    );
}

#[test]
fn js_ts_declare_global_emits_global_module_symbol() {
    let mut parser = LanguageParser::new().unwrap();
    let source = r#"
declare global {
  interface SymbolConstructor {
    readonly observable: symbol;
  }
}
"#;
    let record = ts_record("src/globals.d.ts", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "global" && symbol.kind == SymbolKind::Module),
        "expected synthesized Module:global from `declare global`"
    );
}

#[test]
fn js_ts_using_declaration_is_emitted_as_const() {
    // Tree-sitter still parses `using x = expr` as an assignment_expression
    // with a leading anonymous `using` token, so the regular
    // variable_declarator path can't see it. Synthesizing a Const symbol
    // matches what the TypeScript compiler API reports.
    let mut parser = LanguageParser::new().unwrap();
    let source = r#"
async function open() {
  await using server = await createSimpleServer();
  using cleanup = makeCleanup();
  return server;
}
"#;
    let record = ts_record("src/using.ts", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "server" && symbol.kind == SymbolKind::Const),
        "expected `await using server` to become Const:server"
    );
    assert!(
        parsed
            .symbols
            .iter()
            .any(|symbol| symbol.name == "cleanup" && symbol.kind == SymbolKind::Const),
        "expected `using cleanup` to become Const:cleanup"
    );
}

#[test]
fn js_ts_for_loop_locals_are_not_emitted_as_symbols() {
    // `for (let i = 0; ...)`, `for (const x of ...)`, and `catch (e)` are
    // tiny-scope locals; the prior extractor emitted the for-statement's
    // `lexical_declaration -> variable_declarator` chain as Const symbols,
    // which polluted symbol-by-name lookups with `i`/`len`/`e` per call site.
    let mut parser = LanguageParser::new().unwrap();
    let source = r#"
export function noisy(items) {
  for (let i = 0; i < items.length; i++) {
    items[i] = i;
  }
  for (const item of items) {
    use(item);
  }
  try {
    risky();
  } catch (err) {
    log(err);
  }
}
"#;
    let record = js_record("src/noisy.js", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let names: Vec<_> = parsed
        .symbols
        .iter()
        .map(|symbol| (symbol.name.clone(), symbol.kind))
        .collect();
    assert!(
        names
            .iter()
            .any(|(name, kind)| name == "noisy" && *kind == SymbolKind::Function),
        "expected the outer function to still be a symbol, got {names:?}"
    );
    for forbidden in ["i", "item", "err"] {
        assert!(
            !names.iter().any(|(name, _)| name == forbidden),
            "loop/catch local `{forbidden}` should not be exposed as a symbol"
        );
    }
}

#[test]
fn parser_extracts_ruby_symbols_imports_calls_and_references() {
    let source = r#"
require "json"
require_relative "user"

class Admin < User
  include Auditable

  attr_accessor :name, :email
  attr_reader :role

  def promote(target)
    target.full_name
  end

  def self.find_by_email(email)
    nil
  end
end

def standalone_runner(arg)
  arg.do_thing
end
"#;
    let mut parser = LanguageParser::new().unwrap();
    let record = ruby_record("app/models/admin.rb", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();

    assert!(parsed.unsupported.is_none());
    let admin = parsed
        .symbols
        .iter()
        .find(|s| s.name == "Admin" && s.kind == SymbolKind::Class)
        .expect("Admin class");
    assert!(admin.attributes.contains(&"base:User".to_string()));
    assert!(
        admin
            .attributes
            .iter()
            .any(|a| a == "mixin:include:Auditable")
    );
    assert!(parsed.symbols.iter().any(|s| s.name == "promote"
        && s.kind == SymbolKind::Method
        && s.parent_id == Some(admin.id.clone())));
    let find_by_email = parsed
        .symbols
        .iter()
        .find(|s| s.name == "find_by_email")
        .expect("find_by_email symbol");
    assert_eq!(find_by_email.kind, SymbolKind::Method);
    assert!(
        find_by_email
            .attributes
            .iter()
            .any(|a| a == "ruby:singleton")
    );
    assert!(parsed.symbols.iter().any(|s| s.name == "name"
        && s.kind == SymbolKind::Method
        && s.attributes.iter().any(|a| a == "ruby:synthesized")
        && s.attributes.iter().any(|a| a == "ruby:attr-reader")));
    assert!(parsed.symbols.iter().any(|s| s.name == "name="
        && s.kind == SymbolKind::Method
        && s.attributes.iter().any(|a| a == "ruby:attr-writer")));
    assert!(parsed.symbols.iter().any(|s| s.name == "role"
        && s.kind == SymbolKind::Method
        && s.attributes.iter().any(|a| a == "ruby:attr-reader")));
    assert!(
        parsed
            .symbols
            .iter()
            .any(|s| s.name == "standalone_runner" && s.kind == SymbolKind::Function)
    );
    assert!(parsed.imports.iter().any(|i| i.path == "json"));
    assert!(
        parsed
            .imports
            .iter()
            .any(|i| i.path == "app/models/user.rb")
    );
    assert!(
        parsed
            .calls
            .iter()
            .any(|c| c.name == "full_name" && c.receiver.as_deref() == Some("target"))
    );
}

#[test]
fn parser_handles_ruby_module_and_class_variables() {
    let source = r#"
module Auditable
  CONST_VAL = 42
  @@cvar = "x"

  def audit!(event)
    @event = event
    log(event)
    sql = <<~SQL
      SELECT bogus FROM tbl WHERE id = 1
    SQL
    Foo::Bar.new(sql)
  end
end
"#;
    let mut parser = LanguageParser::new().unwrap();
    let record = ruby_record("app/concerns/auditable.rb", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();

    assert!(parsed.unsupported.is_none());
    let module = parsed
        .symbols
        .iter()
        .find(|s| s.name == "Auditable" && s.kind == SymbolKind::Module)
        .expect("Auditable module");
    assert!(parsed.symbols.iter().any(|s| s.name == "audit!"
        && s.kind == SymbolKind::Method
        && s.parent_id == Some(module.id.clone())));
    assert!(
        parsed
            .symbols
            .iter()
            .any(|s| s.name == "CONST_VAL" && s.kind == SymbolKind::Const)
    );
    assert!(parsed.symbols.iter().any(|s| s.name == "@@cvar"
        && s.kind == SymbolKind::Field
        && s.attributes.iter().any(|a| a == "ruby:cvar")));
    assert!(parsed.symbols.iter().any(|s| s.name == "@event"
        && s.kind == SymbolKind::Field
        && s.attributes.iter().any(|a| a == "ruby:ivar")));
    // The heredoc body should not surface as an identifier reference.
    for reference in &parsed.references {
        assert_ne!(reference.text, "bogus");
    }
    // The `Foo::Bar.new(sql)` call should still register inside the method.
    assert!(
        parsed
            .calls
            .iter()
            .any(|c| c.name == "new" && c.receiver.as_deref() == Some("Foo::Bar"))
    );
}

#[test]
fn parser_records_singleton_class_methods() {
    let source = r#"
class Greeter
  class << self
    def make
      Greeter.new
    end
  end
end
"#;
    let mut parser = LanguageParser::new().unwrap();
    let record = ruby_record("app/services/greeter.rb", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    let greeter = parsed
        .symbols
        .iter()
        .find(|s| s.name == "Greeter" && s.kind == SymbolKind::Class)
        .expect("Greeter class");
    // The `make` method must be hosted by the Greeter class via the
    // singleton_class descend path.
    assert!(parsed.symbols.iter().any(|s| s.name == "make"
        && s.kind == SymbolKind::Method
        && s.parent_id == Some(greeter.id.clone())));
}

#[test]
fn parser_extracts_swift_symbols_imports_calls_and_references() {
    let source = r#"
import Foundation
import struct CoreGraphics.CGRect

@MainActor
public final class UserRepository {
    @Published var users: [String] = []

    public init() {}

    public func refresh() async {
        users.removeAll()
    }
}

protocol Endpoint {
    var path: String { get }
    func encode() -> Data
}

struct UserEndpoint: Endpoint {
    let path: String = "/users"
    func encode() -> Data {
        return Data()
    }
}

actor Cache<Key: Hashable, Value> {
    private var storage: [Key: Value] = [:]
}

extension String {
    func sanitized() -> String {
        return self
    }
}

enum APIResult<Value, Failure: Error> {
    case success(Value)
    case failure(Failure)
}

typealias Endpoints = [Endpoint]

func freeFunction() -> Int { return 1 }
"#;
    let mut parser = LanguageParser::new().unwrap();
    let record = swift_record("Sources/Models/Models.swift", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();

    assert!(
        parsed.unsupported.is_none(),
        "swift parser must not return unsupported"
    );

    // Imports
    assert!(
        parsed
            .imports
            .iter()
            .any(|i| i.path == "Foundation" && i.kind == ImportKind::Named)
    );
    assert!(
        parsed.imports.iter().any(
            |i| i.path == "CoreGraphics.CGRect" && i.imported_name.as_deref() == Some("CGRect")
        )
    );

    // SwiftPM module hint
    assert_eq!(parsed.package.as_deref(), Some("Models"));

    // Symbols: top-level types
    let by_name = |name: &str, kind: SymbolKind| {
        parsed
            .symbols
            .iter()
            .find(|s| s.name == name && s.kind == kind)
    };

    let repo = by_name("UserRepository", SymbolKind::Class).expect("UserRepository class");
    assert!(repo.attributes.iter().any(|a| a == "MainActor"));
    assert!(repo.attributes.iter().any(|a| a == "swift:final"));

    let cache = by_name("Cache", SymbolKind::Class).expect("Cache actor");
    assert!(cache.attributes.iter().any(|a| a == "swift:actor"));
    assert!(
        cache.attributes.iter().any(|a| a == "base:Hashable"),
        "Cache should record `base:Hashable` from generic constraint, got {:?}",
        cache.attributes
    );

    let user_endpoint = by_name("UserEndpoint", SymbolKind::Struct).expect("UserEndpoint struct");
    assert!(
        user_endpoint
            .attributes
            .iter()
            .any(|a| a == "base:Endpoint")
    );

    assert!(by_name("Endpoint", SymbolKind::Trait).is_some(), "protocol");
    assert!(by_name("APIResult", SymbolKind::Enum).is_some(), "enum");
    assert!(
        by_name("Endpoints", SymbolKind::TypeAlias).is_some(),
        "typealias"
    );
    assert!(
        by_name("freeFunction", SymbolKind::Function).is_some(),
        "file-scope func"
    );

    // Enum cases
    assert!(by_name("success", SymbolKind::Variant).is_some());
    assert!(by_name("failure", SymbolKind::Variant).is_some());

    // Methods + arity
    let refresh = by_name("refresh", SymbolKind::Method).expect("refresh method");
    assert!(refresh.attributes.iter().any(|a| a == "swift:async"));
    assert!(by_name("encode", SymbolKind::Method).is_some());
    let init = parsed
        .symbols
        .iter()
        .find(|s| s.name == "init" && s.kind == SymbolKind::Method)
        .expect("init method");
    assert!(init.attributes.iter().any(|a| a == "swift:init"));

    // Fields
    let users_field = by_name("users", SymbolKind::Field).expect("users field");
    assert!(users_field.attributes.iter().any(|a| a == "Published"));

    let path_field = by_name("path", SymbolKind::Field).expect("path field");
    assert!(path_field.attributes.iter().any(|a| a == "type:String"));

    // Extension propagates language_identity
    let sanitized = parsed
        .symbols
        .iter()
        .find(|s| s.name == "sanitized" && s.kind == SymbolKind::Method)
        .expect("sanitized method");
    assert_eq!(sanitized.language_identity.as_deref(), Some("String"));
    assert!(
        sanitized.parent_id.is_none(),
        "extension members emit at file scope"
    );

    // References: at least one Endpoint type reference
    assert!(
        parsed
            .references
            .iter()
            .any(|r| r.text == "Endpoint" && r.kind == ReferenceKind::Type)
    );
}

#[test]
fn parser_extracts_swift_computed_properties_and_attributes() {
    let source = r#"
import Foundation

struct Person {
    let first: String
    let last: String

    var fullName: String {
        get { "\(first) \(last)" }
    }
}

@objc(MyHandler)
class Handler {}
"#;
    let mut parser = LanguageParser::new().unwrap();
    let record = swift_record("Sources/Models/Person.swift", source);
    let parsed = parser.parse_source(&record, source.to_string()).unwrap();
    assert!(parsed.unsupported.is_none());
    let full_name = parsed
        .symbols
        .iter()
        .find(|s| s.name == "fullName" && s.kind == SymbolKind::Field)
        .expect("fullName field");
    assert!(
        full_name.attributes.iter().any(|a| a == "swift:computed"),
        "computed property must carry swift:computed attribute"
    );
    let handler = parsed
        .symbols
        .iter()
        .find(|s| s.name == "Handler" && s.kind == SymbolKind::Class)
        .expect("Handler class");
    assert!(handler.attributes.iter().any(|a| a == "objc"));
    assert!(
        parsed
            .references
            .iter()
            .any(|r| r.text == "MyHandler" && r.kind == ReferenceKind::Attribute),
        "objc(MyHandler) records MyHandler as attribute"
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

fn ruby_record(relative_path: &str, source: &str) -> FileRecord {
    let mut record = record(relative_path, source);
    record.language = LanguageKind::Ruby;
    record
}

fn tsx_record(relative_path: &str, source: &str) -> FileRecord {
    let mut record = record(relative_path, source);
    record.language = LanguageKind::Tsx;
    record
}

fn ts_record(relative_path: &str, source: &str) -> FileRecord {
    let mut record = record(relative_path, source);
    record.language = LanguageKind::TypeScript;
    record
}

fn js_record(relative_path: &str, source: &str) -> FileRecord {
    let mut record = record(relative_path, source);
    record.language = LanguageKind::JavaScript;
    record
}

fn java_record(relative_path: &str, source: &str) -> FileRecord {
    let mut record = record(relative_path, source);
    record.language = LanguageKind::Java;
    record
}

fn csharp_record(relative_path: &str, source: &str) -> FileRecord {
    let mut record = record(relative_path, source);
    record.language = LanguageKind::CSharp;
    record
}

fn c_record(relative_path: &str, source: &str) -> FileRecord {
    let mut record = record(relative_path, source);
    record.language = LanguageKind::C;
    record
}

fn cpp_record(relative_path: &str, source: &str) -> FileRecord {
    let mut record = record(relative_path, source);
    record.language = LanguageKind::Cpp;
    record
}

fn go_record(relative_path: &str, source: &str) -> FileRecord {
    let mut record = record(relative_path, source);
    record.language = LanguageKind::Go;
    record
}

fn kotlin_record(relative_path: &str, source: &str) -> FileRecord {
    let mut record = record(relative_path, source);
    record.language = LanguageKind::Kotlin;
    record
}

fn php_record(relative_path: &str, source: &str) -> FileRecord {
    let mut record = record(relative_path, source);
    record.language = LanguageKind::Php;
    record
}

fn swift_record(relative_path: &str, source: &str) -> FileRecord {
    let mut record = record(relative_path, source);
    record.language = LanguageKind::Swift;
    record
}

fn temp_root(name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("squeezy-{name}-{pid}-{counter}-{nonce}"));
    fs::create_dir_all(&root).unwrap();
    root
}
