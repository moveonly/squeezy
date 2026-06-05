# Website Language Support Research

Status: current-tree research for website copy. Last checked in this checkout on
2026-06-05. Do not treat this file as the user-facing language reference; the
checked user-facing source remains
`crates/squeezy-skills/external-docs/LANGUAGES.md`.

## Evidence Checked

- `crates/squeezy-core/src/lib.rs`: `LanguageKind`, `LanguageFamily`, extension
  mapping, and graph tool defaults.
- `crates/squeezy-parse/src/lib.rs`: parser registry and unsupported-file
  fallback behavior.
- `crates/squeezy-graph/src/lib.rs`: graph data model, query operations,
  refresh behavior, language project facts, and traversal surface.
- `crates/squeezy-tools/src/specs.rs`: agent-facing graph tool descriptions.
- `crates/squeezy-skills/external-docs/LANGUAGES.md`: current user-facing
  language coverage matrix and per-language limitations.
- `docs/internal/SEMANTIC_GRAPH.md` and `docs/internal/BENCHMARKS.md`: graph
  policy, operations, and benchmark/oracle details.
- `benchmarks/corpus.json`, `benchmarks/specs/*-smoke-queries.json`,
  `benchmarks/baselines/*.json`, and `benchmarks/squeezy-graph-bench/src/*`:
  benchmark cases, oracle inventory, query specs, baselines, and mixed-workload
  support.

Note: `docs/external/` is not present in this checkout. External/user-facing
language docs are currently embedded under `crates/squeezy-skills/external-docs/`.

## High-Level Finding

Squeezy currently has first-class tree-sitter parser registration and semantic
graph extraction for 13 language families covering 17 source variants:

- Rust
- Python
- Java
- Kotlin
- Scala
- C#/.NET
- Go
- C/C++
- JavaScript/TypeScript, including JSX/TSX
- PHP
- Ruby
- Swift
- Dart

This is graph-backed navigation, not compiler-perfect semantic analysis. The
production path is local parsing plus deterministic resolver heuristics. LSPs,
language services, compilers, and runtime analyzers are benchmark oracles or
explicit fact-refresh sources, not production navigation dependencies.

Unsupported or unknown languages should be described as bounded fallback support
only: Squeezy can still use list/grep/read tools, but those files must not be
marketed as graph-confident semantic navigation.

## Implemented Operations

Shared graph operations implemented at a high level:

| Operation | User-facing meaning | Caveat |
|---|---|---|
| Repository map | Compact architecture map, language counts, indexed coverage, unsupported samples, and next graph actions. | Coverage is tied to the crawler/indexing policy; excluded generated/vendor/dependency files are reported as fallback evidence. |
| Declaration and definition search | Search graph-backed declarations by name/signature, kind, language, path, visibility, or attributes; resolve likely defining symbols. | Exactness depends on parser coverage and resolver confidence. Ambiguous names return candidates. |
| Containment hierarchy | File to module/class/type/member hierarchy, with bounded depth. | This is containment, not inheritance. Inheritance/mixin queries use declaration attributes such as `base:`, `iface:`, or `mixin:`. |
| Reference search | Name and symbol-bound reference lookup through graph references, alias handling, paths, and reexports where implemented. | Broad text reference search remains heuristic; symbol-bound references are stronger than bare-name references. |
| Callers, callees, and call chains | Upstream/downstream flow, direct callers/callees, bounded BFS, and explicit call-chain search when a target is supplied. | Dynamic dispatch, overloads, macro expansion, framework magic, and runtime reflection remain language-specific caveats. |
| Body search | Search scoped body hits such as identifiers, type names, paths, calls, macro invocations, literals, and attributes. | Body hits are intentionally heuristic and scoped to nearest owner. |
| Read slice | Exact source slices by symbol id, signature/body span, byte range, line range, or diff baseline. | Graph spans make reads cheaper, but unsupported files fall back to ordinary bounded reads. |
| Incremental refresh | Changed-file refresh, parser cache reuse, persisted graph partitions, dirty symbols, and bounded per-query refresh budgets. | A budget-exhausted refresh may settle over multiple graph calls; reports expose that state. |
| Project facts | Rust Cargo facts on explicit refresh; Java, Kotlin, and .NET project metadata facts from local files. | Compiler/build tools are not run implicitly by navigation tools. Cached Cargo diagnostics can become stale. |

Agent-facing tools that expose the graph: `repo_map`, `decl_search`,
`definition_search`, `reference_search`, `upstream_flow`, `downstream_flow`,
`symbol_context`, `hierarchy`, `read_slice`, and `plan_patch`.

