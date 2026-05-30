# Ruby language-implementation spec

Target branch: `langs/ruby`. Scaffold (`LanguageKind::Ruby`, `LanguageFamily::Ruby`, extension `.rb`, placeholder `extract_ruby`) already landed; this spec defines the follow-up implementation commit.

## 1. Template choice

Closest existing language is **Python**. Ruby and Python share the salient shapes we model: dynamic dispatch with no compile-time type tags, duck typing so receiver/argument types are not authoritative, and method-on-class hierarchies that map cleanly onto `SymbolKind::Class` + `SymbolKind::Method` parent/child pairs. The Python extractor (`crates/squeezy-parse/src/languages/python.rs`) and Python graph resolver (`crates/squeezy-graph/src/languages/python.rs`) are the right starting templates; the Ruby resolver will reuse the same "receiver alias → class → method on class or bases" lookup shape that `python_class_for_alias` / `python_method_on_class_or_bases` implement, swapping module/include semantics for Python's MRO.

## 2. Grammar

`tree-sitter-ruby = "0.23"` is already a workspace dep in `Cargo.toml`. Notable quirks the visitor has to tolerate:

- Block syntax has two forms (`do ... end` and `{ ... }`); both parse to `do_block` / `block` and must be skipped as symbol containers but **descended** for nested calls and references.
- Heredocs (`<<~SQL`, `<<-EOT`) produce `heredoc_beginning` plus a body node; body content is not source code we should extract literals or references from. Skip the body subtree.
- `attr_accessor`/`attr_reader`/`attr_writer`/`attr` parse as ordinary `call` nodes whose receiver is implicit `self` on the enclosing class — these need synthesis (see §4a).
- `def self.foo` and `def Foo.bar` parse as `singleton_method` (not `method`); both must be picked up.
- `class << self` opens a `singleton_class` body whose `method` children are class-level singleton methods.
- Constants and class names share node kind `constant` (capitalized identifier); disambiguate by parent context (see §4f).
- `module` nodes nest the same way `class` nodes do and host methods; treat them as a class-like symbol with a distinct `SymbolKind`.
- `rescue`, `ensure`, `begin`, `case`/`when` produce their own block kinds; descend without emitting symbols.
- String interpolation (`"hi #{name}"`) yields `interpolation` nodes whose children are real expressions — descend so calls inside interpolations are captured.

## 3. AST-node to fact mapping

