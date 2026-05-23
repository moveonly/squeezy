# Language Coverage

This document is the canonical source for Squeezy's language coverage. It is
organized by `LanguageFamily`, which maps one or more `LanguageKind` values to a
single parser backend, graph extension, benchmark oracle, and CI benchmark job.

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

## Rust

Indexed: modules, structs, enums, unions, traits, impls, functions, methods,
consts, statics, type aliases, macros, tests, imports, references, calls, and
body hits.

Known limitations: cfg and feature evaluation is local and syntactic until the
compiler-as-fact work lands; macro expansion is not attempted; external crates
and standard-library roots are treated as external rather than resolved through
Cargo metadata.

TODO: compiler-as-fact integration is tracked by `squeezy-cfa.18`.

Oracle: rust-analyzer LSP/symbol probes, plus `cargo check` timing for validation.

## Python

Indexed: classes, functions, methods, imports, calls, decorators, docstrings,
class bases, type annotations, class fields, exports, aliases, and references.

Known limitations: dynamic attributes, metaclasses, runtime import side effects,
monkey-patching, framework magic, and type-inferred receiver dispatch are
heuristic or external. No mypy-style type solving is attempted.

TODO: broaden framework-aware navigation after the core graph-backed navigation
tools stabilize under `squeezy-cfa.5`.

Oracle: CPython `ast` parsing and declaration comparison.

## Java

Indexed: packages, imports, classes, interfaces, enums, records, annotations,
methods, constructors, fields, inheritance, implements edges, calls, references,
and Maven/Gradle project facts.

Known limitations: overload resolution, runtime dispatch, reflection, annotation
processors, generated sources, and external classpaths remain heuristic or
external.

TODO: Java follow-up work lives under `squeezy-cfa.25`.

Oracle: `javac` compiler-tree scans for symbols and navigation query checks.

## C#

Indexed: namespaces, using directives, classes, interfaces, records, structs,
enums, methods, constructors, fields, properties, attributes, calls, references,
and base-type facts.

Known limitations: generic constraint solving, overload resolution, extension
methods, partial project system behavior, generated code, and full MSBuild
semantics are not compiler-equivalent.

TODO: C# declaration, edge, corpus, benchmark, and incremental follow-ups are
tracked under `squeezy-cfa.26`.

Oracle: Roslyn project in `benchmarks/oracle/csharp`.

## Go

Indexed: packages, imports, structs, interfaces, type aliases, functions,
methods, receiver relationships, fields, constants, variables, tests, calls, and
references.

Known limitations: interface satisfaction, full receiver type inference, embedded
field promotion, build tags, generated code, and external modules are candidate,
heuristic, or external facts.

TODO: additional corpus coverage is tracked by `squeezy-cfa.38`.

Oracle: Go parser/types script embedded in the benchmark binary.

## C/C++

Indexed: includes, namespaces, classes, structs, unions, enums, typedefs/type
aliases, fields, functions, methods, constructors/destructors, operators,
templates, macro definitions/usages, and declaration/definition spans.

Known limitations: macro expansion, overload resolution, templates, virtual
dispatch, ADL, build-system flags, and cross-translation-unit semantics are
heuristic. Preprocessor directives are evidence for fallback, not exact compiler
state.

TODO: additional cross-language benchmark corpus coverage is tracked by
`squeezy-cfa.38`.

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

TODO: JS/TS follow-ups are tracked by `squeezy-cfa.27`.

Oracle: TypeScript compiler API. CI installs the pinned `typescript` package and
sets `SQUEEZY_TYPESCRIPT_PATH`.

## Adding A New Language

1. Add the `LanguageKind` variant and map it to a `LanguageFamily` in
   `squeezy-core`.
2. Add extensions to `LanguageFamily::file_extensions`.
3. Register a `LanguageBackend` in `squeezy-parse`.
4. Register a `LanguageGraphExt` in `squeezy-graph`.
5. Register a `LanguageOracle` in `benchmarks/squeezy-graph-bench`.
6. Add fixture and spec files under `benchmarks/fixtures/` and
   `benchmarks/specs/`.
7. Add the language family to `.github/workflows/benchmark.yml` and document the
   oracle/limitations in this file.
