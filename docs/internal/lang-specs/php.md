# PHP language-implementation spec

Target follow-up commit on branch `langs/php`. Scaffold already landed:
`LanguageKind::Php`, `LanguageFamily::Php`, `.php` extension mapping, placeholder
`extract_php` returning `ParsedFile::unsupported`, `PhpBackend` and
`PhpGraphExt` registered in the backend/ext macros, and
`tree-sitter-php = "0.24"` as a workspace dep. This spec defines the
extraction, fixtures, oracle, and CI work for the real implementation.

## 1. Template choice

Template primarily off `crates/squeezy-parse/src/languages/csharp.rs`. PHP's
`namespace Foo\Bar;` (both braced and file-scoped variants) lines up almost
exactly with C#'s `namespace_declaration` / `file_scoped_namespace_declaration`
shape, and PHP's `use Foo\Bar [as B];` import grammar is structurally
identical to a C# `using_directive`. Class/interface/trait/enum declarations,
method/property fact extraction, attribute (`#[Attribute]`) handling, and the
namespace-as-package surface all port over directly. Lift the mixed-language
top-level handling from `js_ts.rs` — the `program` root in `LANGUAGE_PHP`
yields interleaved `text` (raw HTML) and `php_tag`-bracketed `php_statement`
nodes that must be skipped without polluting `references`, the same shape
JS/TS uses for JSX text. The receiver/call-target parsing in
`extract_csharp_call` (`member_access_expression` -> name + receiver) maps
onto PHP's `member_call_expression` / `scoped_call_expression`.

## 2. Grammar

- Crate: `tree-sitter-php = "0.24"` (already in `Cargo.toml`).
- Language to load: `tree_sitter_php::LANGUAGE_PHP` (do **not** use
  `LANGUAGE_PHP_ONLY`). Real-world PHP files routinely interleave inline HTML,
  short echo tags `<?= $x ?>`, and full `<?php ... ?>` blocks; selecting
  `LANGUAGE_PHP` keeps a single grammar handling the whole codebase. Pure-PHP
  files parse fine under this grammar (the leading `<?php` is just another
  `php_tag` node) so we don't need both registered.
- Wire-up: add `tree_sitter_php` as a parse-crate dep, mirror
  `csharp_language()` with a `php_language()` helper, extend
  `LanguageParser` with a `php_parser` field, and add the
  `LanguageKind::Php` arm to `parser_for_language`, `parser_for_language_kind`,
  and `language_for_kind` (currently routed to the `None` arm at
  `crates/squeezy-parse/src/lib.rs:811-818`).
- Quirks to handle in extraction:
  - Open/close tags (`<?php`, `<?=`, `?>`) appear as `php_tag` /
    `text_interpolation` nodes — skip non-PHP children at program root.
  - Heredoc (`<<<EOT ... EOT;`) and nowdoc (`<<<'EOT' ... EOT;`) bodies sit
    inside `heredoc` / `nowdoc` nodes — exclude their inner text entirely;
    only emit a `BodyHit::Literal` for the outer node span.
  - Dynamic class instantiation: `new $className(...)` parses as
    `object_creation_expression` with a `variable_name` child; emit a
    `ParsedCall` with `Confidence::Partial`.
  - Variable variables (`$$x`) and `eval(...)` calls — exclude (see
    section 4).
  - Trait conflict resolution blocks (`use TraitA, TraitB { TraitA::foo
    insteadof TraitB; }`) — the inner `use_list` carries
    `use_instead_of_clause` / `use_as_clause` nodes; record the trait
    references but ignore the per-method aliasing as facts (record as
    `attributes` on the consuming class).
  - First-class callable syntax `strlen(...)` and `$obj->m(...)` — parse as
    `function_call_expression` / `member_call_expression` whose `arguments`
    field contains a single `(...)` literal; treat as a `ParsedCall` with
    arity 0 and add `php:first-class-callable` attribute.
  - Intersection and union types (`Foo&Bar`, `Foo|Bar|null`) — strip to
    individual type references; null/scalar predefined types follow the
    `csharp_is_keyword_or_predefined` pattern.