| tree-sitter node kind | Fact emitted | SymbolKind | Notes |
|---|---|---|---|
| `class` | `ParsedSymbol` | `Class` | name from `name` field (`constant`); superclass from `superclass` field becomes a `ReferenceKind::Type` and `base:<Name>` attribute on the class. |
| `module` | `ParsedSymbol` | `Module` | acts as a namespace; child `method`s become Methods owned by the module symbol. |
| `singleton_class` (`class << self`) | no symbol | — | descend; methods inside get `ruby:singleton` attribute. |
| `method` (`def foo`) | `ParsedSymbol` | `Method` if parent is class/module/singleton_class, otherwise `Function` | arity from `parameters` field counting `identifier`, `optional_parameter`, `keyword_parameter`, `splat_parameter`, `hash_splat_parameter`, `block_parameter`. |
| `singleton_method` (`def self.foo`, `def Foo.bar`) | `ParsedSymbol` | `Method` | `ruby:singleton` attribute; receiver text in attribute `ruby:singleton-receiver:<text>`. |
| `call` where method is `attr_accessor`/`attr_reader`/`attr_writer`/`attr` and parent is class/module | synthesized `ParsedSymbol`s | `Method` (`Confidence::Partial`) | one Method per symbol argument; pair (reader, writer) for `attr_accessor`. Attribute `ruby:attr` + `ruby:synthesized`. |
| `call` where method is `include`/`extend`/`prepend` | `ParsedReference` on enclosing class | — | `ReferenceKind::Type`; attribute on host class `mixin:include:<Mod>` / `mixin:extend:<Mod>` / `mixin:prepend:<Mod>` for ancestor lookup. |
| `call` where method is `require`/`require_relative`/`load`/`autoload` and argument is string literal | `ParsedImport` | — | `ImportKind::Named`; `is_static` false; `require_relative` resolved against the file's directory, `require` recorded as-is (gem-style). `autoload(:Name, "path")` records both the constant name (alias) and the path. |
| `call` (any other) | `ParsedCall` + `BodyHit::Call` | — | `target_text` is full method-receiver chain; `receiver` = text of `receiver` field if present (incl. `self`); `name` = `method` field; `arity` = named child count of `arguments`; `kind = ParsedCallKind::Method` if receiver text contains `.` or is non-empty, else `Direct`. |
| `assignment` where lhs is `constant` and parent is class/module/program | `ParsedSymbol` | `Const` | name from lhs `constant`; signature = trimmed source. `Confidence::High` if rhs is literal/constant, `Partial` if rhs is a call (cannot guarantee runtime value). |
| `assignment` where lhs is `instance_variable` (`@x`) and parent chain contains a class/module | `ParsedSymbol` | `Field` | attribute `ruby:ivar`; one Field per distinct ivar per host class. |
| `assignment` where lhs is `class_variable` (`@@x`) | `ParsedSymbol` | `Field` | attribute `ruby:cvar`. |
| `assignment` (any other) | `ParsedReference` on owner | — | left-hand identifier becomes `ReferenceKind::Identifier`. |
| `identifier` (bare) | `ParsedReference` + `BodyHit::Identifier` | — | `ReferenceKind::Identifier`. |
| `constant` (bare reference, not in `class`/`module` head) | `ParsedReference` + `BodyHit::Identifier` | — | `ReferenceKind::Type` when used as a superclass/mixin/type position, `Identifier` otherwise. |
| `scope_resolution` (`Foo::Bar`) | `ParsedReference` + `BodyHit::Path` | — | `ReferenceKind::Path`; full text recorded so the graph can match qualified resolutions. |
| `string`, `integer`, `float`, `symbol`, `regex`, `true`/`false`/`nil` | `BodyHit::Literal` | — | mirrors `is_python_literal`. |
| `heredoc_body` | none | — | skip subtree; suppresses spurious literal/identifier hits. |
| `do_block` / `block` | none (descend) | — | not a symbol; descend to capture calls/references inside. |
| any `ERROR`/missing node | `ParseDiagnostic` (`Partial`) | — | identical to Python diagnostic path. |

## 4. Language gotchas & heuristics

**(a) `attr_*` synthesis.** Inside a `class`/`module` body, a `call` whose method is `attr_reader`, `attr_writer`, `attr_accessor`, or `attr` synthesizes Method symbols, one per symbol argument. `attr_reader :name` -> Method `name`; `attr_writer :name` -> Method `name=`; `attr_accessor :name` -> both. Span = the call node's span; `Confidence::Partial`; attributes `ruby:attr`, `ruby:synthesized`, plus `ruby:attr-reader`/`ruby:attr-writer`. Arity is 0/1 accordingly.

**(b) `define_method`.** Excluded from extraction. `define_method(:name) { ... }` with a literal symbol/string argument *could* be emitted as Partial, but for the first PR we exclude all `define_method` calls and flag this as a known recall gap; the oracle (§9) also excludes them so they don't count as false negatives.

**(c) `method_missing` / `respond_to_missing?`.** Defined methods are extracted normally as Method symbols. Their *runtime* dispatch behaviour is not modelled. Calls that target methods only reachable via `method_missing` will resolve to `CandidateSet` at best. Documented as a known recall gap.

**(d) `eval`, `instance_eval`, `class_eval`, `module_eval`.** Body string arguments are not parsed. The call itself is recorded as a `ParsedCall`. No symbols/references are mined from the string literal. Documented limitation.

**(e) Blocks (`do...end`, `{...}`).** `do_block` / `block` are local scopes, never symbols. Descend into them so calls and references inside blocks are still captured; the owning symbol stays the enclosing method/class/file. Block-local variables (`|x|`) are not emitted as symbols.

