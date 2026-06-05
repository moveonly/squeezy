# Scala Language Implementation Contract

Scala support is implemented for syntactic semantic-graph navigation. This file
documents the current contract for the parser, extractor, graph resolver,
fixtures, oracle, gates, and known limitations.

## Source Of Truth

- Parser wiring: `crates/squeezy-parse/src/lib.rs` registers
  `tree-sitter-scala = "0.26"` through `LanguageKind::Scala`.
- Extractor: `crates/squeezy-parse/src/languages/scala.rs`.
- Graph resolver: `crates/squeezy-graph/src/languages/scala.rs`.
- Benchmark fixture: `benchmarks/fixtures/scala/semantic-cases/`.
- Smoke spec: `benchmarks/specs/scala-smoke-queries.json`.
- Oracle helper: `benchmarks/oracle/scala/`.
- Oracle integration:
  `benchmarks/squeezy-graph-bench/src/oracles/scala_semanticdb.rs`.
- Corpus entries: `scala-smoke` and `utest` in `benchmarks/corpus.json`.

## Extracted Facts

The extractor emits package markers, imports, package objects, classes, case
classes, objects, traits, enums, enum cases, type aliases, functions, methods,
given definitions, extension methods, values, variables, constructor
parameters, calls, infix calls, object creation calls, type/path/field
references, annotations, and literal body hits.

Scala-specific extraction includes:

- Scala 3 package/import forms, including wildcard and selector imports.
- Companion-object attributes when a class-like symbol and object share a name.
- Case-class fields and normalized synthetic peers used for oracle comparison.
- Extension methods with receiver type stored in `language_identity`.
- `given`, `using`, `inline`, `opaque`, `case-class`, and `scala:object`
  attributes.

## Graph Behavior

The Scala graph resolver handles package/import visibility, companion-object
method lookup, extension-method dispatch, and owner-path matching. It stays
syntactic and does not perform implicit search, overload resolution, or Scala
type inference.

## Oracle And Gates

The Scala oracle uses SemanticDB when the Scala toolchain can produce and read
SemanticDB protobufs. If that path fails, the report records scan-only fallback
status.

The active gate in `benchmarks/squeezy-graph-bench/src/gates.rs` requires Scala
oracle precision >= 0.90 and recall >= 0.75 only when the SemanticDB oracle ran
end to end. Scan-only fallback does not activate the oracle precision/recall
gate.

## Known Limits

- Implicit conversions and `given`/`using` call-site resolution are not modeled.
- Path-dependent types are emitted syntactically and not fully resolved.
- Macro and inline expansion is not modeled beyond attributes and syntactic
  body hits.
- Overloaded calls, inferred extension receivers, and typeclass dispatch remain
  outside the syntactic graph contract.
- SemanticDB availability depends on the local/CI Scala toolchain.