## 3. AST-node -> fact mapping

| tree-sitter node kind | Squeezy fact | Notes |
| --- | --- | --- |
| `namespace_definition` (braced) / `namespace_use_declaration` w/ leading `namespace` keyword (file-scoped form) | `ParsedSymbol { kind: Module }` plus push namespace segments onto scope; file `package` = top namespace | C# parallel: `namespace_declaration` / `file_scoped_namespace_declaration`. The braced form has a `body` child; the file-scoped form (`namespace Foo;`) covers the rest of the file — set `package` once on first hit. |
| `namespace_use_declaration` (no leading namespace decl) | `ParsedImport` | Subclause `namespace_use_clause` carries `name`/`alias`. See section 4(b) for kinds. |
| `class_declaration` | `ParsedSymbol { kind: Class }` | `name` field holds `name` token; `base_clause` (extends) and `class_interface_clause` (implements) populate `attributes: ["base:Foo", ...]` and emit `ReferenceKind::Type` references. `class_modifier` carries `abstract`/`final`/`readonly` — surface as `php:modifier:*`. |
| `interface_declaration` | `ParsedSymbol { kind: Interface }` | Same as C# interface. `base_clause` -> `extends` references. |
| `trait_declaration` | `ParsedSymbol { kind: Trait }` | Use existing `SymbolKind::Trait` (no PHP-specific kind needed). |
| `enum_declaration` | `ParsedSymbol { kind: Enum }` | Backed enums (`: int`/`: string`) — record backing type as `php:backed:<type>` attribute. |
| `enum_case` | `ParsedSymbol { kind: Variant }` | C# `enum_member_declaration` parallel. |
| `function_definition` | `ParsedSymbol { kind: Function }` | Top-level or inside namespace. |
| `method_declaration` | `ParsedSymbol { kind: Method }` | Inside class/trait/interface/enum. `visibility_modifier` -> `visibility`; `static`/`abstract`/`final`/`readonly` -> `php:modifier:*`. Constructor / destructor are method nodes whose `name` is `__construct` / `__destruct` — keep as Method, add `php:ctor` / `php:dtor` attribute. |
| `property_declaration` | `ParsedSymbol { kind: Field }` per property element | C# `field_declaration` parallel; `property_element` children carry per-name decls. `readonly` modifier + typed properties (`type_node`) -> `type:<leaf>` attribute. |
| `const_declaration` (inside class) / `const_declaration` (top-level) | `ParsedSymbol { kind: Field }` (class const) or skip top-level for first PR | First PR: top-level `const FOO = 1;` not emitted as a symbol (rare and noisy in templates). Add follow-up ticket. |
| `attribute_list` -> `attribute` (PHP 8 `#[Foo(...)]`) | populates `attributes_raw`, then `php:attr:<head>` plus framework heuristics (Symfony `#[Route]`, `#[AsCommand]`, `#[AsController]`) | Mirrors `csharp_semantic_attributes`. |
| `use_declaration` inside a class/trait body (trait inclusion) | `ParsedReference { kind: ReferenceKind::Type }` per trait name, plus class `attributes: ["uses_trait:Foo"]` | See section 4(c). |
| `function_call_expression` | `ParsedCall { kind: Direct }` | `function` field = callee. `name` from `qualified_name` or `name` token. |
| `member_call_expression` | `ParsedCall { kind: Method }` | `object` field = receiver, `name` field = method. Receiver text is what C# stores in `receiver`. |
| `scoped_call_expression` (`Foo::bar()`) | `ParsedCall { kind: Method }` w/ receiver = class name | Static call. Add `php:static` attribute on the call. |
| `object_creation_expression` (`new Foo(...)`, `new $cls(...)`) | `ParsedCall { kind: Direct }` + `ParsedReference { kind: Type }` | Mirror `extract_csharp_object_creation`. Dynamic form lowers confidence (section 5). |
| `qualified_name`, `named_type` | `ParsedReference { kind: Type | Path }` when not a declaration name | Match `csharp_qualified_name` handling. |
| `variable_name` (`$foo`) | excluded as reference, but contributes `BodyHit::Identifier` | Don't pollute reference index with locals. |
| `member_access_expression` (read, not call) | `ParsedReference { kind: Field }` | Mirror JS/TS field reference. |
| `encapsed_string`, `integer`, `float`, `boolean`, `null`, `string` literal | `BodyHit { kind: Literal }` | Mirrors `is_csharp_literal`. |
| `heredoc`, `nowdoc` outer node | `BodyHit { kind: Literal }` on outer span only; **do not recurse into body** | Heredoc may interpolate `$vars` — we still skip identifier extraction inside per section 4(g). |
| `text` / `text_interpolation` at program scope | `BodyHit { kind: Literal }` (mixed HTML) | Don't extract identifiers. |

