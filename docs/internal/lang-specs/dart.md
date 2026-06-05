# Dart Language Implementation Contract

Dart support is implemented for syntactic semantic-graph navigation. This file
documents the current contract for the parser, extractor, graph resolver,
fixtures, oracle, gates, and known limitations.

## Source Of Truth

- Parser wiring: `crates/squeezy-parse/src/lib.rs` maps `LanguageKind::Dart`
  to `tree_sitter_dart::LANGUAGE`; the grammar version is pinned as
  `tree-sitter-dart = "0.2.0"` in the workspace `Cargo.toml`.
- Extractor: `crates/squeezy-parse/src/languages/dart.rs`.
- Graph resolver: `crates/squeezy-graph/src/languages/dart.rs`.
- Benchmark fixture: `benchmarks/fixtures/dart/semantic-cases/`.
- Smoke spec: `benchmarks/specs/dart-smoke-queries.json`.
- Oracle helper: `benchmarks/oracle-helpers/dart-oracle/`.
- Oracle integration:
  `benchmarks/squeezy-graph-bench/src/oracles/dart_oracle.rs`.
- Corpus entry: `dart-smoke` in `benchmarks/corpus.json`.

## Extracted Facts

The extractor emits library markers, imports, exports, part/part-of directives,
classes, mixins, extensions, extension types, enums, enum constants, functions,
methods, getters, setters, constructors, factory constructors, fields, top-level
variables, typedefs, calls, constructor invocations, type/path/identifier
references, annotations, and literal body hits.

Dart-specific extraction includes:

- Library `part` / `part of` relationships with sentinel imports.
- Mixins, `on` constraints, `with` clauses, and interface lists as resolver
  metadata.
- Extension and extension-type declarations with receiver/representation
  metadata.
- Named and factory constructors, getters, setters, async/generator attributes,
  and conditional imports.
- String interpolation descent so calls and references inside interpolations
  remain visible.

## Graph Behavior

The Dart graph resolver handles same-library visibility for `part` files,
inherited methods, mixin method lookup, extension-method dispatch, import-prefix
method calls, typed-local method calls, and library lookup. It stays syntactic
and does not run the Dart analyzer for graph construction.

## Oracle And Gates

The Dart oracle uses the analyzer helper under
`benchmarks/oracle-helpers/dart-oracle/` when the Dart SDK and helper
dependencies are available. If analyzer mode is unavailable, the report
degrades to scan-only mode.

The active gate in `benchmarks/squeezy-graph-bench/src/gates.rs` requires Dart
oracle precision >= 0.93 and recall >= 0.85 only when the oracle ran in analyzer
mode.

## Known Limits

- Runtime dispatch through `noSuchMethod` is not modeled.
- Generated files such as `*.g.dart`, `*.freezed.dart`, and `*.mocks.dart` are
  parsed but excluded from oracle precision/recall accounting.
- Conditional imports preserve alternate targets with partial confidence rather
  than platform-specific resolution.
- Type inference, generic instantiation, and overload-like analyzer behavior are
  outside the syntactic graph contract.
- Flutter/platform-channel semantics are represented as ordinary calls and
  imports, not as framework-aware edges.
