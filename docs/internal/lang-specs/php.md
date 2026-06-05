# PHP Language Implementation Contract

PHP support is implemented for syntactic semantic-graph navigation. This file
documents the current contract for the parser, extractor, graph resolver,
fixtures, oracle, gates, and known limitations.

## Source Of Truth

- Parser wiring: `crates/squeezy-parse/src/lib.rs` maps `LanguageKind::Php`
  to `tree_sitter_php::LANGUAGE_PHP`; the grammar version is pinned as
  `tree-sitter-php = "0.24"` in the workspace `Cargo.toml`.
- Extractor: `crates/squeezy-parse/src/languages/php.rs`.
- Graph resolver: `crates/squeezy-graph/src/languages/php.rs`.
- Benchmark fixture: `benchmarks/fixtures/php/semantic-cases/`.
- Smoke spec: `benchmarks/specs/php-smoke-queries.json`.
- Oracle helper: `benchmarks/oracle-helpers/php-oracle/`.
- Oracle integration: `benchmarks/squeezy-graph-bench/src/oracles/php_oracle.rs`.
- Corpus entries: `php-smoke` and `symfony-console` in
  `benchmarks/corpus.json`.

## Extracted Facts

The extractor emits namespace/module, class, interface, trait, enum, enum-case,
function, method, property, and class-constant symbols.

PHP-specific extraction includes:

- File-scoped and braced namespaces as module/package context.
- `use` imports, grouped imports, aliases, `use function`, and `use const`.
- Class inheritance, implemented interfaces, trait inclusion, and trait
  conflict-resolution metadata.
- PHP 8 attributes as symbol attributes and searchable attribute references.
- Function calls, member calls, static calls, and object creation calls.
- Typed properties, class constants, backed enum attributes, magic-method
  attributes, and mixed inline HTML body hits.

## Graph Behavior

The PHP graph resolver matches imported and namespace-qualified symbols,
resolves method calls on known classes, and walks class ancestry plus trait
inclusion with bounded recursion. Dynamic class instantiation and magic
dispatch are retained as syntactic calls but have lower confidence than direct
class and method declarations.

## Oracle And Gates

The PHP oracle helper is a Composer-backed PHP parser/collector under
`benchmarks/oracle-helpers/php-oracle/`. The benchmark runner uses it when the
PHP toolchain and helper dependencies are available.

The active gate in `benchmarks/squeezy-graph-bench/src/gates.rs` requires PHP
oracle precision >= 0.92 and recall >= 0.80 when the oracle ran. Missing PHP
tooling does not mask smoke-query failures; it only suppresses oracle
precision/recall gating.

## Known Limits

- Top-level `const` declarations are intentionally lower-priority than class
  constants and may not be represented with the same fidelity.
- Variable variables and `eval(...)` bodies are not expanded into symbols.
- Runtime magic dispatch through `__call`, `__callStatic`, `__get`, `__set`,
  and `__invoke` is not resolved as a type-checked call graph.
- Composer autoload metadata is not a full type resolver; namespace and import
  matching stay syntactic.
- Trait conflict aliases are recorded as metadata, not as separate method
  declarations.