## Language Matrix

Maturity labels for website planning:

- `Strong`: smoke + oracle + full-tier or mixed coverage is present and the
  public caveats are conventional for local static analysis.
- `Solid`: parser/extractor and smoke coverage are present, with an oracle or
  full-tier path, but public copy should avoid precision-heavy language.
- `Emerging`: first-class parser/extractor exists, but current evidence has
  scan-only/deferred, missing full-tier, or doc inconsistency caveats.

| Family | Variants / extensions | Maturity | Implemented indexing summary | Benchmark / oracle status | Public caveats |
|---|---|---:|---|---|---|
| Rust | `rs` | Strong | Modules, structs, enums, unions, traits, impls, functions, methods, const/static/type aliases, macros, tests, imports, references, calls, body hits, Cargo facts on explicit refresh. | Smoke + full repos: ripgrep, fd, bat, tokio, serde. Mixed workload yes. Oracle: rust-analyzer symbols and sampled LSP probes; Cargo check timing. | No macro expansion, conservative cfg/features, external crates/std roots stay external, Cargo is explicit verification/fact refresh only. |
| Python | `py` | Solid | Classes, functions, methods, imports, calls, decorators, docstrings, class bases, annotations, fields, exports, aliases, references. | Smoke + full repos: requests, flask, click, black, fastapi. Mixed workload no. Oracle: CPython `ast` declaration comparison. | Dynamic attributes, metaclasses, import side effects, monkey-patching, framework magic, and receiver type inference are heuristic or out of scope. |
| Java | `java` | Solid | Packages, imports, classes, interfaces, enums, records, annotations, methods, constructors, fields, inheritance/implements, calls, references, Maven/Gradle facts. | Smoke + full repos: junit5, mockito, guava, retrofit, picocli. Mixed workload no. Oracle: JDK compiler tree scan when available plus query specs. | No compiler-equivalent overload resolution, runtime dispatch, reflection, annotation processor, generated-source, or full classpath claims. |
| Kotlin | `kt`, `kts` | Solid | Packages, imports, classes, objects, companion objects, interfaces, sealed types, enums, methods, constructors, properties, typealiases, primary-constructor properties, extension receivers, modifiers, inheritance, Maven/Gradle facts. | Smoke + full repo: kotlinx-coroutines. Mixed workload no. Oracle: JetBrains kotlin-compiler-embeddable PSI walker when JDK + built jar are available; query gates still run when skipped. | Data-class/generated members, delegated properties, overload resolution, anonymous objects, multiplatform `expect`/`actual`, and type solving are not compiler-equivalent. |
| Scala | `scala`, `sc` | Emerging | Classes, traits, objects, case classes, methods, values, packages/imports, inheritance-style attributes, references/calls covered by fixture specs. | Smoke + full corpus entry: utest. Mixed workload no. Oracle: scalac SemanticDB, with scan-only fallback if unavailable. Baseline records 1.0 precision/recall on smoke fixture. External language doc still says full-tier repos are deferred, so docs should be reconciled before stronger public claims. | Implicit/given resolution at call sites, macro-expanded members, anonymous classes/lambda bodies, locals, params, type params, and SemanticDB synthetic members are not public claims. |
| C#/.NET | `cs`, `csx` | Strong | Namespaces, usings, classes, interfaces, records, structs, enums, methods, constructors, operators, fields, properties, events, enum members, attributes, calls, references, partial-type links, inheritance/implements, `.csproj`/`.sln`/`Directory.Build.*`/lock/global facts. | Smoke + full repos: newtonsoft_json, dapper, automapper, polly, serilog. Mixed workload yes. Oracle: Roslyn declaration symbols plus syntactic extends/implements; `dotnet build` validation is reporting-oriented. | No compiler-equivalent generic constraints, overloads, extension-method binding, MSBuild behavior, generated code flow, dynamic dispatch, Razor/Blazor embedded C# confidence. |
| Go | `go` | Strong | Packages, imports, structs, interfaces, type aliases, functions, methods, receiver relationships, fields, constants, variables, tests, calls, references. | Smoke + full repos: gin, cobra, prometheus, etcd, zap. Mixed workload yes. Oracle: Go parser/types script in benchmark binary. | Interface satisfaction, full receiver type inference, embedded field promotion, build tags, generated code, and external modules are heuristic or external. |
| C/C++ | `c`, `h`, `cc`, `cpp`, `cxx`, `hh`, `hpp`, `hxx` | Strong | Includes, namespaces, classes, structs, unions, enums, typedefs/type aliases, fields, functions, methods, constructors/destructors, operators, templates, macro definitions/usages, declaration/definition spans. | Smoke for C and C++; full repos: redis, curl, sqlite, protobuf, nlohmann_json. Mixed workload yes. Oracle: clang syntax and clang AST JSON; full corpus samples oracle files. | No preprocessor expansion, overload/template instantiation, virtual dispatch, ADL, full build flags, generated headers, compile database, or cross-translation-unit certainty. |
| JavaScript/TypeScript | `cjs`, `cts`, `js`, `jsx`, `mjs`, `mts`, `ts`, `tsx` | Strong | Functions, named arrows, classes, methods, class-property arrows, fields, interfaces, modules/namespaces, decorators, type aliases, enums, imports/exports, CommonJS aliases, JSX/TSX components, calls, member calls, type references, object/member references. | Smoke + full repos: vite, redux, axios, express, prettier. Mixed workload yes. Oracle: TypeScript compiler API and sampled language-service definition/reference probes when Node/TypeScript are available. | Dynamic imports, computed property access, bundler aliases without checked config, package export edge cases, runtime dispatch, and full TypeScript type evaluation are heuristic or external. |
| PHP | `php` | Strong | Namespaces, `use` imports, classes, interfaces, traits, enums, methods, properties, constants, magic-method attribution, attributes, direct/member/scoped calls, object creation, references, trait-use edges. | Smoke + full repo: symfony-console. Mixed workload yes. Oracle: nikic/PHP-Parser subprocess when PHP + Composer helper deps are available; query gates still run when skipped. Corpus rev currently has a `TBD` pin note for symfony-console. | Dynamic class names, variable variables, `eval`, heredoc/nowdoc interpolation, magic dispatch, detailed trait conflict resolution, Composer autoload resolution, and inline HTML graph confidence are not claims. |
| Ruby | `rb` | Solid | Classes, modules, instance/singleton methods, `class << self`, top-level functions, synthesized `attr_*` accessors, require/load/autoload imports, include/extend/prepend mixins, constants, class/instance vars, calls, references. | Smoke + full repo: sinatra. Mixed workload no. Oracle: Ruby Prism subprocess with scan-only fallback. Baseline records 1.0 precision/recall on smoke fixture. | `method_missing`, `define_method`, eval-family methods, anonymous classes, runtime monkey-patching, gem require resolution, and typed receiver dispatch are not guaranteed. |
| Swift | `swift` | Emerging | Classes, structs, actors, protocols, enums/cases, extensions with receiver identity, init/deinit/subscript, computed/stored properties, property wrappers, attributes, generic constraints, module imports, SwiftPM module hints. | Smoke + full repo: swift-nio. Mixed workload no. Oracle: SourceKit-LSP documentSymbol plus sampled definition/reference probes when available; validation oracle not run in first-iteration CI. Baseline is darwin/arm64, Xcode 15 Swift 5.10. | Dynamic member lookup, protocol witness tracking, Obj-C bridging, macro expansion, SwiftPM `Package.swift` parsing, and closure symbols are deferred or body-hit only. |
| Dart | `dart` | Emerging | Libraries, classes, sealed/abstract types, mixins, mixin classes, extensions, extension types, enums, functions, methods, named/factory constructors, getters/setters, fields, typedefs, imports/exports/parts, async modifiers, calls, type refs, library IDs. | Smoke only in current corpus. Mixed workload no. Oracle descriptor and analyzer helper exist, and baseline records analyzer-mode 1.0 precision/recall on smoke fixture; external language doc still says the Dart oracle is deferred, so reconcile before strong public copy. | `noSuchMethod` runtime dispatch is excluded, conditional import resolution is bounded, generated Dart files are parsed but excluded from oracle precision/recall by glob, and full-tier real-repo evidence is not yet in `corpus.json`. |