**(f) Constants vs classes.** Ruby capitalization rule: an identifier starting with `[A-Z]` is a constant. Disambiguation in extraction:

- `constant` that is the `name` field of a `class` or `module` node -> the host's name (no separate reference).
- `constant` that is the `superclass` field, or a `mixin` argument to `include`/`extend`/`prepend`, or appears in `scope_resolution` left of `::` -> `ReferenceKind::Type`.
- Any other bare `constant` reference -> `ReferenceKind::Identifier` (it may name a class or a constant value; the graph resolves).
- `assignment` lhs that is a `constant` and is at class/module/top scope -> `SymbolKind::Const` (not a class alias unless the rhs is itself a class literal — that case stays `Const` for the first PR; class-alias detection is a follow-up).

**(g) Modules + `include`/`extend`/`prepend`.** Recorded as Type references on the enclosing class plus `mixin:include:<Mod>` / `mixin:extend:<Mod>` / `mixin:prepend:<Mod>` attributes on the host. The Ruby graph resolver mirrors `python_method_in_bases`: ancestor lookup walks `base:` then `mixin:include:` then `mixin:prepend:` in Ruby's MRO order (prepend before self, include after self), capped at depth 8. `extend` mixes into the singleton class; resolver uses `mixin:extend:` only for `Class.method` style calls where the receiver names the class.

**(h) Imports.**

- `require "json"` -> `ParsedImport { path: "json", kind: Named, is_static: false }`, `imported_name = Some("json")`.
- `require_relative "../lib/foo"` -> path normalized against the file's directory (strip `./`, resolve `..`, append `.rb` for matching); `imported_name = Some("foo")`.
- `load "x.rb"` -> same as `require` but flagged `ruby:load` in provenance description.
- `autoload(:Bar, "lib/bar")` -> `ParsedImport { path: "lib/bar", alias: Some("Bar"), imported_name: Some("Bar") }`.
- Path resolution differences: `require` searches `$LOAD_PATH` (we do not model this; gem-style paths stay literal). `require_relative` is the only form whose target we can resolve to a workspace file, so the graph's import-matcher only attempts file resolution on `require_relative` and `autoload`. Documented limitation.

**(i) Heredocs.** Skip the `heredoc_body` subtree entirely. Without this, identifier-shaped tokens inside SQL/HTML heredocs would emit junk references.

## 5. Per-symbol confidence rules

| Situation | Confidence |
|---|---|
| `def` inside `class`/`module`/`singleton_class` with non-empty body | `High` |
| `singleton_method` (`def self.foo`) | `High` |
| Top-level `def` (no class/module parent) -> `Function` | `High` |
| `attr_accessor`/`attr_reader`/`attr_writer` synthesized Methods | `Partial` |
| `define_method('literal')` | **excluded** in first PR (not emitted at all) |
| `define_method(symbol_variable)` | excluded |
| `Const` whose rhs is literal or constant | `High` |
| `Const` whose rhs is a call expression | `Partial` |
| `Field` (`@ivar`/`@@cvar` assignment) — first sighting per (class, name) | `Heuristic` (matches Python field path) |
| `class Foo < dynamic_call()` — superclass not a `constant` | class symbol still `High`; superclass reference omitted; diagnostic `Partial` |
| File contains parse `ERROR` node | per-symbol confidence unchanged, file gets `ParseDiagnostic` with `Partial` (mirrors Python) |

## 6. Fixture sketch

Layout under `benchmarks/fixtures/ruby/semantic-cases/`:

- `app/models/user.rb`
  - `class User < ActiveRecord::Base`
  - `attr_accessor :name, :email`
  - `def full_name; "#{name} #{surname}"; end`
  - `def self.find_by_email(email); ...; end`
- `app/models/admin.rb`
  - `require_relative "user"`
  - `class Admin < User`
  - `include Auditable`
  - `def promote(user); user.full_name; end` (exercises cross-file Method call `Admin#promote -> User#full_name`)
- `app/concerns/auditable.rb`
  - `module Auditable`
  - `def audit!(event); log(event); end`
  - exercises module + include resolution and intra-module call