Provenance string: `"tree-sitter-php"` (mirror `"tree-sitter-c-sharp"`).
`language_identity` follows C#'s `T:Foo.Bar.Baz` / `M:Foo.Bar.Baz.method`
shape with `\` namespace separators rewritten as `.` for stable cross-engine
keys.

## 4. Language gotchas & heuristics

(a) **Namespace declaration scoping.** Both `namespace Foo\Bar { ... }` and
`namespace Foo\Bar;` (file-scoped) push segments onto a `PhpScope` (port
`CsharpScope`). File-scoped declarations apply for the remainder of the file
and are not popped at any close brace. Track `top_namespace` for the file's
`package` field; record `php:namespace:<dotted>` on every symbol emitted
within (use `\` -> `.` for stable identity).

(b) **`use` import kinds.** The `namespace_use_declaration` grammar admits
five shapes — map each to a distinct `ImportKind`:

| Source | `ImportKind` | `imported_name` | `path` |
| --- | --- | --- | --- |
| `use Foo\Bar;` | `Named` | `Bar` | `Foo.Bar` |
| `use Foo\Bar as B;` | `Named` w/ alias | `Bar` | `Foo.Bar` |
| `use Foo\{Bar, Baz as Q};` (group use) | one `Named` per clause | per-clause leaf | `Foo.Bar`, `Foo.Baz` |
| `use function Foo\bar;` | `Named` + `attributes: ["php:use-function"]` (no separate kind needed in `ImportKind` enum) | `bar` | `Foo.bar` |
| `use const Foo\BAR;` | `Named` + `attributes: ["php:use-const"]` | `BAR` | `Foo.BAR` |

If we discover later that downstream resolution needs first-class
distinction, add `php:use-function` / `php:use-const` to `attributes` on the
`ParsedImport` rather than expanding `ImportKind` (no enum churn).

(c) **Trait inclusion (`use TraitA, TraitB;` inside a class/trait body).**
Emit one `ParsedReference { kind: Type }` per trait name and stamp the
enclosing class with `attributes: ["uses_trait:TraitA", ...]`. The optional
`use_list` block with `insteadof` / `as` resolution clauses is recorded as
`php:trait-resolution` attribute on the class but the inner aliasing
detail is not modelled in v1.

(d) **Interfaces.** `interface_declaration` -> `SymbolKind::Interface` (already
the convention since the Go/C# PR). `class_interface_clause` produces
`attributes: ["base:Foo", ...]` on the class plus `ReferenceKind::Type`
references.

(e) **`extends` / `implements`.** Same `base_list` pattern as C#. Single
parent class via `base_clause`; multiple interfaces via
`class_interface_clause`. Both lower to references with `base:<leaf>`
attributes — let downstream graph code synthesize Extends/Implements
edges.

(f) **Magic methods.** Detect by name (`__construct`, `__destruct`, `__call`,
`__callStatic`, `__get`, `__set`, `__isset`, `__unset`, `__invoke`,
`__toString`, `__clone`, `__sleep`, `__wakeup`, `__serialize`,
`__unserialize`, `__set_state`, `__debugInfo`). Still emit them as
`SymbolKind::Method`, but **on call sites whose callee name is `__call`,
`__callStatic`, `__get`, `__set`, `__invoke`** (i.e. cases that imply
implicit dispatch), lower the `Confidence` to `Partial`. The
*declaration* keeps `Confidence::ExactSyntax` but gains
`attributes: ["php:magic"]`.

(g) **Heredoc / nowdoc bodies.** `heredoc` and `nowdoc` are single nodes with
an inner string body that the grammar may further parse for interpolations.
At the visit step, when `node.kind() == "heredoc" || node.kind() == "nowdoc"`,
emit one outer `BodyHit::Literal` and **return without recursing into
children**. This prevents `$variables` inside heredoc bodies from being
extracted as references or fake identifiers.

(h) **Inline HTML.** At program root, the grammar yields `text` /
`text_interpolation` siblings interleaved with `php_tag`-led
`php_statement`s. Emit `BodyHit::Literal` for each `text` span (so plain-text
search still hits inline content) and skip identifier/reference extraction.

(i) **Dynamic class instantiation (`new $varname(...)`).** When the
`object_creation_expression`'s `type` field is a `variable_name`, still emit
the `ParsedCall`, but:

- `target_text` = the literal variable text (e.g. `$cls`).
- `name` = `"<dynamic>"`.
- `confidence` = `Confidence::Partial`.
- `attributes` on call: not modeled; flag via `provenance.reason =
  "object_creation_expression dynamic"`.

(j) **Variable variables (`$$x`).** `variable_name` whose `name` field is
itself a `variable_name`. Detect and skip entirely (no reference, no body
hit).

(k) **`eval(...)`.** Detect at `function_call_expression` callee name
`"eval"`. Emit the `ParsedCall` (so the existence is visible in
fallback search), but suppress identifier/literal extraction from the
argument: do not descend into the `arguments` subtree. Tag with
`provenance.reason = "function_call_expression eval"`.

(l) **`class_exists` / `function_exists` / `interface_exists` /
`trait_exists` guards.** Pattern: `if (class_exists('Foo')) { ... }`. The
guarded block's contents are still real PHP — emit symbols / references
normally — but the *string argument* to the guard is **not** a definition
or import. Concretely, do not emit a `ParsedImport` or `ParsedSymbol` for
the string literal `'Foo'`. The call itself stays as a normal
`ParsedCall` with `Confidence::Heuristic`.

## 5. Per-symbol confidence rules

Default `Confidence::ExactSyntax` for any declaration whose namespace path,
name token, and enclosing kind are all derivable from the AST without
guesswork. Drop to `Confidence::Partial` in these cases:

- Magic-method declarations carrying `php:magic` (rule 4(f)) — declaration
  stays Exact, **call sites resolving against `__call` / `__callStatic` /
  `__get` / `__set` / `__invoke`** are Partial.
- Dynamic class instantiation (`new $cls(...)`) — the synthesized
  `ParsedCall` is Partial.
- `object_creation_expression` whose `type` is a `name` not visible in
  current imports and not declared in the same file — stays Heuristic
  (default for calls); downstream graph already handles cross-file
  resolution.
- Anything inside a `function_exists`/`class_exists` guarded branch where
  the guard argument cannot be resolved to a known symbol — leave at
  Heuristic but stamp `provenance.reason` with `"guarded"`.
- Calls extracted via first-class callable syntax `foo(...)` — emit at
  Heuristic with `php:first-class-callable` attribute on the call's
  `attributes` (still no enum churn on `ParsedCall`).

## 6. Fixture sketch

Lay out under `benchmarks/fixtures/php/semantic-cases/` to mirror the
C# semantic-cases shape (`Squeezy.CSharp.SemanticCases.csproj`,
`Runner.cs`, `vendor/Ignored.cs`, ...).

```
benchmarks/fixtures/php/semantic-cases/
  composer.json                          # synthetic, declares PSR-4 root
  src/
    Foo/
      Bar/
        Service.php                      # namespace Foo\Bar; class Service implements IRunner
        Repository.php                   # uses trait Loggable; calls Service::run
      Traits/
        Loggable.php                     # trait Loggable { protected function log(...) }
      IRunner.php                        # interface IRunner { public function run(...) }
      Status.php                         # backed enum Status: string { case Ok = 'ok'; ... }
      Magic.php                          # class Magic { public function __call(...) }
    Mixed/
      template.php                       # <?php $x = 1; ?> <h1><?= $x ?></h1> <?php class T {} ?>
  vendor/
    psr/log/Psr/Log/LoggerInterface.php  # synthetic Composer dep — must be excluded
  generated/
    container_xxx.php                    # mimicked Symfony cache file — must be excluded
