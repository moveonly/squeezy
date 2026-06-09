# Language Coverage

This document is the user-facing source for Squeezy's language coverage. It is
organized by `LanguageFamily`, which maps one or more source languages to a
single parser backend and navigation behavior. The benchmark CLI emits the same
family IDs through `--list-languages` and `--list-oracles`, and CI checks this
matrix against that live registry.

## Coverage Matrix

| Family | LanguageKind variants | Extensions | Tree-sitter grammars | Oracle | Mixed workload | Smoke fixture | Smoke spec | Full-tier repos |
|---|---|---|---|---|---|---|---|---|
| `rust` | Rust | `rs` | `tree-sitter-rust` | `rust_analyzer` | yes | `benchmarks/fixtures/rust/semantic-cases` | `benchmarks/specs/smoke-queries.json` | ripgrep, fd, bat, tokio, serde |
| `python` | Python | `py` | `tree-sitter-python` | `cpython_ast` | no | `benchmarks/fixtures/python/semantic-cases` | `benchmarks/specs/python-smoke-queries.json` | requests, flask, click, black, fastapi |
| `java` | Java | `java` | `tree-sitter-java` | `javac` | no | `benchmarks/fixtures/java/semantic-cases` | `benchmarks/specs/java-smoke-queries.json` | junit5, mockito, guava, retrofit, picocli |
| `kotlin` | Kotlin | `kt`, `kts` | `tree-sitter-kotlin-ng` | `kotlin_compiler_embeddable` | no | `benchmarks/fixtures/kotlin/semantic-cases` | `benchmarks/specs/kotlin-smoke-queries.json` | kotlinx-coroutines |
| `csharp` | C# | `cs`, `csx` | `tree-sitter-c-sharp` | `roslyn` | yes | `benchmarks/fixtures/csharp/semantic-cases` | `benchmarks/specs/csharp-smoke-queries.json` | newtonsoft_json, dapper, automapper, polly, serilog |
| `go` | Go | `go` | `tree-sitter-go` | `go_types` | yes | `benchmarks/fixtures/go/semantic-cases` | `benchmarks/specs/go-smoke-queries.json` | gin, cobra, prometheus, etcd, zap |
| `c-family` | C, C++ | `c`, `h`, `cc`, `cpp`, `cxx`, `hh`, `hpp`, `hxx` | `tree-sitter-c`, `tree-sitter-cpp` | `clang` | yes | `benchmarks/fixtures/c/semantic-cases`, `benchmarks/fixtures/cpp/semantic-cases` | `benchmarks/specs/c-smoke-queries.json`, `benchmarks/specs/cpp-smoke-queries.json` | redis, curl, sqlite, protobuf, nlohmann_json |
| `js-ts` | JavaScript, JSX, TypeScript, TSX | `cjs`, `cts`, `js`, `jsx`, `mjs`, `mts`, `ts`, `tsx` | `tree-sitter-javascript`, `tree-sitter-typescript` | `tsc` | yes | `benchmarks/fixtures/js-ts/semantic-cases` | `benchmarks/specs/js-ts-smoke-queries.json` | vite, redux, axios, express, prettier |
| `php` | PHP | `php` | `tree-sitter-php` | `nikic_php_parser` | yes | `benchmarks/fixtures/php/semantic-cases` | `benchmarks/specs/php-smoke-queries.json` | symfony-console |
| `ruby` | Ruby | `rb` | `tree-sitter-ruby` | `ruby_prism` | no | `benchmarks/fixtures/ruby/semantic-cases` | `benchmarks/specs/ruby-smoke-queries.json` | sinatra |
| `scala` | Scala | `scala`, `sc` | `tree-sitter-scala` | `scala_semanticdb` | no | `benchmarks/fixtures/scala/semantic-cases` | `benchmarks/specs/scala-smoke-queries.json` | utest |
| `swift` | Swift | `swift` | `tree-sitter-swift` | `sourcekit_lsp` | no | `benchmarks/fixtures/swift/semantic-cases` | `benchmarks/specs/swift-smoke-queries.json` | swift-nio |
| `dart` | Dart | `dart` | `tree-sitter-dart` | `dart_analyzer` with scan-only fallback | no | `benchmarks/fixtures/dart/semantic-cases` | `benchmarks/specs/dart-smoke-queries.json` | _(smoke only)_ |

