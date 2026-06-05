# Ruby Language Implementation Contract

Ruby support is implemented for syntactic semantic-graph navigation. This file
documents the current contract for the parser, extractor, graph resolver,
fixtures, oracle, gates, and known limitations.

## Source Of Truth

- Parser wiring: `crates/squeezy-parse/src/lib.rs` registers
  `tree-sitter-ruby = "0.23"` through `LanguageKind::Ruby`.
- Extractor: `crates/squeezy-parse/src/languages/ruby.rs`.
- Graph resolver: `crates/squeezy-graph/src/languages/ruby.rs`.
- Benchmark fixture: `benchmarks/fixtures/ruby/semantic-cases/`.
- Smoke spec: `benchmarks/specs/ruby-smoke-queries.json`.
- Oracle: `benchmarks/squeezy-graph-bench/src/oracles/ruby_oracle.rs`.
- Corpus entries: `ruby-smoke` and `sinatra` in `benchmarks/corpus.json`.

## Extracted Facts

The extractor emits class, module, method, singleton-method, top-level
function, constant, and field symbols from `class`, `module`, `def`,
`def self.foo`, `class << self`, constant assignment, and instance/class
variable assignment forms.

Ruby-specific extraction includes:

- `attr_reader`, `attr_writer`, `attr_accessor`, and `attr` method synthesis
  with `ruby:attr` and `ruby:synthesized` attributes.
- `include`, `extend`, and `prepend` as type references plus mixin attributes
  on the host class or module.
- `require`, `require_relative`, `load`, and `autoload` imports.
- Method calls, singleton calls, bare identifier references, constants, and
  `Foo::Bar` paths.
- Literal body hits while skipping heredoc bodies and still descending into
  interpolation/block expression bodies.

## Graph Behavior

The Ruby graph resolver keeps Ruby matching syntactic and conservative:

- `require_relative` and `autoload` can match workspace files; plain `require`
  remains a gem/load-path style import.
- Ancestor lookup walks superclass and mixin attributes with a bounded depth.
- `prepend` and `include` participate in instance-method lookup; `extend`
  participates in singleton/class method lookup.
- Attribute-synthesized methods are visible to search and graph queries but are
  excluded from Ruby oracle precision/recall accounting.

## Oracle And Gates

The benchmark oracle runs Ruby with Prism when available. If the Ruby toolchain
or Prism is unavailable, the report degrades to scan-only mode instead of
failing benchmark collection.

The active gate in `benchmarks/squeezy-graph-bench/src/gates.rs` requires Ruby
oracle precision >= 0.90 and recall >= 0.75 when the benchmark gate is active.
The gate accepts lower recall than static languages because dynamic dispatch is
a real language limitation for a syntactic graph.

## Known Limits

- `define_method` declarations are not emitted as symbols.
- `method_missing`, `respond_to_missing?`, and similar runtime dispatch are not
  modeled beyond their own method declarations.
- `eval`, `instance_eval`, `class_eval`, and `module_eval` string bodies are
  not parsed for symbols or references.
- `$LOAD_PATH` and gem resolution are not modeled for plain `require`.
- Constant aliasing and anonymous classes created through runtime expressions
  remain syntactic recall gaps.