```

Coverage targets per file:

- `IRunner.php`: namespace + interface declaration with one method shape.
- `Loggable.php`: trait with a protected method, demonstrating
  `Confidence::ExactSyntax` on a trait method.
- `Service.php`: namespace `Foo\Bar`, `use Foo\Traits\Loggable;`,
  `class Service implements IRunner { use Loggable; public function run(...)
  { $this->log(...); } }` — exercises imports, trait inclusion, interface
  base, and a `member_call_expression`.
- `Repository.php`: cross-namespace call chain — `use Foo\Bar\Service;
  $s = new Service(); $s->run($id);` — anchors the `php-call-chain`
  expected_contains.
- `Status.php`: backed enum + a `php:backed:string` attribute test.
- `Magic.php`: declares `__call`, then exercises a call that should
  Partial-resolve (`$m->undefined(...)`).
- `Mixed/template.php`: inline HTML + short echo + class declaration,
  validates section 4(g)/(h).
- `vendor/...` and `generated/...`: drive the `fallback_quality` query
  (must appear in fallback summary, must NOT appear in oracle FP).

`composer.json` is a one-screen fixture file with `psr-4` autoload claiming
`Foo\\` -> `src/Foo/`; nothing actually runs it but it documents intent and
matches the C# fixture's `*.csproj` shape so we can later add a
`composer_project_facts` query family analogous to
`dotnet_project_facts`.

## 7. Real-repo corpus

Recommend `symfony/console`.

- URL: `https://github.com/symfony/console`
- Suggested tag: `v7.2.0` (latest stable Symfony Console line as of write
  time; pin the exact commit SHA when adding to `corpus.json` so the
  benchmark is reproducible — same convention as `ripgrep`,
  `newtonsoft_json`, etc.).