The `Full-tier repos` column reflects the pinned semantic-graph corpus in
`benchmarks/corpus.json`. Separate graph-vs-no-graph eval scenarios under
`crates/squeezy-eval/fixtures/scenarios/benchmarks/` also exercise real-world
Scala on `akka/akka` and Dart on `flutter/flutter`.

Parser-only feature coverage can be summarized from parsed files with
`parser_feature_coverage_report`. The report groups each language's emitted
declaration kinds, import kinds, call kinds, reference kinds, body-hit kinds,
and confidence distribution without running graph resolution or benchmark
oracles. The report shape (`ParserFeatureCoverageReport` and its per-language
`ParserLanguageFeatureCoverage` rows) lives in
`crates/squeezy-parse/src/lib.rs`.

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

## Kotlin

Indexed: packages, imports (named/aliased/wildcard), classes, objects,
companion objects, interfaces, sealed types, enums and enum entries, methods,
secondary constructors, properties (top-level and class-level, `val`/`var`),
typealiases, primary-constructor properties (promoted to fields), extension
functions (receiver captured via `language_identity`), suspend / inline /
operator / infix / tailrec attributes, override / abstract / open flags,
anonymous object literals as Partial synthetic class symbols, and inheritance
through `delegation_specifiers` (`base:<parent>` attributes).

Known limitations: delegated properties expose the property and delegate target
call, but synthetic getter/setter accessor bodies are not generated. Generated
data-class members (`copy`, `componentN`, `equals`, `hashCode`, `toString`),
stable compiler identities for anonymous objects, overload resolution, and full
Kotlin type attribution stay heuristic for v0; the oracle suppresses the same
generated member set so the symbol-set gates remain symmetric. Multiplatform
`expect`/`actual` matching is not attempted.

Known follow-ups: deeper companion-object/member lookup beyond the current
`Host.member()` path, stable anonymous-object modeling, and a Kotlin LSP-based
navigation oracle are scoped for later work once the symbol-set gates remain
stable.

Oracle: JetBrains `kotlin-compiler-embeddable` PSI walker
(`benchmarks/oracle/kotlin/KotlinOracle.kt`). Build with
`benchmarks/oracle/kotlin/build.sh` (requires `kotlinc` + JDK 17).

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

## PHP

Indexed: namespaces, `use` imports (named, aliased, group, `use function`,
`use const`), classes, interfaces, traits, enums (including backed enums),
methods, properties, class constants, magic-method attribution, attribute
heads (`#[Foo]`), calls (direct, member, scoped), object creation, and
references. Trait inclusion (`use TraitA;`) is recorded as a `uses_trait`
attribute on the consuming class plus a Type reference to the trait.

Known limitations: dynamic class names (`new $cls`), variable variables
(`$$x`), `eval`, heredoc/nowdoc interpolations, and magic-method dispatch
(`__call`, `__get`, etc.) lower to Partial confidence or are excluded by
design. Trait conflict resolution (`insteadof`/`as`) is recorded as an
attribute but not modelled in detail. Inline HTML in mixed-template files
is surfaced as fallback body content, not graph confidence.

Known follow-ups: Symfony attribute heuristics beyond `#[Route]`,
`#[AsCommand]`, `#[AsController]`; composer autoload-aware resolution; and
a navigation oracle backed by phpactor LSP probes remain bounded
deferrals.

Oracle: nikic/PHP-Parser declaration scan invoked via a subprocess. CI attempts
to install PHP 8.3 + Composer and runs `composer install` in the oracle helper
directory; absent any of those, the oracle status is `skipped` and only
fixture-query truth gates run.

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

Oracle: Ruby Prism subprocess. CI attempts Ruby 3.3 setup with
`continue-on-error: true`; when Ruby or Prism is missing the oracle degrades to
a `mode = "scan-only"` self-compare, so the report mode/status is part of the
accuracy signal.

