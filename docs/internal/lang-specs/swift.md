# Swift Language Implementation Contract

Swift support is implemented for syntactic semantic-graph navigation. This file
documents the current contract for the parser, extractor, graph resolver,
fixtures, oracle, gates, and known limitations.

## Source Of Truth

- Parser wiring: `crates/squeezy-parse/src/lib.rs` registers
  `tree-sitter-swift = "0.7.3"` through `LanguageKind::Swift`.
- Extractor: `crates/squeezy-parse/src/languages/swift.rs`.
- Graph resolver: `crates/squeezy-graph/src/languages/swift.rs`.
- Benchmark fixture: `benchmarks/fixtures/swift/semantic-cases/`.
- Smoke spec: `benchmarks/specs/swift-smoke-queries.json`.
- Oracle integration:
  `benchmarks/squeezy-graph-bench/src/oracles/swift_sourcekit.rs`.
- Corpus entries: `swift-smoke` and `swift-nio` in `benchmarks/corpus.json`.

## Extracted Facts

The extractor emits imports, classes, structs, actors, protocols, enums, enum
cases, extensions, free functions, methods, initializers, deinitializers,
subscripts, properties, type aliases, calls, navigation references, type
references, attribute references, and literal body hits.

Swift-specific extraction includes:

- Extension members with the extended type stored in `language_identity`.
- Protocol conformance and inheritance as type references and `base:<Name>`
  attributes.
- Property-wrapper attributes while excluding synthesized `$x` and `_x`
  storage from the public symbol contract.
- Computed properties as fields, not getter/setter method symbols.
- Actor, initializer, deinitializer, subscript, `@MainActor`, `@objc`,
  `@Sendable`, and `@available` metadata.

## Graph Behavior

The Swift graph resolver handles import/module matching and extension-method
lookup by matching receiver type to extension `language_identity`. It does not
use SwiftPM build metadata or SourceKit type-checking for graph construction.

## Oracle And Gates

The Swift oracle uses SourceKit-LSP `textDocument/documentSymbol` when
`sourcekit-lsp` is on `PATH` or configured through `SOURCEKIT_LSP`. If
SourceKit-LSP is unavailable, the benchmark report falls back to a syntactic
Squeezy scan and records that status.

The active gate in `benchmarks/squeezy-graph-bench/src/gates.rs` enforces:

- symbol precision >= 0.92 and recall >= 0.80 when comparable symbols exist;
- definition precision >= 0.85 when definition probes ran;
- reference precision >= 0.80 only when the reference sample is large enough to
  avoid noisy tiny-denominator failures.

## Known Limits

- SwiftPM package and target facts are not part of the graph resolver.
- Macros, result builders, generated members, and property-wrapper projected
  storage are not expanded.
- Closure-local symbols are not emitted.
- Type inference, overload resolution, async actor isolation, and protocol
  witness matching remain outside the syntactic graph contract.
- SourceKit-LSP availability and indexing quality affect oracle coverage.