- File count: ~150 `.php` files at that tag.
- No DB, no PHP extensions beyond core, no codegen at runtime.
- Smoke subset selector: `src/Symfony/Component/Console/Command/*.php`
  (~30 files including `Command.php`, `HelpCommand.php`,
  `ListCommand.php`, `DumpCompletionCommand.php`,
  `LazyCommand.php`, `SignalableCommandInterface.php`,
  `TraceableCommand.php`). Mirrors the `redux-smoke` subset pattern
  (`target/benchmark-repos/redux-smoke/src`).
- Why this repo: idiomatic modern PHP 8.x, heavy on typed properties,
  attributes (`#[AsCommand]`), enums, traits, interfaces, and namespaces;
  no framework boot required to *parse* any single file; tests live
  separately from `src/` so the corpus stays clean.

Two corpus entries — `php-smoke` (fixture only) and a single
`symfony-console` full-tier entry — match the C# pattern (smoke +
five full-tier libraries). Add additional libraries in a follow-up
once the smoke gate is green; viable candidates: `phpunit/phpunit`,
`guzzlehttp/guzzle`, `symfony/finder`, `nikic/PHP-Parser` itself,
`laravel/framework` core component subset.

## 8. Smoke query spec

Write to `benchmarks/specs/php-smoke-queries.json`:

```json
{
  "queries": [
    {
      "id": "php-declarations",
      "kind": "signature_search",
      "text": "",
      "expected_contains": [
        "Interface:IRunner",
        "Trait:Loggable",
        "Class:Service",
        "Class:Repository",
        "Class:Magic",
        "Enum:Status",
        "Method:run",
        "Method:log",
        "Method:__call",
        "Variant:Ok",
        "Field:prefix"
      ]
    },
    {
      "id": "php-namespace-hierarchy",
      "kind": "signature_search",
      "text": "",
      "attribute": "php:namespace:Foo.Bar",
      "expected_contains": [
        "Class:Service",
        "Class:Repository"
      ]
    },
    {
      "id": "php-attribute-search",
      "kind": "signature_search",
      "text": "",
      "attribute": "php:backed:string",
      "expected_contains": [
        "Enum:Status"
      ]
    },
    {
      "id": "php-references",
      "kind": "references_to_symbol",
      "to": "IRunner",
      "expected_contains": ["IRunner"]
    },
    {
      "id": "php-call-chain",
      "kind": "call_chain",
      "from": "Repository::fetch",
      "to": "Service::run",
      "expected_contains": ["Repository::fetch -> Service::run"]
    },
    {
      "id": "php-trait-inclusion",
      "kind": "edges",
      "expected_contains": [
        "Implements:Service->IRunner:IRunner:Heuristic",
        "UsesTrait:Service->Loggable:Loggable:Heuristic"
      ]
    },
    {
      "id": "php-magic-method-partial",
      "kind": "signature_search",
      "text": "",
      "attribute": "php:magic",
      "expected_contains": ["Method:__call"]
    },
    {
      "id": "php-body-search",
      "kind": "body_search",
      "text": "log",
      "expected_contains": ["Method:log"]
    },
    {
      "id": "php-fallback-quality",
      "kind": "fallback_quality",
      "expected_contains": ["vendor", "generated"]
    }
  ]
}
```

The `UsesTrait` edge kind lives in `crates/squeezy-core/src/lib.rs` and is
synthesized by `add_php_type_edges` from the `uses_trait:<leaf>` attributes
the extractor stamps on the enclosing class.

## 9. Oracle plan

**Tool:** `nikic/PHP-Parser` (current major: 5.x) invoked from a PHP 8.3
subprocess. This is the canonical PHP AST library — used by Rector, Psalm,
PHPStan, and every static-analysis tool of consequence. Rejected
alternatives:

- The PHP `tokenizer` extension is lexical only; it cannot tell a
  `function` keyword inside a string from a real declaration. Not viable
  for an oracle.
- Re-using a Rust-side PHP grammar (re-running tree-sitter, hand-rolled
  scanner) would be tautological — the oracle has to come from a
  different parser implementation than the one squeezy uses.
- `php-language-server` / `phpactor` / Psalm: too heavy, require project
  config, slow to start. nikic/PHP-Parser is library-only and runs
  in-process.

**Helper layout** under `benchmarks/oracle-helpers/php-oracle/`:

```
benchmarks/oracle-helpers/php-oracle/
  composer.json          # require nikic/php-parser:^5.3
  composer.lock
  oracle.php             # CLI entry; argv[1] = workspace root
  src/Collector.php      # NodeVisitor that walks declarations
```

`oracle.php` walks `argv[1]` recursively, applies the same exclusion
rules as `default_oracle_exclusions` (mirror by emitting an
`unparseable_files` array and a `rows` array with `[relative_path, kind,
name]` tuples), and prints JSON on stdout. Match the C# oracle's output
shape (`CsharpOracleOutput { rows, edges, unparseable_files }`) so we
can reuse `compare_symbol_sets` directly. Cache the
`nikic\PhpParser\Parser` instance once per process — instantiating it
costs ~10ms.

Symbol kinds emitted: `Namespace`, `Class`, `Interface`, `Trait`,
`Enum`, `Function`, `Method`, `Property`, `Constant`, `EnumCase`. Edge
kinds emitted: `Extends`, `Implements`, `UsesTrait`. Names are
normalized to leaf names (Roslyn-style) so they line up with
`normalize_symbol_name`.

**Install in CI:**

```bash
# Linux runner only
sudo apt-get update
sudo apt-get install -y php8.3-cli php8.3-mbstring php8.3-xml composer
( cd benchmarks/oracle-helpers/php-oracle && composer install --no-interaction --prefer-dist )
```