## Scala

Indexed: packages/imports, package objects, classes, case classes, objects,
traits, enums, enum cases, given definitions, extension methods, vals/vars,
constructor parameters, methods/functions, calls, infix calls, object creation
calls, annotations, type/path/field references, literal body hits, companion
attributes, and `language_identity` for extension receivers.

Known limitations: implicit conversions, `given`/`using` call-site resolution,
path-dependent type resolution, macro/inline expansion, overload resolution,
inferred extension receivers, and typeclass dispatch remain outside the
syntactic graph contract.

Oracle: Scala SemanticDB when the Scala toolchain can produce protobufs, with
scan-only fallback recorded in the report when SemanticDB is unavailable.
The pinned semantic-graph corpus includes `utest`; the graph-vs-no-graph
real-world eval scenarios also cover `akka/akka`.

## Swift

Indexed: classes, structs, actors, protocols, enums (with associated-value cases),
extensions (members carry `language_identity = ExtendedType` for cross-file
receiver resolution), `init` / `deinit` / `subscript`, computed and stored
properties (computed properties carry `swift:computed`), property wrappers
(`@Published` etc. as attribute references), `@MainActor` / `@objc` /
`@Sendable` attributes, generic constraints from both the type parameter clause
and `where` clauses, module imports (`import M`, `import struct M.T`), and
SwiftPM module hints derived from `Sources/<Module>/...` paths.

Known limitations: `@dynamicMemberLookup` runtime resolution, full protocol
witness tracking, Objective-C bridging (`.h`/`.m` siblings and `@objc(name)`
mappings), `#externalMacro`/`#freestanding` macro expansion, and SwiftPM
`Package.swift` parsing for module facts are deferred follow-ups. Closures
contribute body hits to their enclosing symbol but do not produce symbols of
their own.

Oracle: SourceKit-LSP when `sourcekit-lsp` or `SOURCEKIT_LSP` is available,
with syntactic scan-only fallback recorded in the report when the Swift
toolchain is absent or cannot launch. CI attempts to install the Swift toolchain
for the language benchmark path. macOS-only frameworks (`Combine`, `SwiftUI`,
`Network`) are intentionally absent from the fixture so the smoke run works on
`ubuntu-latest`.

## Dart

Indexed: libraries, classes (including sealed and abstract), mixins, mixin-class
declarations, extensions (anonymous and named), extension types, enums (with
enhanced-enum methods), top-level functions, methods, named constructors,
factory constructors, getters/setters, fields, typedefs, imports with prefixes
and `show`/`hide` combinators, exports, `part`/`part of` directives, async
modifiers, calls, type references, and library identifiers.

Known limitations: `noSuchMethod` runtime dispatch is excluded (mirrors Ruby's
`method_missing` stance). Conditional imports record both primary and
alternate URIs as separate imports; the resolver prefers the primary when both
exist. Generated `*.g.dart` / `*.freezed.dart` / `*.mocks.dart` files parse but
are excluded from oracle precision/recall accounting via glob.

Oracle: `package:analyzer` helper when the Dart oracle helper is present and
the analyzer run succeeds; otherwise the benchmark degrades to scan-only mode and
records that status in the report. The pinned semantic-graph corpus is currently
smoke-tier for Dart; the graph-vs-no-graph real-world eval scenarios also cover
`flutter/flutter`.

## Benchmark Corpus Reporting

The benchmark report is intentionally claim-ready for v0 validation. Each case
reports deterministic tool/cost metrics, grep-baseline query counts, wall time,
answer-quality counts, oracle precision/recall where available, and fallback
quality. Smoke fixtures for every supported family include generated and vendor
source paths; their specs assert those paths are surfaced as generated/vendor
fallback evidence rather than graph-confident answers.

Contributor steps for adding or changing a language family live in
[`../../../docs/internal/ARCHITECTURE.md`](../../../docs/internal/ARCHITECTURE.md).
Benchmark workflow and gate details live in
[`../../../docs/internal/BENCHMARKS.md`](../../../docs/internal/BENCHMARKS.md).