- `app/services/greeter.rb`
  - `class Greeter`
  - `def greet(user); "hi #{user.full_name}"; end` (cross-file Method call `Greeter#greet -> User#full_name` via inferred class from receiver type annotation in tests, plus call-chain exercising interpolation descent)
- `lib/runner.rb`
  - `require_relative "../app/services/greeter"`
  - `def build_runner; Greeter.new.greet(User.new); end` (top-level Function, cross-file chain)
- `vendor/ignored.rb`
  - `def vendored_runner_shadow; "vendor"; end` — to exercise vendor-dir exclusion (mirrors Python fixture).
- `generated/ignored.rb`
  - `# @generated by benchmark fixture` header + `def generated_runner_shadow; ...; end` — exercises generated-file fallback exclusion.

This gives one module + four classes + cross-file class/method chains across three top-level dirs plus vendor/generated decoys; enough to exercise hierarchy, signature search, references, call chain, fallback quality, and import resolution queries.

## 7. Real-repo corpus

- **Repo**: `https://github.com/sinatra/sinatra`
- **Commit / tag**: `v4.1.1` (recent stable; resolve to its SHA at the time the corpus entry is added)
- **Smoke subset selector**: `lib/sinatra/*.rb` (top-level files only, no `lib/sinatra/show_exceptions/` subtree); ~30 files of plain Ruby. Use a `subdir` field on the corpus repo entry (or check out the full repo and rely on workspace crawl exclusions if `subdir` is not supported yet — confirm by checking the corpus schema before writing the entry).
- **Why Sinatra**: smaller than Rails, no ActiveSupport autoloading magic, idiomatic class+module style, ships its own real `require_relative` chains, has `attr_accessor` / `include` / module nesting, and exercises the routing-DSL `call` patterns that match `attr_*` synthesis cleanly. Avoids the Rails-specific metaprogramming (`belongs_to`, `has_many`, `delegate`, `concerning`) that would dominate FN counts on a Rails corpus.

## 8. Smoke query spec

File: `benchmarks/specs/ruby-smoke-queries.json`. Contents:

```json
{
  "queries": [
    {
      "id": "ruby-hierarchy",
      "kind": "hierarchy_contains",
      "expected_contains": [
        "Class:User",
        "Class:Admin",
        "Class:Greeter",
        "Module:Auditable",
        "Method:full_name",
        "Method:find_by_email",
        "Method:promote",
        "Method:greet",
        "Method:audit!",
        "Function:build_runner"
      ]
    },
    {
      "id": "ruby-class-signature",
      "kind": "signature_search",
      "text": "class User",
      "symbol_kind": "Class",
      "expected_contains": [
        "Class:User"
      ]
    },
    {
      "id": "ruby-singleton-method-signature",
      "kind": "signature_search",
      "text": "def self.find_by_email",
      "symbol_kind": "Method",
      "expected_contains": [
        "Method:find_by_email"
      ]
    },
    {
      "id": "ruby-attr-accessor-synth",
      "kind": "signature_search",
      "text": "attr_accessor :name",
      "symbol_kind": "Method",
      "attribute": "ruby:synthesized",
      "expected_contains": [
        "Method:name",
        "Method:name="
      ]
    },
    {
      "id": "ruby-references",
      "kind": "reference_search",
      "text": "User",
      "expected_contains": [
        "User"
      ]
    },
    {
      "id": "ruby-references-to-symbol",
      "kind": "references_to_symbol",
      "to": "full_name",
      "expected_contains": [
        "user.full_name"
      ]
    },
    {
      "id": "ruby-call-chain-cross-file",
      "kind": "call_chain",
      "from": "promote",
      "to": "full_name",
      "expected_contains": [
        "promote -> full_name"
      ]
    },
    {
      "id": "ruby-call-chain-mixin",
      "kind": "call_chain",
      "from": "audit!",
      "to": "log",
      "expected_contains": [
        "audit! -> log"
      ]
    },
    {
      "id": "ruby-import-resolution",
      "kind": "signature_search",
      "text": "require_relative \"user\"",
      "expected_contains": [
        "require_relative"
      ]
    },
    {
      "id": "ruby-fallback-quality",
      "kind": "fallback_quality",
      "expected_contains": [
        "generated",
        "vendor"
      ]
    }
  ]
}
```