PHP 8.3 is the highest version Ubuntu 24.04 LTS ships in main; if the
runner image is older fall back to the `ondrej/php` PPA. Keep the
install step gated by `if: inputs.language == 'php'`.

**Scan command** (Rust side, in `oracles/php.rs`):

```text
php benchmarks/oracle-helpers/php-oracle/oracle.php <root>
```

Run via `Command::new("php")`, parse JSON, populate `SymbolScan` and
`edges` exactly like
`benchmarks/squeezy-graph-bench/src/oracles/roslyn.rs` does for
`dotnet`. The oracle process holds parsed ASTs in a `cache` array keyed
by absolute path so re-scans in the same run (mixed workload probe)
don't re-parse.

**Exclusion list** (filter inside the PHP oracle before emitting rows):

- Locals (`$variables`) — already excluded by the nikic walk (we visit
  declarations only).
- Inline HTML (`Stmt\InlineHTML`) — skip.
- Heredoc/nowdoc bodies — nikic represents these as
  `Scalar\String_`/`Scalar\Encapsed`; we ignore string nodes entirely.
- `eval()` argument bodies — skip the argument AST (no need to descend).
- Magic-method call sites — `MethodCall` / `StaticCall` nodes are not
  counted as definitions by the oracle anyway; the oracle reports
  declarations only.
- Files the parser throws on — collect path into `unparseable_files`,
  do not emit any rows for that file. Squeezy excludes those files from
  FP accounting via the same `OracleUnparseableFile` exclusion bucket
  used by C#/Java.

**Definition/reference probes:** declarations only, mirroring the
Java/Go/C# oracles. `"ra_lsp_probes": 0` in the corpus entry. Add a
follow-up ticket to wire `phpactor`'s LSP into a navigation oracle if
PHP accuracy becomes a focus area.

**Scan-only fallback:** if `php` is not on `PATH` or `composer install`
hasn't been run, degrade to `common_scan` (port
`collect_csharp_squeezy_symbol_scan_excluding_files` into a
`collect_php_squeezy_symbol_scan_excluding_files`). The oracle status
string becomes `"skipped: php not found"` and the gate only enforces
query truth, matching Java's behavior at
`benchmarks/squeezy-graph-bench/src/oracles/javac.rs:34`.

## 10. Gate thresholds for first PR

- `precision >= 0.92` — static namespace+class declarations are clean to
  extract; FP risk concentrates in (a) heredoc string fragments
  accidentally tokenized, (b) Symfony-style attribute `#[Foo]` bodies
  classified as references, and (c) closure/arrow-function names. All
  three are tractable in tree-sitter visitor code.
- `recall >= 0.80` — losses come from: magic-method dynamic dispatch
  (`__call` on a class hides every `$obj->whatever()` from declaration
  matching), dynamic class names (`new $cls`), traits whose
  `insteadof` rewrites change the method's owning class, and inline-HTML
  files where the PHP fraction is small. 0.80 is the same floor we
  applied to JS/TS (mixed JSX, dynamic prop access) and matches what
  Roslyn-vs-tree-sitter parity hit on initial C#.

`gates.rs` (currently `benchmarks/squeezy-graph-bench/src/gates.rs:1-58`)
enforces the missing-query gate unconditionally; precision/recall floors
live in the per-language oracle report comparison code. Wire the PHP
floors into the `php_oracle_report` block following the C# convention
and the existing `no_speed_gate` opt-out on full-tier entries.

## 11. Speed parity target

Within 1.5x of JS/TS per-file parse+extract time on the hand-built
fixture (~10ms per file on M1 hardware for the JS/TS smoke fixture).
tree-sitter-php with `LANGUAGE_PHP` parses meaningfully faster than
`tree-sitter-typescript` (smaller grammar, no JSX), so 1.5x is a
generous ceiling; expect ~0.8x in practice.

