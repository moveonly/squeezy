# Kotlin Language Implementation Contract

Kotlin support is implemented for syntactic semantic-graph navigation. This
file documents the current contract for the parser, extractor, graph resolver,
fixtures, oracle, gates, and known limitations.

## Source Of Truth

- Parser wiring: `crates/squeezy-parse/src/lib.rs` maps `LanguageKind::Kotlin`
  to `tree_sitter_kotlin_ng::LANGUAGE`; the grammar version is pinned as
  `tree-sitter-kotlin-ng = "1.1"` in the workspace `Cargo.toml`.
- Extractor: `crates/squeezy-parse/src/languages/kotlin.rs`.
- Graph resolver: `crates/squeezy-graph/src/languages/kotlin.rs`.
- Benchmark fixture: `benchmarks/fixtures/kotlin/semantic-cases/`.
- Smoke spec: `benchmarks/specs/kotlin-smoke-queries.json`.
- Oracle helper: `benchmarks/oracle/kotlin/KotlinOracle.kt` and
  `benchmarks/oracle/kotlin/build.sh`.
- Oracle integration:
  `benchmarks/squeezy-graph-bench/src/oracles/kotlin_oracle.rs`.
- Corpus entries: `kotlin-smoke` and `kotlinx-coroutines` in
  `benchmarks/corpus.json`.

## Extracted Facts

The extractor emits package markers, imports, classes, objects, companion
objects, interfaces, enums, enum entries, constructors, functions, methods,
properties, type aliases, calls, type references, navigation references, and
annotations.

Kotlin-specific extraction includes:

- Top-level functions and properties under the file package.
- Companion-object members as static-like callable members.
- Extension functions with receiver type stored in `language_identity`.
- `suspend`, `inline`, `reified`, `data`, `object`, and delegated-property
  attributes.
- Constructor calls, property-delegate calls, navigation expressions, and
  annotation references.
- Delegated-property target calls, sealed-parent references for nested sealed
  children, and `inline reified` type-parameter metadata.
- Anonymous object literals as partial synthetic class symbols with their
  declared members parented underneath.

## Graph Behavior

The Kotlin graph resolver handles package/import visibility, extension-function
dispatch by receiver type, and companion-member dispatch. It follows syntactic
ownership and imports rather than doing JVM or Kotlin type inference.

## Oracle And Gates

The Kotlin oracle is a JetBrains compiler-embeddable PSI walker packaged by
`benchmarks/oracle/kotlin/build.sh`. The generated jar is not committed. If the
jar or toolchain is unavailable, the benchmark report records a skipped oracle
status while still running the smoke fixture queries.

The active gate in `benchmarks/squeezy-graph-bench/src/gates.rs` requires
Kotlin oracle precision >= 0.94 and recall >= 0.85 when the oracle ran.

## Known Limits

- Generated data-class methods such as `copy` and `componentN` are not modeled
  as first-class graph declarations, although the data-class declaration and
  primary-constructor properties are emitted.
- Delegated properties expose the property and delegate target call, but
  synthetic getter/setter accessor bodies are not modeled.
- Sealed-class child references are syntactic; exhaustiveness is not
  type-checked.
- Anonymous object expressions are emitted with synthetic names and Partial
  confidence rather than stable compiler identities.
- Generic, nullable, nested, or inferred extension receiver types may lower
  confidence or miss call-site matching.
- The graph does not resolve overloaded calls through Kotlin compiler type
  attribution.