## Benchmark / Oracle Status Summary

- Every listed family has a smoke fixture and smoke query spec.
- Families with mixed workload support in the benchmark CLI: Rust, C#, Go,
  C/C++, JavaScript/TypeScript, and PHP.
- Full-tier corpus entries are present for every family except Dart. Some
  current entries are intentionally reporting-oriented or have `no_speed_gate`.
- Oracle descriptors exist for all 13 families: rust-analyzer, CPython AST,
  javac, kotlin compiler embeddable, Scala SemanticDB, Roslyn, Go types, clang,
  TypeScript compiler API, nikic/PHP-Parser, Ruby Prism, SourceKit-LSP, and Dart
  analyzer.
- Several oracles degrade gracefully when local toolchains are unavailable.
  Public copy should say "validated against benchmark oracles" only at the
  platform level, not imply every end-user checkout runs those tools.
- Checked baseline files currently exist for Dart, Ruby, Scala, and Swift.
  They are useful evidence for fixture maturity, but they are not broad
  real-world proof by themselves.

## Public Caveats To Preserve

- Squeezy uses local tree-sitter-backed navigation first. It does not run LSPs,
  rust-analyzer, TypeScript language service, Roslyn, SourceKit, Dart analyzer,
  or compilers on the production navigation path.
- Compiler/runtime tools are for benchmarks, explicit verification, or explicit
  fact refresh. They are not hidden dependencies for ordinary graph queries.