Measure via the same mixed-workload probe used elsewhere
(`mixed_iterations` in `corpus.json`). For `php-smoke`, start at
`"mixed_iterations": 1000` (matching JS/TS smoke) and bump after the
first run if the throughput leaves slack. Speed gate is per-iteration
faster than oracle validation; the existing
`report.faster_than_validation` check in `gates.rs:23` handles it
without per-language plumbing.

## 12. CI matrix entry

Patch `.github/workflows/benchmark-lang.yml` in three places:

1. Add `php` to both `workflow_call` and `workflow_dispatch` choice
   lists.
2. Add a runner image and timeout: PHP can live on
   `ubuntu-latest` and use the `60`-minute bucket alongside Go.
3. Add a pre-step that installs PHP + Composer + the oracle deps,
   gated by `inputs.language == 'php'`.

YAML snippet (showing the additions):

```yaml
      php:
        # ...existing 'choice' options block...
        # add 'php' as an allowed value

  jobs:
    benchmark:
      runs-on: >-
        ${{ inputs.language == 'rust' && 'macos-latest'
            || 'ubuntu-latest' }}
      timeout-minutes: >-
        ${{ (inputs.language == 'c-family' || inputs.language == 'csharp') && 90
            || (inputs.language == 'go' || inputs.language == 'php') && 60
            || 120 }}

      steps:
        # ...existing checkout + setup-bench...

        - name: Install PHP oracle toolchain
          if: inputs.language == 'php'
          continue-on-error: true
          run: |
            sudo apt-get update
            sudo apt-get install -y php8.3-cli php8.3-mbstring php8.3-xml composer
            ( cd benchmarks/oracle-helpers/php-oracle \
              && composer install --no-interaction --prefer-dist )

        # ...existing 'Run semantic graph benchmark corpus' step...
```

The `continue-on-error: true` lets the workflow proceed even if
`apt-get` or Composer transiently fails; the benchmark falls back to
`common_scan` per section 9. Also extend
`.github/actions/setup-bench/action.yml` if the workflow uses it for
per-language toolchain setup (the C# job pre-builds the Roslyn oracle
in a dedicated step at `benchmark-lang.yml:76-80` — mirror that with a
`Pre-install PHP oracle` step if Composer install is slow enough to
warrant caching).

Corpus entry to add to `benchmarks/corpus.json`:

```json
{
  "name": "php-smoke",
  "family": "php",
  "language": "php",
  "tier": "smoke",
  "fixture": "benchmarks/fixtures/php/semantic-cases",
  "spec": "benchmarks/specs/php-smoke-queries.json",
  "report": "php/php-smoke.json",
  "ra_lsp_probes": 0
},
{
  "name": "symfony-console",
  "family": "php",
  "language": "php",
  "tier": "full",
  "fixture": "target/benchmark-repos/symfony-console/src/Symfony/Component/Console",
  "spec": "benchmarks/specs/empty-queries.json",
  "report": "php/symfony-console.json",
  "mixed_repo": "target/benchmark-repos/symfony-console/src/Symfony/Component/Console",
  "mixed_iterations": 1500,
  "ra_lsp_probes": 0,
  "no_speed_gate": true,
  "repo": {
    "url": "https://github.com/symfony/console",
    "rev": "<pin v7.2.0 commit SHA when landing>",
    "checkout": "target/benchmark-repos/symfony-console"
  }
}
```

## Implementation order (operational note)

1. Land tree-sitter-php wiring in `lib.rs` (parser field, `parser_for_language`,
   `language_for_kind`) — empty extractor still returns the placeholder.
2. Port `csharp.rs` shape into `php.rs`: scope, namespace handling, class/
   interface/trait/enum/method/property symbols, `use` imports, calls,
   references, body-hit deduping. Keep diff < 800 lines.
3. Add fixture files; iterate `php-smoke-queries.json` until smoke is green
   without the oracle.
4. Add oracle helper + `oracles/php.rs` + corpus entries; gate threshold
   tuning against `symfony/console`.
5. Add CI matrix entry last so the workflow only goes green after local
   thresholds are stable.