## 9. Oracle plan

- **Tool**: **Prism** (the Ruby parser shipped with the Ruby 3.3+ runtime, available as `require "prism"`). Justification: stable serialization API (`Prism.parse_file(path).value`), zero gem installation needed (ships with the stdlib in 3.3), no monkey-patching of the host process, fast (C-backed), and Shopify-maintained so the JSON shape is documented.
- **Install in CI**: Linux runners — `apt-get install -y ruby` (Ubuntu 24.04 ships 3.3). macOS runners — `brew install ruby@3.3` plus PATH export. Wrap both in `continue-on-error: true` so a Ruby toolchain miss degrades gracefully (see §9 "Scan-only fallback").
- **Scan command**: a tiny Rust helper at `benchmarks/oracle-helpers/ruby-oracle/` invokes `ruby` with an inline `-e` script that walks the fixture, parses each `.rb` file with Prism, and emits `{ rows: [[file, kind, name], ...], unparseable_files: [...] }` JSON on stdout — same shape as `PythonAstOracleOutput` / `GoAstOracleOutput`. The Rust side adds a new `collect_ruby_prism_symbol_scan` in `oracles/common_scan.rs` and `oracles/ruby_prism.rs` modelled on `oracles/cpython_ast.rs`. Per-file invocation is too slow on real corpora; the oracle should be one Ruby process that walks the tree and emits one combined JSON. Sketch:

  ```ruby
  require "prism"
  require "json"
  require "find"
  root = ARGV[0]
  rows = []; unparseable = []
  Find.find(root) do |p|
    Find.prune if File.directory?(p) && (File.basename(p).start_with?(".") || %w[vendor node_modules tmp].include?(File.basename(p)))
    next unless p.end_with?(".rb")
    rel = p.sub(/^#{Regexp.escape(root)}\/?/, "")
    res = Prism.parse_file(p)
    if res.failure?
      unparseable << rel
      next
    end
    walk = ->(node, in_class) {
      case node
      when Prism::ClassNode then rows << [rel, "Class", node.constant_path.slice]; node.compact_child_nodes.each { |c| walk.call(c, true) }
      when Prism::ModuleNode then rows << [rel, "Module", node.constant_path.slice]; node.compact_child_nodes.each { |c| walk.call(c, true) }
      when Prism::DefNode then rows << [rel, in_class ? "Method" : "Function", node.name.to_s]; node.compact_child_nodes.each { |c| walk.call(c, in_class) }
      else node.compact_child_nodes.each { |c| walk.call(c, in_class) }
      end
    }
    walk.call(res.value, false)
  end
  puts JSON.generate({rows: rows, unparseable_files: unparseable})
  ```

- **Exclusion list** (the oracle does **not** emit these; the squeezy side suppresses them symmetrically via `default_oracle_exclusions`):
  - block-local and method-local variables
  - block parameters
  - `define_method` symbols (declaration-time only; resolver excluded too)
  - bodies of `eval` / `instance_eval` / `class_eval` / `module_eval`
  - synthesized `attr_*` methods (squeezy emits them as Partial; the oracle does not, so they must be filtered out of squeezy's scan before comparison via `ruby:synthesized` attribute filter in `collect_squeezy_symbol_scan_excluding_files`'s Ruby counterpart, mirroring how the C-family oracle filters `c++:template-specialization`).
- **Definition/reference probes**: declarations only. Prism does not resolve dispatch. Set `ra_lsp_probes: 0` in the corpus entry. The Ruby oracle reuses the `GoOracleReport` style (symbols-only) wired through `go_oracle_to_accuracy`'s pattern; navigation accuracy is empty with status `"Ruby LSP navigation oracle not used"`.
- **Scan-only fallback**: when `Command::new("ruby").output()` fails (missing binary), `collect_ruby_prism_symbol_scan` falls back to a `common_scan`-only mode where the oracle scan is built by re-parsing the same workspace files with tree-sitter-ruby (same code path the squeezy backend uses) — i.e. a degenerate self-compare. The report records `status: "Ruby Prism oracle unavailable; degraded to scan-only"` and the `mode: "scan-only"` field on the oracle report so gates can branch.

