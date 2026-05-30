# Language Coverage

This document is the user-facing source for Squeezy's language coverage. It is
organized by `LanguageFamily`, which maps one or more source languages to a
single parser backend and navigation behavior. The internal benchmark corpus
uses the same family names so coverage claims stay checkable.

## Coverage Matrix

| Family | LanguageKind variants | Extensions | Tree-sitter grammars | Oracle | Mixed workload | Smoke fixture | Smoke spec | Full-tier repos |
|---|---|---|---|---|---|---|---|---|
| `rust` | Rust | `rs` | `tree-sitter-rust` | `rust_analyzer` | yes | `benchmarks/fixtures/rust/semantic-cases` | `benchmarks/specs/smoke-queries.json` | ripgrep, fd, bat, tokio, serde |
| `python` | Python | `py` | `tree-sitter-python` | `cpython_ast` | no | `benchmarks/fixtures/python/semantic-cases` | `benchmarks/specs/python-smoke-queries.json` | requests, flask, click, black, fastapi |
| `java` | Java | `java` | `tree-sitter-java` | `javac` | no | `benchmarks/fixtures/java/semantic-cases` | `benchmarks/specs/java-smoke-queries.json` | junit5, mockito, guava, retrofit, picocli |
| `csharp` | C# | `cs`, `csx` | `tree-sitter-c-sharp` | `roslyn` | yes | `benchmarks/fixtures/csharp/semantic-cases` | `benchmarks/specs/csharp-smoke-queries.json` | newtonsoft_json, dapper, automapper, polly, serilog |
| `go` | Go | `go` | `tree-sitter-go` | `go_types` | yes | `benchmarks/fixtures/go/semantic-cases` | `benchmarks/specs/go-smoke-queries.json` | gin, cobra, prometheus, etcd, zap |
| `c-family` | C, C++ | `c`, `h`, `cc`, `cpp`, `cxx`, `hh`, `hpp`, `hxx` | `tree-sitter-c`, `tree-sitter-cpp` | `clang` | yes | `benchmarks/fixtures/c/semantic-cases`, `benchmarks/fixtures/cpp/semantic-cases` | `benchmarks/specs/c-smoke-queries.json`, `benchmarks/specs/cpp-smoke-queries.json` | redis, curl, sqlite, protobuf, nlohmann_json |
| `js-ts` | JavaScript, JSX, TypeScript, TSX | `cjs`, `cts`, `js`, `jsx`, `mjs`, `mts`, `ts`, `tsx` | `tree-sitter-javascript`, `tree-sitter-typescript` | `tsc` | yes | `benchmarks/fixtures/js-ts/semantic-cases` | `benchmarks/specs/js-ts-smoke-queries.json` | vite, redux, axios, express, prettier |
| `ruby` | Ruby | `rb` | `tree-sitter-ruby` | `ruby_prism` | no | `benchmarks/fixtures/ruby/semantic-cases` | `benchmarks/specs/ruby-smoke-queries.json` | sinatra |

## Rust

Indexed: modules, structs, enums, unions, traits, impls, functions, methods,
consts, statics, type aliases, macros, tests, imports, references, calls, and
body hits.

Known limitations: cfg and feature evaluation is still conservative; macro
expansion is not attempted; external crates and standard-library roots are
treated as external navigation targets. Cargo metadata and optional `cargo
check` diagnostics can be refreshed explicitly as compiler facts, but navigation
tools do not run Cargo automatically.

Oracle: rust-analyzer LSP/symbol probes, plus `cargo check` timing for validation.

## Python

Indexed: classes, functions, methods, imports, calls, decorators, docstrings,
class bases, type annotations, class fields, exports, aliases, and references.

Known limitations: dynamic attributes, metaclasses, runtime import side effects,
monkey-patching, framework magic, and type-inferred receiver dispatch are
heuristic or external. No mypy-style type solving is attempted.

Known follow-ups: framework-aware navigation can improve route, ORM, and
decorator-heavy projects, but the current graph remains tree-sitter and local
heuristic based.

Oracle: CPython `ast` parsing and declaration comparison.

## Java

Indexed: packages, imports, classes, interfaces, enums, records, annotations,
methods, constructors, fields, inheritance, implements edges, calls, references,
and Maven/Gradle project facts.

Known limitations: overload resolution, runtime dispatch, reflection, annotation
processors, generated sources, and external classpaths remain heuristic or
external.

Known follow-ups: overload resolution, classpath completeness, annotation
processing, and generated-source behavior are not compiler-equivalent.