- Graph answers carry confidence and provenance. Same-name lexical hits,
  ambiguous dispatch, glob imports, generated files, unsupported file types, and
  runtime-heavy language features must be framed honestly.
- Unsupported languages and unsupported file types are still usable via bounded
  list/grep/read flows, but they do not receive graph confidence.
- Generated, vendor, dependency, build-output, binary, lockfile, hidden, and
  large files are often excluded or treated as fallback evidence by policy.
- Avoid the phrases "compiler-perfect", "full type inference", "complete
  runtime dispatch", "all frameworks", or "LSP-backed" for the website.
- Prefer "semantic navigation" and "graph-backed code navigation" over bare
  "semantic graph" on marketing surfaces; keep implementation terms in docs.

## Exact Website Messaging Bullets

Safe homepage / language-page bullets:

- "Graph-backed navigation for Rust, Python, Java, Kotlin, Scala, C#/.NET, Go,
  C/C++, JavaScript/TypeScript, PHP, Ruby, Swift, and Dart."
- "Squeezy indexes declarations, imports, references, calls, containment
  hierarchy, and scoped body hits before spending model context."
- "Use local code structure first: find definitions, callers, callees,
  references, and exact source slices without broad grep-and-read loops."
- "Unsupported languages still work through bounded file search and reads; they
  are not assigned semantic graph confidence."
- "Benchmarks validate each supported family with smoke fixtures and
  language-specific oracles where toolchains are available."
- "Compiler and language-server checks are validation oracles, not hidden
  navigation dependencies."

More detailed docs-page bullets:

- "The production navigation path is tree-sitter plus deterministic local
  resolution. LSPs and compiler APIs are used in benchmarks to measure what the
  graph misses."
- "Graph results include confidence, freshness, and provenance so ambiguous or
  stale evidence is visible instead of being flattened into a false exact
  answer."
- "Generated/vendor/dependency files are reported as coverage or fallback
  evidence; Squeezy does not pretend ignored paths are high-confidence graph
  answers."
- "For dynamic language features such as monkey-patching, magic dispatch,
  reflection, macro expansion, or framework runtime wiring, Squeezy keeps the
  answer bounded and marks the limitation instead of claiming complete
  resolution."

Language-count phrasing:

- Use: "13 language families, 17 source variants."
- Use: "Rust; Python; Java; Kotlin; Scala; C#/.NET; Go; C/C++;
  JavaScript/TypeScript; PHP; Ruby; Swift; Dart."
- Avoid: "Every major language" or "universal language support."

Short caveat line for the language page:

- "Coverage means graph-backed parsing and local navigation for supported file
  types. Unsupported languages fall back to bounded search/read tools, without
  graph confidence."

## Copy Risk Notes

- `docs/internal/SEMANTIC_GRAPH.md` still contains a paragraph listing only the
  original family set: Rust, Python, Java, C#, Go, C/C++, and JS/TS. Do not copy
  that list to the website without updating it from `LanguageFamily::ALL`.
- `crates/squeezy-skills/external-docs/LANGUAGES.md` is mostly aligned with the
  live registry via `benchmarks/scripts/check_languages_doc.py`, but its Scala
  and Dart rows/descriptions appear behind current benchmark files: Scala now
  has a full corpus entry, and Dart has a live analyzer oracle/baseline even
  though the doc still says deferred.
- PHP's full corpus entry currently has a `TBD` repo revision note. Avoid a
  public claim that depends on a pinned Symfony revision until that is cleaned
  up.
- Swift and Dart should be shown as supported, but avoid presenting them as the
  strongest evidence examples until broader full-tier/oracle wording is
  reconciled.