## 10. Gate thresholds (first PR)

`precision >= 0.90`, `recall >= 0.75`.

Justification: dynamic dispatch caps recall on real corpora because `method_missing`, `define_method`, and `eval`-built methods are systematic false negatives that the oracle also excludes — but cross-file dispatch through `include`/`extend` (where ancestor chains touch open classes from other gems) and `Class.new(BaseClass) { ... }` anonymous-class assignments remain uncovered for the first PR. Precision is held to the same bar as Java/CSharp because Ruby's parse-time symbols are unambiguous; the dynamic gap shows up in recall, not in spurious FPs. Wire into `gates.rs` alongside the Go block:

```rust
if !no_speed_gate
    && let Some(ruby) = &report.ruby_oracle
    && (ruby.symbols.precision < 0.90 || ruby.symbols.recall < 0.75)
{
    return Err(SqueezyError::Graph(format!(
        "Ruby oracle accuracy regressed: precision={:.3} recall={:.3}",
        ruby.symbols.precision, ruby.symbols.recall
    )));
}
```

Use `<` against thresholds rather than `!= 0` on FP/FN counts because the dynamic-dispatch gap will produce a steady stream of FNs that we accept rather than chase.

## 11. Speed parity target

Within **1.5x** of Python's per-file `parse + extract` time on the hand-built fixture (Python is the closest analogue). Concretely: on a clean run of `benchmarks/fixtures/ruby/semantic-cases/`, the per-file parse+extract wall time reported by the bench must be `<= 1.5 * python_per_file_ms` on the same machine. Tracked in the smoke report; no hard CI gate in the first PR (so block sweeps and ancestor walks can land without thrashing on noisy CI numbers), promoted to a hard gate once the resolver stabilises.

## 12. CI matrix entry

Append to the `language` choice list (lines ~33-41) and the gating logic on line 64 of `.github/workflows/benchmark-lang.yml`:

```yaml
      language:
        description: Benchmark language family.
        required: true
        default: "rust"
        type: choice
        options:
          - rust
          - python
          - java
          - go
          - c-family
          - csharp
          - js-ts
          - ruby
```

Update the runner timeout expression:

```yaml
    timeout-minutes: ${{ (inputs.language == 'c-family' || inputs.language == 'csharp') && 90 || inputs.language == 'go' && 60 || inputs.language == 'ruby' && 45 || 120 }}
```

Add a Ruby toolchain step before the benchmark run, immediately after `Setup benchmark`:

```yaml
      - name: Setup Ruby 3.3 (Prism oracle)
        if: inputs.language == 'ruby'
        continue-on-error: true
        uses: ruby/setup-ruby@v1
        with:
          ruby-version: '3.3'

      - name: Verify Prism availability
        if: inputs.language == 'ruby'
        continue-on-error: true
        run: ruby -rprism -e 'puts Prism::VERSION'
```

`continue-on-error: true` on both ensures the oracle degrades to scan-only mode (per §9) instead of failing the workflow if the Ruby toolchain is unavailable. The corpus entry to add to `benchmarks/corpus.json`:

```json
{
  "name": "ruby-smoke",
  "family": "ruby",
  "language": "ruby",
  "tier": "smoke",
  "fixture": "benchmarks/fixtures/ruby/semantic-cases",
  "spec": "benchmarks/specs/ruby-smoke-queries.json",
  "report": "ruby/ruby-smoke.json",
  "ra_lsp_probes": 0
}
```

A `ruby-full` entry pointing at the Sinatra checkout (`repo.url` / `repo.rev = v4.1.1`, `checkout: target/benchmark-repos/sinatra-smoke`) follows in the second PR once smoke is green.