Oracle: `javac` compiler-tree scans for symbols and navigation query checks.

## C#

Indexed: namespaces, using directives, classes, interfaces, records, structs,
enums, methods, constructors, operators, fields, properties, events, enum
members, attributes, calls, references, stable language identities, partial-type
links, inheritance and implements edges, and C# project-file facts from `.csproj`,
`.sln`, `.slnx`, `Directory.Build.props`, `Directory.Build.targets`,
`global.json`, and `packages.lock.json`.

Known limitations: generic constraint solving, overload resolution, extension
methods, partial project system behavior, generated code, and full MSBuild
semantics are not compiler-equivalent. Razor, Blazor `.razor`, and `.cshtml`
files are intentionally discovered as bounded fallback inputs for v0; embedded
C# is not assigned graph confidence yet.

Known follow-ups: broader C# project-file fidelity, generated-source
handling, and framework-specific navigation remain bounded local heuristics.

Oracle: Roslyn project in `benchmarks/oracle/csharp` for declaration symbols and
syntactic extends/implements edges.

## Go

Indexed: packages, imports, structs, interfaces, type aliases, functions,
methods, receiver relationships, fields, constants, variables, tests, calls, and
references.

Known limitations: interface satisfaction, full receiver type inference, embedded
field promotion, build tags, generated code, and external modules are candidate,
heuristic, or external facts.

Known follow-ups: additional corpus coverage, interface satisfaction, and build
tag handling can improve precision and recall.

Oracle: Go parser/types script embedded in the benchmark binary.

## C/C++

Indexed: includes, namespaces, classes, structs, unions, enums, typedefs/type
aliases, fields, functions, methods, constructors/destructors, operators,
templates, macro definitions/usages, and declaration/definition spans.

Known limitations: macro expansion, overload resolution, templates, virtual
dispatch, ADL, build-system flags, and cross-translation-unit semantics are
heuristic. Preprocessor directives are evidence for fallback, not exact compiler
state.

Known follow-ups: additional C and C++ corpus coverage, compile database
handling, and macro/template handling can improve precision and recall.

Oracle: clang AST JSON, shared by C and C++.

## JavaScript and TypeScript

Indexed: functions, arrow functions assigned to names, classes, methods,
class-property arrow methods, fields, interfaces, modules/namespaces,
decorators, type aliases, enums, imports/exports, CommonJS require/export
aliases, JSX/TSX component-like declarations, calls, member calls, type
references, and object/member references.

Known limitations: dynamic imports, computed property access, bundler aliases
without checked config, package export edge cases, runtime dispatch, and full
TypeScript type evaluation are heuristic, external, or fallback results.
`tsconfig.json` path handling is local and intentionally bounded.

Known follow-ups: bundler alias handling, package export edge cases, and
framework conventions can improve precision and recall.

Oracle: TypeScript compiler API. CI installs the pinned `typescript` package and
sets `SQUEEZY_TYPESCRIPT_PATH`.

## Ruby

Indexed: classes, modules, methods, singleton methods (`def self.foo`),
`class << self` singleton class bodies, top-level functions, synthesized
`attr_accessor`/`attr_reader`/`attr_writer` accessors, `require`/
`require_relative`/`load`/`autoload` imports, `include`/`extend`/`prepend`
mixins (recorded as both Type references and `mixin:include:<Mod>` style
attributes for ancestor walks), constants, class variables, instance
variables, calls, and references.

Known limitations: dynamic dispatch through `method_missing`,
`define_method`, `eval`/`instance_eval`/`class_eval`/`module_eval`-built
methods, anonymous classes via `Class.new`, runtime monkey-patching, and
gem-style `require` path resolution are documented recall gaps and are
excluded from the oracle as well. Receiver-typed call resolution is best
effort because Ruby lacks parameter types.

Oracle: Ruby Prism subprocess. CI installs Ruby 3.3 via `ruby/setup-ruby`
with `continue-on-error: true`; when the toolchain is missing the oracle
degrades to a `mode = "scan-only"` self-compare.

## Benchmark Corpus Reporting

The benchmark report is intentionally claim-ready for v0 validation. Each case
reports deterministic tool/cost metrics, grep-baseline query counts, wall time,
answer-quality counts, oracle precision/recall where available, and fallback
quality. Smoke fixtures for every supported family include generated and vendor
source paths; their specs assert those paths are surfaced as generated/vendor
fallback evidence rather than graph-confident answers.

Contributor steps for adding or changing a language family live in
[`../internal/ARCHITECTURE.md`](../internal/ARCHITECTURE.md).
